//! `tear-config` — shikumi-style live configuration for tear.
//!
//! Mirrors the shape mado uses for `~/.config/mado/mado.yaml`:
//! a typed [`TearConfig`] struct deserialized from YAML, made
//! available behind an [`arc_swap::ArcSwap`] for lock-free reads,
//! refreshed on file-change via `notify`. Operators set up tear
//! and mado the same way; muscle memory carries.
//!
//! ## Layout
//!
//! ```text
//! ~/.config/tear/tear.yaml
//! ─────────────────────────
//! prefix:        "ctrl+b"        # keybinding prefix (legacy tmux compat)
//! default_shell: "/bin/zsh"      # spawned in new sessions/windows/panes
//! mouse:         true            # enable mouse support
//! base_index:    1               # window numbering start (tmux default 0; many prefer 1)
//!
//! keys:
//!   - chord:   "ctrl+a c"
//!     action:  { kind: new-window }
//!     note:    "create window"
//!
//! status:
//!   refresh_interval_seconds: 5
//!   left:
//!     - { kind: text, value: "#[fg=yellow]" }
//!     - { kind: session-name }
//!     - { kind: text, value: " · " }
//!     - { kind: window-name }
//!   right:
//!     - { kind: time, format: "%H:%M" }
//!     - { kind: hostname, short: true }
//!
//! theme:
//!   name: nord
//! ```
//!
//! ## Hot reload
//!
//! [`spawn_watcher`] kicks off a background thread that re-parses on
//! file mutation. Errors during reload are logged + the previous
//! `Arc<TearConfig>` stays in place — operators with a syntactically
//! broken file aren't dropped to defaults mid-session.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::time::Duration;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

use tear_types::{KeyBind, KeyChord, StatusBar, TearTheme};

/// Failure modes for config loading.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(PathBuf),
    #[error("io error reading config: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error("watcher failed: {0}")]
    Watch(#[from] notify::Error),
}

/// The full live tear configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TearConfig {
    /// Legacy tmux-style prefix key. Pressed before bindings in the
    /// `"prefix"` key table. tmux default is `C-b`; many operators
    /// override to `C-a` or `C-Space`. Empty string disables prefix
    /// mode entirely.
    #[serde(default = "default_prefix")]
    pub prefix: String,

    /// Shell program spawned in new sessions / windows / panes when
    /// the operator doesn't override.
    #[serde(default = "default_shell")]
    pub default_shell: String,

    /// Mouse support. tmux's `set -g mouse on`. Default on.
    #[serde(default = "default_mouse")]
    pub mouse: bool,

    /// Window numbering base — `0` mirrors tmux's out-of-box behavior,
    /// `1` is more keyboard-ergonomic on the number row.
    #[serde(default = "default_base_index")]
    pub base_index: u16,

    /// Keybindings. Order matters within a chord prefix — earliest
    /// match wins.
    #[serde(default)]
    pub keys: Vec<KeyBind>,

    /// Status-bar layout.
    #[serde(default)]
    pub status: StatusBar,

    /// Active theme.
    #[serde(default)]
    pub theme: TearTheme,

    /// File watcher debounce. Operators sometimes hit Save twice in
    /// a row; coalescing keeps the reload count modest.
    #[serde(default = "default_debounce")]
    pub reload_debounce_ms: u64,

    /// #48c — directory for auto-flushed recordings. When set, the
    /// daemon writes any active recording on a pane (or session)
    /// to `<dir>/<session_id>-<unix_ts>-<pane_id>.cast` on kill.
    /// `None` disables auto-flush (operator still uses
    /// `tear pane-record export --out PATH` explicitly).
    /// Example: `~/.local/share/tear/recordings`.
    #[serde(default)]
    pub recording_auto_dir: Option<String>,

    /// #4 — `tear ai` LLM proxy config. `None` disables `tear ai`
    /// (operator gets a clean "no provider configured" error).
    /// Defaults work for a stock local Ollama install — no API
    /// key, no network, no telemetry.
    #[serde(default)]
    pub ai: Option<AiConfig>,

    /// #6 — append-only JSONL audit log. When set, the daemon
    /// writes one line per typed event (session_create /
    /// session_kill / set_input_policy / start_recording /
    /// stop_recording / set_config). `None` disables. Path
    /// supports leading `~/`. Example:
    /// `~/.local/share/tear/audit.log`.
    #[serde(default)]
    pub audit_log: Option<String>,

    /// #5 — name of an env var that holds a shared-secret auth
    /// token. When set, the daemon resolves the env var at startup
    /// and requires every client connection to send the matching
    /// `Request::Authenticate(token)` as its first request. Used
    /// for TCP-bound daemons reachable from a network — UDS
    /// daemons can still set this for defence-in-depth, though
    /// filesystem perms are usually enough for local-only sockets.
    ///
    /// Operator workflow:
    /// 1. `openssl rand -hex 32 > ~/.config/tear/auth-token`
    /// 2. `export TEAR_AUTH_TOKEN="$(cat ~/.config/tear/auth-token)"`
    /// 3. Set `auth_token_env: TEAR_AUTH_TOKEN` in tear.yaml
    /// 4. Every client inherits the env var via the shell session.
    #[serde(default)]
    pub auth_token_env: Option<String>,
}

/// `tear ai` provider + model. Lives in `tear-config` so it
/// round-trips through the same shikumi YAML as every other
/// daemon-side knob; an operator flipping models picks up via
/// the next reload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AiConfig {
    /// Provider implementation. Default `ollama`.
    #[serde(default = "default_ai_provider")]
    pub provider: String,
    /// Model name (e.g. `llama3.2`, `qwen2.5-coder:7b`,
    /// `claude-sonnet-4-5`).
    #[serde(default = "default_ai_model")]
    pub model: String,
    /// Full HTTP endpoint. Default points at a stock local
    /// Ollama (`http://127.0.0.1:11434`); override for any
    /// OpenAI-compatible API.
    #[serde(default = "default_ai_endpoint")]
    pub endpoint: String,
    /// Name of an env var that holds the API key. Read at
    /// request time. `None` for providers that need no auth
    /// (Ollama).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Max output bytes from the latest block to feed in as
    /// context. Default 2000 — most LLMs handle the rest of
    /// the context (cwd + cmd + exit) trivially.
    #[serde(default = "default_ai_context_bytes")]
    pub context_bytes: usize,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: default_ai_provider(),
            model: default_ai_model(),
            endpoint: default_ai_endpoint(),
            api_key_env: None,
            context_bytes: default_ai_context_bytes(),
        }
    }
}

fn default_ai_provider() -> String {
    "ollama".into()
}
fn default_ai_model() -> String {
    "llama3.2".into()
}
fn default_ai_endpoint() -> String {
    "http://127.0.0.1:11434".into()
}
fn default_ai_context_bytes() -> usize {
    2000
}

impl Default for TearConfig {
    fn default() -> Self {
        Self {
            prefix: default_prefix(),
            default_shell: default_shell(),
            mouse: default_mouse(),
            base_index: default_base_index(),
            keys: default_keybinds(),
            status: default_status(),
            theme: TearTheme::default(),
            reload_debounce_ms: default_debounce(),
            recording_auto_dir: None,
            ai: None,
            audit_log: None,
            auth_token_env: None,
        }
    }
}

fn default_prefix() -> String {
    "ctrl+b".into()
}
fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}
fn default_mouse() -> bool {
    true
}
fn default_base_index() -> u16 {
    1
}
fn default_debounce() -> u64 {
    250
}

/// A sane minimum keybind set for first-run operators.
fn default_keybinds() -> Vec<KeyBind> {
    use tear_types::{Action, Direction, KeyTableName};

    vec![
        KeyBind {
            chord: KeyChord::from_tmux("C-b c"),
            action: Action::NewWindow,
            note: "new window".into(),
        },
        KeyBind {
            chord: KeyChord::from_tmux("C-b n"),
            action: Action::NextWindow,
            note: "next window".into(),
        },
        KeyBind {
            chord: KeyChord::from_tmux("C-b p"),
            action: Action::PreviousWindow,
            note: "previous window".into(),
        },
        KeyBind {
            chord: KeyChord::from_tmux("C-b %"),
            action: Action::SplitPane {
                direction: Direction::Right,
            },
            note: "split right".into(),
        },
        KeyBind {
            chord: KeyChord::from_tmux("C-b \""),
            action: Action::SplitPane {
                direction: Direction::Below,
            },
            note: "split below".into(),
        },
        KeyBind {
            chord: KeyChord::from_tmux("C-b d"),
            action: Action::Detach,
            note: "detach client".into(),
        },
        KeyBind {
            chord: KeyChord::from_tmux("C-b R"),
            action: Action::ReloadConfig,
            note: "reload tear-config".into(),
        },
        KeyBind {
            chord: KeyChord::from_tmux("C-b :"),
            action: Action::EnterTable {
                table: KeyTableName("command".into()),
            },
            note: "open command prompt".into(),
        },
    ]
}

/// A sensible default status bar — session/window names left, clock
/// + host right, refresh every 5 s.
fn default_status() -> StatusBar {
    use tear_types::Segment;
    StatusBar {
        left: vec![
            Segment::Text {
                value: "[".into(),
            },
            Segment::SessionName,
            Segment::Text {
                value: ":".into(),
            },
            Segment::WindowName,
            Segment::Text {
                value: "] ".into(),
            },
        ],
        center: vec![],
        right: vec![
            Segment::PaneCommand,
            Segment::Text {
                value: " · ".into(),
            },
            Segment::Time {
                format: "%H:%M".into(),
            },
            Segment::Text {
                value: " · ".into(),
            },
            Segment::Hostname { short: true },
        ],
        refresh_interval_seconds: 5,
        visible: true,
    }
}

/// Resolve the operator's tear config path. Honours `$XDG_CONFIG_HOME`
/// and `$TEAR_CONFIG_FILE`; falls back to `~/.config/tear/tear.yaml`.
#[must_use]
pub fn default_config_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("TEAR_CONFIG_FILE") {
        return PathBuf::from(explicit);
    }
    let xdg = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME").ok().map(|h| {
                let mut p = PathBuf::from(h);
                p.push(".config");
                p
            })
        })
        .unwrap_or_else(|| PathBuf::from("."));
    xdg.join("tear").join("tear.yaml")
}

/// Read + parse the config file at `path`. Returns the parsed
/// [`TearConfig`] on success, or an error variant per
/// [`ConfigError`].
pub fn load_from(path: &Path) -> Result<TearConfig, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::NotFound(path.to_path_buf()));
    }
    let text = std::fs::read_to_string(path)?;
    let cfg: TearConfig = serde_yaml_ng::from_str(&text)?;
    Ok(cfg)
}

/// Load the config from the canonical path. If the file is missing,
/// return the default config so first-run users don't have to author
/// YAML before tear starts.
pub fn load_or_default() -> Arc<TearConfig> {
    let path = default_config_path();
    match load_from(&path) {
        Ok(cfg) => Arc::new(cfg),
        Err(ConfigError::NotFound(_)) => {
            info!(?path, "no tear config found — using defaults");
            Arc::new(TearConfig::default())
        }
        Err(e) => {
            warn!(error = %e, "tear config parse failed — falling back to defaults");
            Arc::new(TearConfig::default())
        }
    }
}

/// Lock-free, hot-reloadable handle to the live config. Cheap clone
/// (Arc bump). Readers call [`Self::load`] to get an `Arc<TearConfig>`
/// they can hold across a frame.
///
/// Also supports change-broadcast subscriptions —
/// [`Self::subscribe`] returns a receiver that gets one frame on
/// every `replace()` (which includes notify-driven reloads + manual
/// `SetConfig` RPCs + explicit `reload()`s). The pleme-io fleet uses
/// this to push theme/keybind changes to every attached mado at the
/// same moment, broadcast-style.
#[derive(Clone)]
pub struct LiveConfig {
    inner: Arc<ArcSwap<TearConfig>>,
    /// Per-subscriber senders. Cloning a LiveConfig clones the Arc
    /// (so daemon + watcher + RPC handlers see the same subscriber
    /// list). Mutex<Vec<Sender>> is enough — fan-out is rare (only
    /// on config replace) and the lock is held only for the
    /// fan-out loop.
    subscribers: Arc<Mutex<Vec<mpsc::Sender<Arc<TearConfig>>>>>,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from(load_or_default())),
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl LiveConfig {
    /// Get the current config — Arc clone, no lock.
    #[must_use]
    pub fn load(&self) -> Arc<TearConfig> {
        self.inner.load_full()
    }

    /// Register a config-change subscriber. Returns the receiver
    /// end of an mpsc channel; every successful `replace()` (and
    /// every successful `reload()` / `SetConfig` RPC) sends one
    /// frame on the corresponding sender. Drop the receiver to
    /// unsubscribe — the next broadcast prunes the dead sender.
    pub fn subscribe(&self) -> mpsc::Receiver<Arc<TearConfig>> {
        let (tx, rx) = mpsc::channel();
        self.subscribers.lock().expect("subscribers poisoned").push(tx);
        rx
    }

    /// Replace the current config atomically. Logs the swap and
    /// fans out to every change subscriber. Dead senders are
    /// pruned in place (same shape as InProcess pane-byte
    /// broadcast).
    pub fn replace(&self, cfg: TearConfig) {
        info!("tear-config: applying new config");
        let new_arc = Arc::new(cfg);
        self.inner.store(new_arc.clone());
        let mut subs = self.subscribers.lock().expect("subscribers poisoned");
        let mut i = 0;
        while i < subs.len() {
            if subs[i].send(new_arc.clone()).is_err() {
                subs.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// Reload from the canonical path. Logs success/failure; on
    /// failure the previous config remains in place.
    pub fn reload(&self) -> Result<(), ConfigError> {
        let path = default_config_path();
        let cfg = load_from(&path)?;
        self.replace(cfg);
        Ok(())
    }

    /// Spawn a background watcher that reloads on file change.
    /// Returns the watcher handle — drop it to stop watching.
    pub fn spawn_watcher(&self) -> Result<notify::RecommendedWatcher, ConfigError> {
        use notify::{EventKind, RecursiveMode, Watcher};

        let path = default_config_path();
        let parent = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let live = self.clone();
        let debounce_ms = self.load().reload_debounce_ms;
        let last_reload = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else {
                return;
            };
            // Only react to writes / creates — ignore access events.
            if !matches!(
                ev.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            ) {
                return;
            }
            // Debounce — coalesce bursts (operator saving in vim
            // commonly emits 3-4 events back to back).
            {
                let mut last = last_reload.lock().unwrap();
                if last.elapsed() < Duration::from_millis(debounce_ms) {
                    return;
                }
                *last = std::time::Instant::now();
            }
            if let Err(e) = live.reload() {
                warn!(error = %e, "tear-config reload failed; keeping previous config");
            }
        })?;
        watcher.watch(&parent, RecursiveMode::NonRecursive)?;
        info!(?parent, "tear-config: watching for changes");
        Ok(watcher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_constructible() {
        let cfg = TearConfig::default();
        assert_eq!(cfg.prefix, "ctrl+b");
        assert!(!cfg.keys.is_empty());
        assert!(cfg.status.visible);
    }

    #[test]
    fn default_keybinds_include_split_and_reload() {
        use tear_types::Action;
        let cfg = TearConfig::default();
        assert!(
            cfg.keys.iter().any(|k| matches!(k.action, Action::SplitPane { .. })),
            "default keys should include a split-pane binding"
        );
        assert!(
            cfg.keys.iter().any(|k| matches!(k.action, Action::ReloadConfig)),
            "default keys should include a reload-config binding"
        );
    }

    #[test]
    fn live_config_swap_is_atomic() {
        let live = LiveConfig::default();
        let a = live.load();
        let mut b = (*a).clone();
        b.prefix = "ctrl+space".into();
        live.replace(b.clone());
        let after = live.load();
        assert_eq!(after.prefix, "ctrl+space");
    }
}
