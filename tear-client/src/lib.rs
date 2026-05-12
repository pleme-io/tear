//! `tear-client` — typed RPC client for `tear-daemon`.
//!
//! Connects over UDS locally, over SSH/mosh-tunneled UDS remotely.
//! Speaks the same typed `MultiplexerControl` trait the daemon
//! implements — connection mode is the *only* difference visible to
//! the consumer.

#![forbid(unsafe_code)]
