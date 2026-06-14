//! [`SessionRecord`] — the persisted, ranked, searchable unit praca
//! tracks for every session it knows about.
//!
//! A record carries the stable [`SessionId`], the project root the
//! session is bound to, the session's last cwd, frecency counters
//! (`visits` + `last_seen`), free-form `tags`, a lifecycle `state`, and
//! the resolved [`SessionName`].
//!
//! ## Why the name is stored as a (seed, style) mirror
//!
//! `ishou_tokens::SessionName` is `Copy` but NOT `Serialize` /
//! `Deserialize` (its identity holds `&'static str`s into the curated
//! atlas). Rather than bolt serde onto a foreign type, a record persists
//! the deterministic *inputs* — a `u64` `name_seed` and a serde-friendly
//! [`NameStyle`] mirror of [`SessionNameStyle`] — and reconstructs the
//! live `SessionName` via [`SessionRecord::name`]. The atlas is pure +
//! deterministic, so `(seed, style)` round-trips to byte-identical
//! display text across daemon restarts and across hosts.
//!
//! ## Time is injected
//!
//! `last_seen` is unix-seconds supplied by the CALLER. This crate never
//! reads the clock, so frecency + ranking stay deterministic and
//! testable.

use std::path::PathBuf;

use ishou_tokens::{FleetSessionNames, SessionName, SessionNameStyle};
use serde::{Deserialize, Serialize};
use tear_types::id::SessionId;

/// Lifecycle state of a tracked session, from praca's point of view.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// A running session backed by a live daemon session.
    Live,
    /// A session whose layout/cwd we persisted but whose PTYs are not
    /// currently running — restorable on attach.
    Saved,
    /// A reusable template (a saved shape with no specific instance) —
    /// "spawn me a session like this".
    Templated,
}

/// Serde-friendly mirror of [`SessionNameStyle`] (the ishou enum is not
/// `Serialize`/`Deserialize`). Converts losslessly both ways.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameStyle {
    /// Wide emoji + word (`🌊 tide`).
    #[default]
    Emoji,
    /// Clean single-width glyph + word (`≈ tide`).
    Glyph,
}

impl From<SessionNameStyle> for NameStyle {
    fn from(s: SessionNameStyle) -> Self {
        match s {
            SessionNameStyle::Emoji => NameStyle::Emoji,
            SessionNameStyle::Glyph => NameStyle::Glyph,
        }
    }
}

impl From<NameStyle> for SessionNameStyle {
    fn from(s: NameStyle) -> Self {
        match s {
            NameStyle::Emoji => SessionNameStyle::Emoji,
            NameStyle::Glyph => SessionNameStyle::Glyph,
        }
    }
}

/// One tracked session — the persisted, ranked, searchable record.
///
/// The session's display name is reconstructed from `name_seed` +
/// `name_style` via [`Self::name`]; the resolved [`SessionName`] is not
/// stored directly because it is not serde-friendly. `name_seed` is the
/// `ishou_tokens::stable_seed` of the project root (the same seed
/// `FleetSessionNames::from_project_path` uses), so a record's name is
/// stable for its project across restarts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Stable daemon handle.
    pub id: SessionId,
    /// Deterministic atlas seed (typically
    /// `ishou_tokens::stable_seed(project_root_bytes)`).
    pub name_seed: u64,
    /// Render style for the name.
    pub name_style: NameStyle,
    /// The project this session is bound to (cd here auto-attaches it).
    pub project_root: PathBuf,
    /// The session's most-recent working directory.
    pub cwd: PathBuf,
    /// Frecency: total times this session has been visited/touched.
    pub visits: u32,
    /// Frecency: unix-seconds of the last touch — INJECTED by the
    /// caller, never read from the clock here.
    pub last_seen: u64,
    /// Free-form operator tags, searchable in the index.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Lifecycle state.
    pub state: SessionState,
}

impl SessionRecord {
    /// Construct a record whose name seed is derived deterministically
    /// from `project_root` (the automation-default naming). `cwd`
    /// defaults to `project_root`; adjust afterward if the session was
    /// opened deeper in the tree.
    #[must_use]
    pub fn for_project(
        id: SessionId,
        project_root: PathBuf,
        style: SessionNameStyle,
        last_seen: u64,
    ) -> Self {
        let name_seed = ishou_tokens::fleet_session_names::stable_seed(
            project_root.to_string_lossy().as_bytes(),
        );
        Self {
            id,
            name_seed,
            name_style: style.into(),
            cwd: project_root.clone(),
            project_root,
            visits: 1,
            last_seen,
            tags: Vec::new(),
            state: SessionState::Live,
        }
    }

    /// Reconstruct the live [`SessionName`] from the stored
    /// `(seed, style)` pair. Pure + deterministic.
    #[must_use]
    pub fn name(&self) -> SessionName {
        FleetSessionNames::name(self.name_seed, self.name_style.into())
    }

    /// The session's name *word* (`"tide"`, `"frost"`, …) — the stable,
    /// style-independent token used for fuzzy search matching.
    #[must_use]
    pub fn name_word(&self) -> &'static str {
        FleetSessionNames::identity(self.name_seed).word
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sid(seed: &str) -> SessionId {
        SessionId::from_seed(seed)
    }

    #[test]
    fn style_mirror_round_trips() {
        for s in [SessionNameStyle::Emoji, SessionNameStyle::Glyph] {
            let mirror: NameStyle = s.into();
            let back: SessionNameStyle = mirror.into();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn for_project_derives_stable_name() {
        let root = PathBuf::from("/code/pleme-io/mado");
        let a = SessionRecord::for_project(sid("a"), root.clone(), SessionNameStyle::Emoji, 100);
        let b = SessionRecord::for_project(sid("b"), root.clone(), SessionNameStyle::Emoji, 200);
        // Same project root -> same name word + seed regardless of id/time.
        assert_eq!(a.name_seed, b.name_seed);
        assert_eq!(a.name_word(), b.name_word());
        // Matches what the atlas would have produced directly.
        let direct = FleetSessionNames::from_project_path(Path::new("/code/pleme-io/mado"), SessionNameStyle::Emoji);
        assert_eq!(a.name().to_string(), direct.to_string());
    }

    #[test]
    fn name_word_is_style_independent() {
        let root = PathBuf::from("/x/y/z");
        let e = SessionRecord::for_project(sid("e"), root.clone(), SessionNameStyle::Emoji, 0);
        let g = SessionRecord::for_project(sid("g"), root, SessionNameStyle::Glyph, 0);
        assert_eq!(e.name_word(), g.name_word());
        assert_ne!(e.name().to_string(), g.name().to_string());
    }

    #[test]
    fn record_serde_round_trips() {
        let rec = SessionRecord {
            id: sid("rt"),
            name_seed: 42,
            name_style: NameStyle::Glyph,
            project_root: PathBuf::from("/a/b"),
            cwd: PathBuf::from("/a/b/c"),
            visits: 7,
            last_seen: 1234,
            tags: vec!["infra".into(), "deploy".into()],
            state: SessionState::Saved,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
        assert_eq!(rec.name().to_string(), back.name().to_string());
    }
}
