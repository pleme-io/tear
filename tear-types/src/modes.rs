//! The terminal modes a client must read from the AUTHORITY, never from a
//! parser of its own.
//!
//! ## Why these are types and not `bool`s
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
//! would type-check. One newtype per mode makes substituting one mode for
//! another an **`E0308`** — the compiler will not let a client confuse them.
//!
//! ## Why [`ModeSet`] is carried BY a view and never fetched separately
//!
//! A client that could ask for modes independently could render frame N's
//! cells while encoding a keystroke under frame N+1's modes — bracketed
//! paste toggling in the gap between the grid you drew and the key you
//! sent. Because a `ModeSet` is only obtainable from the view it came
//! from, "modes from a different instant than the cells" has no
//! representation. The cost is a few dozen bytes.

use std::io::Write;

use serde::{Deserialize, Serialize};

/// Declare a boolean mode newtype with its DEC number in the docs.
macro_rules! mode_flag {
    (@default) => { false };
    (@default $on:literal) => { $on };
    ($(#[$m:meta])* $name:ident $(= $on:literal)?) => {
        $(#[$m])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(bool);

        impl Default for $name {
            fn default() -> Self {
                Self(mode_flag!(@default $($on)?))
            }
        }

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
    /// DEC 25 (DECTCEM) — cursor visibility. On in a fresh terminal.
    CursorVisible = true
}
mode_flag! {
    /// DEC 7 (DECAWM) — autowrap at the right margin. On in a fresh terminal.
    AutoWrap = true
}
mode_flag! {
    /// The alternate screen buffer is active (vim, less, htop). Set via
    /// DEC 47 / 1047 / 1049.
    AltScreen
}

mode_flag! {
    KeypadApplication
}
mode_flag! {
    ReverseVideo
}
mode_flag! {
    AlternateScroll
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

    #[must_use]
    pub const fn mode(self) -> Option<u16> {
        match self {
            Self::Off => None,
            Self::Click => Some(1000),
            Self::Drag => Some(1002),
            Self::Motion => Some(1003),
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseEncoding {
    #[default]
    X10,
    Utf8,
    Sgr,
    Urxvt,
    SgrPixels,
}

impl MouseEncoding {
    pub const MODES: [u16; 4] = [1005, 1006, 1015, 1016];

    #[must_use]
    pub const fn from_mode(code: u16) -> Option<Self> {
        match code {
            1005 => Some(Self::Utf8),
            1006 => Some(Self::Sgr),
            1015 => Some(Self::Urxvt),
            1016 => Some(Self::SgrPixels),
            _ => None,
        }
    }

    #[must_use]
    pub const fn mode(self) -> Option<u16> {
        match self {
            Self::X10 => None,
            Self::Utf8 => Some(1005),
            Self::Sgr => Some(1006),
            Self::Urxvt => Some(1015),
            Self::SgrPixels => Some(1016),
        }
    }

    #[must_use]
    pub fn set(self, code: u16, on: bool) -> Self {
        match Self::from_mode(code) {
            Some(e) if on => e,
            Some(e) if e == self => Self::X10,
            _ => self,
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorStyle {
    #[default]
    Unset,
    Default,
    BlinkingBlock,
    SteadyBlock,
    BlinkingUnderline,
    SteadyUnderline,
    BlinkingBar,
    SteadyBar,
}

impl CursorStyle {
    #[must_use]
    pub const fn from_ps(ps: u16) -> Option<Self> {
        match ps {
            0 => Some(Self::Default),
            1 => Some(Self::BlinkingBlock),
            2 => Some(Self::SteadyBlock),
            3 => Some(Self::BlinkingUnderline),
            4 => Some(Self::SteadyUnderline),
            5 => Some(Self::BlinkingBar),
            6 => Some(Self::SteadyBar),
            _ => None,
        }
    }

    #[must_use]
    pub const fn ps(self) -> Option<u16> {
        match self {
            Self::Unset => None,
            Self::Default => Some(0),
            Self::BlinkingBlock => Some(1),
            Self::SteadyBlock => Some(2),
            Self::BlinkingUnderline => Some(3),
            Self::SteadyUnderline => Some(4),
            Self::BlinkingBar => Some(5),
            Self::SteadyBar => Some(6),
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModifyOtherKeys {
    #[default]
    Off,
    ExceptWellKnown,
    All,
}

impl ModifyOtherKeys {
    #[must_use]
    pub const fn from_level(level: u16) -> Option<Self> {
        match level {
            0 => Some(Self::Off),
            1 => Some(Self::ExceptWellKnown),
            2 => Some(Self::All),
            _ => None,
        }
    }

    #[must_use]
    pub const fn level(self) -> u16 {
        match self {
            Self::Off => 0,
            Self::ExceptWellKnown => 1,
            Self::All => 2,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KittySet {
    Replace,
    Union,
    Difference,
}

impl KittySet {
    #[must_use]
    pub const fn from_mode(mode: u16) -> Option<Self> {
        match mode {
            1 => Some(Self::Replace),
            2 => Some(Self::Union),
            3 => Some(Self::Difference),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct KittyFlagStack {
    len: u8,
    flags: [u16; KittyFlagStack::DEPTH],
}

impl KittyFlagStack {
    pub const DEPTH: usize = 8;

    #[must_use]
    pub fn entries(&self) -> &[u16] {
        &self.flags[..usize::from(self.len)]
    }

    #[must_use]
    pub fn current(&self) -> u16 {
        self.entries().last().copied().unwrap_or(0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, flags: u16) {
        if usize::from(self.len) == Self::DEPTH {
            self.flags.copy_within(1.., 0);
            self.flags[Self::DEPTH - 1] = flags;
        } else {
            self.flags[usize::from(self.len)] = flags;
            self.len += 1;
        }
    }

    pub fn pop(&mut self, n: usize) {
        let keep = usize::from(self.len).saturating_sub(n);
        self.flags[keep..].fill(0);
        self.len = u8::try_from(keep).unwrap_or(0);
    }

    pub fn set(&mut self, flags: u16, how: KittySet) {
        if self.is_empty() {
            self.push(0);
        }
        let top = &mut self.flags[usize::from(self.len) - 1];
        *top = match how {
            KittySet::Replace => flags,
            KittySet::Union => *top | flags,
            KittySet::Difference => *top & !flags,
        };
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn write_restore(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[<99u");
        for f in self.entries() {
            let _ = write!(out, "\x1b[>{f}u");
        }
    }
}

impl Serialize for KittyFlagStack {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.entries().serialize(s)
    }
}

impl<'de> Deserialize<'de> for KittyFlagStack {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let all = Vec::<u16>::deserialize(d)?;
        let mut stack = Self::default();
        for f in &all[all.len().saturating_sub(Self::DEPTH)..] {
            stack.push(*f);
        }
        Ok(stack)
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KittyKeyboard {
    #[serde(default)]
    pub main: KittyFlagStack,
    #[serde(default)]
    pub alternate: KittyFlagStack,
}

impl KittyKeyboard {
    #[must_use]
    pub const fn screen(&self, alternate: bool) -> &KittyFlagStack {
        if alternate {
            &self.alternate
        } else {
            &self.main
        }
    }

    pub const fn screen_mut(&mut self, alternate: bool) -> &mut KittyFlagStack {
        if alternate {
            &mut self.alternate
        } else {
            &mut self.main
        }
    }
}

/// Every mode a client needs, taken at ONE instant.
///
/// Obtainable only from a pane view — see the module docs for why that is
/// the point rather than an inconvenience.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ModeSetWire", into = "ModeSetWire")]
pub struct ModeSet {
    pub bracketed_paste: BracketedPaste,
    pub cursor_keys: CursorKeys,
    pub focus_reporting: FocusReporting,
    pub sync_output: SyncOutput,
    pub mouse: MouseTracking,
    pub mouse_encoding: MouseEncoding,
    pub cursor_visible: CursorVisible,
    pub autowrap: AutoWrap,
    pub alt_screen: AltScreen,
    pub keypad: KeypadApplication,
    pub reverse_video: ReverseVideo,
    pub alternate_scroll: AlternateScroll,
    pub cursor_style: CursorStyle,
    pub modify_other_keys: ModifyOtherKeys,
    pub kitty_keyboard: KittyKeyboard,
}

impl ModeSet {
    #[must_use]
    pub fn mouse_sgr(&self) -> MouseSgr {
        MouseSgr::new(self.mouse_encoding == MouseEncoding::Sgr)
    }

    pub fn write_restore(&self, out: &mut Vec<u8>, alternate: bool) {
        let dec = |out: &mut Vec<u8>, code: u16, on: bool| {
            let _ = write!(out, "\x1b[?{code}{}", if on { 'h' } else { 'l' });
        };
        dec(out, 1, self.cursor_keys.enabled());
        out.extend_from_slice(if self.keypad.enabled() {
            b"\x1b="
        } else {
            b"\x1b>"
        });
        dec(out, 5, self.reverse_video.enabled());
        dec(out, 7, self.autowrap.enabled());
        dec(out, 25, self.cursor_visible.enabled());
        out.extend_from_slice(b"\x1b[?1000;1002;1003l");
        if let Some(code) = self.mouse.mode() {
            dec(out, code, true);
        }
        out.extend_from_slice(b"\x1b[?1005;1006;1015;1016l");
        if let Some(code) = self.mouse_encoding.mode() {
            dec(out, code, true);
        }
        dec(out, 1004, self.focus_reporting.enabled());
        dec(out, 1007, self.alternate_scroll.enabled());
        dec(out, 2004, self.bracketed_paste.enabled());
        if let Some(ps) = self.cursor_style.ps() {
            let _ = write!(out, "\x1b[{ps} q");
        }
        let _ = write!(out, "\x1b[>4;{}m", self.modify_other_keys.level());
        if alternate {
            self.kitty_keyboard.alternate.write_restore(out);
        }
        dec(out, 2026, self.sync_output.enabled());
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct ModeSetWire {
    bracketed_paste: BracketedPaste,
    cursor_keys: CursorKeys,
    focus_reporting: FocusReporting,
    sync_output: SyncOutput,
    mouse: MouseTracking,
    mouse_sgr: MouseSgr,
    cursor_visible: CursorVisible,
    autowrap: AutoWrap,
    alt_screen: AltScreen,
    mouse_encoding: Option<MouseEncoding>,
    keypad: KeypadApplication,
    reverse_video: ReverseVideo,
    alternate_scroll: AlternateScroll,
    cursor_style: CursorStyle,
    modify_other_keys: ModifyOtherKeys,
    kitty_keyboard: KittyKeyboard,
}

impl From<ModeSet> for ModeSetWire {
    fn from(m: ModeSet) -> Self {
        Self {
            bracketed_paste: m.bracketed_paste,
            cursor_keys: m.cursor_keys,
            focus_reporting: m.focus_reporting,
            sync_output: m.sync_output,
            mouse: m.mouse,
            mouse_sgr: m.mouse_sgr(),
            cursor_visible: m.cursor_visible,
            autowrap: m.autowrap,
            alt_screen: m.alt_screen,
            mouse_encoding: Some(m.mouse_encoding),
            keypad: m.keypad,
            reverse_video: m.reverse_video,
            alternate_scroll: m.alternate_scroll,
            cursor_style: m.cursor_style,
            modify_other_keys: m.modify_other_keys,
            kitty_keyboard: m.kitty_keyboard,
        }
    }
}

impl From<ModeSetWire> for ModeSet {
    fn from(w: ModeSetWire) -> Self {
        Self {
            bracketed_paste: w.bracketed_paste,
            cursor_keys: w.cursor_keys,
            focus_reporting: w.focus_reporting,
            sync_output: w.sync_output,
            mouse: w.mouse,
            mouse_encoding: w.mouse_encoding.unwrap_or(if w.mouse_sgr.enabled() {
                MouseEncoding::Sgr
            } else {
                MouseEncoding::X10
            }),
            cursor_visible: w.cursor_visible,
            autowrap: w.autowrap,
            alt_screen: w.alt_screen,
            keypad: w.keypad,
            reverse_video: w.reverse_video,
            alternate_scroll: w.alternate_scroll,
            cursor_style: w.cursor_style,
            modify_other_keys: w.modify_other_keys,
            kitty_keyboard: w.kitty_keyboard,
        }
    }
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

    #[test]
    fn a_fresh_terminal_shows_its_cursor_and_wraps() {
        let m = ModeSet::default();
        assert!(m.cursor_visible.enabled());
        assert!(m.autowrap.enabled());
        assert_eq!(m.cursor_style, CursorStyle::Unset);
        assert_eq!(m.mouse_encoding, MouseEncoding::X10);
        assert!(m.kitty_keyboard.main.is_empty() && m.kitty_keyboard.alternate.is_empty());
    }

    #[test]
    fn a_kitty_stack_pushes_pops_sets_and_evicts_its_oldest_entry() {
        let mut k = KittyFlagStack::default();
        assert_eq!(k.current(), 0);
        k.push(1);
        k.push(3);
        assert_eq!(k.entries(), &[1, 3]);
        k.set(4, KittySet::Union);
        assert_eq!(k.current(), 7);
        k.set(1, KittySet::Difference);
        assert_eq!(k.current(), 6);
        k.pop(1);
        assert_eq!(k.entries(), &[1]);
        k.pop(99);
        assert!(k.is_empty());
        k.set(5, KittySet::Replace);
        assert_eq!(k.entries(), &[5]);
        k.clear();
        for f in 0..10u16 {
            k.push(f);
        }
        assert_eq!(k.entries(), &[2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn a_mouse_encoding_is_reset_only_by_its_own_mode() {
        let e = MouseEncoding::X10.set(1006, true);
        assert_eq!(e, MouseEncoding::Sgr);
        assert_eq!(e.set(1015, false), MouseEncoding::Sgr);
        assert_eq!(e.set(1016, true), MouseEncoding::SgrPixels);
        assert_eq!(e.set(1006, false), MouseEncoding::X10);
        for code in MouseEncoding::MODES {
            assert_eq!(
                MouseEncoding::from_mode(code).and_then(MouseEncoding::mode),
                Some(code)
            );
        }
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    #[allow(clippy::struct_excessive_bools)]
    struct NineFieldModeSet {
        bracketed_paste: bool,
        cursor_keys: bool,
        focus_reporting: bool,
        sync_output: bool,
        mouse: MouseTracking,
        mouse_sgr: bool,
        cursor_visible: bool,
        autowrap: bool,
        alt_screen: bool,
    }

    fn cbor<T: Serialize>(v: &T) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::ser::into_writer(v, &mut out).unwrap();
        out
    }

    #[test]
    fn a_nine_field_mode_set_from_an_older_daemon_still_decodes() {
        let old = NineFieldModeSet {
            bracketed_paste: true,
            cursor_keys: true,
            focus_reporting: false,
            sync_output: false,
            mouse: MouseTracking::Click,
            mouse_sgr: true,
            cursor_visible: true,
            autowrap: true,
            alt_screen: true,
        };
        let m: ModeSet = ciborium::de::from_reader(&cbor(&old)[..]).unwrap();
        assert!(m.bracketed_paste.enabled() && m.cursor_keys.enabled());
        assert_eq!(m.mouse, MouseTracking::Click);
        assert_eq!(m.mouse_encoding, MouseEncoding::Sgr);
        assert_eq!(m.cursor_style, CursorStyle::Unset);
        assert_eq!(m.keypad, KeypadApplication::default());
    }

    #[test]
    fn an_older_reader_decodes_a_full_mode_set_and_keeps_its_sgr_flag() {
        let mut m = ModeSet {
            mouse_encoding: MouseEncoding::Sgr,
            cursor_style: CursorStyle::SteadyBar,
            modify_other_keys: ModifyOtherKeys::All,
            ..ModeSet::default()
        };
        m.kitty_keyboard.alternate.push(5);
        let old: NineFieldModeSet = ciborium::de::from_reader(&cbor(&m)[..]).unwrap();
        assert!(old.mouse_sgr);
        assert!(old.cursor_visible && old.autowrap);
        let back: ModeSet = ciborium::de::from_reader(&cbor(&m)[..]).unwrap();
        assert_eq!(back, m);
    }
}
