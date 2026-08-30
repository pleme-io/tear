//! freio — the operator's brake.
//!
//! Portuguese for exactly what it is. One gesture stops every pane an
//! automation is driving, without killing anything, and leaves every pane
//! the operator is typing in untouched.
//!
//! ## What it is NOT, and why
//!
//! **Not `SIGSTOP`.** Wrong target: the agent is not in the pane, it is on
//! the other end of a socket calling `send_keys`. Stopping the shell does
//! not stop the writes — they land in the kernel PTY buffer and execute
//! the instant you continue it. You would brake the symptom and buffer the
//! cause. It is also not cleanly undoable (a process stopped mid-DECSET
//! leaves terminal modes a continue does not repair) and a daemon crash
//! between stop and continue leaves stopped orphans. A panic button whose
//! failure mode is "your shell is frozen forever" is not a panic button.
//!
//! **Not a bare `frozen: bool`.** That creates a SECOND authority over
//! "may this pane accept input", beside `input_policy`. Two authorities
//! over one question is exactly how you get a pane that reports `Free`
//! while refusing input.
//!
//! **What it is:** a session-scoped typed hold, consulted *before* the
//! policy lattice by one total function. Consulting it first is what makes
//! it non-advisory — a pane explicitly pinned to `Free` still cannot
//! escape a brake, because the brake is answered before the pin is ever
//! read.
//!
//! ## Why session-scoped rather than daemon-global
//!
//! One home for the state, and a killed session takes its brake with it.
//! The one-gesture ergonomics live in the VERB — `tear freio` fans out
//! over every session — rather than in a daemon-global flag that could
//! drift out of sync with the per-session records it is supposed to
//! describe.

use serde::{Deserialize, Serialize};

/// Whether an operator has braked a session's automation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Freio {
    /// Not braked. What every pre-freio session record deserialises to, so
    /// landing this type is a no-op on existing state.
    #[default]
    Released,
    /// Every pane in this session whose [`crate::yurai::Yurai`] is
    /// `Automation` refuses input.
    ///
    /// Human and Unknown panes are untouched — **the operator keeps
    /// typing, always.** That is not a convenience; a brake that can lock
    /// you out of your own terminal during the emergency you engaged it
    /// for is worse than no brake.
    ///
    /// `at_unix` is stamped BY THE DAEMON. No wire request carries it, so
    /// a backdated brake has no syntax.
    Engaged { at_unix: u64 },
}

impl Freio {
    #[must_use]
    pub const fn is_engaged(self) -> bool {
        matches!(self, Self::Engaged { .. })
    }

    /// When the brake was engaged, if it is.
    #[must_use]
    pub const fn engaged_at(self) -> Option<u64> {
        match self {
            Self::Engaged { at_unix } => Some(at_unix),
            Self::Released => None,
        }
    }
}

/// What input a pane ACTUALLY accepts right now.
///
/// The single answer to a question that currently has two authorities: the
/// `Locked` check inside `tear-core`'s `send_keys` and the `Leader` check
/// in the daemon's serve loop. freio must not make that three, so it joins
/// them rather than adding to them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Anyone may write.
    Accept,
    Refuse(RefusalReason),
    /// Only the connection whose client id matches.
    ///
    /// The DAEMON resolves this; `tear-core` structurally cannot, because
    /// there is no client identity at the in-process trait surface. That
    /// split is today's reality made explicit rather than commented.
    OnlyLeader {
        id: u64,
    },
}

/// Why input was refused. Distinct variants because the operator-facing
/// message differs: a policy refusal is a state they set, a freio refusal
/// is a brake they can release.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    Policy,
    Freio,
}

impl RefusalReason {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Policy => "policy",
            Self::Freio => "freio",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_is_the_default_so_landing_freio_changes_nothing() {
        assert_eq!(Freio::default(), Freio::Released);
        assert!(!Freio::default().is_engaged());
        assert_eq!(Freio::default().engaged_at(), None);
    }

    #[test]
    fn an_engaged_brake_remembers_when() {
        let f = Freio::Engaged {
            at_unix: 1_785_000_000,
        };
        assert!(f.is_engaged());
        assert_eq!(f.engaged_at(), Some(1_785_000_000));
    }

    /// The two refusals must stay distinguishable: one is a state the
    /// operator set, the other is a brake they can release, and telling a
    /// user the wrong one sends them to the wrong fix.
    #[test]
    fn a_freio_refusal_is_not_a_policy_refusal() {
        assert_ne!(
            Admission::Refuse(RefusalReason::Freio),
            Admission::Refuse(RefusalReason::Policy)
        );
        assert_eq!(RefusalReason::Freio.label(), "freio");
    }

    /// ★ No wire syntax for a backdated brake.
    ///
    /// `at_unix` exists on the type because a client must be able to SEE
    /// when a brake was engaged. It must never be settable by a peer — the
    /// same discipline that made `SessionSource` derived rather than
    /// declared. The request carries a bool; the daemon stamps the time.
    #[test]
    fn the_engaged_variant_is_daemon_minted_only() {
        // A peer's request shape is `SetFreio { engaged: bool }`. This row
        // pins the reason: if a request ever carried a `Freio` directly,
        // this deserialisation would be a peer-supplied timestamp.
        let json = r#"{"kind":"engaged","at_unix":1}"#;
        let f: Freio = serde_json::from_str(json).expect("Freio is decodable");
        assert_eq!(f.engaged_at(), Some(1));
        // …which is exactly why no Request variant may carry one. Guarded
        // by a source scan over wire.rs, not by this type.
    }
}
