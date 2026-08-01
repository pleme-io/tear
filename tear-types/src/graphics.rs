//! Inline-image payloads carried through the authority.
//!
//! ## The seam: the authority owns TRANSMISSION, the renderer owns PIXELS
//!
//! A terminal image arrives as an escape sequence carrying encoded bytes —
//! a sixel stream, or a PNG / raw RGBA under the kitty protocol. Two
//! separate facts come out of that, and conflating them is what makes this
//! hard:
//!
//! 1. **What was transmitted, and where the cursor was.** That is terminal
//!    state. It belongs to the authority, exactly like a cell.
//! 2. **What those bytes look like as pixels.** That is rendering. It needs
//!    a PNG decoder, a sixel decoder, a GPU texture.
//!
//! tear owns (1) and deliberately **not** (2). The payload is stored
//! undecoded, so `tear-core` needs no `image` crate, no `icy_sixel`, and no
//! GPU dependency — a daemon on a headless box carries images perfectly
//! well without being able to draw one.
//!
//! This is what closes the last flip blocker in
//! [`SHUKEN`](https://github.com/pleme-io/tear/blob/main/docs/SHUKEN.md).
//! Before it, `GridState` implemented no `hook`/`put`/`unhook` and vte
//! silently swallows APC in its `SosPmApcString` state, so **every sixel
//! and every kitty image vanished with no error and no flag** — a renderer
//! could not even know content had been dropped. Carrying them undecoded
//! keeps mado's decoders exactly where they are while making the authority
//! lossless.

use serde::{Deserialize, Serialize};

/// Largest payload accepted for one image.
///
/// Matches mado's `SIXEL_DCS_MAX` / `APC_MAX`. A terminal image is bounded
/// by what a program can reasonably paint; anything past this is a runaway
/// or hostile stream, and accepting it would let a child process drive the
/// daemon out of memory.
pub const GRAPHIC_PAYLOAD_MAX: usize = 8 * 1024 * 1024;

/// Which protocol delivered a payload. The renderer needs this to pick a
/// decoder; the authority only needs to record it faithfully.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphicProtocol {
    /// DCS `q` — a sixel stream.
    Sixel,
    /// APC `G` — the kitty graphics protocol. `params` carries its
    /// key/value prefix (`a=T,f=100,…`), which says whether the data is
    /// PNG or raw RGBA, and how it is chunked.
    Kitty,
}

/// One transmitted image, undecoded, with the cursor position it arrived
/// at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graphic {
    pub protocol: GraphicProtocol,
    /// Protocol parameters preceding the payload. Empty for sixel, whose
    /// parameters are part of the stream itself.
    pub params: String,
    /// The payload exactly as transmitted. **Never decoded here** — see
    /// the module docs.
    pub data: Vec<u8>,
    /// Cursor row when the sequence completed, so a renderer can place it
    /// without re-deriving where the program thought it was.
    pub at_row: usize,
    /// Cursor column when the sequence completed.
    pub at_col: usize,
    /// True when the payload hit [`GRAPHIC_PAYLOAD_MAX`] and was cut.
    ///
    /// Recorded rather than dropped: a truncated image is a fact the
    /// renderer must be able to SEE, because silently rendering a partial
    /// image is worse than rendering none. This is the flag whose absence
    /// made the old swallow-everything behaviour undiagnosable.
    pub truncated: bool,
}

impl Graphic {
    /// Bytes actually retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_truncated_graphic_says_so_rather_than_pretending_to_be_whole() {
        let g = Graphic {
            protocol: GraphicProtocol::Sixel,
            params: String::new(),
            data: vec![0u8; 4],
            at_row: 2,
            at_col: 5,
            truncated: true,
        };
        assert!(g.truncated, "a renderer must be able to see the cut");
        assert_eq!(g.len(), 4);
    }

    #[test]
    fn the_payload_bound_is_the_same_one_mado_uses() {
        assert_eq!(GRAPHIC_PAYLOAD_MAX, 8 * 1024 * 1024);
    }
}
