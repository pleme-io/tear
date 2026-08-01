//! `InProcess` — the in-memory [`tear_types::MultiplexerControl`]
//! implementation backed by `parking_lot::RwLock<Registry>` +
//! `BTreeMap<PaneId, PtyHandle>`.
//!
//! ## Architecture
//!
//! - **Registry** (`Arc<RwLock<Registry>>`) — pure typed state: which
//!   sessions / windows / panes exist, what their layouts are. Read
//!   by `list_sessions`, `get_*`; written by mutating ops.
//! - **PTYs** (`Arc<Mutex<BTreeMap<PaneId, PtyHandle>>>`) — the
//!   physical PTY handles. Separated from the typed registry so a
//!   daemon can serialise the registry to disk for resurrection
//!   while PTYs (which can't outlive a process) stay in memory.
//!
//! ## Mado integration shape (M5)
//!
//! At M5 mado's `render::SharedTerminal` will become an
//! `Arc<InProcess>` instead of an `Arc<RwLock<Terminal>>`. Each
//! mado pane is then just a `PaneId` view over the shared
//! `InProcess`; `mado_pane.feed(bytes)` delegates to
//! `inproc.feed_pane_bytes(pane_id, bytes)` which calls the same
//! `vte` parser the tear-daemon uses on the headless path.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use portable_pty::PtySize;
use tracing::{debug, info};

use tear_types::{
    ControlError, ControlResult, Direction, LayoutKind, LayoutNode, LeafRemoval,
    MultiplexerControl, PaneId, Rect, SessionId, SplitOrientation, TearPane, TearSession,
    TearWindow, WindowId,
};

use std::sync::mpsc;

use crate::pane_grid::PaneGrid;
use crate::pty::PtyHandle;
use crate::reap::AllPanesExited;
use crate::recording::PaneRecording;
use crate::registry::Registry;

/// Per-pane byte-stream fan-out state.
///
/// Co-locates the live subscriber senders with a `closed`
/// end-of-stream marker so that *registering* a subscriber and
/// *closing* the stream on child-exit are decided under a single
/// lock. Without the co-located marker, a `subscribe` that races with
/// (or follows) the pane's exit could push a sender that is never
/// dropped — the exact "receiver blocks forever" failure this whole
/// change exists to remove.
#[derive(Default)]
struct PaneSubscribers {
    /// `Some(code)` once the pane's PTY child has exited; no further
    /// bytes will ever be sent. New subscribers then receive an
    /// already-disconnected receiver instead of a live registration.
    closed: Option<i32>,
    /// Live subscribers — each receives a clone of every PTY chunk.
    senders: Vec<mpsc::Sender<Vec<u8>>>,
}

/// The native in-process multiplexer backend.
pub struct InProcess {
    registry: Arc<RwLock<Registry>>,
    ptys: Arc<Mutex<BTreeMap<PaneId, PtyHandle>>>,
    /// Per-pane VT parser + cell grid. Phase-2-MVP wires PTY bytes
    /// into these so [`Self::pane_snapshot`] returns the rendered
    /// state. Wrapped per-pane in `Mutex` so the PTY reader thread
    /// and snapshot callers can race independently per pane.
    grids: Arc<Mutex<BTreeMap<PaneId, Arc<Mutex<PaneGrid>>>>>,
    /// Per-pane byte-stream fan-out state ([`PaneSubscribers`]): the
    /// live subscriber senders plus a `closed` end-of-stream marker.
    /// On send error the fan-out prunes dead subscribers; on child
    /// exit [`Self::spawn_pty_for`]'s `on_exit` hook marks the entry
    /// closed and drops the senders, so every [`mpsc::Receiver`]
    /// disconnects — the end-of-stream signal mado's
    /// `attach_live.run()` and the daemon's `serve_subscription`
    /// block on.
    subscribers: Arc<Mutex<BTreeMap<PaneId, PaneSubscribers>>>,
    /// Per-pane recording (#4). Cheap when disabled — the on_bytes
    /// hook hits a single boolean before deciding whether to
    /// deep-copy the chunk. Recording is opt-in via
    /// `enable_pane_recording`.
    recordings: Arc<Mutex<BTreeMap<PaneId, Arc<PaneRecording>>>>,
    /// UDS path the tear-daemon bound to. Stamped onto every PTY
    /// child's `TEAR_SOCKET` env var so shells / prompts /
    /// child processes can re-discover the daemon without
    /// scanning the XDG runtime dir.
    socket_path: Arc<RwLock<Option<std::path::PathBuf>>>,
    /// Embedder-supplied env + cwd override (mado's typed capability
    /// projection — `TERM=xterm-ghostty` + `TERMINFO` + `COLORTERM` +
    /// `PWD`), applied to every child's env AFTER the inherited +
    /// fallback env so the embedder's richer capability set wins over
    /// the conservative `xterm-256color` default. Empty by default
    /// (the pre-seam behaviour); set via [`Self::set_spawn_env`]. This
    /// is the fix for "vim grey + wrong font in the embedded-tear
    /// window" (operator report 2026-06-12).
    spawn_env: Arc<RwLock<tear_types::SpawnEnv>>,
}

impl Default for InProcess {
    fn default() -> Self {
        Self::new()
    }
}

impl InProcess {
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Arc::new(RwLock::new(Registry::new())),
            ptys: Arc::new(Mutex::new(BTreeMap::new())),
            grids: Arc::new(Mutex::new(BTreeMap::new())),
            subscribers: Arc::new(Mutex::new(BTreeMap::new())),
            recordings: Arc::new(Mutex::new(BTreeMap::new())),
            socket_path: Arc::new(RwLock::new(None)),
            spawn_env: Arc::new(RwLock::new(tear_types::SpawnEnv::none())),
        }
    }

    /// Set the embedder's typed env + cwd override, applied to every
    /// subsequent child PTY's env AFTER the inherited + fallback env.
    /// mado calls this with its `caps::EnvProjection` pairs (+ the boot
    /// cwd) so vim in an embedded-tear window sees `xterm-ghostty` +
    /// truecolor + the vendored terminfo — identical to the local-PTY
    /// path. Idempotent; the last write wins.
    pub fn set_spawn_env(&self, env: tear_types::SpawnEnv) {
        *self.spawn_env.write() = env;
    }

    /// The embedder's current spawn cwd override (`SpawnEnv.cwd`), if any.
    ///
    /// mado stamps this per-spawn (the focused pane's cwd / boot cwd)
    /// before each `new_session`, so the daemon can read it at session-
    /// create time to learn which directory a session was opened in —
    /// the seed for praça's project↔session binding (M1 "Remember").
    /// `None` when no embedder cwd override is set (the bare-daemon path).
    #[must_use]
    pub fn spawn_cwd(&self) -> Option<std::path::PathBuf> {
        self.spawn_env
            .read()
            .cwd
            .as_ref()
            .map(std::path::PathBuf::from)
    }

    /// Record the UDS path the daemon bound to. Subsequent PTY
    /// spawns stamp `TEAR_SOCKET=<path>` on the child env. Called
    /// by `tear-daemon::start*` immediately after `bind`.
    pub fn set_socket_path(&self, path: std::path::PathBuf) {
        *self.socket_path.write() = Some(path);
    }

    /// Borrow the recorded socket path, if any.
    pub fn socket_path(&self) -> Option<std::path::PathBuf> {
        self.socket_path.read().clone()
    }

    /// Enable recording for `pane_id`. Idempotent — calling on an
    /// already-enabled pane resets the recording buffer (per
    /// `PaneRecording::enable` semantics). Reads the pane's
    /// current size from the registry for the asciinema header.
    pub fn enable_pane_recording(&self, pane_id: PaneId) -> ControlResult<()> {
        let (cols, rows) = {
            let r = self.registry.read();
            let Some((sid, _wid)) = r.locate_pane(pane_id) else {
                return Err(ControlError::NoSuchPane(pane_id));
            };
            let Some(p) = r.sessions.get(&sid).and_then(|s| s.panes.get(&pane_id)) else {
                return Err(ControlError::NoSuchPane(pane_id));
            };
            p.size_cells
        };
        let mut recs = self.recordings.lock();
        let rec = recs
            .entry(pane_id)
            .or_insert_with(|| Arc::new(PaneRecording::default()));
        rec.enable(cols, rows);
        Ok(())
    }

    /// Stop recording for `pane_id`. The captured buffer is
    /// retained until the next `enable` or `kill_pane`; the
    /// operator can still `export` after stopping.
    pub fn disable_pane_recording(&self, pane_id: PaneId) -> ControlResult<()> {
        let recs = self.recordings.lock();
        match recs.get(&pane_id) {
            Some(r) => {
                r.disable();
                Ok(())
            }
            None => Err(ControlError::NoSuchPane(pane_id)),
        }
    }

    /// Export the pane's captured recording as asciinema v2
    /// .cast (JSON-lines). Returns an empty string when nothing
    /// has been captured yet (recording was never enabled or the
    /// pane has no events).
    pub fn export_pane_recording(&self, pane_id: PaneId) -> ControlResult<String> {
        let recs = self.recordings.lock();
        match recs.get(&pane_id) {
            Some(r) => Ok(r.to_cast_json()),
            None => Err(ControlError::NoSuchPane(pane_id)),
        }
    }

    /// `(is_enabled, event_count)` snapshot — driven by the
    /// pane-info / pane-record-status ergonomics.
    pub fn pane_recording_status(&self, pane_id: PaneId) -> ControlResult<(bool, u32)> {
        let recs = self.recordings.lock();
        match recs.get(&pane_id) {
            Some(r) => Ok((r.is_enabled(), r.event_count() as u32)),
            None => Ok((false, 0)),
        }
    }

    /// List captured blocks for a pane (oldest-first). Filters by
    /// `since_index` — pass 0 to get every retained block. The
    /// daemon currently caps at 10_000 blocks per pane (ring
    /// eviction). Returns `NoSuchPane` if the pane has no grid.
    pub fn pane_blocks_list(
        &self,
        pane_id: PaneId,
        since_index: u64,
        limit: u32,
    ) -> ControlResult<Vec<crate::blocks::Block>> {
        let grid_arc = {
            let map = self.grids.lock();
            map.get(&pane_id)
                .cloned()
                .ok_or(ControlError::NoSuchPane(pane_id))?
        };
        let grid = grid_arc.lock();
        let out: Vec<crate::blocks::Block> = grid
            .state
            .blocks
            .iter()
            .filter(|b| b.index >= since_index)
            .take(limit as usize)
            .cloned()
            .collect();
        Ok(out)
    }

    /// Fetch one block by per-pane index. Returns NoSuchPane if
    /// the pane is gone, or the InvalidArgument variant via
    /// Rejected when the block has been evicted / never existed.
    pub fn pane_block_at(
        &self,
        pane_id: PaneId,
        index: u64,
    ) -> ControlResult<crate::blocks::Block> {
        let grid_arc = {
            let map = self.grids.lock();
            map.get(&pane_id)
                .cloned()
                .ok_or(ControlError::NoSuchPane(pane_id))?
        };
        let grid = grid_arc.lock();
        grid.state
            .blocks
            .get(index)
            .cloned()
            .ok_or_else(|| ControlError::Rejected(format!(
                "no block at index {index} (oldest evicted or never existed)"
            )))
    }

    /// `(total_completed_blocks, current_in_progress)` — useful
    /// for status displays. `tear top` reads this column.
    pub fn pane_blocks_status(&self, pane_id: PaneId) -> ControlResult<(u32, bool)> {
        let grid_arc = {
            let map = self.grids.lock();
            map.get(&pane_id)
                .cloned()
                .ok_or(ControlError::NoSuchPane(pane_id))?
        };
        let grid = grid_arc.lock();
        Ok((
            grid.state.blocks.len() as u32,
            grid.state.blocks.current().is_some(),
        ))
    }

    /// Register a byte-stream subscriber for the named pane.
    /// Returns the receiver end of an `mpsc::channel`; every PTY
    /// chunk that lands in this pane is sent on the corresponding
    /// sender. Drop the receiver to unsubscribe — the next send
    /// will error and the daemon prunes the dead sender.
    ///
    /// Returns `NoSuchPane` if the pane has no PTY (never spawned
    /// or already killed).
    ///
    /// If the pane's child has already exited (remain-on-exit dead
    /// pane), the returned receiver is born already-disconnected: no
    /// live sender is registered, so the consumer can replay the
    /// pane's final grid snapshot and then immediately observe
    /// end-of-stream (`recv() -> Err`) instead of blocking forever on
    /// a pane that will never emit again. The `closed` check and the
    /// sender push happen under the same lock, so a `subscribe` that
    /// races with the pane's exit can't leak a sender that never
    /// disconnects.
    pub fn subscribe_pane_bytes(
        &self,
        pane: PaneId,
    ) -> ControlResult<mpsc::Receiver<Vec<u8>>> {
        // Confirm the pane exists; we don't actually need the
        // grid here (the sender is registered regardless), but
        // subscribing to a phantom pane silently is a footgun.
        if !self.ptys.lock().contains_key(&pane) {
            return Err(ControlError::NoSuchPane(pane));
        }
        let (tx, rx) = mpsc::channel();
        let mut subs = self.subscribers.lock();
        let ps = subs.entry(pane).or_default();
        if ps.closed.is_none() {
            ps.senders.push(tx);
        }
        // else: stream already closed — drop `tx` here so `rx` is
        // immediately disconnected.
        Ok(rx)
    }

    /// Borrow the registry read-only — useful for callers that want
    /// to scan multiple entities atomically without locking per-call.
    pub fn with_registry<R>(&self, f: impl FnOnce(&Registry) -> R) -> R {
        let r = self.registry.read();
        f(&r)
    }

    /// Return a serializable snapshot of the named pane's rendered
    /// grid. Returns `NoSuchPane` if the pane never had a grid
    /// installed (which can only happen if it never had a PTY —
    /// every PTY-spawning code path also installs a grid).
    pub fn pane_snapshot(&self, pane_id: PaneId) -> ControlResult<tear_types::PaneSnapshot> {
        let grid_arc = {
            let map = self.grids.lock();
            map.get(&pane_id)
                .cloned()
                .ok_or(ControlError::NoSuchPane(pane_id))?
        };
        let grid = grid_arc.lock();
        Ok(grid.snapshot())
    }

    /// No-alloc DECCKM lookup — reads one `bool` off the live
    /// `PaneGrid` rather than building a full `PaneSnapshot`.
    /// Mado's embedded-tear input loop hits this on every arrow
    /// keystroke; the snapshot path would clone the entire cell
    /// grid (~100KB per call on an 80×40 pane).
    pub fn pane_cursor_keys_mode(&self, pane_id: PaneId) -> ControlResult<bool> {
        let grid_arc = {
            let map = self.grids.lock();
            map.get(&pane_id)
                .cloned()
                .ok_or(ControlError::NoSuchPane(pane_id))?
        };
        let grid = grid_arc.lock();
        Ok(grid.cursor_keys_mode())
    }

    /// Spawn a PTY for the given pane. Caller pre-creates the typed
    /// pane via the registry; this attaches the runtime + installs
    /// the per-pane VT parser AND injects the `TEAR_*` env vars so
    /// shells and prompts (starship) can see they're running inside
    /// a tear session.
    /// `args` is `shell`'s argv[1..], handed to [`PtyHandle::spawn`]
    /// as an argument vector. It reaches `execvp` directly — there is
    /// no shell in between, so no quoting or escaping applies.
    fn spawn_pty_for(
        &self,
        pane_id: PaneId,
        shell: &str,
        args: &[String],
        size: (u16, u16),
    ) -> anyhow::Result<()> {
        // Typed cross-tool env-var names (the SAME source seki's prompt
        // reads) — hoisted to the top of the fn so it's an item, not a
        // statement-position import.
        use ishou_tokens::FleetStateVar as Fsv;
        let pty_size = PtySize {
            rows: size.1,
            cols: size.0,
            pixel_width: 0,
            pixel_height: 0,
        };
        // Resolve the session this pane belongs to so we can stamp
        // TEAR_SESSION_{ID,NAME} on the child's env. Look-up is
        // cheap (BTreeMap walk over typically <10 sessions); the
        // alternative — caller threading session_id in — would
        // bloat every call site.
        let (session_id, session_name) = {
            let r = self.registry.read();
            r.sessions
                .values()
                .find(|s| s.panes.contains_key(&pane_id))
                .map(|s| (s.id.to_string(), s.name.clone()))
                .unwrap_or_else(|| (String::new(), String::new()))
        };
        // portable_pty's CommandBuilder uses an explicit env-vec
        // — any env we pass REPLACES the parent process's env
        // rather than augmenting it. The tear-daemon typically
        // runs under launchd (macOS) / systemd-user (Linux) with
        // a minimal env, so we MUST inherit the daemon's env
        // first (which carries PATH from blackmatter-shell's
        // session-vars), THEN stamp TEAR_* on top, THEN ensure
        // TERM is set so terminfo-based programs (`clear`, vi,
        // anything that reads $TERM) work.
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        // The env-var NAMES come from the typed cross-tool contract
        // (`Fsv`, hoisted above) — the SAME source seki's prompt reads,
        // so a rename is a compile-time change on both sides. The VALUES
        // are unchanged.
        env.push(("TEAR".into(), "1".into()));
        env.push((Fsv::TearSessionId.name().into(), session_id));
        env.push((Fsv::TearSessionName.name().into(), session_name));
        env.push((Fsv::TearPaneId.name().into(), pane_id.to_string()));
        if let Some(p) = self.socket_path() {
            env.push((
                Fsv::TearSocket.name().into(),
                p.to_string_lossy().to_string(),
            ));
        }
        // TERM fallback — if the daemon was spawned by launchd
        // and doesn't have TERM set, every shell inside tear
        // would see `TERM environment variable not set` and
        // `clear` / `tput` / readline arrow keys would break.
        // xterm-256color is the modern conservative default.
        if !env.iter().any(|(k, _)| k == "TERM") {
            env.push(("TERM".into(), "xterm-256color".into()));
        }
        // COLORTERM advertises 24-bit colour support to apps
        // that opt-in (newer vim/neovim, modern btop, etc.).
        if !env.iter().any(|(k, _)| k == "COLORTERM") {
            env.push(("COLORTERM".into(), "truecolor".into()));
        }
        // PATH augmentation — when the daemon is spawned by
        // launchd (macOS) / systemd-user (Linux), its inherited
        // PATH is the minimal `/usr/bin:/bin:/usr/sbin:/sbin`.
        // Shells that try to invoke `tear` from a starship custom
        // block, or any home-manager-installed binary, fail —
        // and starship's prompt rendering hangs / errors silently.
        // We prepend the operator's home-manager + nix-profile
        // bin dirs to whatever PATH was inherited so the shell
        // can find the same binaries the user sees outside tear.
        if let Some(home) = env.iter().find(|(k, _)| k == "HOME").map(|(_, v)| v.clone()) {
            let user = env
                .iter()
                .find(|(k, _)| k == "USER")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let extra_paths = [
                format!("/etc/profiles/per-user/{user}/bin"),
                format!("{home}/.nix-profile/bin"),
                "/run/current-system/sw/bin".to_string(),
                "/nix/var/nix/profiles/default/bin".to_string(),
                "/usr/local/bin".to_string(),
            ];
            // Find existing PATH entry to prepend to; if missing,
            // build PATH from scratch with sensible defaults.
            let existing_path = env
                .iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| "/usr/bin:/bin:/usr/sbin:/sbin".to_string());
            // Prepend extras that aren't already in PATH (de-dupe
            // so we don't bloat PATH on every nested spawn).
            let mut new_path = String::new();
            for p in &extra_paths {
                if !existing_path
                    .split(':')
                    .any(|seg| seg == p.as_str())
                {
                    if !new_path.is_empty() {
                        new_path.push(':');
                    }
                    new_path.push_str(p);
                }
            }
            if !new_path.is_empty() {
                new_path.push(':');
                new_path.push_str(&existing_path);
            } else {
                new_path = existing_path;
            }
            // Replace existing PATH entry (or append if missing).
            if let Some(slot) = env.iter_mut().find(|(k, _)| k == "PATH") {
                slot.1 = new_path;
            } else {
                env.push(("PATH".into(), new_path));
            }
        }
        // Embedder env + cwd override (mado's capability projection),
        // applied LAST so its TERM=xterm-ghostty + TERMINFO + COLORTERM
        // win over the xterm-256color fallback above (the "vim grey"
        // fix), and PWD is stamped to match the cwd. Empty pre-seam.
        let spawn_env = self.spawn_env.read().clone();
        spawn_env.apply_to(&mut env);
        let cwd = spawn_env.cwd.clone();
        // Allocate the per-pane grid and register it BEFORE spawning
        // the PTY — the reader thread starts immediately on spawn,
        // and we want the first bytes to find their grid.
        let grid = Arc::new(Mutex::new(PaneGrid::new(size.0 as usize, size.1 as usize)));
        self.grids.lock().insert(pane_id, grid.clone());

        let grid_for_callback = grid.clone();
        let subscribers_for_callback = self.subscribers.clone();
        let recordings_for_callback = self.recordings.clone();
        let on_bytes = Box::new(move |bytes: &[u8]| {
            grid_for_callback.lock().feed(bytes);
            // Fan out to subscribers (Phase-2.5 push subscriptions).
            // Cheap when there are zero subscribers; per-subscriber
            // cost is a Vec clone + mpsc::send. On send error the
            // sender is dead — prune it.
            let mut subs = subscribers_for_callback.lock();
            if let Some(ps) = subs.get_mut(&pane_id) {
                let senders = &mut ps.senders;
                let mut i = 0;
                while i < senders.len() {
                    if senders[i].send(bytes.to_vec()).is_err() {
                        senders.swap_remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
            drop(subs);
            // Push to the recording (#4). The Arc-cloned
            // recording handle's `push` is a single Mutex-lock
            // + early return when disabled, so this is cheap
            // even when nothing's recording.
            if let Some(rec) = recordings_for_callback.lock().get(&pane_id) {
                rec.push(bytes);
            }
            debug!(pane_id = %pane_id, n = bytes.len(), "tear-core: pty bytes fed to grid + subscribers");
        });
        // on_exit — fired once by the PTY reader thread when the child
        // exits (PTY EOF). Three typed consequences:
        //
        //  1. Mark the pane `PaneState::Exited { code }` in the typed
        //     registry. A *watched* pane + its final grid stay (tmux
        //     remain-on-exit) so `tear list` / snapshots still see it;
        //     only the live byte stream ends.
        //  2. Mark the subscriber entry `closed` + drop every live
        //     sender. Each engate/daemon `Receiver.recv()` then
        //     returns `Err` — the end-of-stream signal mado's
        //     `attach_live.run()` and the daemon's `serve_subscription`
        //     block on. Without this the channel stays open forever and
        //     a single-pane GUI (mado embedded) never learns the shell
        //     exited, so its window never closes.
        //  3. Reap-on-exit (session-leak fix, 2026-07-06): when EVERY
        //     pane of the owning session has exited AND no pane carried
        //     a live subscriber at exit time (nothing was watching),
        //     the whole session is removed from the registry and its
        //     runtime artifacts are detached. Without this, sessions
        //     whose shell exited unwatched (agent spawns that never
        //     attached, one-shot commands) linger in the registry
        //     forever — surfacing as ghost rows in mado's Ctrl-S
        //     picker. Conservative by construction: any live sender on
        //     any pane of the session (a mado window, a recorder)
        //     preserves the remain-on-exit behavior unchanged, and a
        //     session with any Running/Spawning pane is never touched.
        //
        // Lock order matches the kill paths (the registry write, the
        // subscribers lock, and the reap's detach scope are separate
        // critical sections — never nested — so no inversion). The
        // subscribers step is gated on the pane still being present in
        // the registry so an explicit `kill_pane` that races with
        // natural exit doesn't leave a lingering empty entry. The reap
        // runs on this pane's own reader thread, which is safe: on the
        // natural-exit path the child slot is already `take()`n +
        // reaped (so `PtyHandle::drop` skips the kill) and every
        // sibling pane is `Exited` (their readers finished), and the
        // handle drops happen with NO `InProcess` lock held
        // (`detach_panes`' deadlock contract).
        let subscribers_for_exit = Arc::clone(&self.subscribers);
        let registry_for_exit = Arc::clone(&self.registry);
        let ptys_for_exit = Arc::clone(&self.ptys);
        let grids_for_exit = Arc::clone(&self.grids);
        let on_exit = Box::new(move |code: Option<i32>| {
            let (still_present, fully_exited) = {
                let mut r = registry_for_exit.write();
                let mut found = false;
                let mut fully: Option<SessionId> = None;
                for s in r.sessions.values_mut() {
                    if let Some(p) = s.panes.get_mut(&pane_id) {
                        p.state = tear_types::PaneState::Exited {
                            code: code.unwrap_or(-1),
                        };
                        found = true;
                        if s.panes.values().all(|p| {
                            matches!(p.state, tear_types::PaneState::Exited { .. })
                        }) {
                            fully = Some(s.id);
                        }
                        break;
                    }
                }
                (found, fully)
            };
            // `watched` = any pane of the (fully-exited) session still
            // carried a live sender at exit time — this pane's senders
            // inspected BEFORE the clear, siblings' as they stand
            // (their own on_exit already cleared them if they exited
            // unwatched). Watched sessions keep remain-on-exit.
            let watched = {
                let mut subs = subscribers_for_exit.lock();
                let this_pane_watched = if still_present {
                    let ps = subs.entry(pane_id).or_default();
                    let had_live_sender = !ps.senders.is_empty();
                    ps.closed = Some(code.unwrap_or(-1));
                    ps.senders.clear();
                    had_live_sender
                } else {
                    // Pane was explicitly killed concurrently — drop any
                    // entry rather than recreating one for a dead id.
                    subs.remove(&pane_id);
                    true
                };
                this_pane_watched
                    || fully_exited.is_some_and(|sid| {
                        registry_for_exit.read().sessions.get(&sid).is_some_and(|s| {
                            s.panes.keys().any(|p| {
                                *p != pane_id
                                    && subs.get(p).is_some_and(|ps| !ps.senders.is_empty())
                            })
                        })
                    })
            };
            debug!(pane_id = %pane_id, ?code, "tear-core: pane child exited — marked Exited + disconnected subscribers");
            if let Some(sid) = fully_exited.filter(|_| !watched) {
                // The proof is re-taken under the write lock inside
                // `reap_proven_dead`, so a window/pane spawned into the
                // session between the mark and here (or a concurrent
                // explicit kill) aborts the reap.
                //
                // BOUND TO A `let` ON PURPOSE — do not inline this into
                // the `if let` scrutinee. Rust 2024's if-let rescoping
                // drops scrutinee temporaries before the `else` arm but
                // NOT before the success arm, so the read guard would
                // still be alive inside the block and `reap_proven_dead`'s
                // `registry.write()` would wait forever on a reader held
                // by its own thread. Measured 2026-07-31: the daemon
                // wedged on the first pane whose child exited, with
                // `tear-pty-reader` parked in `wait_for_readers` and
                // every later `list_sessions` parked behind it.
                let proof = registry_for_exit
                    .read()
                    .sessions
                    .get(&sid)
                    .and_then(AllPanesExited::witness);
                if let Some(proof) = proof {
                    reap_proven_dead(
                        &registry_for_exit,
                        &ptys_for_exit,
                        &grids_for_exit,
                        &subscribers_for_exit,
                        proof,
                    );
                }
            }
        });
        let pty = PtyHandle::spawn(
            shell,
            args,
            cwd.as_deref(),
            &env,
            pty_size,
            on_bytes,
            on_exit,
        )?;
        self.ptys.lock().insert(pane_id, pty);
        // Reap-race guard: a child that exits faster than this insert
        // lands may already have been reaped (session gone from the
        // registry) — pull the handle straight back out so a dead
        // session can't strand a PtyHandle in the map. The handle (if
        // any) drops outside every lock, per `detach_panes`' contract.
        let orphaned = {
            let in_registry = self
                .registry
                .read()
                .sessions
                .values()
                .any(|s| s.panes.contains_key(&pane_id));
            if in_registry {
                None
            } else {
                self.grids.lock().remove(&pane_id);
                self.subscribers.lock().remove(&pane_id);
                self.ptys.lock().remove(&pane_id)
            }
        };
        drop(orphaned);
        Ok(())
    }

    /// Detach every runtime artifact for `panes` — PTY handle, VT grid,
    /// subscriber fan-out — under the three map locks, RETURNING the
    /// PTY handles instead of dropping them. Callers drop the returned
    /// vec only after every `InProcess` lock is released.
    ///
    /// DEADLOCK CONTRACT (mado L1 teardown wedge, 2026-06-10):
    /// dropping a [`PtyHandle`] kills + reaps the child, and the
    /// pane's `tear-pty-reader` thread may simultaneously be blocked
    /// acquiring `subscribers` (inside `on_bytes`) or `registry`
    /// (inside `on_exit`). Dropping the handle while this thread holds
    /// those locks is a mutual wait: the reap can't finish until the
    /// reader drains, the reader can't drain until the locks release —
    /// observed as a 20+ minute wedge. The handles therefore ALWAYS
    /// leave the maps inside the lock scope and die outside it (the
    /// reap itself is additionally bounded — see `pty::reap_with_deadline`).
    fn detach_panes(&self, panes: &[PaneId]) -> Vec<PtyHandle> {
        let mut ptys = self.ptys.lock();
        let mut grids = self.grids.lock();
        let mut subs = self.subscribers.lock();
        let mut detached = Vec::with_capacity(panes.len());
        for p in panes {
            if let Some(h) = ptys.remove(p) {
                detached.push(h);
            }
            grids.remove(p);
            // Dropping the sender vec disconnects subscribers
            // cleanly — their recv() returns Err on next read.
            subs.remove(p);
        }
        detached
    }

    /// Remove a session the caller has **proven** dead — every pane's
    /// child process has exited.
    ///
    /// This is the *only* way for a daemon to end a session on its own
    /// initiative, and [`AllPanesExited`] is the only ticket in. A rule
    /// keyed on "nobody is attached", on an idle timer, or on the
    /// session's [`tear_types::SessionSource`] cannot construct one —
    /// see [`crate::reap`] for the incident that motivated the narrowing.
    ///
    /// Returns `true` if the session was removed. `false` means the proof
    /// went stale between witnessing and now (a `new_window` landed, or
    /// an explicit `kill_session` won the race) — the proof is re-taken
    /// under the write lock, so a resurrected session is never reaped.
    ///
    /// **Call with no registry guard held.** Mint the proof from an owned
    /// [`TearSession`] (`list_sessions()` / `get_session()` both clone) or
    /// from a `read()` bound to its own `let` statement. Passing a proof
    /// witnessed inside a still-live read guard deadlocks on the `write()`
    /// below — `parking_lot`'s `RwLock` is not reentrant and this thread
    /// would be waiting for itself.
    pub fn reap_proven_dead_session(&self, proof: AllPanesExited) -> bool {
        reap_proven_dead(
            &self.registry,
            &self.ptys,
            &self.grids,
            &self.subscribers,
            proof,
        )
    }
}

/// Free-standing so both `spawn_pty_for`'s `on_exit` hook (which owns
/// cloned `Arc`s, not a `&self`) and [`InProcess::reap_proven_dead_session`]
/// share one implementation — the session-removal path exists once.
///
/// Same shape as [`InProcess::detach_panes`]: artifacts leave the maps
/// under the locks, handles die outside them (that deadlock contract).
fn reap_proven_dead(
    registry: &RwLock<Registry>,
    ptys: &Mutex<BTreeMap<PaneId, PtyHandle>>,
    grids: &Mutex<BTreeMap<PaneId, Arc<Mutex<PaneGrid>>>>,
    subscribers: &Mutex<BTreeMap<PaneId, PaneSubscribers>>,
    proof: AllPanesExited,
) -> bool {
    let sid = proof.session();
    let panes_to_detach: Vec<PaneId> = {
        let mut r = registry.write();
        // Re-witness under the write lock: the caller's proof was taken
        // outside it and a racing `new_window` / `split_pane` /
        // `kill_session` invalidates it.
        if r.sessions
            .get(&sid)
            .and_then(AllPanesExited::witness)
            .is_some()
        {
            r.sessions
                .remove(&sid)
                .map(|s| s.panes.keys().copied().collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    if panes_to_detach.is_empty() {
        return false;
    }
    let detached: Vec<PtyHandle> = {
        let mut ptys = ptys.lock();
        let mut grids = grids.lock();
        let mut subs = subscribers.lock();
        panes_to_detach
            .iter()
            .filter_map(|p| {
                grids.remove(p);
                subs.remove(p);
                ptys.remove(p)
            })
            .collect()
    };
    drop(detached);
    info!(session = %sid, "tear-core: reaped fully-exited unwatched session");
    true
}

impl MultiplexerControl for InProcess {
    fn list_sessions(&self) -> ControlResult<Vec<TearSession>> {
        Ok(self.registry.read().sessions_in_order())
    }

    fn get_session(&self, id: SessionId) -> ControlResult<TearSession> {
        self.registry
            .read()
            .sessions
            .get(&id)
            .cloned()
            .ok_or(ControlError::NoSuchSession(id))
    }

    fn get_window(&self, id: WindowId) -> ControlResult<(SessionId, TearWindow)> {
        let r = self.registry.read();
        for s in r.sessions.values() {
            if let Some(w) = s.windows.get(&id) {
                return Ok((s.id, w.clone()));
            }
        }
        Err(ControlError::NoSuchWindow(id))
    }

    fn get_pane(&self, id: PaneId) -> ControlResult<TearPane> {
        let r = self.registry.read();
        for s in r.sessions.values() {
            if let Some(p) = s.panes.get(&id) {
                return Ok(p.clone());
            }
        }
        Err(ControlError::NoSuchPane(id))
    }

    fn new_session_with_source_and_size(
        &self,
        name: &str,
        shell: &str,
        args: &[String],
        source: tear_types::SessionSource,
        size_cells: (u16, u16),
    ) -> ControlResult<SessionId> {
        let size = (size_cells.0.max(1), size_cells.1.max(1));
        // The embedder's cwd projection is the same value
        // `spawn_pty_for` will apply, so recording it here keeps the
        // typed pane record and the real child in agreement.
        let cwd = self.spawn_env.read().cwd.clone();
        let mut r = self.registry.write();
        let sid = r.create_session(name);
        // Stamp provenance on the typed session entry. The
        // registry.create_session built it with Source::default()
        // (Human); overwrite when the caller asked for something
        // else.
        if let Some(s) = r.sessions.get_mut(&sid) {
            s.source = source.clone();
        }
        let Some((_wid, pane_id)) =
            r.add_window(sid, "main", shell, args, cwd.as_deref(), &[], size)
        else {
            return Err(ControlError::Internal(anyhow::anyhow!(
                "registry.add_window returned None after fresh create_session"
            )));
        };
        drop(r); // release write lock before spawning PTY
        if let Err(e) = self.spawn_pty_for(pane_id, shell, args, size) {
            // Roll back the session — registry is small, easier to
            // remove than to leave a sessionless typed entry.
            self.registry.write().sessions.remove(&sid);
            return Err(ControlError::Internal(e));
        }
        info!(
            session = %sid,
            name,
            shell,
            source = %source.label(),
            cols = size.0,
            rows = size.1,
            "tear-core: new session"
        );
        Ok(sid)
    }

    fn rename_session(&self, id: SessionId, new_name: &str) -> ControlResult<()> {
        let mut r = self.registry.write();
        let s = r.sessions.get_mut(&id).ok_or(ControlError::NoSuchSession(id))?;
        s.name = new_name.into();
        Ok(())
    }

    fn kill_session(&self, id: SessionId) -> ControlResult<()> {
        let panes_to_kill: Vec<PaneId> = {
            let r = self.registry.read();
            let s = r.sessions.get(&id).ok_or(ControlError::NoSuchSession(id))?;
            s.panes.keys().copied().collect()
        };
        // Pull the runtime artifacts out under the locks…
        let detached = self.detach_panes(&panes_to_kill);
        self.registry.write().sessions.remove(&id);
        // …and kill + reap the PTY children with NO InProcess lock
        // held (detach_panes' deadlock contract).
        drop(detached);
        info!(session = %id, "tear-core: killed session");
        Ok(())
    }

    fn new_window(
        &self,
        session: SessionId,
        name: &str,
        shell: &str,
        args: &[String],
    ) -> ControlResult<WindowId> {
        let size = (80, 24);
        let cwd = self.spawn_env.read().cwd.clone();
        let (wid, pid) = {
            let mut r = self.registry.write();
            r.add_window(session, name, shell, args, cwd.as_deref(), &[], size)
                .ok_or(ControlError::NoSuchSession(session))?
        };
        if let Err(e) = self.spawn_pty_for(pid, shell, args, size) {
            return Err(ControlError::Internal(e));
        }
        info!(session = %session, window = %wid, name, "tear-core: new window");
        Ok(wid)
    }

    fn kill_window(&self, id: WindowId) -> ControlResult<()> {
        let panes_to_kill: Vec<PaneId> = {
            let r = self.registry.read();
            let mut out = Vec::new();
            for s in r.sessions.values() {
                if let Some(w) = s.windows.get(&id) {
                    out.extend(w.layout.panes());
                    break;
                }
            }
            if out.is_empty() {
                return Err(ControlError::NoSuchWindow(id));
            }
            out
        };
        // Same shape as kill_session: artifacts leave the maps under
        // the locks, handles die only after every lock is released
        // (detach_panes' deadlock contract).
        let detached = self.detach_panes(&panes_to_kill);
        {
            let mut r = self.registry.write();
            for s in r.sessions.values_mut() {
                if s.windows.remove(&id).is_some() {
                    for p in &panes_to_kill {
                        s.panes.remove(p);
                    }
                    // Retarget focus off the removed window, else
                    // active_window dangles and the s.windows[&active_window]
                    // index sites panic.
                    if s.active_window == id {
                        s.active_window =
                            s.windows.keys().next().copied().unwrap_or(WindowId::NULL);
                    }
                    break;
                }
            }
        }
        drop(detached);
        info!(window = %id, "tear-core: killed window");
        Ok(())
    }

    fn select_window(&self, id: WindowId) -> ControlResult<()> {
        let mut r = self.registry.write();
        for s in r.sessions.values_mut() {
            if s.windows.contains_key(&id) {
                s.active_window = id;
                return Ok(());
            }
        }
        Err(ControlError::NoSuchWindow(id))
    }

    fn split_pane(
        &self,
        origin: PaneId,
        direction: Direction,
        shell: &str,
        args: &[String],
    ) -> ControlResult<PaneId> {
        // Correct split: replace ONLY the origin leaf in the window's
        // layout tree with a balanced split (origin, new). Every other
        // pane keeps its slot — no whole-window re-wrap. Geometry is
        // reflowed from the tree afterwards (apply_layout_geometry).
        let (sid, wid) = self
            .registry
            .read()
            .locate_pane(origin)
            .ok_or(ControlError::NoSuchPane(origin))?;
        // Placeholder spawn size; apply_layout_geometry SIGWINCHes the
        // real per-pane geometry right after the PTY is up.
        let size = (80, 12);
        let cwd = self.spawn_env.read().cwd.clone();
        let pid = {
            let mut r = self.registry.write();
            let Some(s) = r.sessions.get_mut(&sid) else {
                return Err(ControlError::NoSuchSession(sid));
            };
            let new_pid = crate::registry::mint_pane_id(wid, shell);
            s.panes.insert(
                new_pid,
                TearPane {
                    id: new_pid,
                    shell: shell.into(),
                    args: args.to_vec(),
                    cwd: cwd.clone(),
                    env: vec![],
                    size_cells: size,
                    origin_cells: (0, 0),
                    state: tear_types::PaneState::Running,
                    title: shell.into(),
                    input_policy: tear_types::InputPolicy::default(),
                },
            );
            // Split the matched leaf only. If `origin` isn't in this
            // window's tree (it should be — we just located it), roll
            // back the pane we inserted so we never leave an orphan.
            let split_ok = s.windows.get_mut(&wid).is_some_and(|w| {
                let ok = w.layout.split_leaf(origin, new_pid, direction, 0.5);
                if ok {
                    w.active_pane = new_pid;
                }
                ok
            });
            if !split_ok {
                s.panes.remove(&new_pid);
                return Err(ControlError::NoSuchPane(origin));
            }
            new_pid
        };
        self.spawn_pty_for(pid, shell, args, size)
            .map_err(ControlError::Internal)?;
        self.apply_layout_geometry(sid, wid);
        info!(pane = %pid, "tear-core: split pane");
        Ok(pid)
    }

    fn kill_pane(&self, id: PaneId) -> ControlResult<()> {
        // NOTE pre-fix this was `self.ptys.lock().remove(&id);` — the
        // returned PtyHandle was a temporary dropped BEFORE the lock
        // guard (reverse creation order), i.e. the kill + reap ran
        // with the ptys lock held. Same wedge class as kill_session;
        // same cure: detach under the locks, drop outside them.
        let Some((sid, wid)) = self.registry.read().locate_pane(id) else {
            return Err(ControlError::NoSuchPane(id));
        };
        let detached = self.detach_panes(&[id]);
        // true → pane removed from a multi-pane window, geometry needs a
        // reflow; false → the window collapsed (was its last pane) so
        // there's nothing left to reflow.
        let needs_reflow = {
            let mut r = self.registry.write();
            let Some(s) = r.sessions.get_mut(&sid) else {
                drop(detached);
                return Err(ControlError::NoSuchSession(sid));
            };
            let outcome = match s.windows.get_mut(&wid) {
                Some(w) => w.layout.remove_leaf(id),
                None => LeafRemoval::NotFound,
            };
            match outcome {
                // NotFound means `locate_pane` said this pane was in BOTH
                // `s.panes` and the window's tree, and then `remove_leaf`
                // could not find it in the tree — i.e. the two had already
                // diverged. That is structural corruption, and it used to
                // share an arm with the normal close: the record was
                // silently dropped and the caller got Ok(()), so a real bug
                // was indistinguishable from a routine kill.
                //
                // Roll the record back in and refuse. NoSuchPane already
                // exists and already round-trips over the wire, so this
                // costs no new type and no wire churn.
                LeafRemoval::NotFound => {
                    drop(detached);
                    return Err(ControlError::NoSuchPane(id));
                }
                // Parent split collapsed into the sibling — the flat pane
                // record goes too, and active_pane retargets if it pointed
                // at the dead pane.
                LeafRemoval::Removed => {
                    s.panes.remove(&id);
                    if let Some(w) = s.windows.get_mut(&wid) {
                        if w.active_pane == id {
                            w.active_pane =
                                w.layout.panes().first().copied().unwrap_or(PaneId::NULL);
                        }
                    }
                    true
                }
                // The window's only pane — nothing the tree can represent
                // remains, so the whole window closes (tmux semantics).
                // Retarget active_window if it pointed here, else it would
                // dangle at a removed id and panic the
                // s.windows[&active_window] index sites.
                LeafRemoval::WasRoot => {
                    s.panes.remove(&id);
                    s.windows.remove(&wid);
                    if s.active_window == wid {
                        s.active_window = s.windows.keys().next().copied().unwrap_or(WindowId::NULL);
                    }
                    false
                }
            }
        };
        drop(detached);
        if needs_reflow {
            self.apply_layout_geometry(sid, wid);
        }
        Ok(())
    }

    fn select_pane(&self, id: PaneId) -> ControlResult<()> {
        let (sid, wid) = self
            .registry
            .read()
            .locate_pane(id)
            .ok_or(ControlError::NoSuchPane(id))?;
        let mut r = self.registry.write();
        if let Some(s) = r.sessions.get_mut(&sid) {
            if let Some(w) = s.windows.get_mut(&wid) {
                w.active_pane = id;
            }
        }
        Ok(())
    }

    fn resize_pane(
        &self,
        id: PaneId,
        direction: Direction,
        delta_cells: i16,
    ) -> ControlResult<()> {
        // Slide the divider of the split governing `id` along `direction`.
        // delta_cells is converted to a fraction of the window's span on
        // the relevant axis, then resize_leaf clamps + applies it. If the
        // pane has no governing split that way (single pane / wrong axis)
        // it's a no-op — still Ok, like tmux.
        let (sid, wid) = self
            .registry
            .read()
            .locate_pane(id)
            .ok_or(ControlError::NoSuchPane(id))?;
        {
            let mut r = self.registry.write();
            let Some(s) = r.sessions.get_mut(&sid) else {
                return Err(ControlError::NoSuchSession(sid));
            };
            let Some(w) = s.windows.get_mut(&wid) else {
                return Err(ControlError::NoSuchWindow(wid));
            };
            let span = match direction.orientation() {
                SplitOrientation::Vertical => w.size_cells.0,
                SplitOrientation::Horizontal => w.size_cells.1,
            };
            let delta_frac = if span > 0 {
                f32::from(delta_cells) / f32::from(span)
            } else {
                0.0
            };
            w.layout.resize_leaf(id, direction, delta_frac);
        }
        self.apply_layout_geometry(sid, wid);
        Ok(())
    }

    fn apply_layout(&self, window: WindowId, kind: LayoutKind) -> ControlResult<()> {
        // Locate the window's session under a read lock; release it before
        // taking the write lock (the lock discipline this file keeps).
        let sid = {
            let r = self.registry.read();
            r.sessions
                .iter()
                .find(|(_, s)| s.windows.contains_key(&window))
                .map(|(id, _)| *id)
        }
        .ok_or(ControlError::NoSuchWindow(window))?;
        // Re-arrange the window's existing panes (display order) into the
        // named layout. Panes keep their PTYs — only the tree changes.
        // Custom / empty → from_kind returns None → no-op.
        let applied = {
            let mut r = self.registry.write();
            let Some(s) = r.sessions.get_mut(&sid) else {
                return Err(ControlError::NoSuchWindow(window));
            };
            let Some(w) = s.windows.get_mut(&window) else {
                return Err(ControlError::NoSuchWindow(window));
            };
            match LayoutNode::from_kind(kind, &w.layout.panes()) {
                Some(tree) => {
                    w.layout = tree;
                    true
                }
                None => false,
            }
        };
        if applied {
            self.apply_layout_geometry(sid, window);
        }
        info!(window = %window, ?kind, applied, "tear-core: apply layout");
        Ok(())
    }

    fn send_keys(&self, id: PaneId, bytes: &[u8]) -> ControlResult<()> {
        // Input-policy gate (#2).
        //
        // - Locked: always reject — operator-explicit "no input
        //   now". Surfaced before we touch the PTY so a Locked
        //   pane never writes a partial frame.
        // - Leader: identity-gating semantics are enforced ONE
        //   layer up by the daemon's serve_connection_with_auth
        //   path (which carries per-connection client_id). The
        //   in-process trait surface has no client identity, so
        //   Leader is treated as Free here — once the daemon's
        //   gate authorises a SendKeys, this layer accepts. Pure
        //   in-process consumers (mado tier-3) that need Leader
        //   semantics must gate at their own layer.
        {
            let r = self.registry.read();
            let Some((sid, _wid)) = r.locate_pane(id) else {
                return Err(ControlError::NoSuchPane(id));
            };
            let Some(pane) = r.sessions.get(&sid).and_then(|s| s.panes.get(&id)) else {
                return Err(ControlError::NoSuchPane(id));
            };
            if matches!(pane.input_policy, tear_types::InputPolicy::Locked) {
                return Err(ControlError::Rejected(format!(
                    "pane {id} input_policy=locked — send_keys rejected"
                )));
            }
        }
        let ptys = self.ptys.lock();
        let pty = ptys.get(&id).ok_or(ControlError::NoSuchPane(id))?;
        pty.write(bytes)
            .map_err(|e| ControlError::Transport(e.to_string()))?;
        Ok(())
    }

    fn pane_subscriber_count(&self, id: PaneId) -> ControlResult<u32> {
        // Mirrors the byte-stream fan-out path: subscribers are
        // indexed per-pane in InProcess.subscribers. Counting +
        // sender liveness check (try_send via a synthetic
        // empty-byte cycle would be too heavy) means we just
        // report the slot length — a sender drops naturally on
        // next broadcast if dead, so the count is an upper bound.
        let count = self
            .subscribers
            .lock()
            .get(&id)
            .map(|ps| ps.senders.len() as u32)
            .unwrap_or(0);
        Ok(count)
    }

    fn set_input_policy(
        &self,
        id: PaneId,
        policy: tear_types::InputPolicy,
    ) -> ControlResult<()> {
        let mut r = self.registry.write();
        // Walk the session→panes maps to find the target. Mirrors
        // locate_pane's address logic but with mutable access.
        for s in r.sessions.values_mut() {
            if let Some(p) = s.panes.get_mut(&id) {
                p.input_policy = policy;
                return Ok(());
            }
        }
        Err(ControlError::NoSuchPane(id))
    }

    /// Phase-2 override of the trait's default `pane_snapshot`.
    /// Delegates to the inherent [`Self::pane_snapshot`] method,
    /// which reads the per-pane `PaneGrid` installed by
    /// [`Self::spawn_pty_for`].
    fn pane_snapshot(&self, id: PaneId) -> ControlResult<tear_types::PaneSnapshot> {
        InProcess::pane_snapshot(self, id)
    }

    /// Override the trait default with the no-alloc lookup —
    /// mado's input loop calls this per keystroke, so the
    /// fast path matters here.
    fn pane_cursor_keys_mode(&self, id: PaneId) -> ControlResult<bool> {
        InProcess::pane_cursor_keys_mode(self, id)
    }

    /// Phase-3.1 override — resize the underlying PTY (fires
    /// SIGWINCH at the child) AND resize the per-pane PaneGrid
    /// so subsequent snapshots reflect the new geometry.
    fn pane_resize_absolute(
        &self,
        id: PaneId,
        cols: u16,
        rows: u16,
    ) -> ControlResult<()> {
        use portable_pty::PtySize;
        let pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        // Resize the PTY (delivers SIGWINCH to child).
        {
            let ptys = self.ptys.lock();
            let pty = ptys.get(&id).ok_or(ControlError::NoSuchPane(id))?;
            pty.resize(pty_size)
                .map_err(|e| ControlError::Internal(anyhow::anyhow!(e)))?;
        }
        // Resize the parser-backed grid so subsequent snapshots
        // honour the new geometry.
        if let Some(grid) = self.grids.lock().get(&id).cloned() {
            grid.lock().resize(cols as usize, rows as usize);
        }
        Ok(())
    }
}

/// Inherent helpers that aren't part of the [`MultiplexerControl`] trait
/// surface — internal reflow + geometry plumbing the trait methods call.
impl InProcess {
    /// Reflow a window's panes from its [`LayoutNode`] tree — the single
    /// geometry source. Computes every pane's rect against the window's
    /// cell size, writes the cell size/origin onto each flat pane record,
    /// and SIGWINCHes each PTY to match. `split_pane`/`kill_pane`/
    /// `resize_pane` all call this after mutating the tree, so what a
    /// window displays always matches its layout. Best-effort: a pane
    /// squeezed to zero cells (too-small window) or whose PTY hasn't
    /// spawned yet is skipped, never an error.
    fn apply_layout_geometry(&self, sid: SessionId, wid: WindowId) {
        // Phase 1 — under the registry lock, recompute rects + stamp them
        // onto the pane records. We collect the rect set to apply to PTYs
        // AFTER dropping the lock (pane_resize_absolute takes ptys/grids
        // locks; holding registry across that is the wedge class we avoid
        // elsewhere in this file).
        let rects = {
            let mut r = self.registry.write();
            let Some(s) = r.sessions.get_mut(&sid) else {
                return;
            };
            let rects = match s.windows.get(&wid) {
                Some(w) => w
                    .layout
                    .compute_rects(Rect::sized(w.size_cells.0, w.size_cells.1)),
                None => return,
            };
            for (pane, rect) in &rects {
                if let Some(p) = s.panes.get_mut(pane) {
                    p.size_cells = (rect.w.max(1), rect.h.max(1));
                    p.origin_cells = (rect.x, rect.y);
                }
            }
            rects
        };
        // Phase 2 — SIGWINCH each PTY to its new geometry.
        for (pane, rect) in rects {
            if rect.w == 0 || rect.h == 0 {
                continue;
            }
            let _ = self.pane_resize_absolute(pane, rect.w, rect.h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_inproc_starts_empty() {
        let inproc = InProcess::new();
        let sessions = inproc.list_sessions().unwrap();
        assert!(sessions.is_empty());
    }

    /// A diverged tree is refused rather than silently healed.
    ///
    /// ★ READ THIS BEFORE TRUSTING IT — it does NOT prove what its first
    /// draft claimed. `kill_pane`'s `LeafRemoval::NotFound` arm was split out
    /// from the ordinary `Removed` close (they shared an arm, so structural
    /// corruption was indistinguishable from a routine kill). This test was
    /// written to earn that split, and MEASURED VACUOUS against it: it passes
    /// identically with the old merged arm restored.
    ///
    /// The reason is `locate_pane` (registry.rs), which requires membership
    /// in BOTH `s.panes` and the window's tree and runs at inproc.rs's read
    /// lock *before* `remove_leaf` is ever called. So a diverged pane is
    /// rejected there, and the refusal this asserts comes from that guard.
    ///
    /// Consequence, stated honestly: **`LeafRemoval::NotFound` is unreachable
    /// from a single-threaded caller.** It is a guard on the TOCTOU window
    /// between the read lock that resolves the pane and the write lock that
    /// mutates the tree — real under concurrency, and not reachable by any
    /// test that does not race those two locks. The arm split is kept as
    /// defence in depth, and is graded accordingly: **only-mitigated, ceiling
    /// = no single-threaded test can reach it; earning a green here needs a
    /// concurrent test that interleaves the two lock acquisitions.**
    ///
    /// What this test DOES pin, and it is worth pinning: `locate_pane`'s
    /// both-memberships requirement is load-bearing, and a refactor that
    /// relaxed it to a single lookup would make this go red.
    #[test]
    fn kill_pane_refuses_when_tree_and_pane_map_have_diverged() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("divergence", "/bin/sh").unwrap();
        let pane = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();

        // Break the invariant behind the API's back: evict the pane from the
        // layout tree while leaving its record in `s.panes`, so `locate_pane`
        // still resolves it but `remove_leaf` cannot find it.
        {
            let mut r = inproc.registry.write();
            let s = r.sessions.get_mut(&sid).unwrap();
            let wid = *s.windows.keys().next().unwrap();
            let w = s.windows.get_mut(&wid).unwrap();
            w.layout = tear_types::LayoutNode::leaf(tear_types::PaneId::from_seed("decoy"));
        }

        let err = inproc
            .kill_pane(pane)
            .expect_err("a diverged tree must be refused, not silently healed");
        assert!(
            matches!(err, ControlError::NoSuchPane(p) if p == pane),
            "expected NoSuchPane for the diverged pane, got {err:?}"
        );
    }

    /// Forcing function: the TEAR_* env-var names tear stamps onto every
    /// spawned pane come from the typed cross-tool contract
    /// (`ishou_tokens::FleetStateVar`), which seki's prompt reads from the
    /// same source. Pinning the variant→name mapping here makes a rename
    /// on the producer side a compile+test failure on the single source of
    /// truth, so it can never silently drift from the consumer.
    #[test]
    fn pane_env_var_names_come_from_fleet_state_contract() {
        use ishou_tokens::FleetStateVar;
        assert_eq!(FleetStateVar::TearSessionId.name(), "TEAR_SESSION_ID");
        assert_eq!(FleetStateVar::TearSessionName.name(), "TEAR_SESSION_NAME");
        assert_eq!(FleetStateVar::TearPaneId.name(), "TEAR_PANE_ID");
        assert_eq!(FleetStateVar::TearSocket.name(), "TEAR_SOCKET");
    }

    #[test]
    fn pty_env_path_includes_nix_profile_dirs() {
        // Reproduces the production bug where the launchd-spawned
        // tear-daemon inherited PATH = "/usr/bin:/bin:/usr/sbin:
        // /sbin" — every shell tear spawned then couldn't find
        // `tear` (or any home-manager binary), and starship's
        // [custom.tear] prompt block hung trying to invoke it.
        // The fix prepends /etc/profiles/per-user/$USER/bin +
        // ~/.nix-profile/bin + /run/current-system/sw/bin so
        // home-manager binaries resolve.
        let inproc = Arc::new(InProcess::new());
        let sid = inproc.new_session("path-test", "/bin/sh").unwrap();
        let pane = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        inproc.subscribers.lock().entry(pane).or_default().senders.push(tx);

        inproc.send_keys(pane, b"printf 'PATH=[%s]\\n' \"$PATH\"\n").expect("send_keys");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut buf = Vec::<u8>::new();
        while std::time::Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                buf.extend_from_slice(&chunk);
                if let Ok(s) = std::str::from_utf8(&buf) {
                    if s.contains("PATH=[") && s.contains("]\n") { break; }
                }
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("PATH=["), "no PATH output: {text:?}");
        assert!(
            text.contains("/etc/profiles/per-user/")
                || text.contains("/.nix-profile/bin")
                || text.contains("/run/current-system/sw/bin"),
            "PATH missing Nix profile dirs — home-manager binaries (tear, starship, etc.) wouldn't resolve: {text:?}"
        );
    }

    #[test]
    fn pty_env_provides_term_default_when_parent_lacks_it() {
        // Reproduces the production bug where launchd-spawned
        // tear-daemons had no TERM in their env, so every shell
        // they spawned reported "TERM environment variable not
        // set" and `clear`/arrow keys broke. We can't perfectly
        // simulate the launchd-clean env in-process, but we can
        // assert TERM is always non-empty in the spawned child.
        let inproc = Arc::new(InProcess::new());
        let sid = inproc.new_session("term-test", "/bin/sh").unwrap();
        let pane = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        inproc.subscribers.lock().entry(pane).or_default().senders.push(tx);

        inproc
            .send_keys(pane, b"printf 'TERM=[%s]\\n' \"${TERM:-MISSING}\"\n")
            .expect("send_keys");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut buf = Vec::<u8>::new();
        while std::time::Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                buf.extend_from_slice(&chunk);
                if let Ok(s) = std::str::from_utf8(&buf) {
                    if s.contains("TERM=[") && s.contains("]\n") {
                        break;
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("TERM=["), "no TERM output: {text:?}");
        assert!(
            !text.contains("TERM=[MISSING]"),
            "TERM unset in child shell — terminfo would fail: {text:?}"
        );
    }

    #[test]
    fn pty_env_includes_tear_session_pane_socket_vars() {
        // Spawn a fresh shell that prints the TEAR_* env vars + a
        // sentinel string, then subscribe to its bytes and assert
        // we see the sentinel + the env values land. Proves the
        // spawn_pty_for env injection works end-to-end (the
        // shell's child PROCESS observes them).
        let inproc = Arc::new(InProcess::new());
        inproc.set_socket_path(std::path::PathBuf::from("/tmp/tear-test-env.sock"));

        let sid = inproc
            .new_session("env-test", "/bin/sh")
            .expect("new_session");
        let pane = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        inproc.subscribers.lock().entry(pane).or_default().senders.push(tx);

        inproc
            .send_keys(
                pane,
                b"printf 'SENTINEL[T=%s][S=%s][P=%s][SOCK=%s]\\n' \"${TEAR}\" \"${TEAR_SESSION_NAME}\" \"${TEAR_PANE_ID}\" \"${TEAR_SOCKET}\"\n",
            )
            .expect("send_keys");

        // Collect output for up to 2 seconds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut buf = Vec::<u8>::new();
        while std::time::Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                buf.extend_from_slice(&chunk);
                if let Ok(s) = std::str::from_utf8(&buf) {
                    if s.contains("SENTINEL[") && s.contains(']') {
                        // Wait briefly for the rest of the line to arrive.
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        while let Ok(more) = rx.try_recv() {
                            buf.extend_from_slice(&more);
                        }
                        break;
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("SENTINEL["), "no sentinel in output: {text:?}");
        assert!(text.contains("T=1"), "TEAR=1 not present: {text:?}");
        assert!(text.contains("S=env-test"), "TEAR_SESSION_NAME wrong: {text:?}");
        assert!(text.contains("SOCK=/tmp/tear-test-env.sock"), "TEAR_SOCKET wrong: {text:?}");
    }

    /// **`SpawnEnv` override reaches the child + wins over the
    /// fallback** (operator report 2026-06-12: vim grey + wrong font in
    /// the embedded-tear window came from the embedded path stamping only
    /// xterm-256color). An embedder (mado) sets a `SpawnEnv` whose
    /// `TERM` override + `COLORTERM` must land on the child's env,
    /// overriding the conservative fallback `spawn_pty_for` would
    /// otherwise stamp. PTY-gated (openpty); passes in isolation.
    #[test]
    fn spawn_env_override_reaches_child_and_wins_over_fallback() {
        let inproc = Arc::new(InProcess::new());
        // The embedder's capability projection: a richer TERM than the
        // xterm-256color fallback + the truecolor signal vim needs.
        inproc.set_spawn_env(tear_types::SpawnEnv::from_overrides(vec![
            ("TERM".into(), "xterm-ghostty".into()),
            ("COLORTERM".into(), "truecolor".into()),
        ]));
        let sid = inproc.new_session("spawnenv-test", "/bin/sh").unwrap();
        let pane = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        inproc.subscribers.lock().entry(pane).or_default().senders.push(tx);
        inproc
            .send_keys(
                pane,
                b"printf 'SENV[T=%s][C=%s]\\n' \"${TERM}\" \"${COLORTERM}\"\n",
            )
            .expect("send_keys");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut buf = Vec::<u8>::new();
        while std::time::Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                buf.extend_from_slice(&chunk);
                if let Ok(s) = std::str::from_utf8(&buf) {
                    if s.contains("SENV[") && s.contains(']') {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        while let Ok(more) = rx.try_recv() {
                            buf.extend_from_slice(&more);
                        }
                        break;
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("SENV["), "no sentinel in output: {text:?}");
        assert!(
            text.contains("T=xterm-ghostty"),
            "embedder TERM override did not reach the child (fallback won): {text:?}"
        );
        assert!(
            text.contains("C=truecolor"),
            "embedder COLORTERM override did not reach the child: {text:?}"
        );
    }

    #[test]
    fn get_nonexistent_session_errors() {
        let inproc = InProcess::new();
        let err = inproc.get_session(SessionId(99)).unwrap_err();
        assert!(matches!(err, ControlError::NoSuchSession(_)));
    }

    #[test]
    fn subscribe_pane_bytes_on_nonexistent_pane_returns_nosuch() {
        let inproc = InProcess::new();
        let pane = PaneId::from_seed("phantom");
        let err = inproc.subscribe_pane_bytes(pane).unwrap_err();
        assert!(matches!(err, ControlError::NoSuchPane(p) if p == pane));
    }

    #[test]
    fn child_exit_disconnects_subscribers_and_marks_pane_exited() {
        // Regression (mado embedded-tear "typing `exit` does nothing"):
        // when the shell exits, the per-pane byte channel MUST
        // disconnect so a single-pane GUI learns the child is gone and
        // can close its window. Before the fix the PTY reader thread
        // just ended on EOF, leaving every engate/daemon Receiver
        // blocked forever on a phantom-Running pane whose senders were
        // never dropped.
        let inproc = Arc::new(InProcess::new());
        let sid = inproc
            .new_session("exit-test", "/bin/sh")
            .expect("new_session");
        let pane = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();

        // Subscribe while the pane is alive (mirrors mado's attach).
        let rx = inproc.subscribe_pane_bytes(pane).expect("subscribe");

        // Drive the shell to exit with a specific code.
        inproc.send_keys(pane, b"exit 7\n").expect("send_keys");

        // The receiver must eventually disconnect — that Err is the
        // end-of-stream signal engate's run() / the daemon's
        // serve_subscription block on.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut disconnected = false;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(_) => continue, // drain echoed input / shell output
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        assert!(
            disconnected,
            "subscriber channel never disconnected after `exit` — a single-pane GUI would hang open"
        );

        // The pane must be modeled as Exited (remain-on-exit) with the
        // child's real exit code propagated.
        let session = inproc.get_session(sid).unwrap();
        let state = session.panes.get(&pane).map(|p| p.state);
        assert_eq!(
            state,
            Some(tear_types::PaneState::Exited { code: 7 }),
            "pane should be Exited{{ code: 7 }}, got {state:?}"
        );

        // A subscribe AFTER exit must return an already-disconnected
        // receiver, never a live registration that would block forever.
        let rx2 = inproc
            .subscribe_pane_bytes(pane)
            .expect("subscribe after exit still resolves (remain-on-exit pane)");
        assert!(
            matches!(rx2.recv(), Err(mpsc::RecvError)),
            "post-exit subscribe must yield an immediately-disconnected receiver"
        );
    }

    #[test]
    fn fully_exited_unwatched_session_is_reaped() {
        // Session-leak fix (2026-07-06): a session whose shell exits
        // with NO subscriber ever attached (agent spawn-and-walk-away,
        // one-shot command) must leave the registry entirely — before
        // the fix it lingered forever as a ghost row in mado's Ctrl-S
        // picker.
        let inproc = Arc::new(InProcess::new());
        let sid = inproc
            .new_session("reap-test", "/bin/sh")
            .expect("new_session");
        let pane = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();

        // No subscriber attaches. Exit the shell.
        inproc.send_keys(pane, b"exit\n").expect("send_keys");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if inproc.list_sessions().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            inproc.list_sessions().unwrap().is_empty(),
            "fully-exited unwatched session was not reaped from the registry"
        );
        // Runtime artifacts must be gone too — a reaped session that
        // strands a PtyHandle/grid/subscriber entry is a slow leak.
        assert!(inproc.ptys.lock().is_empty(), "reap left a PtyHandle behind");
        assert!(inproc.grids.lock().is_empty(), "reap left a grid behind");
        assert!(
            inproc.subscribers.lock().is_empty(),
            "reap left a subscriber entry behind"
        );
    }

    #[test]
    fn watched_session_survives_exit_with_remain_on_exit() {
        // The conservative half of the reap: a session SOMEONE was
        // watching (live sender at exit time — a mado window, a
        // recorder) keeps tmux remain-on-exit semantics: pane marked
        // Exited, final grid inspectable, session still listed.
        let inproc = Arc::new(InProcess::new());
        let sid = inproc
            .new_session("remain-test", "/bin/sh")
            .expect("new_session");
        let pane = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();

        let rx = inproc.subscribe_pane_bytes(pane).expect("subscribe");
        inproc.send_keys(pane, b"exit\n").expect("send_keys");

        // Wait for the end-of-stream disconnect (fires after the exit
        // handler ran its reap decision with `watched = true`).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut disconnected = false;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        assert!(disconnected, "subscriber never disconnected after exit");

        let sessions = inproc.list_sessions().unwrap();
        assert_eq!(
            sessions.len(),
            1,
            "watched session must remain-on-exit, not be reaped"
        );
        let state = sessions[0].panes.get(&pane).map(|p| p.state);
        assert!(
            matches!(state, Some(tear_types::PaneState::Exited { .. })),
            "pane should be Exited, got {state:?}"
        );
    }

    #[test]
    fn pane_snapshot_on_nonexistent_pane_returns_nosuch() {
        let inproc = InProcess::new();
        let pane = PaneId::from_seed("phantom");
        let err = inproc.pane_snapshot(pane).unwrap_err();
        assert!(matches!(err, ControlError::NoSuchPane(p) if p == pane));
    }

    #[test]
    fn pane_resize_absolute_on_nonexistent_pane_returns_nosuch() {
        let inproc = InProcess::new();
        let pane = PaneId::from_seed("phantom");
        let err = inproc.pane_resize_absolute(pane, 80, 24).unwrap_err();
        assert!(matches!(err, ControlError::NoSuchPane(p) if p == pane));
    }

    #[test]
    fn rename_session_to_same_name_is_idempotent() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("work", "/bin/sh").unwrap();
        // Rename to current name should succeed without error.
        inproc.rename_session(sid, "work").unwrap();
        let s = inproc.get_session(sid).unwrap();
        assert_eq!(s.name, "work");
        // Now rename to a different name.
        inproc.rename_session(sid, "play").unwrap();
        let s2 = inproc.get_session(sid).unwrap();
        assert_eq!(s2.name, "play");
    }

    #[test]
    fn kill_session_with_active_subscriber_returns_promptly() {
        // Regression (mado L1 teardown wedge, 2026-06-10): kill_session
        // used to drop PtyHandles INSIDE the ptys/grids/subscribers
        // lock scope; PtyHandle::Drop then block-waited on the child
        // while the pane's reader thread sat blocked acquiring the
        // same subscribers lock from on_bytes — mutual wait, observed
        // as a 20+ minute wedge. Post-fix the handles leave the maps
        // under the locks but die outside them, and the reap itself is
        // bounded (pty::reap_with_deadline).
        //
        // /bin/cat echoes everything, so a feeder thread keeps the
        // reader thread hot (contending the subscribers lock) while
        // another thread kills the session. The watchdog channel turns
        // a re-introduced deadlock into a <5s test failure instead of
        // a hung suite.
        let inproc = Arc::new(InProcess::new());
        let sid = inproc
            .new_session("kill-fast", "/bin/cat")
            .expect("new_session");
        let pane = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();

        // Active subscriber (mirrors mado's attach_live).
        let rx = inproc.subscribe_pane_bytes(pane).expect("subscribe");

        // Feeder — keeps bytes flowing through on_bytes so the reader
        // thread is actively taking the subscribers lock during kill.
        let feeder_inproc = Arc::clone(&inproc);
        let feeding = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let feeding_for_thread = Arc::clone(&feeding);
        let feeder = std::thread::spawn(move || {
            let chunk = vec![b'x'; 4096];
            while feeding_for_thread.load(std::sync::atomic::Ordering::Relaxed) {
                if feeder_inproc.send_keys(pane, &chunk).is_err() {
                    break; // pane gone — the kill landed
                }
            }
        });
        // Drain one echo so we know the reader thread is live.
        let _ = rx.recv_timeout(std::time::Duration::from_secs(2));

        // kill_session on a helper thread + watchdog recv.
        let killer_inproc = Arc::clone(&inproc);
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let result = killer_inproc.kill_session(sid);
            let _ = done_tx.send((result, started.elapsed()));
        });
        let (result, elapsed) = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("kill_session deadlocked — did not return within 5s");
        feeding.store(false, std::sync::atomic::Ordering::Relaxed);
        result.expect("kill_session errored");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "kill_session took {elapsed:?} with an active subscriber"
        );
        let _ = feeder.join();

        // The subscriber must observe end-of-stream (senders dropped by
        // detach_panes), never block forever on a dead pane.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut disconnected = false;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(_) => continue, // drain buffered echo
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        assert!(
            disconnected,
            "subscriber channel never disconnected after kill_session"
        );
    }

    #[test]
    fn new_session_then_kill_then_subscribe_returns_nosuch() {
        // The kill-session path must prune subscribers + grids + ptys
        // so a later subscribe to the dead pane errors cleanly.
        let inproc = InProcess::new();
        let sid = inproc.new_session("temp", "/bin/sh").unwrap();
        let pane = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();
        inproc.kill_session(sid).unwrap();
        let err = inproc.subscribe_pane_bytes(pane).unwrap_err();
        assert!(matches!(err, ControlError::NoSuchPane(_)));
    }

    // ── #2 input policy ────────────────────────────────────────

    #[test]
    fn send_keys_rejected_when_pane_locked() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("policy", "/bin/sh").unwrap();
        let session = inproc.get_session(sid).unwrap();
        let pane_id = *session.panes.keys().next().unwrap();

        // Lock it.
        inproc
            .set_input_policy(pane_id, tear_types::InputPolicy::Locked)
            .unwrap();
        // send_keys must error with Rejected.
        let err = inproc.send_keys(pane_id, b"x").unwrap_err();
        assert!(
            matches!(err, tear_types::ControlError::Rejected(_)),
            "expected Rejected, got {err:?}"
        );

        // Unlock — send_keys works again.
        inproc
            .set_input_policy(pane_id, tear_types::InputPolicy::Free)
            .unwrap();
        inproc.send_keys(pane_id, b"y").unwrap();
    }

    #[test]
    fn set_input_policy_on_nonexistent_pane_returns_nosuch() {
        let inproc = InProcess::new();
        let err = inproc
            .set_input_policy(tear_types::PaneId(0xdead_beef), tear_types::InputPolicy::Locked)
            .unwrap_err();
        assert!(matches!(err, tear_types::ControlError::NoSuchPane(_)));
    }

    #[test]
    fn set_input_policy_is_idempotent() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("idem", "/bin/sh").unwrap();
        let pane_id = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();
        inproc.set_input_policy(pane_id, tear_types::InputPolicy::Locked).unwrap();
        inproc.set_input_policy(pane_id, tear_types::InputPolicy::Locked).unwrap();
        inproc.set_input_policy(pane_id, tear_types::InputPolicy::Free).unwrap();
        inproc.set_input_policy(pane_id, tear_types::InputPolicy::Free).unwrap();
        // No assertion needed — the test is that none of these panic
        // or return Err on duplicate state.
    }

    #[test]
    fn send_keys_treats_leader_as_free_at_inproc_layer() {
        // In-process consumers have no per-client identity at the
        // trait surface, so Leader collapses to Free here — the
        // daemon adds the identity-gating layer on top via
        // serve_connection_with_auth. This test pins the semantic
        // so future refactors don't accidentally start rejecting.
        let inproc = InProcess::new();
        let sid = inproc.new_session("leader-inproc", "/bin/sh").unwrap();
        let pane_id = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();
        inproc
            .set_input_policy(pane_id, tear_types::InputPolicy::leader(7))
            .unwrap();
        // No error — InProcess::send_keys does not enforce Leader.
        inproc.send_keys(pane_id, b"x").unwrap();
    }

    #[test]
    fn send_keys_unaffected_when_policy_remains_free() {
        // Default policy is Free; send_keys should accept right away
        // without the operator touching the policy. Smoke-checks the
        // policy gate's no-op path.
        let inproc = InProcess::new();
        let sid = inproc.new_session("free-default", "/bin/sh").unwrap();
        let pane_id = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();
        inproc.send_keys(pane_id, b"hello").unwrap();
    }

    #[test]
    fn send_keys_after_unlock_round_trip() {
        // Locked → Free → Locked → Free. Each Free interval must
        // accept input.
        let inproc = InProcess::new();
        let sid = inproc.new_session("rt", "/bin/sh").unwrap();
        let pane_id = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();
        for round in 0..2 {
            inproc
                .set_input_policy(pane_id, tear_types::InputPolicy::Locked)
                .unwrap();
            assert!(
                inproc.send_keys(pane_id, b"x").is_err(),
                "round {round}: Locked accepted send_keys"
            );
            inproc
                .set_input_policy(pane_id, tear_types::InputPolicy::Free)
                .unwrap();
            inproc
                .send_keys(pane_id, b"y")
                .unwrap_or_else(|e| panic!("round {round}: Free rejected send_keys: {e:?}"));
        }
    }

    // ── Pane-as-block (OSC 133) ────────────────────────────

    #[test]
    fn pane_blocks_captures_osc_133_round_trip() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("blocks-test", "/bin/sh").unwrap();
        let pane_id = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();

        // Drive a full OSC 133 cycle through the PTY by sending
        // the bytes via send_keys. The shell's echo back loops
        // them through PaneGrid → block extractor.
        //
        // We use raw escape bytes: ESC ] 133 ; X BEL
        //
        // bash + readline echo the bytes back to the PTY only on
        // INPUT, not on output. The cleanest test path: write
        // the OSC 133 sequence directly into the pty's slave
        // side via send_keys, then drain.
        //
        // For the unit test we bypass the PTY shell and feed the
        // grid directly via a synthetic call. The block
        // extractor is fully covered by tear-core/src/blocks.rs
        // unit tests; here we verify the wiring through
        // pane_blocks_list returns what we'd expect when blocks
        // exist.
        let grid_arc = {
            let map = inproc.grids.lock();
            map.get(&pane_id).cloned().unwrap()
        };
        {
            let mut grid = grid_arc.lock();
            grid.feed(b"\x1b]133;A\x07");
            grid.feed(b"$ ");
            grid.feed(b"\x1b]133;B\x07");
            grid.feed(b"echo hi");
            grid.feed(b"\x1b]133;C\x07");
            grid.feed(b"hi\r\n");
            grid.feed(b"\x1b]133;D;0\x07");
        }

        let blocks = inproc.pane_blocks_list(pane_id, 0, 10).unwrap();
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.prompt, "$ ");
        assert_eq!(b.command, "echo hi");
        assert!(b.output.contains("hi"));
        assert_eq!(b.exit_code, Some(0));

        let (total, in_progress) = inproc.pane_blocks_status(pane_id).unwrap();
        assert_eq!(total, 1);
        assert!(!in_progress);

        let one = inproc.pane_block_at(pane_id, 0).unwrap();
        assert_eq!(one.index, 0);
    }

    #[test]
    fn pane_block_at_on_missing_index_returns_rejected() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("missing-block", "/bin/sh").unwrap();
        let pane_id = *inproc.get_session(sid).unwrap().panes.keys().next().unwrap();
        let err = inproc.pane_block_at(pane_id, 99).unwrap_err();
        assert!(matches!(err, ControlError::Rejected(_)));
    }

    #[test]
    fn pane_blocks_on_nonexistent_pane_returns_nosuch() {
        let inproc = InProcess::new();
        let err = inproc
            .pane_blocks_list(PaneId(0xdead_beef), 0, 10)
            .unwrap_err();
        assert!(matches!(err, ControlError::NoSuchPane(_)));
    }

    #[test]
    fn list_sessions_returns_all_created_sessions() {
        let inproc = InProcess::new();
        let a = inproc.new_session("alpha", "/bin/sh").unwrap();
        let b = inproc.new_session("beta", "/bin/sh").unwrap();
        let c = inproc.new_session("gamma", "/bin/sh").unwrap();
        let sessions = inproc.list_sessions().unwrap();
        assert_eq!(sessions.len(), 3);
        let mut names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
        let ids: std::collections::HashSet<_> =
            sessions.iter().map(|s| s.id).collect();
        for id in [a, b, c] {
            assert!(ids.contains(&id));
        }
        // Note: sessions_in_order sorts by created_at_unix (second
        // precision); same-second creates are ordered by BTreeMap key
        // (BLAKE3-derived SessionId). Tests asserting strict
        // insertion order would be flaky.
    }

    /// The active pane of a session's active window — the usual split
    /// origin.
    fn active_pane(inproc: &InProcess, sid: SessionId) -> PaneId {
        let s = inproc.get_session(sid).unwrap();
        s.windows[&s.active_window].active_pane
    }

    #[test]
    fn split_pane_targets_the_origin_leaf_and_validates() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("split", "/bin/sh").unwrap();
        let origin = active_pane(&inproc, sid);
        let new = inproc.split_pane(origin, Direction::Right, "/bin/sh", &[]).unwrap();

        let s = inproc.get_session(sid).unwrap();
        let w = &s.windows[&s.active_window];
        // The tree now holds exactly two leaves, origin then new (Right
        // puts the new pane after the origin).
        assert_eq!(w.layout.pane_count(), 2);
        assert_eq!(w.layout.panes(), vec![origin, new]);
        assert_eq!(w.active_pane, new);
        assert!(s.panes.contains_key(&origin) && s.panes.contains_key(&new));
        // Structurally sound — no NULL leaf, no aliasing, ratio in range.
        w.layout.validate().unwrap();
    }

    #[test]
    fn kill_pane_collapses_tree_without_dangling_leaf() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("kill", "/bin/sh").unwrap();
        let origin = active_pane(&inproc, sid);
        let new = inproc.split_pane(origin, Direction::Below, "/bin/sh", &[]).unwrap();

        inproc.kill_pane(new).unwrap();
        let s = inproc.get_session(sid).unwrap();
        let w = &s.windows[&s.active_window];
        // The split collapsed back into the surviving origin leaf — no
        // PaneId::NULL hole, validate() proves it.
        assert_eq!(w.layout, tear_types::LayoutNode::leaf(origin));
        assert_eq!(w.active_pane, origin);
        assert!(!s.panes.contains_key(&new));
        w.layout.validate().unwrap();
    }

    #[test]
    fn killing_active_windows_last_pane_retargets_active_window() {
        // Regression: kill_pane's WasRoot arm removed the window but left
        // session.active_window pointing at the removed id, panicking the
        // s.windows[&active_window] index sites. A second window must
        // inherit focus when the active window's last pane is killed.
        let inproc = InProcess::new();
        let sid = inproc.new_session("multi", "/bin/sh").unwrap();
        let w1 = inproc.get_session(sid).unwrap().active_window;
        // A second window — and make it the active one we then kill.
        let w2 = inproc.new_window(sid, "second", "/bin/sh", &[]).unwrap();
        inproc.select_window(w2).unwrap();
        let only_pane_w2 = {
            let s = inproc.get_session(sid).unwrap();
            s.windows[&w2].active_pane
        };
        inproc.kill_pane(only_pane_w2).unwrap();
        let s = inproc.get_session(sid).unwrap();
        // w2 gone, focus fell back to the surviving window — and crucially
        // active_window still indexes a live window (no panic).
        assert!(!s.windows.contains_key(&w2));
        assert_eq!(s.active_window, w1);
        let _ = &s.windows[&s.active_window]; // must not panic
    }

    #[test]
    fn kill_last_pane_closes_the_window() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("solo", "/bin/sh").unwrap();
        let only = active_pane(&inproc, sid);
        // Killing the window's only pane closes the window (tmux
        // semantics) — nothing the layout tree can represent remains.
        inproc.kill_pane(only).unwrap();
        let s = inproc.get_session(sid).unwrap();
        assert!(s.windows.is_empty());
        assert!(!s.panes.contains_key(&only));
    }

    #[test]
    fn resize_pane_shifts_the_governing_divider() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("resize", "/bin/sh").unwrap();
        let origin = active_pane(&inproc, sid);
        let _new = inproc.split_pane(origin, Direction::Right, "/bin/sh", &[]).unwrap();

        let w_before = {
            let s = inproc.get_session(sid).unwrap();
            s.windows[&s.active_window].size_cells.0
        };
        let origin_w_before = inproc.get_session(sid).unwrap().panes[&origin].size_cells.0;
        // Grow the (left) origin pane rightward by a quarter of the window.
        inproc
            .resize_pane(origin, Direction::Right, (w_before / 4) as i16)
            .unwrap();
        let origin_w_after = inproc.get_session(sid).unwrap().panes[&origin].size_cells.0;
        assert!(
            origin_w_after > origin_w_before,
            "origin pane should have widened: {origin_w_after} !> {origin_w_before}"
        );
    }

    #[test]
    fn apply_layout_rearranges_existing_panes_into_a_named_preset() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("layout", "/bin/sh").unwrap();
        let s0 = inproc.get_session(sid).unwrap();
        let wid = s0.active_window;
        let p0 = s0.windows[&wid].active_pane;
        // Build an arbitrary 3-pane shape: p0 | (p1 / p2).
        let p1 = inproc.split_pane(p0, Direction::Right, "/bin/sh", &[]).unwrap();
        let _p2 = inproc.split_pane(p1, Direction::Below, "/bin/sh", &[]).unwrap();
        let before: std::collections::BTreeSet<PaneId> =
            inproc.get_window(wid).unwrap().1.layout.panes().into_iter().collect();

        // Re-tile into an even horizontal row — panes keep their PTYs.
        inproc.apply_layout(wid, LayoutKind::EvenHorizontal).unwrap();

        let w = inproc.get_window(wid).unwrap().1;
        let after: std::collections::BTreeSet<PaneId> =
            w.layout.panes().into_iter().collect();
        assert_eq!(before, after, "the same panes are preserved (PTYs kept)");
        w.layout.validate().unwrap();
        // EvenHorizontal of 3 = three equal thirds (the even-ratio refinement).
        let rects = w.layout.compute_rects(Rect::sized(90, 24));
        let mut widths: Vec<u16> = rects.iter().map(|(_, r)| r.w).collect();
        widths.sort_unstable();
        assert_eq!(widths, vec![30, 30, 30]);
    }

    #[test]
    fn apply_layout_custom_leaves_the_tree_untouched() {
        let inproc = InProcess::new();
        let sid = inproc.new_session("custom", "/bin/sh").unwrap();
        let wid = inproc.get_session(sid).unwrap().active_window;
        let p0 = inproc.get_session(sid).unwrap().windows[&wid].active_pane;
        inproc.split_pane(p0, Direction::Right, "/bin/sh", &[]).unwrap();
        let before = inproc.get_window(wid).unwrap().1.layout.clone();
        // Custom has no canonical arrangement → no-op.
        inproc.apply_layout(wid, LayoutKind::Custom).unwrap();
        assert_eq!(inproc.get_window(wid).unwrap().1.layout, before);
    }

    #[test]
    fn apply_layout_unknown_window_is_no_such_window() {
        let inproc = InProcess::new();
        let err = inproc
            .apply_layout(WindowId::from_seed("ghost"), LayoutKind::Tiled)
            .unwrap_err();
        assert!(matches!(err, ControlError::NoSuchWindow(_)));
    }
}
