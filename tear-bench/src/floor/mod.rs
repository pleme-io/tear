pub mod band;
pub mod dur;
pub mod parse;
pub mod pty;
pub mod ser;
pub mod spawn;
pub mod tcp;
pub mod timer;
pub mod uds;
pub mod wake;

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::time::{ClockId, clock_gettime};

use crate::matrix::{Band, Floor};
use crate::receipt::{Receipt, Sample, Unit};
use crate::verdict::FloorSet;

#[cfg(target_os = "macos")]
const MONOTONIC: ClockId = ClockId::from_raw(libc::CLOCK_UPTIME_RAW);
#[cfg(not(target_os = "macos"))]
const MONOTONIC: ClockId = ClockId::CLOCK_MONOTONIC;

#[must_use]
pub fn now_ns() -> u64 {
    clock_gettime(MONOTONIC).map_or(0, |t| {
        u64::try_from(t.tv_sec()).unwrap_or(0) * 1_000_000_000
            + u64::try_from(t.tv_nsec()).unwrap_or(0)
    })
}

pub struct Lcg(pub u64);

impl Lcg {
    pub fn draw(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    pub fn pace(&mut self, gap_us: u64, jitter_us: u64) -> Duration {
        let j = if jitter_us > 0 {
            self.draw() % jitter_us
        } else {
            0
        };
        Duration::from_micros(gap_us + j)
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Plan {
    pub scale: u64,
    pub band: Band,
}

impl Plan {
    #[must_use]
    pub fn n(&self, full: u64) -> usize {
        usize::try_from((full / self.scale.max(1)).max(20)).unwrap_or(usize::MAX)
    }
}

pub struct Measured {
    pub floor: Floor,
    pub variant: String,
    pub ns: Vec<f64>,
}

impl Measured {
    #[must_use]
    pub fn new(floor: Floor, variant: impl Into<String>, ns: Vec<f64>) -> Self {
        Self {
            floor,
            variant: variant.into(),
            ns,
        }
    }

    pub fn record(&self, receipt: &Receipt) -> io::Result<()> {
        for (i, v) in self.ns.iter().enumerate() {
            receipt.sample(&Sample::new(
                &format!("floor:{}", self.floor.name()),
                &self.variant,
                self.floor.name(),
                i,
                *v,
                Unit::Ns,
            ))?;
        }
        Ok(())
    }
}

pub fn peer_command(args: &[&str]) -> io::Result<Command> {
    let mut c = Command::new(std::env::current_exe()?);
    c.arg("floor-peer").args(args).stdin(Stdio::null());
    Ok(c)
}

pub fn wait_for_path(path: &Path, child: &mut Child, deadline: Duration) -> io::Result<()> {
    let until = Instant::now() + deadline;
    while !path.exists() {
        if let Some(st) = child.try_wait()? {
            return Err(io::Error::other(format!("floor peer exited early: {st}")));
        }
        if Instant::now() > until {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("floor peer never created {}", path.display()),
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

pub fn scratch_path(stem: &str) -> PathBuf {
    PathBuf::from(format!("{stem}-{}", std::process::id()))
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Suite {
    Sentinels,
    Standard,
}

pub fn run_suite(suite: Suite, plan: Plan, receipt: &Receipt) -> io::Result<FloorSet> {
    let mut set = FloorSet::default();
    let mut keep = |m: io::Result<Measured>, label: &str| -> io::Result<()> {
        match m {
            Ok(m) => {
                m.record(receipt)?;
                set.add(m.floor, m.ns.iter().copied());
                Ok(())
            }
            Err(e) => receipt.fact(&format!("floor-error:{label}"), &e.to_string()),
        }
    };
    keep(uds::round_trip(true, 64, plan, 0), "uds-framed")?;
    keep(wake::hot(plan), "wake-hot")?;
    keep(uds::one_way(plan), "uds-one-way")?;
    keep(wake::run_loop(plan, false), "wake-run-loop")?;
    if suite == Suite::Sentinels {
        return Ok(set);
    }
    keep(uds::round_trip(false, 64, plan, 0), "uds-raw")?;
    keep(uds::send(plan), "uds-send")?;
    keep(wake::idle(plan), "wake-idle")?;
    keep(pty::echo(plan), "pty-echo")?;
    keep(pty::output_ceiling(plan), "pty-output")?;
    keep(pty::input_ceiling(plan), "pty-input")?;
    keep(uds::throughput(plan), "uds-throughput")?;
    keep(tcp::round_trip(plan), "tcp")?;
    for mode in dur::Mode::ALL {
        if let Some(floor) = mode.floor() {
            keep(dur::flush(*mode, plan), floor.name())?;
        }
    }
    keep(ser::byte_strings(plan), "ser")?;
    keep(timer::sleep_1ms(plan), "timer")?;
    keep(spawn::spawn_openpty(plan), "spawn-openpty")?;
    keep(parse::screen(plan), "screen-parse")?;
    Ok(set)
}

pub fn peer(args: &[String]) -> io::Result<()> {
    let kind = args.first().map(String::as_str).unwrap_or_default();
    let rest = &args[args.len().min(1)..];
    match kind {
        "uds-echo" => uds::echo_peer(rest),
        "uds-reader" => uds::reader_peer(rest),
        "uds-sink" => uds::sink_peer(rest),
        "pty-writer" => pty::writer_peer(rest),
        "pty-drain" => pty::drain_peer(rest),
        "tcp-echo" => tcp::echo_peer(rest),
        "producer" => crate::harness::cases::producer_peer(rest),
        "idle" => band::idle_peer(rest),
        "exit" => Ok(()),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown floor peer {other:?}"),
        )),
    }
}

pub fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

pub fn flag_u64(args: &[String], name: &str, default: u64) -> io::Result<u64> {
    flag(args, name).map_or(Ok(default), |v| {
        v.parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} {v}: {e}")))
    })
}

pub fn flag_band(args: &[String]) -> io::Result<Band> {
    flag(args, "--band").map_or(Ok(Band::Default), |v| {
        Band::parse(v)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("--band {v}")))
    })
}
