//! Wire-shape mirror of `tear_core::blocks::Block`.
//!
//! Lives in `tear-types` because the daemon's wire layer needs
//! the type but can't depend on `tear-core` (no upward dep —
//! tear-core depends on tear-types). The two crate's `Block`
//! structs share the same serde representation byte-for-byte;
//! we use `From` conversions on the tear-core side to bridge.

use serde::{Deserialize, Serialize};

use crate::yurai::Yurai;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub index: u64,
    pub prompt: String,
    pub command: String,
    pub output: String,
    /// Bytes of output DROPPED because the block hit its byte cap.
    ///
    /// ── ★ THE CAP EXISTS BECAUSE THIS FIELD WAS UNBOUNDED ───────────────
    /// `blocks.rs`'s own header presents the ring buffer as the storage
    /// bound — "a configurable cap (default 10 000 blocks)… oldest blocks
    /// evict first". That bounds the COUNT. Nothing bounded one block's
    /// output, and nothing bounded the IN-FLIGHT block at all, so a single
    /// `journalctl -f` or a large build grew the tear daemon's heap in
    /// lockstep with its stdout and kept it afterwards.
    ///
    /// A COUNT and not a flag: a reader that has to decide whether to go to
    /// the pane's scrollback instead needs to know how much is missing, and
    /// a `usize` costs the same as a `bool`. Zero is the normal case.
    #[serde(default)]
    pub output_dropped_bytes: usize,
    pub exit_code: Option<i32>,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: Option<u64>,
    /// Working directory at prompt start, captured from the
    /// shell's OSC 7 `file://<host><path>` notification.
    /// `None` when the shell hasn't emitted OSC 7 (older
    /// configurations or pure /bin/sh).
    #[serde(default)]
    pub cwd: Option<String>,
    /// WHO ran this block — the provenance of the pane that
    /// produced it, stamped at prompt start.
    ///
    /// Not `Option`: every block answers the question, and
    /// [`Yurai::Unknown`] IS an answer — the honest one for a
    /// pane no attested connection minted, or for a record
    /// written before this field existed. Making it optional
    /// would let a consumer skip the question entirely, which
    /// is the state this field exists to remove.
    ///
    /// **This is the field a block history is worth having.**
    /// Without it an agent-run `rm -rf` and an operator-run one
    /// are the same row, so "who ran this" is answerable only
    /// by correlating logs — a reconstruction, not a record.
    /// With it, attribution is carried by the artifact itself.
    ///
    /// Tier-honest: this records a CLAIM at the tier it was
    /// made. `Yurai::Automation` means a connection declared
    /// itself automation; it is not proof, and
    /// [`Yurai`]'s own docs are the authority on that boundary.
    #[serde(default)]
    pub yurai: Yurai,
}

impl Block {
    /// Wall-clock duration of the block (output end - start).
    /// `None` while still in progress.
    #[must_use]
    pub fn duration_ms(&self) -> Option<u64> {
        self.ended_at_unix_ms
            .map(|end| end.saturating_sub(self.started_at_unix_ms))
    }
}
