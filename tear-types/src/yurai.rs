//! yurai (由来) — what a PANE remembers about the actor that spawned it.
//!
//! ## Why this is not a [`Shutai`]
//!
//! A pane outlives the connection that made it. [`Shutai`] is
//! per-connection and deliberately has no `Deserialize`, while `TearPane`
//! derives one — so `TearPane { shutai: Shutai, .. }` does not compile.
//!
//! That `E0277` is not an obstacle to route around. It is the design
//! telling the truth: a pane must not hold a live identity, because the
//! entity that identity described may have exited an hour ago. Asking a
//! pane "who owns you?" has no honest answer; asking it "what kind of
//! thing made you?" does.
//!
//! So `Yurai` is what SURVIVES the socket closing — the classification,
//! not the actor. It is deliberately coarser than [`Declared`]: no uid, no
//! client id, no pid. `freio` does not need to know WHO; it needs to know
//! WHAT KIND, and carrying more would be carrying a fact that goes stale.
//!
//! ## Tier
//!
//! `Yurai` **does** implement `Deserialize` — it must, because it rides
//! inside `TearPane` inside a `Response` that clients decode. That
//! structurally reopens the payload-identity door [`Shutai`] closed, and
//! it is held shut one tier lower: no `Request` variant carries a
//! `TearPane`, so there is no path from a peer's bytes into the daemon's
//! pane records. **only-mitigated — ceiling: adding any request that
//! carries a `TearPane` silently restores the payload path.** Guarded by
//! a source scan, not by the type.

use serde::{Deserialize, Serialize};

use crate::shutai::{Declared, Shutai};

/// The provenance a pane carries for its whole life.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Yurai {
    /// No connection minted this pane, or the daemon that did predates
    /// the field.
    ///
    /// **NOT "human".** Unknown stays unknown, the same discipline
    /// [`Declared::Unknown`] follows and for a sharper reason: assuming
    /// human here would make `freio` silently skip a pane it was pressed
    /// to stop. An unbraked pane the operator believes is braked is worse
    /// than an unbraked pane they can see.
    #[default]
    Unknown,
    /// The spawning connection declared itself human, or declared nothing
    /// and resolved to human.
    Human,
    /// The spawning connection declared [`Declared::Agent`] or
    /// [`Declared::Reconciler`].
    ///
    /// The label is the peer's own string, carried verbatim for display
    /// and audit — never parsed, never matched on. It is a claim, at the
    /// same tier the claim was made.
    Automation { label: Option<String> },
}

impl Yurai {
    /// The ONE constructor that can produce [`Yurai::Automation`].
    ///
    /// Takes a [`Shutai`] the daemon minted from a connection it holds, so
    /// in-process code cannot fabricate a provenance without first holding
    /// an attested connection.
    #[must_use]
    pub fn from_shutai(s: &Shutai) -> Self {
        match s.declared() {
            Declared::Unknown => Self::Unknown,
            Declared::Human => Self::Human,
            Declared::Agent { label } | Declared::Reconciler { label } => Self::Automation {
                label: label.clone(),
            },
        }
    }

    /// The predicate `freio` filters on.
    ///
    /// Reads exactly as far as [`Shutai::is_automation`] did and no
    /// further — a claim, recorded at its tier.
    #[must_use]
    pub const fn is_automation(&self) -> bool {
        matches!(self, Self::Automation { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_undeclared_connection_stamps_unknown_not_human() {
        let s = Shutai::from_peer_uid(501);
        assert_eq!(
            Yurai::from_shutai(&s),
            Yurai::Unknown,
            "assuming human would make freio silently skip a pane it was \
             pressed to stop"
        );
        assert!(!Yurai::from_shutai(&s).is_automation());
    }

    #[test]
    fn an_agent_connection_stamps_automation_with_its_label() {
        let s = Shutai::from_peer_uid(501).declaring(Declared::Agent {
            label: Some("claude-code".into()),
        });
        assert_eq!(
            Yurai::from_shutai(&s),
            Yurai::Automation {
                label: Some("claude-code".into())
            }
        );
        assert!(Yurai::from_shutai(&s).is_automation());
    }

    /// A reconciler is a different ACTUATOR but the same KIND for the
    /// brake's purposes — it drives the terminal without a human.
    #[test]
    fn a_reconciler_is_automation_too() {
        let s = Shutai::from_peer_uid(501).declaring(Declared::Reconciler {
            label: Some("ghost-sweeper".into()),
        });
        assert!(
            Yurai::from_shutai(&s).is_automation(),
            "a reconciler drives the terminal without a human — freio must \
             reach it"
        );
    }

    #[test]
    fn a_human_connection_is_not_automation() {
        let s = Shutai::from_peer_uid(501).declaring(Declared::Human);
        assert_eq!(Yurai::from_shutai(&s), Yurai::Human);
        assert!(!Yurai::from_shutai(&s).is_automation());
    }

    /// A record from a pre-yurai daemon decodes as `Unknown`, which is
    /// what its absence honestly means.
    #[test]
    fn the_default_is_unknown_so_legacy_records_stay_honest() {
        assert_eq!(Yurai::default(), Yurai::Unknown);
    }
}
