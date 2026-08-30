//! `tear-ws-bridge` binary — runs the [`tear_ws_bridge::start`]
//! accept loop as a foreground daemon.

#![forbid(unsafe_code)]

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(
    name = "tear-ws-bridge",
    version,
    about = "WebSocket bridge for tear-daemon — turns the CBOR wire into ws:// for browser / wasm renderers."
)]
struct Cli {
    /// WebSocket listen address.
    #[arg(long, default_value = "127.0.0.1:8181")]
    listen: std::net::SocketAddr,
    /// Tear-daemon target — UDS path or `tcp://host:port`.
    /// Defaults to the standard XDG socket path.
    #[arg(long)]
    tear: Option<String>,
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let tear = match cli.tear {
        Some(s) => tear_client::Transport::parse(&s)?,
        None => tear_client::Transport::Unix(tear_types::wire::default_socket_path()),
    };

    let bridge = tear_ws_bridge::start(tear_ws_bridge::BridgeConfig {
        listen: cli.listen,
        tear,
    })?;
    info!(listen = %bridge.listen, "tear-ws-bridge ready");
    println!("tear-ws-bridge listening on ws://{}", bridge.listen);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_handler = stop.clone();
    ctrlc::set_handler(move || stop_for_handler.store(true, Ordering::SeqCst))?;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    println!("\ntear-ws-bridge stopping...");
    bridge.stop();
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("tear_ws_bridge=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .try_init();
}
