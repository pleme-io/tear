//! Genesis + Guid — the *immutable* half of a session's two keys.
//!
//! A [`Guid`] is a BLAKE3 commitment to a session's **genesis**: the
//! program that runs it, the intent it was spawned with, and the
//! context it was born into. It is the answer to "which session is
//! this, forever?", and it never changes, because none of its inputs
//! can change after birth.
//!
//! The companion key is [`crate::Address`] — mutable, unique, dotted,
//! and used for lookup and aggregation. Durable records key on the
//! `Guid`; nothing keys on the `Address`. That is the whole point:
//! renaming or re-parenting a session moves the alias and leaves the
//! identity — and therefore the data — exactly where it was.
//!
//! ## What is NOT hashed
//!
//! The session's **current** address. There is no field for it here,
//! so a rename cannot reach the hash even by accident — the omission
//! is structural, not a convention someone has to remember.
//!
//! What *is* hashed is [`Genesis::requested_address`]: the address
//! asked for at birth. That is a historical fact about the spawn, as
//! immutable as the cwd it ran in, and including it makes two
//! otherwise-identical spawns into two distinct sessions.
//!
//! ## Why a Merkle root and not one flat hash
//!
//! Modelled on tameshi's `CertificationArtifact`: the three inputs are
//! separate **leaves** composed into a root, with RFC 9162 / Certificate
//! Transparency domain separation —
//!
//! ```text
//! leaf(x)      = BLAKE3(0x00 || framed-fields)
//! node(l, r)   = BLAKE3(0x01 || l || r)
//!
//!                      root = node(node(L0, L1), L2)
//!                          /                    \
//!            node(L0, L1)                        L2  context
//!             /        \                             (cwd, parent)
//!   L0 program          L1 intent
//!                          (requested address, args)
//! ```
//!
//! The distinct `0x00` / `0x01` prefixes mean a leaf hash and an
//! interior hash can never collide, so no attacker-supplied leaf
//! content can be re-read as an interior node (the second-preimage
//! attack RFC 9162 §2.1.1 exists to stop). Within a leaf every field
//! is length-framed (`u64` little-endian length, then bytes), so
//! `["a", "b"]` and `["ab"]` are different commitments rather than the
//! same concatenation.
//!
//! Leaf structure also leaves room to hand out an inclusion proof for
//! one leaf later without revealing the others; nothing here needs
//! that yet, and nothing here forecloses it.
//!
//! ## Full 256 bits, deliberately
//!
//! [`Guid`] keeps the whole BLAKE3 root — 32 bytes, 64 hex characters.
//! It does **not** truncate the way [`crate::SessionId::from_seed`]
//! does (first 8 little-endian bytes, 64 bits). A 64-bit identifier has
//! a ~50% collision probability around 5 billion values and is birthday-
//! attackable by anyone who can influence a seed; an attestable identity
//! cannot be built on that. `SessionId` stays as it is — this is a new,
//! wider key alongside it, not a change to the old one.
//!
//! ## No way to invent one
//!
//! [`Guid`]'s byte array is private to this module, so no other module
//! — inside this crate or outside it — can name it. The only public
//! function returning a `Guid` is [`Genesis::guid`]. There is
//! deliberately no `FromStr`, no `From<[u8; 32]>`, and no
//! `Deserialize`: a peer cannot hand the daemon an identity, because
//! there is no code path that turns bytes back into one.
//!
//! `Serialize` is implemented (as lowercase hex) for the same reason
//! [`crate::Shutai`] implements it — identity is *reported* outward to
//! audit logs, `tear list`, and MCP reads. Information flows out;
//! authority does not flow in.

use core::fmt;

use serde::{Serialize, Serializer};

use crate::address::Address;

/// RFC 9162 leaf-domain prefix.
const LEAF_DOMAIN: u8 = 0x00;

/// RFC 9162 interior-node-domain prefix.
const NODE_DOMAIN: u8 = 0x01;

/// Leaf label: the program that runs the session.
const LABEL_PROGRAM: &str = "tear.genesis.program.v1";

/// Leaf label: what the spawn asked for.
const LABEL_INTENT: &str = "tear.genesis.intent.v1";

/// Leaf label: where and under whom it was born.
const LABEL_CONTEXT: &str = "tear.genesis.context.v1";

/// A session's immutable identity — the full 256-bit BLAKE3 root of
/// its [`Genesis`].
///
/// Obtainable only from [`Genesis::guid`]. See the module docs for why
/// there is no other constructor and no `Deserialize`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Guid([u8; 32]);

impl Guid {
    /// The full 32-byte root.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, 64 characters — the canonical text form.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for b in self.0 {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    /// The first 16 hex characters, for logs and `tear list` columns.
    /// A display abbreviation only — the identity is always the full
    /// root, and nothing accepts a short form as input.
    #[must_use]
    pub fn short(&self) -> String {
        self.to_hex()[..16].to_string()
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Guid({})", self.to_hex())
    }
}

impl Serialize for Guid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

/// Everything a session is committed to at birth.
///
/// Three groups, one per Merkle leaf:
///
/// | leaf | fields | what it pins |
/// |---|---|---|
/// | 0 | [`program`](Genesis::program) | the image that runs |
/// | 1 | [`requested_address`](Genesis::requested_address), [`args`](Genesis::args) | what the spawn asked for |
/// | 2 | [`cwd`](Genesis::cwd), [`parent`](Genesis::parent) | where, and under whom |
///
/// There is no field for the session's *current* address — see the
/// module docs.
///
/// Like [`Guid`] and [`crate::Shutai`], `Genesis` is `Serialize` but
/// not `Deserialize`: it is minted from a spawn the daemon is
/// performing, never parsed from a payload a peer sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Genesis {
    /// The shell or program image that runs the session — the resolved
    /// path where one is known (`/run/current-system/sw/bin/zsh`), the
    /// bare name otherwise.
    pub program: String,
    /// The address the spawn asked for. A historical fact about the
    /// birth, not the session's live alias; the live alias is not an
    /// input to the hash at all.
    pub requested_address: Address,
    /// Arguments the spawn requested, in order, excluding `argv[0]`.
    pub args: Vec<String>,
    /// The working directory the session was born in.
    pub cwd: String,
    /// The spawning session, when there was one. `None` is a
    /// top-level spawn. Because a [`Guid`] can only come from a
    /// [`Genesis`], a parent link is unforgeable by construction: you
    /// cannot claim a parent you never derived.
    pub parent: Option<Guid>,
}

impl Genesis {
    /// A top-level genesis: program, requested address, cwd. Add args
    /// with [`Genesis::with_args`] and a parent with
    /// [`Genesis::with_parent`].
    #[must_use]
    pub fn new(
        program: impl Into<String>,
        requested_address: Address,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            program: program.into(),
            requested_address,
            args: Vec::new(),
            cwd: cwd.into(),
            parent: None,
        }
    }

    /// Set the spawn arguments.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Record the spawning session.
    #[must_use]
    pub fn with_parent(mut self, parent: Guid) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Derive this genesis's [`Guid`] — the only way a `Guid` comes
    /// into existence.
    ///
    /// Pure and total: the same `Genesis` always yields the same
    /// `Guid`, on any host, in any process, forever.
    #[must_use]
    pub fn guid(&self) -> Guid {
        let [program, intent, context] = self.leaves();
        Guid(node(&node(&program, &intent), &context))
    }

    /// The three domain-separated leaf hashes, in tree order.
    fn leaves(&self) -> [[u8; 32]; 3] {
        let program = leaf(LABEL_PROGRAM, |h| {
            frame(h, self.program.as_bytes());
        });

        let intent = leaf(LABEL_INTENT, |h| {
            frame(h, self.requested_address.to_string().as_bytes());
            frame_len(h, self.args.len());
            for arg in &self.args {
                frame(h, arg.as_bytes());
            }
        });

        let context = leaf(LABEL_CONTEXT, |h| {
            frame(h, self.cwd.as_bytes());
            match &self.parent {
                None => {
                    h.update(&[0u8]);
                }
                Some(parent) => {
                    h.update(&[1u8]);
                    h.update(parent.as_bytes());
                }
            }
        });

        [program, intent, context]
    }
}

/// Length-framed field: `u64` little-endian length, then the bytes.
/// Framing is what makes `["a", "b"]` and `["ab"]` different
/// commitments.
fn frame(h: &mut blake3::Hasher, bytes: &[u8]) {
    frame_len(h, bytes.len());
    h.update(bytes);
}

fn frame_len(h: &mut blake3::Hasher, len: usize) {
    h.update(&(len as u64).to_le_bytes());
}

/// `BLAKE3(0x00 || framed(label) || body)` — RFC 9162 leaf domain.
fn leaf(label: &str, body: impl FnOnce(&mut blake3::Hasher)) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[LEAF_DOMAIN]);
    frame(&mut h, label.as_bytes());
    body(&mut h);
    *h.finalize().as_bytes()
}

/// `BLAKE3(0x01 || left || right)` — RFC 9162 interior-node domain.
fn node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[NODE_DOMAIN]);
    h.update(left);
    h.update(right);
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::Address;

    fn addr(s: &str) -> Address {
        Address::parse(s).expect("test fixture must be a legal address")
    }

    fn sample() -> Genesis {
        // Compile-checked literal: if `Genesis` gains, loses, or
        // renames a field this test stops COMPILING. That is the
        // forcing function behind "the current address is not an input
        // to the hash" — the only way to break the invariant is to add
        // a field here, and doing so cannot happen silently.
        Genesis {
            program: "/bin/zsh".into(),
            requested_address: addr("work.acme.helm-charts.build"),
            args: vec!["-l".into()],
            cwd: "/code/acme/helm-charts".into(),
            parent: None,
        }
    }

    // ── reproducibility ────────────────────────────────────────────

    #[test]
    fn the_same_genesis_always_derives_the_same_guid() {
        assert_eq!(sample().guid(), sample().guid());
        // and repeated derivation from one value is stable too
        let g = sample();
        assert_eq!(g.guid(), g.guid());
    }

    #[test]
    fn the_guid_is_a_full_256_bit_root_not_a_truncation() {
        let guid = sample().guid();
        assert_eq!(guid.as_bytes().len(), 32);
        assert_eq!(guid.to_hex().len(), 64);
        // The bug being avoided: SessionId::from_seed keeps only the
        // first 8 bytes. Prove the other 24 carry real entropy.
        assert!(
            guid.as_bytes()[8..].iter().any(|b| *b != 0),
            "bytes past the first 8 must not be dropped"
        );
        assert_eq!(guid.short(), &guid.to_hex()[..16]);
    }

    // ── one leaf differs → a different Guid ────────────────────────

    #[test]
    fn a_different_program_derives_a_different_guid() {
        let mut other = sample();
        other.program = "/bin/bash".into();
        assert_ne!(sample().guid(), other.guid());
    }

    #[test]
    fn a_different_requested_address_derives_a_different_guid() {
        let mut other = sample();
        other.requested_address = addr("work.acme.helm-charts.test");
        assert_ne!(sample().guid(), other.guid());
    }

    #[test]
    fn different_args_derive_a_different_guid() {
        let mut other = sample();
        other.args = vec!["-i".into()];
        assert_ne!(sample().guid(), other.guid());

        let mut none = sample();
        none.args = Vec::new();
        assert_ne!(sample().guid(), none.guid());
        assert_ne!(other.guid(), none.guid());
    }

    #[test]
    fn a_different_cwd_derives_a_different_guid() {
        let mut other = sample();
        other.cwd = "/code/acme/cli".into();
        assert_ne!(sample().guid(), other.guid());
    }

    #[test]
    fn a_different_parent_derives_a_different_guid() {
        let parent_a = sample().guid();
        let parent_b = Genesis::new("/bin/bash", addr("work.other"), "/tmp").guid();
        assert_ne!(parent_a, parent_b);

        let orphan = sample();
        let under_a = sample().with_parent(parent_a);
        let under_b = sample().with_parent(parent_b);

        assert_ne!(orphan.guid(), under_a.guid());
        assert_ne!(orphan.guid(), under_b.guid());
        assert_ne!(under_a.guid(), under_b.guid());
    }

    #[test]
    fn every_leaf_is_load_bearing() {
        // One assertion covering the whole claim: perturb each leaf's
        // fields one at a time; all five perturbations, plus the
        // baseline, must be six distinct Guids.
        let base = sample();
        let mut variants = vec![base.guid()];

        let mut v = base.clone();
        v.program = "/bin/bash".into();
        variants.push(v.guid());

        let mut v = base.clone();
        v.requested_address = addr("work.other");
        variants.push(v.guid());

        let mut v = base.clone();
        v.args = vec!["-l".into(), "-i".into()];
        variants.push(v.guid());

        let mut v = base.clone();
        v.cwd = "/elsewhere".into();
        variants.push(v.guid());

        let v = base.clone().with_parent(base.guid());
        variants.push(v.guid());

        let mut seen = std::collections::BTreeSet::new();
        for g in &variants {
            assert!(seen.insert(g.to_hex()), "leaf perturbation collided: {g:?}");
        }
        assert_eq!(seen.len(), 6);
    }

    // ── framing + domain separation ────────────────────────────────

    #[test]
    fn args_are_length_framed_not_concatenated() {
        let split = sample().with_args(["a", "b"]);
        let joined = sample().with_args(["ab"]);
        let dotted = sample().with_args(["a.b"]);
        assert_ne!(split.guid(), joined.guid());
        assert_ne!(split.guid(), dotted.guid());
        assert_ne!(joined.guid(), dotted.guid());

        // A trailing empty arg is a real difference, not a no-op.
        let trailing_empty = sample().with_args(["a", ""]);
        let bare = sample().with_args(["a"]);
        assert_ne!(trailing_empty.guid(), bare.guid());
    }

    #[test]
    fn leaf_and_interior_domains_cannot_collide() {
        // The RFC 9162 guarantee, tested directly: the same body under
        // the leaf prefix and under the node prefix are different
        // hashes, so no leaf can be re-read as an interior node.
        let l = leaf("x", |h| frame(h, b"left"));
        let r = leaf("x", |h| frame(h, b"right"));
        let interior = node(&l, &r);

        // A leaf whose body is byte-identical to the interior body.
        let leafish = {
            let mut h = blake3::Hasher::new();
            h.update(&[LEAF_DOMAIN]);
            h.update(&l);
            h.update(&r);
            *h.finalize().as_bytes()
        };
        assert_ne!(interior, leafish, "domain prefixes must separate");
        assert_ne!(LEAF_DOMAIN, NODE_DOMAIN);

        // And a domain-separated leaf is not the raw BLAKE3 of its body.
        let raw = *blake3::hash(b"left").as_bytes();
        assert_ne!(l, raw);
    }

    #[test]
    fn the_root_is_the_documented_tree_shape() {
        let g = sample();
        let [l0, l1, l2] = g.leaves();
        let expected = node(&node(&l0, &l1), &l2);
        assert_eq!(g.guid().as_bytes(), &expected);
        // Leaves are mutually distinct — the labels do their job.
        assert_ne!(l0, l1);
        assert_ne!(l1, l2);
        assert_ne!(l0, l2);
    }

    // ── identity vs alias ──────────────────────────────────────────

    #[test]
    fn a_parent_link_can_only_be_a_derived_guid() {
        // The chain is closed: the only value `with_parent` accepts is
        // one that came out of some Genesis, so an invented ancestry is
        // unrepresentable rather than merely discouraged.
        let parent = Genesis::new("/bin/zsh", addr("work"), "/code");
        let child =
            Genesis::new("/bin/zsh", addr("work.build"), "/code").with_parent(parent.guid());
        assert_eq!(child.parent, Some(parent.guid()));
        assert_ne!(child.guid(), parent.guid());
    }

    #[test]
    fn renaming_is_structurally_outside_the_hash() {
        // The session's live alias is a separate value that Genesis
        // never sees. Moving it — even to something wildly different —
        // touches nothing the Guid was derived from.
        let genesis = sample();
        let identity = genesis.guid();

        let mut live_alias = genesis.requested_address.clone();
        assert_eq!(live_alias.to_string(), "work.acme.helm-charts.build");
        live_alias = addr("archive.2026.helm-charts.build");

        assert_eq!(genesis.guid(), identity);
        assert_ne!(live_alias, genesis.requested_address);
    }

    // ── outward reporting ──────────────────────────────────────────

    #[test]
    fn guid_serializes_outward_as_lowercase_hex() {
        let guid = sample().guid();
        let json = serde_json::to_string(&guid).unwrap();
        assert_eq!(json, format!("\"{}\"", guid.to_hex()));
        assert_eq!(json.len(), 66);
        assert!(guid.to_hex().chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!guid.to_hex().chars().any(|c| c.is_ascii_uppercase()));
    }

    #[test]
    fn genesis_serializes_with_its_address_as_a_plain_string() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(
            json["requested_address"],
            serde_json::json!("work.acme.helm-charts.build")
        );
        assert_eq!(json["parent"], serde_json::Value::Null);
    }

    #[test]
    fn guid_display_and_debug_agree_on_the_hex() {
        let guid = sample().guid();
        assert_eq!(guid.to_string(), guid.to_hex());
        assert_eq!(format!("{guid:?}"), format!("Guid({})", guid.to_hex()));
    }

    // ── structural forcing functions ───────────────────────────────

    #[test]
    fn genesis_commits_to_exactly_program_intent_and_context() {
        // An exhaustive destructure. Adding a field to `Genesis` breaks
        // this at COMPILE time, which is what keeps "the live address is
        // not an input" from decaying into a convention someone forgets.
        let Genesis {
            program,
            requested_address,
            args,
            cwd,
            parent,
        } = sample();
        assert_eq!(program, "/bin/zsh");
        assert_eq!(requested_address.to_string(), "work.acme.helm-charts.build");
        assert_eq!(args, vec!["-l".to_string()]);
        assert_eq!(cwd, "/code/acme/helm-charts");
        assert!(parent.is_none());
    }

    /// ★ THE STRUCTURAL PROPERTY, as a forcing function.
    ///
    /// A `Guid` must never become inventable. Absence of a trait impl
    /// cannot be asserted at runtime, so this is a comment-stripped
    /// source scan — the same construction as `shutai.rs`'s
    /// `shutai_never_becomes_deserializable`.
    #[test]
    fn guid_never_gains_a_constructor_other_than_genesis() {
        let src = include_str!("genesis.rs");
        let code: String = src
            .lines()
            .map(str::trim_start)
            .filter(|l| !l.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let code = code.split("mod tests").next().unwrap_or(&code);

        assert!(
            !code.contains("Deserialize"),
            "`Deserialize` appeared in genesis.rs. A peer could then SEND an \
             identity instead of deriving one, which is the whole thing this \
             type prevents."
        );
        assert!(
            !code.contains("FromStr"),
            "`FromStr` would let any string become a Guid"
        );
        assert!(
            !code.contains("impl From<"),
            "a `From` impl would be a second way to mint a Guid"
        );
        assert!(
            code.contains("pub struct Guid([u8; 32]);"),
            "Guid's bytes must stay private — a `pub` field is a constructor"
        );
        assert_eq!(
            code.matches("-> Guid").count(),
            1,
            "exactly one function may return a Guid, and it is Genesis::guid"
        );

        // Anti-vacuity: the scan must be looking at real code.
        assert!(code.contains("pub fn guid(&self) -> Guid"));
        assert!(
            code.contains("impl Serialize for Guid"),
            "a Guid must still report OUTWARD (audit, list, MCP reads)"
        );
    }
}
