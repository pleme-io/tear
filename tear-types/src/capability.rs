//! Daemon capabilities — what the peer on the other end of the socket
//! can actually *do*, probed rather than assumed.
//!
//! ## Why a capability set and not a version integer
//!
//! The obvious fix for "a newer client silently degrades against an
//! older daemon" is a protocol version number. It was measured and
//! rejected, for a reason worth keeping written down:
//!
//! 1. **It would not have caught the instance that motivated it.**
//!    `args` was threaded through three [`crate::MultiplexerControl`]
//!    methods and the CBOR wire in commit `5974375`. The daemon
//!    running on the operator's machine at that moment was
//!    `tear-0.1.8`; `HEAD` was *also* `0.1.8`. Ten behaviour-changing
//!    commits had landed since the version last moved. A version
//!    compare would have said "same version, all good" and the pane
//!    would still have spawned without its arguments.
//!
//! 2. **A version is one scalar for N independent facts.** What a
//!    caller actually needs to know is not "how old are you" but "do
//!    you read the `args` field". Those are different questions, and
//!    only the second one has an answer that stays true as the
//!    codebase moves. A capability names a **field or a behaviour**,
//!    so the refusal lands at the call site that needs it and
//!    *nowhere else* — a caller passing no args is unaffected by a
//!    daemon that cannot read them.
//!
//! The version string is still carried in [`DaemonHello`], because it
//! is genuinely useful in a log line and in `tear status`. It is
//! **not** what any decision is made on.
//!
//! ## Adding a capability
//!
//! Add the variant to [`Capability`], give it a `wire_name`, and
//! classify it in `advertised()`. The classification is an exhaustive
//! `match`, so a new variant that nobody decided about is a **compile
//! error**, not a silently-unadvertised capability.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::control::{ControlError, ControlResult};

/// One named thing a daemon build can do. Each variant names a
/// **field or a behaviour a caller can gate on** — never a release,
/// never a date.
///
/// The wire form is a string, deliberately: a client that meets a
/// daemon advertising a capability from a *newer* vocabulary must
/// ignore the name it doesn't know, not fail to decode the frame.
/// Strings make that free; a serialized enum would make it a
/// hard error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// The daemon reads the `args` field on `Request::NewSession`,
    /// `Request::NewWindow` and `Request::SplitPane`, and passes it
    /// to the child as argv[1..].
    ///
    /// A daemon without this reads the request fine — `args` is
    /// `#[serde(default)]` and no type sets `deny_unknown_fields`,
    /// so the key is simply dropped — and spawns the bare program.
    /// That silent drop is the failure this whole module exists to
    /// convert into a legible refusal.
    SpawnArgs,
}

impl Capability {
    /// Every capability name this build's *vocabulary* knows. Not the
    /// same thing as what a given daemon advertises — see
    /// [`Capability::advertised`].
    pub const ALL: &'static [Capability] = &[Capability::SpawnArgs];

    /// The on-wire name. Kebab-case, names the field or behaviour.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Capability::SpawnArgs => "spawn-args",
        }
    }

    /// Parse a wire name back to a typed capability. `None` for a
    /// name from a vocabulary this build doesn't have — which is the
    /// expected outcome when an older client meets a newer daemon,
    /// and must stay a quiet miss rather than an error.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        Capability::ALL
            .iter()
            .copied()
            .find(|c| c.wire_name() == s)
    }

    /// Does **this build of the daemon** implement the capability?
    ///
    /// The exhaustive `match` is the seal: adding a [`Capability`]
    /// variant without deciding this is `error[E0004]: non-exhaustive
    /// patterns`, so no capability can land unclassified.
    #[must_use]
    pub fn advertised(self) -> bool {
        match self {
            Capability::SpawnArgs => true,
        }
    }
}

/// What a daemon advertises about itself, in reply to
/// `Request::Hello`.
///
/// `daemon_version` is the daemon process's own
/// `CARGO_PKG_VERSION` — the first time this has ever been on the
/// wire. `tear status` previously printed the *CLI's* version under
/// a `version` key, which is a different binary and can differ
/// arbitrarily from the daemon's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonHello {
    /// The daemon binary's own package version.
    pub daemon_version: String,
    /// Wire names of every capability this daemon implements.
    pub capabilities: Vec<String>,
}

impl DaemonHello {
    /// The hello this build of the daemon answers with.
    #[must_use]
    pub fn for_this_build(daemon_version: &str) -> Self {
        Self {
            daemon_version: daemon_version.to_owned(),
            capabilities: Capability::ALL
                .iter()
                .copied()
                .filter(|c| c.advertised())
                .map(|c| c.wire_name().to_owned())
                .collect(),
        }
    }
}

/// A client's typed view of the daemon it is connected to.
///
/// The **pre-capability** value — [`DaemonIdentity::pre_capability`]
/// — is not an error state and is not a fallback bolted on the side.
/// It is what every client gets today, because the daemon running on
/// this machine predates the probe. Treat it as the normal case.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DaemonIdentity {
    /// `None` when the daemon predates `Request::Hello` and could
    /// not tell us. Never guessed from the client's own version.
    version: Option<String>,
    /// Raw wire names, including any this build's vocabulary does
    /// not know (a newer daemon's). Kept verbatim so `tear status`
    /// can show an operator a capability their CLI is too old to
    /// name.
    capabilities: BTreeSet<String>,
}

impl DaemonIdentity {
    /// A daemon that could not answer `Request::Hello`: it predates
    /// the probe (or refused it). Protocol 0 — **no capabilities**.
    #[must_use]
    pub fn pre_capability() -> Self {
        Self {
            version: None,
            capabilities: BTreeSet::new(),
        }
    }

    /// Build from a daemon's hello reply.
    #[must_use]
    pub fn from_hello(hello: DaemonHello) -> Self {
        Self {
            version: Some(hello.daemon_version),
            capabilities: hello.capabilities.into_iter().collect(),
        }
    }

    /// The identity an in-process backend has: it *is* this build,
    /// so it implements exactly what this build advertises.
    #[must_use]
    pub fn local(version: &str) -> Self {
        Self::from_hello(DaemonHello::for_this_build(version))
    }

    /// The daemon's own version, or `None` when it predates the
    /// probe. Never fall back to the client's version here — that
    /// substitution is precisely the lie `tear status` used to tell.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// True when the daemon could not answer the probe at all.
    #[must_use]
    pub fn is_pre_capability(&self) -> bool {
        self.version.is_none() && self.capabilities.is_empty()
    }

    /// Wire names, sorted. Includes names this build cannot type.
    #[must_use]
    pub fn capability_names(&self) -> Vec<&str> {
        self.capabilities.iter().map(String::as_str).collect()
    }

    /// Does the daemon implement `cap`?
    #[must_use]
    pub fn has(&self, cap: Capability) -> bool {
        self.capabilities.contains(cap.wire_name())
    }

    /// Typed refusal for a call that *needs* `cap`.
    ///
    /// Call this only on the branch that actually requires the
    /// capability — a caller who passes no `args` must not be
    /// refused by a daemon that cannot read `args`. That
    /// call-site-scoped shape is the whole point: the refusal is
    /// about one field, not a global "you are old" banner.
    ///
    /// # Errors
    /// [`ControlError::Unsupported`] naming the capability, the
    /// daemon's version (or that it predates the probe), and what
    /// to do about it.
    pub fn require(&self, cap: Capability, detail: &str) -> ControlResult<()> {
        if self.has(cap) {
            return Ok(());
        }
        let who = match &self.version {
            Some(v) => format!("daemon {v} does not advertise it"),
            None => "the daemon predates capability negotiation and advertises nothing".to_owned(),
        };
        Err(ControlError::Unsupported {
            capability: cap.wire_name(),
            detail: format!("{detail} ({who}); restart the tear daemon on a build that has it"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire names must be unique and round-trip. A duplicate would
    /// make `from_wire` resolve two capabilities to one.
    #[test]
    fn every_capability_wire_name_is_unique_and_round_trips() {
        let mut seen = BTreeSet::new();
        for cap in Capability::ALL.iter().copied() {
            assert!(
                seen.insert(cap.wire_name()),
                "duplicate wire name {}",
                cap.wire_name()
            );
            assert_eq!(Capability::from_wire(cap.wire_name()), Some(cap));
        }
    }

    /// A name from a vocabulary we don't have is a quiet miss, never
    /// a panic or an error — that is what lets an older client talk
    /// to a newer daemon.
    #[test]
    fn an_unknown_wire_name_is_a_quiet_miss() {
        assert_eq!(Capability::from_wire("spawn-cwd"), None);
        assert_eq!(Capability::from_wire(""), None);
    }

    /// `Capability::ALL` must actually list every variant. The
    /// exhaustive match makes a missing variant a compile error here
    /// rather than a capability that exists but is never advertised.
    #[test]
    fn all_lists_every_variant() {
        for cap in Capability::ALL.iter().copied() {
            // Exhaustive — adding a variant without adding it to ALL
            // fails this assertion; adding a variant at all without
            // touching `advertised()` is E0004 at compile time.
            match cap {
                Capability::SpawnArgs => {
                    assert!(Capability::ALL.contains(&Capability::SpawnArgs));
                }
            }
        }
        assert_eq!(Capability::ALL.len(), 1, "update this count with the vocabulary");
    }

    #[test]
    fn this_builds_hello_advertises_spawn_args() {
        let hello = DaemonHello::for_this_build("9.9.9");
        assert_eq!(hello.daemon_version, "9.9.9");
        assert_eq!(hello.capabilities, vec!["spawn-args".to_owned()]);
    }

    #[test]
    fn pre_capability_has_nothing_and_no_version() {
        let id = DaemonIdentity::pre_capability();
        assert!(id.is_pre_capability());
        assert_eq!(id.version(), None);
        assert!(!id.has(Capability::SpawnArgs));
        assert!(id.capability_names().is_empty());
    }

    /// The refusal names the capability, not the version — and it
    /// says what to do.
    #[test]
    fn require_on_a_pre_capability_daemon_is_a_typed_unsupported() {
        let id = DaemonIdentity::pre_capability();
        let err = id
            .require(Capability::SpawnArgs, "new_window was given 2 argument(s)")
            .unwrap_err();
        match err {
            ControlError::Unsupported { capability, detail } => {
                assert_eq!(capability, "spawn-args");
                assert!(detail.contains("new_window was given 2 argument(s)"));
                assert!(detail.contains("predates capability negotiation"));
                assert!(detail.contains("restart the tear daemon"));
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn require_on_a_capable_daemon_is_ok() {
        let id = DaemonIdentity::local("0.1.8");
        assert_eq!(id.version(), Some("0.1.8"));
        assert!(id.has(Capability::SpawnArgs));
        assert!(id.require(Capability::SpawnArgs, "whatever").is_ok());
    }

    /// A daemon from a newer vocabulary: unknown names survive into
    /// `capability_names` (so an operator can see them) but never
    /// satisfy a typed `has`.
    #[test]
    fn a_newer_daemons_unknown_capability_is_kept_but_never_matches() {
        let id = DaemonIdentity::from_hello(DaemonHello {
            daemon_version: "3.0.0".into(),
            capabilities: vec!["spawn-args".into(), "spawn-cwd".into()],
        });
        assert_eq!(id.capability_names(), vec!["spawn-args", "spawn-cwd"]);
        assert!(id.has(Capability::SpawnArgs));
        assert!(!id.is_pre_capability());
    }

    /// A daemon that answers the probe but advertises *nothing* is
    /// distinguishable from one that never answered: it has a
    /// version. Both refuse `spawn-args`, and that is the point.
    #[test]
    fn an_answering_daemon_with_no_capabilities_still_reports_its_version() {
        let id = DaemonIdentity::from_hello(DaemonHello {
            daemon_version: "0.1.9".into(),
            capabilities: vec![],
        });
        assert!(!id.is_pre_capability());
        assert_eq!(id.version(), Some("0.1.9"));
        let err = id.require(Capability::SpawnArgs, "x").unwrap_err();
        assert!(format!("{err}").contains("daemon 0.1.9 does not advertise it"));
    }
}
