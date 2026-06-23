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

use ishou_tokens::{FleetSessionNames, SessionIdentity, SessionName, SessionNameStyle, SessionTheme};
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

/// Serde-friendly mirror of [`SessionTheme`] (the ishou enum is not
/// `Serialize`/`Deserialize`). Converts losslessly both ways.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMirror {
    /// Frost register (elemental / celestial / natural).
    #[default]
    Frost,
    /// Brazil register (warm tropical).
    Brazil,
}

impl From<SessionTheme> for ThemeMirror {
    fn from(t: SessionTheme) -> Self {
        match t {
            SessionTheme::Frost => ThemeMirror::Frost,
            SessionTheme::Brazil => ThemeMirror::Brazil,
        }
    }
}

impl From<ThemeMirror> for SessionTheme {
    fn from(t: ThemeMirror) -> Self {
        match t {
            ThemeMirror::Frost => SessionTheme::Frost,
            ThemeMirror::Brazil => SessionTheme::Brazil,
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
/// Resolve the emoji identity for a `(name_seed, theme)` pair — the one
/// place the fleet name lattice is consulted. Shared by [`SessionRecord`]
/// and `SessionDefinition` so the naming logic lives once (the prime
/// directive: a pattern used twice becomes a shared helper).
#[must_use]
pub fn identity_for(name_seed: u64, theme: Option<ThemeMirror>) -> SessionIdentity {
    match theme {
        Some(t) => FleetSessionNames::in_theme(name_seed, t.into()),
        None => FleetSessionNames::identity(name_seed),
    }
}

/// Resolve the display name for a `(name_seed, style, theme, custom_name)`
/// tuple: the operator's custom name if set, else the rendered themed
/// emoji name (`🌊 tide`). The shared counterpart to [`identity_for`].
#[must_use]
pub fn display_name_for(
    name_seed: u64,
    name_style: NameStyle,
    theme: Option<ThemeMirror>,
    custom_name: Option<&str>,
) -> String {
    match custom_name {
        Some(n) => n.to_string(),
        None => SessionName {
            identity: identity_for(name_seed, theme),
            style: name_style.into(),
        }
        .to_string(),
    }
}

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
    /// Operator-chosen name overriding the emoji identity. `None` = use the
    /// themed emoji name; `Some` = the operator renamed it (jumped in and
    /// gave it a human name). Searched at the highest tier.
    #[serde(default)]
    pub custom_name: Option<String>,
    /// Theme the (random) name is drawn from. `None` = whole-pool
    /// (project-bound sessions, path-stable); `Some` = a themed ad-hoc
    /// session drawing a random name from one register.
    #[serde(default)]
    pub theme: Option<ThemeMirror>,
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
            custom_name: None,
            theme: None,
            state: SessionState::Live,
        }
    }

    /// Construct a fresh AD-HOC session with a **random themed** name. Unlike
    /// [`Self::for_project`] (path-stable), this draws a deterministic name
    /// from `theme` (Frost / Brazil) keyed on `name_seed` — the caller passes
    /// a per-session seed (e.g. the session counter or a hash of the id) so
    /// the name is stable for THIS session but varies session-to-session.
    /// The operator can [`Self::rename`] it after jumping in.
    #[must_use]
    pub fn for_adhoc(
        id: SessionId,
        name_seed: u64,
        theme: SessionTheme,
        cwd: PathBuf,
        style: SessionNameStyle,
        last_seen: u64,
    ) -> Self {
        Self {
            id,
            name_seed,
            name_style: style.into(),
            project_root: cwd.clone(),
            cwd,
            visits: 1,
            last_seen,
            tags: Vec::new(),
            custom_name: None,
            theme: Some(theme.into()),
            state: SessionState::Live,
        }
    }

    /// The resolved emoji identity — themed via [`FleetSessionNames::in_theme`]
    /// when a theme is set, else the whole-pool [`FleetSessionNames::identity`].
    #[must_use]
    pub fn identity(&self) -> SessionIdentity {
        identity_for(self.name_seed, self.theme)
    }

    /// The session's display name: the operator's custom name if set, else
    /// the rendered themed emoji name (`🌊 tide`).
    #[must_use]
    pub fn display_name(&self) -> String {
        display_name_for(self.name_seed, self.name_style, self.theme, self.custom_name.as_deref())
    }

    /// Rename the session (operator jumped in and gave it a human name).
    /// An empty / whitespace-only name clears the override, reverting to the
    /// emoji name.
    pub fn rename(&mut self, name: impl Into<String>) {
        let n = name.into();
        self.custom_name = if n.trim().is_empty() { None } else { Some(n) };
    }

    /// The identity's search keywords (`wave`/`water`/… for `🌊 tide`) — the
    /// index matches these so "type wave, find the tide session" works.
    #[must_use]
    pub fn keywords(&self) -> &'static [&'static str] {
        self.identity().keywords
    }

    /// Reconstruct the live [`SessionName`] from the stored
    /// `(seed, style)` pair. Pure + deterministic.
    #[must_use]
    pub fn name(&self) -> SessionName {
        SessionName { identity: self.identity(), style: self.name_style.into() }
    }

    /// The session's name *word* (`"tide"`, `"frost"`, …) — the stable,
    /// style-independent token used for fuzzy search matching.
    #[must_use]
    pub fn name_word(&self) -> &'static str {
        self.identity().word
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
            custom_name: Some("billing-stack".into()),
            theme: Some(ThemeMirror::Brazil),
            state: SessionState::Saved,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
        assert_eq!(rec.name().to_string(), back.name().to_string());
    }

    #[test]
    fn rename_overrides_display_then_clears() {
        let mut r =
            SessionRecord::for_project(sid("r"), PathBuf::from("/x"), SessionNameStyle::Emoji, 1);
        let emoji_word = r.name_word();
        r.rename("billing-stack");
        assert_eq!(r.display_name(), "billing-stack");
        assert_eq!(r.custom_name.as_deref(), Some("billing-stack"));
        // Whitespace-only reverts to the emoji name.
        r.rename("   ");
        assert!(r.custom_name.is_none());
        assert!(r.display_name().contains(emoji_word));
    }

    #[test]
    fn adhoc_draws_a_deterministic_themed_name() {
        let r = SessionRecord::for_adhoc(
            sid("a"),
            5,
            SessionTheme::Brazil,
            PathBuf::from("/tmp/scratch"),
            SessionNameStyle::Emoji,
            1,
        );
        assert_eq!(r.identity().theme, SessionTheme::Brazil, "ad-hoc name stays in theme");
        assert!(!r.name_word().is_empty());
        // Stable for this session (same seed+theme → same name).
        let again = SessionRecord::for_adhoc(
            sid("a"),
            5,
            SessionTheme::Brazil,
            PathBuf::from("/tmp/scratch"),
            SessionNameStyle::Emoji,
            1,
        );
        assert_eq!(r.name_word(), again.name_word());
    }
}
