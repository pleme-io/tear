//! `TearDaemonState` — the aggregator the kanshou server exposes.
//!
//! Walks `InProcess::with_registry` for live session + pane data and
//! pulls Subscribe/RPC counters from the daemon-side handle. The
//! tear MCP server (which currently reaches into the daemon via the
//! UDS RPC) can forward introspection queries through this socket
//! and get the registry-walked truth without an extra RPC roundtrip.

use std::sync::Arc;

use kanshou::{Introspect, Query, QueryError, QueryResult};
use tear_core::InProcess;

/// Live aggregator. Hand-implements [`Introspect`] because the leaves
/// are a method call into `InProcess` rather than per-struct fields.
///
/// Add new queryable surface by extending the match arms below + the
/// `schema` array.
pub struct TearDaemonState {
    pub inproc: Arc<InProcess>,
    pub started_at_unix_ms: u64,
}

impl TearDaemonState {
    #[must_use]
    pub fn new(inproc: Arc<InProcess>) -> Self {
        Self {
            inproc,
            started_at_unix_ms: now_unix_ms(),
        }
    }
}

impl Introspect for TearDaemonState {
    fn query(&self, q: &Query) -> QueryResult {
        let Some(first) = q.path.first().map(String::as_str) else {
            return Err(QueryError::unknown_field(String::new()));
        };
        match first {
            "sessions" => {
                let summaries: Vec<serde_json::Value> =
                    self.inproc.with_registry(|reg| {
                        reg.sessions
                            .iter()
                            .map(|(sid, session)| {
                                let pane_count: usize = session
                                    .windows
                                    .values()
                                    .map(|w| w.layout.pane_count())
                                    .sum();
                                let window_count = session.windows.len();
                                serde_json::json!({
                                    "id": sid.to_string(),
                                    "name": &session.name,
                                    "window_count": window_count,
                                    "pane_count": pane_count,
                                })
                            })
                            .collect()
                    });
                Ok(serde_json::json!({
                    "count": summaries.len(),
                    "sessions": summaries,
                }))
            }
            "panes" => {
                let total = self.inproc.with_registry(|reg| {
                    reg.sessions
                        .values()
                        .flat_map(|s| s.windows.values())
                        .map(|w| w.layout.pane_count())
                        .sum::<usize>()
                });
                Ok(serde_json::json!({ "total": total }))
            }
            "socket" => Ok(serde_json::json!({
                "path": self.inproc.socket_path().map(|p| p.display().to_string()),
            })),
            "process" => Ok(serde_json::json!({
                "pid": std::process::id(),
                "binary": std::env::current_exe()
                    .ok()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                "started_at_unix_ms": self.started_at_unix_ms,
                "uptime_ms": now_unix_ms().saturating_sub(self.started_at_unix_ms),
                "version": env!("CARGO_PKG_VERSION"),
            })),
            other => Err(QueryError::unknown_field(other.to_string())),
        }
    }

    fn schema(&self) -> &'static [&'static str] {
        &["sessions", "panes", "socket", "process"]
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Spawn the kanshou server in a tokio task. Returns the path the
/// server bound to. The task is detached; on drop the socket file
/// is unlinked. `app_name` is the canonical wire identifier — pass
/// `"tear-daemon"` for the long-running daemon process.
pub fn spawn_server(
    app_name: &str,
    state: Arc<TearDaemonState>,
) -> std::io::Result<std::path::PathBuf> {
    let server = kanshou::Server::new(app_name, state)?;
    let socket_path = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            tracing::warn!(error = ?e, "tear-daemon kanshou server exited with error");
        }
    });
    Ok(socket_path)
}
