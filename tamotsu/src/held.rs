use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use makimono::PaneDir;
use parking_lot::{Condvar, Mutex};

use crate::args::HoldArgs;
use crate::authority::{Authority, Verdict};
use crate::proto::{FromHolder, PROTO, ToHolder, read_from_holder, write_to_holder};

const READY_DEADLINE: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_TRIES: u32 = 3;
const REVIVE_WINDOW: Duration = Duration::from_secs(60);
const REVIVE_BUDGET: usize = 5;
const KEY_ECHO: Duration = Duration::from_secs(2);
const MUTE_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldProgram {
    pub program: PathBuf,
    pub prefix: Vec<String>,
}

impl HoldProgram {
    #[must_use]
    pub fn current_exe_subcommand(subcommand: &str) -> Option<Self> {
        std::env::current_exe().ok().map(|program| Self {
            program,
            prefix: vec![subcommand.to_string()],
        })
    }
}

#[derive(Clone, Debug)]
pub struct Revival {
    pub program: HoldProgram,
    pub args: HoldArgs,
    pub env: Vec<(String, String)>,
}

pub type OnBytes = Box<dyn FnMut(&[u8]) + Send>;
pub type OnExit = Box<dyn FnOnce(Option<i32>) + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Key { base: u64 },
    Read,
}

struct Link {
    writer: Mutex<Option<UnixStream>>,
    detached: AtomicBool,
    ended: AtomicBool,
    consumed: AtomicU64,
    next: Mutex<u64>,
    arrived: Condvar,
    healthy_at: Mutex<Option<Instant>>,
    socket: PathBuf,
    authority: Authority,
}

pub struct HeldPty {
    link: Arc<Link>,
    edges: SyncSender<Edge>,
}

enum Health {
    Healthy,
    Mute { received: u64, journaled: u64 },
    Unknown,
}

impl Link {
    fn publish(&self, next: u64) {
        *self.next.lock() = next;
        self.arrived.notify_all();
    }

    fn arrives_past(&self, base: u64, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let mut next = self.next.lock();
        while *next <= base {
            if self.arrived.wait_until(&mut next, deadline).timed_out() {
                return *next > base;
            }
        }
        true
    }

    fn recently_healthy(&self) -> bool {
        self.healthy_at
            .lock()
            .is_some_and(|at| at.elapsed() < MUTE_GRACE)
    }

    fn probe(&self) -> Health {
        let Ok(mut stream) = UnixStream::connect(&self.socket) else {
            return Health::Unknown;
        };
        let Ok(journaled) = status_end(&mut stream) else {
            return Health::Unknown;
        };
        drop(stream);
        let mut next = self.next.lock();
        let mut seen = *next;
        let mut quiet_until = Instant::now() + MUTE_GRACE;
        while *next < journaled {
            if self.detached.load(Ordering::Acquire) || self.ended.load(Ordering::Acquire) {
                return Health::Unknown;
            }
            let timed_out = self.arrived.wait_until(&mut next, quiet_until).timed_out();
            if *next != seen {
                seen = *next;
                quiet_until = Instant::now() + MUTE_GRACE;
            } else if timed_out {
                return Health::Mute {
                    received: *next,
                    journaled,
                };
            }
        }
        Health::Healthy
    }

    fn cut(&self) {
        if let Some(stream) = self.writer.lock().as_ref() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn check(link: &Link, edges: &Receiver<Edge>) {
    while let Ok(edge) = edges.recv() {
        if link.detached.load(Ordering::Acquire) || link.ended.load(Ordering::Acquire) {
            continue;
        }
        if let Edge::Key { base } = edge
            && link.arrives_past(base, KEY_ECHO)
        {
            continue;
        }
        if link.recently_healthy() {
            continue;
        }
        match link.probe() {
            Health::Healthy => *link.healthy_at.lock() = Some(Instant::now()),
            Health::Unknown => {}
            Health::Mute {
                received,
                journaled,
            } => {
                tracing::warn!(
                    socket = %link.socket.display(),
                    received,
                    journaled,
                    "tamotsu: the holder journaled bytes this daemon never received and its link made no progress; re-attaching by offset"
                );
                if link.authority.holds() {
                    link.cut();
                }
            }
        }
    }
}

impl HeldPty {
    pub fn launch(
        revival: Revival,
        authority: &Authority,
        on_bytes: OnBytes,
        on_exit: OnExit,
    ) -> io::Result<Self> {
        authority.require()?;
        spawn_holder(&revival)?;
        let stream = connect_when_ready(&revival.args.socket, READY_DEADLINE)?;
        Self::start(stream, revival, authority, on_bytes, on_exit)
    }

    pub fn adopt(
        revival: Revival,
        authority: &Authority,
        on_bytes: OnBytes,
        on_exit: OnExit,
    ) -> io::Result<Self> {
        authority.require()?;
        let stream = UnixStream::connect(&revival.args.socket)?;
        Self::start(stream, revival, authority, on_bytes, on_exit)
    }

    #[must_use]
    pub fn probe(socket: &Path) -> bool {
        let Ok(mut stream) = UnixStream::connect(socket) else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        hello(&mut stream).is_ok()
    }

    fn start(
        stream: UnixStream,
        revival: Revival,
        authority: &Authority,
        on_bytes: OnBytes,
        on_exit: OnExit,
    ) -> io::Result<Self> {
        let (reader, writer) = handshake(stream, 0, authority.declared())?;
        let link = Arc::new(Link {
            writer: Mutex::new(Some(writer)),
            detached: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            consumed: AtomicU64::new(0),
            next: Mutex::new(0),
            arrived: Condvar::new(),
            healthy_at: Mutex::new(None),
            socket: revival.args.socket.clone(),
            authority: authority.clone(),
        });
        let (edges, inbox) = sync_channel(1);
        let thread_link = Arc::clone(&link);
        thread::Builder::new()
            .name("tamotsu-held-reader".into())
            .spawn(move || {
                follow(&thread_link, reader, revival, on_bytes, on_exit);
                thread_link.ended.store(true, Ordering::Release);
                thread_link.arrived.notify_all();
            })?;
        let check_link = Arc::clone(&link);
        thread::Builder::new()
            .name("tamotsu-held-check".into())
            .spawn(move || check(&check_link, &inbox))?;
        Ok(Self { link, edges })
    }

    pub fn read_edge(&self) {
        let _ = self.edges.try_send(Edge::Read);
    }

    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let base = *self.link.next.lock();
        self.send(&ToHolder::Write(bytes.to_vec()))?;
        if !bytes.is_empty() {
            let _ = self.edges.try_send(Edge::Key { base });
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.send(&ToHolder::Resize { cols, rows })
    }

    #[must_use]
    pub fn bytes_consumed(&self) -> u64 {
        self.link.consumed.load(Ordering::Relaxed)
    }

    pub fn end(&self) {
        let _ = self.send(&ToHolder::End);
    }

    fn send(&self, msg: &ToHolder) -> io::Result<()> {
        let mut guard = self.link.writer.lock();
        let stream = guard.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "holder link is being repaired")
        })?;
        write_to_holder(stream, msg)
    }
}

impl Drop for HeldPty {
    fn drop(&mut self) {
        self.link.detached.store(true, Ordering::Release);
        if let Some(stream) = self.link.writer.lock().take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn follow(
    link: &Link,
    reader: UnixStream,
    revival: Revival,
    mut on_bytes: OnBytes,
    on_exit: OnExit,
) {
    let pane = PaneDir::at(&revival.args.pane_dir);
    let mut r = BufReader::new(reader);
    let mut next: u64 = 0;
    let mut revivals: VecDeque<Instant> = VecDeque::new();
    loop {
        match read_from_holder(&mut r) {
            Ok(FromHolder::Bytes { at, data }) => {
                let end = at + data.len() as u64;
                if end <= next {
                    continue;
                }
                let skip = usize::try_from(next.saturating_sub(at)).unwrap_or(data.len());
                let fresh = &data[skip.min(data.len())..];
                link.consumed
                    .fetch_add(fresh.len() as u64, Ordering::Relaxed);
                on_bytes(fresh);
                next = end;
                link.publish(next);
                continue;
            }
            Ok(FromHolder::Exited { ending }) => {
                on_exit(ending.exit_code());
                return;
            }
            Ok(FromHolder::Displaced { by }) => {
                if link.detached.load(Ordering::Acquire) {
                    return;
                }
                if let Verdict::Lost = link.authority.displaced(by) {
                    *link.writer.lock() = None;
                    return;
                }
            }
            Ok(_) => continue,
            Err(_) => {}
        }
        if link.detached.load(Ordering::Acquire) {
            return;
        }
        *link.writer.lock() = None;
        if let Ok(Some(ending)) = pane.tombstone() {
            on_exit(ending.exit_code());
            return;
        }
        if !link.authority.holds() {
            return;
        }
        match repair(&revival, &link.authority, &pane, &mut next, &mut revivals) {
            Ok((reader, writer)) => {
                if link.detached.load(Ordering::Acquire) {
                    let _ = writer.shutdown(std::net::Shutdown::Both);
                    return;
                }
                link.publish(next);
                *link.writer.lock() = Some(writer);
                r = BufReader::new(reader);
            }
            Err(e) => {
                if !link.authority.holds() {
                    return;
                }
                tracing::warn!(
                    pane = %pane.path().display(),
                    error = %e,
                    "tamotsu: holder lost and could not be revived; the session stays on disk for the next daemon start"
                );
                on_exit(None);
                return;
            }
        }
    }
}

fn repair(
    revival: &Revival,
    authority: &Authority,
    pane: &PaneDir,
    next: &mut u64,
    revivals: &mut VecDeque<Instant>,
) -> io::Result<(UnixStream, UnixStream)> {
    for _ in 0..RECONNECT_TRIES {
        authority.require()?;
        if let Ok(stream) = UnixStream::connect(&revival.args.socket) {
            if let Ok(pair) = handshake(stream, *next, authority.declared()) {
                return Ok(pair);
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    authority.require()?;
    let now = Instant::now();
    while revivals
        .front()
        .is_some_and(|t| now.duration_since(*t) > REVIVE_WINDOW)
    {
        revivals.pop_front();
    }
    if revivals.len() >= REVIVE_BUDGET {
        return Err(io::Error::other(format!(
            "holder died {REVIVE_BUDGET} times within {}s",
            REVIVE_WINDOW.as_secs()
        )));
    }
    revivals.push_back(now);
    let mut args = revival.args.clone();
    args.resurrected = true;
    if let Ok(Some(mut meta)) = pane.read_meta() {
        args.cwd = pane.revive_cwd(&meta).or(args.cwd);
        meta.resurrections += 1;
        let _ = pane.write_meta(&meta);
    }
    let revived = Revival {
        program: revival.program.clone(),
        args,
        env: revival.env.clone(),
    };
    spawn_holder(&revived)?;
    let mut stream = connect_when_ready(&revived.args.socket, READY_DEADLINE)?;
    let end = status_end(&mut stream)?;
    if end < *next {
        *next = end;
    }
    handshake(stream, *next, authority.declared())
}

fn spawn_holder(revival: &Revival) -> io::Result<()> {
    let pane = PaneDir::at(&revival.args.pane_dir);
    pane.create()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pane.holder_log())?;
    let mut child = Command::new(&revival.program.program)
        .args(&revival.program.prefix)
        .args(revival.args.to_argv())
        .env_clear()
        .envs(revival.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()?;
    let _ = thread::Builder::new()
        .name("tamotsu-holder-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(())
}

fn connect_when_ready(socket: &Path, deadline: Duration) -> io::Result<UnixStream> {
    let until = Instant::now() + deadline;
    loop {
        match UnixStream::connect(socket) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= until => return Err(e),
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn hello(stream: &mut UnixStream) -> io::Result<()> {
    write_to_holder(stream, &ToHolder::Hello { proto: PROTO })?;
    match read_from_holder(stream)? {
        FromHolder::Hello { .. } => Ok(()),
        FromHolder::Refused { reason } => Err(io::Error::new(io::ErrorKind::Unsupported, reason)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected Hello, holder sent {other:?}"),
        )),
    }
}

fn status_end(stream: &mut UnixStream) -> io::Result<u64> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    hello(stream)?;
    write_to_holder(stream, &ToHolder::Status)?;
    loop {
        match read_from_holder(stream)? {
            FromHolder::Status(s) => {
                stream.set_read_timeout(None)?;
                return Ok(s.end);
            }
            FromHolder::Bytes { .. } => {}
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected Status, holder sent {other:?}"),
                ));
            }
        }
    }
}

fn handshake(
    mut stream: UnixStream,
    from: u64,
    incarnation: Option<u64>,
) -> io::Result<(UnixStream, UnixStream)> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    hello(&mut stream)?;
    stream.set_read_timeout(None)?;
    write_to_holder(&mut stream, &ToHolder::Attach { from, incarnation })?;
    let reader = stream.try_clone()?;
    Ok((reader, stream))
}
