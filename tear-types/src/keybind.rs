//! Keybinding model — typed key chords + actions.
//!
//! Operators author bindings declaratively in the shikumi config
//! (`~/.config/tear/tear.yaml`). The same vocabulary applies to mado:
//! when mado embeds tear-core at tier 3, the multiplexer's bindings
//! and mado's bindings come from a unified table so muscle memory
//! transfers between the two apps.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A keybinding belongs to a named table — most live in the default
/// `"root"` table but tmux operators recognise `"prefix"` (active
/// after `C-b`) and `"copy"` (modal copy mode). Tear honours the
/// same names so dropped-in tmux configs feel native.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyTableName(pub String);

impl Default for KeyTableName {
    fn default() -> Self {
        Self::root()
    }
}

impl KeyTableName {
    pub fn root() -> Self {
        Self("root".into())
    }
    pub fn prefix() -> Self {
        Self("prefix".into())
    }
    pub fn copy() -> Self {
        Self("copy".into())
    }
}

/// A single key chord — modifiers + key. The string is the
/// canonical lowercase form: `"ctrl+a"`, `"alt+left"`, `"super+l"`,
/// `"f10"`. tmux's `C-a`, `M-Left` shorthand normalises through here.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyChord(pub String);

impl KeyChord {
    /// Build a KeyChord from a tmux-style shorthand like `"C-a"`.
    /// Returns the canonical normalised form.
    pub fn from_tmux(s: &str) -> Self {
        let mut parts: Vec<String> = Vec::new();
        let mut key = String::new();
        for seg in s.split('-') {
            match seg {
                "C" | "c" => parts.push("ctrl".into()),
                "M" | "m" => parts.push("alt".into()),
                "S" | "s" => parts.push("shift".into()),
                "D" | "d" => parts.push("super".into()),
                other => key = other.to_ascii_lowercase(),
            }
        }
        parts.push(key);
        Self(parts.join("+"))
    }
}

/// Action a keybinding fires. Variants intentionally mirror tmux
/// command names so a tmux user's mental model carries over.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Action {
    /// Create a new pane by splitting the active pane.
    SplitPane {
        direction: crate::Direction,
    },
    /// Send keys to the active pane as if typed.
    SendKeys {
        keys: String,
    },
    /// Run a tear/tmux command, e.g. `"new-window -n logs"`.
    Command {
        cmd: String,
    },
    /// Switch focus to a pane by relative direction.
    SelectPane {
        direction: crate::Direction,
    },
    /// Move to the next/previous window.
    NextWindow,
    PreviousWindow,
    /// Create a new window in the active session.
    NewWindow,
    /// Kill the active pane / window / session.
    KillPane,
    KillWindow,
    KillSession,
    /// Detach the current client.
    Detach,
    /// Reload the shikumi config — operator-driven hot-reload trigger
    /// in addition to the file-watcher path.
    ReloadConfig,
    /// Enter named key table — tmux's modal copy-mode etc.
    EnterTable {
        table: KeyTableName,
    },
}

/// Single binding row in a [`KeyTable`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyBind {
    pub chord: KeyChord,
    pub action: Action,
    /// Free-form note rendered by `tear keybinds list` — operators
    /// often forget what they bound; this is the affordance.
    #[serde(default)]
    pub note: String,
}

/// A named set of bindings. Tables are looked up by name at chord-
/// dispatch time; the "root" table is the implicit default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyTable {
    pub name: KeyTableName,
    pub bindings: Vec<KeyBind>,
}

/// The full keybinding store: every table by name. Lives on
/// [`crate::TearTheme`] / the shikumi config so a reload swaps the
/// whole map atomically.
pub type KeyTableMap = BTreeMap<KeyTableName, KeyTable>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_tmux_normalises_chord_shorthand() {
        assert_eq!(KeyChord::from_tmux("C-a").0, "ctrl+a");
        assert_eq!(KeyChord::from_tmux("M-Left").0, "alt+left");
        assert_eq!(KeyChord::from_tmux("C-M-x").0, "ctrl+alt+x");
    }
}
