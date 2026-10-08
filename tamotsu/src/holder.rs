use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use makimono::{Ending, Journal, PaneDir};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

use crate::args::HoldArgs;
use crate::proto::{FromHolder, HolderStatus, PROTO, ToHolder, read_to_holder, write_from_holder};
use crate::sink::Sink;

const SINK_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_GRACE: Duration = Duration::from_millis(300);
const END_GRACE: Duration = Duration::from_millis(1500);
const TICK: Duration = Duration::from_millis(250);
const CWD_POLL_TICKS: u32 = 8;
const REPLAY_CHUNK: usize = 256 * 1024;
const BANNER: &str = "\r\n\x1b[0m\x1b[2m── tear: session resurrected — the processes above ended; this is a new shell in the same place ──\x1b[0m\r\n";

struct Attached {
    id: u64,
    incarnation: Option<u64>,
    sink: Sink,
}

struct State {
    journal: Journal,
    attached: Option<Attached>,
    authority: Option<u64>,
}

enum Writer {
    Own(UnixStream),
    Attached,
    Silent,
}

enum Admission {
    Admit,
    Refuse { by: u64 },
    LeaveSilent { holding: u64 },
}

impl State {
    fn admission(&self, incarnation: Option<u64>) -> Admission {
        match incarnation {
            Some(n) => match self.authority {
                Some(by) if n < by => Admission::Refuse { by },
                _ => Admission::Admit,
            },
            None => match self.attached.as_ref().and_then(|a| a.incarnation) {
                Some(holding) => Admission::LeaveSilent { holding },
                None => Admission::Admit,
            },
        }
    }

    fn retire_failed(&mut self) {
        let Some(failed) = self.attached.take() else {
            return;
        };
        #[cfg(feature = "bench-probes")]
        if tear_types::probes::active(tear_types::probes::Fault::MuteSink) {
            tear_types::probes::bump(tear_types::probes::Counter::HolderSinksMuted, 1);
            failed.sink.leave_silent();
            return;
        }
        #[cfg(feature = "bench-probes")]
        tear_types::probes::bump(tear_types::probes::Counter::HolderSinksShut, 1);
        eprintln!(
            "tamotsu: a write to {} failed; its connection is shut down both ways, so the daemon re-attaches by offset",
            who(failed.incarnation)
        );
        drop(failed);
    }
}

fn retire_displaced(previous: Attached, by: Option<u64>) {
    match (previous.incarnation, by) {
        (Some(was), Some(by)) if was < by => {
            let mut sink = previous.sink;
            let _ = sink.send(&FromHolder::Displaced { by });
            #[cfg(feature = "bench-probes")]
            tear_types::probes::bump(tear_types::probes::Counter::HolderDisplaced, 1);
            eprintln!("tamotsu: incarnation {was} Displaced by incarnation {by}");
        }
        (Some(_), _) => drop(previous),
        (None, _) => {
            eprintln!(
                "tamotsu: a daemon that declares no incarnation was replaced by {}; it is left attached and silent, as a second attach always left it",
                who(by)
            );
            previous.sink.leave_silent();
        }
    }
}

fn who(incarnation: Option<u64>) -> String {
    incarnation.map_or_else(
        || "a daemon that declares no incarnation".to_string(),
        |n| format!("incarnation {n}"),
    )
}

fn replay(journal: &Journal, sink: &mut Sink, from: u64) -> io::Result<()> {
    let mut at = from;
    while let Ok(Some(chunk)) = journal.read_from(at, REPLAY_CHUNK) {
        at = chunk.at + chunk.data.len() as u64;
        sink.send(&FromHolder::Bytes {
            at: chunk.at,
            data: chunk.data,
        })?;
    }
    Ok(())
}

struct Holder {
    pane: PaneDir,
    socket: PathBuf,
    state: Mutex<State>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child_pid: Option<u32>,
    human_end: AtomicBool,
    pump_eof: AtomicBool,
    finalized: AtomicBool,
    next_conn: AtomicU64,
}

pub fn run(args: HoldArgs) -> anyhow::Result<()> {
    let _ = nix::unistd::setsid();
    let pane = PaneDir::at(&args.pane_dir);
    pane.create()?;
    let mut journal = pane.open_journal(args.bounds)?;
    if args.resurrected {
        journal.append(BANNER.as_bytes())?;
        journal.sync()?;
    }
    let listener = bind(&args.socket)?;
    let pair = NativePtySystem::default().openpty(PtySize {
        rows: args.rows.max(1),
        cols: args.cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new(&args.program);
    for a in &args.args {
        cmd.arg(a);
    }
    match &args.cwd {
        Some(d) => {
            cmd.cwd(d);
            cmd.env("PWD", d);
        }
        None => cmd.env_remove("PWD"),
    }
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let holder = Arc::new(Holder {
        pane,
        socket: args.socket.clone(),
        state: Mutex::new(State {
            journal,
            attached: None,
            authority: None,
        }),
        writer: Mutex::new(writer),
        master: Mutex::new(pair.master),
        child_pid: child.process_id(),
        human_end: AtomicBool::new(false),
        pump_eof: AtomicBool::new(false),
        finalized: AtomicBool::new(false),
        next_conn: AtomicU64::new(1),
    });

    {
        let h = Arc::clone(&holder);
        thread::Builder::new()
            .name("tamotsu-accept".into())
            .spawn(move || h.accept(&listener))?;
    }
    {
        let h = Arc::clone(&holder);
        thread::Builder::new()
            .name("tamotsu-tick".into())
            .spawn(move || h.tick())?;
    }
    {
        let h = Arc::clone(&holder);
        thread::Builder::new()
            .name("tamotsu-pump".into())
            .spawn(move || h.pump(reader))?;
    }

    let code = child
        .wait()
        .ok()
        .map(|s| i32::try_from(s.exit_code()).unwrap_or(i32::MAX));
    let deadline = Instant::now() + DRAIN_GRACE;
    while !holder.pump_eof.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let ending = if holder.human_end.load(Ordering::Acquire) {
        Ending::by_control()
    } else {
        Ending::Exited { code }
    };
    holder.finalize(ending)
}

fn bind(socket: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = socket.parent() {
        makimono::atomic::create_private_dir(parent)?;
    }
    match fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

impl Holder {
    fn pump(&self, mut reader: Box<dyn Read + Send>) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.ingest(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        self.pump_eof.store(true, Ordering::Release);
    }

    fn ingest(&self, bytes: &[u8]) {
        let mut st = self.state.lock();
        let at = st.journal.end();
        if let Err(e) = st.journal.append(bytes) {
            eprintln!("tamotsu: journal append failed ({e}); bytes stay live but are not durable");
            st.journal.note_lost(bytes.len() as u64);
        }
        let failed = st.attached.as_mut().is_some_and(|a| {
            a.sink
                .send(&FromHolder::Bytes {
                    at,
                    data: bytes.to_vec(),
                })
                .is_err()
        });
        if failed {
            st.retire_failed();
        }
    }

    fn accept(self: &Arc<Self>, listener: &UnixListener) {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let h = Arc::clone(self);
            let _ = thread::Builder::new()
                .name("tamotsu-conn".into())
                .spawn(move || h.serve(stream));
        }
    }

    fn serve(self: &Arc<Self>, stream: UnixStream) {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let Ok(read_half) = stream.try_clone() else {
            return;
        };
        let _ = stream.set_write_timeout(Some(SINK_WRITE_TIMEOUT));
        let mut writer = Writer::Own(stream);
        let mut r = BufReader::new(read_half);
        while let Ok(msg) = read_to_holder(&mut r) {
            match msg {
                ToHolder::Hello { proto } if proto != PROTO => {
                    self.reply(
                        id,
                        &mut writer,
                        &FromHolder::Refused {
                            reason: format!("holder speaks protocol {PROTO}, client {proto}"),
                        },
                    );
                    break;
                }
                ToHolder::Hello { .. } => self.reply(
                    id,
                    &mut writer,
                    &FromHolder::Hello {
                        proto: PROTO,
                        pid: std::process::id(),
                        child_pid: self.child_pid,
                    },
                ),
                ToHolder::Attach { from, incarnation } => {
                    writer = self.attach(id, writer, from, incarnation);
                }
                ToHolder::Write(data) => {
                    let mut w = self.writer.lock();
                    let _ = w.write_all(&data).and_then(|()| w.flush());
                }
                ToHolder::Resize { cols, rows } => {
                    let _ = self.master.lock().resize(PtySize {
                        rows: rows.max(1),
                        cols: cols.max(1),
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                ToHolder::End => self.end(),
                ToHolder::Status => {
                    let status = {
                        let st = self.state.lock();
                        HolderStatus {
                            pid: std::process::id(),
                            child_pid: self.child_pid,
                            start: st.journal.start(),
                            end: st.journal.end(),
                        }
                    };
                    self.reply(id, &mut writer, &FromHolder::Status(status));
                }
            }
        }
        let mut st = self.state.lock();
        if st.attached.as_ref().is_some_and(|a| a.id == id) {
            st.attached = None;
        }
    }

    fn reply(&self, id: u64, writer: &mut Writer, msg: &FromHolder) {
        match writer {
            Writer::Own(stream) => {
                let _ = write_from_holder(stream, msg);
            }
            Writer::Attached => {
                let mut st = self.state.lock();
                let failed = st
                    .attached
                    .as_mut()
                    .filter(|a| a.id == id)
                    .is_some_and(|a| a.sink.send(msg).is_err());
                if failed {
                    st.retire_failed();
                }
            }
            Writer::Silent => {}
        }
    }

    fn attach(&self, id: u64, writer: Writer, from: u64, incarnation: Option<u64>) -> Writer {
        let mut st = self.state.lock();
        let mut sink = match writer {
            Writer::Own(stream) => Sink::new(stream),
            Writer::Attached => match st.attached.take_if(|a| a.id == id) {
                Some(own) => own.sink,
                None => return Writer::Silent,
            },
            Writer::Silent => return Writer::Silent,
        };
        match st.admission(incarnation) {
            Admission::Refuse { by } => {
                let _ = sink.send(&FromHolder::Displaced { by });
                #[cfg(feature = "bench-probes")]
                tear_types::probes::bump(tear_types::probes::Counter::HolderDisplaced, 1);
                eprintln!(
                    "tamotsu: refused {}: Displaced by incarnation {by}",
                    who(incarnation)
                );
                return Writer::Silent;
            }
            Admission::LeaveSilent { holding } => {
                eprintln!(
                    "tamotsu: {} attached while incarnation {holding} holds the pane; it is left attached and silent",
                    who(incarnation)
                );
                sink.leave_silent();
                return Writer::Silent;
            }
            Admission::Admit => {}
        }
        let _ = st.journal.sync();
        if let Err(e) = replay(&st.journal, &mut sink, from) {
            eprintln!(
                "tamotsu: the replay to {} failed ({e}); its connection is shut down both ways",
                who(incarnation)
            );
            return Writer::Silent;
        }
        if let Some(n) = incarnation {
            st.authority = Some(st.authority.map_or(n, |held| held.max(n)));
        }
        let previous = st.attached.replace(Attached {
            id,
            incarnation,
            sink,
        });
        #[cfg(feature = "bench-probes")]
        tear_types::probes::bump(tear_types::probes::Counter::HolderAttaches, 1);
        eprintln!(
            "tamotsu: attached from offset {from} for {}",
            who(incarnation)
        );
        if let Some(previous) = previous {
            retire_displaced(previous, incarnation);
        }
        Writer::Attached
    }

    fn end(self: &Arc<Self>) {
        self.human_end.store(true, Ordering::Release);
        let _ = self.pane.write_tombstone(&Ending::by_control());
        self.signal(Signal::SIGHUP);
        let h = Arc::clone(self);
        let _ = thread::Builder::new()
            .name("tamotsu-end".into())
            .spawn(move || {
                thread::sleep(END_GRACE);
                if h.finalized.load(Ordering::Acquire) {
                    return;
                }
                h.signal(Signal::SIGKILL);
                thread::sleep(Duration::from_millis(500));
                h.finalize(Ending::by_control());
            });
    }

    fn signal(&self, sig: Signal) {
        let Some(pid) = self.child_pid.and_then(|p| i32::try_from(p).ok()) else {
            return;
        };
        let pid = Pid::from_raw(pid);
        let _ = killpg(pid, sig);
        let _ = kill(pid, sig);
    }

    fn tick(&self) {
        let mut n: u32 = 0;
        loop {
            thread::sleep(TICK);
            {
                let mut st = self.state.lock();
                let _ = st.journal.sync_if_due();
            }
            n = n.wrapping_add(1);
            if n % CWD_POLL_TICKS == 0 {
                if let Some(cwd) = self.child_pid.and_then(child_cwd) {
                    let _ = self.pane.write_polled_cwd(&cwd);
                }
            }
        }
    }

    fn finalize(&self, ending: Ending) -> ! {
        if self.finalized.swap(true, Ordering::AcqRel) {
            loop {
                thread::park();
            }
        }
        let _ = self.pane.write_tombstone(&ending);
        let recorded = self.pane.tombstone().ok().flatten().unwrap_or(ending);
        {
            let mut st = self.state.lock();
            let _ = st.journal.sync();
            if let Some(mut attached) = st.attached.take() {
                let _ = attached.sink.send(&FromHolder::Exited { ending: recorded });
            }
        }
        let _ = fs::remove_file(&self.socket);
        #[cfg(feature = "bench-probes")]
        let _ = makimono::probes::dump("holder");
        std::process::exit(0)
    }
}

#[cfg(target_os = "linux")]
fn child_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn child_cwd(_pid: u32) -> Option<String> {
    None
}
