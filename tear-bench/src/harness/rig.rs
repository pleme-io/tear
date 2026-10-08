use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tear_client::{Client, SubscribeHandle, Transport};
use tear_core::InProcess;
use tear_types::wire::{Request, Response, write_msg};
use tear_types::{MultiplexerControl, PaneId, SessionId, SessionSource};

use super::{COLS, ECHO_SCRIPT, FLOOD_SCRIPT, Harness, ROWS};
use crate::matrix::{Runtime, Variant};

pub enum Backend {
    Embedded(Arc<InProcess>),
    Daemon(Arc<Client>),
}

pub struct Rig {
    pub backend: Backend,
    pub transport: Option<Transport>,
    pub variant: Variant,
    pub label: String,
    pub daemon_pid: Option<i32>,
    pub auth: Option<String>,
    sessions: Mutex<Vec<SessionId>>,
}

pub type Chunks = Receiver<(Instant, Vec<u8>)>;

pub struct Sub {
    pub rx: Chunks,
    _handle: Option<SubscribeHandle>,
}

impl Rig {
    #[must_use]
    pub fn embedded(label: &str) -> Self {
        Self {
            backend: Backend::Embedded(Arc::new(InProcess::new())),
            transport: None,
            variant: Variant::EMBEDDED,
            label: label.to_string(),
            daemon_pid: None,
            auth: None,
            sessions: Mutex::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn daemon(
        client: Client,
        transport: Transport,
        variant: Variant,
        label: &str,
        pid: i32,
    ) -> Self {
        Self {
            backend: Backend::Daemon(Arc::new(client)),
            transport: Some(transport),
            variant,
            label: label.to_string(),
            daemon_pid: Some(pid),
            auth: None,
            sessions: Mutex::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.auth = token;
        self
    }

    #[must_use]
    pub fn runtime(&self) -> Runtime {
        match self.backend {
            Backend::Embedded(_) => Runtime::Embedded,
            Backend::Daemon(_) => Runtime::Daemon,
        }
    }

    #[must_use]
    pub fn ctl(&self) -> &dyn MultiplexerControl {
        match &self.backend {
            Backend::Embedded(i) => i.as_ref(),
            Backend::Daemon(c) => c.as_ref(),
        }
    }

    #[must_use]
    pub fn ctl_arc(&self) -> Arc<dyn MultiplexerControl> {
        match &self.backend {
            Backend::Embedded(i) => Arc::clone(i) as Arc<dyn MultiplexerControl>,
            Backend::Daemon(c) => Arc::clone(c) as Arc<dyn MultiplexerControl>,
        }
    }

    pub fn fresh_client(&self) -> io::Result<Arc<Client>> {
        let t = self
            .transport
            .clone()
            .ok_or_else(|| io::Error::other("an embedded rig has no transport"))?;
        Client::connect_transport_with_auth(t, self.auth.clone()).map(Arc::new)
    }

    pub fn new_pane(
        &self,
        name: &str,
        script: &str,
        extra: &[String],
    ) -> io::Result<(SessionId, PaneId)> {
        let mut args = vec!["-c".to_string(), script.to_string()];
        args.extend(extra.iter().cloned());
        let sid = self
            .ctl()
            .new_session_with_source_and_size(
                name,
                "/bin/sh",
                &args,
                SessionSource::Human,
                (COLS, ROWS),
            )
            .map_err(|e| io::Error::other(format!("new session: {e}")))?;
        if let Ok(mut v) = self.sessions.lock() {
            v.push(sid);
        }
        let sess = self
            .ctl()
            .get_session(sid)
            .map_err(|e| io::Error::other(format!("get session: {e}")))?;
        let pane = *sess
            .panes
            .keys()
            .next()
            .ok_or_else(|| io::Error::other("a new session has no pane"))?;
        Ok((sid, pane))
    }

    pub fn kill(&self, sid: SessionId) {
        let _ = self.ctl().kill_session(sid);
        if let Ok(mut v) = self.sessions.lock() {
            v.retain(|x| *x != sid);
        }
    }

    pub fn kill_all(&self) {
        let sids: Vec<SessionId> = self.sessions.lock().map(|v| v.clone()).unwrap_or_default();
        for sid in sids {
            self.kill(sid);
        }
    }

    pub fn subscribe(&self, pane: PaneId) -> io::Result<Sub> {
        let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
        match &self.backend {
            Backend::Embedded(i) => {
                let inner = i
                    .subscribe_pane_bytes(pane)
                    .map_err(|e| io::Error::other(format!("subscribe: {e}")))?;
                thread::Builder::new()
                    .name("tearbench-forward".into())
                    .spawn(move || {
                        while let Ok(v) = inner.recv() {
                            if tx.send((Instant::now(), v)).is_err() {
                                break;
                            }
                        }
                    })?;
                Ok(Sub { rx, _handle: None })
            }
            Backend::Daemon(c) => {
                let h = c
                    .subscribe_pane_bytes(pane, move |b: &[u8]| {
                        let _ = tx.send((Instant::now(), b.to_vec()));
                    })
                    .map_err(|e| io::Error::other(format!("subscribe: {e}")))?;
                Ok(Sub {
                    rx,
                    _handle: Some(h),
                })
            }
        }
    }

    pub fn wait_text(&self, pane: PaneId, needle: &str, timeout: Duration) -> io::Result<Duration> {
        let t0 = Instant::now();
        loop {
            if let Ok(snap) = self.ctl().pane_snapshot(pane)
                && snap.to_text().contains(needle)
            {
                return Ok(t0.elapsed());
            }
            if t0.elapsed() > timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timeout waiting for {needle:?}"),
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn echo_pane(&self, tag: &str) -> io::Result<(SessionId, PaneId, Sub)> {
        let (sid, pane) = self.new_pane(tag, ECHO_SCRIPT, &[])?;
        self.wait_text(pane, "READY", Duration::from_secs(10))?;
        let sub = self.subscribe(pane)?;
        drain(&sub.rx, Duration::from_millis(150));
        Ok((sid, pane, sub))
    }

    pub fn fill_pane(
        &self,
        h: &Harness,
        tag: &str,
        file: &std::path::Path,
        size: u64,
    ) -> io::Result<(SessionId, PaneId, Duration)> {
        let (sid, pane) =
            self.new_pane(tag, FLOOD_SCRIPT, &[file.to_string_lossy().into_owned()])?;
        self.wait_text(pane, "READY", Duration::from_secs(10))?;
        let sub = self.subscribe(pane)?;
        drain(&sub.rx, Duration::from_millis(150));
        let t0 = Instant::now();
        self.ctl()
            .send_keys(pane, b"g")
            .map_err(|e| io::Error::other(format!("trigger: {e}")))?;
        let mut got = 0u64;
        while got < size {
            match sub.rx.recv_timeout(Duration::from_secs(60)) {
                Ok((_, v)) => got += v.len() as u64,
                Err(_) => return Err(io::Error::other(format!("fill stalled at {got}/{size}"))),
            }
        }
        let dt = t0.elapsed();
        drop(sub);
        thread::sleep(Duration::from_millis(200));
        h.log(&format!(
            "{} {tag}: filled {size} B in {:.1} ms",
            self.label,
            dt.as_secs_f64() * 1e3
        ));
        Ok((sid, pane, dt))
    }
}

pub fn drain(rx: &Chunks, quiet: Duration) -> (usize, usize) {
    let mut frames = 0;
    let mut bytes = 0;
    while let Ok((_, v)) = rx.recv_timeout(quiet) {
        frames += 1;
        bytes += v.len();
    }
    (frames, bytes)
}

pub fn wait_byte(rx: &Chunks, b: u8, timeout: Duration) -> Option<Instant> {
    let until = Instant::now() + timeout;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        match rx.recv_timeout(left) {
            Ok((t, v)) if v.contains(&b) => return Some(t),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

pub trait RawStream: Read + Write + Send {
    fn timeout(&self, d: Option<Duration>);
}

impl RawStream for UnixStream {
    fn timeout(&self, d: Option<Duration>) {
        let _ = self.set_read_timeout(d);
    }
}

impl RawStream for TcpStream {
    fn timeout(&self, d: Option<Duration>) {
        let _ = self.set_read_timeout(d);
    }
}

pub fn raw_connect(t: &Transport) -> io::Result<Box<dyn RawStream>> {
    Ok(match t {
        Transport::Unix(p) => Box::new(UnixStream::connect(p)?),
        Transport::Tcp(a) => Box::new(TcpStream::connect(a)?),
    })
}

pub fn read_frame_raw(s: &mut dyn RawStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

pub struct FrameProbe {
    pub bytes: u64,
    pub header: Duration,
    pub whole: Duration,
}

pub fn raw_frame_probe(t: &Transport, req: &Request) -> io::Result<FrameProbe> {
    let mut st = raw_connect(t)?;
    st.timeout(Some(Duration::from_secs(60)));
    let t0 = Instant::now();
    write_msg(&mut st, req)?;
    if matches!(req, Request::Subscribe(_)) {
        read_frame_raw(st.as_mut())?;
    }
    let mut len = [0u8; 4];
    st.read_exact(&mut len)?;
    let header = t0.elapsed();
    let n = u64::from(u32::from_be_bytes(len));
    let copied = io::copy(&mut (&mut st).take(n), &mut io::sink())?;
    let whole = t0.elapsed();
    if copied != n {
        return Err(io::Error::other(format!("short frame {copied}/{n}")));
    }
    Ok(FrameProbe {
        bytes: n,
        header,
        whole,
    })
}

pub struct RawSub {
    pub rx: Chunks,
    pub first_len: u64,
    pub first_at: Duration,
}

pub fn raw_subscribe(t: &Transport, pane: PaneId) -> io::Result<RawSub> {
    let mut st = raw_connect(t)?;
    st.timeout(Some(Duration::from_secs(60)));
    let t0 = Instant::now();
    write_msg(&mut st, &Request::Subscribe(pane))?;
    read_frame_raw(st.as_mut())?;
    let mut len = [0u8; 4];
    st.read_exact(&mut len)?;
    let n = u64::from(u32::from_be_bytes(len));
    io::copy(&mut (&mut st).take(n), &mut io::sink())?;
    let first_at = t0.elapsed();
    st.timeout(None);
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("tearbench-raw-sub".into())
        .spawn(move || {
            while let Ok(buf) = read_frame_raw(st.as_mut()) {
                let now = Instant::now();
                match ciborium::de::from_reader::<Response, _>(&buf[..]) {
                    Ok(Response::PaneBytes(v)) => {
                        if tx.send((now, v)).is_err() {
                            return;
                        }
                    }
                    _ => return,
                }
            }
        })?;
    Ok(RawSub {
        rx,
        first_len: n,
        first_at,
    })
}
