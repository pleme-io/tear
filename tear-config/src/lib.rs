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
    /// M1: shikumi's load/reload errors surface through this
    /// variant so the LiveConfig public API (which returns
    /// Result<(), ConfigError>) stays stable for callers.
    #[error("shikumi error: {0}")]
    Shikumi(#[from] shikumi::ShikumiError),
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

    /// Per-pane scrollback configuration — the full operator-tunable
    /// surface for "how much history do I keep, and across which
    /// events." Defaults to effectively-unlimited scrollback (1M
    /// rows) that survives clear-screen, doesn't accumulate during
    /// alt-screen sessions (vim/htop), and uses no byte cap. See
    /// [`ScrollbackConfig`] for the per-knob detail.
    ///
    /// Mado pushes its own preferred value via SetConfig at attach
    /// time so tear sessions spawned/attached via mado inherit the
    /// "huge scrollback" default automatically. See
    /// `theory/CONFIGURATION-MANAGEMENT.md` § III for the
    /// cross-tool composition pattern.
    #[serde(default)]
    pub scrollback: ScrollbackConfig,
}

/// Per-pane scrollback policy. Every knob is operator-tunable;
/// every default targets "the operator never has to think about
/// it on a modern machine."
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollbackConfig {
    /// Maximum number of off-screen rows retained per pane.
    ///
    /// **Default: `usize::MAX` — unlimited.** The operator-facing
    /// contract is "never lose anything." Rows accumulate in a
    /// `VecDeque` that grows on demand; the only ceiling is host
    /// RAM. On a 64 GB Mac with typical 100-col cells (≈ 50
    /// bytes/cell × 100 cols = 5 KB/row), that's ~12 million
    /// rows before the OS even notices.
    ///
    /// `0` disables scrollback entirely (top rows are dropped on
    /// scroll — matches the xterm `-sl 0` flag). Operators on
    /// low-RAM systems or with daemon panes that emit billions
    /// of lines override to a finite number (10_000–50_000 is
    /// typical for the bounded case).
    #[serde(default = "default_scrollback_rows")]
    pub rows: usize,

    /// Optional hard byte cap. When set, the pane evicts the
    /// oldest rows once `bytes_estimate > max_bytes` regardless
    /// of `rows`. Default `None` — operators rely on the row cap
    /// alone. Useful for binary-log streams (Kitty image escape
    /// payloads, hex dumps) where one "row" can blow past the
    /// expected ~50-byte average.
    ///
    /// Estimate is `cells × ~50 bytes` per row; not exact —
    /// designed as a generous ceiling, not a precise limit.
    #[serde(default)]
    pub max_bytes: Option<usize>,

    /// Keep scrollback rows through a full-screen clear
    /// (`\x1b[2J`, `\x1b[3J`, `Ctrl-L`). **Default: true** — the
    /// near-universal operator expectation; `clear` is for
    /// "shove current view down out of sight", not "erase my
    /// history." Set `false` to mimic naive xterm behavior.
    #[serde(default = "default_keep_on_clear")]
    pub keep_on_clear: bool,

    /// Push rows to scrollback while the alt-screen buffer
    /// (`\x1b[?1049h`, used by vim/htop/less/btop) is active.
    /// **Default: false** — matches xterm's tradition: alt-screen
    /// is a separate scratch surface and its scrollback would
    /// pollute the primary scrollback that operators actually
    /// want to scroll through. Set `true` for "remember
    /// everything ever displayed in this pane."
    #[serde(default)]
    pub on_alt_screen: bool,

    /// Drop rows that contain only blank cells (no printable
    /// content) before pushing to scrollback. **Default: false** —
    /// preserves exact terminal history (blank rows between
    /// `printf` outputs, padding from `cat /dev/zero | head`).
    /// Set `true` to compact scrollback on chatty sessions.
    #[serde(default)]
    pub skip_blank_rows: bool,

    /// On grid resize (e.g., font-size change, window resize),
    /// reflow scrollback rows to the new column width. **Default:
    /// true** — operators expect long lines to re-wrap after
    /// resize. Set `false` for "scrollback is immutable history"
    /// behavior (matches kitty's default; alacritty also defaults
    /// to true).
    #[serde(default = "default_reflow_on_resize")]
    pub reflow_on_resize: bool,
}

impl Default for ScrollbackConfig {
    fn default() -> Self {
        Self {
            rows: default_scrollback_rows(),
            max_bytes: None,
            keep_on_clear: default_keep_on_clear(),
            on_alt_screen: false,
            skip_blank_rows: false,
            reflow_on_resize: default_reflow_on_resize(),
        }
    }
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
            scrollback: ScrollbackConfig::default(),
        }
    }
}

fn default_prefix() -> String {
    // Source the multiplexer prefix from the fleet atlas rather than
    // hard-coding "ctrl+b" here. The atlas declares it as "C-b"
    // (tmux-shorthand canonical); tear-types' KeyChord::from_tmux
    // normalizes to the long form ("ctrl+b") that the rest of tear's
    // chord-dispatch path expects. Atlas drift → tear default prefix
    // changes fleet-wide on next compile, no hand-edit here required.
    //
    // Operators who want a different prefix continue to override via
    // ~/.config/tear/tear.yaml's `prefix:` field — the atlas is the
    // prescribed-default tier, not a hard-cap.
    tear_types::KeyChord::from_tmux(ishou_tokens::FleetKeybinds::prescribed().multiplexer_prefix).0
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
fn default_scrollback_rows() -> usize {
    // "Never lose anything" — the cap is the host's RAM, not a
    // hard-coded row count. Operators opt back in to bounded
    // retention by setting a finite number; the runtime treats
    // usize::MAX as the "do not enforce a row cap" sentinel.
    usize::MAX
}
fn default_keep_on_clear() -> bool {
    // Operators expect Ctrl-L to push current view down, not to
    // erase history.
    true
}
fn default_reflow_on_resize() -> bool {
    // Alacritty + kitty default; the post-resize "wait, where did
    // my long line go" surprise comes from `false`.
    true
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
    use tear_types::{Segment, SignalRenderMode, TearSignalKind};
    StatusBar {
        left: vec![
            // The active-session 🌊 tide mark — sourced from the fleet
            // atlas (`FleetSignals::session_active` via `TearSignals`),
            // matching the mado/tear/praça 🌊 convention drift-free. The
            // status bar is only ever drawn for an attached session, so
            // the mark is unconditionally the active glyph here.
            Segment::Signal {
                signal: TearSignalKind::SessionActive,
                mode: SignalRenderMode::Emoji,
            },
            Segment::Text {
                value: " [".into(),
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
///
/// ## M1 — shikumi-backed (2026-05-19)
///
/// The internal storage is now `Arc<shikumi::ConfigStore<TearConfig>>`,
/// the same primitive mado and frost use. The public API is
/// preserved (LiveConfig + load + subscribe + replace + reload +
/// spawn_watcher) but the atomic-swap + bookkeeping flow through
/// shikumi's `ConfigStore::replace` / `reload`. Eliminates the
/// hand-rolled ArcSwap+notify duplication; tear becomes shikumi
/// consumer #3 (after mado + frost) — same operator UX, same
/// hot-reload semantics, same env-override grammar fleet-wide.
#[derive(Clone)]
pub struct LiveConfig {
    /// Shikumi store — owns the inner `Arc<ArcSwap<TearConfig>>`
    /// + generation counter + last-publish bookkeeping. We
    /// delegate every atomic-swap to its `replace()` / `reload()`
    /// methods so observability stays consistent across the fleet.
    store: Arc<shikumi::ConfigStore<TearConfig>>,
    /// Per-subscriber senders. Cloning a LiveConfig clones the Arc
    /// (so daemon + watcher + RPC handlers see the same subscriber
    /// list). Mutex<Vec<Sender>> is enough — fan-out is rare (only
    /// on config replace) and the lock is held only for the
    /// fan-out loop.
    subscribers: Arc<Mutex<Vec<mpsc::Sender<Arc<TearConfig>>>>>,
}

impl Default for LiveConfig {
    fn default() -> Self {
        let path = default_config_path();
        // shikumi::ConfigStore::load handles missing-file gracefully
        // (serde defaults fill in) — same contract as the prior
        // load_or_default(). Env-prefix `TEAR_` lets operators
        // override any nested key via env without touching YAML.
        let store = match shikumi::ConfigStore::<TearConfig>::load(&path, "TEAR_") {
            Ok(s) => s,
            Err(err) => {
                // Same fall-through as the pre-M1 load_or_default():
                // operator-broken YAML should not crash the daemon;
                // log + use defaults. Tests cover both branches.
                warn!(error = %err, path = %path.display(),
                    "tear-config: shikumi load failed; using defaults");
                // Synthesize a defaults-only store via a tempfile so
                // shikumi's bookkeeping (generation, last_publish_at)
                // is still wired up — replace() can then push fresh
                // configs over the top.
                shikumi::ConfigStore::<TearConfig>::load(
                    std::path::Path::new("/dev/null/tear-defaults-missing"),
                    "TEAR_",
                )
                .unwrap_or_else(|_| panic!(
                    "shikumi defaults-only load failed (both real path \
                     and synthetic path errored); please file a bug"
                ))
            }
        };
        Self {
            store: Arc::new(store),
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl LiveConfig {
    /// Get the current config — Arc clone, no lock. Delegates to
    /// shikumi's `ConfigStore::get` which returns a Guard that we
    /// upgrade to a long-lived Arc via Guard's deref.
    #[must_use]
    pub fn load(&self) -> Arc<TearConfig> {
        Arc::clone(&self.store.get())
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
        // shikumi's replace() does the atomic swap + bookkeeping;
        // we follow with our own fan-out to the mpsc subscribers
        // (shikumi has only a single on_reload callback, which
        // doesn't fit the mado-multi-subscriber pattern).
        self.store.replace(cfg);
        let new_arc = self.load();
        Self::fan_out(&self.subscribers, new_arc);
    }

    /// Reload from the canonical path. Logs success/failure; on
    /// failure the previous config remains in place.
    pub fn reload(&self) -> Result<(), ConfigError> {
        self.store.reload()?;
        let new_arc = self.load();
        Self::fan_out(&self.subscribers, new_arc);
        Ok(())
    }

    /// Internal fan-out helper. Mirrors the pre-M1 swap-remove
    /// loop that prunes dead senders in place.
    fn fan_out(
        subscribers: &Arc<Mutex<Vec<mpsc::Sender<Arc<TearConfig>>>>>,
        snap: Arc<TearConfig>,
    ) {
        let mut subs = subscribers.lock().expect("subscribers poisoned");
        let mut i = 0;
        while i < subs.len() {
            if subs[i].send(snap.clone()).is_err() {
                subs.swap_remove(i);
            } else {
                i += 1;
            }
        }
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

// ── shikumi::TieredConfig — fleet-wide tier model (M-166 backfill) ──
//
// Operators reach via:
//   TEAR_TIER=bare tear ...
//   TEAR_TIER=default tear ...
//
// `bare()` = zero-opinion floor (empty strings, zero numerics, false
// flags, empty Vecs, None Options). `prescribed_default()` = the
// curated defaults that ship today (delegates to Default::default).
// Prior migrations: tatara, zoekt-mcp, kindling, ayatsuri, kenshi,
// taimen. See `shikumi/src/tiered.rs` for the trait contract.

impl shikumi::TieredConfig for TearConfig {
    /// Tier 0 — bare: zero-opinion floor. Every field empty / zero /
    /// false / None. The deliberate minimum-viable config; documents
    /// "no opinion" knob-by-knob.
    fn bare() -> Self {
        Self {
            prefix: String::new(),
            default_shell: String::new(),
            mouse: false,
            base_index: 0,
            keys: Vec::new(),
            status: StatusBar::default(),
            theme: TearTheme::default(),
            reload_debounce_ms: 0,
            recording_auto_dir: None,
            ai: None,
            audit_log: None,
            auth_token_env: None,
            scrollback: <ScrollbackConfig as shikumi::TieredConfig>::bare(),
        }
    }

    /// Tier 2 — prescribed: the curated tear defaults shipped today.
    /// Delegates to Default so there's one source.
    fn prescribed_default() -> Self {
        Self::default()
    }
}

impl shikumi::TieredConfig for ScrollbackConfig {
    /// Tier 0 — bare: zero rows, no caps, all flags false.
    fn bare() -> Self {
        Self {
            rows: 0,
            max_bytes: None,
            keep_on_clear: false,
            on_alt_screen: false,
            skip_blank_rows: false,
            reflow_on_resize: false,
        }
    }

    fn prescribed_default() -> Self {
        Self::default()
    }
}

impl shikumi::TieredConfig for AiConfig {
    /// Tier 0 — bare: empty provider / model / endpoint, no key, zero
    /// context bytes. Documents what "no AI opinion" looks like.
    fn bare() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            endpoint: String::new(),
            api_key_env: None,
            context_bytes: 0,
        }
    }

    fn prescribed_default() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tiered_tests {
    use super::*;
    use shikumi::{ConfigTier, TieredConfig};

    #[test]
    fn tear_config_bare_is_zero_opinion() {
        let b = <TearConfig as TieredConfig>::bare();
        assert_eq!(b.prefix, "");
        assert_eq!(b.default_shell, "");
        assert!(!b.mouse);
        assert_eq!(b.base_index, 0);
        assert!(b.keys.is_empty());
        assert_eq!(b.reload_debounce_ms, 0);
        assert!(b.recording_auto_dir.is_none());
        assert!(b.ai.is_none());
        assert!(b.audit_log.is_none());
        assert!(b.auth_token_env.is_none());
        assert_eq!(b.scrollback.rows, 0);
    }

    #[test]
    fn tear_config_prescribed_matches_default() {
        let p = <TearConfig as TieredConfig>::prescribed_default();
        let d = TearConfig::default();
        assert_eq!(p.prefix, d.prefix);
        assert_eq!(p.base_index, d.base_index);
        assert_eq!(p.mouse, d.mouse);
        assert_eq!(p.keys.len(), d.keys.len());
    }

    #[test]
    fn tear_config_diff_bare_vs_default_is_non_empty() {
        let b = <TearConfig as TieredConfig>::bare();
        let d = <TearConfig as TieredConfig>::prescribed_default();
        let diff = d.diff_against(&b);
        assert!(
            !diff.is_empty_diff(),
            "bare and prescribed_default must differ"
        );
    }

    #[test]
    fn tear_config_resolve_tier_dispatches_correctly() {
        let bare = <TearConfig as TieredConfig>::resolve_tier(ConfigTier::Bare);
        assert_eq!(bare.prefix, "");
        assert_eq!(bare.base_index, 0);

        let default = <TearConfig as TieredConfig>::resolve_tier(ConfigTier::Default);
        assert_eq!(default.prefix, "ctrl+b");
        assert_eq!(default.base_index, 1);
    }

    #[test]
    fn scrollback_config_bare_and_prescribed_differ() {
        let b = <ScrollbackConfig as TieredConfig>::bare();
        assert_eq!(b.rows, 0);
        assert!(!b.keep_on_clear);
        assert!(!b.reflow_on_resize);

        let p = <ScrollbackConfig as TieredConfig>::prescribed_default();
        assert_eq!(p.rows, usize::MAX);
        assert!(p.keep_on_clear);
        assert!(p.reflow_on_resize);
    }

    #[test]
    fn ai_config_bare_and_prescribed_differ() {
        let b = <AiConfig as TieredConfig>::bare();
        assert_eq!(b.provider, "");
        assert_eq!(b.model, "");
        assert_eq!(b.context_bytes, 0);

        let p = <AiConfig as TieredConfig>::prescribed_default();
        assert_eq!(p.provider, "ollama");
        assert_eq!(p.context_bytes, 2000);
    }

    #[test]
    fn prefix_chord_converges_with_fleet_keybinds_atlas() {
        // Closes the loop on the 2026-05-21 chord-drift class of bug
        // for tear's multiplexer surface: the prescribed prefix MUST
        // come from `ishou_tokens::FleetKeybinds::multiplexer_prefix`
        // routed through KeyChord::from_tmux. Drift in either direction
        // (atlas changes vs tear hard-codes) fails here loudly rather
        // than at operator-press time.
        let atlas_chord = ishou_tokens::FleetKeybinds::prescribed().multiplexer_prefix;
        let normalized = tear_types::KeyChord::from_tmux(atlas_chord).0;
        let prescribed = <TearConfig as TieredConfig>::prescribed_default();
        assert_eq!(
            prescribed.prefix, normalized,
            "tear prefix drifted from FleetKeybinds atlas: prescribed={:?}, atlas-normalized={:?}",
            prescribed.prefix, normalized,
        );
    }

    // NOTE: ishou_tokens::convergence::Guard's `expect_multiplexer_prefix`
    // compares the supplied string against the atlas's raw "C-b" form.
    // Tear's prefix is in the long-form "ctrl+b" after KeyChord::from_tmux
    // normalization. The two notations refer to the same chord but the
    // Guard's string-equality check rejects them as drift. The fix lives
    // in a future ishou_tokens release (Guard.normalize via tear-types'
    // canonical-form coercion) and is tracked separately. Tear's
    // convergence is meanwhile pinned by the atlas-routing assertion
    // above (`prefix_chord_converges_with_fleet_keybinds_atlas`) which
    // performs the exact normalization both sides need.
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

    // ── round-trip coverage for the newest fields ──

    #[test]
    fn auth_token_env_round_trips_through_yaml() {
        let mut cfg = TearConfig::default();
        cfg.auth_token_env = Some("TEAR_AUTH_TOKEN".into());
        let y = serde_yaml_ng::to_string(&cfg).unwrap();
        assert!(y.contains("TEAR_AUTH_TOKEN"), "yaml: {y}");
        let back: TearConfig = serde_yaml_ng::from_str(&y).unwrap();
        assert_eq!(back.auth_token_env, Some("TEAR_AUTH_TOKEN".into()));
    }

    #[test]
    fn audit_log_round_trips_through_yaml() {
        let mut cfg = TearConfig::default();
        cfg.audit_log = Some("~/.local/share/tear/audit.log".into());
        let y = serde_yaml_ng::to_string(&cfg).unwrap();
        let back: TearConfig = serde_yaml_ng::from_str(&y).unwrap();
        assert_eq!(back.audit_log.as_deref(), Some("~/.local/share/tear/audit.log"));
    }

    #[test]
    fn ai_config_round_trips_through_yaml() {
        let mut cfg = TearConfig::default();
        cfg.ai = Some(AiConfig {
            provider: "openai".into(),
            model: "gpt-5-codex".into(),
            endpoint: "https://api.openai.com/v1".into(),
            api_key_env: Some("OPENAI_API_KEY".into()),
            context_bytes: 4000,
        });
        let y = serde_yaml_ng::to_string(&cfg).unwrap();
        let back: TearConfig = serde_yaml_ng::from_str(&y).unwrap();
        assert_eq!(back.ai, cfg.ai);
    }

    #[test]
    fn empty_yaml_yields_defaults_for_new_fields() {
        // A pre-existing operator's tear.yaml that predates #5 / #6
        // must keep working; serde-default fills in the new fields.
        let cfg: TearConfig = serde_yaml_ng::from_str("{}").unwrap();
        assert_eq!(cfg.auth_token_env, None);
        assert_eq!(cfg.audit_log, None);
        assert_eq!(cfg.ai, None);
    }
}
