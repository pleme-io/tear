//! `tear-daemon` — long-running tear server.
//!
//! Owns sessions across client disconnects; exposes typed UDS RPC so
//! `tear-client` (or any consumer — mado at Tier 2, fleet operators
//! over SSH at Tier 3) can drive the [`tear_types::MultiplexerControl`]
//! surface remotely.
//!
//! Wraps `tear_core::InProcess` rather than reimplementing — pane
//! semantics live in one place fleet-wide.

#![forbid(unsafe_code)]
