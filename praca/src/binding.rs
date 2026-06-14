//! [`ProjectBinding`] — the persisted project-root → session map.
//!
//! This is the automation's memory: "the last session I opened for
//! `~/code/pleme-io/mado` was `<id>`". When the operator `cd`s back into
//! that project, the attach engine ([`crate::attach`]) looks the root up
//! here and auto-switches to the bound session instead of spawning a new
//! one.
//!
//! Keyed by `PathBuf` (the project root, as resolved by
//! [`crate::project::project_root`]); ordered ([`BTreeMap`]) so the
//! persisted form is deterministic.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tear_types::id::SessionId;

/// Persisted map from a project root to the session bound to it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectBinding {
    map: BTreeMap<PathBuf, SessionId>,
}

impl ProjectBinding {
    /// An empty binding map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a project `root` to a session `id`, replacing any prior
    /// binding for that root. Returns the previously-bound id, if any.
    pub fn bind(&mut self, root: impl Into<PathBuf>, id: SessionId) -> Option<SessionId> {
        self.map.insert(root.into(), id)
    }

    /// The session bound to `root`, if any.
    #[must_use]
    pub fn lookup(&self, root: &Path) -> Option<SessionId> {
        self.map.get(root).copied()
    }

    /// Drop the binding for `root`. Returns the unbound id, if any.
    pub fn unbind(&mut self, root: &Path) -> Option<SessionId> {
        self.map.remove(root)
    }

    /// Drop every binding pointing at a dead session `id`. Called when a
    /// session ends so no project keeps resolving to a session that no
    /// longer exists. Returns how many bindings were removed.
    pub fn remove_session(&mut self, id: SessionId) -> usize {
        let before = self.map.len();
        self.map.retain(|_, bound| *bound != id);
        before - self.map.len()
    }

    /// Number of live bindings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether there are no bindings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Iterate `(root, session)` pairs in deterministic key order.
    pub fn iter(&self) -> impl Iterator<Item = (&PathBuf, SessionId)> {
        self.map.iter().map(|(p, id)| (p, *id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId::from_seed(s)
    }

    #[test]
    fn bind_then_lookup() {
        let mut b = ProjectBinding::new();
        let root = PathBuf::from("/code/mado");
        assert_eq!(b.bind(root.clone(), sid("mado")), None);
        assert_eq!(b.lookup(&root), Some(sid("mado")));
        assert_eq!(b.lookup(Path::new("/code/other")), None);
    }

    #[test]
    fn rebind_returns_previous() {
        let mut b = ProjectBinding::new();
        let root = PathBuf::from("/code/mado");
        b.bind(root.clone(), sid("first"));
        assert_eq!(b.bind(root.clone(), sid("second")), Some(sid("first")));
        assert_eq!(b.lookup(&root), Some(sid("second")));
    }

    #[test]
    fn unbind_removes() {
        let mut b = ProjectBinding::new();
        let root = PathBuf::from("/code/mado");
        b.bind(root.clone(), sid("x"));
        assert_eq!(b.unbind(&root), Some(sid("x")));
        assert_eq!(b.lookup(&root), None);
        assert!(b.is_empty());
    }

    #[test]
    fn remove_session_drops_all_matching_bindings() {
        let mut b = ProjectBinding::new();
        b.bind(PathBuf::from("/a"), sid("dead"));
        b.bind(PathBuf::from("/b"), sid("dead"));
        b.bind(PathBuf::from("/c"), sid("alive"));
        assert_eq!(b.remove_session(sid("dead")), 2);
        assert_eq!(b.lookup(Path::new("/a")), None);
        assert_eq!(b.lookup(Path::new("/b")), None);
        assert_eq!(b.lookup(Path::new("/c")), Some(sid("alive")));
    }

    #[test]
    fn serde_round_trips() {
        let mut b = ProjectBinding::new();
        b.bind(PathBuf::from("/code/mado"), sid("mado"));
        b.bind(PathBuf::from("/code/tear"), sid("tear"));
        let json = serde_json::to_string(&b).unwrap();
        let back: ProjectBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(b, back);
        assert_eq!(back.lookup(Path::new("/code/tear")), Some(sid("tear")));
    }
}
