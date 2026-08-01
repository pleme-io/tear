//! shutai (主体) — the acting entity behind a connection.
//!
//! Everything in tear's authorization surface is a projection of one
//! question: **what is driving this pane, and can I trust the answer?**
//!
//! Today the answer is `Option<u64>` set by `Request::IdentifyClient(u64)`
//! — a number the peer chooses, on a socket, with no verification. And
//! `SessionSource` is worse than unverified: it is *caller-declared*. A
//! client passes `SessionSource::Human` and the daemon records it, so the
//! one field an operator would use to triage what an agent started is set
//! by the thing being triaged.
//!
//! ## The split that makes this honest
//!
//! A shutai has two halves and **they are different tiers**. Flattening
//! them into one enum is the mistake this type exists to prevent, because
//! it would let a later reader believe the whole thing is verified.
//!
//! | half | source | can it be forged? | tier |
//! |---|---|---|---|
//! | [`Attested`] | the kernel, from the connection | **no** | truly-unrepresentable at the local boundary |
//! | [`Declared`] | the peer says so | yes, by any same-uid process | only-mitigated |
//!
//! The attested half costs a syscall — `getpeereid` / `SO_PEERCRED` — and
//! **no network call, no PKI, no broker**. It works offline, on a plane,
//! with Akeyless deleted from the flake. That is why identity roots here
//! rather than in a credential plane.
//!
//! The declared half is *provenance the daemon records*, not identity it
//! verifies. Same-uid processes are mutually trusting by construction:
//! anything that can open your socket can also read your files and send
//! you signals. Claiming otherwise would be the round-up this project is
//! most likely to commit.
//!
//! ## Why there is no `Deserialize`
//!
//! [`Shutai`] deliberately does **not** implement `Deserialize`. A peer
//! cannot send one, because there is no code path that turns wire bytes
//! into one — the daemon mints it from the connection it already holds.
//! That is the structural difference from `IdentifyClient(u64)`, where the
//! identity *is* the payload.
//!
//! `Serialize` is implemented: the daemon reports shutai outward (audit,
//! `tear list`, MCP reads). Information flows out, authority does not flow
//! in.

use serde::Serialize;

use crate::session::SessionSource;

/// What the kernel says about the peer. Not forgeable by a payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Attested {
    /// A local peer on the Unix socket, at this uid, per the kernel.
    ///
    /// The daemon's socket is `0600`, so in practice this is always the
    /// operator's own uid — the mode is what makes that true, and the
    /// peer credential is what makes it *knowable* for attribution.
    LocalUid { uid: u32 },
    /// A peer that arrived over TCP. There is no uid to attest: the
    /// listener refuses to bind a non-loopback address without a token,
    /// so this means either loopback (same trust boundary as the UDS) or
    /// a token-bearing peer.
    Remote,
}

/// What the peer says it is. Self-asserted; recorded, never trusted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Declared {
    /// Nothing was declared. The honest default — a connection that never
    /// said what it is stays unknown rather than being assumed human.
    Unknown,
    /// An interactive operator.
    Human,
    /// An AI agent — Claude Code, Cursor, the mado MCP surface.
    Agent { label: Option<String> },
    /// An in-process reconciler: a vigy tatara-lisp script. Distinguished
    /// from `Agent` because its actuator is different — it mutates
    /// privileged state directly rather than typing into a pane.
    Reconciler { label: Option<String> },
}

/// The acting entity behind one connection.
///
/// Minted by the daemon from a connection it holds; never parsed from a
/// payload. See the module docs for why the two halves are separate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Shutai {
    attested: Attested,
    declared: Declared,
}

impl Shutai {
    /// Mint from a local peer credential the daemon read from the socket.
    ///
    /// `uid` must come from the kernel (`getpeereid` / `SO_PEERCRED`) and
    /// never from a request field. This is the one place the local arm is
    /// created, so there is a single site to audit.
    #[must_use]
    pub const fn from_peer_uid(uid: u32) -> Self {
        Self {
            attested: Attested::LocalUid { uid },
            declared: Declared::Unknown,
        }
    }

    /// Mint for a peer with no attestable uid (TCP).
    #[must_use]
    pub const fn remote() -> Self {
        Self {
            attested: Attested::Remote,
            declared: Declared::Unknown,
        }
    }

    /// Record what the peer says it is.
    ///
    /// Takes `self` by value and returns a new value rather than mutating
    /// in place, so a declaration is applied at one point in a connection's
    /// setup rather than drifting later in its life.
    #[must_use]
    pub fn declaring(self, declared: Declared) -> Self {
        Self { declared, ..self }
    }

    #[must_use]
    pub const fn attested(&self) -> &Attested {
        &self.attested
    }

    #[must_use]
    pub const fn declared(&self) -> &Declared {
        &self.declared
    }

    /// Is this a non-human actuator? The predicate `freio` needs to know
    /// which panes to brake, and `ashiato` needs to attribute a block.
    ///
    /// Reads the DECLARED half, so it is exactly as trustworthy as the
    /// peer's own claim — which is the honest answer, because a same-uid
    /// process could lie and there is no local mechanism that would catch
    /// it.
    #[must_use]
    pub const fn is_automation(&self) -> bool {
        matches!(
            self.declared,
            Declared::Agent { .. } | Declared::Reconciler { .. }
        )
    }

    /// The session provenance this actor implies.
    ///
    /// **This is the point of the type.** `SessionSource` is currently a
    /// parameter the caller passes to `new_session_with_source`, so the
    /// field an operator uses to triage what an agent started is set by
    /// the thing being triaged. Deriving it from the connection closes
    /// that: a client can still *lie about what it is*, but it can no
    /// longer declare one thing and be recorded as another.
    ///
    /// The nix repo's `readOnly`-derived-option reflex, applied here: a
    /// value that is a function of typed inputs is derived once, never
    /// hand-passed by each consumer.
    #[must_use]
    pub fn session_source(&self) -> SessionSource {
        match &self.declared {
            Declared::Agent { label: Some(l) } | Declared::Reconciler { label: Some(l) } => {
                SessionSource::Named(l.clone())
            }
            Declared::Agent { label: None } | Declared::Reconciler { label: None } => {
                SessionSource::Agent
            }
            // An undeclared connection is recorded as Human, matching the
            // existing `#[serde(default)]` on SessionSource so pre-shutai
            // sessions keep their meaning.
            Declared::Unknown | Declared::Human => SessionSource::Human,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_local_shutai_declares_nothing() {
        let s = Shutai::from_peer_uid(501);
        assert_eq!(*s.attested(), Attested::LocalUid { uid: 501 });
        assert_eq!(*s.declared(), Declared::Unknown);
        assert!(
            !s.is_automation(),
            "an undeclared connection must not be assumed to be an agent"
        );
    }

    #[test]
    fn declaring_does_not_touch_the_attested_half() {
        let s = Shutai::from_peer_uid(501).declaring(Declared::Agent {
            label: Some("claude-code".into()),
        });
        assert_eq!(
            *s.attested(),
            Attested::LocalUid { uid: 501 },
            "a declaration must never be able to rewrite what the kernel said"
        );
        assert!(s.is_automation());
    }

    #[test]
    fn session_source_is_derived_rather_than_declared() {
        assert_eq!(
            Shutai::from_peer_uid(1).session_source(),
            SessionSource::Human,
            "undeclared stays Human so pre-shutai sessions keep their meaning"
        );
        assert_eq!(
            Shutai::from_peer_uid(1)
                .declaring(Declared::Agent { label: None })
                .session_source(),
            SessionSource::Agent
        );
        assert_eq!(
            Shutai::from_peer_uid(1)
                .declaring(Declared::Agent {
                    label: Some("pleme-ci".into())
                })
                .session_source(),
            SessionSource::Named("pleme-ci".into())
        );
    }

    /// A reconciler is an agent for triage purposes but a distinct
    /// actuator: it mutates privileged state directly rather than typing.
    #[test]
    fn a_reconciler_is_automation_but_keeps_its_own_arm() {
        let s = Shutai::from_peer_uid(1).declaring(Declared::Reconciler {
            label: Some("ghost-session-sweeper".into()),
        });
        assert!(s.is_automation());
        assert!(matches!(s.declared(), Declared::Reconciler { .. }));
    }

    /// ★ THE STRUCTURAL PROPERTY, as a forcing function.
    ///
    /// `Shutai` must never gain `Deserialize`. The whole difference from
    /// `IdentifyClient(u64)` is that identity is *derived from the
    /// connection* rather than *carried in the payload* — and a
    /// `Deserialize` impl would silently restore the payload path.
    ///
    /// This cannot be asserted by the type system (you cannot test for the
    /// absence of a trait impl at runtime), so it is a comment-stripped
    /// source scan, the same construction as mado's `ux_unification.rs`
    /// and garasu's `pane.rs` escape-hatch scan.
    #[test]
    fn shutai_never_becomes_deserializable() {
        let src = include_str!("shutai.rs");
        let code: String = src
            .lines()
            .map(str::trim_start)
            .filter(|l| !l.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let code = code.split("mod tests").next().unwrap_or(&code);

        assert!(
            !code.contains("Deserialize"),
            "`Deserialize` appeared in shutai.rs. A peer would then be able \
             to SEND a Shutai, which is exactly the payload-supplied identity \
             this type replaces. If this is deliberate, the module docs must \
             be re-graded in the same commit."
        );
        // Anti-vacuity: the scan must be looking at real code.
        assert!(
            code.contains("pub struct Shutai"),
            "the scan lost sight of Shutai — fix the scan, not the assert"
        );
        assert!(
            code.contains("Serialize"),
            "Shutai must still serialise OUTWARD (audit, list, MCP reads)"
        );
    }
}
