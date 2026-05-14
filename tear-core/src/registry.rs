//! In-memory session/window/pane registry — the typed state machine
//! every [`crate::InProcess`] op composes against.
//!
//! Held inside `Arc<RwLock<Registry>>` so the daemon (and tier-3
//! mado embedders) can lock for reads/writes from any thread.
//! parking_lot's RwLock for cheaper acquire + true reader-writer
//! semantics — same pattern mado uses for its terminal lock (P30).

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use tear_types::{
    LayoutNode, PaneId, PaneState, SessionId, SessionState, TearPane, TearSession, TearWindow,
    WindowId, WindowState,
};

/// Monotonic counter used by the seed-based ID minting so IDs stay
/// distinct even when minted in the same millisecond.
fn next_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint a fresh [`SessionId`] from a name + the global counter.
#[must_use]
pub fn mint_session_id(name: &str) -> SessionId {
    let seed = format!("session:{}:{}:{}", name, now_unix(), next_counter());
    SessionId::from_seed(&seed)
}
#[must_use]
pub fn mint_window_id(parent: SessionId, name: &str) -> WindowId {
    let seed = format!("window:{}:{}:{}:{}", parent, name, now_unix(), next_counter());
    WindowId::from_seed(&seed)
}
#[must_use]
pub fn mint_pane_id(parent: WindowId, shell: &str) -> PaneId {
    let seed = format!("pane:{}:{}:{}:{}", parent, shell, now_unix(), next_counter());
    PaneId::from_seed(&seed)
}

/// The complete registry of sessions / windows / panes. Stored flat
/// at the registry level — every entity is reachable in O(log N) via
/// its id, and the [`TearSession`] struct holds child windows/panes
/// by id for the typed-domain consumers.
#[derive(Debug, Default)]
pub struct Registry {
    pub sessions: BTreeMap<SessionId, TearSession>,
}

impl Registry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sorted list of sessions, oldest-first.
    pub fn sessions_in_order(&self) -> Vec<TearSession> {
        let mut v: Vec<_> = self.sessions.values().cloned().collect();
        v.sort_by_key(|s| s.created_at_unix);
        v
    }

    /// Insert a freshly minted session with no windows yet — caller
    /// is expected to add at least one window before returning.
    pub fn create_session(&mut self, name: &str) -> SessionId {
        let id = mint_session_id(name);
        let s = TearSession {
            id,
            name: name.into(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            active_window: WindowId::NULL,
            state: SessionState::Active,
            created_at_unix: now_unix(),
            description: String::new(),
        };
        self.sessions.insert(id, s);
        id
    }

    /// Insert a window into a session, with a single seed-pane.
    pub fn add_window(
        &mut self,
        session_id: SessionId,
        name: &str,
        shell: &str,
        size_cells: (u16, u16),
    ) -> Option<(WindowId, PaneId)> {
        let s = self.sessions.get_mut(&session_id)?;
        let win_id = mint_window_id(session_id, name);
        let pane_id = mint_pane_id(win_id, shell);
        let pane = TearPane {
            id: pane_id,
            shell: shell.into(),
            args: vec![],
            cwd: None,
            env: vec![],
            size_cells,
            origin_cells: (0, 0),
            state: PaneState::Running,
            title: shell.into(),
        };
        let win = TearWindow {
            id: win_id,
            name: name.into(),
            layout: LayoutNode::leaf(pane_id),
            active_pane: pane_id,
            size_cells,
            state: WindowState::Active,
        };
        s.windows.insert(win_id, win);
        s.panes.insert(pane_id, pane);
        if s.active_window == WindowId::NULL {
            s.active_window = win_id;
        }
        Some((win_id, pane_id))
    }

    /// Find the parent session + window for a given pane id. Returns
    /// `None` if the pane doesn't exist in any session.
    pub fn locate_pane(&self, pane: PaneId) -> Option<(SessionId, WindowId)> {
        for s in self.sessions.values() {
            if !s.panes.contains_key(&pane) {
                continue;
            }
            for (wid, w) in &s.windows {
                if w.layout.panes().contains(&pane) {
                    return Some((s.id, *wid));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_session_lands_in_map() {
        let mut r = Registry::new();
        let id = r.create_session("work");
        assert!(r.sessions.contains_key(&id));
        assert_eq!(r.sessions[&id].name, "work");
    }

    #[test]
    fn add_window_creates_initial_pane_and_focuses_it() {
        let mut r = Registry::new();
        let sid = r.create_session("work");
        let (wid, pid) = r.add_window(sid, "main", "/bin/zsh", (80, 24)).unwrap();
        let s = &r.sessions[&sid];
        assert!(s.windows.contains_key(&wid));
        assert!(s.panes.contains_key(&pid));
        assert_eq!(s.active_window, wid);
        assert_eq!(s.windows[&wid].active_pane, pid);
        assert_eq!(s.windows[&wid].layout.pane_count(), 1);
    }

    #[test]
    fn locate_pane_finds_its_parent() {
        let mut r = Registry::new();
        let sid = r.create_session("work");
        let (wid, pid) = r.add_window(sid, "main", "/bin/zsh", (80, 24)).unwrap();
        assert_eq!(r.locate_pane(pid), Some((sid, wid)));
    }
}
