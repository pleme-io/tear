//! Who answers a VT query on a pane — the Relay→Host transition.
//!
//! ## Why this is a type and not a behaviour change
//!
//! A running program can ASK the terminal things: "where is the cursor?"
//! (`CSI 6n`), "what are you?" (`CSI c`). Exactly one participant may
//! answer. Two answers is not a cosmetic bug — the second reply arrives on
//! the PTY as if the operator had typed it, so a shell sees a line of
//! garbage like `^[[24;80R` injected into its input.
//!
//! Today tear is a **relay**: it passes query bytes through to whatever
//! terminal is attached, and mado answers from its own parser. That is
//! pinned by `tear-core`'s espelho conformance rows, whose header states it
//! outright — *"`feed()` has no write-back surface at all, so it cannot
//! answer `ESC[6n` itself … the host duty lives one layer DOWN."*
//!
//! [`SHUKEN`](https://github.com/pleme-io/tear/blob/main/docs/SHUKEN.md)
//! changes that. Once `PaneGrid` is the sole VT authority, mado has no
//! parser and *cannot* answer — so if tear also does not, nothing does, and
//! every program that probes the terminal hangs waiting for a reply that no
//! longer exists. Prompt libraries using CPR and DA-based capability
//! detection are the common casualties.
//!
//! So the role must move Relay → Host. But it cannot move *today*, because
//! mado still parses: tear answering now would mean BOTH answer. This enum
//! is that transition made explicit and typed, defaulting to the shipped
//! behaviour, per ★★ MODULARIZE, DON'T DELETE — the relay path is
//! configured off, never removed.

use serde::{Deserialize, Serialize};

/// Which participant answers VT queries for a pane.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostRole {
    /// tear relays query bytes downstream and answers **nothing**. The
    /// attached terminal is the host.
    ///
    /// The DEFAULT, and the currently shipped behaviour — a pane in this
    /// role is byte-for-byte what tear did before the response path
    /// existed, which is what makes landing that path a no-op today.
    #[default]
    Relay,
    /// tear answers queries itself and queues the reply for write-back to
    /// the PTY.
    ///
    /// Required once the attached client has no parser of its own. Setting
    /// this while a parsing terminal is still attached produces DOUBLE
    /// replies — see the module doc.
    Host,
}

impl HostRole {
    #[must_use]
    pub const fn answers_queries(self) -> bool {
        matches!(self, Self::Host)
    }
}

/// What tear advertises about itself when it is the [`HostRole::Host`].
///
/// ## These constants are a PROMISE, not a copy
///
/// A DA reply is a capability advertisement: a program reads it and then
/// *uses* what it claims. So each field here has to be true of tear's own
/// renderer, and the temptation to paste mado's constants is a trap.
///
/// mado advertises `\x1b[?62;4;22c`. The `4` means **sixel**, and mado's own
/// comment notes it was added "since the decode path landed". tear has no
/// sixel: `GridState` implements no `hook`/`put`/`unhook`, so a DCS image
/// payload is swallowed by vte's default no-op. Advertising `4` would tell
/// every program on the system to send image data tear cannot draw.
///
/// So tear advertises VT220 + ANSI colour and nothing it cannot honour.
/// **When graphics land in `PaneGrid`, this constant moves in the same
/// commit** — that coupling is the point of it living beside the role.
pub struct TearCaps;

impl TearCaps {
    /// Primary DA (`CSI c`): VT220 (62) with ANSI colour (22).
    ///
    /// Deliberately WITHOUT `4` (sixel) — see the type doc.
    pub const PRIMARY_DA: &'static [u8] = b"\x1b[?62;22c";

    /// Secondary DA (`CSI > c`): terminal id 1, version 0, cartridge 0 —
    /// the same shape mado reports.
    pub const SECONDARY_DA: &'static [u8] = b"\x1b[>1;0;0c";

    /// Device status report, "terminal OK" (`CSI 5n`).
    pub const STATUS_OK: &'static [u8] = b"\x1b[0n";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_is_the_default_so_landing_the_host_path_changes_nothing() {
        assert_eq!(HostRole::default(), HostRole::Relay);
        assert!(!HostRole::default().answers_queries());
    }

    #[test]
    fn host_answers() {
        assert!(HostRole::Host.answers_queries());
    }

    /// tear must not advertise a capability it cannot honour. The `4`
    /// parameter is sixel; tear has no DCS hook, so claiming it would tell
    /// programs to send image data that gets silently swallowed.
    ///
    /// When sixel lands in `PaneGrid`, this test is what forces the
    /// advertisement to move with it.
    #[test]
    fn primary_da_does_not_advertise_sixel_tear_cannot_render() {
        let da = std::str::from_utf8(TearCaps::PRIMARY_DA).unwrap();
        let params: Vec<&str> = da
            .trim_start_matches("\x1b[?")
            .trim_end_matches('c')
            .split(';')
            .collect();
        assert!(
            !params.contains(&"4"),
            "PRIMARY_DA advertises sixel (4) but PaneGrid implements no \
             hook/put/unhook — either add the DCS path or drop the claim: {da:?}"
        );
        assert!(params.contains(&"62"), "should advertise VT220");
        assert!(params.contains(&"22"), "should advertise ANSI colour");
    }
}
