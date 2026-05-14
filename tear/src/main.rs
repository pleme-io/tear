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
    /// Create a new session and start its first pane.
    Up {
        /// Session name. Defaults to a generated label.
        #[arg(short, long)]
        name: Option<String>,
        /// Shell to spawn. Defaults to $SHELL (or /bin/sh).
        #[arg(short, long)]
        shell: Option<String>,
    },
    /// List active sessions / windows / panes.
    List {
        /// Render as YAML instead of human-readable.
        #[arg(long)]
        yaml: bool,
    },
    /// Kill a session by id (or name with --name).
    Kill {
        id: String,
        /// Resolve `id` as a session name instead of an id.
        #[arg(long)]
        name: bool,
    },
    /// Rename a session.
    Rename {
        id: String,
        new_name: String,
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
    /// Placeholder — M2 daemon attach. Reports a clear error today.
    Attach {
        target: Option<String>,
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
        Cmd::Up { name, shell } => cmd_up(name, shell),
        Cmd::List { yaml } => cmd_list(yaml),
        Cmd::Kill { id, name } => cmd_kill(&id, name),
        Cmd::Rename { id, new_name } => cmd_rename(&id, &new_name),
        Cmd::ConfigCheck => cmd_config_check(),
        Cmd::ConfigPath => {
            let p = tear_config::default_config_path();
            println!("{}", p.display());
            Ok(())
        }
        Cmd::Render { backend } => cmd_render(backend),
        Cmd::Attach { target } => cmd_attach(target),
    }
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

fn cmd_up(name: Option<String>, shell: Option<String>) -> Result<()> {
    let inproc = InProcess::new();
    let live = LiveConfig::default();
    let cfg = live.load();
    let shell = shell.unwrap_or_else(|| cfg.default_shell.clone());
    let name = name.unwrap_or_else(|| {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("session-{n}")
    });
    let id = inproc.new_session(&name, &shell)?;
    info!(session_id = %id, name, shell, "tear up");
    println!("created session {id} ({name})");
    println!("note: M0 — session lives in this CLI process; daemon mode (persistence) lands at M2.");
    Ok(())
}

fn cmd_list(yaml: bool) -> Result<()> {
    let inproc = InProcess::new();
    let sessions = inproc.list_sessions()?;
    if yaml {
        println!("{}", serde_yaml_ng::to_string(&sessions)?);
    } else if sessions.is_empty() {
        println!("(no sessions)");
        println!("note: M0 in-process listing only; daemon-backed listing lands at M2.");
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

fn cmd_kill(id: &str, by_name: bool) -> Result<()> {
    let _ = (id, by_name);
    eprintln!("M0 — kill requires the daemon path (M2). Use `pkill tear` for the in-process CLI today.");
    std::process::exit(2);
}

fn cmd_rename(id: &str, new_name: &str) -> Result<()> {
    let _ = (id, new_name);
    eprintln!("M0 — rename requires the daemon path (M2).");
    std::process::exit(2);
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

fn cmd_attach(target: Option<String>) -> Result<()> {
    let _ = target;
    eprintln!(
        "M0 — attach requires the tear-daemon UDS path (M2). \
         Today: `tear render --backend tmux | tmux source -` then `tmux attach`."
    );
    std::process::exit(2);
}
