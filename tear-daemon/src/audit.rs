//! #6 — append-only JSONL audit log.
//!
//! Operators (and security-minded sysadmins) need to know what
//! the daemon did on their behalf, especially when AI agents
//! drive sessions via mado MCP. The audit log is one line of
//! JSON per typed event, written via `O_APPEND` so concurrent
//! daemon threads don't interleave records.
//!
//! Wire shape per line:
//! ```text
//! {"ts_ms":1779093000123,"kind":"session_create",
//!  "sid":"a1b2c3d4e5f60718","name":"demo","source":"agent"}
//! ```
//!
//! `kind` enumerates:
//!   - session_create
//!   - session_kill
//!   - set_input_policy
//!   - start_recording
//!   - stop_recording
//!   - set_config
//!
//! Reader: `tear audit --since 1h [--kind <k>]` parses + filters
//! the file. See tear/src/main.rs::cmd_audit.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::warn;

#[derive(Clone)]
pub struct AuditLog {
    inner: Arc<AuditInner>,
}

struct AuditInner {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl AuditLog {
    /// Open an audit log at `path` for append. Creates parent
    /// dirs + the file if missing. `~/` is expanded.
    pub fn open(path_str: &str) -> std::io::Result<Self> {
        let expanded = tear_types::path::expand_tilde(path_str);
        let path = PathBuf::from(&expanded);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            inner: Arc::new(AuditInner {
                path,
                file: Mutex::new(file),
            }),
        })
    }

    /// Append one event. Errors are logged at warn level — the
    /// audit log is best-effort, never blocks daemon ops.
    pub fn emit<E: Serialize>(&self, event: &E) {
        let mut payload = match serde_json::to_string(event) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "audit: serialise failed; dropping event");
                return;
            }
        };
        payload.push('\n');
        let mut f = self.inner.file.lock().expect("audit file lock poisoned");
        if let Err(e) = f.write_all(payload.as_bytes()) {
            warn!(error = %e, path = %self.inner.path.display(), "audit: write failed");
        }
    }
}

/// Typed event payload that gets serialised to JSON per line.
/// Internally-tagged enum (`kind` discriminator) so consumers
/// can `jq 'select(.kind=="session_kill")'` cleanly.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditEvent {
    SessionCreate {
        ts_ms: u64,
        sid: String,
        name: String,
        shell: String,
        source: String,
    },
    SessionKill {
        ts_ms: u64,
        sid: String,
    },
    SetInputPolicy {
        ts_ms: u64,
        pid: String,
        policy: String,
    },
    StartRecording {
        ts_ms: u64,
        pid: String,
    },
    StopRecording {
        ts_ms: u64,
        pid: String,
    },
    SetConfig {
        ts_ms: u64,
        /// SHA-256 of the YAML payload — useful to grep diffs
        /// without storing the whole config in every audit row.
        config_hash: String,
    },
}

impl AuditEvent {
    /// Get the current `ts_ms` — convenience for call sites that
    /// just want "now".
    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_event_serialises_with_kind_tag() {
        let ev = AuditEvent::SessionKill {
            ts_ms: 1_700_000_000_000,
            sid: "abcd1234".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"kind\":\"session_kill\""), "got: {s}");
        assert!(s.contains("\"sid\":\"abcd1234\""), "got: {s}");
    }

    #[test]
    fn open_creates_parent_dirs_and_appends() {
        let mut dir = std::env::temp_dir();
        let pid = std::process::id();
        dir.push(format!("tear-audit-test-{pid}"));
        let path = dir.join("nested").join("audit.log");
        let log = AuditLog::open(path.to_str().unwrap()).expect("open");

        log.emit(&AuditEvent::SessionCreate {
            ts_ms: 1,
            sid: "s1".into(),
            name: "n1".into(),
            shell: "/bin/sh".into(),
            source: "human".into(),
        });
        log.emit(&AuditEvent::SessionKill {
            ts_ms: 2,
            sid: "s1".into(),
        });
        // Force the file's mutex to drop so we can read.
        drop(log);

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("session_create"));
        assert!(lines[1].contains("session_kill"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
