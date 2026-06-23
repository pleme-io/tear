//! The 1→N map from a session *definition* to its live *instances*.
//!
//! The pressure-test's illegal state #2: the old [`crate::binding::ProjectBinding`]
//! is a `BTreeMap<PathBuf, SessionId>` — strictly 1:1, so *two concurrent
//! live instances of one project's definition* was unrepresentable. The
//! [`InstanceRegistry`] replaces it with a `BTreeMap<DefinitionId,
//! BTreeSet<InstanceId>>`: the value is a SET, so N concurrent instances
//! is the only expressible shape — the 1:1 limitation simply has no way
//! to be written down. A reverse index keeps reaping O(log n).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use tear_types::{DefinitionId, InstanceId};

/// Maps each [`DefinitionId`] to the set of live [`InstanceId`]s realizing
/// it, with a reverse index for O(log n) reap. Replaces the 1:1
/// `ProjectBinding` — here a definition can have many concurrent live
/// instances, and that is the natural (only) shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstanceRegistry {
    /// definition → its live instances. An empty set is pruned (a
    /// definition with no live instances simply isn't a key).
    live: BTreeMap<DefinitionId, BTreeSet<InstanceId>>,
    /// instance → its owning definition, so `reap` is O(log n).
    owner: BTreeMap<InstanceId, DefinitionId>,
}

impl InstanceRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `instance` is a live incarnation of `def`. Idempotent:
    /// re-registering the same pair is a no-op. Returns `true` if this was
    /// a new instance for that definition.
    pub fn register(&mut self, def: DefinitionId, instance: InstanceId) -> bool {
        self.owner.insert(instance, def);
        self.live.entry(def).or_default().insert(instance)
    }

    /// The live instances of `def`, in id order (empty if none).
    #[must_use]
    pub fn instances_of(&self, def: DefinitionId) -> impl Iterator<Item = InstanceId> + '_ {
        self.live.get(&def).into_iter().flatten().copied()
    }

    /// How many live instances `def` has.
    #[must_use]
    pub fn instance_count(&self, def: DefinitionId) -> usize {
        self.live.get(&def).map_or(0, BTreeSet::len)
    }

    /// The lowest-id live instance of `def`, if any — the deterministic
    /// "the one to switch to" when a caller wants a single existing
    /// instance (e.g. `cd` auto-attach).
    #[must_use]
    pub fn first_instance(&self, def: DefinitionId) -> Option<InstanceId> {
        self.live.get(&def).and_then(|s| s.iter().next().copied())
    }

    /// The definition an instance belongs to (`None` if unknown).
    #[must_use]
    pub fn def_for(&self, instance: InstanceId) -> Option<DefinitionId> {
        self.owner.get(&instance).copied()
    }

    /// Is `instance` known to be live?
    #[must_use]
    pub fn is_live(&self, instance: InstanceId) -> bool {
        self.owner.contains_key(&instance)
    }

    /// Remove a dead instance. Returns the definition it belonged to (if
    /// known); prunes the definition's entry when its last instance dies.
    pub fn reap(&mut self, instance: InstanceId) -> Option<DefinitionId> {
        let def = self.owner.remove(&instance)?;
        if let Some(set) = self.live.get_mut(&def) {
            set.remove(&instance);
            if set.is_empty() {
                self.live.remove(&def);
            }
        }
        Some(def)
    }

    /// Every definition that currently has at least one live instance.
    #[must_use]
    pub fn live_definitions(&self) -> impl Iterator<Item = DefinitionId> + '_ {
        self.live.keys().copied()
    }

    /// Total live instances across all definitions.
    #[must_use]
    pub fn total_instances(&self) -> usize {
        self.owner.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.owner.is_empty()
    }

    /// Lossless migration from the old 1:1 [`crate::binding::ProjectBinding`]
    /// iteration: each `(project_root, instance)` becomes
    /// `register(DefinitionId::from_project(root), instance)`. A 1:1 map
    /// is just the degenerate case where every definition has exactly one
    /// instance — so no information is lost moving up to 1:N.
    pub fn from_project_bindings<'a, I>(bindings: I) -> Self
    where
        I: IntoIterator<Item = (&'a Path, InstanceId)>,
    {
        let mut reg = Self::new();
        for (root, instance) in bindings {
            reg.register(DefinitionId::from_project(root), instance);
        }
        reg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tear_types::SessionId;

    #[test]
    fn one_definition_holds_many_instances() {
        let mut reg = InstanceRegistry::new();
        let def = DefinitionId::from_project(Path::new("/code/mado"));
        reg.register(def, SessionId(1));
        reg.register(def, SessionId(2));
        reg.register(def, SessionId(3));
        // The shape the old 1:1 ProjectBinding could not represent.
        assert_eq!(reg.instance_count(def), 3);
        assert_eq!(reg.instances_of(def).collect::<Vec<_>>(), vec![SessionId(1), SessionId(2), SessionId(3)]);
        assert_eq!(reg.first_instance(def), Some(SessionId(1)));
    }

    #[test]
    fn reap_prunes_definition_when_last_instance_dies() {
        let mut reg = InstanceRegistry::new();
        let def = DefinitionId::from_project(Path::new("/x"));
        reg.register(def, SessionId(1));
        reg.register(def, SessionId(2));
        assert_eq!(reg.reap(SessionId(1)), Some(def));
        assert_eq!(reg.instance_count(def), 1);
        assert!(reg.is_live(SessionId(2)));
        assert_eq!(reg.reap(SessionId(2)), Some(def));
        // Last instance gone → definition pruned entirely.
        assert_eq!(reg.instance_count(def), 0);
        assert_eq!(reg.live_definitions().count(), 0);
        assert!(reg.is_empty());
    }

    #[test]
    fn def_for_reverses_the_mapping() {
        let mut reg = InstanceRegistry::new();
        let a = DefinitionId::from_project(Path::new("/a"));
        let b = DefinitionId::from_project(Path::new("/b"));
        reg.register(a, SessionId(10));
        reg.register(b, SessionId(20));
        assert_eq!(reg.def_for(SessionId(10)), Some(a));
        assert_eq!(reg.def_for(SessionId(20)), Some(b));
        assert_eq!(reg.def_for(SessionId(99)), None);
    }

    #[test]
    fn register_is_idempotent() {
        let mut reg = InstanceRegistry::new();
        let def = DefinitionId::from_project(Path::new("/x"));
        assert!(reg.register(def, SessionId(1)));
        assert!(!reg.register(def, SessionId(1))); // already present
        assert_eq!(reg.instance_count(def), 1);
    }

    #[test]
    fn migration_from_one_to_one_bindings_is_lossless() {
        // The old 1:1 world: two projects, one session each.
        let bindings: Vec<(&Path, InstanceId)> = vec![
            (Path::new("/a"), SessionId(1)),
            (Path::new("/b"), SessionId(2)),
        ];
        let reg = InstanceRegistry::from_project_bindings(bindings);
        assert_eq!(reg.def_for(SessionId(1)), Some(DefinitionId::from_project(Path::new("/a"))));
        assert_eq!(reg.def_for(SessionId(2)), Some(DefinitionId::from_project(Path::new("/b"))));
        assert_eq!(reg.total_instances(), 2);
    }
}
