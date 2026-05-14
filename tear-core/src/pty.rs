//! PTY ownership — one [`PtyHandle`] per running pane.
//!
//! Wraps `portable_pty` so the rest of `tear-core` doesn't import
//! it directly. Keeps the dep boundary clear and makes future
//! backend swaps (a custom platform-specific PTY layer) a single-
//! crate change.

use std::io::{Read, Write};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use tracing::warn;

/// Handle to one pane's PTY. The master side is held inside an
/// `Arc<Mutex<...>>` so the reader thread and the writer thread (or
/// `send_keys` caller) can share it. The child process is owned by
/// the master end of `MasterPty`; dropping the handle reaps it.
pub struct PtyHandle {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
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
    pub fn spawn(
        shell: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        size: PtySize,
        mut on_bytes: Box<dyn FnMut(&[u8]) + Send>,
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
        let _child = pair.slave.spawn_command(cmd)?;
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
            })?;

        Ok(Self {
            master,
            writer,
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
        if let Some(j) = self.reader_join.take() {
            // Closing the master end of the PTY signals EOF to the
            // reader; the join will return shortly.
            drop(j);
        }
    }
}
