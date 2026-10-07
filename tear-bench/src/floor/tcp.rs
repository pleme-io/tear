use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use super::{Measured, Plan, flag, flag_band, flag_u64, peer_command, scratch_path, wait_for_path};
use crate::matrix::Floor;
use crate::seam;

pub fn round_trip(plan: Plan) -> io::Result<Measured> {
    let size = 64usize;
    let portfile = scratch_path("tcp.port");
    let _ = std::fs::remove_file(&portfile);
    let mut child = peer_command(&[
        "tcp-echo",
        "--portfile",
        &portfile.to_string_lossy(),
        "--size",
        &size.to_string(),
        "--band",
        plan.band.name(),
    ])?
    .spawn()?;
    wait_for_path(&portfile, &mut child, Duration::from_secs(10))?;
    let port: u16 = std::fs::read_to_string(&portfile)?
        .trim()
        .parse()
        .map_err(|e| io::Error::other(format!("port file: {e}")))?;
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_nodelay(true)?;
    seam::apply_process_band(plan.band)?;
    let n = plan.n(20_000);
    let warm = n / 10;
    let out = vec![0x5Au8; size];
    let mut back = vec![0u8; size];
    let mut ns = Vec::with_capacity(n);
    for i in 0..(warm + n) {
        let t0 = Instant::now();
        s.write_all(&out)?;
        s.read_exact(&mut back)?;
        if i >= warm {
            ns.push(t0.elapsed().as_nanos() as f64);
        }
    }
    drop(s);
    let _ = child.wait();
    let _ = std::fs::remove_file(&portfile);
    Ok(Measured::new(
        Floor::TcpRoundTrip,
        format!("loopback;nodelay;size={size};band={}", plan.band.name()),
        ns,
    ))
}

pub fn echo_peer(args: &[String]) -> io::Result<()> {
    let portfile = flag(args, "--portfile").ok_or_else(|| io::Error::other("--portfile"))?;
    let size = usize::try_from(flag_u64(args, "--size", 64)?).unwrap_or(64);
    seam::apply_process_band(flag_band(args)?)?;
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    let tmp = format!("{portfile}.tmp");
    std::fs::write(&tmp, port.to_string())?;
    std::fs::rename(&tmp, portfile)?;
    let (mut s, _) = l.accept()?;
    s.set_nodelay(true)?;
    let mut b = vec![0u8; size];
    loop {
        match s.read_exact(&mut b) {
            Ok(()) => s.write_all(&b)?,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}
