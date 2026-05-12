//! `tear` — multi-call CLI for the tear multiplexer.
//!
//! Subcommands (planned): `up`, `attach`, `snapshot`, `restore`,
//! `render --backend tmux|daemon`. M0 ships the tmux backend renderer
//! first; daemon mode follows.

#![forbid(unsafe_code)]

fn main() {
    eprintln!(
        "tear v{} — Rust-native tmux-compatible multiplexer. Scaffolding stage.",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(1);
}
