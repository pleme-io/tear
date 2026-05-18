//! `tear` — multi-call CLI for the tear multiplexer.
//!
//! Subcommands fall into three families:
//!
//! - **Session lifecycle**: `up`, `list`, `kill`, `rename`.
//! - **Config**: `config-check`, `config-path`, `render`.
//! - **Daemon glue**: `attach` (placeholder — M2 lands the UDS
//!   client side).
//!
//! Every command operates on a [`tear_core::InProcess`] instance OR
//! a [`tear_tmux_backend`]-rendered tmux.conf, selected via the
//! `Render --backend` flag. Defaults to `tmux`.

#![forbid(unsafe_code)]

// mimalloc global allocator — same pattern mado uses. Terminal
// multiplexers are allocation-bound on PTY pump + scrollback push;
// mimalloc's small-object path is measurably faster than the system
// allocator on macOS and Linux.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tear_config::{LiveConfig, TearConfig};
use tear_core::InProcess;
use tear_types::MultiplexerControl;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "tear",
    version,
    about = "Rust-native tmux-compatible terminal multiplexer with typed shikumi config.",
    long_about = "tear weaves panes, windows, and sessions into a working fabric. \
                  Composes with mado at tier 2 (GPU-native splits) and with stock \
                  Ghostty / iTerm2 / WezTerm / xterm at tier 1 via tmux passthrough."
)]
struct Cli {
    /// Verbose tracing output (sets RUST_LOG=tear=debug if unset).
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Create a new session in the running tear-daemon. Routes
    /// through tear-client → UDS → daemon, so the session
    /// persists across `tear` CLI invocations (and across mado
    /// restarts when mado is the consumer).
    Up {
        /// Session name. Defaults to a generated label.
        #[arg(short, long)]
        name: Option<String>,
        /// Shell to spawn. Defaults to the daemon's
        /// `default_shell` config (or /bin/sh).
        #[arg(short, long)]
        shell: Option<String>,
        /// Daemon UDS path. Defaults to the standard XDG location.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// List the daemon's active sessions / windows / panes.
    List {
        /// Render as YAML instead of human-readable.
        #[arg(long)]
        yaml: bool,
        /// Daemon UDS path. Defaults to the standard XDG location.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Kill a daemon session by id (or name with --name).
    Kill {
        id: String,
        /// Resolve `id` as a session name instead of an id.
        #[arg(long)]
        name: bool,
        /// Daemon UDS path. Defaults to the standard XDG location.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Rename a daemon session.
    Rename {
        id: String,
        new_name: String,
        /// Daemon UDS path. Defaults to the standard XDG location.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Probe daemon reachability. Prints `{reachable, socket,
    /// sessions, version}` as text or JSON (--json). Suitable for
    /// shell-prompt hooks (starship custom command, p10k poweradd,
    /// etc.) — returns exit code 0 when reachable, 1 when not.
    Status {
        /// Daemon UDS path. Defaults to the standard XDG location.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Suppress text output; only the exit code is meaningful.
        /// Useful for fast prompt-hook polling.
        #[arg(long)]
        quiet: bool,
    },
    /// Validate the shikumi config at the canonical path.
    ConfigCheck,
    /// Print the canonical config path (~/.config/tear/tear.yaml).
    ConfigPath,
    /// Render the live shikumi config into a backend-specific format
    /// and print it to stdout. With `--backend tmux` this produces a
    /// drop-in `tmux.conf`.
    Render {
        #[arg(long, value_enum, default_value_t = Backend::Tmux)]
        backend: Backend,
    },
    /// Connect to a running tear-daemon and print its session list.
    /// (Stub interactive attach is a Phase-4 mado-side concern; today
    /// this proves the daemon ↔ client RPC path end-to-end.)
    Attach {
        target: Option<String>,
        /// Daemon UDS path. Defaults to `$XDG_RUNTIME_DIR/tear.sock`
        /// (or `~/.local/share/tear/tear.sock`).
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Start a long-running tear-daemon listening on a UDS. The
    /// daemon owns sessions across client disconnects; mado, the
    /// `tear attach` CLI, and any other typed driver can connect.
    /// Blocks until SIGINT.
    Daemon {
        /// UDS path. Defaults to `$XDG_RUNTIME_DIR/tear.sock` (or
        /// `~/.local/share/tear/tear.sock`). The directory is created
        /// if missing; a stale socket file at the path is unlinked.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Print a pane's rendered cell grid (Phase 2 introspection).
    /// Useful for manually verifying that `send-keys` round-tripped
    /// through the daemon ↔ PTY ↔ vte parser path.
    Snapshot {
        /// Pane id (16-char lowercase hex — as printed by `tear list`).
        pane: String,
        /// Daemon UDS path. Defaults to the standard XDG location.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Backend {
    /// Render the typed profile into a tmux configuration string.
    Tmux,
    /// Print the typed in-process state as YAML (debugging aid).
    Yaml,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Cmd::Up { name, shell, socket } => cmd_up(name, shell, socket),
        Cmd::List { yaml, socket } => cmd_list(yaml, socket),
        Cmd::Kill { id, name, socket } => cmd_kill(&id, name, socket),
        Cmd::Rename { id, new_name, socket } => cmd_rename(&id, &new_name, socket),
        Cmd::Status { socket, json, quiet } => cmd_status(socket, json, quiet),
        Cmd::ConfigCheck => cmd_config_check(),
        Cmd::ConfigPath => {
            let p = tear_config::default_config_path();
            println!("{}", p.display());
            Ok(())
        }
        Cmd::Render { backend } => cmd_render(backend),
        Cmd::Attach { target, socket } => cmd_attach(target, socket),
        Cmd::Daemon { socket } => cmd_daemon(socket),
        Cmd::Snapshot { pane, socket } => cmd_snapshot(&pane, socket),
    }
}

/// Common daemon-connect helper for every subcommand that routes
/// through the running daemon. Extracts the repetitive
/// "resolve socket → connect → error-message-with-hint" pattern.
/// Returns the Client + the resolved socket path so callers can
/// include the path in their output (helpful for the operator).
fn connect_to_daemon(
    socket: Option<std::path::PathBuf>,
) -> Result<(tear_client::Client, std::path::PathBuf)> {
    let socket_path = socket.unwrap_or_else(tear_types::wire::default_socket_path);
    let client = tear_client::Client::connect(&socket_path).map_err(|e| {
        anyhow::anyhow!(
            "tear-daemon not reachable at {}: {}\nStart it with: tear daemon \
             (or enable the launchd/systemd user unit via the tear flake's HM module)",
            socket_path.display(),
            e
        )
    })?;
    Ok((client, socket_path))
}

fn init_tracing(verbose: bool) {
    use tracing_subscriber::EnvFilter;
    let filter = if verbose {
        EnvFilter::new("tear=debug,tear_core=debug,tear_config=debug,tear_tmux_backend=debug")
    } else {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("tear=info,tear_core=warn"))
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .try_init();
}

fn cmd_up(
    name: Option<String>,
    shell: Option<String>,
    socket: Option<std::path::PathBuf>,
) -> Result<()> {
    let (client, _socket_path) = connect_to_daemon(socket)?;
    // The daemon already enforces its own default_shell from its
    // live config; we only need to fall back here if the user
    // didn't pass one and the daemon doesn't either. The simpler
    // path: leave shell resolution to the daemon, which lets a
    // single `tear daemon`-side config update change every CLI
    // invocation's default without recompile.
    let shell = shell.unwrap_or_else(|| {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    });
    let name = name.unwrap_or_else(|| {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("session-{n}")
    });
    let id = client.new_session(&name, &shell)?;
    info!(session_id = %id, name, shell, "tear up");
    println!("created session {id} ({name}) in daemon");
    Ok(())
}

fn cmd_list(yaml: bool, socket: Option<std::path::PathBuf>) -> Result<()> {
    let (client, socket_path) = connect_to_daemon(socket)?;
    let sessions = client.list_sessions()?;
    if yaml {
        println!("{}", serde_yaml_ng::to_string(&sessions)?);
    } else if sessions.is_empty() {
        println!("(no sessions on {})", socket_path.display());
    } else {
        for s in sessions {
            println!(
                "{} {}  windows={} panes={}  state={:?}",
                s.id,
                s.name,
                s.windows.len(),
                s.panes.len(),
                s.state,
            );
        }
    }
    Ok(())
}

fn cmd_kill(id: &str, by_name: bool, socket: Option<std::path::PathBuf>) -> Result<()> {
    let (client, _socket_path) = connect_to_daemon(socket)?;
    let session_id = resolve_session_id(&client, id, by_name)?;
    client.kill_session(session_id)?;
    println!("killed session {session_id}");
    Ok(())
}

fn cmd_rename(
    id: &str,
    new_name: &str,
    socket: Option<std::path::PathBuf>,
) -> Result<()> {
    let (client, _socket_path) = connect_to_daemon(socket)?;
    // Rename only accepts the id form on the wire; `--name`-style
    // lookup isn't exposed at the CLI level (no operator hit asks
    // for it). Keep the surface narrow until proven otherwise.
    let session_id = id
        .parse::<tear_types::SessionId>()
        .map_err(|e| anyhow::anyhow!("invalid session id `{id}`: {e}"))?;
    client.rename_session(session_id, new_name)?;
    println!("renamed {session_id} → {new_name}");
    Ok(())
}

/// Resolve a CLI argument to a `SessionId`. Honors `--name` by
/// looking up the daemon's session list; otherwise parses as a hex
/// id. Lifted out of `cmd_kill` so future commands (e.g. snapshot
/// taking a session name) can share the same resolution.
fn resolve_session_id(
    client: &tear_client::Client,
    arg: &str,
    by_name: bool,
) -> Result<tear_types::SessionId> {
    if !by_name {
        return arg
            .parse::<tear_types::SessionId>()
            .map_err(|e| anyhow::anyhow!("invalid session id `{arg}`: {e}"));
    }
    let sessions = client.list_sessions()?;
    let hits: Vec<_> = sessions.iter().filter(|s| s.name == arg).collect();
    match hits.as_slice() {
        [] => Err(anyhow::anyhow!("no session named `{arg}`")),
        [s] => Ok(s.id),
        many => Err(anyhow::anyhow!(
            "name `{arg}` matches {} sessions: {}. Specify by id instead.",
            many.len(),
            many.iter()
                .map(|s| s.id.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

/// `tear status` — operator visibility into the daemon's health.
/// Exit code 0 when the daemon is reachable, 1 when it isn't, so
/// shell prompts can branch on `if tear status --quiet; then …`.
fn cmd_status(
    socket: Option<std::path::PathBuf>,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let socket_path = socket.unwrap_or_else(tear_types::wire::default_socket_path);
    let socket_str = socket_path.to_string_lossy().to_string();
    let version = env!("CARGO_PKG_VERSION");

    let probe = tear_client::Client::connect(&socket_path);
    match probe {
        Ok(client) => {
            let sessions = client.list_sessions().unwrap_or_default();
            if quiet {
                return Ok(());
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "reachable": true,
                        "socket": socket_str,
                        "sessions": sessions.len(),
                        "version": version,
                    })
                );
            } else {
                println!(
                    "tear-daemon: ok  socket={socket_str}  sessions={}  version={version}",
                    sessions.len()
                );
            }
            Ok(())
        }
        Err(e) => {
            if !quiet {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "reachable": false,
                            "socket": socket_str,
                            "error": e.to_string(),
                            "version": version,
                            "hint": "tear daemon (or enable programs.tear.daemon.enable in HM)",
                        })
                    );
                } else {
                    eprintln!(
                        "tear-daemon: unreachable  socket={socket_str}  error={e}\n\
                         hint: `tear daemon` (or enable programs.tear.daemon.enable in HM)"
                    );
                }
            }
            std::process::exit(1);
        }
    }
}

fn cmd_config_check() -> Result<()> {
    let path = tear_config::default_config_path();
    if !path.exists() {
        println!("note: {} does not exist — defaults will be used.", path.display());
        println!("ok (defaults are valid)");
        let _ = TearConfig::default();
        return Ok(());
    }
    match tear_config::load_from(&path) {
        Ok(cfg) => {
            println!("ok  {}", path.display());
            println!(
                "  prefix={}  shell={}  mouse={}  base_index={}",
                cfg.prefix, cfg.default_shell, cfg.mouse, cfg.base_index
            );
            println!(
                "  keys={}  status-visible={}  theme={}",
                cfg.keys.len(),
                cfg.status.visible,
                cfg.theme.name
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("error: {} — {}", path.display(), e);
            std::process::exit(2);
        }
    }
}

fn cmd_render(backend: Backend) -> Result<()> {
    let live = LiveConfig::default();
    let cfg = live.load();
    let out = match backend {
        Backend::Tmux => tear_tmux_backend::render_tmux_conf(&cfg),
        Backend::Yaml => serde_yaml_ng::to_string(&*cfg)?,
    };
    print!("{out}");
    Ok(())
}

fn cmd_attach(target: Option<String>, socket: Option<std::path::PathBuf>) -> Result<()> {
    let _ = target; // future: dial-target selection (named session, last, etc.)
    let socket_path = socket.unwrap_or_else(tear_types::wire::default_socket_path);
    let client = tear_client::Client::connect(&socket_path).map_err(|e| {
        anyhow::anyhow!(
            "tear-daemon not reachable at {}: {}\nStart it with: tear daemon",
            socket_path.display(),
            e
        )
    })?;
    let sessions = client.list_sessions()?;
    if sessions.is_empty() {
        println!("(no sessions on {})", socket_path.display());
    } else {
        println!("connected to tear-daemon at {}", socket_path.display());
        for s in sessions {
            println!(
                "  {} {}  windows={} panes={}  state={:?}",
                s.id,
                s.name,
                s.windows.len(),
                s.panes.len(),
                s.state
            );
        }
    }
    Ok(())
}

fn cmd_snapshot(pane: &str, socket: Option<std::path::PathBuf>) -> Result<()> {
    let socket_path = socket.unwrap_or_else(tear_types::wire::default_socket_path);
    let client = tear_client::Client::connect(&socket_path).map_err(|e| {
        anyhow::anyhow!(
            "tear-daemon not reachable at {}: {}",
            socket_path.display(),
            e
        )
    })?;
    let pane_id: tear_types::PaneId = pane
        .parse()
        .map_err(|e: anyhow::Error| anyhow::anyhow!("invalid pane id `{pane}`: {e}"))?;
    let snap = client.pane_snapshot(pane_id)?;
    // Print a header so the rendered grid is visually framed.
    println!("─ pane {pane_id} ({}x{}) ─", snap.cols, snap.rows);
    for row in snap.to_text_rows() {
        println!("│{}│", row);
    }
    println!("─ cursor: row {} col {} ─", snap.cursor_row, snap.cursor_col);
    Ok(())
}

fn cmd_daemon(socket: Option<std::path::PathBuf>) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let socket_path = socket.unwrap_or_else(tear_types::wire::default_socket_path);
    let inproc = Arc::new(InProcess::new());
    let handle = tear_daemon::start(socket_path.clone(), inproc).map_err(|e| {
        anyhow::anyhow!(
            "tear-daemon failed to bind {}: {}",
            socket_path.display(),
            e
        )
    })?;
    info!(socket = %socket_path.display(), "tear-daemon ready");
    println!("tear-daemon listening on {}", socket_path.display());

    // Block until SIGINT / SIGTERM. The handle stops + cleans up the
    // socket on drop, so a panic or normal exit both leave the
    // filesystem clean.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_handler = stop.clone();
    ctrlc::set_handler(move || stop_for_handler.store(true, Ordering::SeqCst))
        .map_err(|e| anyhow::anyhow!("install signal handler: {e}"))?;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    println!("\ntear-daemon shutting down...");
    handle.stop();
    Ok(())
}
