//! `tear-tmux-backend` — `MultiplexerControl` impl that delegates to
//! vanilla tmux.
//!
//! Renders a typed `TearProfile` to a `tmux.conf`, spawns/attaches
//! sessions via tmux's command interface. The M0 deliverable that
//! flips blackmatter-shell's dormant tmux module on through tear's
//! typed authoring layer, before `tear-daemon` exists.
//!
//! Kept permanently as the escape hatch for remote hosts that have
//! tmux but not tear.

#![forbid(unsafe_code)]
