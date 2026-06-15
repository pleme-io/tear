//! M1 "Remember" — the daemon-side praça persistence store.
//!
//! The `praca` crate (M0) is a pure, time-injected substrate: it holds
//! the [`praca::Praca`] facade (session index + frecency + project↔
//! session bindings) but does no I/O. M1 wires that substrate into the
//! daemon so the bindings + frecency **survive a daemon restart**:
//!
//! * the daemon holds one [`PracaStore`] guarding a live `Praca`;
//! * on every session lifecycle mutation (create / kill) the daemon
//!   updates the `Praca` and **persists** the whole [`praca::PracaSnapshot`]
//!   to a JSON file;
//! * at startup the daemon **loads** that file back, so a later `cd`
//!   sees the binding written by an earlier session.
//!
//! ## On-disk shape + path
//!
//! One JSON document — `serde_json` of a [`praca::PracaSnapshot`] (the
//! index array + the binding map + policy + name style). The default
//! path follows the daemon's existing XDG convention (the config side
//! lives under `~/.config/tear/`); state belongs under the XDG **state**
//! dir, so the default is `$XDG_STATE_HOME/tear/praca.json`, falling
//! back to `~/.local/state/tear/praca.json`. `$TEAR_STATE_DIR` overrides
//! the directory wholesale (the seam tests inject a temp dir through).
//!
//! ## Atomicity
//!
//! Writes are **write-temp-then-rename**: the snapshot is serialised to
//! `praca.json.tmp.<pid>` in the same directory, then `rename`d over the
//! real path. `rename(2)` within one filesystem is atomic, so a crash
//! mid-write can leave the temp file but never a half-written
//! `praca.json` — a reload always sees either the old complete document
//! or the new complete one, never a torn one.
//!
//! ## Concurrency + time
//!
//! The daemon serves connections on many threads; the store guards the
//! `Praca` behind a `Mutex`. Persistence is best-effort — a write
//! failure is logged at `warn` and never blocks the session op (the
//! operator's "open this session now" intent always wins, exactly like
//! the audit log). Time (`now` unix-seconds) is **injected** by the
//! caller — the store never reads the clock, mirroring the M0 substrate.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use praca::{Praca, PracaSnapshot};
use tracing::{debug, warn};

/// Thread-safe, file-backed home for the daemon's [`Praca`] facade.
///
/// Cheap to clone (an `Arc` bump) so each connection thread shares the
/// one live store + one on-disk file.
#[derive(Clone)]
pub struct PracaStore {
    inner: Arc<PracaInner>,
}

struct PracaInner {
    /// The live orchestrator — index + binding + policy + style.
    praca: Mutex<Praca>,
    /// Where the snapshot persists.
    path: PathBuf,
}

impl PracaStore {
    /// Open the store at `path`, loading any existing snapshot.
    ///
    /// A missing file is **not** an error — first run starts from an
    /// empty `Praca`. A present-but-corrupt file is logged at `warn`
    /// and treated as empty so a single bad write never wedges the
    /// daemon (the next mutation overwrites it cleanly).
    #[must_use]
    pub fn open(path: PathBuf) -> Self {
        let praca = match load_snapshot(&path) {
            Ok(Some(snap)) => {
                debug!(
                    path = %path.display(),
                    bindings = snap.binding.len(),
                    sessions = snap.index.len(),
                    "praca store: loaded snapshot"
                );
                Praca::from_snapshot(snap)
            }
            Ok(None) => {
                debug!(path = %path.display(), "praca store: no snapshot yet (fresh)");
                Praca::new()
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "praca store: snapshot load failed; starting empty"
                );
                Praca::new()
            }
        };
        Self {
            inner: Arc::new(PracaInner {
                praca: Mutex::new(praca),
                path,
            }),
        }
    }

    /// Open the store at the daemon's default state path
    /// ([`default_praca_path`]).
    #[must_use]
    pub fn open_default() -> Self {
        Self::open(default_praca_path())
    }

    /// The file this store persists to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Run `f` against the live `Praca` under the lock, then persist the
    /// resulting snapshot atomically. Returns whatever `f` returns.
    ///
    /// This is the one mutate-then-persist chokepoint — every lifecycle
    /// hook routes through it, so a mutation that isn't persisted is
    /// unrepresentable (there is no path that mutates without the
    /// trailing write).
    pub fn mutate<R>(&self, f: impl FnOnce(&mut Praca) -> R) -> R {
        let mut guard = self.inner.praca.lock().expect("praca store lock poisoned");
        let out = f(&mut guard);
        let snap = guard.to_snapshot();
        drop(guard);
        if let Err(e) = persist_snapshot(&self.inner.path, &snap) {
            warn!(
                path = %self.inner.path.display(),
                error = %e,
                "praca store: persist failed; binding may not survive restart"
            );
        }
        out
    }

    /// Borrow the live `Praca` read-only under the lock and return
    /// whatever `f` extracts. Used by the auto-attach query path
    /// (later phases) — read without a persist.
    pub fn with<R>(&self, f: impl FnOnce(&Praca) -> R) -> R {
        let guard = self.inner.praca.lock().expect("praca store lock poisoned");
        f(&guard)
    }
}

/// Resolve the daemon's default praça state path. Honours
/// `$TEAR_STATE_DIR` (whole-directory override, used by tests), then
/// `$XDG_STATE_HOME`, then `~/.local/state`, landing the file at
/// `<state>/tear/praca.json`.
#[must_use]
pub fn default_praca_path() -> PathBuf {
    state_dir().join("tear").join("praca.json")
}

/// The base state directory, per the override ladder documented on
/// [`default_praca_path`].
fn state_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("TEAR_STATE_DIR") {
        return PathBuf::from(explicit);
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg);
    }
    if let Ok(home) = std::env::var("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".local");
        p.push("state");
        return p;
    }
    PathBuf::from(".")
}

/// Read + parse the snapshot at `path`. `Ok(None)` when the file is
/// absent (first run); `Err` only for read / parse failures of a file
/// that does exist.
fn load_snapshot(path: &Path) -> std::io::Result<Option<PracaSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    let snap: PracaSnapshot = serde_json::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(snap))
}

/// Serialise `snap` to `path` atomically (write-temp-then-rename).
/// Creates parent dirs on demand.
fn persist_snapshot(path: &Path, snap: &PracaSnapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(snap)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Temp file in the SAME directory so the rename stays within one
    // filesystem (cross-device rename is not atomic / fails outright).
    let tmp = tmp_path(path);
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `<path>.tmp.<pid>` — pid-tagged so two daemons (or a test running
/// several stores) never collide on the same temp name.
fn tmp_path(path: &Path) -> PathBuf {
    let pid = std::process::id();
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".tmp.{pid}"));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use praca::SessionRecord;
    use std::path::PathBuf;
    use tear_types::SessionId;

    fn sid(s: &str) -> SessionId {
        SessionId::from_seed(s)
    }

    /// A unique temp dir for one test, removed by the caller.
    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nonce: u128 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("tear-praca-{tag}-{pid}-{nonce}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn open_missing_file_starts_empty() {
        let dir = temp_dir("missing");
        let path = dir.join("praca.json");
        let store = PracaStore::open(path.clone());
        assert!(!path.exists(), "open must not create the file eagerly");
        store.with(|p| {
            assert!(p.index.is_empty());
            assert!(p.binding.is_empty());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mutate_persists_and_reload_survives_round_trip() {
        let dir = temp_dir("round-trip");
        let path = dir.join("praca.json");

        // Session 1: write a binding + a record with frecency.
        {
            let store = PracaStore::open(path.clone());
            store.mutate(|p| {
                let rec = SessionRecord::for_project(
                    sid("tide"),
                    PathBuf::from("/code/pleme-io/mado"),
                    praca::SessionNameStyle::Emoji,
                    1_700,
                );
                p.index.upsert(rec);
                p.binding
                    .bind(PathBuf::from("/code/pleme-io/mado"), sid("tide"));
                p.record_visit(sid("tide"), 1_999);
            });
            assert!(path.exists(), "mutate must persist the file");
        }

        // A FRESH store at the same path (a daemon "restart") sees the
        // binding + the exact frecency counters — byte-for-byte.
        let reloaded = PracaStore::open(path.clone());
        reloaded.with(|p| {
            assert_eq!(
                p.binding.lookup(Path::new("/code/pleme-io/mado")),
                Some(sid("tide")),
                "binding survived the restart"
            );
            let r = p.index.get(sid("tide")).expect("record survived");
            assert_eq!(r.last_seen, 1_999, "frecency last_seen survived");
            assert_eq!(r.visits, 2, "frecency visits survived (1 init + 1 visit)");
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_session_resolves_persisted_binding() {
        // Proves the M1 payoff: a binding written in one "session"
        // (store instance) is read back to auto-attach in a later one.
        let dir = temp_dir("second-session");
        let path = dir.join("praca.json");
        let root = PathBuf::from("/code/pleme-io/tear");

        // First session binds the project root.
        {
            let store = PracaStore::open(path.clone());
            store.mutate(|p| {
                let rec = SessionRecord::for_project(
                    sid("frost"),
                    root.clone(),
                    praca::SessionNameStyle::Emoji,
                    100,
                );
                p.index.upsert(rec);
                p.binding.bind(root.clone(), sid("frost"));
            });
        }

        // Second session (restart): the attach engine resolves the
        // persisted binding to a SwitchTo, NOT a SpawnNew.
        let store2 = PracaStore::open(path.clone());
        let decision = store2.with(|p| {
            // current=None, cd straight into the bound project root.
            praca::decide_with_root(
                p.policy,
                None,
                &root,
                &p.binding,
                &p.index,
                p.name_style,
            )
        });
        assert_eq!(
            decision,
            praca::AttachDecision::SwitchTo(sid("frost")),
            "a persisted binding auto-attaches in a later session"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_leaves_no_temp_and_valid_json() {
        let dir = temp_dir("atomic");
        let path = dir.join("praca.json");
        let store = PracaStore::open(path.clone());
        store.mutate(|p| {
            p.binding.bind(PathBuf::from("/x"), sid("x"));
        });

        // The real file exists and parses; the temp sidecar is gone
        // (rename consumed it) — a torn write is unrepresentable.
        assert!(path.exists());
        let tmp = tmp_path(&path);
        assert!(!tmp.exists(), "temp file must be renamed away, not left behind");
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: PracaSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.binding.lookup(Path::new("/x")), Some(sid("x")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_starts_empty_not_wedged() {
        let dir = temp_dir("corrupt");
        let path = dir.join("praca.json");
        std::fs::write(&path, b"{not valid json").unwrap();
        // Open must not panic; it starts empty and the next mutate
        // overwrites the bad bytes cleanly.
        let store = PracaStore::open(path.clone());
        store.with(|p| assert!(p.binding.is_empty()));
        store.mutate(|p| {
            p.binding.bind(PathBuf::from("/recover"), sid("r"));
        });
        let reloaded = PracaStore::open(path.clone());
        reloaded.with(|p| {
            assert_eq!(
                p.binding.lookup(Path::new("/recover")),
                Some(sid("r")),
                "store recovers from a corrupt file on the next write"
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
