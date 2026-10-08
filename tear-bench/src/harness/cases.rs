use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tear_core::pty::PtyHandle;
use tear_types::wire::{Request, Response, write_msg};
use tear_types::{
    BRACKETED_PASTE_CLOSE, BRACKETED_PASTE_OPEN, Durability, MultiplexerControl, PaneId,
    SessionSource,
};

use super::daemon::Daemon;
use super::rig::{Rig, drain, raw_frame_probe, raw_subscribe, wait_byte};
use super::{COLS, ECHO_SCRIPT, FLOOD_SCRIPT, Harness, PASTE_SCRIPT, ROWS, Tag, files};
use crate::matrix::{Case, Cell, Floor, Handover, Input, Metric, Remote, Runtime, TransportKind};
use crate::receipt::{Sample, Unit};

pub const SPIKE_NS: f64 = 3_000_000.0;
pub const HZ_GAP_MS: (f64, f64) = (990.0, 1_040.0);
pub const OFF_LAG_MS: (f64, f64) = (1_490.0, 1_540.0);
pub const FLOOD_BYTES: u64 = 64 * 1024 * 1024;
pub const KEYLOSS_ROWS: usize = 3_000;
pub const KEYLOSS_KEYS: usize = 20;
pub const TOKENED_KEYS: usize = 20;
pub const PASTE_BYTES: usize = 16 * 1024 * 1024;

fn ns(d: Duration) -> f64 {
    d.as_nanos() as f64
}

fn key_byte(i: usize) -> u8 {
    b'a' + u8::try_from(i % 26).unwrap_or(0)
}

#[must_use]
pub fn echo_case(rig: &Rig) -> Option<Case> {
    match (rig.runtime(), rig.variant.transport, rig.variant.durability) {
        (Runtime::Embedded, _, _) => Some(Case::C1),
        (Runtime::Daemon, TransportKind::Tcp, _) => None,
        (Runtime::Daemon, TransportKind::Unix, Durability::ProcessBound) => Some(Case::C2),
        (Runtime::Daemon, TransportKind::Unix, Durability::Held) => Some(Case::C4),
    }
}

fn is_canonical_bound(rig: &Rig) -> bool {
    rig.runtime() == Runtime::Daemon && rig.variant == crate::matrix::Variant::BOUND
}

fn is_canonical_held(rig: &Rig) -> bool {
    rig.runtime() == Runtime::Daemon && rig.variant == crate::matrix::Variant::HELD
}

pub fn rpc(h: &Harness, rig: &Rig) -> io::Result<()> {
    let (sid, pane, _sub) = rig.echo_pane("rpc")?;
    let ctl = rig.ctl();
    for _ in 0..300 {
        let _ = ctl.get_pane(pane);
    }
    let n = 3_000;
    for i in 0..n {
        let t0 = Instant::now();
        let ok = ctl.get_pane(pane).is_ok();
        let mut s =
            Sample::new("rpc", &rig.label, "get_pane", i, ns(t0.elapsed()), Unit::Ns).ok(ok);
        if is_canonical_bound(rig) {
            s = s.cell(Cell::new(Case::C2, Metric::RoundTrip));
        }
        h.emit(s);
    }
    for (name, count) in [
        ("list_sessions", 2_000),
        ("pane_snapshot", 1_000),
        ("cursor_keys_mode", 1_000),
    ] {
        for i in 0..count {
            let t0 = Instant::now();
            let ok = match name {
                "list_sessions" => ctl.list_sessions().is_ok(),
                "pane_snapshot" => ctl.pane_snapshot(pane).is_ok(),
                _ => ctl.pane_cursor_keys_mode(pane).is_ok(),
            };
            h.emit(Sample::new("rpc", &rig.label, name, i, ns(t0.elapsed()), Unit::Ns).ok(ok));
        }
    }
    rig.kill(sid);
    Ok(())
}

struct Keys<'a> {
    n: usize,
    gap: Duration,
    mode: &'a str,
    cell: Option<Cell>,
}

fn closed_loop(
    h: &Harness,
    rig: &Rig,
    pane: PaneId,
    sub: &super::rig::Sub,
    keys: &Keys<'_>,
) -> usize {
    let Keys { n, gap, mode, cell } = *keys;
    let ctl = rig.ctl();
    let mut lost = 0;
    for i in 0..n {
        let b = key_byte(i);
        let t0 = Instant::now();
        let mut deckm_ok = true;
        if mode == "mado-shaped" {
            deckm_ok = ctl.pane_cursor_keys_mode(pane).is_ok();
        }
        let sent = ctl.send_keys(pane, &[b]).is_ok();
        let echo = wait_byte(&sub.rx, b, Duration::from_secs(3));
        let (value, ok) = if let Some(t) = echo {
            (ns(t.saturating_duration_since(t0)), sent && deckm_ok)
        } else {
            lost += 1;
            (f64::NAN, false)
        };
        let mut s = Sample::new("echo", &rig.label, mode, i, value, Unit::Ns).ok(ok);
        if let Some(c) = cell {
            s = s.cell(c);
        }
        h.emit(s);
        if !gap.is_zero() {
            thread::sleep(gap);
        }
    }
    lost
}

pub fn echo(h: &Harness, rig: &Rig) -> io::Result<()> {
    let (sid, pane, sub) = rig.echo_pane("echo")?;
    for i in 0..100 {
        let b = key_byte(i);
        rig.ctl()
            .send_keys(pane, &[b])
            .map_err(|e| io::Error::other(e.to_string()))?;
        let _ = wait_byte(&sub.rx, b, Duration::from_secs(2));
    }
    let canonical =
        rig.runtime() == Runtime::Embedded || is_canonical_bound(rig) || is_canonical_held(rig);
    let case = echo_case(rig).filter(|_| canonical);
    let lost = closed_loop(
        h,
        rig,
        pane,
        &sub,
        &Keys {
            n: 1_500,
            gap: Duration::from_millis(2),
            mode: "plain-gap",
            cell: case.map(|c| Cell::new(c, Metric::EchoGap)),
        },
    );
    h.log(&format!("{} echo plain-gap lost={lost}", rig.label));
    let rpcs_before = client_rpcs();
    let keys = 1_000;
    let key_cell = is_canonical_bound(rig).then(|| Cell::new(Case::C12(Input::Keys), Metric::Key));
    let lost = closed_loop(
        h,
        rig,
        pane,
        &sub,
        &Keys {
            n: keys,
            gap: Duration::from_millis(2),
            mode: "mado-shaped",
            cell: key_cell,
        },
    );
    if let (Some(before), Some(after), true) = (rpcs_before, client_rpcs(), is_canonical_bound(rig))
    {
        h.emit(
            Sample::new(
                "echo",
                &rig.label,
                "rpcs-per-mado-key",
                0,
                (after - before) as f64 / keys as f64,
                Unit::Ratio,
            )
            .cell(Cell::new(Case::C12(Input::Keys), Metric::Rpcs))
            .detail("tear-client probe: ClientRpcs over the mado-shaped series"),
        );
    }
    h.log(&format!("{} echo mado-shaped lost={lost}", rig.label));
    let lost = closed_loop(
        h,
        rig,
        pane,
        &sub,
        &Keys {
            n: 1_000,
            gap: Duration::ZERO,
            mode: "plain-warm",
            cell: case.map(|c| Cell::new(c, Metric::EchoWarm)),
        },
    );
    h.log(&format!("{} echo plain-warm lost={lost}", rig.label));
    if let Some(Case::C2) = case {
        let plain = h
            .collected()
            .into_iter()
            .filter(|s| s.bench == "echo" && s.variant == rig.label && s.name == "plain-gap")
            .map(|s| s.value)
            .collect::<Vec<f64>>();
        h.emit_all(
            plain
                .into_iter()
                .enumerate()
                .map(|(i, v)| {
                    Sample::new(
                        &format!("floor:{}", Floor::PlainKeyEcho.name()),
                        &rig.label,
                        Floor::PlainKeyEcho.name(),
                        i,
                        v,
                        Unit::Ns,
                    )
                })
                .collect(),
        );
    }
    drop(sub);
    rig.kill(sid);
    Ok(())
}

fn client_rpcs() -> Option<u64> {
    client_count(ClientCount::Rpcs)
}

#[derive(Copy, Clone)]
enum ClientCount {
    Rpcs,
    Redials,
}

#[cfg(feature = "bench-probes")]
#[allow(clippy::unnecessary_wraps)]
fn client_count(c: ClientCount) -> Option<u64> {
    use tear_types::probes::{Counter, count};
    Some(count(match c {
        ClientCount::Rpcs => Counter::ClientRpcs,
        ClientCount::Redials => Counter::ClientRedials,
    }))
}

#[cfg(not(feature = "bench-probes"))]
fn client_count(_: ClientCount) -> Option<u64> {
    None
}

pub struct Series {
    pub sent: usize,
    pub echoes: Vec<(f64, f64)>,
}

impl Series {
    #[must_use]
    pub fn spikes(&self) -> Vec<f64> {
        self.echoes
            .iter()
            .filter(|(_, lat)| lat.is_finite() && *lat > SPIKE_NS)
            .map(|(t_ms, _)| *t_ms)
            .collect()
    }

    #[must_use]
    pub fn gaps_at_one_hertz(&self) -> (usize, usize) {
        let spikes = self.spikes();
        let gaps: Vec<f64> = spikes.windows(2).map(|w| w[1] - w[0]).collect();
        let hz = gaps
            .iter()
            .filter(|g| (HZ_GAP_MS.0..=HZ_GAP_MS.1).contains(*g))
            .count();
        (hz, gaps.len())
    }

    #[must_use]
    pub fn pairs_at(&self, lag_ms: (f64, f64)) -> usize {
        let spikes = self.spikes();
        spikes
            .iter()
            .enumerate()
            .map(|(i, a)| {
                spikes[i + 1..]
                    .iter()
                    .filter(|b| (lag_ms.0..=lag_ms.1).contains(&(*b - a)))
                    .count()
            })
            .sum()
    }
}

type Typed = (Vec<(Instant, bool)>, Vec<(Instant, u8)>, Instant);

fn type_open_loop(
    rig: &Rig,
    pane: PaneId,
    sub: &super::rig::Sub,
    n: usize,
    period: Duration,
) -> io::Result<Typed> {
    let ctl = rig.ctl_arc();
    let start = Instant::now() + Duration::from_millis(50);
    let sends: Arc<Mutex<Vec<(Instant, bool)>>> = Arc::new(Mutex::new(Vec::with_capacity(n)));
    let sends_t = Arc::clone(&sends);
    let sender = thread::Builder::new()
        .name("tearbench-series".into())
        .spawn(move || {
            for i in 0..n {
                let target = start + period * u32::try_from(i).unwrap_or(u32::MAX);
                let now = Instant::now();
                if target > now {
                    thread::sleep(target - now);
                }
                let ts = Instant::now();
                let ok = ctl.send_keys(pane, &[key_byte(i)]).is_ok();
                if let Ok(mut v) = sends_t.lock() {
                    v.push((ts, ok));
                }
            }
        })?;
    let mut recvs: Vec<(Instant, u8)> = Vec::with_capacity(n);
    let until = start + period * u32::try_from(n).unwrap_or(u32::MAX) + Duration::from_secs(10);
    while recvs.len() < n {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match sub.rx.recv_timeout(left) {
            Ok((t, v)) => recvs.extend(v.into_iter().map(|b| (t, b))),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
    sender
        .join()
        .map_err(|_| io::Error::other("series sender panicked"))?;
    let sends = sends.lock().map(|v| v.clone()).unwrap_or_default();
    Ok((sends, recvs, start))
}

fn record_series(h: &Harness, rig: &Rig, typed: &Typed) -> Series {
    let (sends, recvs, start) = typed;
    let mut echoes = Vec::with_capacity(sends.len());
    let mut mismatched = 0;
    for (i, (ts, ok)) in sends.iter().enumerate() {
        let t_ms = ts.saturating_duration_since(*start).as_secs_f64() * 1e3;
        let (lat, got) = recvs.get(i).map_or((f64::NAN, false), |(t, b)| {
            if *b != key_byte(i) {
                mismatched += 1;
            }
            (ns(t.saturating_duration_since(*ts)), true)
        });
        echoes.push((t_ms, lat));
        h.emit(
            Sample::new("series", &rig.label, "echo", i, lat, Unit::Ns)
                .ok(*ok && got)
                .detail(format!("t_send_ms={t_ms:.3}")),
        );
    }
    let out = Series {
        sent: sends.len(),
        echoes,
    };
    let spikes = out.spikes().len();
    let (hz, gaps) = out.gaps_at_one_hertz();
    let mut s = Sample::new("series", &rig.label, "spikes-over-3ms", 0, spikes as f64, Unit::Count).detail(format!(
        "{hz} of {gaps} inter-spike gaps at 990-1040 ms; n={}; received={}; mismatched={mismatched}",
        out.sent,
        recvs.len()
    ));
    if is_canonical_held(rig) {
        s = s.cell(Cell::new(Case::C4, Metric::Spikes));
    }
    h.emit(s);
    if rig.variant.journal == crate::matrix::JournalSync::PageCache {
        h.emit(Sample::new(
            &format!("floor:{}", Floor::PageCacheSeries.name()),
            &rig.label,
            Floor::PageCacheSeries.name(),
            0,
            spikes as f64,
            Unit::Count,
        ));
    }
    h.log(&format!(
        "{} series n={} received={} spikes={spikes} hz-gaps={hz}/{gaps} mismatched={mismatched}",
        rig.label,
        out.sent,
        recvs.len()
    ));
    out
}

pub fn series(h: &Harness, rig: &Rig, secs: u64, period: Duration) -> io::Result<Series> {
    let (sid, pane, sub) = rig.echo_pane("series")?;
    for i in 0..50 {
        let b = key_byte(i);
        rig.ctl()
            .send_keys(pane, &[b])
            .map_err(|e| io::Error::other(e.to_string()))?;
        let _ = wait_byte(&sub.rx, b, Duration::from_secs(2));
    }
    drain(&sub.rx, Duration::from_millis(100));
    let n = usize::try_from(u128::from(secs) * 1000 / period.as_millis().max(1)).unwrap_or(0);
    let typed = type_open_loop(rig, pane, &sub, n, period)?;
    let out = record_series(h, rig, &typed);
    drop(sub);
    rig.kill(sid);
    Ok(out)
}

pub fn contention(h: &Harness, rig: &Rig, hz: u32) -> io::Result<()> {
    let (sid, pane, sub) = rig.echo_pane("contention")?;
    let poll: Arc<dyn MultiplexerControl> = match rig.runtime() {
        Runtime::Daemon => rig.fresh_client()?,
        Runtime::Embedded => rig.ctl_arc(),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = Arc::clone(&stop);
    let polls = Arc::new(AtomicU64::new(0));
    let polls_t = Arc::clone(&polls);
    let period = Duration::from_nanos(1_000_000_000 / u64::from(hz.max(1)));
    let poller = thread::Builder::new()
        .name("tearbench-poller".into())
        .spawn(move || {
            let start = Instant::now();
            let mut i: u32 = 0;
            while !stop_t.load(Ordering::SeqCst) {
                let target = start + period * i;
                let now = Instant::now();
                if target > now {
                    thread::sleep(target - now);
                }
                let _ = poll.get_pane(pane);
                polls_t.fetch_add(1, Ordering::Relaxed);
                i = i.wrapping_add(1);
            }
        })?;
    thread::sleep(Duration::from_millis(200));
    let mode = format!("contended-{hz}hz");
    let lost = closed_loop(
        h,
        rig,
        pane,
        &sub,
        &Keys {
            n: 1_500,
            gap: Duration::from_millis(2),
            mode: &mode,
            cell: None,
        },
    );
    stop.store(true, Ordering::SeqCst);
    poller
        .join()
        .map_err(|_| io::Error::other("poller panicked"))?;
    h.log(&format!(
        "{} contention lost={lost} polls={}",
        rig.label,
        polls.load(Ordering::Relaxed)
    ));
    drop(sub);
    rig.kill(sid);
    Ok(())
}

pub struct Flood {
    pub bytes: u64,
    pub frames: u64,
    pub complete: bool,
}

fn flood_rep(h: &Harness, rig: &Rig, file: &Path, size: u64, rep: usize) -> io::Result<Flood> {
    let (sid, pane) = rig.new_pane(
        "flood",
        FLOOD_SCRIPT,
        &[file.to_string_lossy().into_owned()],
    )?;
    rig.wait_text(pane, "READY", Duration::from_secs(10))?;
    let sub = rig.subscribe(pane)?;
    drain(&sub.rx, Duration::from_millis(150));
    let pid = rig
        .daemon_pid
        .unwrap_or_else(|| i32::try_from(std::process::id()).unwrap_or(-1));
    let before = crate::seam::task(pid).unwrap_or_default();
    let t0 = Instant::now();
    rig.ctl()
        .send_keys(pane, b"g")
        .map_err(|e| io::Error::other(format!("trigger: {e}")))?;
    let mut got = 0u64;
    let mut frames = 0u64;
    let mut last = t0;
    let mut complete = true;
    while got < size {
        if let Ok((t, v)) = sub.rx.recv_timeout(Duration::from_secs(120)) {
            got += v.len() as u64;
            frames += 1;
            last = t;
        } else {
            complete = false;
            break;
        }
    }
    let dt = last.saturating_duration_since(t0);
    let after = crate::seam::task(pid).unwrap_or_default();
    drop(sub);
    rig.kill(sid);
    let flood = Flood {
        bytes: got,
        frames,
        complete,
    };
    emit_flood(h, rig, &flood, dt, (before, after), rep);
    Ok(flood)
}

fn emit_flood(
    h: &Harness,
    rig: &Rig,
    f: &Flood,
    dt: Duration,
    task: (crate::seam::TaskReading, crate::seam::TaskReading),
    rep: usize,
) {
    let (before, after) = task;
    let mib = (f.bytes as f64 / f64::from(1 << 20)).max(f64::MIN_POSITIVE);
    let detail = format!(
        "bytes={} frames={} complete={} rep={rep}",
        f.bytes, f.frames, f.complete
    );
    let mut frames = Sample::new(
        "flood",
        &rig.label,
        "frames-per-mib",
        rep,
        f.frames as f64 / mib,
        Unit::Ratio,
    )
    .ok(f.complete)
    .detail(detail.clone());
    let mut tput = Sample::new(
        "flood",
        &rig.label,
        "ns-per-mib",
        rep,
        ns(dt) / mib,
        Unit::NsPerMib,
    )
    .ok(f.complete)
    .detail(detail.clone());
    let mut rss = Sample::new(
        "flood",
        &rig.label,
        "rss-growth",
        rep,
        after.resident_bytes.saturating_sub(before.resident_bytes) as f64,
        Unit::Bytes,
    )
    .detail(format!(
        "rss {} -> {} B; cpu {:.2} s",
        before.resident_bytes,
        after.resident_bytes,
        after.cpu_ns.saturating_sub(before.cpu_ns) as f64 / 1e9
    ));
    if is_canonical_bound(rig) {
        frames = frames.cell(Cell::new(Case::C9, Metric::Frames));
        rss = rss.cell(Cell::new(Case::C9, Metric::Memory));
    }
    let canonical =
        rig.runtime() == Runtime::Embedded || is_canonical_bound(rig) || is_canonical_held(rig);
    if let Some(c) = echo_case(rig).filter(|_| canonical) {
        tput = tput.cell(Cell::new(c, Metric::Throughput));
    }
    h.emit(frames);
    h.emit(tput);
    h.emit(rss);
    h.emit(
        Sample::new(
            "flood",
            &rig.label,
            "frames",
            rep,
            f.frames as f64,
            Unit::Count,
        )
        .ok(f.complete)
        .detail(detail),
    );
    h.log(&format!(
        "{} flood rep {rep}: {} B in {:.1} ms = {:.1} MiB/s, {} frames",
        rig.label,
        f.bytes,
        dt.as_secs_f64() * 1e3,
        mib / dt.as_secs_f64().max(f64::MIN_POSITIVE),
        f.frames
    ));
}

pub fn flood(h: &Harness, rig: &Rig, reps: usize) -> io::Result<Vec<Flood>> {
    let file = h.files().join("flood_64m.txt");
    let size = files::log_file(&file, FLOOD_BYTES)?;
    let mut out = Vec::new();
    for rep in 0..reps {
        out.push(flood_rep(h, rig, &file, size, rep)?);
        thread::sleep(Duration::from_millis(1_500));
    }
    Ok(out)
}

fn attach_frames(h: &Harness, rig: &Rig, pane: PaneId, size: u64) {
    let Some(t) = rig.transport.clone() else {
        return;
    };
    for (kind, req) in [
        ("snapshot", Request::PaneSnapshot(pane)),
        ("replay", Request::Subscribe(pane)),
    ] {
        let name = format!("{kind}-frame-bytes");
        let s = match raw_frame_probe(&t, &req) {
            Ok(p) => {
                let s = Sample::new("attach", &rig.label, &name, 0, p.bytes as f64, Unit::Bytes)
                    .detail(format!(
                        "history={size} over16mib={}",
                        p.bytes > 16 * 1024 * 1024
                    ));
                if kind == "replay" && is_canonical_bound(rig) {
                    s.cell(Cell::new(Case::C7(Handover::Attach), Metric::Keyframe))
                } else {
                    s
                }
            }
            Err(e) => Sample::new("attach", &rig.label, &name, 0, f64::NAN, Unit::Bytes)
                .ok(false)
                .detail(format!("history={size} err={e}")),
        };
        h.emit(s);
    }
}

fn attach_embedded(h: &Harness, rig: &Rig, pane: PaneId, size: u64, rep: usize) -> io::Result<()> {
    let t0 = Instant::now();
    let snap = rig
        .ctl()
        .pane_snapshot(pane)
        .map_err(|e| io::Error::other(e.to_string()))?;
    let ansi = snap.to_ansi();
    let sub = rig.subscribe(pane)?;
    rig.ctl()
        .send_keys(pane, b"Q")
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mk = wait_byte(&sub.rx, b'Q', Duration::from_secs(10));
    h.emit(
        Sample::new(
            "attach",
            &rig.label,
            "embedded-first-key",
            rep,
            mk.map_or(f64::NAN, |t| ns(t.saturating_duration_since(t0))),
            Unit::Ns,
        )
        .ok(mk.is_some())
        .cell(Cell::new(Case::C1, Metric::Attach))
        .detail(format!(
            "history={size} ansi={} scrollback={}",
            ansi.len(),
            snap.scrollback.len()
        )),
    );
    Ok(())
}

fn attach_daemon(
    h: &Harness,
    rig: &Rig,
    pane: PaneId,
    size: u64,
    rep: usize,
    kind: &str,
) -> io::Result<()> {
    let client = rig.fresh_client()?;
    let t0 = Instant::now();
    let snap1 = (kind == "mado-shaped").then(|| client.pane_snapshot(pane).is_ok());
    let (tx, rx) = std::sync::mpsc::channel::<(Instant, Vec<u8>)>();
    let handle = client
        .subscribe_pane_bytes(pane, move |b: &[u8]| {
            let _ = tx.send((Instant::now(), b.to_vec()));
        })
        .map_err(|e| io::Error::other(format!("attach subscribe: {e}")))?;
    let first = rx.recv_timeout(Duration::from_secs(20));
    if kind == "mado-shaped" {
        let _ = client.pane_snapshot(pane);
    }
    let marker = rig.fresh_client()?;
    let _ = marker.send_keys(pane, b"Q");
    let mk = wait_byte(&rx, b'Q', Duration::from_secs(5));
    let status = match (&first, mk) {
        (Ok(_), Some(_)) => "ok".to_string(),
        (Ok(_), None) => "marker-lost".to_string(),
        (Err(_), Some(_)) => "no-replay-but-live".to_string(),
        (Err(_), None) => format!(
            "subscription-dead subscribers={}",
            marker.pane_subscriber_count(pane).unwrap_or(0)
        ),
    };
    let mut s = Sample::new(
        "attach",
        &rig.label,
        &format!("{kind}-first-key"),
        rep,
        mk.map_or(f64::NAN, |t| ns(t.saturating_duration_since(t0))),
        Unit::Ns,
    )
    .ok(status == "ok")
    .detail(format!(
        "history={size} status={status} first={} snap1={snap1:?}",
        first.as_ref().map_or(0, |(_, v)| v.len())
    ));
    if kind == "mado-shaped" && is_canonical_bound(rig) {
        s = s.cell(Cell::new(Case::C7(Handover::Attach), Metric::Attach));
    }
    h.emit(s);
    drop(handle);
    thread::sleep(Duration::from_millis(100));
    Ok(())
}

pub fn attach(h: &Harness, rig: &Rig, sizes: &[u64], reps: usize) -> io::Result<()> {
    for &target in sizes {
        let file = h.files().join(format!("hist_{target}.txt"));
        let size = files::log_file(&file, target)?;
        let (sid, pane, _) = rig.fill_pane(h, &format!("attach-{target}"), &file, size)?;
        attach_frames(h, rig, pane, size);
        for rep in 0..reps {
            match rig.runtime() {
                Runtime::Embedded => attach_embedded(h, rig, pane, size, rep)?,
                Runtime::Daemon => {
                    for kind in ["subscribe-only", "mado-shaped"] {
                        if let Err(e) = attach_daemon(h, rig, pane, size, rep, kind) {
                            h.log(&format!("attach {kind}: {e}"));
                        }
                    }
                }
            }
        }
        rig.kill(sid);
        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

pub fn cliff(h: &Harness, rig: &Rig, depths: &[usize]) -> io::Result<()> {
    for &rows in depths {
        let file = h.files().join(format!("rows_{rows}.txt"));
        let size = files::rows_file(&file, rows)?;
        let (sid, pane, _) = rig.fill_pane(h, &format!("cliff-{rows}"), &file, size)?;
        let frame = rig
            .transport
            .as_ref()
            .map(|t| raw_frame_probe(t, &Request::PaneSnapshot(pane)));
        if let Some(f) = &frame {
            let mut s = Sample::new(
                "cliff",
                &rig.label,
                "snapshot-frame-bytes",
                rows,
                f.as_ref().map_or(f64::NAN, |p| p.bytes as f64),
                Unit::Bytes,
            )
            .ok(f.is_ok())
            .detail(format!("rows={rows}"));
            if is_canonical_bound(rig) {
                s = s.cell(Cell::new(Case::C13, Metric::Keyframe));
            }
            h.emit(s);
        }
        for rep in 0..10 {
            let t0 = Instant::now();
            let r = match rig.runtime() {
                Runtime::Daemon => rig.fresh_client()?.pane_snapshot(pane),
                Runtime::Embedded => rig.ctl().pane_snapshot(pane),
            };
            let dt = t0.elapsed();
            let err = r
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            let mut s = Sample::new("cliff", &rig.label, "snapshot", rep, ns(dt), Unit::Ns)
                .ok(r.is_ok())
                .detail(format!(
                    "rows={rows} scrollback={} {err}",
                    r.as_ref().map_or(0, |s| s.scrollback.len())
                ));
            if is_canonical_bound(rig) {
                s = s.cell(Cell::new(Case::C13, Metric::Observer));
            }
            h.emit(s);
        }
        rig.kill(sid);
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn frame_bytes<T: serde::Serialize>(msg: &T) -> io::Result<u64> {
    let mut v = Vec::new();
    write_msg(&mut v, msg)?;
    Ok(v.len() as u64)
}

pub struct KeyLoss {
    pub delivered: usize,
    pub sent: usize,
    pub control_delivered: usize,
    pub control_sent: usize,
    pub snapshot_frame: u64,
    pub redials: Option<u64>,
}

impl KeyLoss {
    #[must_use]
    pub fn lost(&self) -> u64 {
        (self.sent - self.delivered) as u64
    }

    fn emit(&self, h: &Harness, label: &str, rows: usize, tag: Tag) {
        let cell = Cell::new(Case::C3, Metric::Loss);
        if let Some(n) = self.redials {
            tag.emit(
                h,
                Sample::new("keyloss", label, "redials", 0, n as f64, Unit::Count)
                    .cell(cell)
                    .detail(format!(
                        "tear-client probe: ClientRedials over {KEYLOSS_KEYS} mado-shaped keys at {rows} rows"
                    )),
            );
        }
        tag.emit(
            h,
            Sample::new("keyloss", label, "keys-lost", 0, self.lost() as f64, Unit::Count)
                .cell(cell)
                .detail(format!(
                    "{} of {} mado-shaped keys delivered at {rows} rows; control {} of {}; snapshot frame {} B",
                    self.delivered,
                    self.sent,
                    self.control_delivered,
                    self.control_sent,
                    self.snapshot_frame
                )),
        );
    }
}

pub fn keyloss(h: &Harness, rig: &Rig, rows: usize, tag: Tag) -> io::Result<KeyLoss> {
    let t = rig
        .transport
        .clone()
        .ok_or_else(|| io::Error::other("key loss needs a daemon"))?;
    let file = h.files().join(format!("rows_{rows}.txt"));
    let size = files::rows_file(&file, rows)?;
    let (sid, pane, _) = rig.fill_pane(h, &format!("keyloss-{rows}"), &file, size)?;
    let raw = raw_subscribe(&t, pane)?;
    h.log(&format!(
        "{} keyloss: replay frame {} B in {:.1} ms",
        rig.label,
        raw.first_len,
        raw.first_at.as_secs_f64() * 1e3
    ));
    let probe = raw_frame_probe(&t, &Request::PaneSnapshot(pane))?;
    let mut out = KeyLoss {
        delivered: 0,
        sent: 0,
        control_delivered: 0,
        control_sent: 0,
        snapshot_frame: probe.bytes,
        redials: None,
    };
    let snapshot_request = frame_bytes(&Request::PaneSnapshot(pane))?;
    let keys_request = frame_bytes(&Request::SendKeys {
        id: pane,
        bytes: vec![b'A'],
    })?;
    let keys_reply = frame_bytes(&Response::Ok)?;
    let snapshot_reply = 4 + probe.bytes;
    tag.emit(
        h,
        Sample::new(
            "keyloss",
            &rig.label,
            "bytes-per-mado-key",
            0,
            (snapshot_request + snapshot_reply + keys_request + keys_reply) as f64,
            Unit::Bytes,
        )
        .cell(Cell::new(Case::C12(Input::Keys), Metric::WireBytes))
        .detail(format!(
            "every frame of one mado-shaped key at {rows} rows, each with its 4 B length header: PaneSnapshot request {snapshot_request} B, its reply {snapshot_reply} B, SendKeys {keys_request} B, its reply {keys_reply} B; §2's convention (the SendKeys frame plus the snapshot body) reads {} B",
            keys_request + probe.bytes
        )),
    );
    for (phase, count) in [("send-only-control", 5usize), ("mado-shaped", KEYLOSS_KEYS)] {
        let client = rig.fresh_client()?;
        let redials_before = client_count(ClientCount::Redials);
        for i in 0..count {
            let b = if phase == "mado-shaped" {
                b'A' + u8::try_from(i).unwrap_or(0)
            } else {
                b'0' + u8::try_from(i).unwrap_or(0)
            };
            let deckm = if phase == "mado-shaped" {
                match client.pane_cursor_keys_mode(pane) {
                    Ok(v) => format!("ok:{v}"),
                    Err(e) => format!("err:{e}"),
                }
            } else {
                "skipped".into()
            };
            let sent = match client.send_keys(pane, &[b]) {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("err:{e}"),
            };
            let got = wait_byte(&raw.rx, b, Duration::from_millis(1_500)).is_some();
            if phase == "mado-shaped" {
                out.sent += 1;
                out.delivered += usize::from(got);
            } else {
                out.control_sent += 1;
                out.control_delivered += usize::from(got);
            }
            tag.emit(
                h,
                Sample::new(
                    "keyloss",
                    &rig.label,
                    phase,
                    i,
                    if got { 1.0 } else { 0.0 },
                    Unit::Flag,
                )
                .ok(got)
                .detail(format!("rows={rows} deckm={deckm} send={sent}")),
            );
            thread::sleep(Duration::from_millis(100));
        }
        if phase == "mado-shaped"
            && let (Some(before), Some(after)) =
                (redials_before, client_count(ClientCount::Redials))
        {
            out.redials = Some(after - before);
        }
    }
    out.emit(h, &rig.label, rows, tag);
    rig.kill(sid);
    Ok(out)
}

pub struct Tokened {
    pub sent: usize,
    pub delivered: usize,
    pub refusal: Option<String>,
}

impl Tokened {
    #[must_use]
    pub fn lost(&self) -> u64 {
        (self.sent - self.delivered) as u64
    }
}

pub fn tokened(h: &Harness, rig: &Rig, keys: usize, tag: Tag) -> io::Result<Tokened> {
    if rig.auth.is_none() {
        return Err(io::Error::other(
            "the tokened case needs a daemon started with a token",
        ));
    }
    let (sid, pane) = rig.new_pane("tokened", ECHO_SCRIPT, &[])?;
    rig.wait_text(pane, "READY", Duration::from_secs(10))?;
    let client = rig.fresh_client()?;
    let (tx, rx) = std::sync::mpsc::channel::<(Instant, Vec<u8>)>();
    let mut out = Tokened {
        sent: keys,
        delivered: 0,
        refusal: None,
    };
    match client.subscribe_pane_bytes(pane, move |b: &[u8]| {
        let _ = tx.send((Instant::now(), b.to_vec()));
    }) {
        Err(e) => out.refusal = Some(e.to_string()),
        Ok(handle) => {
            drain(&rx, Duration::from_millis(150));
            for i in 0..keys {
                let b = key_byte(i);
                client.send_keys(pane, &[b]).map_err(|e| {
                    io::Error::other(format!("send over the tokened connection: {e}"))
                })?;
                out.delivered +=
                    usize::from(wait_byte(&rx, b, Duration::from_millis(1_500)).is_some());
            }
            drop(handle);
        }
    }
    let mut s = Sample::new(
        "tokened",
        &rig.label,
        "keys-lost",
        0,
        out.lost() as f64,
        Unit::Count,
    )
    .detail(match &out.refusal {
        Some(e) => {
            format!("the subscription was refused, so no echo of {keys} keys reached it: {e}")
        }
        None => format!(
            "{} of {keys} echoes reached a subscription to a daemon that requires a token",
            out.delivered
        ),
    });
    if rig.variant.transport == TransportKind::Tcp {
        s = s.cell(Cell::new(Case::C6(Remote::Tcp), Metric::Loss));
    }
    tag.emit(h, s);
    h.log(&format!(
        "{} tokened: {} of {keys} delivered{}",
        rig.label,
        out.delivered,
        out.refusal
            .as_ref()
            .map(|e| format!("; subscribe refused: {e}"))
            .unwrap_or_default()
    ));
    rig.kill(sid);
    Ok(out)
}

#[must_use]
pub fn paste_body(len: usize) -> Vec<u8> {
    let mut state: u32 = 0x9e37_79b9;
    (0..len)
        .map(|i| {
            if i % 64 == 63 {
                b'\n'
            } else {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                b'a' + u8::try_from((state >> 24) % 26).unwrap_or(0)
            }
        })
        .collect()
}

const fn cksum_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 0x8000_0000 == 0 {
                c << 1
            } else {
                (c << 1) ^ 0x04c1_1db7
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

const CKSUM: [u32; 256] = cksum_table();

#[must_use]
pub fn posix_cksum(parts: &[&[u8]]) -> (u32, u64) {
    let step = |crc: u32, b: u8| (crc << 8) ^ CKSUM[usize::from(((crc >> 24) as u8) ^ b)];
    let mut crc = 0u32;
    let mut len = 0u64;
    for p in parts {
        crc = p.iter().fold(crc, |c, b| step(c, *b));
        len += p.len() as u64;
    }
    let mut n = len;
    while n != 0 {
        crc = step(crc, (n & 0xff) as u8);
        n >>= 8;
    }
    (!crc, len)
}

fn parse_cksum(text: &str) -> Option<(u32, u64)> {
    text.lines().rev().find_map(|line| {
        let mut words = line.split_whitespace();
        let crc = words.next()?.parse().ok()?;
        let len = words.next()?.parse().ok()?;
        words.next().is_none().then_some((crc, len))
    })
}

pub struct Pasted {
    pub expected: (u32, u64),
    pub got: Option<(u32, u64)>,
    pub took: Duration,
    pub sends: Vec<String>,
}

impl Pasted {
    #[must_use]
    pub fn missing(&self) -> u64 {
        self.got
            .map_or(self.expected.1, |(_, len)| self.expected.1.abs_diff(len))
    }

    #[must_use]
    pub fn mismatched(&self) -> u64 {
        u64::from(self.got.is_none_or(|g| g != self.expected))
    }
}

pub fn paste(h: &Harness, rig: &Rig, body_len: usize, tag: Tag) -> io::Result<Pasted> {
    let body = paste_body(body_len);
    let parts: [&[u8]; 3] = [BRACKETED_PASTE_OPEN, &body, BRACKETED_PASTE_CLOSE];
    let expected = posix_cksum(&parts);
    let (sid, pane) = rig.new_pane("paste", PASTE_SCRIPT, &[expected.1.to_string()])?;
    rig.wait_text(pane, "READY", Duration::from_secs(10))?;
    let sub = rig.subscribe(pane)?;
    drain(&sub.rx, Duration::from_millis(150));
    let t0 = Instant::now();
    let sends: Vec<String> = parts
        .iter()
        .map(|part| match rig.ctl().send_keys(pane, part) {
            Ok(()) => format!("{} B ok", part.len()),
            Err(e) => format!("{} B err: {e}", part.len()),
        })
        .collect();
    let mut text = String::new();
    let wait = if sends.iter().all(|s| s.ends_with(" ok")) {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(3)
    };
    let until = Instant::now() + wait;
    let mut got = None;
    while got.is_none() {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match sub.rx.recv_timeout(left) {
            Ok((_, v)) => {
                text.push_str(&String::from_utf8_lossy(&v));
                if text.ends_with('\n') {
                    got = parse_cksum(&text);
                }
            }
            Err(_) => break,
        }
    }
    let out = Pasted {
        expected,
        got,
        took: t0.elapsed(),
        sends,
    };
    let detail = format!(
        "mado-shaped paste of {} B ({} B body inside ESC[200~ … ESC[201~), sends [{}]; child cksum {}; expected {} {}; {:.1} ms",
        expected.1,
        body_len,
        out.sends.join(", "),
        out.got.map_or_else(
            || format!("never printed within {} s", wait.as_secs()),
            |(c, l)| format!("{c} {l}")
        ),
        expected.0,
        expected.1,
        out.took.as_secs_f64() * 1e3
    );
    let cell = is_canonical_bound(rig).then(|| Cell::new(Case::C12(Input::Paste), Metric::Loss));
    for (name, value) in [
        ("paste-bytes-missing", out.missing()),
        ("paste-checksum-mismatch", out.mismatched()),
    ] {
        let mut s = Sample::new("paste", &rig.label, name, 0, value as f64, Unit::Count)
            .detail(detail.clone());
        if let Some(c) = cell {
            s = s.cell(c);
        }
        tag.emit(h, s);
    }
    h.log(&format!("{} paste: {detail}", rig.label));
    drop(sub);
    rig.kill(sid);
    Ok(out)
}

pub struct AuditCount {
    pub keys: usize,
    pub key_writes: u64,
    pub lifecycle_requests: u64,
    pub lifecycle_writes: u64,
}

impl AuditCount {
    #[must_use]
    pub fn samples(&self, bench: &str, variant: &str) -> [Sample; 2] {
        [
            Sample::new(
                bench,
                variant,
                "audit-writes-during-keys",
                0,
                self.key_writes as f64,
                Unit::Count,
            )
            .detail(format!(
                "{} audit records appended while {} keys were typed and echoed",
                self.key_writes, self.keys
            )),
            Sample::new(
                bench,
                variant,
                "audit-writes-off-lifecycle",
                0,
                self.lifecycle_writes.abs_diff(self.lifecycle_requests) as f64,
                Unit::Count,
            )
            .detail(format!(
                "{} audit records for {} lifecycle requests (NewSession, SetInputPolicy, KillSession)",
                self.lifecycle_writes, self.lifecycle_requests
            )),
        ]
    }
}

fn audit_records(path: &Path) -> u64 {
    std::fs::read_to_string(path).map_or(0, |s| s.lines().count() as u64)
}

fn settle_records(path: &Path, at_least: u64) -> u64 {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let n = audit_records(path);
        if n >= at_least || Instant::now() > until {
            return n;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

pub fn audit(h: &Harness, d: &Daemon, rig: &Rig, keys: usize) -> io::Result<AuditCount> {
    let log = d.audit_log.clone().ok_or_else(|| {
        io::Error::other("the audit case needs a daemon started with an audit log")
    })?;
    let l0 = audit_records(&log);
    let (sid, pane, sub) = rig.echo_pane("audit")?;
    rig.ctl()
        .set_input_policy(pane, tear_types::InputPolicy::Free)
        .map_err(|e| io::Error::other(format!("set input policy: {e}")))?;
    let l1 = settle_records(&log, l0 + 2);
    for i in 0..keys {
        let b = key_byte(i);
        rig.ctl()
            .send_keys(pane, &[b])
            .map_err(|e| io::Error::other(e.to_string()))?;
        if wait_byte(&sub.rx, b, Duration::from_secs(3)).is_none() {
            return Err(io::Error::other(format!("key {i} never echoed")));
        }
    }
    let l2 = audit_records(&log);
    drop(sub);
    rig.kill(sid);
    let l3 = settle_records(&log, l2 + 1);
    let out = AuditCount {
        keys,
        key_writes: l2 - l1,
        lifecycle_requests: 3,
        lifecycle_writes: (l1 - l0) + (l3 - l2),
    };
    h.log(&format!(
        "{} audit: {} records during {keys} keys; {} for {} lifecycle requests",
        rig.label, out.key_writes, out.lifecycle_writes, out.lifecycle_requests
    ));
    Ok(out)
}

pub const PROBE_BACKED: &[Cell] = &[
    Cell::new(Case::C9, Metric::Loss),
    Cell::new(Case::C9, Metric::Allocations),
];

fn without_probes(cell: Cell) -> String {
    format!(
        "{} reaches PaneGrid through tear-core's bench-probes feature; this build has none",
        cell.name()
    )
}

pub fn probes_reach(cell: Cell) -> Result<(), String> {
    if cfg!(feature = "bench-probes") || !PROBE_BACKED.contains(&cell) {
        Ok(())
    } else {
        Err(without_probes(cell))
    }
}

tear_types::closed_vocabulary! {
    Split { Feeder => "feeder", OldSplitter => "old-splitter" }
}

#[cfg(feature = "bench-probes")]
pub fn split_losses(split: Split) -> io::Result<(u64, String)> {
    use tear_core::probes::{SPLIT_READ, SPLIT_TAILS, Splitter, split_losses};
    let lost = split_losses(match split {
        Split::Feeder => Splitter::Feeder,
        Split::OldSplitter => Splitter::Legacy,
    });
    let detail = if lost.is_empty() {
        format!(
            "0 of {} corpora lose a character when fed in {SPLIT_READ} B reads",
            SPLIT_TAILS.len()
        )
    } else {
        lost.iter()
            .map(|l| format!("{:?}: whole {:?}, read {:?}", l.tail, l.whole, l.read))
            .collect::<Vec<_>>()
            .join("; ")
    };
    Ok((lost.len() as u64, detail))
}

#[cfg(not(feature = "bench-probes"))]
pub fn split_losses(_split: Split) -> io::Result<(u64, String)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        without_probes(Cell::new(Case::C9, Metric::Loss)),
    ))
}

pub fn split(h: &Harness) -> io::Result<u64> {
    let (lost, detail) = split_losses(Split::Feeder)?;
    h.emit(
        Sample::new(
            "split",
            "tear-core",
            "corpora-losing-a-character",
            0,
            lost as f64,
            Unit::Count,
        )
        .cell(Cell::new(Case::C9, Metric::Loss))
        .detail(detail),
    );
    Ok(lost)
}

pub const ALLOC_COLS: usize = 163;
pub const ALLOC_SCROLLBACK: usize = 10_000;
pub const ALLOC_CHUNK: usize = 1_024;
pub const ALLOC_WARM_CHUNKS: usize = 20_000;
pub const ALLOC_MEASURED_CHUNKS: usize = 2_000;

#[must_use]
pub fn yes_output(bytes: usize) -> Vec<u8> {
    b"y\n".iter().copied().cycle().take(bytes).collect()
}

#[cfg(feature = "bench-probes")]
pub fn allocations(h: &Harness) -> io::Result<u64> {
    let data = yes_output((ALLOC_WARM_CHUNKS + ALLOC_MEASURED_CHUNKS) * ALLOC_CHUNK);
    let allocs = tear_core::probes::feed_allocations(
        &tear_core::probes::FeedPlan {
            cols: ALLOC_COLS,
            rows: usize::from(ROWS),
            scrollback: ALLOC_SCROLLBACK,
            chunk: ALLOC_CHUNK,
            warm_chunks: ALLOC_WARM_CHUNKS,
            measured_chunks: ALLOC_MEASURED_CHUNKS,
        },
        &data,
        crate::seam::allocations_during,
    );
    let per_kib = allocs as f64 / ALLOC_MEASURED_CHUNKS as f64;
    h.emit(
        Sample::new("allocations", "tear-core", "allocations-per-kib", 0, per_kib, Unit::Ratio)
            .cell(Cell::new(Case::C9, Metric::Allocations))
            .detail(format!(
                "{allocs} allocations over {ALLOC_MEASURED_CHUNKS} chunks of {ALLOC_CHUNK} B of `yes` after {ALLOC_WARM_CHUNKS} warm-up chunks; PaneGrid {ALLOC_COLS}x{ROWS}, scrollback {ALLOC_SCROLLBACK} rows"
            )),
    );
    Ok(allocs)
}

#[cfg(not(feature = "bench-probes"))]
pub fn allocations(_h: &Harness) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        without_probes(Cell::new(Case::C9, Metric::Allocations)),
    ))
}

pub fn startup(h: &Harness, variant: crate::matrix::Variant, reps: usize) -> io::Result<()> {
    let bound = variant.durability == Durability::ProcessBound;
    for rep in 0..reps {
        let mut d = Daemon::start(h, variant, &format!("startup{rep}"))?;
        h.emit(
            d.ready_sample("startup", rep)
                .cell(Cell::new(Case::C8, Metric::Startup)),
        );
        let client = d.client()?;
        let mut audited = rep > 0;
        for k in 0..5 {
            let tc = Instant::now();
            let sid = client
                .new_session_with_source_and_size(
                    &format!("startup-{k}"),
                    "/bin/sh",
                    &["-c".to_string(), ECHO_SCRIPT.to_string()],
                    SessionSource::Human,
                    (COLS, ROWS),
                )
                .map_err(|e| io::Error::other(e.to_string()))?;
            let create = tc.elapsed();
            if !audited {
                d.require_isolated(h)?;
                audited = true;
            }
            let s = if bound {
                Sample::new(
                    &format!("floor:{}", Floor::BoundNewSession.name()),
                    &d.label,
                    Floor::BoundNewSession.name(),
                    rep * 5 + k,
                    ns(create),
                    Unit::Ns,
                )
            } else {
                Sample::new(
                    "startup",
                    &d.label,
                    "new-session",
                    rep * 5 + k,
                    ns(create),
                    Unit::Ns,
                )
                .cell(Cell::new(Case::C8, Metric::NewSession))
            };
            h.emit(s);
            let _ = client.kill_session(sid);
        }
        thread::sleep(Duration::from_millis(300));
        d.stop(h);
    }
    Ok(())
}

pub fn restart(
    h: &Harness,
    variant: crate::matrix::Variant,
    history: u64,
    reps: usize,
) -> io::Result<()> {
    let mut d = Daemon::start(h, variant, "restart-a")?;
    let rig = Rig::daemon(d.client()?, d.transport.clone(), variant, &d.label, d.pid);
    let file = h.files().join(format!("hist_{history}.txt"));
    let size = files::log_file(&file, history)?;
    let (sid, pane, _) = rig.fill_pane(h, "restart", &file, size)?;
    d.require_isolated(h)?;
    thread::sleep(Duration::from_millis(1_500));
    drop(rig);
    for rep in 0..reps {
        d.stop_daemon_only(h);
        let t0 = Instant::now();
        d = Daemon::start(h, variant, &format!("restart-b{rep}"))?;
        h.emit(
            d.ready_sample("restart", rep)
                .cell(Cell::new(Case::C8, Metric::Restart)),
        );
        let c = d.client()?;
        let found = c
            .list_sessions()
            .is_ok_and(|v| v.iter().any(|x| x.id == sid));
        let raw = raw_subscribe(&d.transport, pane);
        let tm = Instant::now();
        let sent = c.send_keys(pane, b"Q").is_ok();
        let mk = raw
            .as_ref()
            .ok()
            .and_then(|r| wait_byte(&r.rx, b'Q', Duration::from_secs(60)));
        h.emit(
            Sample::new(
                "restart",
                &d.label,
                "live-after-restart",
                rep,
                mk.map_or(f64::NAN, |t| ns(t.saturating_duration_since(t0))),
                Unit::Ns,
            )
            .ok(mk.is_some() && found && sent)
            .cell(Cell::new(Case::C7(Handover::Readopt), Metric::Restart))
            .detail(format!(
                "journal={} found={found} replay={} key={:?}",
                d.journal_bytes(),
                raw.as_ref().map_or(0, |r| r.first_len),
                mk.map(|t| t.saturating_duration_since(tm))
            )),
        );
        thread::sleep(Duration::from_millis(500));
    }
    if let Ok(c) = d.client() {
        let _ = c.kill_session(sid);
    }
    thread::sleep(Duration::from_millis(500));
    d.stop(h);
    Ok(())
}

pub fn connect(h: &Harness, rig: &Rig, reps: usize) -> io::Result<()> {
    let (sid, pane, _sub) = rig.echo_pane("connect")?;
    let t = rig
        .transport
        .clone()
        .ok_or_else(|| io::Error::other("connect needs a daemon"))?;
    let cell = (rig.variant.transport == TransportKind::Tcp)
        .then(|| Cell::new(Case::C6(Remote::Tcp), Metric::RoundTrip));
    for rep in 0..reps {
        let t0 = Instant::now();
        let c = tear_client::Client::connect_transport(t.clone())?;
        let conn = t0.elapsed();
        let (tx, rx) = std::sync::mpsc::channel::<Instant>();
        let t1 = Instant::now();
        let handle = c
            .subscribe_pane_bytes(pane, move |_b: &[u8]| {
                let _ = tx.send(Instant::now());
            })
            .map_err(|e| io::Error::other(e.to_string()))?;
        let sub = t1.elapsed();
        let first = rx.recv_timeout(Duration::from_secs(5)).ok();
        let mut s = Sample::new("connect", &rig.label, "connect", rep, ns(conn), Unit::Ns);
        if let Some(c) = cell {
            s = s.cell(c);
        }
        h.emit(s);
        h.emit(
            Sample::new("connect", &rig.label, "subscribe", rep, ns(sub), Unit::Ns).detail(
                format!(
                    "first-frame={:?}",
                    first.map(|f| f.saturating_duration_since(t1))
                ),
            ),
        );
        drop(handle);
        drop(c);
        thread::sleep(Duration::from_millis(20));
    }
    rig.kill(sid);
    Ok(())
}

pub struct Stall {
    pub delivered: u64,
    pub journaled: u64,
    pub stall_for: Duration,
}

impl Stall {
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.journaled.saturating_sub(self.delivered)
    }

    #[must_use]
    pub fn sample(&self, variant: &str, after: AfterStall) -> Sample {
        Sample::new(
            "stall",
            variant,
            "bytes-lost",
            0,
            self.lost() as f64,
            Unit::Bytes,
        )
        .detail(format!(
            "stalled the daemon {:.1} s under a 64 KiB / 50 ms producer, then {}: delivered {} B while the journal grew {} B",
            self.stall_for.as_secs_f64(),
            after.describe(),
            self.delivered,
            self.journaled
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterStall {
    Silence,
    ReadEdges,
}

impl AfterStall {
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            AfterStall::Silence => "no read or key reached the daemon",
            AfterStall::ReadEdges => "a snapshot read every 250 ms",
        }
    }
}

pub const STALL_SETTLE: Duration = Duration::from_secs(1);
pub const STALL_DEADLINE: Duration = Duration::from_secs(40);

pub fn stall(
    _h: &Harness,
    d: &Daemon,
    rig: &Rig,
    stall_for: Duration,
    after: AfterStall,
) -> io::Result<Stall> {
    let me = std::env::current_exe()?;
    let producing = Duration::from_secs(1) + stall_for + Duration::from_secs(2);
    let script = format!(
        "stty raw -echo; printf READY; dd bs=1 count=1 of=/dev/null 2>/dev/null; '{}' floor-peer producer --chunk 65536 --period-ms 50 --secs {} --overwrite; exec cat",
        me.display(),
        producing.as_secs()
    );
    let (sid, pane) = rig.new_pane("stall", &script, &[])?;
    rig.wait_text(pane, "READY", Duration::from_secs(10))?;
    let sub = rig.subscribe(pane)?;
    rig.ctl()
        .send_keys(pane, b"g")
        .map_err(|e| io::Error::other(format!("start the producer: {e}")))?;
    let started = Instant::now();
    let received = Arc::new(AtomicU64::new(0));
    let r2 = Arc::clone(&received);
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let counter = thread::Builder::new()
        .name("tearbench-stall-count".into())
        .spawn(move || {
            while !s2.load(Ordering::SeqCst) {
                if let Ok((_, v)) = sub.rx.recv_timeout(Duration::from_millis(50)) {
                    r2.fetch_add(v.len() as u64, Ordering::SeqCst);
                }
            }
        })?;
    thread::sleep(Duration::from_secs(1));
    let r0 = received.load(Ordering::SeqCst);
    let j0 = d.journal_bytes();
    kill(Pid::from_raw(d.pid), Signal::SIGSTOP).map_err(io::Error::from)?;
    thread::sleep(stall_for);
    kill(Pid::from_raw(d.pid), Signal::SIGCONT).map_err(io::Error::from)?;
    let deadline = Instant::now() + STALL_DEADLINE;
    let mut last = (d.journal_bytes(), received.load(Ordering::SeqCst));
    let mut steady_since = Instant::now();
    while Instant::now() < deadline {
        if after == AfterStall::ReadEdges
            && let Ok(c) = rig.fresh_client()
        {
            let _ = c.pane_snapshot(pane);
        }
        thread::sleep(Duration::from_millis(250));
        let now = (d.journal_bytes(), received.load(Ordering::SeqCst));
        if now != last {
            last = now;
            steady_since = Instant::now();
        } else if started.elapsed() > producing + Duration::from_secs(1)
            && steady_since.elapsed() >= STALL_SETTLE
        {
            break;
        }
    }
    stop.store(true, Ordering::SeqCst);
    let _ = counter.join();
    if let Ok(c) = rig.fresh_client() {
        let _ = c.kill_session(sid);
    }
    rig.kill(sid);
    Ok(Stall {
        delivered: last.1 - r0,
        journaled: last.0.saturating_sub(j0),
        stall_for,
    })
}

pub struct Twin {
    pub authorities: u64,
    pub attaches: Option<usize>,
    pub displaced_logged: bool,
}

impl Twin {
    #[must_use]
    pub fn sample(&self, variant: &str, window: Duration) -> Sample {
        Sample::new(
            "twin",
            variant,
            "authorities",
            0,
            self.authorities as f64,
            Unit::Count,
        )
        .detail(format!(
            "two daemons on one store for {} s under a trickle: the pane took input through {} of them; holder attaches {}; the first daemon logged Displaced: {}",
            window.as_secs(),
            self.authorities,
            self.attaches
                .map_or_else(|| "unlogged (a holder before R4)".to_string(), |n| n.to_string()),
            self.displaced_logged
        ))
    }
}

fn holder_log_of(root: &Path, pane: PaneId) -> Option<String> {
    let want = pane.to_string();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n.to_string_lossy() == want) {
                    let log = p.join("holder.log");
                    if log.exists() {
                        return std::fs::read_to_string(log).ok();
                    }
                }
                stack.push(p);
            }
        }
    }
    None
}

pub fn twin(
    h: &Harness,
    first: &Daemon,
    rig: &Rig,
    second: &super::daemon::Options,
    window: Duration,
) -> io::Result<Twin> {
    let me = std::env::current_exe()?;
    let script = format!(
        "stty raw -echo; printf READY; dd bs=1 count=1 of=/dev/null 2>/dev/null; '{}' floor-peer producer --chunk 512 --period-ms 50 --secs {} --overwrite; exec cat",
        me.display(),
        window.as_secs() + 30
    );
    let (sid, pane) = rig.new_pane("twin", &script, &[])?;
    rig.wait_text(pane, "READY", Duration::from_secs(10))?;
    rig.ctl()
        .send_keys(pane, b"g")
        .map_err(|e| io::Error::other(format!("start the producer: {e}")))?;
    let opts = super::daemon::Options {
        socket_suffix: Some("second".into()),
        ..second.clone()
    };
    let mut other = Daemon::start_with(h, first.variant, "twin-second", &opts)?;
    let outcome = (|| {
        let c2 = other.client()?;
        let adopted = Instant::now() + Duration::from_secs(10);
        while c2.get_pane(pane).is_err() {
            if Instant::now() > adopted {
                return Err(io::Error::other("the second daemon never adopted the pane"));
            }
            thread::sleep(Duration::from_millis(50));
        }
        let until = Instant::now() + window;
        while Instant::now() < until {
            let _ = rig.ctl().pane_snapshot(pane);
            let _ = c2.pane_snapshot(pane);
            thread::sleep(Duration::from_millis(200));
        }
        let takes_input =
            |c: io::Result<tear_client::Client>| c.is_ok_and(|c| c.send_keys(pane, b"").is_ok());
        let authorities =
            u64::from(takes_input(first.client())) + u64::from(takes_input(other.client()));
        let _ = other.client().map(|c| c.kill_session(sid));
        let log = holder_log_of(&first.iso, pane);
        Ok(Twin {
            authorities,
            attaches: log
                .as_deref()
                .filter(|l| l.contains("tamotsu: attached"))
                .map(|l| l.matches("tamotsu: attached from offset").count()),
            displaced_logged: std::fs::read_to_string(&first.log)
                .is_ok_and(|l| l.contains("Displaced")),
        })
    })();
    other.stop_daemon_only(h);
    rig.kill(sid);
    outcome
}

pub fn producer_peer(args: &[String]) -> io::Result<()> {
    use std::io::Write;
    let chunk = usize::try_from(crate::floor::flag_u64(args, "--chunk", 65_536)?).unwrap_or(65_536);
    let period = Duration::from_millis(crate::floor::flag_u64(args, "--period-ms", 50)?);
    let secs = crate::floor::flag_u64(args, "--secs", 30)?;
    let eol = if args.iter().any(|a| a == "--overwrite") {
        b'\r'
    } else {
        b'\n'
    };
    let buf: Vec<u8> = (0..chunk)
        .map(|i| if i % 80 == 79 { eol } else { b'x' })
        .collect();
    let mut out = io::stdout().lock();
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        out.write_all(&buf)?;
        out.flush()?;
        thread::sleep(period);
    }
    Ok(())
}

pub fn ptyraw(h: &Harness, reps: usize) -> io::Result<()> {
    let file = h.files().join("flood_64m.txt");
    let size = files::log_file(&file, FLOOD_BYTES)?;
    for rep in 0..reps {
        let counter = Arc::new(AtomicU64::new(0));
        let reads = Arc::new(AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::channel::<Instant>();
        let (c2, r2) = (Arc::clone(&counter), Arc::clone(&reads));
        let tx = Mutex::new(Some(tx));
        let on_bytes = Box::new(move |b: &[u8]| {
            let n = c2.fetch_add(b.len() as u64, Ordering::SeqCst) + b.len() as u64;
            r2.fetch_add(1, Ordering::SeqCst);
            if n >= size
                && let Some(t) = tx.lock().ok().and_then(|mut g| g.take())
            {
                let _ = t.send(Instant::now());
            }
        });
        let env: Vec<(String, String)> = std::env::vars().collect();
        let args = vec![
            "-c".to_string(),
            "stty raw -echo; dd bs=1 count=1 of=/dev/null 2>/dev/null; cat \"$0\"; exec cat"
                .to_string(),
            file.to_string_lossy().into_owned(),
        ];
        let pty = PtyHandle::spawn(
            "/bin/sh",
            &args,
            None,
            &env,
            portable_pty::PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            },
            on_bytes,
            Box::new(|_| {}),
        )
        .map_err(|e| io::Error::other(format!("pty spawn: {e}")))?;
        thread::sleep(Duration::from_millis(400));
        counter.store(0, Ordering::SeqCst);
        reads.store(0, Ordering::SeqCst);
        let t0 = Instant::now();
        pty.write(b"g")?;
        let t1 = rx
            .recv_timeout(Duration::from_secs(120))
            .map_err(|_| io::Error::other("ptyraw stalled"))?;
        let dt = t1.saturating_duration_since(t0);
        let got = counter.load(Ordering::SeqCst);
        let n = reads.load(Ordering::SeqCst);
        h.emit(
            Sample::new(
                "ptyraw",
                "tear-core-pty",
                "ns-per-mib",
                rep,
                ns(dt) / (got as f64 / f64::from(1 << 20)),
                Unit::NsPerMib,
            )
            .detail(format!(
                "{got} B in {n} reads, {:.1} B per read",
                got as f64 / n.max(1) as f64
            )),
        );
        drop(pty);
        thread::sleep(Duration::from_millis(300));
    }
    Ok(())
}

pub fn wire(h: &Harness, tag: Tag) -> io::Result<Vec<(Cell, f64)>> {
    let file = h.files().join("hist_1048576.txt");
    files::log_file(&file, 1_048_576)?;
    let text = std::fs::read(&file)?;
    let cell = Cell::new(Case::C2, Metric::WireBytes);
    let mut out = Vec::new();
    for chunk in [1usize, 16, 1_024, 4_096, 65_536] {
        let payload = text[..chunk.min(text.len())].to_vec();
        let mut enc = Vec::new();
        write_msg(&mut enc, &Response::PaneBytes(payload.clone()))?;
        let per_kib = enc.len() as f64 * 1024.0 / payload.len() as f64;
        let mut s = Sample::new(
            "wire",
            "cbor",
            &format!("pane-bytes-{chunk}"),
            0,
            per_kib,
            Unit::Ratio,
        )
        .detail(format!(
            "{} B encoded for {} B of output, per KiB",
            enc.len(),
            payload.len()
        ));
        if chunk == 1_024 {
            s = s.cell(cell);
            out.push((cell, per_kib));
        }
        tag.emit(h, s);
    }
    Ok(out)
}

pub const CODEC_SAMPLES: usize = 400;

pub fn codec(h: &Harness, tag: Tag, n: usize) -> io::Result<Vec<(Cell, f64)>> {
    use crate::floor::ser::{FRAME_BYTES, payload, timed};
    crate::seam::apply_process_band(crate::matrix::Band::Interactive)?;
    let msg = Response::PaneBytes(payload(FRAME_BYTES));
    let body = tear_types::wire::encode(&msg)?.len();
    let measured = timed(FRAME_BYTES, n, || {
        let e = tear_types::wire::encode(std::hint::black_box(&msg))?;
        let back: Response = ciborium::de::from_reader(e.as_slice())
            .map_err(|err| io::Error::other(err.to_string()))?;
        std::hint::black_box(back);
        Ok(())
    });
    crate::seam::apply_process_band(crate::matrix::Band::Default)?;
    let cell = Cell::new(Case::C2, Metric::Encodes);
    let mut out = Vec::with_capacity(n);
    for (i, v) in measured?.into_iter().enumerate() {
        tag.emit(
            h,
            Sample::new(
                "codec",
                "tear-types",
                "pane-bytes-64KiB-round-trip",
                i,
                v,
                Unit::Ns,
            )
            .cell(cell)
            .detail(format!("{body} B body for {FRAME_BYTES} B of output")),
        );
        out.push((cell, v));
    }
    Ok(out)
}

pub fn replay_file(h: &Harness, path: &Path) -> io::Result<()> {
    let size = std::fs::metadata(path)?.len();
    let rig = Rig::embedded("replay-file");
    let (sid, pane, dt) = rig.fill_pane(h, "replay-file", path, size)?;
    let snap = rig
        .ctl()
        .pane_snapshot(pane)
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut enc = Vec::new();
    write_msg(&mut enc, &Response::PaneSnapshot(snap.clone()))?;
    let ansi = snap.to_ansi();
    let mut enc2 = Vec::new();
    write_msg(&mut enc2, &Response::PaneBytes(ansi.clone()))?;
    for (name, v) in [
        ("snapshot-frame-bytes", enc.len()),
        ("replay-ansi-bytes", ansi.len()),
        ("replay-frame-bytes", enc2.len()),
        ("scrollback-rows", snap.scrollback.len()),
    ] {
        h.emit(
            Sample::new("replay-file", "embedded", name, 0, v as f64, Unit::Bytes).detail(format!(
                "journal={size} fill_ms={:.1}",
                dt.as_secs_f64() * 1e3
            )),
        );
    }
    rig.kill(sid);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_cksum_matches_the_posix_check_value() {
        assert_eq!(posix_cksum(&[b"123456789"]), (930_766_865, 9));
        assert_eq!(posix_cksum(&[b"1234", b"56789"]), (930_766_865, 9));
        assert_eq!(posix_cksum(&[]), (4_294_967_295, 0));
    }

    #[test]
    fn the_child_s_cksum_line_is_read_from_the_stream_tail() {
        assert_eq!(parse_cksum("READY\n930766865 9\n"), Some((930_766_865, 9)));
        assert_eq!(parse_cksum("noise"), None);
        assert_eq!(parse_cksum("1 2 3\n"), None);
    }

    #[test]
    fn a_paste_body_is_printable_and_never_frames_itself() {
        let body = paste_body(4_096);
        assert_eq!(body.len(), 4_096);
        assert!(body.iter().all(|b| b.is_ascii_lowercase() || *b == b'\n'));
        assert!(
            !body
                .windows(BRACKETED_PASTE_CLOSE.len())
                .any(|w| w == BRACKETED_PASTE_CLOSE)
        );
    }

    fn series(spikes_ms: &[f64]) -> Series {
        let mut echoes: Vec<(f64, f64)> = (0..1_500)
            .map(|i| (f64::from(i) * 10.0, 150_000.0))
            .collect();
        for t in spikes_ms {
            echoes.push((*t, 5_000_000.0));
        }
        echoes.sort_by(|a, b| a.0.total_cmp(&b.0));
        Series {
            sent: echoes.len(),
            echoes,
        }
    }

    #[test]
    fn a_flush_every_second_reads_as_gaps_and_pairs_at_one_second() {
        let flushes: Vec<f64> = (1..=14).map(|s| f64::from(s) * 1_003.0).collect();
        let s = series(&flushes);
        assert_eq!(s.spikes().len(), 14);
        assert_eq!(s.gaps_at_one_hertz(), (13, 13));
        assert_eq!(s.pairs_at(HZ_GAP_MS), 13);
        assert_eq!(s.pairs_at(OFF_LAG_MS), 0);
    }

    #[test]
    fn interleaved_spikes_break_gaps_but_not_pairs() {
        let mut spikes: Vec<f64> = (1..=10).map(|s| f64::from(s) * 1_000.0).collect();
        spikes.extend((1..=10).map(|s| f64::from(s) * 1_000.0 + 333.0));
        let s = series(&spikes);
        assert_eq!(s.gaps_at_one_hertz().0, 0);
        assert_eq!(s.pairs_at(HZ_GAP_MS), 18);
    }
}
