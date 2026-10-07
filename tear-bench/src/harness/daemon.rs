use std::fs::{self, File};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tear_client::{Client, Transport};
use tear_types::MultiplexerControl;

use super::isolation::{self, FORBID_ENV};
use super::{Harness, procs};
use crate::matrix::{Band, TransportKind, Variant, transport_row};
use crate::receipt::{Sample, Unit};
use crate::seam;

pub struct Daemon {
    pub child: Child,
    pub pid: i32,
    pub transport: Transport,
    pub variant: Variant,
    pub label: String,
    pub ready_after: Duration,
    pub iso: PathBuf,
    pub socket_dir: PathBuf,
    pub audit_log: Option<PathBuf>,
    pub daemon_dump_rows: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub audit_log: bool,
    pub faults: Option<String>,
}

fn free_port() -> io::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

pub fn band_exec(args: &[String]) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let band = args
        .first()
        .and_then(|b| Band::parse(b))
        .ok_or_else(|| io::Error::other("band-exec <band> -- <program> [args]"))?;
    let rest = args
        .iter()
        .position(|a| a == "--")
        .map(|i| &args[i + 1..])
        .ok_or_else(|| io::Error::other("band-exec needs -- before the program"))?;
    let (program, program_args) = rest
        .split_first()
        .ok_or_else(|| io::Error::other("band-exec needs a program"))?;
    seam::apply_process_band(band)?;
    Err(Command::new(program).args(program_args).exec())
}

impl Daemon {
    fn listen_on(variant: Variant, socket_dir: &Path) -> io::Result<(Transport, Vec<String>)> {
        let out = match variant.transport {
            TransportKind::Tcp => {
                let addr: SocketAddr = format!("127.0.0.1:{}", free_port()?)
                    .parse()
                    .map_err(|e| io::Error::other(format!("{e}")))?;
                (
                    Transport::Tcp(addr),
                    vec!["daemon".into(), "--tcp".into(), addr.to_string()],
                )
            }
            TransportKind::Unix => {
                let sock = socket_dir.join("tear.sock");
                (
                    Transport::Unix(sock.clone()),
                    vec![
                        "daemon".into(),
                        "--socket".into(),
                        sock.to_string_lossy().into_owned(),
                    ],
                )
            }
        };
        debug_assert_eq!(transport_row(&out.0).kind, variant.transport);
        Ok(out)
    }

    fn command(
        h: &Harness,
        variant: Variant,
        iso: &Path,
        args: &[String],
        log: File,
        faults: &str,
    ) -> io::Result<Command> {
        let env = isolation::env_for(iso, &h.settings.path_env)?;
        let mut cmd = match variant.daemon_band {
            Band::Background => {
                let mut c = Command::new(std::env::current_exe()?);
                c.arg("band-exec")
                    .arg(Band::Background.name())
                    .arg("--")
                    .arg(&h.settings.tear_bin);
                c
            }
            Band::Interactive | Band::Default => Command::new(&h.settings.tear_bin),
        };
        cmd.args(args)
            .current_dir(&h.settings.root)
            .env_clear()
            .envs(env)
            .env(tear_types::probe_env::DUMP, iso.join("probes"))
            .env(tear_types::probe_env::FAULTS, faults)
            .env(FORBID_ENV, &h.settings.forbid)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        Ok(cmd)
    }

    fn wait_ready(
        child: &mut Child,
        pid: i32,
        transport: &Transport,
        label: &str,
    ) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match Client::connect_transport(transport.clone()) {
                Ok(c) => {
                    let version = c.capabilities().version().map(str::to_string);
                    drop(c);
                    if version.as_deref() == Some(env!("CARGO_PKG_VERSION")) {
                        return Ok(());
                    }
                    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
                    let _ = child.wait();
                    return Err(io::Error::other(format!(
                        "the daemon answered as {version:?}, not this workspace's {}",
                        env!("CARGO_PKG_VERSION")
                    )));
                }
                Err(e) => {
                    if let Some(st) = child.try_wait()? {
                        return Err(io::Error::other(format!(
                            "daemon {label} exited {st} before answering: {e}"
                        )));
                    }
                    if Instant::now() > deadline {
                        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
                        let _ = child.wait();
                        return Err(io::Error::other(format!(
                            "daemon {label} not ready in 20 s: {e}"
                        )));
                    }
                    std::thread::sleep(Duration::from_micros(500));
                }
            }
        }
    }

    pub fn start(h: &Harness, variant: Variant, tag: &str) -> io::Result<Daemon> {
        Self::start_with(h, variant, tag, &Options::default())
    }

    pub fn start_with(
        h: &Harness,
        variant: Variant,
        tag: &str,
        opts: &Options,
    ) -> io::Result<Daemon> {
        let label = variant.label();
        let iso = h.settings.root.join("iso").join(&label);
        fs::create_dir_all(iso.join("probes"))?;
        fs::create_dir_all(iso.join("config").join("tear"))?;
        let audit_log = opts.audit_log.then(|| iso.join(format!("audit-{tag}.log")));
        let mut yaml = variant.config_yaml();
        if let Some(p) = &audit_log {
            yaml.push_str("audit_log: ");
            yaml.push_str(&p.to_string_lossy());
            yaml.push('\n');
        }
        fs::write(iso.join("config").join("tear").join("tear.yaml"), yaml)?;
        let socket_dir = PathBuf::from("run").join(&label);
        fs::create_dir_all(h.settings.root.join(&socket_dir))?;
        let (transport, args) = Self::listen_on(variant, &socket_dir)?;
        let log = File::create(
            h.settings
                .root
                .join("logs")
                .join(format!("daemon-{label}-{tag}.log")),
        )?;
        let faults = opts.faults.as_deref().unwrap_or(&h.settings.faults);
        let mut cmd = Self::command(h, variant, &iso, &args, log, faults)?;
        let t0 = Instant::now();
        let mut child = cmd.spawn()?;
        let pid = i32::try_from(child.id()).unwrap_or(-1);
        h.record_pid(pid, &format!("daemon {label} {tag}"));
        Self::wait_ready(&mut child, pid, &transport, &label)?;
        let ready_after = t0.elapsed();
        h.log(&format!(
            "daemon {label} pid {pid} ready after {:.3} ms",
            ready_after.as_secs_f64() * 1e3
        ));
        Ok(Daemon {
            child,
            pid,
            transport,
            variant,
            label,
            ready_after,
            iso,
            socket_dir,
            audit_log,
            daemon_dump_rows: 0,
        })
    }

    pub fn client(&self) -> io::Result<Client> {
        Client::connect_transport(self.transport.clone())
    }

    #[must_use]
    pub fn holders(&self, h: &Harness) -> Vec<procs::Proc> {
        h.adopt_descendants(self.pid);
        let all = procs::ps();
        let mut out = procs::holders_under(&all, &self.iso);
        for p in procs::descendants(&all, &[self.pid]) {
            if procs::holder_pane_dir(&p.cmd).is_some() && !out.iter().any(|q| q.pid == p.pid) {
                out.push(p);
            }
        }
        out
    }

    #[must_use]
    pub fn journal_bytes(&self) -> u64 {
        fn walk(p: &Path, acc: &mut u64) {
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    let path = e.path();
                    if path.is_dir() {
                        walk(&path, acc);
                    } else if e.file_name().to_string_lossy().starts_with("seg-") {
                        *acc += e.metadata().map_or(0, |m| m.len());
                    }
                }
            }
        }
        let mut acc = 0;
        walk(&self.iso.join("state"), &mut acc);
        acc
    }

    pub fn audit_isolation(&self, h: &Harness) -> io::Result<usize> {
        let mut pids = vec![self.pid];
        pids.extend(self.holders(h).iter().map(|p| p.pid));
        let root = fs::canonicalize(&h.settings.root)?;
        let forbid = isolation::resolved(&h.settings.forbid);
        let mut found = 0;
        for pid in pids {
            let listing = isolation::open_files(pid)?;
            fs::write(
                h.settings
                    .root
                    .join("data")
                    .join(format!("open-files-{}-{pid}.txt", self.label)),
                &listing,
            )?;
            for v in isolation::violations(&listing, &root, &forbid) {
                h.log(&format!("ISOLATION VIOLATION pid {pid}: {v}"));
                found += 1;
            }
        }
        h.receipt.fact(
            &format!("isolation-violations:{}", self.label),
            &found.to_string(),
        )?;
        Ok(found)
    }

    pub fn require_isolated(&self, h: &Harness) -> io::Result<()> {
        match self.audit_isolation(h) {
            Ok(0) => {
                self.record_bands(h);
                Ok(())
            }
            Ok(n) => Err(io::Error::other(format!(
                "refusing to measure: {n} files open outside the run root"
            ))),
            Err(e) => Err(io::Error::other(format!(
                "refusing to measure: the isolation audit could not run: {e}"
            ))),
        }
    }

    pub fn record_bands(&self, h: &Harness) {
        let mut pids = vec![("daemon", self.pid)];
        pids.extend(self.holders(h).iter().map(|p| ("holder", p.pid)));
        for (role, pid) in pids {
            let reading = seam::task(pid).map_or_else(
                || "unreadable".to_string(),
                |t| format!("priority={} threads={}", t.priority, t.threads),
            );
            let _ = h
                .receipt
                .fact(&format!("band:{}:{role}:{pid}", self.label), &reading);
        }
    }

    pub fn band_matches(&self, h: &Harness) -> Result<(), String> {
        let want_bg = matches!(self.variant.daemon_band, Band::Background);
        let mut pids = vec![self.pid];
        pids.extend(self.holders(h).iter().map(|p| p.pid));
        for pid in pids {
            match seam::in_background(pid) {
                None => return Err(format!("pid {pid}: band unreadable")),
                Some(bg) if bg != want_bg => {
                    return Err(format!(
                        "pid {pid} runs {} the background band, the variant declares {}",
                        if bg { "in" } else { "outside" },
                        self.variant.daemon_band.name()
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    pub fn stop_daemon_only(&mut self, h: &Harness) {
        h.adopt_descendants(self.pid);
        let _ = kill(Pid::from_raw(self.pid), Signal::SIGINT);
        let until = Instant::now() + Duration::from_secs(6);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    h.log(&format!(
                        "daemon {} did not exit on SIGINT; SIGKILL",
                        self.pid
                    ));
                    let _ = kill(Pid::from_raw(self.pid), Signal::SIGKILL);
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        self.collect_probes(h);
        h.log(&format!(
            "daemon {} stopped; its holders keep running",
            self.pid
        ));
    }

    pub fn stop(&mut self, h: &Harness) {
        let all = procs::ps();
        let holders: Vec<i32> = procs::holders_under(&all, &self.iso)
            .iter()
            .map(|p| p.pid)
            .collect();
        let mut scope: Vec<i32> = procs::descendants(&all, &[self.pid])
            .iter()
            .chain(procs::descendants(&all, &holders).iter())
            .map(|p| p.pid)
            .collect();
        scope.extend(holders);
        self.stop_daemon_only(h);
        let mine: Vec<procs::Started> = h
            .started()
            .into_iter()
            .filter(|s| scope.contains(&s.pid) && s.alive())
            .collect();
        procs::terminate(&mine, &|m| h.log(m));
        self.collect_probes(h);
    }

    pub fn collect_probes(&mut self, h: &Harness) {
        for (role, pid, line) in super::read_probe_dumps(&self.iso.join("probes")) {
            if role == "daemon" {
                self.daemon_dump_rows += 1;
            }
            let mut cols = line.split('\t');
            let (Some(kind), Some(name), Some(value)) = (cols.next(), cols.next(), cols.next())
            else {
                continue;
            };
            let value: u64 = value.parse().unwrap_or(0);
            let _ = h
                .receipt
                .probe(&format!("{}:{role}", self.label), &pid, kind, name, value);
        }
        let _ = fs::remove_dir_all(self.iso.join("probes"));
        let _ = fs::create_dir_all(self.iso.join("probes"));
    }

    #[must_use]
    pub fn ready_sample(&self, bench: &str, i: usize) -> Sample {
        Sample::new(
            bench,
            &self.label,
            "daemon-ready",
            i,
            self.ready_after.as_nanos() as f64,
            Unit::Ns,
        )
    }
}
