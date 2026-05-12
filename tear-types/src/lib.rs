//! `tear-types` — pure typed domain for the tear multiplexer.
//!
//! Owns the typed primitives (`TearSession`, `TearWindow`, `TearPane`,
//! `TearLayout`, `TearKeyTable`, `TearHook`, `TearStatusBar`,
//! `TearTheme`) plus the [`MultiplexerControl`] trait that every
//! backend (in-process, local daemon, remote SSH) implements.
//!
//! No I/O. No subprocess. No filesystem. Just types + serde + (in M0)
//! `#[derive(TataraDomain)]` so authors can declare sessions
//! in tatara-lisp via `(deftear-session …)`.
//!
//! See <https://github.com/pleme-io/tear> for the project plan and the
//! `MultiplexerControl` trait's three backends (InProcess, Local,
//! Remote).

#![forbid(unsafe_code)]
