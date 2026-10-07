use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::Instant;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::pty::openpty;
use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};

use super::{Measured, Plan, flag_u64, peer_command};
use crate::matrix::Floor;
use crate::seam;

pub struct Pty {
    pub master: File,
    pub slave: OwnedFd,
}

pub fn open_raw() -> io::Result<Pty> {
    let p = openpty(None, None).map_err(io::Error::from)?;
    for fd in [&p.master, &p.slave] {
        fcntl(fd.as_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(io::Error::from)?;
    }
    let mut t = tcgetattr(p.slave.as_fd()).map_err(io::Error::from)?;
    cfmakeraw(&mut t);
    tcsetattr(p.slave.as_fd(), SetArg::TCSANOW, &t).map_err(io::Error::from)?;
    Ok(Pty {
        master: File::from(p.master),
        slave: p.slave,
    })
}

pub fn slave_stdio(slave: &OwnedFd) -> io::Result<Stdio> {
    Ok(Stdio::from(slave.try_clone()?))
}

pub fn echo(plan: Plan) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let Pty { mut master, slave } = open_raw()?;
    let mut child = Command::new("cat")
        .stdin(slave_stdio(&slave)?)
        .stdout(slave_stdio(&slave)?)
        .stderr(Stdio::null())
        .spawn()?;
    drop(slave);
    let n = plan.n(20_000);
    let warm = n / 20;
    let mut ns = Vec::with_capacity(n);
    let mut r = [0u8; 1];
    for i in 0..(warm + n) {
        let t0 = Instant::now();
        master.write_all(b"x")?;
        master.read_exact(&mut r)?;
        if r[0] != b'x' {
            return Err(io::Error::other(format!("pty echo returned {:#x}", r[0])));
        }
        if i >= warm {
            ns.push(t0.elapsed().as_nanos() as f64);
        }
    }
    drop(master);
    let _ = child.wait();
    Ok(Measured::new(
        Floor::PtyEcho,
        format!("cat;raw;band={}", plan.band.name()),
        ns,
    ))
}

pub fn output_ceiling(plan: Plan) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let bytes: u64 = 64 << 20;
    let reps = usize::try_from((5 / plan.scale.max(1)).max(2)).unwrap_or(2);
    let mut ns = Vec::new();
    let mut buf = vec![0u8; 65_536];
    for _ in 0..reps {
        let Pty { mut master, slave } = open_raw()?;
        let mut child = peer_command(&["pty-writer", "--bytes", &bytes.to_string()])?
            .stdout(slave_stdio(&slave)?)
            .stderr(Stdio::inherit())
            .spawn()?;
        drop(slave);
        let mut total = 0u64;
        let mut first: Option<Instant> = None;
        let mut last = Instant::now();
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => {
                    let t = Instant::now();
                    first.get_or_insert(t);
                    last = t;
                    total += k as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) => return Err(e),
            }
        }
        let st = child.wait()?;
        if !st.success() || total < bytes {
            return Err(io::Error::other(format!(
                "pty writer: {st}, {total} of {bytes}"
            )));
        }
        let dt = last.saturating_duration_since(first.unwrap_or(last));
        ns.push(dt.as_nanos() as f64 / (total as f64 / f64::from(1 << 20)));
    }
    Ok(Measured::new(
        Floor::PtyOutputCeiling,
        format!("ns-per-mib;raw;band={}", plan.band.name()),
        ns,
    ))
}

pub fn writer_peer(args: &[String]) -> io::Result<()> {
    let bytes = flag_u64(args, "--bytes", 64 << 20)?;
    let chunk = 65_536usize;
    let mut line = Vec::with_capacity(80);
    for k in 0..79u8 {
        line.push(b' ' + 1 + (k % 94));
    }
    line.push(b'\n');
    let mut buf = Vec::with_capacity(chunk);
    while buf.len() < chunk {
        let take = (chunk - buf.len()).min(line.len());
        buf.extend_from_slice(&line[..take]);
    }
    let mut out = io::stdout().lock();
    let mut left = bytes;
    while left > 0 {
        let k = usize::try_from(left).unwrap_or(chunk).min(chunk);
        out.write_all(&buf[..k])?;
        left -= k as u64;
    }
    out.flush()
}

pub fn input_ceiling(plan: Plan) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let bytes: u64 = 1 << 20;
    let reps = plan.n(200).min(200);
    let Pty { mut master, slave } = open_raw()?;
    let mut child = peer_command(&[
        "pty-drain",
        "--bytes",
        &bytes.to_string(),
        "--reps",
        &reps.to_string(),
    ])?
    .stdin(slave_stdio(&slave)?)
    .stdout(slave_stdio(&slave)?)
    .stderr(Stdio::inherit())
    .spawn()?;
    drop(slave);
    let mut line = Vec::with_capacity(80);
    for k in 0..79u8 {
        line.push(b' ' + 1 + (k % 94));
    }
    line.push(b'\r');
    let chunk: Vec<u8> = line.iter().copied().cycle().take(65_536).collect();
    let mut ns = Vec::with_capacity(reps);
    let mut ack = [0u8; 1];
    for _ in 0..reps {
        let t0 = Instant::now();
        let mut left = bytes;
        while left > 0 {
            let k = usize::try_from(left)
                .unwrap_or(chunk.len())
                .min(chunk.len());
            master.write_all(&chunk[..k])?;
            left -= k as u64;
        }
        master.read_exact(&mut ack)?;
        ns.push(t0.elapsed().as_nanos() as f64 / (bytes as f64 / f64::from(1 << 20)));
    }
    drop(master);
    let _ = child.wait();
    Ok(Measured::new(
        Floor::PtyInputCeiling,
        format!("ns-per-mib;raw;{bytes}B;band={}", plan.band.name()),
        ns,
    ))
}

pub fn drain_peer(args: &[String]) -> io::Result<()> {
    let bytes = flag_u64(args, "--bytes", 1 << 20)?;
    let reps = flag_u64(args, "--reps", 1)?;
    let mut input = io::stdin().lock();
    let mut out = io::stdout().lock();
    let mut b = vec![0u8; 65_536];
    for _ in 0..reps {
        let mut got = 0u64;
        while got < bytes {
            let k = usize::try_from(bytes - got).unwrap_or(b.len()).min(b.len());
            let r = input.read(&mut b[..k])?;
            if r == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "drain eof"));
            }
            got += r as u64;
        }
        out.write_all(b"k")?;
        out.flush()?;
    }
    Ok(())
}
