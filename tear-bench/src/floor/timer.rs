use std::io;
use std::time::{Duration, Instant};

use super::{Measured, Plan};
use crate::matrix::Floor;
use crate::seam;

pub fn sleep_1ms(plan: Plan) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let n = plan.n(2_000);
    let d = Duration::from_millis(1);
    let mut ns = Vec::with_capacity(n);
    for i in 0..(n + 20) {
        let t0 = Instant::now();
        std::thread::sleep(d);
        if i >= 20 {
            ns.push(t0.elapsed().as_nanos() as f64);
        }
    }
    Ok(Measured::new(
        Floor::Timer,
        format!("sleep;req=1ms;band={}", plan.band.name()),
        ns,
    ))
}
