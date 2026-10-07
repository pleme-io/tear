use std::io;
use std::time::Instant;

use super::pty::{open_raw, slave_stdio};
use super::{Measured, Plan, peer_command};
use crate::matrix::Floor;
use crate::seam;

pub fn spawn_openpty(plan: Plan) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let n = plan.n(200).min(200);
    let mut ns = Vec::with_capacity(n);
    for _ in 0..n {
        let t0 = Instant::now();
        let pty = open_raw()?;
        let mut child = peer_command(&["exit"])?
            .stdin(slave_stdio(&pty.slave)?)
            .stdout(slave_stdio(&pty.slave)?)
            .stderr(slave_stdio(&pty.slave)?)
            .spawn()?;
        drop(pty.slave);
        let st = child.wait()?;
        let dt = t0.elapsed();
        drop(pty.master);
        if !st.success() {
            return Err(io::Error::other(format!("spawn floor peer exited {st}")));
        }
        ns.push(dt.as_nanos() as f64);
    }
    Ok(Measured::new(
        Floor::SpawnOpenpty,
        format!("openpty+spawn+exit;band={}", plan.band.name()),
        ns,
    ))
}
