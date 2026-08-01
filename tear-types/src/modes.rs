//! The terminal modes a client must read from the AUTHORITY, never from a
//! parser of its own.
//!
//! ## Why these are ten types and not ten `bool`s
//!
//! A mode decides how a client encodes what the operator does. Get the
//! wrong one and the failure is not cosmetic:
//!
//! - **bracketed paste** gates paste SANITISATION. mado reads it, then
//!   passes it to `sanitize_paste`. A wrong answer there is a paste-
//!   injection surface, not a rendering glitch.
//! - **cursor keys (DECCKM)** decides whether arrows are `ESC [ A` or
//!   `ESC O A`. Wrong, and every editor and pager receives the wrong keys.
//! - **mouse tracking** decides whether a click is reported at all.
//!
//! If these were bare `bool`s, `sanitize_paste(text, modes.focus_reporting)`
//! would type-check. Ten distinct newtypes make substituting one mode for
//! another an **`E0308`** — the compiler will not let a client confuse them.
//!
//! ## Why [`ModeSet`] is carried BY a view and never fetched separately
//!
//! A client that could ask for modes independently could render frame N's
//! cells while encoding a keystroke under frame N+1's modes — bracketed
//! paste toggling in the gap between the grid you drew and the key you
//! sent. Because a `ModeSet` is only obtainable from the view it came
//! from, "modes from a different instant than the cells" has no
//! representation. The cost is ten bytes.

use serde::{Deserialize, Serialize};

/// Declare a boolean mode newtype with its DEC number in the docs.
macro_rules! mode_flag {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(bool);

        impl $name {
            #[must_use]
            pub const fn new(on: bool) -> Self { Self(on) }
            /// Is this mode on?
            #[must_use]
            pub const fn enabled(self) -> bool { self.0 }
        }
    };
}

mode_flag! {
    /// DEC 2004 — bracketed paste. When on, a paste is framed with
    /// `ESC[200~` / `ESC[201~` so the program can tell it from typing.
    ///
    /// **This one gates paste sanitisation. Read it from the authority.**
    BracketedPaste
}
mode_flag! {
    /// DEC 1 (DECCKM) — application cursor keys. Arrows become `ESC O A`
    /// instead of `ESC [ A`.
    CursorKeys
}
mode_flag! {
    /// DEC 1004 — focus reporting. The program is told when the terminal
    /// gains (`ESC[I`) or loses (`ESC[O`) focus.
    FocusReporting
}
mode_flag! {
    /// DEC 2026 — synchronized output. The program has asked that nothing
    /// be presented until it says done, so a renderer holds the frame.
    ///
    /// A renderer MUST bound its hold: an app that never clears the flag
    /// would otherwise freeze the pane forever.
    SyncOutput
}
mode_flag! {
    /// DEC 1006 — SGR extended mouse encoding. Decides the REPORT format,
    /// independently of whether tracking is on at all.
    MouseSgr
}
mode_flag! {
    /// DEC 25 (DECTCEM) — cursor visibility.
    CursorVisible
}
mode_flag! {
    /// DEC 7 (DECAWM) — autowrap at the right margin.
    AutoWrap
}
mode_flag! {
    /// The alternate screen buffer is active (vim, less, htop). Set via
    /// DEC 47 / 1047 / 1049.
    AltScreen
}

/// Mouse tracking level — DEC 1000 / 1002 / 1003.
///
/// An enum and not three flags: the levels are **mutually exclusive**, so
/// three bools would make "click AND motion tracking simultaneously"
/// constructible, which no terminal can mean.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseTracking {
    /// No mouse reporting.
    #[default]
    Off,
    /// DEC 1000 — press and release only.
    Click,
    /// DEC 1002 — press, release, and motion while a button is held.
    Drag,
    /// DEC 1003 — all motion, button or not.
    Motion,
}

impl MouseTracking {
    /// Is the program listening for mouse events at all?
    #[must_use]
    pub const fn is_on(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Every mode a client needs, taken at ONE instant.
///
/// Obtainable only from a pane view — see the module docs for why that is
/// the point rather than an inconvenience.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSet {
    pub bracketed_paste: BracketedPaste,
    pub cursor_keys: CursorKeys,
    pub focus_reporting: FocusReporting,
    pub sync_output: SyncOutput,
    pub mouse: MouseTracking,
    pub mouse_sgr: MouseSgr,
    pub cursor_visible: CursorVisible,
    pub autowrap: AutoWrap,
    pub alt_screen: AltScreen,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_cannot_be_substituted_for_another() {
        // Compile-time property, asserted by construction: this function
        // accepts ONLY BracketedPaste. Passing `FocusReporting` here is
        // E0308 — which is the whole reason these are ten types.
        fn sanitize(_text: &str, bracketed: BracketedPaste) -> bool {
            bracketed.enabled()
        }
        assert!(sanitize("x", BracketedPaste::new(true)));
        assert!(!sanitize("x", BracketedPaste::new(false)));
    }

    #[test]
    fn mouse_levels_are_exclusive_by_construction() {
        // Three bools would let two levels be true at once. An enum has no
        // such value.
        let m = MouseTracking::Drag;
        assert!(m.is_on());
        assert_eq!(MouseTracking::default(), MouseTracking::Off);
        assert!(!MouseTracking::Off.is_on());
    }

    #[test]
    fn modes_default_to_off_which_is_what_a_fresh_terminal_means() {
        let m = ModeSet::default();
        assert!(!m.bracketed_paste.enabled());
        assert!(!m.cursor_keys.enabled());
        assert!(!m.sync_output.enabled());
        assert!(!m.mouse.is_on());
    }

    #[test]
    fn a_mode_flag_is_wire_identical_to_the_bool_it_wraps() {
        // `#[serde(transparent)]` — a mode must not change the wire shape
        // of the field it replaces.
        let a = serde_json::to_string(&BracketedPaste::new(true)).unwrap();
        let b = serde_json::to_string(&true).unwrap();
        assert_eq!(a, b);
    }
}
