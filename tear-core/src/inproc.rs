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

use crate::pty::PtyHandle;
use crate::registry::Registry;

/// The native in-process multiplexer backend.
pub struct InProcess {
    registry: Arc<RwLock<Registry>>,
    ptys: Arc<Mutex<BTreeMap<PaneId, PtyHandle>>>,
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
        }
    }

    /// Borrow the registry read-only — useful for callers that want
    /// to scan multiple entities atomically without locking per-call.
    pub fn with_registry<R>(&self, f: impl FnOnce(&Registry) -> R) -> R {
        let r = self.registry.read();
        f(&r)
    }

    /// Spawn a PTY for the given pane. Caller pre-creates the typed
    /// pane via the registry; this attaches the runtime.
    fn spawn_pty_for(&self, pane_id: PaneId, shell: &str, size: (u16, u16)) -> anyhow::Result<()> {
        let pty_size = PtySize {
            rows: size.1,
            cols: size.0,
            pixel_width: 0,
            pixel_height: 0,
        };
        let on_bytes = Box::new(move |bytes: &[u8]| {
            // TODO M2: feed bytes to the per-pane vte::Parser kept in
            // the InProcess's grid map. For M0 the bytes simply
            // increment the stats counter via PtyHandle::bytes_consumed.
            debug!(pane_id = %pane_id, n = bytes.len(), "tear-core: pty bytes");
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
            for p in &panes_to_kill {
                ptys.remove(p);
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
            for p in &panes_to_kill {
                ptys.remove(p);
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
