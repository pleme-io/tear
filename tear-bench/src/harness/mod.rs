pub mod cases;
pub mod daemon;
pub mod files;
pub mod isolation;
pub mod procs;
pub mod reproduce;
pub mod rig;
pub mod window;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::receipt::{Receipt, Sample};

pub const COLS: u16 = 163;
pub const ROWS: u16 = 48;
pub const STARTED: &str = "started.pids";
pub const ECHO_SCRIPT: &str = "stty raw -echo; printf READY; exec cat";
pub const PASTE_SCRIPT: &str = "stty raw -echo; printf READY; head -c \"$0\" | cksum; exec cat";
pub const FLOOD_SCRIPT: &str =
    "stty raw -echo; printf READY; dd bs=1 count=1 of=/dev/null 2>/dev/null; cat \"$0\"; exec cat";

pub struct Settings {
    pub root: PathBuf,
    pub tear_bin: PathBuf,
    pub prev_tear_bin: Option<PathBuf>,
    pub oldest_tear_bin: Option<PathBuf>,
    pub mado_bin: Option<PathBuf>,
    pub parked_window_bin: Option<PathBuf>,
    pub forbid: PathBuf,
    pub path_env: String,
    pub faults: String,
    pub size: (u16, u16),
}

pub struct Harness {
    pub settings: Settings,
    pub receipt: Receipt,
    started: Mutex<Vec<procs::Started>>,
    collected: Mutex<Vec<Sample>>,
}

impl Harness {
    pub fn open(settings: Settings, run: Option<String>) -> io::Result<Self> {
        for d in ["data", "files", "logs", "run", "iso"] {
            fs::create_dir_all(settings.root.join(d))?;
        }
        let receipt = Receipt::open(&settings.root.join("data"), run)?;
        let h = Self {
            settings,
            receipt,
            started: Mutex::new(Vec::new()),
            collected: Mutex::new(Vec::new()),
        };
        h.record_facts()?;
        Ok(h)
    }

    fn record_facts(&self) -> io::Result<()> {
        let r = &self.receipt;
        r.fact("tearbench-version", env!("CARGO_PKG_VERSION"))?;
        r.fact("tear-bin", &self.settings.tear_bin.to_string_lossy())?;
        r.fact(
            "tear-bin-version",
            &self.tear_version().unwrap_or_else(|e| e.to_string()),
        )?;
        r.fact("os", std::env::consts::OS)?;
        r.fact("arch", std::env::consts::ARCH)?;
        r.fact(
            "probes",
            if cfg!(feature = "bench-probes") {
                "compiled"
            } else {
                "absent"
            },
        )?;
        r.fact("faults", &self.settings.faults)?;
        r.fact("started-unix", &now_unix().to_string())?;
        for (key, cmd, args) in [
            ("host-model", "sysctl", &["-n", "hw.model"][..]),
            ("loadavg", "sysctl", &["-n", "vm.loadavg"][..]),
            ("os-build", "sw_vers", &["-buildVersion"][..]),
            ("uname", "uname", &["-a"][..]),
        ] {
            if let Ok(o) = Command::new(cmd).args(args).output()
                && o.status.success()
            {
                r.fact(key, String::from_utf8_lossy(&o.stdout).trim())?;
            }
        }
        Ok(())
    }

    pub fn tear_version(&self) -> io::Result<String> {
        let o = Command::new(&self.settings.tear_bin)
            .arg("--version")
            .output()?;
        if !o.status.success() {
            return Err(io::Error::other(format!(
                "{} --version exited {}",
                self.settings.tear_bin.display(),
                o.status
            )));
        }
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    pub fn require_workspace_build(&self) -> io::Result<()> {
        let v = self.tear_version()?;
        let want = env!("CARGO_PKG_VERSION");
        if v.split_whitespace().any(|w| w == want) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "refusing to run: {} reports {v:?}, not this workspace's {want}; pass --tear-bin <the workspace build>",
                self.settings.tear_bin.display()
            )))
        }
    }

    pub fn log(&self, msg: &str) {
        let line = format!("[tearbench {:.3}] {msg}", now_secs());
        eprintln!("{line}");
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.settings.root.join("logs").join("harness.log"))
        {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn record_pid(&self, pid: i32, what: &str) {
        let Some(s) = procs::Started::of(pid) else {
            self.log(&format!(
                "pid {pid} ({what}) is gone before its start time could be read; it is not recorded"
            ));
            return;
        };
        if let Ok(mut v) = self.started.lock() {
            if v.contains(&s) {
                return;
            }
            v.push(s);
        }
        let _ = self.receipt.fact("started-pid", &format!("{pid} {what}"));
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.settings.root.join("run").join(STARTED))
        {
            let _ = writeln!(f, "{}", s.row(what));
        }
    }

    pub fn adopt_descendants(&self, pid: i32) {
        let all = procs::ps();
        for p in procs::descendants(&all, &[pid]) {
            self.record_pid(p.pid, &format!("descendant of {pid}: {}", p.cmd));
        }
    }

    #[must_use]
    pub fn started(&self) -> Vec<procs::Started> {
        let mut out: Vec<procs::Started> =
            self.started.lock().map(|v| v.clone()).unwrap_or_default();
        if let Ok(body) = fs::read_to_string(self.settings.root.join("run").join(STARTED)) {
            out.extend(body.lines().filter_map(procs::Started::parse_row));
        }
        out.sort_by_key(|s| (s.pid, s.start));
        out.dedup();
        out
    }

    pub fn emit(&self, s: Sample) {
        if let Err(e) = self.receipt.sample(&s) {
            self.log(&format!("receipt write failed: {e}"));
        }
        if let Ok(mut v) = self.collected.lock() {
            v.push(s);
        }
    }

    pub fn emit_all(&self, all: Vec<Sample>) {
        for s in all {
            self.emit(s);
        }
    }

    #[must_use]
    pub fn collected(&self) -> Vec<Sample> {
        self.collected.lock().map(|v| v.clone()).unwrap_or_default()
    }

    #[must_use]
    pub fn files(&self) -> PathBuf {
        self.settings.root.join("files")
    }

    #[must_use]
    pub fn iso(&self) -> PathBuf {
        self.settings.root.join("iso")
    }

    pub fn cleanup(&self) {
        for s in self.started() {
            if s.alive() {
                self.adopt_descendants(s.pid);
            }
        }
        let mine: Vec<procs::Started> = self.started().into_iter().filter(|s| s.alive()).collect();
        procs::terminate(&mine, &|m| self.log(m));
        let ours: Vec<procs::Started> = self.started();
        for p in procs::holders_under(&procs::ps(), &self.iso()) {
            if !ours.iter().any(|s| s.pid == p.pid && s.alive()) {
                self.log(&format!(
                    "left running, not started by this harness as far as it recorded: pid {} ({})",
                    p.pid, p.cmd
                ));
            }
        }
    }
}

#[cfg(feature = "bench-probes")]
pub fn arm_client(faults: &[crate::matrix::Control]) {
    let armed: Vec<tear_types::probes::Fault> = faults
        .iter()
        .filter_map(|c| tear_types::probes::Fault::parse(c.name()))
        .collect();
    tear_types::probes::arm(&armed);
}

#[cfg(not(feature = "bench-probes"))]
pub fn arm_client(_: &[crate::matrix::Control]) {}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tag {
    Cells,
    Control(crate::matrix::Control),
}

impl Tag {
    pub fn emit(self, h: &Harness, s: Sample) {
        h.emit(match self {
            Tag::Cells => s,
            Tag::Control(c) => Sample {
                bench: format!("control:{}:{}", c.name(), s.bench),
                cell: None,
                ..s
            },
        });
    }
}

#[must_use]
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

#[must_use]
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn read_probe_dumps(dir: &Path) -> Vec<(String, String, String)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".tsv") else {
            continue;
        };
        let (role, pid) = stem.rsplit_once('-').unwrap_or((stem, ""));
        if let Ok(body) = fs::read_to_string(e.path()) {
            for line in body.lines().skip(1) {
                out.push((role.to_string(), pid.to_string(), line.to_string()));
            }
        }
    }
    out
}

pub fn isolated_rig(
    h: &Harness,
    variant: crate::matrix::Variant,
    tag: &str,
) -> io::Result<(daemon::Daemon, rig::Rig)> {
    isolated_rig_with(h, variant, tag, &daemon::Options::default())
}

pub fn isolated_rig_with(
    h: &Harness,
    variant: crate::matrix::Variant,
    tag: &str,
    opts: &daemon::Options,
) -> io::Result<(daemon::Daemon, rig::Rig)> {
    let mut d = daemon::Daemon::start_with(h, variant, tag, opts)?;
    let rig = rig::Rig::daemon(d.client()?, d.transport.clone(), variant, &d.label, d.pid)
        .with_auth(d.auth_token.clone());
    let probe = rig.echo_pane("isolation-probe");
    std::thread::sleep(std::time::Duration::from_millis(800));
    let isolated = d.require_isolated(h);
    if let Ok((sid, _, sub)) = probe {
        drop(sub);
        rig.kill(sid);
    }
    match isolated {
        Ok(()) => Ok((d, rig)),
        Err(e) => {
            drop(rig);
            d.stop(h);
            Err(e)
        }
    }
}
