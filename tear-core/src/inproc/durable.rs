use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use makimono::{Ending, JournalBounds, PaneDir, PaneMeta, SessionDir, Store, Verdict, classify};
use parking_lot::{Mutex, RwLock};
use portable_pty::PtySize;
use tamotsu::{HeldPty, HoldArgs, HoldProgram, Revival};
use tear_types::{LeafRemoval, PaneId, PaneState, SessionId, TearPane, TearSession, WindowId};
use tracing::{info, warn};

use super::InProcess;
use crate::pane_grid::PaneGrid;
use crate::pty::PtyHandle;
use crate::registry::Registry;

const PERSIST_TICK: Duration = Duration::from_millis(500);
const ENDED_RETENTION: usize = 32;

#[derive(Clone, Debug)]
pub struct Durable {
    pub store: Store,
    pub holder_dir: PathBuf,
    pub program: HoldProgram,
    pub bounds: JournalBounds,
}

impl Durable {
    #[must_use]
    pub fn socket_for(&self, pane: PaneId) -> PathBuf {
        self.holder_dir.join(format!("{pane}.sock"))
    }

    fn synth_meta(&self, pane: &TearPane) -> PaneMeta {
        PaneMeta {
            shell: pane.shell.clone(),
            args: pane.args.clone(),
            spawn_cwd: pane.cwd.clone(),
            last_cwd: None,
            title: pane.title.clone(),
            size: pane.size_cells,
            holder_socket: self.socket_for(pane.id),
            holder_pid: None,
            overrides: pane.env.clone(),
            resurrections: 0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub sessions: usize,
    pub adopted: usize,
    pub resurrected: usize,
    pub ended: usize,
    pub failed: usize,
}

pub(super) enum PaneIo {
    Local(PtyHandle),
    Held(HeldPty),
}

impl PaneIo {
    pub(super) fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Local(p) => p.write(bytes),
            Self::Held(h) => h.write(bytes),
        }
    }

    pub(super) fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        match self {
            Self::Local(p) => p.resize(size),
            Self::Held(h) => h.resize(size.cols, size.rows).map_err(Into::into),
        }
    }

    pub(super) fn end(self) {
        if let Self::Held(h) = &self {
            h.end();
        }
        drop(self);
    }
}

pub(super) struct SpawnPlan<'a> {
    pub pane_id: PaneId,
    pub shell: &'a str,
    pub args: &'a [String],
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub size: PtySize,
    pub overrides: Vec<(String, String)>,
}

impl InProcess {
    pub fn enable_durability(&self, durable: Option<Durable>) {
        let enabling = durable.is_some();
        *self.durable.write() = durable;
        if enabling && !self.persister_started.swap(true, Ordering::AcqRel) {
            self.spawn_persister();
        }
    }

    #[must_use]
    pub fn durability(&self) -> Option<Durable> {
        self.durable.read().clone()
    }

    pub fn release_durable(&self) {
        if self.durable.write().take().is_none() {
            return;
        }
        let held: Vec<PaneIo> = {
            let mut ptys = self.ptys.lock();
            let ids: Vec<PaneId> = ptys
                .iter()
                .filter(|(_, io)| matches!(io, PaneIo::Held(_)))
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| ptys.remove(id)).collect()
        };
        info!(panes = held.len(), "tear-core: held panes released to their holders");
        drop(held);
    }

    pub(super) fn session_dir_of(&self, pane: PaneId) -> Option<(Durable, SessionDir)> {
        let durable = self.durable.read().clone()?;
        let sid = self.registry.read().locate_pane(pane).map(|(s, _)| s).or_else(|| {
            self.registry
                .read()
                .sessions
                .values()
                .find(|s| s.panes.contains_key(&pane))
                .map(|s| s.id)
        })?;
        let dir = durable.store.session(sid);
        Some((durable, dir))
    }

    pub(super) fn tombstone_dir(&self, pane: PaneId) -> Option<PaneDir> {
        self.session_dir_of(pane).map(|(_, dir)| dir.pane(pane))
    }

    pub(super) fn record_human_end(&self, panes: &[PaneId]) {
        for pane in panes {
            if let Some(dir) = self.tombstone_dir(*pane) {
                let _ = dir.create();
                if let Err(e) = dir.write_tombstone(&Ending::by_control()) {
                    warn!(pane = %pane, error = %e, "tear-core: could not record a human end — the session may be revived on restart");
                }
            }
        }
    }

    pub(super) fn open_pane_io(&self, plan: SpawnPlan<'_>, grid: &Arc<Mutex<PaneGrid>>) -> anyhow::Result<PaneIo> {
        if let Some((durable, dir)) = self.session_dir_of(plan.pane_id) {
            match self.launch_held(&durable, &dir, &plan, grid) {
                Ok(held) => {
                    self.persist_session_now(dir.id());
                    return Ok(PaneIo::Held(held));
                }
                Err(e) => warn!(
                    pane = %plan.pane_id,
                    error = %e,
                    "tear-core: could not hand the pane to a tamotsu holder — running it in-process; it will be resurrected (not re-adopted) after a daemon restart"
                ),
            }
        }
        let tombstone = self.tombstone_dir(plan.pane_id);
        let (on_bytes, on_exit) = self.pane_callbacks(plan.pane_id, grid, tombstone);
        let pty = PtyHandle::spawn(
            plan.shell,
            plan.args,
            plan.cwd.as_deref(),
            &plan.env,
            plan.size,
            on_bytes,
            on_exit,
        )?;
        if let Some((_, dir)) = self.session_dir_of(plan.pane_id) {
            self.persist_session_now(dir.id());
        }
        Ok(PaneIo::Local(pty))
    }

    fn launch_held(
        &self,
        durable: &Durable,
        dir: &SessionDir,
        plan: &SpawnPlan<'_>,
        grid: &Arc<Mutex<PaneGrid>>,
    ) -> std::io::Result<HeldPty> {
        let pane_dir = dir.pane(plan.pane_id);
        pane_dir.create()?;
        let meta = PaneMeta {
            shell: plan.shell.to_string(),
            args: plan.args.to_vec(),
            spawn_cwd: plan.cwd.clone(),
            last_cwd: None,
            title: String::new(),
            size: (plan.size.cols, plan.size.rows),
            holder_socket: durable.socket_for(plan.pane_id),
            holder_pid: None,
            overrides: plan.overrides.clone(),
            resurrections: 0,
        };
        pane_dir.write_meta(&meta)?;
        let revival = Revival {
            program: durable.program.clone(),
            args: HoldArgs {
                pane_dir: pane_dir.path().to_path_buf(),
                socket: meta.holder_socket.clone(),
                cols: plan.size.cols,
                rows: plan.size.rows,
                cwd: plan.cwd.clone(),
                resurrected: false,
                bounds: durable.bounds,
                program: plan.shell.to_string(),
                args: plan.args.to_vec(),
            },
            env: plan.env.clone(),
        };
        let (on_bytes, on_exit) = self.pane_callbacks(plan.pane_id, grid, None);
        HeldPty::launch(revival, on_bytes, on_exit)
    }

    pub(super) fn persist_session_now(&self, sid: SessionId) {
        let Some(durable) = self.durable.read().clone() else {
            return;
        };
        let session = self.registry.read().sessions.get(&sid).cloned();
        if let Some(s) = session {
            if let Err(e) = durable.store.session(sid).write_doc(&s) {
                warn!(session = %sid, error = %e, "tear-core: session document write failed");
            }
        }
    }

    fn spawn_persister(&self) {
        let registry = Arc::downgrade(&self.registry);
        let grids = Arc::downgrade(&self.grids);
        let durable = Arc::downgrade(&self.durable);
        let restored = Arc::downgrade(&self.restored);
        let spawned = thread::Builder::new()
            .name("tear-persist".into())
            .spawn(move || {
                let mut docs: HashMap<SessionId, blake3::Hash> = HashMap::new();
                let mut metas: HashMap<PaneId, (Option<String>, String, (u16, u16))> = HashMap::new();
                loop {
                    thread::sleep(PERSIST_TICK);
                    let (Some(registry), Some(grids), Some(durable), Some(restored)) = (
                        registry.upgrade(),
                        grids.upgrade(),
                        durable.upgrade(),
                        restored.upgrade(),
                    ) else {
                        return;
                    };
                    let Some(d) = durable.read().clone() else {
                        continue;
                    };
                    persist_pass(
                        &d,
                        &registry,
                        &grids,
                        restored.load(Ordering::Acquire),
                        &mut docs,
                        &mut metas,
                    );
                }
            });
        if let Err(e) = spawned {
            warn!(error = %e, "tear-core: session persister could not start — documents are written only at spawn");
        }
    }

    pub fn restore_durable(&self) -> RestoreReport {
        let mut report = RestoreReport::default();
        let Some(d) = self.durability() else {
            return report;
        };
        let dirs = match d.store.sessions() {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "tear-core: cannot list durable sessions");
                self.restored.store(true, Ordering::Release);
                return report;
            }
        };
        for dir in dirs {
            if self.registry.read().sessions.contains_key(&dir.id()) {
                continue;
            }
            match dir.read_doc() {
                Ok(Some(session)) => self.restore_session(&d, &dir, session, &mut report),
                Ok(None) => {
                    if session_is_over(&dir, None) {
                        let _ = d.store.archive(&dir, ENDED_RETENTION);
                    }
                }
                Err(e) => {
                    warn!(session = %dir.id(), error = %e, "tear-core: unreadable session document left in place");
                    report.failed += 1;
                }
            }
        }
        self.restored.store(true, Ordering::Release);
        info!(
            sessions = report.sessions,
            adopted = report.adopted,
            resurrected = report.resurrected,
            ended = report.ended,
            failed = report.failed,
            "tear-core: durable sessions restored"
        );
        report
    }

    fn restore_session(
        &self,
        d: &Durable,
        dir: &SessionDir,
        mut session: TearSession,
        report: &mut RestoreReport,
    ) {
        let mut plan: Vec<(PaneId, PaneMeta, bool)> = Vec::new();
        let mut ended: Vec<PaneId> = Vec::new();
        for (pid, pane) in &session.panes {
            let pd = dir.pane(*pid);
            let meta = pd
                .read_meta()
                .ok()
                .flatten()
                .unwrap_or_else(|| d.synth_meta(pane));
            let tomb = pd.tombstone().ok().flatten();
            match classify(tomb, HeldPty::probe(&meta.holder_socket)) {
                Verdict::Ended(_) => ended.push(*pid),
                Verdict::Held => plan.push((*pid, meta, false)),
                Verdict::Orphaned => plan.push((*pid, meta, true)),
            }
        }
        for pid in &ended {
            prune_pane(&mut session, *pid);
        }
        report.ended += ended.len();
        if session.panes.is_empty() {
            let _ = d.store.archive(dir, ENDED_RETENTION);
            return;
        }
        for p in session.panes.values_mut() {
            p.state = PaneState::Running;
        }
        let sid = session.id;
        let yurai: BTreeMap<PaneId, tear_types::Yurai> = session
            .panes
            .iter()
            .map(|(id, p)| (*id, p.yurai.clone()))
            .collect();
        self.registry.write().sessions.insert(sid, session);
        report.sessions += 1;
        for (pid, meta, revive) in plan {
            let y = yurai.get(&pid).cloned().unwrap_or_default();
            match self.attach_restored(d, dir, pid, meta, revive, y) {
                Ok(()) if revive => report.resurrected += 1,
                Ok(()) => report.adopted += 1,
                Err(e) => {
                    warn!(session = %sid, pane = %pid, error = %e, "tear-core: pane could not be restored; it stays on disk for the next start");
                    report.failed += 1;
                    self.grids.lock().remove(&pid);
                    let mut r = self.registry.write();
                    if let Some(p) = r.sessions.get_mut(&sid).and_then(|s| s.panes.get_mut(&pid)) {
                        p.state = PaneState::Exited { code: -1 };
                    }
                    drop(r);
                    self.subscribers.lock().entry(pid).or_default().closed = Some(-1);
                }
            }
        }
        self.persist_session_now(sid);
    }

    fn attach_restored(
        &self,
        d: &Durable,
        dir: &SessionDir,
        pid: PaneId,
        mut meta: PaneMeta,
        revive: bool,
        yurai: tear_types::Yurai,
    ) -> std::io::Result<()> {
        let pd = dir.pane(pid);
        pd.create()?;
        let size = (meta.size.0.max(1), meta.size.1.max(1));
        let grid = Arc::new(Mutex::new(PaneGrid::with_scrollback(
            size.0 as usize,
            size.1 as usize,
            *self.scrollback_rows.read(),
        )));
        grid.lock().stamp_yurai(yurai);
        self.grids.lock().insert(pid, Arc::clone(&grid));
        let spawn_env = tear_types::SpawnEnv::from_overrides(meta.overrides.clone());
        let env = self.child_env(pid, &spawn_env);
        let revival_for = |meta: &PaneMeta, resurrected: bool| Revival {
            program: d.program.clone(),
            args: HoldArgs {
                pane_dir: pd.path().to_path_buf(),
                socket: meta.holder_socket.clone(),
                cols: size.0,
                rows: size.1,
                cwd: if resurrected {
                    pd.revive_cwd(meta)
                } else {
                    meta.spawn_cwd.clone()
                },
                resurrected,
                bounds: d.bounds,
                program: meta.shell.clone(),
                args: meta.args.clone(),
            },
            env: env.clone(),
        };
        let held = if revive {
            None
        } else {
            let (on_bytes, on_exit) = self.pane_callbacks(pid, &grid, None);
            HeldPty::adopt(revival_for(&meta, false), on_bytes, on_exit).ok()
        };
        let held = match held {
            Some(h) => h,
            None => {
                meta.resurrections += 1;
                meta.holder_socket = d.socket_for(pid);
                pd.write_meta(&meta)?;
                let (on_bytes, on_exit) = self.pane_callbacks(pid, &grid, None);
                HeldPty::launch(revival_for(&meta, true), on_bytes, on_exit)?
            }
        };
        self.ptys.lock().insert(pid, PaneIo::Held(held));
        Ok(())
    }
}

fn persist_pass(
    d: &Durable,
    registry: &RwLock<Registry>,
    grids: &Mutex<BTreeMap<PaneId, Arc<Mutex<PaneGrid>>>>,
    restored: bool,
    docs: &mut HashMap<SessionId, blake3::Hash>,
    metas: &mut HashMap<PaneId, (Option<String>, String, (u16, u16))>,
) {
    let sessions: Vec<TearSession> = registry.read().sessions.values().cloned().collect();
    let live: HashSet<SessionId> = sessions.iter().map(|s| s.id).collect();
    let mut live_panes: HashSet<PaneId> = HashSet::new();
    for s in &sessions {
        let dir = d.store.session(s.id);
        if let Ok(json) = serde_json::to_vec(s) {
            let h = blake3::hash(&json);
            if docs.get(&s.id) != Some(&h) && dir.write_doc(s).is_ok() {
                docs.insert(s.id, h);
            }
        }
        for (pid, pane) in &s.panes {
            live_panes.insert(*pid);
            if matches!(pane.state, PaneState::Exited { .. }) {
                continue;
            }
            let cwd = grids
                .lock()
                .get(pid)
                .cloned()
                .and_then(|g| g.lock().state.blocks.current_cwd().map(str::to_owned));
            let key = (cwd.clone(), pane.title.clone(), pane.size_cells);
            if metas.get(pid) == Some(&key) {
                continue;
            }
            let pd = dir.pane(*pid);
            let mut meta = pd
                .read_meta()
                .ok()
                .flatten()
                .unwrap_or_else(|| d.synth_meta(pane));
            if cwd.is_some() {
                meta.last_cwd = cwd;
            }
            meta.title.clone_from(&pane.title);
            meta.size = pane.size_cells;
            if pd.create().is_ok() && pd.write_meta(&meta).is_ok() {
                metas.insert(*pid, key);
            }
        }
    }
    docs.retain(|id, _| live.contains(id));
    metas.retain(|id, _| live_panes.contains(id));
    if !restored {
        return;
    }
    let Ok(on_disk) = d.store.sessions() else {
        return;
    };
    for dir in on_disk {
        if live.contains(&dir.id()) {
            continue;
        }
        let doc = dir.read_doc().ok().flatten();
        if session_is_over(&dir, doc.as_ref()) {
            if let Err(e) = d.store.archive(&dir, ENDED_RETENTION) {
                warn!(session = %dir.id(), error = %e, "tear-core: could not archive an ended session");
            }
        }
    }
}

fn session_is_over(dir: &SessionDir, doc: Option<&TearSession>) -> bool {
    let panes = dir.panes().unwrap_or_default();
    if panes.is_empty() {
        return doc.is_none_or(|s| s.panes.is_empty());
    }
    dir.all_panes_ended().unwrap_or(false)
}

fn prune_pane(session: &mut TearSession, pid: PaneId) {
    session.panes.remove(&pid);
    let mut emptied: Vec<WindowId> = Vec::new();
    for (wid, w) in &mut session.windows {
        match w.layout.remove_leaf(pid) {
            LeafRemoval::WasRoot => emptied.push(*wid),
            LeafRemoval::Removed => {
                if w.active_pane == pid {
                    w.active_pane = w.layout.panes().first().copied().unwrap_or(PaneId::NULL);
                }
            }
            LeafRemoval::NotFound => {}
        }
    }
    for wid in emptied {
        session.windows.remove(&wid);
    }
    if !session.windows.contains_key(&session.active_window) {
        session.active_window = session.windows.keys().next().copied().unwrap_or(WindowId::NULL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tear_types::{Direction, LayoutNode, SessionSource, SessionState, TearWindow, WindowState};

    fn pane(id: PaneId) -> TearPane {
        TearPane {
            id,
            shell: "/bin/sh".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            size_cells: (80, 24),
            origin_cells: (0, 0),
            state: PaneState::Running,
            title: String::new(),
            input_policy: tear_types::InputPolicy::default(),
            yurai: tear_types::Yurai::Unknown,
        }
    }

    #[test]
    fn pruning_ended_panes_keeps_the_layout_consistent() {
        let (a, b, c) = (PaneId::from_seed("a"), PaneId::from_seed("b"), PaneId::from_seed("c"));
        let (w1, w2) = (WindowId::from_seed("w1"), WindowId::from_seed("w2"));
        let mut layout = LayoutNode::leaf(a);
        assert!(layout.split_leaf(a, b, Direction::Right, 0.5));
        let mut windows = BTreeMap::new();
        windows.insert(
            w1,
            TearWindow {
                id: w1,
                name: "one".into(),
                layout,
                active_pane: b,
                size_cells: (80, 24),
                state: WindowState::Active,
            },
        );
        windows.insert(
            w2,
            TearWindow {
                id: w2,
                name: "two".into(),
                layout: LayoutNode::leaf(c),
                active_pane: c,
                size_cells: (80, 24),
                state: WindowState::Active,
            },
        );
        let mut session = TearSession {
            id: SessionId::from_seed("s"),
            name: "s".into(),
            windows,
            panes: [(a, pane(a)), (b, pane(b)), (c, pane(c))].into_iter().collect(),
            active_window: w2,
            state: SessionState::Active,
            created_at_unix: 0,
            description: String::new(),
            source: SessionSource::Human,
            freio: tear_types::Freio::default(),
        };
        prune_pane(&mut session, b);
        assert_eq!(session.windows[&w1].active_pane, a);
        assert_eq!(session.windows[&w1].layout.panes(), vec![a]);
        prune_pane(&mut session, c);
        assert!(!session.windows.contains_key(&w2));
        assert_eq!(session.active_window, w1);
        assert_eq!(session.panes.keys().copied().collect::<Vec<_>>(), vec![a]);
    }
}
