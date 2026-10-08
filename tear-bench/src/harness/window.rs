use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tear_client::Transport;
use tear_types::PaneId;

use super::daemon::Daemon;
use super::rig::Rig;
use super::{Harness, isolation, procs};
use crate::matrix::Control;

pub const CONFIG_ENV: &str = "MADO_CONFIG";
pub const FAULTS_ENV: &str = tear_types::probe_env::FAULTS;
pub const FATE_BACKSTOP_SECS: u64 = 3_600;

tear_types::closed_vocabulary! {
    PaneFate { Edge => "edge", Poll => "poll" }
}

tear_types::closed_vocabulary! {
    Pacing { Demand => "demand", Capped => "capped" }
}

pub const QUIET_SPAN: Duration = Duration::from_secs(60);
pub const WAKEUP_SPAN: Duration = Duration::from_secs(3);
pub const PARKED_SETTLE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub struct WindowOptions {
    pub pane_fate: PaneFate,
    pub pacing: Pacing,
    pub quiet: Option<Duration>,
    pub faults: &'static [Control],
    pub settle: Duration,
    pub idle: Option<Duration>,
    pub echoes: usize,
    pub echo_gap: Duration,
}

impl WindowOptions {
    pub const ECHOES: usize = 24;

    #[must_use]
    pub fn clean() -> Self {
        Self {
            pane_fate: PaneFate::Edge,
            pacing: Pacing::Demand,
            quiet: None,
            faults: &[],
            settle: Duration::from_secs(3),
            idle: Some(Duration::from_secs(10)),
            echoes: Self::ECHOES,
            echo_gap: Duration::from_millis(500),
        }
    }

    #[must_use]
    pub fn idle_only(pane_fate: PaneFate) -> Self {
        Self {
            pane_fate,
            echoes: 0,
            ..Self::clean()
        }
    }

    #[must_use]
    pub fn echoes_only(faults: &'static [Control]) -> Self {
        Self {
            faults,
            idle: None,
            ..Self::clean()
        }
    }

    #[must_use]
    pub fn quiet(pacing: Pacing) -> Self {
        Self {
            pacing,
            quiet: Some(QUIET_SPAN),
            idle: None,
            echoes: 0,
            ..Self::clean()
        }
    }

    #[must_use]
    pub fn faults_env(&self) -> String {
        self.faults
            .iter()
            .map(|c| c.name())
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Idle {
    pub get_pane: u64,
    pub secs: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Quiet {
    pub ticks: u64,
    pub secs: f64,
    pub wakeups: Vec<f64>,
    pub hides: u64,
    pub pacing: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowRun {
    pub pid: i32,
    pub idle: Option<Idle>,
    pub quiet: Option<Quiet>,
    pub hides: u64,
    pub presents_ns: Vec<f64>,
    pub echoes: usize,
    pub armed: Vec<String>,
}

impl WindowRun {
    #[must_use]
    pub fn unarmed<'a>(&self, wanted: &'a [Control]) -> Vec<&'a str> {
        wanted
            .iter()
            .map(|c| c.name())
            .filter(|name| !self.armed.iter().any(|a| a == name))
            .collect()
    }
}

#[must_use]
pub fn config_yaml(socket: &Path, pane_fate: PaneFate, pacing: Pacing) -> String {
    format!(
        "tear:\n  mode: attach\n  runtime: resident\n  socket: {socket}\n  auto_spawn: false\n  session_switching: true\n  auto_attach: off\n  pane_fate: {fate}\n  fate_backstop_secs: {FATE_BACKSTOP_SECS}\nshell:\n  command: /bin/sh\ncursor:\n  blink: false\nsuggestions:\n  enabled: false\nperformance:\n  histograms: on\n  pacing: {pacing}\nwindow:\n  width: 420\n  height: 260\n",
        socket = socket.display(),
        fate = pane_fate.name(),
        pacing = pacing.name()
    )
}

pub fn loop_ticks(frame_perf: &serde_json::Value) -> io::Result<u64> {
    frame_perf["loop"]["ticks"]
        .as_u64()
        .ok_or_else(|| io::Error::other("frame_perf carries no loop.ticks: a mado before R11"))
}

#[must_use]
pub fn window_hides(frame_perf: &serde_json::Value) -> u64 {
    frame_perf["window"]["hides"].as_u64().unwrap_or(0)
}

pub fn wakeup_rates(pid: i32, span: Duration, every: Duration) -> io::Result<Vec<f64>> {
    let read = || {
        crate::seam::task(pid)
            .map(|t| t.context_switches)
            .ok_or_else(|| io::Error::other(format!("pid {pid} has no task to read")))
    };
    let mut out = Vec::new();
    let mut before = read()?;
    let mut at = Instant::now();
    let end = at + span;
    while at < end {
        std::thread::sleep(every.min(end - at));
        let now = Instant::now();
        let switches = read()?;
        let secs = now.duration_since(at).as_secs_f64();
        out.push(switches.saturating_sub(before) as f64 / secs);
        before = switches;
        at = now;
    }
    Ok(out)
}

pub fn frame_perf(socket: &Path) -> io::Result<serde_json::Value> {
    let mut s = UnixStream::connect(socket)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let req = serde_json::to_vec(&serde_json::json!({"path": ["frame_perf"], "args": []}))?;
    let len = u32::try_from(req.len()).map_err(io::Error::other)?;
    s.write_all(&len.to_be_bytes())?;
    s.write_all(&req)?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head)?;
    let mut body = vec![0u8; u32::from_be_bytes(head) as usize];
    s.read_exact(&mut body)?;
    let answer: serde_json::Value = serde_json::from_slice(&body)?;
    answer
        .get("Ok")
        .cloned()
        .ok_or_else(|| io::Error::other(format!("frame_perf refused: {answer}")))
}

pub fn ui_get_pane(frame_perf: &serde_json::Value) -> io::Result<u64> {
    frame_perf["ui_thread_tear_calls"]["get_pane"]
        .as_u64()
        .ok_or_else(|| io::Error::other("frame_perf carries no ui_thread_tear_calls.get_pane"))
}

pub fn byte_to_present_buckets(frame_perf: &serde_json::Value) -> io::Result<BTreeMap<u64, u64>> {
    let buckets = frame_perf["latency_us"]["byte_to_present"]["buckets"]
        .as_array()
        .ok_or_else(|| io::Error::other("frame_perf carries no latency_us.byte_to_present"))?;
    buckets
        .iter()
        .map(|b| match (b[0].as_u64(), b[1].as_u64()) {
            (Some(high), Some(n)) => Ok((high, n)),
            _ => Err(io::Error::other(format!(
                "a byte_to_present bucket is not [high, n]: {b}"
            ))),
        })
        .collect()
}

#[must_use]
pub fn presents_between(before: &BTreeMap<u64, u64>, after: &BTreeMap<u64, u64>) -> Vec<f64> {
    let mut out = Vec::new();
    for (high, n) in after {
        let fresh = n.saturating_sub(before.get(high).copied().unwrap_or(0));
        out.extend(std::iter::repeat_n(*high as f64 * 1_000.0, fresh as usize));
    }
    out
}

#[must_use]
pub fn armed_faults(frame_perf: &serde_json::Value) -> Vec<String> {
    frame_perf["bench_faults"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn first_answer(child: &mut Child, socket: &Path, log: &Path) -> io::Result<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(v) = frame_perf(socket) {
            return Ok(v);
        }
        if let Some(st) = child.try_wait()? {
            let tail = fs::read_to_string(log).unwrap_or_default();
            let tail: String = tail.lines().rev().take(5).collect::<Vec<_>>().join(" | ");
            return Err(io::Error::other(format!(
                "mado exited {st} before its kanshou socket answered: {tail}"
            )));
        }
        if Instant::now() > deadline {
            return Err(io::Error::other(
                "mado's kanshou socket did not answer in 30 s",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn the_window_s_pane(rig: &Rig) -> io::Result<PaneId> {
    let sessions = rig.ctl().list_sessions().map_err(io::Error::other)?;
    let panes: Vec<PaneId> = sessions
        .iter()
        .flat_map(|s| s.panes.keys().copied())
        .collect();
    match panes.as_slice() {
        [pane] => Ok(*pane),
        other => Err(io::Error::other(format!(
            "the window's daemon holds {} panes, not the window's one",
            other.len()
        ))),
    }
}

fn measure(
    rig: &Rig,
    child: &mut Child,
    kanshou: &Path,
    log: &Path,
    pid: i32,
    opts: &WindowOptions,
) -> io::Result<WindowRun> {
    let first = first_answer(child, kanshou, log)?;
    let armed = armed_faults(&first);
    std::thread::sleep(opts.settle);
    let quiet = match opts.quiet {
        Some(span) => {
            let before = frame_perf(kanshou)?;
            let t0 = Instant::now();
            let wakeups = wakeup_rates(pid, span, WAKEUP_SPAN)?;
            let after = frame_perf(kanshou)?;
            Some(Quiet {
                ticks: loop_ticks(&after)?.saturating_sub(loop_ticks(&before)?),
                secs: t0.elapsed().as_secs_f64(),
                wakeups,
                hides: window_hides(&after).saturating_sub(window_hides(&before)),
                pacing: after["window"]["pacing"]
                    .as_str()
                    .unwrap_or("unreported")
                    .to_string(),
            })
        }
        None => None,
    };
    let idle = match opts.idle {
        Some(span) => {
            let before = ui_get_pane(&frame_perf(kanshou)?)?;
            let t0 = Instant::now();
            std::thread::sleep(span);
            let after = ui_get_pane(&frame_perf(kanshou)?)?;
            Some(Idle {
                get_pane: after.saturating_sub(before),
                secs: t0.elapsed().as_secs_f64(),
            })
        }
        None => None,
    };
    let mut presents_ns = Vec::new();
    if opts.echoes > 0 {
        let pane = the_window_s_pane(rig)?;
        let before = byte_to_present_buckets(&frame_perf(kanshou)?)?;
        for _ in 0..opts.echoes {
            std::thread::sleep(opts.echo_gap);
            rig.ctl().send_keys(pane, b"x").map_err(io::Error::other)?;
        }
        std::thread::sleep(opts.echo_gap);
        let after = byte_to_present_buckets(&frame_perf(kanshou)?)?;
        presents_ns = presents_between(&before, &after);
    }
    let hides = window_hides(&frame_perf(kanshou)?).saturating_sub(window_hides(&first));
    Ok(WindowRun {
        pid,
        idle,
        quiet,
        hides,
        presents_ns,
        echoes: opts.echoes,
        armed,
    })
}

pub fn run_window(
    h: &Harness,
    d: &Daemon,
    rig: &Rig,
    mado: &Path,
    tag: &str,
    opts: &WindowOptions,
) -> io::Result<WindowRun> {
    let Transport::Unix(socket) = &d.transport else {
        return Err(io::Error::other(
            "a resident window attaches over a unix socket",
        ));
    };
    let iso = h.iso().join(format!("mado-{tag}"));
    let env = isolation::env_for(&iso, &h.settings.path_env)?;
    let config_dir = iso.join("config").join("mado");
    fs::create_dir_all(&config_dir)?;
    let config = config_dir.join("mado.yaml");
    fs::write(&config, config_yaml(socket, opts.pane_fate, opts.pacing))?;
    let log_path: PathBuf = h.settings.root.join("logs").join(format!("mado-{tag}.log"));
    let log = File::create(&log_path)?;
    let mut child = Command::new(mado)
        .current_dir(&h.settings.root)
        .env_clear()
        .envs(env)
        .env(CONFIG_ENV, &config)
        .env(FAULTS_ENV, opts.faults_env())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    let pid = i32::try_from(child.id()).unwrap_or(-1);
    h.record_pid(pid, &format!("mado window {tag}"));
    let kanshou = iso.join("kanshou").join(format!("mado-{pid}.sock"));
    let measured = measure(rig, &mut child, &kanshou, &log_path, pid, opts);
    if let Some(started) = procs::Started::of(pid) {
        procs::terminate(&[started], &|m| h.log(m));
    }
    let _ = child.wait();
    measured
}

pub fn run_parked(h: &Harness, parked: &Path, tag: &str, span: Duration) -> io::Result<Vec<f64>> {
    let iso = h.iso().join(format!("parked-{tag}"));
    let env = isolation::env_for(&iso, &h.settings.path_env)?;
    let log_path: PathBuf = h
        .settings
        .root
        .join("logs")
        .join(format!("parked-{tag}.log"));
    let log = File::create(&log_path)?;
    let lifetime = (PARKED_SETTLE + span + Duration::from_secs(30)).as_secs();
    let mut child = Command::new(parked)
        .current_dir(&h.settings.root)
        .env_clear()
        .envs(env)
        .arg("--secs")
        .arg(lifetime.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    let pid = i32::try_from(child.id()).unwrap_or(-1);
    h.record_pid(pid, &format!("parked madori window {tag}"));
    std::thread::sleep(PARKED_SETTLE);
    let measured = match child.try_wait()? {
        Some(st) => Err(io::Error::other(format!(
            "the parked window exited {st} before it was measured"
        ))),
        None => wakeup_rates(pid, span, WAKEUP_SPAN),
    };
    if let Some(started) = procs::Started::of(pid) {
        procs::terminate(&[started], &|m| h.log(m));
    }
    let _ = child.wait();
    measured
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_config_names_the_daemon_socket_the_fate_policy_and_a_backstop_past_the_run() {
        let yaml = config_yaml(
            Path::new("run/bound/tear.sock"),
            PaneFate::Poll,
            Pacing::Capped,
        );
        assert!(yaml.contains("pacing: capped"));
        assert!(yaml.contains("socket: run/bound/tear.sock"));
        assert!(yaml.contains("runtime: resident"));
        assert!(yaml.contains("pane_fate: poll"));
        assert!(yaml.contains("auto_spawn: false"));
        assert!(yaml.contains("histograms: on"));
        assert!(yaml.contains("cursor:\n  blink: false\n"));
        assert!(yaml.contains("suggestions:\n  enabled: false\n"));
        assert!(yaml.contains(&format!("fate_backstop_secs: {FATE_BACKSTOP_SECS}")));
        let o = WindowOptions::clean();
        let longest = 30.0
            + o.settle.as_secs_f64()
            + o.idle.unwrap_or_default().as_secs_f64()
            + o.echo_gap.as_secs_f64() * (o.echoes + 1) as f64;
        assert!(
            (FATE_BACKSTOP_SECS as f64) > longest,
            "a backstop read inside the measured window would read as an idle get_pane"
        );
    }

    #[test]
    fn the_get_pane_reading_is_the_ui_thread_counter_and_a_missing_one_is_refused() {
        let v = serde_json::json!({"ui_thread_tear_calls": {"get_pane": 600}});
        assert_eq!(ui_get_pane(&v).unwrap(), 600);
        assert!(ui_get_pane(&serde_json::json!({})).is_err());
    }

    #[test]
    fn presents_are_the_histogram_entries_recorded_between_two_readings_in_ns() {
        let before = serde_json::json!({"latency_us": {"byte_to_present": {"buckets": [[527, 2], [9215, 1]]}}});
        let after = serde_json::json!({"latency_us": {"byte_to_present": {"buckets": [[527, 4], [559, 1], [9215, 1]]}}});
        let a = byte_to_present_buckets(&before).unwrap();
        let b = byte_to_present_buckets(&after).unwrap();
        assert_eq!(
            presents_between(&a, &b),
            vec![527_000.0, 527_000.0, 559_000.0]
        );
        assert!(byte_to_present_buckets(&serde_json::json!({})).is_err());
    }

    #[test]
    fn a_fault_the_window_did_not_report_armed_is_named() {
        let run = WindowRun {
            pid: 1,
            idle: None,
            quiet: None,
            hides: 0,
            presents_ns: Vec::new(),
            echoes: 0,
            armed: armed_faults(&serde_json::json!({"bench_faults": ["wake-off"]})),
        };
        assert!(run.unarmed(&[Control::WakeOff]).is_empty());
        let plain = WindowRun {
            armed: armed_faults(&serde_json::json!({})),
            ..run
        };
        assert_eq!(plain.unarmed(&[Control::WakeOff]), vec!["wake-off"]);
        assert_eq!(
            WindowOptions::echoes_only(&[Control::WakeOff]).faults_env(),
            "wake-off"
        );
    }
}
