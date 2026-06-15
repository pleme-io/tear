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
    ControlError, ControlResult, Direction, MultiplexerControl, PaneId, SessionId, TearPane,
    TearSession, TearWindow, WindowId,
};

use std::sync::mpsc;

use crate::pane_grid::PaneGrid;
use crate::pty::PtyHandle;
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
    fn spawn_pty_for(&self, pane_id: PaneId, shell: &str, size: (u16, u16)) -> anyhow::Result<()> {
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
        // exits (PTY EOF). Two typed consequences:
        //
        //  1. Mark the pane `PaneState::Exited { code }` in the typed
        //     registry. The pane + its final grid stay (tmux
        //     remain-on-exit) so `tear list` / snapshots still see it;
        //     only the live byte stream ends.
        //  2. Mark the subscriber entry `closed` + drop every live
        //     sender. Each engate/daemon `Receiver.recv()` then
        //     returns `Err` — the end-of-stream signal mado's
        //     `attach_live.run()` and the daemon's `serve_subscription`
        //     block on. Without this the channel stays open forever and
        //     a single-pane GUI (mado embedded) never learns the shell
        //     exited, so its window never closes.
        //
        // Lock order matches the kill paths (the registry write is a
        // separate critical section from the subscribers lock — never
        // nested — so no inversion). The subscribers step is gated on
        // the pane still being present in the registry so an explicit
        // `kill_pane` that races with natural exit doesn't leave a
        // lingering empty entry.
        let subscribers_for_exit = Arc::clone(&self.subscribers);
        let registry_for_exit = Arc::clone(&self.registry);
        let on_exit = Box::new(move |code: Option<i32>| {
            let still_present = {
                let mut r = registry_for_exit.write();
                let mut found = false;
                for s in r.sessions.values_mut() {
                    if let Some(p) = s.panes.get_mut(&pane_id) {
                        p.state = tear_types::PaneState::Exited {
                            code: code.unwrap_or(-1),
                        };
                        found = true;
                        break;
                    }
                }
                found
            };
            let mut subs = subscribers_for_exit.lock();
            if still_present {
                let ps = subs.entry(pane_id).or_default();
                ps.closed = Some(code.unwrap_or(-1));
                ps.senders.clear();
            } else {
                // Pane was explicitly killed concurrently — drop any
                // entry rather than recreating one for a dead id.
                subs.remove(&pane_id);
            }
            debug!(pane_id = %pane_id, ?code, "tear-core: pane child exited — marked Exited + disconnected subscribers");
        });
        let pty = PtyHandle::spawn(
            shell,
            &[],
            cwd.as_deref(),
            &env,
            pty_size,
            on_bytes,
            on_exit,
        )?;
        self.ptys.lock().insert(pane_id, pty);
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
        source: tear_types::SessionSource,
        size_cells: (u16, u16),
    ) -> ControlResult<SessionId> {
        let size = (size_cells.0.max(1), size_cells.1.max(1));
        let mut r = self.registry.write();
        let sid = r.create_session(name);
        // Stamp provenance on the typed session entry. The
        // registry.create_session built it with Source::default()
        // (Human); overwrite when the caller asked for something
        // else.
        if let Some(s) = r.sessions.get_mut(&sid) {
            s.source = source.clone();
        }
        let Some((_wid, pane_id)) = r.add_window(sid, "main", shell, size) else {
            return Err(ControlError::Internal(anyhow::anyhow!(
                "registry.add_window returned None after fresh create_session"
            )));
        };
        drop(r); // release write lock before spawning PTY
        if let Err(e) = self.spawn_pty_for(pane_id, shell, size) {
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

    fn new_window(&self, session: SessionId, name: &str, shell: &str) -> ControlResult<WindowId> {
        let size = (80, 24);
        let (wid, pid) = {
            let mut r = self.registry.write();
            r.add_window(session, name, shell, size)
                .ok_or(ControlError::NoSuchSession(session))?
        };
        if let Err(e) = self.spawn_pty_for(pid, shell, size) {
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
        _direction: Direction,
        shell: &str,
    ) -> ControlResult<PaneId> {
        // M0 minimal viable split: locate parent, add a leaf to the
        // window's layout as a balanced split with the origin. The
        // full directional / ratio logic lands in M2.
        let (sid, wid) = self
            .registry
            .read()
            .locate_pane(origin)
            .ok_or(ControlError::NoSuchPane(origin))?;
        let size = (80, 12);
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
                    args: vec![],
                    cwd: None,
                    env: vec![],
                    size_cells: size,
                    origin_cells: (0, 0),
                    state: tear_types::PaneState::Running,
                    title: shell.into(),
                    input_policy: tear_types::InputPolicy::default(),
                },
            );
            // Replace the window's layout with a balanced split.
            if let Some(w) = s.windows.get_mut(&wid) {
                let old = std::mem::replace(
                    &mut w.layout,
                    tear_types::LayoutNode::leaf(tear_types::PaneId::NULL),
                );
                w.layout = tear_types::LayoutNode::split(
                    _direction.orientation(),
                    old,
                    tear_types::LayoutNode::leaf(new_pid),
                );
                w.active_pane = new_pid;
            }
            new_pid
        };
        self.spawn_pty_for(pid, shell, size)
            .map_err(ControlError::Internal)?;
        info!(pane = %pid, "tear-core: split pane");
        Ok(pid)
    }

    fn kill_pane(&self, id: PaneId) -> ControlResult<()> {
        // NOTE pre-fix this was `self.ptys.lock().remove(&id);` — the
        // returned PtyHandle was a temporary dropped BEFORE the lock
        // guard (reverse creation order), i.e. the kill + reap ran
        // with the ptys lock held. Same wedge class as kill_session;
        // same cure: detach under the locks, drop outside them.
        let detached = self.detach_panes(&[id]);
        let found = {
            let mut r = self.registry.write();
            let mut found = false;
            for s in r.sessions.values_mut() {
                if s.panes.remove(&id).is_some() {
                    found = true;
                    // Layout update — M2 will properly rebalance the
                    // tree. For now we leave the leaf reference dangling;
                    // the renderer treats a missing pane as blank.
                    break;
                }
            }
            found
        };
        drop(detached);
        if !found {
            return Err(ControlError::NoSuchPane(id));
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
        _direction: Direction,
        _delta_cells: i16,
    ) -> ControlResult<()> {
        // M2 — proper layout-tree resize. For M0 we acknowledge.
        let _ = self.registry.read().locate_pane(id).ok_or(ControlError::NoSuchPane(id))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_inproc_starts_empty() {
        let inproc = InProcess::new();
        let sessions = inproc.list_sessions().unwrap();
        assert!(sessions.is_empty());
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
}
