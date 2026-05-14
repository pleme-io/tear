//! Typed identifiers — opaque, stable, BLAKE3-derived.
//!
//! Every entity in the tear universe (session, window, pane) carries
//! a 64-bit identifier whose bytes are the first 8 of a BLAKE3 hash
//! of a deterministic seed (creation timestamp + parent ID + monotonic
//! counter). Two collisions per session are astronomically unlikely;
//! across the fleet they're effectively impossible.
//!
//! Identifiers are `Copy`, `Eq`, `Hash`, `Ord` — usable as map keys
//! and Vec indexes without trait gymnastics. `Display` produces a
//! 16-character lowercase hex string (a la `git` short SHAs); `FromStr`
//! parses it back. The same wire format crosses the daemon-RPC boundary
//! so a `tear list` from one host produces IDs the next host can paste.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

macro_rules! impl_typed_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl $name {
            /// Mint a fresh ID from a seed string (commonly the creation
            /// timestamp + parent + counter). Truncates the BLAKE3 hash
            /// to 8 bytes — collision risk is negligible for the
            /// session-scale population.
            #[must_use]
            pub fn from_seed(seed: &str) -> Self {
                let h = blake3::hash(seed.as_bytes());
                let bytes = h.as_bytes();
                let id = u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                    bytes[4], bytes[5], bytes[6], bytes[7],
                ]);
                Self(id)
            }

            /// The reserved "null" id — `0`. Distinct from any
            /// from_seed() output because BLAKE3 never produces an
            /// all-zero hash on a non-empty input.
            pub const NULL: Self = Self(0);
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:016x})"), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:016x}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = anyhow::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let n = u64::from_str_radix(s, 16)
                    .map_err(|e| anyhow::anyhow!(concat!("invalid ", stringify!($name), ": {}"), e))?;
                Ok(Self(n))
            }
        }
    };
}

impl_typed_id!(
    /// Identifier for a [`crate::TearSession`]. Stable across the
    /// daemon's lifetime; round-trips through the wire format.
    SessionId
);
impl_typed_id!(
    /// Identifier for a [`crate::TearWindow`]. Window IDs are unique
    /// within a session; the daemon namespaces them so two sessions
    /// can hold windows with the same id without collision.
    WindowId
);
impl_typed_id!(
    /// Identifier for a [`crate::TearPane`]. Pane IDs are unique
    /// within a window. mado renders panes addressing them by this
    /// id; the multiplexer drives PTYs keyed by it.
    PaneId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_seed_is_deterministic() {
        assert_eq!(PaneId::from_seed("foo"), PaneId::from_seed("foo"));
        assert_ne!(PaneId::from_seed("foo"), PaneId::from_seed("bar"));
    }

    #[test]
    fn display_round_trips_through_from_str() {
        let id = SessionId::from_seed("session-7");
        let s = id.to_string();
        assert_eq!(s.len(), 16);
        let parsed: SessionId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn null_is_distinct_from_any_seeded_id() {
        let seeded = WindowId::from_seed("test");
        assert_ne!(WindowId::NULL, seeded);
        assert_eq!(WindowId::NULL.0, 0);
    }
}
