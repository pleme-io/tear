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

pub mod inproc;
pub mod pane_grid;
pub mod pty;
pub mod registry;

pub use inproc::InProcess;
pub use pane_grid::{Cell, PaneGrid, PaneSnapshot};
