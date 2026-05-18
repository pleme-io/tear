//! Wire-shape mirror of `tear_core::blocks::Block`.
//!
//! Lives in `tear-types` because the daemon's wire layer needs
//! the type but can't depend on `tear-core` (no upward dep —
//! tear-core depends on tear-types). The two crate's `Block`
//! structs share the same serde representation byte-for-byte;
//! we use `From` conversions on the tear-core side to bridge.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub index: u64,
    pub prompt: String,
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: Option<u64>,
}
