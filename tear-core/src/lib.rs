//! `tear-core` — runtime logic for the tear multiplexer.
//!
//! Houses the session/window/pane state machine, PTY ownership
//! (`portable-pty`), the layout engine, and the [`InProcess`]
//! implementation of [`tear_types::MultiplexerControl`].
//!
//! ## Why this matters for the mado integration
//!
//! `mado` currently owns its own `pane.rs`/`tab.rs` with private
//! state machines. At M5 those modules rebase onto
//! `tear-core::InProcess` — both apps then share one source of
//! truth for pane semantics. This crate's `InProcess` impl is the
//! gravitational center the eventual rebase lands on.
//!
//! No daemon — those live in `tear-daemon`. Daemon-side composition
//! wraps `InProcess` rather than reimplementing.

#![forbid(unsafe_code)]

pub mod blocks;
#[cfg(feature = "engate")]
pub mod engate_producer;
pub mod inproc;
pub mod pane_grid;
pub mod pty;
pub mod reap;
pub mod recording;
pub mod registry;

/// Terminal-conformance rows against the `espelho` contract.
///
/// Lives in `src/` rather than `tests/` deliberately: it constructs and
/// feeds a [`PaneGrid`] directly, and those verbs are `pub(crate)` so that
/// no consumer outside this crate can mint a second authoritative grid (see
/// the authority note on [`pane_grid::PaneGrid`]). An integration test links
/// the crate as an external consumer and would therefore be denied the same
/// access mado is — correctly. Being a unit test is what lets it keep
/// testing the sealed surface.
#[cfg(test)]
mod espelho_conformance;

pub use blocks::{Block, BlockExtractor};
pub use inproc::InProcess;
pub use reap::AllPanesExited;
pub use pane_grid::{Cell, PaneGrid, PaneSnapshot};
pub use recording::{PaneEvent, PaneRecording};
