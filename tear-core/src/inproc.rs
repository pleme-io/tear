//! `InProcess` — the in-memory [`tear_types::MultiplexerControl`]
//! implementation backed by `parking_lot::RwLock<Registry>` +
//! `BTreeMap<PaneId, PtyHandle>`.
//!
//! ## Architecture
//!
//! - **Registry** (`Arc<RwLock<Registry>>`) — pure typed state: which
//!   sessions / windows / panes exist, what their layouts are. Read
//!   by `list_sessions`, `get_*`; written by mutating ops.
//! - **PTYs** (`Arc<Mutex<BTreeMap<PaneId, PtyHandle>>>`) — the
//!   physical PTY handles. Separated from the typed registry so a
//!   daemon can serialise the registry to disk for resurrection
//!   while PTYs (which can't outlive a process) stay in memory.
//!
//! ## Mado integration shape (M5)
//!
//! At M5 mado's `render::SharedTerminal` will become an
//! `Arc<InProcess>` instead of an `Arc<RwLock<Terminal>>`. Each
//! mado pane is then just a `PaneId` view over the shared
//! `InProcess`; `mado_pane.feed(bytes)` delegates to
//! `inproc.feed_pane_bytes(pane_id, bytes)` which calls the same
//! `vte` parser the tear-daemon uses on the headless path.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use portable_pty::PtySize;
use tracing::{debug, info};

use tear_types::{
    ControlError, ControlResult, Direction, MultiplexerControl, PaneId, SessionId, TearPane,
    TearSession, TearWindow, WindowId,
};

use std::sync::mpsc;

use crate::pane_grid::PaneGrid;
use crate::pty::PtyHandle;
use crate::registry::Registry;

/// The native in-process multiplexer backend.
pub struct InProcess {
    registry: Arc<RwLock<Registry>>,
    ptys: Arc<Mutex<BTreeMap<PaneId, PtyHandle>>>,
    /// Per-pane VT parser + cell grid. Phase-2-MVP wires PTY bytes
    /// into these so [`Self::pane_snapshot`] returns the rendered
    /// state. Wrapped per-pane in `Mutex` so the PTY reader thread
    /// and snapshot callers can race independently per pane.
    grids: Arc<Mutex<BTreeMap<PaneId, Arc<Mutex<PaneGrid>>>>>,
    /// Per-pane byte-stream subscribers. Each subscriber receives a
    /// clone of every PTY chunk (after it lands in the grid). On
    /// send error the daemon's serve thread can prune dead
    /// subscribers; the InProcess side just fans out.
    subscribers: Arc<Mutex<BTreeMap<PaneId, Vec<mpsc::Sender<Vec<u8>>>>>>,
}

impl Default for InProcess {
    fn default() -> Self {
        Self::new()
    }
}

impl InProcess {
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Arc::new(RwLock::new(Registry::new())),
            ptys: Arc::new(Mutex::new(BTreeMap::new())),
            grids: Arc::new(Mutex::new(BTreeMap::new())),
            subscribers: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Register a byte-stream subscriber for the named pane.
    /// Returns the receiver end of an `mpsc::channel`; every PTY
    /// chunk that lands in this pane is sent on the corresponding
    /// sender. Drop the receiver to unsubscribe — the next send
    /// will error and the daemon prunes the dead sender.
    ///
    /// Returns `NoSuchPane` if the pane has no PTY (never spawned
    /// or already killed).
    pub fn subscribe_pane_bytes(
        &self,
        pane: PaneId,
    ) -> ControlResult<mpsc::Receiver<Vec<u8>>> {
        // Confirm the pane exists; we don't actually need the
        // grid here (the sender is registered regardless), but
        // subscribing to a phantom pane silently is a footgun.
        if !self.ptys.lock().contains_key(&pane) {
            return Err(ControlError::NoSuchPane(pane));
        }
        let (tx, rx) = mpsc::channel();
        self.subscribers.lock().entry(pane).or_default().push(tx);
        Ok(rx)
    }

    /// Borrow the registry read-only — useful for callers that want
    /// to scan multiple entities atomically without locking per-call.
    pub fn with_registry<R>(&self, f: impl FnOnce(&Registry) -> R) -> R {
        let r = self.registry.read();
        f(&r)
    }

    /// Return a serializable snapshot of the named pane's rendered
    /// grid. Returns `NoSuchPane` if the pane never had a grid
    /// installed (which can only happen if it never had a PTY —
    /// every PTY-spawning code path also installs a grid).
    pub fn pane_snapshot(&self, pane_id: PaneId) -> ControlResult<tear_types::PaneSnapshot> {
        let grid_arc = {
            let map = self.grids.lock();
            map.get(&pane_id)
                .cloned()
                .ok_or(ControlError::NoSuchPane(pane_id))?
        };
        let grid = grid_arc.lock();
        Ok(grid.snapshot())
    }

    /// Spawn a PTY for the given pane. Caller pre-creates the typed
    /// pane via the registry; this attaches the runtime + installs
    /// the per-pane VT parser.
    fn spawn_pty_for(&self, pane_id: PaneId, shell: &str, size: (u16, u16)) -> anyhow::Result<()> {
        let pty_size = PtySize {
            rows: size.1,
            cols: size.0,
            pixel_width: 0,
            pixel_height: 0,
        };
        // Allocate the per-pane grid and register it BEFORE spawning
        // the PTY — the reader thread starts immediately on spawn,
        // and we want the first bytes to find their grid.
        let grid = Arc::new(Mutex::new(PaneGrid::new(size.0 as usize, size.1 as usize)));
        self.grids.lock().insert(pane_id, grid.clone());

        let grid_for_callback = grid.clone();
        let subscribers_for_callback = self.subscribers.clone();
        let on_bytes = Box::new(move |bytes: &[u8]| {
            grid_for_callback.lock().feed(bytes);
            // Fan out to subscribers (Phase-2.5 push subscriptions).
            // Cheap when there are zero subscribers; per-subscriber
            // cost is a Vec clone + mpsc::send. On send error the
            // sender is dead — prune it.
            let mut subs = subscribers_for_callback.lock();
            if let Some(senders) = subs.get_mut(&pane_id) {
                let mut i = 0;
                while i < senders.len() {
                    if senders[i].send(bytes.to_vec()).is_err() {
                        senders.swap_remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
            debug!(pane_id = %pane_id, n = bytes.len(), "tear-core: pty bytes fed to grid + subscribers");
        });
        let pty = PtyHandle::spawn(shell, &[], None, &[], pty_size, on_bytes)?;
        self.ptys.lock().insert(pane_id, pty);
        Ok(())
    }
}

impl MultiplexerControl for InProcess {
    fn list_sessions(&self) -> ControlResult<Vec<TearSession>> {
        Ok(self.registry.read().sessions_in_order())
    }

    fn get_session(&self, id: SessionId) -> ControlResult<TearSession> {
        self.registry
            .read()
            .sessions
            .get(&id)
            .cloned()
            .ok_or(ControlError::NoSuchSession(id))
    }

    fn get_window(&self, id: WindowId) -> ControlResult<(SessionId, TearWindow)> {
        let r = self.registry.read();
        for s in r.sessions.values() {
            if let Some(w) = s.windows.get(&id) {
                return Ok((s.id, w.clone()));
            }
        }
        Err(ControlError::NoSuchWindow(id))
    }

    fn get_pane(&self, id: PaneId) -> ControlResult<TearPane> {
        let r = self.registry.read();
        for s in r.sessions.values() {
            if let Some(p) = s.panes.get(&id) {
                return Ok(p.clone());
            }
        }
        Err(ControlError::NoSuchPane(id))
    }

    fn new_session(&self, name: &str, shell: &str) -> ControlResult<SessionId> {
        let mut r = self.registry.write();
        let sid = r.create_session(name);
        let Some((_wid, pane_id)) = r.add_window(sid, "main", shell, (80, 24)) else {
            return Err(ControlError::Internal(anyhow::anyhow!(
                "registry.add_window returned None after fresh create_session"
            )));
        };
        drop(r); // release write lock before spawning PTY
        if let Err(e) = self.spawn_pty_for(pane_id, shell, (80, 24)) {
            // Roll back the session — registry is small, easier to
            // remove than to leave a sessionless typed entry.
            self.registry.write().sessions.remove(&sid);
            return Err(ControlError::Internal(e));
        }
        info!(session = %sid, name, shell, "tear-core: new session");
        Ok(sid)
    }

    fn rename_session(&self, id: SessionId, new_name: &str) -> ControlResult<()> {
        let mut r = self.registry.write();
        let s = r.sessions.get_mut(&id).ok_or(ControlError::NoSuchSession(id))?;
        s.name = new_name.into();
        Ok(())
    }

    fn kill_session(&self, id: SessionId) -> ControlResult<()> {
        let panes_to_kill: Vec<PaneId> = {
            let r = self.registry.read();
            let s = r.sessions.get(&id).ok_or(ControlError::NoSuchSession(id))?;
            s.panes.keys().copied().collect()
        };
        {
            let mut ptys = self.ptys.lock();
            let mut grids = self.grids.lock();
            let mut subs = self.subscribers.lock();
            for p in &panes_to_kill {
                ptys.remove(p);
                grids.remove(p);
                // Dropping the sender vec disconnects subscribers
                // cleanly — their recv() returns Err on next read.
                subs.remove(p);
            }
        }
        self.registry.write().sessions.remove(&id);
        info!(session = %id, "tear-core: killed session");
        Ok(())
    }

    fn new_window(&self, session: SessionId, name: &str, shell: &str) -> ControlResult<WindowId> {
        let size = (80, 24);
        let (wid, pid) = {
            let mut r = self.registry.write();
            r.add_window(session, name, shell, size)
                .ok_or(ControlError::NoSuchSession(session))?
        };
        if let Err(e) = self.spawn_pty_for(pid, shell, size) {
            return Err(ControlError::Internal(e));
        }
        info!(session = %session, window = %wid, name, "tear-core: new window");
        Ok(wid)
    }

    fn kill_window(&self, id: WindowId) -> ControlResult<()> {
        let panes_to_kill: Vec<PaneId> = {
            let r = self.registry.read();
            let mut out = Vec::new();
            for s in r.sessions.values() {
                if let Some(w) = s.windows.get(&id) {
                    out.extend(w.layout.panes());
                    break;
                }
            }
            if out.is_empty() {
                return Err(ControlError::NoSuchWindow(id));
            }
            out
        };
        {
            let mut ptys = self.ptys.lock();
            let mut grids = self.grids.lock();
            let mut subs = self.subscribers.lock();
            for p in &panes_to_kill {
                ptys.remove(p);
                grids.remove(p);
                // Dropping the sender vec disconnects subscribers
                // cleanly — their recv() returns Err on next read.
                subs.remove(p);
            }
        }
        let mut r = self.registry.write();
        for s in r.sessions.values_mut() {
            if s.windows.remove(&id).is_some() {
                for p in &panes_to_kill {
                    s.panes.remove(p);
                }
                break;
            }
        }
        info!(window = %id, "tear-core: killed window");
        Ok(())
    }

    fn select_window(&self, id: WindowId) -> ControlResult<()> {
        let mut r = self.registry.write();
        for s in r.sessions.values_mut() {
            if s.windows.contains_key(&id) {
                s.active_window = id;
                return Ok(());
            }
        }
        Err(ControlError::NoSuchWindow(id))
    }

    fn split_pane(
        &self,
        origin: PaneId,
        _direction: Direction,
        shell: &str,
    ) -> ControlResult<PaneId> {
        // M0 minimal viable split: locate parent, add a leaf to the
        // window's layout as a balanced split with the origin. The
        // full directional / ratio logic lands in M2.
        let (sid, wid) = self
            .registry
            .read()
            .locate_pane(origin)
            .ok_or(ControlError::NoSuchPane(origin))?;
        let size = (80, 12);
        let pid = {
            let mut r = self.registry.write();
            let Some(s) = r.sessions.get_mut(&sid) else {
                return Err(ControlError::NoSuchSession(sid));
            };
            let new_pid = crate::registry::mint_pane_id(wid, shell);
            s.panes.insert(
                new_pid,
                TearPane {
                    id: new_pid,
                    shell: shell.into(),
                    args: vec![],
                    cwd: None,
                    env: vec![],
                    size_cells: size,
                    origin_cells: (0, 0),
                    state: tear_types::PaneState::Running,
                    title: shell.into(),
                },
            );
            // Replace the window's layout with a balanced split.
            if let Some(w) = s.windows.get_mut(&wid) {
                let old = std::mem::replace(
                    &mut w.layout,
                    tear_types::LayoutNode::leaf(tear_types::PaneId::NULL),
                );
                w.layout = tear_types::LayoutNode::split(
                    _direction.orientation(),
                    old,
                    tear_types::LayoutNode::leaf(new_pid),
                );
                w.active_pane = new_pid;
            }
            new_pid
        };
        self.spawn_pty_for(pid, shell, size)
            .map_err(ControlError::Internal)?;
        info!(pane = %pid, "tear-core: split pane");
        Ok(pid)
    }

    fn kill_pane(&self, id: PaneId) -> ControlResult<()> {
        self.ptys.lock().remove(&id);
        self.grids.lock().remove(&id);
        self.subscribers.lock().remove(&id);
        let mut r = self.registry.write();
        let mut found = false;
        for s in r.sessions.values_mut() {
            if s.panes.remove(&id).is_some() {
                found = true;
                // Layout update — M2 will properly rebalance the
                // tree. For now we leave the leaf reference dangling;
                // the renderer treats a missing pane as blank.
                break;
            }
        }
        if !found {
            return Err(ControlError::NoSuchPane(id));
        }
        Ok(())
    }

    fn select_pane(&self, id: PaneId) -> ControlResult<()> {
        let (sid, wid) = self
            .registry
            .read()
            .locate_pane(id)
            .ok_or(ControlError::NoSuchPane(id))?;
        let mut r = self.registry.write();
        if let Some(s) = r.sessions.get_mut(&sid) {
            if let Some(w) = s.windows.get_mut(&wid) {
                w.active_pane = id;
            }
        }
        Ok(())
    }

    fn resize_pane(
        &self,
        id: PaneId,
        _direction: Direction,
        _delta_cells: i16,
    ) -> ControlResult<()> {
        // M2 — proper layout-tree resize. For M0 we acknowledge.
        let _ = self.registry.read().locate_pane(id).ok_or(ControlError::NoSuchPane(id))?;
        Ok(())
    }

    fn send_keys(&self, id: PaneId, bytes: &[u8]) -> ControlResult<()> {
        let ptys = self.ptys.lock();
        let pty = ptys.get(&id).ok_or(ControlError::NoSuchPane(id))?;
        pty.write(bytes)
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        Ok(())
    }

    /// Phase-2 override of the trait's default `pane_snapshot`.
    /// Delegates to the inherent [`Self::pane_snapshot`] method,
    /// which reads the per-pane `PaneGrid` installed by
    /// [`Self::spawn_pty_for`].
    fn pane_snapshot(&self, id: PaneId) -> ControlResult<tear_types::PaneSnapshot> {
        InProcess::pane_snapshot(self, id)
    }

    /// Phase-3.1 override — resize the underlying PTY (fires
    /// SIGWINCH at the child) AND resize the per-pane PaneGrid
    /// so subsequent snapshots reflect the new geometry.
    fn pane_resize_absolute(
        &self,
        id: PaneId,
        cols: u16,
        rows: u16,
    ) -> ControlResult<()> {
        use portable_pty::PtySize;
        let pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        // Resize the PTY (delivers SIGWINCH to child).
        {
            let ptys = self.ptys.lock();
            let pty = ptys.get(&id).ok_or(ControlError::NoSuchPane(id))?;
            pty.resize(pty_size)
                .map_err(|e| ControlError::Internal(anyhow::anyhow!(e)))?;
        }
        // Resize the parser-backed grid so subsequent snapshots
        // honour the new geometry.
        if let Some(grid) = self.grids.lock().get(&id).cloned() {
            grid.lock().resize(cols as usize, rows as usize);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_inproc_starts_empty() {
        let inproc = InProcess::new();
        let sessions = inproc.list_sessions().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn get_nonexistent_session_errors() {
        let inproc = InProcess::new();
        let err = inproc.get_session(SessionId(99)).unwrap_err();
        assert!(matches!(err, ControlError::NoSuchSession(_)));
    }
}
