use std::fs::OpenOptions;
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use makimono::{JournalBounds, PaneDir, Store};
use tamotsu::proto::{FromHolder, PROTO, ToHolder, read_from_holder, write_to_holder};
use tamotsu::{Authority, HeldPty, HoldArgs, HoldProgram, Revival};

pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Self {
        let p = PathBuf::from("/tmp").join(format!("tmt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let socket = self.0.join("h.sock");
        let me = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
        if let Some(pid) = holder_pid(&socket).filter(|pid| *pid > 1 && *pid != me) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
            let until = Instant::now() + Duration::from_secs(5);
            while HeldPty::probe(&socket) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn holder_program() -> PathBuf {
    std::env::var_os("TAMOTSU_HOLDER_BIN").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_tamotsu")),
        PathBuf::from,
    )
}

pub fn authority(dir: &Path) -> Authority {
    Authority::new(
        Store::open(dir.join("store"))
            .unwrap()
            .take_lease()
            .unwrap(),
    )
}

pub fn revival(dir: &Path) -> Revival {
    Revival {
        program: HoldProgram {
            program: holder_program(),
            prefix: vec![],
        },
        args: HoldArgs {
            pane_dir: dir.join("pane"),
            socket: dir.join("h.sock"),
            cols: 80,
            rows: 24,
            cwd: Some(dir.to_string_lossy().into_owned()),
            resurrected: false,
            bounds: JournalBounds::default(),
            program: "/bin/sh".into(),
            args: vec![],
        },
        env: vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("PS1".into(), "$ ".into()),
            ("TERM".into(), "dumb".into()),
        ],
    }
}

pub type Seen = Arc<Mutex<Vec<u8>>>;

pub fn sinks() -> (
    Seen,
    tamotsu::OnBytes,
    mpsc::Receiver<Option<i32>>,
    tamotsu::OnExit,
) {
    let seen: Seen = Arc::default();
    let s = Arc::clone(&seen);
    let (tx, rx) = mpsc::channel();
    (
        seen,
        Box::new(move |b: &[u8]| s.lock().unwrap().extend_from_slice(b)),
        rx,
        Box::new(move |code| {
            let _ = tx.send(code);
        }),
    )
}

pub fn eventually(holds: impl Fn() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if holds() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

pub fn wait_for(seen: &Seen, needle: &str) -> bool {
    eventually(|| String::from_utf8_lossy(&seen.lock().unwrap()).contains(needle))
}

pub fn gone(socket: &Path) -> bool {
    eventually(|| !HeldPty::probe(socket))
}

pub fn child_pid(socket: &Path) -> Option<u32> {
    let mut s = UnixStream::connect(socket).ok()?;
    write_to_holder(&mut s, &ToHolder::Hello { proto: PROTO }).ok()?;
    match read_from_holder(&mut s).ok()? {
        FromHolder::Hello { child_pid, .. } => child_pid,
        _ => None,
    }
}

pub fn holder_pid(socket: &Path) -> Option<i32> {
    let mut s = UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write_to_holder(&mut s, &ToHolder::Hello { proto: PROTO }).ok()?;
    match read_from_holder(&mut s).ok()? {
        FromHolder::Hello { pid, .. } => i32::try_from(pid).ok(),
        _ => None,
    }
}

pub fn spawn_holder(rv: &Revival) -> Child {
    let pane = PaneDir::at(&rv.args.pane_dir);
    pane.create().unwrap();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pane.holder_log())
        .unwrap();
    let child = Command::new(&rv.program.program)
        .args(&rv.program.prefix)
        .args(rv.args.to_argv())
        .env_clear()
        .envs(rv.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    assert!(
        eventually(|| HeldPty::probe(&rv.args.socket)),
        "the holder never answered"
    );
    child
}

pub fn holder_log(rv: &Revival) -> String {
    std::fs::read_to_string(PaneDir::at(&rv.args.pane_dir).holder_log()).unwrap_or_default()
}

pub fn attaches(rv: &Revival) -> usize {
    holder_log(rv)
        .matches("tamotsu: attached from offset")
        .count()
}

#[derive(Debug)]
pub enum Frame {
    Msg(FromHolder),
    Eof,
    Quiet,
}

pub struct Raw {
    s: UnixStream,
    r: BufReader<UnixStream>,
    pub next: u64,
    pub tail: Vec<u8>,
}

impl Raw {
    pub fn attached(socket: &Path, from: u64, incarnation: Option<u64>) -> Self {
        let s = UnixStream::connect(socket).unwrap();
        let mut raw = Self {
            r: BufReader::new(s.try_clone().unwrap()),
            s,
            next: from,
            tail: Vec::new(),
        };
        write_to_holder(&mut raw.s, &ToHolder::Hello { proto: PROTO }).unwrap();
        match raw.frame(Duration::from_secs(5)) {
            Frame::Msg(FromHolder::Hello { .. }) => {}
            other => panic!("expected Hello, got {other:?}"),
        }
        write_to_holder(&mut raw.s, &ToHolder::Attach { from, incarnation }).unwrap();
        raw
    }

    pub fn write(&mut self, bytes: &[u8]) {
        write_to_holder(&mut self.s, &ToHolder::Write(bytes.to_vec())).unwrap();
    }

    pub fn frame(&mut self, within: Duration) -> Frame {
        if self.s.set_read_timeout(Some(within)).is_err() {
            return Frame::Eof;
        }
        match read_from_holder(&mut self.r) {
            Ok(FromHolder::Bytes { at, data }) => {
                assert!(
                    at <= self.next,
                    "a gap: the holder sent offset {at} while {} was expected",
                    self.next
                );
                let end = at + data.len() as u64;
                if end > self.next {
                    let skip = usize::try_from(self.next - at).unwrap();
                    self.tail.extend_from_slice(&data[skip..]);
                    let keep = self.tail.len().saturating_sub(4096);
                    self.tail.drain(..keep);
                    self.next = end;
                }
                Frame::Msg(FromHolder::Bytes { at, data })
            }
            Ok(other) => Frame::Msg(other),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Frame::Quiet
            }
            Err(_) => Frame::Eof,
        }
    }

    pub fn until_quiet_or_eof(&mut self, quiet: Duration, deadline: Duration) -> Frame {
        let until = Instant::now() + deadline;
        while Instant::now() < until {
            match self.frame(quiet) {
                Frame::Msg(FromHolder::Bytes { .. }) => {}
                other => return other,
            }
        }
        Frame::Quiet
    }

    pub fn until_text(&mut self, needle: &str, deadline: Duration) -> bool {
        let until = Instant::now() + deadline;
        while Instant::now() < until {
            if String::from_utf8_lossy(&self.tail).contains(needle) {
                return true;
            }
            if let Frame::Eof = self.frame(Duration::from_millis(200)) {
                return false;
            }
        }
        String::from_utf8_lossy(&self.tail).contains(needle)
    }
}

pub fn status_end(socket: &Path) -> u64 {
    let mut s = UnixStream::connect(socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write_to_holder(&mut s, &ToHolder::Hello { proto: PROTO }).unwrap();
    let _ = read_from_holder(&mut s).unwrap();
    write_to_holder(&mut s, &ToHolder::Status).unwrap();
    loop {
        if let FromHolder::Status(st) = read_from_holder(&mut s).unwrap() {
            return st.end;
        }
    }
}
