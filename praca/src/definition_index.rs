//! A searchable catalog of latent [`SessionDefinition`]s — the sibling of
//! [`crate::SessionIndex`] for *presets*.
//!
//! Same shape as the live-session index (upsert / remove / get / by_project
//! / search), keyed by [`DefinitionId`]. Its `search` delegates to the
//! SHARED [`crate::index::rank`] over [`crate::Searchable`] — the exact
//! comparator `SessionIndex` uses — so a latent preset and a live session
//! rank by ONE order (the foundation under the union picker). A definition
//! appears in this catalog the moment it is authored or saved
//! (`SessionDefinition::from_live`), *before* any live instance exists, so
//! a not-running preset is searchable, not invisible-until-spawned.
//!
//! Buildable-now: pure in-memory storage. *Persisting* the catalog (a
//! `PracaSnapshot.definitions` field) is the separate M2 leg.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tear_types::DefinitionId;

use crate::definition::SessionDefinition;
use crate::index::rank;

/// An in-memory catalog of latent session presets, searchable through the
/// same ranking as live sessions. Serde-friendly so it can persist in a
/// [`crate::PracaSnapshot`] (presets survive a restart).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DefinitionIndex {
    #[serde(default)]
    defs: Vec<SessionDefinition>,
}

impl DefinitionIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a definition, or replace the existing one with the same
    /// [`DefinitionId`]. Returns the replaced definition, if any.
    pub fn upsert(&mut self, def: SessionDefinition) -> Option<SessionDefinition> {
        if let Some(slot) = self.defs.iter_mut().find(|d| d.def_id == def.def_id) {
            Some(std::mem::replace(slot, def))
        } else {
            self.defs.push(def);
            None
        }
    }

    /// Remove a definition by id, returning it if present.
    pub fn remove(&mut self, id: DefinitionId) -> Option<SessionDefinition> {
        let pos = self.defs.iter().position(|d| d.def_id == id)?;
        Some(self.defs.remove(pos))
    }

    /// Look up a definition by id.
    #[must_use]
    pub fn get(&self, id: DefinitionId) -> Option<&SessionDefinition> {
        self.defs.iter().find(|d| d.def_id == id)
    }

    /// The definition bound to a project root, if any.
    #[must_use]
    pub fn by_project(&self, root: &Path) -> Option<&SessionDefinition> {
        self.defs.iter().find(|d| d.project_root.as_path() == root)
    }

    /// Every catalogued definition.
    #[must_use]
    pub fn all(&self) -> &[SessionDefinition] {
        &self.defs
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Search presets — reuses the SAME `rank` over `Searchable` that
    /// `SessionIndex::search` uses (empty query → frecency; non-empty →
    /// tier/quality/frecency/id). One comparator, two catalogs.
    #[must_use]
    pub fn search(&self, query: &str, now: u64) -> Vec<&SessionDefinition> {
        rank(&self.defs, query, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::NameStyle;

    fn def(root: &str, last_seen: u64, visits: u32) -> SessionDefinition {
        let mut d = SessionDefinition::single_pane(root, "/bin/zsh", NameStyle::Emoji, last_seen);
        d.visits = visits;
        d
    }

    #[test]
    fn upsert_replaces_by_definition_id() {
        let mut idx = DefinitionIndex::new();
        assert!(idx.upsert(def("/a", 1, 1)).is_none());
        // Same project root → same DefinitionId → replaces, doesn't duplicate.
        let old = idx.upsert(def("/a", 9, 5)).expect("replaced");
        assert_eq!(old.visits, 1);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.by_project(Path::new("/a")).unwrap().visits, 5);
    }

    #[test]
    fn get_and_remove_by_id() {
        let mut idx = DefinitionIndex::new();
        let d = def("/proj", 0, 1);
        let id = d.def_id;
        idx.upsert(d);
        assert!(idx.get(id).is_some());
        assert_eq!(idx.remove(id).unwrap().def_id, id);
        assert!(idx.get(id).is_none());
        assert!(idx.is_empty());
    }

    #[test]
    fn search_ranks_presets_by_frecency_when_query_empty() {
        let mut idx = DefinitionIndex::new();
        idx.upsert(def("/low", 100, 1));
        idx.upsert(def("/high", 100, 50));
        let ranked = idx.search("", 100);
        assert_eq!(ranked.len(), 2);
        // Higher-frecency preset first — the same rule SessionIndex uses.
        assert_eq!(ranked[0].project_root.as_path(), Path::new("/high"));
    }

    #[test]
    fn search_fuzzy_matches_a_preset_by_path() {
        let mut idx = DefinitionIndex::new();
        idx.upsert(def("/code/substrate", 100, 1));
        idx.upsert(def("/code/mado", 100, 1));
        let ranked = idx.search("subst", 100);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].project_root.as_path(), Path::new("/code/substrate"));
    }
}
