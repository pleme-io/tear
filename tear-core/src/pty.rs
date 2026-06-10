//! PTY ownership — one [`PtyHandle`] per running pane.
//!
//! Wraps `portable_pty` so the rest of `tear-core` doesn't import
//! it directly. Keeps the dep boundary clear and makes future
//! backend swaps (a custom platform-specific PTY layer) a single-
//! crate change.

use std::io::{Read, Write};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use tracing::warn;

/// Hard upper bound on how long [`PtyHandle`]'s drop path waits for
/// the killed child to die before handing the reap to a detached
/// thread. `portable_pty`'s `kill()` is itself escalating on Unix
/// (SIGHUP → ~250ms grace → SIGKILL), so the poll below almost always
/// completes in well under a second; the deadline only exists so a
/// pathological child (uninterruptible sleep on a dead fuse/network
/// fd) can never block a teardown caller.
const REAP_DEADLINE: Duration = Duration::from_secs(2);

/// Poll interval for the bounded reap loop.
const REAP_POLL: Duration = Duration::from_millis(10);

/// Handle to one pane's PTY. The master side is held inside an
/// `Arc<Mutex<...>>` so the reader thread and the writer thread (or
/// `send_keys` caller) can share it. The child process handle is
/// retained so `Drop` can explicitly `kill()` it — without this,
/// dropping the master Arc doesn't kill the shell (the reader
/// thread holds a cloned master fd that keeps the slave open),
/// shells accumulate as orphans, and every subsequent fork copies
/// a bloated address space.
pub struct PtyHandle {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Child handle retained for explicit kill on drop. Wrapped
    /// in Mutex<Option<...>> so `kill()` can take ownership while
    /// still leaving the slot in a valid (None) state after.
    child: Arc<Mutex<Option<Box<dyn Child + Send + Sync>>>>,
    /// Reader thread joins on `drop()`.
    reader_join: Option<JoinHandle<()>>,
    /// Total bytes consumed by the on-pane VT parser since spawn.
    bytes_consumed: Arc<std::sync::atomic::AtomicU64>,
}

impl PtyHandle {
    /// Spawn a child process attached to a freshly minted PTY pair.
    /// The caller provides a sink for child bytes — typically a
    /// `Box<dyn FnMut(&[u8]) + Send>` that feeds into a `vte` parser
    /// or appends to a scrollback grid. The reader thread loops on
    /// `master.try_clone_reader()` until EOF, calling the sink on
    /// each read.
    ///
    /// `on_exit` is the typed end-of-stream notification: when the
    /// reader loop ends (PTY EOF because the child exited, or a read
    /// error), the reader thread reaps the child to capture its exit
    /// code, then calls `on_exit(code)` exactly once. `code` is
    /// `None` only when the child was already reaped elsewhere (the
    /// explicit-kill path, where [`Drop`] took the child first) or
    /// `wait()` failed. This is the single edge that lets the
    /// multiplexer mark the pane exited and disconnect its byte-stream
    /// subscribers; without it a reader thread that hit EOF would just
    /// end silently and every subscriber would block forever.
    pub fn spawn(
        shell: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        size: PtySize,
        mut on_bytes: Box<dyn FnMut(&[u8]) + Send>,
        on_exit: Box<dyn FnOnce(Option<i32>) + Send>,
    ) -> anyhow::Result<Self> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(size)?;
        let mut cmd = CommandBuilder::new(shell);
        for a in args {
            cmd.arg(a);
        }
        if let Some(d) = cwd {
            cmd.cwd(d);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Retain the child handle so Drop can kill() it. Without
        // this the shell becomes an orphan after PtyHandle drops:
        // closing the master fd would normally SIGHUP the child,
        // but the reader thread (below) holds a separate cloned
        // master fd that keeps the slave open, so the shell never
        // sees HUP. Accumulating orphan shells bloats the daemon
        // and slows every fork.
        let child = pair.slave.spawn_command(cmd)?;
        let child = Arc::new(Mutex::new(Some(child)));
        // Clone for the reader thread so it can reap the child + read
        // its exit code the instant the PTY hits EOF (rather than
        // leaving the zombie + the exit notification to `Drop`, which
        // only fires on explicit kill).
        let child_for_reader = Arc::clone(&child);
        // Slave fd retained by the child; once it exits the master
        // reader hits EOF.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master = Arc::new(Mutex::new(pair.master));
        let writer = Arc::new(Mutex::new(writer));

        let bytes_consumed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let bytes_consumed_for_thread = Arc::clone(&bytes_consumed);
        let reader_join = std::thread::Builder::new()
            .name("tear-pty-reader".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            bytes_consumed_for_thread
                                .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                            on_bytes(&buf[..n]);
                        }
                        Err(e) => {
                            warn!(error = %e, "tear pty reader error");
                            break;
                        }
                    }
                }
                // EOF (or read error) — the byte stream is over. Reap
                // the child to capture its exit code AND clear the
                // zombie, then fire the typed exit notification so the
                // multiplexer marks the pane exited + disconnects its
                // subscribers. If the explicit-kill path (PtyHandle::
                // Drop) already took the child, `take()` is None and
                // `code` is None — the notification still fires so the
                // disconnect is idempotent.
                //
                // `take()` and `wait()` are SEPARATE statements on
                // purpose: chained, the temporary `MutexGuard` lives
                // until the end of the full expression, holding the
                // child slot's lock across a blocking `wait()`. The
                // explicit-kill path locks the same slot, so a wait
                // that outlives the child's fds (EOF without exit —
                // daemonizing children) would hand `Drop` an unbounded
                // lock to block on. Take under the lock, wait outside.
                let taken = child_for_reader.lock().take();
                let code = taken
                    .and_then(|mut c| c.wait().ok())
                    .map(|status| status.exit_code() as i32);
                on_exit(code);
            })?;

        Ok(Self {
            master,
            writer,
            child,
            reader_join: Some(reader_join),
            bytes_consumed,
        })
    }

    /// Send bytes to the child's stdin.
    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock();
        w.write_all(bytes)
    }

    /// Resize the PTY winsize. Causes SIGWINCH delivery to the child.
    pub fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        let m = self.master.lock();
        m.resize(size)?;
        Ok(())
    }

    /// Total bytes consumed by the pane's parser since spawn.
    pub fn bytes_consumed(&self) -> u64 {
        self.bytes_consumed
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        // Explicitly kill the child shell first. Dropping the
        // master Arc alone is NOT enough — the reader thread
        // holds a cloned master fd that keeps the slave alive,
        // so the shell never sees SIGHUP. kill() sends SIGHUP,
        // grace-polls ~250ms, then escalates to SIGKILL
        // (portable_pty's behavior on Unix); the shell exits,
        // the slave fd closes, and the reader's read() returns
        // Ok(0). The reader thread then exits the loop and the
        // JoinHandle drop below completes immediately.
        //
        // On the natural-exit path the reader thread has already
        // `take()`n + `wait()`ed the child (see `spawn`), so this
        // slot is `None` and we skip straight to detaching the
        // reader join handle — no double-wait, no double-kill.
        if let Some(child) = self.child.lock().take() {
            reap_with_deadline(child);
        }
        if let Some(j) = self.reader_join.take() {
            drop(j);
        }
    }
}

/// Kill `child` and reap it within [`REAP_DEADLINE`] — NEVER an
/// unbounded `wait()`.
///
/// Drop runs on the teardown caller's own thread, and the caller may
/// be a lock-heavy path (`InProcess::kill_*`); a blocking `wait()`
/// here wedged mado's L1 teardown for 20+ minutes (2026-06-10) when
/// the pane's reader thread was simultaneously blocked acquiring an
/// `InProcess` lock the caller held — mutual wait. The fix is two
/// halves: the kill paths drop handles outside every lock (see
/// `InProcess::detach_panes`), and this reap is bounded regardless.
///
/// A child that survives even kill()'s SIGKILL escalation past the
/// deadline (uninterruptible-sleep pathology) is handed to a detached
/// reaper thread, so the zombie is still collected without the caller
/// ever blocking on it.
fn reap_with_deadline(mut child: Box<dyn Child + Send + Sync>) {
    if let Err(e) = child.kill() {
        warn!(error = %e, "tear pty child kill failed (already exited?)");
    }
    let deadline = Instant::now() + REAP_DEADLINE;
    loop {
        match child.try_wait() {
            // Reaped — no zombie left in the daemon's pid table.
            Ok(Some(_)) => return,
            // Already reaped elsewhere (kill()'s internal grace loop
            // try_waits, so a fast-dying child is often collected
            // before we get here) — nothing left to do.
            Err(_) => return,
            Ok(None) => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(REAP_POLL);
            }
        }
    }
    warn!("tear pty child survived kill past reap deadline — detaching reaper thread");
    let _ = std::thread::Builder::new()
        .name("tear-pty-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_reaps_sighup_immune_child_within_deadline() {
        // portable_pty's kill() leads with SIGHUP; a child that ignores
        // it (anything nohup-wrapped) only dies on the SIGKILL
        // escalation. The drop path must stay bounded for that class —
        // the pre-fix unbounded `wait()` is exactly what a teardown
        // caller would block on. nohup redirects stdout to ./nohup.out
        // (stdout IS a tty here), so cwd points at a temp dir.
        let dir = std::env::temp_dir();
        let handle = PtyHandle::spawn(
            "/usr/bin/nohup",
            &["cat".into()],
            dir.to_str(),
            &[("PATH".into(), "/usr/bin:/bin".into())],
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .expect("spawn nohup cat");
        // Give nohup a beat to exec cat with SIGHUP ignored — killing
        // during the exec window would test the wrong disposition.
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        drop(handle);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "PtyHandle drop blocked {elapsed:?} on a SIGHUP-immune child — reap is unbounded again"
        );
    }
}
