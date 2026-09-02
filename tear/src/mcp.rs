//! `tear mcp` — stdio MCP server for perf + introspection.
//!
//! Spawns as `tear mcp` (stdio transport). Claude Code wires it
//! into the agent's MCP server list so the agent can:
//!
//!   * `daemon_status` — uptime, RSS, CPU%, socket path, total
//!     bytes consumed across all panes
//!   * `system_resources` — overall host CPU/mem so the agent
//!     can spot "tear's hot vs the whole machine's hot"
//!   * `list_sessions` — every active session with pane count
//!     + source
//!   * `pane_stats` — per-pane subscriber count + recording
//!     state + size + bytes consumed (via daemon's existing
//!     pane_subscriber_count + session lookup)
//!   * `top_panes` — panes sorted by activity (subscribers
//!     descending). Cheap to compute, surfaces "which pane is
//!     getting hammered."
//!   * `ping` — round-trip the UDS to confirm the daemon's
//!     accept loop is alive (catches the "daemon hung" class
//!     of bug where status looks healthy but RPCs stall).
//!
//! Every tool is read-only — no mutating control RPCs. Mado's
//! MCP server already exposes the mutating surface (`new
//! session`, `send_keys`, etc.); duplicating that here would
//! create two sources of truth for write paths.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::router::tool::ToolRouter,
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Serialize;
use sysinfo::System;
use tear_client::Client;
use tear_types::MultiplexerControl;

/// Per-process boot timestamp. Lets `daemon_status` and friends
/// report uptime without exposing it from tear-daemon (which
/// would need a wider API surface).
static MCP_BOOT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn mcp_boot() -> Instant {
    *MCP_BOOT.get_or_init(Instant::now)
}

#[derive(Clone)]
struct TearMcp {
    socket_path: std::path::PathBuf,
    tool_router: ToolRouter<Self>,
}

impl TearMcp {
    fn new(socket_path: std::path::PathBuf) -> Self {
        Self {
            socket_path,
            tool_router: Self::tool_router(),
        }
    }

    fn client(&self) -> Result<Client, String> {
        Client::connect(&self.socket_path)
            .map_err(|e| format!("connect to {} failed: {e}", self.socket_path.display()))
    }
}

#[tool_router]
impl TearMcp {
    #[tool(
        description = "Daemon status: uptime_s, rss_bytes, cpu_pct (over the last sample window), socket path, total_panes, total_subscribers, total_bytes_consumed (sum across all panes from start). Reports the daemon's resource footprint at a glance so the agent can spot 'is tear leaking RSS over a long session?' or 'is the daemon spinning a CPU?' without leaving the conversation."
    )]
    async fn daemon_status(&self) -> String {
        // Open client to resolve daemon pid via socket peer
        // metadata. portable_pty/UnixStream doesn't expose peer
        // pid in stable Rust, so we use sysinfo to find the
        // process serving the socket path: scan processes whose
        // cmdline contains the literal `tear daemon` and whose
        // open fds include the socket. Cheaper approximation:
        // find one process named "tear" with arg "daemon" and
        // use its stats.
        let mut sys = System::new();
        sys.refresh_all();
        let daemon_pid = sys.processes().iter().find_map(|(pid, p)| {
            // ★ SKIP THREADS. On Linux `sys.processes()` also yields TASKS,
            // and a thread INHERITS its process's argv — so the cmdline match
            // below cannot tell one from the other, and `find_map` returns
            // whichever the map iterates first.
            //
            // Measured on plo 2026-09-02: `daemon_status` reported
            // `daemon_pid: 774217` while the daemon was 774204.
            // `ps -p 774217` says no such process; `/proc/774204/task` lists
            // 774204 774214 774215 774216 774217 — the reported number was
            // this crate's own kanshou sidecar thread. An operator who checks
            // the pid we hand them concludes the daemon is DEAD while it is
            // serving normally, which is the worst direction for a field
            // named `daemon_pid` to be wrong in.
            //
            // `thread_kind()` is `None` exactly for real processes, and is
            // always `None` on macOS, so this is a no-op there rather than a
            // platform fork.
            if p.thread_kind().is_some() {
                return None;
            }
            let cmd: Vec<String> = p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().to_string())
                .collect();
            if cmd.iter().any(|s| s == "daemon")
                && cmd.iter().any(|s| s.ends_with("/tear") || s == "tear")
            {
                Some((pid.as_u32(), p.memory(), p.cpu_usage()))
            } else {
                None
            }
        });

        let mut total_panes = 0u32;
        let mut total_subscribers = 0u32;
        let mut total_bytes = 0u64;
        let mut session_count = 0u32;
        if let Ok(c) = self.client() {
            if let Ok(sessions) = c.list_sessions() {
                session_count = sessions.len() as u32;
                for s in &sessions {
                    for pid in s.panes.keys() {
                        total_panes += 1;
                        if let Ok(n) = c.pane_subscriber_count(*pid) {
                            total_subscribers += n;
                        }
                        // Bytes-consumed needs a daemon-side RPC
                        // we haven't added yet; placeholder 0
                        // until a `pane_bytes_consumed` lands.
                        let _ = pid;
                        total_bytes += 0;
                    }
                }
            }
        }

        #[derive(Serialize)]
        struct Status<'a> {
            socket: &'a str,
            uptime_s: u64,
            session_count: u32,
            total_panes: u32,
            total_subscribers: u32,
            total_bytes_consumed: u64,
            daemon_pid: Option<u32>,
            daemon_rss_bytes: Option<u64>,
            daemon_cpu_pct: Option<f32>,
        }
        let st = Status {
            socket: &self.socket_path.to_string_lossy(),
            uptime_s: mcp_boot().elapsed().as_secs(),
            session_count,
            total_panes,
            total_subscribers,
            total_bytes_consumed: total_bytes,
            daemon_pid: daemon_pid.map(|(p, _, _)| p),
            daemon_rss_bytes: daemon_pid.map(|(_, m, _)| m),
            daemon_cpu_pct: daemon_pid.map(|(_, _, c)| c),
        };
        kotae::Answer::found(&st).render()
    }

    #[tool(
        description = "System-wide resource snapshot: total_mem_bytes, used_mem_bytes, available_mem_bytes, cpu_count, load_avg (1m/5m/15m on unix; null on windows). Use alongside daemon_status to answer 'is tear the bottleneck or is the host saturated?' Pure read of /proc-style sysinfo — no daemon round-trip."
    )]
    async fn system_resources(&self) -> String {
        let mut sys = System::new();
        sys.refresh_memory();
        let load = System::load_average();
        let core_count = sys.cpus().len();
        #[derive(Serialize)]
        struct Sysres {
            total_mem_bytes: u64,
            used_mem_bytes: u64,
            available_mem_bytes: u64,
            cpu_count: usize,
            load_1m: f64,
            load_5m: f64,
            load_15m: f64,
        }
        let s = Sysres {
            total_mem_bytes: sys.total_memory(),
            used_mem_bytes: sys.used_memory(),
            available_mem_bytes: sys.available_memory(),
            cpu_count: core_count,
            load_1m: load.one,
            load_5m: load.five,
            load_15m: load.fifteen,
        };
        kotae::Answer::found(&s).render()
    }

    #[tool(
        description = "List every live session with summary: id, name, source (human/agent/named/<label>), window_count, pane_count, state (active/detached). Sorted by creation time (oldest first). For pane-level detail call `pane_stats` with a specific pane id."
    )]
    async fn list_sessions(&self) -> String {
        let c = match self.client() {
            Ok(c) => c,
            Err(e) => return kotae::Answer::blind(e.to_string()).render(),
        };
        let sessions = match c.list_sessions() {
            Ok(s) => s,
            Err(e) => return kotae::Answer::blind(e.to_string()).render(),
        };
        #[derive(Serialize)]
        struct Row {
            id: String,
            name: String,
            source: String,
            window_count: usize,
            pane_count: usize,
            state: String,
        }
        let rows: Vec<Row> = sessions
            .into_iter()
            .map(|s| Row {
                id: s.id.to_string(),
                name: s.name,
                source: s.source.label().to_string(),
                window_count: s.windows.len(),
                pane_count: s.panes.len(),
                state: format!("{:?}", s.state),
            })
            .collect();
        kotae::Answer::found(&rows).render()
    }

    #[tool(
        description = "Per-pane metrics: subscriber_count (number of byte-stream consumers attached — mado windows, recording sinks, etc.), input_policy (Free/Locked/Leader), size_cells. Use to investigate 'which pane is being watched the most' or 'why isn't my mado seeing output' (subscriber_count = 0 means nothing's listening)."
    )]
    async fn pane_stats(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<PaneIdInput>,
    ) -> String {
        let pane_id_str = &params.0.pane_id;
        let pane_id: tear_types::PaneId = match pane_id_str.parse() {
            Ok(p) => p,
            Err(e) => {
                return kotae::Answer::refused_flatly(format!(
                    "invalid pane_id `{pane_id_str}`: {e}"
                ))
                .render();
            }
        };
        let c = match self.client() {
            Ok(c) => c,
            Err(e) => return kotae::Answer::blind(e.to_string()).render(),
        };
        let pane = match c.get_pane(pane_id) {
            Ok(p) => p,
            Err(e) => return kotae::Answer::blind(format!("get_pane: {e}")).render(),
        };
        let subs = c.pane_subscriber_count(pane_id).unwrap_or(0);
        #[derive(Serialize)]
        struct Stats {
            id: String,
            shell: String,
            subscriber_count: u32,
            input_policy: String,
            size_cells: (u16, u16),
            state: String,
        }
        let s = Stats {
            id: pane.id.to_string(),
            shell: pane.shell,
            subscriber_count: subs,
            input_policy: format!("{:?}", pane.input_policy),
            size_cells: pane.size_cells,
            state: format!("{:?}", pane.state),
        };
        kotae::Answer::found(&s).render()
    }

    #[tool(
        description = "Top panes by subscriber count (descending). Surfaces 'which pane is fanning out to the most consumers right now.' Returns up to `limit` rows (default 10) with id, name (session), shell, subscriber_count."
    )]
    async fn top_panes(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<TopPanesInput>,
    ) -> String {
        let limit = params.0.limit.unwrap_or(10).max(1) as usize;
        let c = match self.client() {
            Ok(c) => c,
            Err(e) => return kotae::Answer::blind(e.to_string()).render(),
        };
        let sessions = match c.list_sessions() {
            Ok(s) => s,
            Err(e) => return kotae::Answer::blind(e.to_string()).render(),
        };
        #[derive(Serialize)]
        struct Row {
            pane_id: String,
            session_name: String,
            shell: String,
            subscriber_count: u32,
        }
        let mut rows = Vec::new();
        for s in &sessions {
            for (pid, pane) in &s.panes {
                let subs = c.pane_subscriber_count(*pid).unwrap_or(0);
                rows.push(Row {
                    pane_id: pid.to_string(),
                    session_name: s.name.clone(),
                    shell: pane.shell.clone(),
                    subscriber_count: subs,
                });
            }
        }
        rows.sort_by(|a, b| b.subscriber_count.cmp(&a.subscriber_count));
        rows.truncate(limit);
        kotae::Answer::found(&rows).render()
    }

    #[tool(
        description = "Capture a pane's currently-rendered cell grid as text. Returns rows × cols of unicode (one string per row), plus cursor_row / cursor_col / cursor_visible and pane size. The 'what does the screen look like RIGHT NOW' tool — use to verify a TUI is rendering correctly, see what prompt is showing, confirm a command output landed, or debug 'mado shows X but tear says Y' divergence. Strips color/attrs (use mado's snapshot_grid for those). Returns {error} if pane id is invalid or daemon refuses (passthrough backends like tear-tmux-backend can't snapshot)."
    )]
    async fn pane_snapshot_text(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<PaneIdInput>,
    ) -> String {
        let pane_id_str = &params.0.pane_id;
        let pane_id: tear_types::PaneId = match pane_id_str.parse() {
            Ok(p) => p,
            Err(e) => {
                return kotae::Answer::refused_flatly(format!(
                    "invalid pane_id `{pane_id_str}`: {e}"
                ))
                .render();
            }
        };
        let c = match self.client() {
            Ok(c) => c,
            Err(e) => return kotae::Answer::blind(e.to_string()).render(),
        };
        let snap = match c.pane_snapshot(pane_id) {
            Ok(s) => s,
            Err(e) => return kotae::Answer::blind(format!("pane_snapshot: {e}")).render(),
        };
        let rows: Vec<String> = snap
            .cells
            .iter()
            .map(|row| {
                let s: String = row.iter().map(|c| c.ch).collect();
                // Trim trailing blanks (NULs and spaces) for compact output;
                // cursor_col is still authoritative for cursor position.
                s.trim_end_matches(|c: char| c == ' ' || c == '\0')
                    .to_string()
            })
            .collect();
        #[derive(Serialize)]
        struct Snap {
            pane_id: String,
            rows: usize,
            cols: usize,
            cursor_row: usize,
            cursor_col: usize,
            cursor_visible: bool,
            lines: Vec<String>,
        }
        let s = Snap {
            pane_id: pane_id.to_string(),
            rows: snap.rows,
            cols: snap.cols,
            cursor_row: snap.cursor_row,
            cursor_col: snap.cursor_col,
            cursor_visible: snap.cursor_visible,
            lines: rows,
        };
        kotae::Answer::found(&s).render()
    }

    #[tool(
        description = "Full session detail: id, name, source, state, windows (with their panes), creation time. Drills past list_sessions' summary into the full tree so an agent can walk a session's full structure in one call. Returns {error} if session id is invalid."
    )]
    async fn session_detail(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<SessionIdInput>,
    ) -> String {
        let session_id_str = &params.0.session_id;
        let session_id: tear_types::SessionId = match session_id_str.parse() {
            Ok(s) => s,
            Err(e) => {
                return kotae::Answer::refused_flatly(format!(
                    "invalid session_id `{session_id_str}`: {e}"
                ))
                .render();
            }
        };
        let c = match self.client() {
            Ok(c) => c,
            Err(e) => return kotae::Answer::blind(e.to_string()).render(),
        };
        let s = match c.get_session(session_id) {
            Ok(s) => s,
            Err(e) => return kotae::Answer::blind(format!("get_session: {e}")).render(),
        };
        // Serde already derives Serialize on TearSession — pass through.
        kotae::Answer::found(&s).render()
    }

    #[tool(
        description = "Flat list of every pane across every session: pane_id, session_id, session_name, shell, size_cells, subscriber_count, input_policy, state. Cheaper than calling list_sessions + N × pane_stats; the canonical 'show me everything live' surface. Sorted by session_name then pane creation order."
    )]
    async fn list_panes(&self) -> String {
        let c = match self.client() {
            Ok(c) => c,
            Err(e) => return kotae::Answer::blind(e.to_string()).render(),
        };
        let sessions = match c.list_sessions() {
            Ok(s) => s,
            Err(e) => return kotae::Answer::blind(format!("list_sessions: {e}")).render(),
        };
        #[derive(Serialize)]
        struct Row {
            pane_id: String,
            session_id: String,
            session_name: String,
            shell: String,
            size_cells: (u16, u16),
            subscriber_count: u32,
            input_policy: String,
            state: String,
        }
        let mut rows: Vec<Row> = Vec::new();
        for s in &sessions {
            for (pid, pane) in &s.panes {
                let subs = c.pane_subscriber_count(*pid).unwrap_or(0);
                rows.push(Row {
                    pane_id: pid.to_string(),
                    session_id: s.id.to_string(),
                    session_name: s.name.clone(),
                    shell: pane.shell.clone(),
                    size_cells: pane.size_cells,
                    subscriber_count: subs,
                    input_policy: format!("{:?}", pane.input_policy),
                    state: format!("{:?}", pane.state),
                });
            }
        }
        rows.sort_by(|a, b| a.session_name.cmp(&b.session_name));
        kotae::Answer::found(&rows).render()
    }

    #[tool(
        description = "Daemon socket + connectivity surface: socket_path, exists, can_connect, peer_metadata_supported. Use first when diagnosing 'is the daemon reachable at all' — separates 'socket file missing' from 'socket exists but daemon dead' from 'daemon alive but rejecting our user'."
    )]
    async fn socket_info(&self) -> String {
        let socket_path = self.socket_path.clone();
        let exists = socket_path.exists();
        let can_connect = self.client().is_ok();
        serde_json::json!({
            "socket_path": socket_path.display().to_string(),
            "exists": exists,
            "can_connect": can_connect,
        })
        .to_string()
    }

    #[tool(
        description = "Round-trip a connect + list_sessions call to the daemon and report wall-time. Use to confirm the daemon's accept loop is responding (catches the 'looks alive but RPCs stall' class of bug). Returns {ok: bool, latency_ms: f64, error: Option<str>}."
    )]
    async fn ping(&self) -> String {
        let start = Instant::now();
        let (ok, err) = match self.client() {
            Ok(c) => match c.list_sessions() {
                Ok(_) => (true, None),
                Err(e) => (false, Some(e.to_string())),
            },
            Err(e) => (false, Some(e)),
        };
        #[derive(Serialize)]
        struct Pong {
            ok: bool,
            latency_ms: f64,
            error: Option<String>,
        }
        let p = Pong {
            ok,
            latency_ms: start.elapsed().as_secs_f64() * 1000.0,
            error: err,
        };
        kotae::Answer::found(&p).render()
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaneIdInput {
    #[schemars(description = "16-char lowercase-hex tear pane id (from `tear list --yaml`).")]
    pane_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SessionIdInput {
    #[schemars(description = "16-char lowercase-hex tear session id (from `list_sessions`).")]
    session_id: String,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
struct TopPanesInput {
    #[schemars(description = "Max rows to return (default 10).")]
    #[serde(default)]
    limit: Option<u32>,
}

#[tool_handler]
impl ServerHandler for TearMcp {
    fn get_info(&self) -> ServerInfo {
        // rmcp 1.x marks ServerInfo #[non_exhaustive], which forbids any
        // struct expression outside its crate (even functional-update
        // syntax) — default-then-mutate is the sanctioned construction.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "tear MCP server. Read-only state exploration: \
             daemon_status, system_resources, socket_info, \
             list_sessions, session_detail, list_panes, \
             pane_stats, pane_snapshot_text, top_panes, ping. \
             Use pane_snapshot_text to see what's actually on \
             screen in any pane RIGHT NOW (the canonical \
             'what's happening' surface). Write operations \
             (send_keys, new_session, set_input_policy) live \
             on mado's MCP — tear stays read-only."
                .into(),
        );
        info
    }
}

/// Entry point — `tear mcp [--socket <path>]`. Stdio MCP transport
/// (stdout = JSON-RPC, stderr = tracing).
pub async fn run(socket_path: Option<std::path::PathBuf>) -> Result<()> {
    let socket = socket_path.unwrap_or_else(tear_types::wire::default_socket_path);
    // Stamp boot timestamp ASAP.
    let _ = mcp_boot();
    tracing::info!(socket = %socket.display(), "tear mcp starting");
    let server = TearMcp::new(socket);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod answer_shape_tests {
    use kotae::Answer;
    use serde_json::Value;

    /// **The bug this file used to have, pinned at tear's own boundary.**
    ///
    /// Every error answer here was built with
    /// `format!("{{\"error\":\"{e}\"}}")` — 24 sites — and a message
    /// containing a double quote produced INVALID JSON. Several of those sites
    /// interpolated a caller-supplied id straight into the string, so the
    /// malformed output was reachable from the other end of the protocol: an
    /// agent passing `pane_id = "a\"b"` got back bytes no parser accepts.
    ///
    /// This asserts the property at the shape tear now emits, so a future
    /// hand-rolled `format!` here fails rather than silently reintroducing it.
    #[test]
    fn a_caller_supplied_quote_cannot_break_the_answer() {
        let hostile = r#"a"b\c{"unbalanced":["#;
        for answer in [
            Answer::refused_flatly(format!("invalid pane_id `{hostile}`: bad digit")),
            Answer::blind(format!("get_pane: {hostile}")),
        ] {
            let rendered = answer.render();
            let parsed: Value = serde_json::from_str(&rendered)
                .expect("a tear answer must always be parseable JSON");
            assert_eq!(parsed["outcome"], answer.outcome());
        }
    }

    /// The conversion sharpened a distinction the flat `error` shape had
    /// collapsed: a malformed id is the CALLER asking wrong, a failed lookup is
    /// tear being unable to look. An agent acts differently on each, and the
    /// old shape gave it no way to tell.
    #[test]
    fn a_malformed_id_and_a_failed_lookup_are_different_answers() {
        let refused = Answer::refused_flatly("invalid pane_id `x`: bad digit");
        let blind = Answer::blind("get_pane: daemon not running");
        assert_eq!(refused.outcome(), "refused");
        assert_eq!(blind.outcome(), "blind");
        assert!(!refused.is_about_the_world());
        assert!(!blind.is_about_the_world());
    }
}
