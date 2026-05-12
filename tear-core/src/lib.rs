//! `tear-core` — runtime logic for the tear multiplexer.
//!
//! Houses the session/window/pane state machine, PTY ownership
//! (`portable-pty`), layout algorithms, tmux.conf parser, and format-
//! string evaluator. No daemon — those live in `tear-daemon`.
//!
//! The `InProcess` impl of [`tear_types::MultiplexerControl`] is the
//! single source of truth for pane semantics across the substrate:
//! `tear-daemon` wraps it for cross-process use; `mado` will swap its
//! current `pane.rs`/`tab.rs` to consume it directly (post-tear M2 —
//! see project memory `project_tear_mado_overlap`).

#![forbid(unsafe_code)]
