use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use nix::sys::socket::{getsockopt, setsockopt, sockopt};

use super::{Lcg, Measured, Plan, flag, flag_band, flag_u64, now_ns, peer_command, scratch_path};
use crate::matrix::Floor;
use crate::seam;

fn size_buffers(s: &UnixStream, bytes: u64) -> io::Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let b = usize::try_from(bytes).unwrap_or(usize::MAX);
    setsockopt(s, sockopt::SndBuf, &b).map_err(io::Error::from)?;
    setsockopt(s, sockopt::RcvBuf, &b).map_err(io::Error::from)?;
    Ok(())
}

#[must_use]
pub fn buffer_sizes(s: &UnixStream) -> (usize, usize) {
    (
        getsockopt(s, sockopt::SndBuf).unwrap_or(0),
        getsockopt(s, sockopt::RcvBuf).unwrap_or(0),
    )
}

fn connect_retry(path: &Path, child: &mut std::process::Child) -> io::Result<UnixStream> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) => {
                if let Some(st) = child.try_wait()? {
                    return Err(io::Error::other(format!("uds peer exited early: {st}")));
                }
                if Instant::now() > until {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

pub fn round_trip(framed: bool, size: usize, plan: Plan, buf: u64) -> io::Result<Measured> {
    let path = scratch_path("rtt.sock");
    let frame = if framed { "framed" } else { "raw" };
    let mut child = peer_command(&[
        "uds-echo",
        "--path",
        &path.to_string_lossy(),
        "--size",
        &size.to_string(),
        "--frame",
        frame,
        "--buf",
        &buf.to_string(),
        "--band",
        plan.band.name(),
    ])?
    .spawn()?;
    let mut s = connect_retry(&path, &mut child)?;
    seam::apply_process_band(plan.band)?;
    size_buffers(&s, buf)?;
    let n = plan.n(20_000);
    let warm = n / 10;
    let out = vec![0x5Au8; size];
    let mut back = vec![0u8; size];
    let len = u32::try_from(size).unwrap_or(u32::MAX).to_be_bytes();
    let mut ns = Vec::with_capacity(n);
    for i in 0..(warm + n) {
        let t0 = Instant::now();
        if framed {
            s.write_all(&len)?;
            s.write_all(&out)?;
            let mut l4 = [0u8; 4];
            s.read_exact(&mut l4)?;
            s.read_exact(&mut back)?;
        } else {
            s.write_all(&out)?;
            s.read_exact(&mut back)?;
        }
        if i >= warm {
            ns.push(t0.elapsed().as_nanos() as f64);
        }
    }
    drop(s);
    let st = child.wait()?;
    let _ = std::fs::remove_file(&path);
    if !st.success() {
        return Err(io::Error::other(format!("uds echo peer exited {st}")));
    }
    let floor = if framed {
        Floor::UdsRoundTripFramed
    } else {
        Floor::UdsRoundTripRaw
    };
    Ok(Measured::new(
        floor,
        format!("{frame};size={size};buf={buf};band={}", plan.band.name()),
        ns,
    ))
}

pub fn echo_peer(args: &[String]) -> io::Result<()> {
    let path = flag(args, "--path").ok_or_else(|| io::Error::other("--path"))?;
    let size = usize::try_from(flag_u64(args, "--size", 64)?).unwrap_or(64);
    let framed = flag(args, "--frame") == Some("framed");
    let buf = flag_u64(args, "--buf", 0)?;
    seam::apply_process_band(flag_band(args)?)?;
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    let (mut s, _) = l.accept()?;
    size_buffers(&s, buf)?;
    let mut b = vec![0u8; size.max(4)];
    let res: io::Result<()> = (|| {
        loop {
            if framed {
                let mut l4 = [0u8; 4];
                s.read_exact(&mut l4)?;
                let n = u32::from_be_bytes(l4) as usize;
                if n > b.len() {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too big"));
                }
                s.read_exact(&mut b[..n])?;
                s.write_all(&l4)?;
                s.write_all(&b[..n])?;
            } else {
                s.read_exact(&mut b[..size])?;
                s.write_all(&b[..size])?;
            }
        }
    })();
    let _ = std::fs::remove_file(path);
    match res {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
        other => other,
    }
}

fn one_way_inner(plan: Plan) -> io::Result<(Vec<f64>, Vec<f64>)> {
    let path = scratch_path("oneway.sock");
    let out = scratch_path("oneway.lat");
    let n = plan.n(10_000);
    let warm = n / 20;
    let total = warm + n;
    let mut child = peer_command(&[
        "uds-reader",
        "--path",
        &path.to_string_lossy(),
        "--n",
        &total.to_string(),
        "--out",
        &out.to_string_lossy(),
        "--band",
        plan.band.name(),
    ])?
    .spawn()?;
    let mut s = connect_retry(&path, &mut child)?;
    seam::apply_process_band(plan.band)?;
    let mut rng = Lcg(0x00C0_FFEE);
    let mut msg = [0u8; 64];
    let mut send = Vec::with_capacity(n);
    for i in 0..total {
        std::thread::sleep(rng.pace(50, 100));
        let t0 = now_ns();
        msg[..8].copy_from_slice(&t0.to_ne_bytes());
        s.write_all(&msg)?;
        let t1 = now_ns();
        if i >= warm {
            send.push(t1.saturating_sub(t0) as f64);
        }
    }
    drop(s);
    let st = child.wait()?;
    let _ = std::fs::remove_file(&path);
    if !st.success() {
        return Err(io::Error::other(format!("uds reader peer exited {st}")));
    }
    let raw = std::fs::read_to_string(&out)?;
    let _ = std::fs::remove_file(&out);
    let lat: Vec<f64> = raw
        .lines()
        .skip(warm)
        .filter_map(|l| l.parse::<f64>().ok())
        .collect();
    Ok((lat, send))
}

pub fn one_way(plan: Plan) -> io::Result<Measured> {
    let (lat, _) = one_way_inner(plan)?;
    Ok(Measured::new(
        Floor::UdsOneWay,
        format!("64B;gap=50+100us;band={}", plan.band.name()),
        lat,
    ))
}

pub fn send(plan: Plan) -> io::Result<Measured> {
    let (_, send) = one_way_inner(plan)?;
    Ok(Measured::new(
        Floor::UdsSend,
        format!("64B;gap=50+100us;band={}", plan.band.name()),
        send,
    ))
}

pub fn reader_peer(args: &[String]) -> io::Result<()> {
    let path = flag(args, "--path").ok_or_else(|| io::Error::other("--path"))?;
    let out = flag(args, "--out").ok_or_else(|| io::Error::other("--out"))?;
    let total = usize::try_from(flag_u64(args, "--n", 1000)?).unwrap_or(1000);
    seam::apply_process_band(flag_band(args)?)?;
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    let (mut s, _) = l.accept()?;
    let mut msg = [0u8; 64];
    let mut lat = Vec::with_capacity(total);
    for _ in 0..total {
        s.read_exact(&mut msg)?;
        let t1 = now_ns();
        let mut t0 = [0u8; 8];
        t0.copy_from_slice(&msg[..8]);
        lat.push(t1.saturating_sub(u64::from_ne_bytes(t0)));
    }
    let mut body = String::with_capacity(lat.len() * 8);
    for v in &lat {
        body.push_str(&v.to_string());
        body.push('\n');
    }
    std::fs::write(out, body)
}

pub fn throughput(plan: Plan) -> io::Result<Measured> {
    let bytes: u64 = 256 << 20;
    let chunk = 65_536usize;
    let reps = usize::try_from((5 / plan.scale.max(1)).max(2)).unwrap_or(2);
    let data = vec![0x61u8; chunk];
    let mut ns = Vec::new();
    for rep in 0..reps {
        let path = scratch_path(&format!("tput{rep}.sock"));
        let mut child = peer_command(&[
            "uds-sink",
            "--path",
            &path.to_string_lossy(),
            "--band",
            plan.band.name(),
        ])?
        .spawn()?;
        let mut s = connect_retry(&path, &mut child)?;
        let t0 = Instant::now();
        s.write_all(&bytes.to_be_bytes())?;
        let mut left = bytes;
        while left > 0 {
            let k = usize::try_from(left).unwrap_or(chunk).min(chunk);
            s.write_all(&data[..k])?;
            left -= k as u64;
        }
        let mut ack = [0u8; 8];
        s.read_exact(&mut ack)?;
        let dt = t0.elapsed();
        drop(s);
        let st = child.wait()?;
        let _ = std::fs::remove_file(&path);
        if !st.success() || u64::from_be_bytes(ack) != bytes {
            return Err(io::Error::other("uds sink did not count every byte"));
        }
        ns.push(dt.as_nanos() as f64 / (bytes as f64 / f64::from(1 << 20)));
    }
    Ok(Measured::new(
        Floor::UdsThroughput,
        format!(
            "ns-per-mib;chunk={chunk};buf=default;band={}",
            plan.band.name()
        ),
        ns,
    ))
}

pub fn sink_peer(args: &[String]) -> io::Result<()> {
    let path = flag(args, "--path").ok_or_else(|| io::Error::other("--path"))?;
    seam::apply_process_band(flag_band(args)?)?;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let (mut stream, _) = listener.accept()?;
    let mut hdr = [0u8; 8];
    stream.read_exact(&mut hdr)?;
    let want = u64::from_be_bytes(hdr);
    let mut buf = vec![0u8; 65_536];
    let mut total = 0u64;
    while total < want {
        let k = usize::try_from(want - total)
            .unwrap_or(buf.len())
            .min(buf.len());
        let got = stream.read(&mut buf[..k])?;
        if got == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "sink eof"));
        }
        total += got as u64;
    }
    stream.write_all(&total.to_be_bytes())?;
    let _ = std::fs::remove_file(path);
    Ok(())
}
