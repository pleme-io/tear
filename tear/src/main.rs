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
// Typed cross-tool environment contract — the canonical TEAR_* env-var
// names live in exactly one place, shared with tear-core's pane-spawn
// path and seki's prompt.
use ishou_tokens::FleetStateVar;
use tear_config::{LiveConfig, TearConfig};
use tear_core::InProcess;
use tear_types::MultiplexerControl;
use tracing::info;

mod ai;
mod mcp;
mod top;

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
        /// Provenance tag — written into the daemon's session
        /// metadata so `tear list --source` can audit. Accepts
        /// `human` (default), `agent`, or `named:<label>`.
        #[arg(long, default_value = "human")]
        source: String,
    },
    /// List the daemon's active sessions / windows / panes.
    List {
        /// Render as YAML instead of human-readable.
        #[arg(long)]
        yaml: bool,
        /// Daemon UDS path. Defaults to the standard XDG location.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// Filter by provenance. Accepts `human`, `agent`,
        /// `named` (any named source), or `named:<label>` (exact
        /// named match). Omit to list everything.
        #[arg(long)]
        source: Option<String>,
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
    /// Set a pane's typed input policy. `lock` → Locked (rejects
    /// every send_keys until unlocked); `unlock` → Free (default);
    /// `leader` → only the client whose connection identified with
    /// the matching `--leader-id` (e.g. an AI agent) may send keys.
    /// Useful for observer / demo sessions, agent-only panes, and
    /// the migration handoff window.
    PaneInput {
        pane: String,
        #[arg(value_enum)]
        action: PaneInputAction,
        /// Required when `action == leader`. 64-bit identity the
        /// permitted client uses in `TEAR_CLIENT_ID`.
        #[arg(long)]
        leader_id: Option<u64>,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Show typed pane metadata — size, state, input policy,
    /// current subscriber count. Useful for migration ergonomics
    /// (`tear pane-info <pane>` before attaching a second mado to
    /// see if anyone's already there).
    PaneInfo {
        pane: String,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// #4 — daemon-native pane recording. `start` flips capture on
    /// (resets prior buffer). `stop` halts capture but retains
    /// the buffer for export. `export` writes asciinema v2 .cast
    /// (JSON-lines) to stdout (or `--out <path>`). `status`
    /// reports `(enabled, event_count)`.
    PaneRecord {
        pane: String,
        #[arg(value_enum)]
        action: PaneRecordAction,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// File path for `export` (defaults to stdout).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Pane-as-block: list every captured prompt+command+output
    /// block for a pane. Blocks are detected via OSC 133 prompt
    /// marks; your shell must be emitting them (powerlevel10k,
    /// starship, modern bash with `__vsc_*` hooks). Default
    /// `--limit 20` keeps the output digestible.
    Blocks {
        pane: String,
        #[arg(long, default_value_t = 0)]
        since: u64,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// #3 — handoff wrapper. Designed for the end of `.zshrc`:
    /// ensures a tear-daemon is running (auto-spawns one with
    /// `--start-daemon`), ensures a session of the given name exists
    /// (creates if missing), and emits a `eval`-able shell snippet
    /// (`export TEAR_SESSION=<id> ...`) so the calling shell can
    /// surface session identity in its prompt.
    ///
    /// Idempotent: re-running for the same `--name` returns the
    /// existing session id without creating a duplicate. Drop into
    /// your shell init:
    ///
    /// ```sh
    /// # at the end of .zshrc
    /// [ -z "$TEAR_SESSION" ] && command -v tear >/dev/null \
    ///   && eval "$(tear migrate --shell-snippet --start-daemon)"
    /// ```
    Migrate {
        /// Session name. Defaults to the current directory's basename.
        #[arg(short, long)]
        name: Option<String>,
        /// Shell to spawn if a new session is created.
        #[arg(short, long)]
        shell: Option<String>,
        /// If the daemon isn't reachable, spawn a detached `tear
        /// daemon` and poll until it accepts a connection (up to 3s).
        #[arg(long)]
        start_daemon: bool,
        /// Emit `export TEAR_SESSION=... TEAR_SESSION_NAME=...` to
        /// stdout (silently); print nothing else. Designed for
        /// `eval "$(tear migrate --shell-snippet)"`.
        #[arg(long)]
        shell_snippet: bool,
        /// Daemon UDS path. Defaults to the standard XDG location.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// #4 — replay an asciinema v2 .cast to the current terminal
    /// with timing fidelity. Pure-Rust player; no asciinema CLI
    /// dep. `--speed 2.0` plays back at 2x; `--max-delay-ms 200`
    /// caps long pauses (great for skipping `sleep` commands).
    Replay {
        cast: std::path::PathBuf,
        #[arg(long, default_value_t = 1.0)]
        speed: f32,
        #[arg(long, default_value_t = 200)]
        max_delay_ms: u64,
    },
    /// #6 audit reader — replays the daemon's JSONL audit log.
    /// Filters: --since-secs (last N seconds), --kind (one of
    /// session_create | session_kill | set_input_policy |
    /// start_recording | stop_recording | set_config). Reads
    /// the path the daemon writes to (from
    /// `services.tear.settings.audit_log` in tear.yaml).
    Audit {
        /// Explicit audit-log path. Default reads tear's config
        /// via the daemon for the canonical path.
        #[arg(long)]
        path: Option<std::path::PathBuf>,
        /// Filter to the last N seconds (0 = everything).
        #[arg(long, default_value_t = 0)]
        since_secs: u64,
        /// Filter to a specific event kind.
        #[arg(long)]
        kind: Option<String>,
        /// Max rows to return.
        #[arg(long, default_value_t = 200)]
        limit: u32,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// #4 LLM proxy — assembles context from the latest captured
    /// pane block (or `--block <index>`) and forwards to the
    /// configured LLM. Defaults to a local Ollama install. Set
    /// `services.tear.settings.ai.{provider,model,endpoint,api_key_env}`
    /// in `~/.config/tear/tear.yaml` to use anything else.
    Ai {
        prompt: Vec<String>,
        /// Pane to draw context from. Defaults to the only pane
        /// in the only active session; fails if ambiguous.
        #[arg(long)]
        pane: Option<String>,
        /// Specific block index. Default: the most recent.
        #[arg(long)]
        block: Option<u64>,
        /// Model override (defaults to the configured model).
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Semantic history — typed projection of every captured block
    /// across one pane, one session, or the whole daemon. Each
    /// row is (started_at, pane, exit_code, duration_ms, cwd,
    /// command). Filter by --pane / --session / --exit-code /
    /// --since (last hour). Text or --json output.
    History {
        #[arg(long)]
        pane: Option<String>,
        #[arg(long)]
        session: Option<String>,
        /// Only blocks newer than this many seconds ago. Default
        /// 0 means "everything retained".
        #[arg(long, default_value_t = 0)]
        since_secs: u64,
        /// Filter to exit code (e.g. --exit-code 0 for successes,
        /// --exit-code 1 for failures, etc.).
        #[arg(long)]
        exit_code: Option<i32>,
        /// Max rows to return.
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Pane-as-block: fetch one block by per-pane index. With
    /// --latest, returns the most recent completed block instead.
    Block {
        pane: String,
        #[arg(long, conflicts_with = "latest")]
        index: Option<u64>,
        #[arg(long, conflicts_with = "index")]
        latest: bool,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// #48a — Interactive curses dashboard. Live view of every
    /// session + pane (subscriber count, input policy, recording
    /// state) with keybindings for lock/unlock, record on/off,
    /// kill. `q` to quit. `--refresh-ms <n>` controls polling
    /// cadence (default 1000ms).
    Top {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        #[arg(long, default_value_t = 1000)]
        refresh_ms: u64,
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
    /// Show the materialized config at a tier (bare/default/discovered/custom/env).
    ConfigShow(shikumi::cli::ConfigShowCommand),
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
    /// Run as MCP server (stdio transport) — exposes daemon /
    /// session / pane perf introspection over JSON-RPC for
    /// Claude Code agents. Stdout is the protocol channel;
    /// tracing routes to stderr only.
    Mcp {
        /// Daemon UDS path to query. Defaults to the standard
        /// XDG location.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Start a long-running tear-daemon listening on a UDS or TCP
    /// port. The daemon owns sessions across client disconnects;
    /// mado, the `tear attach` CLI, and any other typed driver can
    /// connect. Blocks until SIGINT.
    Daemon {
        /// UDS path. Defaults to `$XDG_RUNTIME_DIR/tear.sock` (or
        /// `~/.local/share/tear/tear.sock`). The directory is created
        /// if missing; a stale socket file at the path is unlinked.
        /// Ignored when `--tcp` is set.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// #5 — bind a TCP listener instead of a UDS. Operator
        /// must front this with SSH-tunnel / WireGuard / TLS-proxy
        /// for untrusted networks (the wire is unencrypted at
        /// this layer). Use `0.0.0.0:5111` to listen on all
        /// interfaces, `127.0.0.1:5111` for localhost-only.
        #[arg(long)]
        tcp: Option<std::net::SocketAddr>,
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

#[derive(Copy, Clone, Debug, ValueEnum)]
enum PaneInputAction {
    /// Reject every subsequent send_keys for this pane until unlocked.
    Lock,
    /// Restore the default Free policy.
    Unlock,
    /// #2 — only the client identified by --leader-id may send keys.
    Leader,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum PaneRecordAction {
    Start,
    Stop,
    Export,
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // MCP must dispatch BEFORE the global tracing init below.
    // The default `init_tracing` writes to stdout, but stdout is
    // the JSON-RPC framing channel for `tear mcp` — any tracing
    // line would break protocol. cmd_mcp installs its own
    // stderr-only subscriber via shidou; doing it after the
    // stdout-fmt init would also double-set the global default
    // and panic.
    if let Cmd::Mcp { ref socket } = cli.command {
        return cmd_mcp(socket.clone());
    }

    init_tracing(cli.verbose);

    match cli.command {
        Cmd::Up { name, shell, socket, source } => cmd_up(name, shell, socket, source),
        Cmd::List { yaml, socket, source } => cmd_list(yaml, socket, source),
        Cmd::Kill { id, name, socket } => cmd_kill(&id, name, socket),
        Cmd::Rename { id, new_name, socket } => cmd_rename(&id, &new_name, socket),
        Cmd::PaneInput { pane, action, leader_id, socket } => {
            cmd_pane_input(&pane, action, leader_id, socket)
        }
        Cmd::PaneInfo { pane, socket, json } => cmd_pane_info(&pane, socket, json),
        Cmd::PaneRecord { pane, action, socket, out } => cmd_pane_record(&pane, action, socket, out),
        Cmd::Blocks { pane, since, limit, socket, json } => {
            cmd_blocks(&pane, since, limit, socket, json)
        }
        Cmd::Block { pane, index, latest, socket, json } => {
            cmd_block(&pane, index, latest, socket, json)
        }
        Cmd::History { pane, session, since_secs, exit_code, limit, socket, json } => {
            cmd_history(pane, session, since_secs, exit_code, limit, socket, json)
        }
        Cmd::Ai { prompt, pane, block, model, socket } => {
            cmd_ai(prompt, pane, block, model, socket)
        }
        Cmd::Audit { path, since_secs, kind, limit, socket, json } => {
            cmd_audit(path, since_secs, kind, limit, socket, json)
        }
        Cmd::Replay { cast, speed, max_delay_ms } => cmd_replay(&cast, speed, max_delay_ms),
        Cmd::Migrate { name, shell, start_daemon, shell_snippet, socket } => {
            cmd_migrate(name, shell, start_daemon, shell_snippet, socket)
        }
        Cmd::Top { socket, refresh_ms } => cmd_top(socket, refresh_ms),
        Cmd::Status { socket, json, quiet } => cmd_status(socket, json, quiet),
        Cmd::Mcp { socket } => cmd_mcp(socket),
        Cmd::ConfigCheck => cmd_config_check(),
        Cmd::ConfigPath => {
            let p = tear_config::default_config_path();
            println!("{}", p.display());
            Ok(())
        }
        Cmd::ConfigShow(cmd) => cmd
            .run::<TearConfig>(FleetStateVar::TearTier.name())
            .map_err(|e| anyhow::anyhow!("config-show failed: {e}")),
        Cmd::Render { backend } => cmd_render(backend),
        Cmd::Attach { target, socket } => cmd_attach(target, socket),
        Cmd::Daemon { socket, tcp } => cmd_daemon(socket, tcp),
        Cmd::Snapshot { pane, socket } => cmd_snapshot(&pane, socket),
    }
}

/// Common daemon-connect helper for every subcommand that routes
/// through the running daemon. Extracts the repetitive
/// "resolve socket → connect → error-message-with-hint" pattern.
/// Returns the Client + the resolved socket path so callers can
/// include the path in their output (helpful for the operator).
///
/// `socket` accepts either a filesystem path (UDS) or a
/// `tcp://host:port` URL — see [`tear_client::Transport::parse`].
fn connect_to_daemon(
    socket: Option<std::path::PathBuf>,
) -> Result<(tear_client::Client, std::path::PathBuf)> {
    // Resolve the typed transport. If the operator passed
    // `tcp://...` as the --socket value, route via Tcp; else
    // treat it as a UDS path. Backward-compat: a None input
    // falls through to default_socket_path().
    let raw = socket
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| {
            tear_types::wire::default_socket_path()
                .display()
                .to_string()
        });
    let transport = tear_client::Transport::parse(&raw).map_err(|e| {
        anyhow::anyhow!("invalid --socket value `{raw}`: {e}")
    })?;
    // #5 — auth token passes through the env var. The daemon checks
    // `config.auth_token_env` and resolves the *same* env var on its
    // side; an explicit name (e.g. `TEAR_AUTH_TOKEN`) is the canonical
    // shape, but we don't hardcode it here — we read whatever the
    // operator exported as `TEAR_AUTH_TOKEN`. Daemons without an
    // `auth_token_env` configured silently accept the Authenticate;
    // daemons with one configured but a *different* env name still
    // work via the same env var name `TEAR_AUTH_TOKEN` so long as
    // operators stick to the convention. Custom env names: export the
    // same value under `TEAR_AUTH_TOKEN` on the client side.
    let auth_token = std::env::var("TEAR_AUTH_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let mut client = tear_client::Client::connect_transport_with_auth(
        transport.clone(),
        auth_token,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "tear-daemon not reachable at {}: {}\nStart it with: tear daemon \
             (or enable the launchd/systemd user unit via the tear flake's HM module)",
            transport.display_string(),
            e
        )
    })?;
    // #2 — opt-in client identity for Leader policy. Parse as u64;
    // an unparseable value is operator misconfiguration and bubbles
    // up as a clear anyhow error.
    if let Ok(raw) = std::env::var(FleetStateVar::TearClientId.name()) {
        if !raw.is_empty() {
            let id: u64 = raw.parse().map_err(|e| {
                anyhow::anyhow!("TEAR_CLIENT_ID={raw} must parse as u64: {e}")
            })?;
            client.identify_as(id).map_err(|e| {
                anyhow::anyhow!("tear-daemon rejected IdentifyClient({id}): {e}")
            })?;
        }
    }
    Ok((client, std::path::PathBuf::from(transport.display_string())))
}

/// Parse an operator-supplied 16-hex pane id string. Centralises
/// the "bad CLI id → clear anyhow message" template so every
/// pane-taking subcommand surfaces the same shape. The optional
/// `label` is the flag/positional name (e.g. `"pane"`, `"--pane"`)
/// to make the error point at the offending input.
fn parse_pane_id(s: &str, label: &str) -> Result<tear_types::PaneId> {
    s.parse::<tear_types::PaneId>()
        .map_err(|e| anyhow::anyhow!("invalid {label} `{s}`: {e}"))
}

/// Parse an operator-supplied 16-hex session id string. Sibling of
/// [`parse_pane_id`] — same template, same error shape.
fn parse_session_id(s: &str, label: &str) -> Result<tear_types::SessionId> {
    s.parse::<tear_types::SessionId>()
        .map_err(|e| anyhow::anyhow!("invalid {label} `{s}`: {e}"))
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
    source: String,
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
    let source = parse_source_for_creation(&source)?;
    let id = client.new_session_with_source(&name, &shell, source.clone())?;
    info!(session_id = %id, name, shell, source = %source.label(), "tear up");
    println!("created session {id} ({name}) in daemon  source={}", source.label());
    Ok(())
}

fn cmd_list(
    yaml: bool,
    socket: Option<std::path::PathBuf>,
    source_filter: Option<String>,
) -> Result<()> {
    let (client, socket_path) = connect_to_daemon(socket)?;
    let mut sessions = client.list_sessions()?;
    if let Some(spec) = source_filter.as_deref() {
        let filter = parse_source_filter(spec)?;
        sessions.retain(|s| filter.matches(&s.source));
    }
    if yaml {
        println!("{}", serde_yaml_ng::to_string(&sessions)?);
    } else if sessions.is_empty() {
        println!("(no sessions on {})", socket_path.display());
    } else {
        for s in sessions {
            println!(
                "{} {}  windows={} panes={}  state={:?}  source={}",
                s.id,
                s.name,
                s.windows.len(),
                s.panes.len(),
                s.state,
                source_display(&s.source),
            );
        }
    }
    Ok(())
}

/// CLI surface for `--source human|agent|named:<label>` on
/// session-CREATION commands (`tear up`). Anything else is an
/// operator error.
fn parse_source_for_creation(spec: &str) -> Result<tear_types::SessionSource> {
    use tear_types::SessionSource;
    if let Some(label) = spec.strip_prefix("named:") {
        if label.is_empty() {
            return Err(anyhow::anyhow!(
                "--source named:<label> requires a non-empty label"
            ));
        }
        return Ok(SessionSource::Named(label.to_owned()));
    }
    match spec {
        "human" => Ok(SessionSource::Human),
        "agent" => Ok(SessionSource::Agent),
        "named" => Err(anyhow::anyhow!(
            "--source named requires a label (e.g. --source named:ci-runner)"
        )),
        other => Err(anyhow::anyhow!(
            "invalid --source `{other}`. Accepted: human | agent | named:<label>"
        )),
    }
}

/// Filter spec for `tear list --source`. Adds `named` as a
/// wildcard meaning "anything tagged Named(_)".
enum SourceFilter {
    Human,
    Agent,
    AnyNamed,
    Named(String),
}

impl SourceFilter {
    fn matches(&self, s: &tear_types::SessionSource) -> bool {
        use tear_types::SessionSource;
        match (self, s) {
            (SourceFilter::Human, SessionSource::Human) => true,
            (SourceFilter::Agent, SessionSource::Agent) => true,
            (SourceFilter::AnyNamed, SessionSource::Named(_)) => true,
            (SourceFilter::Named(want), SessionSource::Named(got)) => want == got,
            _ => false,
        }
    }
}

fn parse_source_filter(spec: &str) -> Result<SourceFilter> {
    if let Some(label) = spec.strip_prefix("named:") {
        if label.is_empty() {
            return Err(anyhow::anyhow!("--source named:<label> requires a label"));
        }
        return Ok(SourceFilter::Named(label.to_owned()));
    }
    match spec {
        "human" => Ok(SourceFilter::Human),
        "agent" => Ok(SourceFilter::Agent),
        "named" => Ok(SourceFilter::AnyNamed),
        other => Err(anyhow::anyhow!(
            "invalid --source filter `{other}`. Accepted: human | agent | named | named:<label>"
        )),
    }
}

fn source_display(s: &tear_types::SessionSource) -> String {
    use tear_types::SessionSource;
    match s {
        SessionSource::Human => "human".into(),
        SessionSource::Agent => "agent".into(),
        SessionSource::Named(label) => format!("named:{label}"),
    }
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
    let session_id = parse_session_id(id, "session id")?;
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
        return parse_session_id(arg, "session id");
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

/// `tear pane-input <pane> lock|unlock` — flip a pane's typed
/// InputPolicy via the daemon.
fn cmd_pane_input(
    pane: &str,
    action: PaneInputAction,
    leader_id: Option<u64>,
    socket: Option<std::path::PathBuf>,
) -> Result<()> {
    let (client, _) = connect_to_daemon(socket)?;
    let pane_id = parse_pane_id(pane, "pane id")?;
    let policy = match action {
        PaneInputAction::Lock => tear_types::InputPolicy::Locked,
        PaneInputAction::Unlock => tear_types::InputPolicy::Free,
        PaneInputAction::Leader => {
            let id = leader_id.ok_or_else(|| {
                anyhow::anyhow!(
                    "--leader-id <u64> is required when `action == leader` \
                     (the permitted client will export TEAR_CLIENT_ID=<id>)"
                )
            })?;
            tear_types::InputPolicy::leader(id)
        }
    };
    client.set_input_policy(pane_id, policy)?;
    println!("pane {pane_id} input_policy={}", policy.label());
    Ok(())
}

/// `tear pane-record <pane> {start|stop|export|status}` — #4
/// daemon-native recording ergonomic.
fn cmd_pane_record(
    pane: &str,
    action: PaneRecordAction,
    socket: Option<std::path::PathBuf>,
    out: Option<std::path::PathBuf>,
) -> Result<()> {
    let (client, _) = connect_to_daemon(socket)?;
    let pane_id = parse_pane_id(pane, "pane id")?;
    match action {
        PaneRecordAction::Start => {
            client.start_pane_recording(pane_id)?;
            println!("recording started: pane {pane_id}");
        }
        PaneRecordAction::Stop => {
            client.stop_pane_recording(pane_id)?;
            println!("recording stopped: pane {pane_id}");
        }
        PaneRecordAction::Export => {
            let cast = client.export_pane_recording(pane_id)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, cast.as_bytes())
                        .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
                    println!("exported recording → {}", path.display());
                }
                None => {
                    print!("{cast}");
                }
            }
        }
        PaneRecordAction::Status => {
            let (enabled, events) = client.pane_recording_status(pane_id)?;
            println!("pane {pane_id}  recording={enabled}  events={events}");
        }
    }
    Ok(())
}

/// `tear pane-info <pane>` — typed pane metadata + current
/// subscriber count. Migration ergonomic.
fn cmd_pane_info(
    pane: &str,
    socket: Option<std::path::PathBuf>,
    json: bool,
) -> Result<()> {
    let (client, _) = connect_to_daemon(socket)?;
    let pane_id = parse_pane_id(pane, "pane id")?;
    let p = client.get_pane(pane_id)?;
    let subs = client.pane_subscriber_count(pane_id).unwrap_or(0);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": pane_id.to_string(),
                "shell": p.shell,
                "size_cells": [p.size_cells.0, p.size_cells.1],
                "state": format!("{:?}", p.state),
                "input_policy": p.input_policy.label(),
                "subscribers": subs,
                "title": p.title,
            })
        );
    } else {
        println!(
            "pane {pane_id}  shell={}  size={}x{}  state={:?}  input={}  subscribers={subs}",
            p.shell,
            p.size_cells.0,
            p.size_cells.1,
            p.state,
            p.input_policy.label(),
        );
    }
    Ok(())
}

/// `tear blocks <pane>` — list captured OSC 133 blocks.
fn cmd_blocks(
    pane: &str,
    since: u64,
    limit: u32,
    socket: Option<std::path::PathBuf>,
    json: bool,
) -> Result<()> {
    let (client, _) = connect_to_daemon(socket)?;
    let pane_id = parse_pane_id(pane, "pane id")?;
    let blocks = client.pane_blocks_list(pane_id, since, limit)?;
    if json {
        println!("{}", serde_json::to_string(&blocks)?);
    } else if blocks.is_empty() {
        println!("(no captured blocks for {pane_id} — make sure your shell emits OSC 133 prompt marks)");
    } else {
        for b in &blocks {
            let exit = b
                .exit_code
                .map(|c| format!(" exit={c}"))
                .unwrap_or_default();
            let cmd = b.command.replace('\n', "⏎");
            let cmd_short: String = cmd.chars().take(60).collect();
            println!(
                "[{:>4}]{exit}  ${cmd_short}{}",
                b.index,
                if cmd.chars().count() > 60 { "…" } else { "" }
            );
        }
    }
    Ok(())
}

/// `tear block <pane>` — fetch one block.
fn cmd_block(
    pane: &str,
    index: Option<u64>,
    latest: bool,
    socket: Option<std::path::PathBuf>,
    json: bool,
) -> Result<()> {
    let (client, _) = connect_to_daemon(socket)?;
    let pane_id = parse_pane_id(pane, "pane id")?;
    let block = if latest {
        let (total, _) = client.pane_blocks_status(pane_id)?;
        if total == 0 {
            return Err(anyhow::anyhow!("no captured blocks for pane {pane_id}"));
        }
        // Index of the most recent completed block depends on
        // ring eviction — fetch a small tail and pick the
        // highest index.
        let tail = client.pane_blocks_list(pane_id, 0, total)?;
        let last = tail
            .into_iter()
            .last()
            .ok_or_else(|| anyhow::anyhow!("no blocks returned"))?;
        last
    } else {
        let idx = index.ok_or_else(|| {
            anyhow::anyhow!("either --index <N> or --latest is required")
        })?;
        client.pane_block_at(pane_id, idx)?
    };
    if json {
        println!("{}", serde_json::to_string(&block)?);
    } else {
        println!("─ block {} ─", block.index);
        if let Some(c) = block.exit_code {
            println!("exit: {c}");
        }
        println!("prompt: {}", block.prompt.trim_end());
        println!("command: {}", block.command.trim_end());
        println!("─ output ─");
        print!("{}", block.output);
    }
    Ok(())
}

/// `tear replay <cast>` — pure-Rust asciinema v2 .cast player.
/// Streams rows via `CastPlayer` so the timing logic is reusable
/// from any other consumer (TUI scrubbers, web playback, mado
/// embedded replay).
fn cmd_replay(
    cast: &std::path::Path,
    speed: f32,
    max_delay_ms: u64,
) -> Result<()> {
    use std::io::Write;
    let content = std::fs::read_to_string(cast)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", cast.display()))?;
    let mut player = replay::CastPlayer::new(speed, max_delay_ms);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        player.feed_line(line, &mut out)?;
    }
    out.flush()?;
    Ok(())
}

mod replay {
    use std::io::{self, Write};
    use std::time::Duration;
    use tear_types::cast::{CastParseError, CastRow, CastRowKind};

    /// Timing-aware asciinema .cast replayer. Pure logic: takes
    /// one row at a time, sleeps the inter-row delta (scaled by
    /// `speed` and clamped by `max_delay_ms`), then writes the
    /// row's bytes to the provided sink.
    ///
    /// `speed > 1.0` plays back faster; `speed < 1.0` slower.
    /// `max_delay_ms` caps any single sleep so long pauses
    /// (`sleep 30s` recorded into a cast) don't stall the player.
    pub struct CastPlayer {
        speed: f32,
        max_delay_ms: u64,
        last_t: f64,
    }

    impl CastPlayer {
        #[must_use]
        pub fn new(speed: f32, max_delay_ms: u64) -> Self {
            Self {
                speed,
                max_delay_ms,
                last_t: 0.0,
            }
        }

        /// Parse + replay one line. Malformed rows are silently
        /// skipped (matches `asciinema play`'s resilience); the
        /// asciinema header (a JSON object on row 1) is also a
        /// silent skip.
        pub fn feed_line<W: Write>(
            &mut self,
            line: &str,
            out: &mut W,
        ) -> io::Result<()> {
            match CastRow::parse(line) {
                Ok(row) => self.feed_row(row, out),
                Err(CastParseError::HeaderRow) => Ok(()),
                Err(_) => Ok(()), // skip silently
            }
        }

        /// Feed a pre-parsed row — used by tests and by consumers
        /// that already hold a `CastRow` stream (e.g. a typed cast
        /// store).
        pub fn feed_row<W: Write>(
            &mut self,
            row: CastRow,
            out: &mut W,
        ) -> io::Result<()> {
            let delay = self.compute_delay_ms(row.t);
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
            self.last_t = row.t;
            if row.kind == CastRowKind::Output {
                out.write_all(row.payload.as_bytes())?;
                out.flush()?;
            }
            Ok(())
        }

        /// Pure timing math — useful in tests so we can assert the
        /// sleep budget without actually sleeping.
        #[must_use]
        pub fn compute_delay_ms(&self, t: f64) -> u64 {
            // Treat speed <= 0 as 1x to avoid div-by-zero or
            // negative scaling. Pure math; never panics.
            let speed = if self.speed <= 0.0 { 1.0 } else { f64::from(self.speed) };
            let delta = (t - self.last_t).max(0.0) / speed;
            let ms = (delta * 1000.0) as u64;
            ms.min(self.max_delay_ms)
        }

        #[must_use]
        pub fn last_t(&self) -> f64 {
            self.last_t
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn compute_delay_clamps_to_max_delay() {
            let p = CastPlayer::new(1.0, 100);
            // 5 second delta → 5000 ms, clamped to 100.
            assert_eq!(p.compute_delay_ms(5.0), 100);
        }

        #[test]
        fn compute_delay_scales_by_speed() {
            let p = CastPlayer::new(2.0, 10_000);
            // 1.0 sec at 2x = 500 ms.
            assert_eq!(p.compute_delay_ms(1.0), 500);
        }

        #[test]
        fn compute_delay_zero_speed_treated_as_one() {
            let p = CastPlayer::new(0.0, 10_000);
            assert_eq!(p.compute_delay_ms(1.0), 1_000);
        }

        #[test]
        fn compute_delay_negative_delta_is_zero() {
            let mut p = CastPlayer::new(1.0, 10_000);
            p.last_t = 10.0;
            // Row at t=5.0 (out of order) → 0 ms.
            assert_eq!(p.compute_delay_ms(5.0), 0);
        }

        #[test]
        fn feed_row_writes_output_payload() {
            let mut p = CastPlayer::new(1.0, 0);
            let row = CastRow {
                t: 0.0,
                kind: CastRowKind::Output,
                payload: "hello".into(),
            };
            let mut buf = Vec::new();
            p.feed_row(row, &mut buf).unwrap();
            assert_eq!(&buf, b"hello");
        }

        #[test]
        fn feed_row_ignores_input_payload() {
            let mut p = CastPlayer::new(1.0, 0);
            let row = CastRow {
                t: 0.0,
                kind: CastRowKind::Input,
                payload: "typed".into(),
            };
            let mut buf = Vec::new();
            p.feed_row(row, &mut buf).unwrap();
            assert!(buf.is_empty());
        }

        #[test]
        fn feed_line_skips_header_silently() {
            let mut p = CastPlayer::new(1.0, 0);
            let mut buf = Vec::new();
            p.feed_line(r#"{"version":2}"#, &mut buf).unwrap();
            assert!(buf.is_empty());
        }

        #[test]
        fn feed_line_skips_malformed_silently() {
            let mut p = CastPlayer::new(1.0, 0);
            let mut buf = Vec::new();
            p.feed_line("not json at all", &mut buf).unwrap();
            assert!(buf.is_empty());
        }

        #[test]
        fn feed_row_advances_last_t_for_input_rows_too() {
            // last_t must advance on input rows so the next output's
            // delta is computed from the correct anchor.
            let mut p = CastPlayer::new(1.0, 0);
            let r1 = CastRow { t: 1.5, kind: CastRowKind::Input, payload: "x".into() };
            let r2 = CastRow { t: 2.0, kind: CastRowKind::Output, payload: "z".into() };
            let mut buf = Vec::new();
            p.feed_row(r1, &mut buf).unwrap();
            assert_eq!(p.last_t(), 1.5);
            p.feed_row(r2, &mut buf).unwrap();
            assert_eq!(&buf, b"z");
            assert_eq!(p.last_t(), 2.0);
        }
    }
}

/// `tear migrate` — auto-handoff wrapper. Ensures daemon up,
/// ensures session of `name` exists (creates if missing), and
/// optionally emits a shell snippet that the caller `eval`s to
/// surface session identity (`TEAR_SESSION`, `TEAR_SESSION_NAME`)
/// in their prompt environment.
fn cmd_migrate(
    name: Option<String>,
    shell: Option<String>,
    start_daemon: bool,
    shell_snippet: bool,
    socket: Option<std::path::PathBuf>,
) -> Result<()> {
    let name = name.unwrap_or_else(default_session_name_for_cwd);
    let shell = shell.unwrap_or_else(|| {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    });

    let (client, socket_path) = match connect_to_daemon(socket.clone()) {
        Ok(pair) => pair,
        Err(orig) => {
            if !start_daemon {
                return Err(orig);
            }
            spawn_detached_daemon(socket.as_deref())?;
            // Poll for readiness up to ~3 s.
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(3);
            loop {
                if let Ok(pair) = connect_to_daemon(socket.clone()) {
                    break pair;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "tear migrate: spawned daemon but it did not become \
                         ready within 3s"
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    };

    let existing = client.list_sessions().map_err(|e| {
        anyhow::anyhow!("tear migrate: list_sessions failed: {e}")
    })?;
    let sid = if let Some(found) = existing.iter().find(|s| s.name == name) {
        found.id.to_string()
    } else {
        client
            .new_session_with_source(
                &name,
                &shell,
                tear_types::SessionSource::Human,
            )
            .map_err(|e| {
                anyhow::anyhow!("tear migrate: new_session failed: {e}")
            })?
            .to_string()
    };

    if shell_snippet {
        // Silent eval-friendly output only.
        let shell_quoted_name = shell_single_quote(&name);
        println!("export TEAR_SESSION={sid} TEAR_SESSION_NAME={shell_quoted_name}");
    } else {
        println!(
            "migrate: session {sid} ({name}) ready on {}",
            socket_path.display()
        );
        println!("hint: eval \"$(tear migrate --shell-snippet)\" in .zshrc");
    }
    Ok(())
}

/// Spawn a detached `tear daemon` so the CLI can return immediately
/// while the daemon takes over the socket. Uses the current
/// executable so version drift can't happen.
fn spawn_detached_daemon(socket: Option<&std::path::Path>) -> Result<()> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().map_err(|e| {
        anyhow::anyhow!("tear migrate: cannot resolve current exe: {e}")
    })?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(p) = socket {
        cmd.arg("--socket").arg(p);
    }
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("tear migrate: spawn daemon: {e}"))?;
    Ok(())
}

/// Default session name = basename of CWD, falling back to "tear".
/// Used by `tear migrate` so re-entering the same directory's shell
/// reattaches to the same session without operator ceremony.
fn default_session_name_for_cwd() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tear".to_string())
}

/// Quote a string for safe inclusion in a single-quoted POSIX shell
/// literal. Conservative: wraps in single quotes and replaces every
/// embedded single quote with `'"'"'`.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\"'\"'");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// `tear audit` — read + filter the daemon's JSONL audit log.
fn cmd_audit(
    path: Option<std::path::PathBuf>,
    since_secs: u64,
    kind_filter: Option<String>,
    limit: u32,
    socket: Option<std::path::PathBuf>,
    json: bool,
) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let log_path = match path {
        Some(p) => p,
        None => {
            let (client, _) = connect_to_daemon(socket)?;
            let yaml = client.get_config_yaml()?;
            let cfg: tear_config::TearConfig = serde_yaml_ng::from_str(&yaml)?;
            let raw = cfg.audit_log.ok_or_else(|| {
                anyhow::anyhow!("daemon has no audit_log configured (set services.tear.settings.audit_log in tear.yaml)")
            })?;
            std::path::PathBuf::from(tear_types::path::expand_tilde(&raw))
        }
    };
    let content = std::fs::read_to_string(&log_path).map_err(|e| {
        anyhow::anyhow!("read {}: {e}", log_path.display())
    })?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let cutoff = if since_secs > 0 {
        now_ms.saturating_sub(since_secs * 1000)
    } else {
        0
    };

    let mut rows: Vec<serde_json::Value> = Vec::new();
    for line in content.lines().rev() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(ts) = v.get("ts_ms").and_then(|t| t.as_u64()) {
            if ts < cutoff {
                continue;
            }
        }
        if let Some(want) = kind_filter.as_deref() {
            if v.get("kind").and_then(|k| k.as_str()) != Some(want) {
                continue;
            }
        }
        rows.push(v);
        if rows.len() >= limit as usize {
            break;
        }
    }
    rows.reverse(); // chronological

    if json {
        println!("{}", serde_json::to_string(&rows)?);
    } else if rows.is_empty() {
        println!("(no matching audit rows)");
    } else {
        for r in &rows {
            let kind = r.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
            let ts = r.get("ts_ms").and_then(|t| t.as_u64()).unwrap_or(0);
            let rest: Vec<String> = r
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter(|(k, _)| !matches!(k.as_str(), "kind" | "ts_ms"))
                        .map(|(k, v)| format!("{k}={}", v.to_string().trim_matches('"')))
                        .collect()
                })
                .unwrap_or_default();
            println!("{} {kind:<18} {}", short_ts(ts), rest.join(" "));
        }
    }
    Ok(())
}

/// `tear ai <prompt>` — assemble block context + forward to LLM.
fn cmd_ai(
    prompt: Vec<String>,
    pane_filter: Option<String>,
    block_index: Option<u64>,
    model_override: Option<String>,
    socket: Option<std::path::PathBuf>,
) -> Result<()> {
    if prompt.is_empty() {
        return Err(anyhow::anyhow!(
            "missing prompt — usage: tear ai \"why did this fail\""
        ));
    }
    let user_prompt = prompt.join(" ");
    let (client, _) = connect_to_daemon(socket)?;

    // Resolve pane: explicit --pane wins, otherwise expect exactly
    // one pane in exactly one session.
    let pane_id = if let Some(p) = pane_filter {
        parse_pane_id(&p, "--pane")?
    } else {
        let sessions = client.list_sessions()?;
        let mut all_panes: Vec<tear_types::PaneId> =
            sessions.iter().flat_map(|s| s.panes.keys().copied()).collect();
        match all_panes.len() {
            0 => return Err(anyhow::anyhow!("no panes — `tear up` first")),
            1 => all_panes.pop().unwrap(),
            n => {
                return Err(anyhow::anyhow!(
                    "{n} panes exist — pass --pane <id> explicitly"
                ))
            }
        }
    };

    // Resolve block: explicit --block wins, otherwise the
    // most recent completed one.
    let block = match block_index {
        Some(idx) => Some(client.pane_block_at(pane_id, idx)?),
        None => {
            let blocks = client.pane_blocks_list(pane_id, 0, 10_000)?;
            blocks.into_iter().last()
        }
    };

    // Load + override config.
    let cfg_yaml = client.get_config_yaml()?;
    let cfg: tear_config::TearConfig = serde_yaml_ng::from_str(&cfg_yaml)?;
    let mut ai_cfg = cfg.ai.clone().unwrap_or_default();
    if let Some(m) = model_override {
        ai_cfg.model = m;
    }
    let provider = ai::provider_from_config(&ai_cfg)?;
    let full_prompt = ai::assemble_prompt(&user_prompt, block.as_ref(), ai_cfg.context_bytes);

    let response = provider.generate(&full_prompt)?;
    print!("{response}");
    if !response.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// `tear history` — semantic history across one pane / session /
/// the whole daemon. Aggregates blocks client-side; cheap because
/// `pane_blocks_list` is already a typed wire RPC.
#[allow(clippy::too_many_arguments)]
fn cmd_history(
    pane_filter: Option<String>,
    session_filter: Option<String>,
    since_secs: u64,
    exit_code_filter: Option<i32>,
    limit: u32,
    socket: Option<std::path::PathBuf>,
    json: bool,
) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let (client, _) = connect_to_daemon(socket)?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let cutoff_ms = if since_secs > 0 {
        now_ms.saturating_sub(since_secs.saturating_mul(1000))
    } else {
        0
    };
    let session_id_filter = match session_filter.as_deref() {
        Some(s) => Some(parse_session_id(&s, "--session")?),
        None => None,
    };
    let pane_id_filter = match pane_filter.as_deref() {
        Some(s) => Some(parse_pane_id(&s, "--pane")?),
        None => None,
    };

    let sessions = client.list_sessions()?;
    let mut rows: Vec<HistoryRow> = Vec::new();
    for session in sessions {
        if let Some(sf) = session_id_filter {
            if session.id != sf {
                continue;
            }
        }
        for pane_id in session.panes.keys() {
            if let Some(pf) = pane_id_filter {
                if *pane_id != pf {
                    continue;
                }
            }
            let blocks = client.pane_blocks_list(*pane_id, 0, 10_000).unwrap_or_default();
            for b in blocks {
                if b.started_at_unix_ms < cutoff_ms {
                    continue;
                }
                if let Some(ec) = exit_code_filter {
                    if b.exit_code != Some(ec) {
                        continue;
                    }
                }
                rows.push(HistoryRow {
                    started_at_unix_ms: b.started_at_unix_ms,
                    session_id: session.id.to_string(),
                    session_name: session.name.clone(),
                    pane_id: pane_id.to_string(),
                    exit_code: b.exit_code,
                    duration_ms: b.duration_ms(),
                    cwd: b.cwd.clone(),
                    command: b.command.clone(),
                });
            }
        }
    }
    // Newest first.
    rows.sort_by(|a, b| b.started_at_unix_ms.cmp(&a.started_at_unix_ms));
    rows.truncate(limit as usize);

    if json {
        println!("{}", serde_json::to_string(&rows)?);
    } else if rows.is_empty() {
        println!("(no matching history rows)");
    } else {
        for r in &rows {
            let exit = r
                .exit_code
                .map(|c| format!("exit={c:<3}"))
                .unwrap_or_else(|| "exit=?  ".into());
            let dur = r
                .duration_ms
                .map(|d| format!("{d:>5}ms"))
                .unwrap_or_else(|| "    ?ms".into());
            let cwd = r.cwd.as_deref().unwrap_or("-");
            let cmd = r.command.trim_end().replace('\n', "⏎");
            let cmd_short: String = cmd.chars().take(60).collect();
            let cmd_ell = if cmd.chars().count() > 60 { "…" } else { "" };
            println!(
                "{} {} {} {} [{}] {}{}",
                short_ts(r.started_at_unix_ms),
                exit,
                dur,
                shorten(&r.pane_id, 8),
                cwd,
                cmd_short,
                cmd_ell,
            );
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct HistoryRow {
    started_at_unix_ms: u64,
    session_id: String,
    session_name: String,
    pane_id: String,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    cwd: Option<String>,
    command: String,
}

fn short_ts(ms: u64) -> String {
    let secs = ms / 1000;
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}

fn shorten(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…", &s[..max]) }
}

/// `tear top` — interactive curses dashboard.
fn cmd_top(
    socket: Option<std::path::PathBuf>,
    refresh_ms: u64,
) -> Result<()> {
    let (client, _) = connect_to_daemon(socket)?;
    top::run(client, refresh_ms)
}

/// `tear status` — operator visibility into the daemon's health.
/// Exit code 0 when the daemon is reachable, 1 when it isn't, so
/// shell prompts can branch on `if tear status --quiet; then …`.
fn cmd_status(
    socket: Option<std::path::PathBuf>,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let raw = socket
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| {
            tear_types::wire::default_socket_path()
                .display()
                .to_string()
        });
    let transport = tear_client::Transport::parse(&raw).map_err(|e| {
        anyhow::anyhow!("invalid --socket value `{raw}`: {e}")
    })?;
    let socket_str = transport.display_string();
    let version = env!("CARGO_PKG_VERSION");

    let probe = tear_client::Client::connect_transport(transport);
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

fn cmd_mcp(socket: Option<std::path::PathBuf>) -> Result<()> {
    // Stderr-only tracing — stdout is the MCP JSON-RPC framing
    // channel. Any tracing line on stdout would break the
    // protocol the way it does for mado mcp / rust-analyzer.
    shidou::init_tracing_to_stderr();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(mcp::run(socket))
}

fn cmd_daemon(
    socket: Option<std::path::PathBuf>,
    tcp: Option<std::net::SocketAddr>,
) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let inproc = Arc::new(InProcess::new());
    let (handle, listen_addr) = if let Some(addr) = tcp {
        let h = tear_daemon::start_tcp(addr, inproc)
            .map_err(|e| anyhow::anyhow!("tear-daemon failed to bind tcp://{addr}: {e}"))?;
        let disp = format!("tcp://{addr}");
        (h, disp)
    } else {
        let socket_path = socket.unwrap_or_else(tear_types::wire::default_socket_path);
        let h = tear_daemon::start(socket_path.clone(), inproc).map_err(|e| {
            anyhow::anyhow!(
                "tear-daemon failed to bind {}: {}",
                socket_path.display(),
                e
            )
        })?;
        (h, socket_path.display().to_string())
    };
    info!(listen = %listen_addr, "tear-daemon ready");
    println!("tear-daemon listening on {listen_addr}");

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

#[cfg(test)]
mod main_helper_tests {
    use super::*;

    /// Forcing function: the TEAR_TIER (config-tier env) and
    /// TEAR_CLIENT_ID (Leader-policy identity) names this binary reads
    /// come from the typed cross-tool contract — a rename on the producer
    /// (tear) or consumer (seki) side is a compile+test failure here.
    #[test]
    fn binary_env_var_names_come_from_fleet_state_contract() {
        assert_eq!(FleetStateVar::TearTier.name(), "TEAR_TIER");
        assert_eq!(FleetStateVar::TearClientId.name(), "TEAR_CLIENT_ID");
    }

    #[test]
    fn shell_single_quote_wraps_in_quotes() {
        assert_eq!(shell_single_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_single_quote_escapes_embedded_single_quotes() {
        // POSIX-safe escape: close, double-quoted-single, reopen.
        assert_eq!(shell_single_quote("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn shell_single_quote_passes_through_special_chars_safely() {
        // Backticks, $, etc. are literal inside single quotes.
        let q = shell_single_quote("$(rm -rf /)");
        assert!(q.starts_with('\''));
        assert!(q.ends_with('\''));
        // The dangerous chars are inside the quotes, so the shell
        // never expands them.
        assert!(q.contains("$(rm -rf /)"));
    }

    #[test]
    fn shell_single_quote_handles_empty_string() {
        assert_eq!(shell_single_quote(""), "''");
    }

    #[test]
    fn default_session_name_uses_cwd_basename_when_possible() {
        // We can't `cd` reliably without affecting other tests, so
        // just verify the function returns a non-empty string. The
        // fallback "tear" is also acceptable.
        let name = default_session_name_for_cwd();
        assert!(!name.is_empty());
    }

    // ── parse_pane_id / parse_session_id helpers ─────────────────

    #[test]
    fn parse_pane_id_accepts_canonical_hex() {
        // `tear list` prints 16-hex pane ids; canonical round-trip
        // must succeed.
        let p = tear_types::PaneId(0x1234_5678_9abc_def0);
        let s = p.to_string();
        let back = parse_pane_id(&s, "pane id").unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn parse_pane_id_surfaces_label_on_invalid_input() {
        let err = parse_pane_id("not-hex", "--pane").unwrap_err().to_string();
        assert!(err.contains("--pane"), "msg: {err}");
        assert!(err.contains("not-hex"), "msg: {err}");
    }

    #[test]
    fn parse_session_id_accepts_canonical_hex_and_rejects_garbage() {
        let s = tear_types::SessionId(0xdead_beef_cafe_babe);
        assert_eq!(parse_session_id(&s.to_string(), "x").unwrap(), s);
        assert!(parse_session_id("zz-not-a-hex-id", "--session").is_err());
    }
}
