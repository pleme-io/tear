use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{Lcg, Measured, Plan};
use crate::matrix::{Band, Floor};
use crate::seam;

fn wait_armed(armed: &AtomicBool) -> io::Result<()> {
    let until = Instant::now() + Duration::from_secs(20);
    while !armed.swap(false, Ordering::SeqCst) {
        if Instant::now() > until {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "waiter never armed",
            ));
        }
        std::hint::spin_loop();
    }
    Ok(())
}

fn channel(total: usize, band: Band, gap_us: u64, jitter_us: u64) -> io::Result<Vec<f64>> {
    let armed = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Instant>();
    let a2 = Arc::clone(&armed);
    let waiter = std::thread::Builder::new()
        .name("tearbench-wake".into())
        .spawn(move || -> io::Result<Vec<f64>> {
            seam::apply_thread_band(band)?;
            let mut v = Vec::with_capacity(total);
            for _ in 0..total {
                a2.store(true, Ordering::SeqCst);
                let t0 = rx
                    .recv()
                    .map_err(|e| io::Error::other(format!("wake channel: {e}")))?;
                v.push(t0.elapsed().as_nanos() as f64);
            }
            Ok(v)
        })?;
    let mut rng = Lcg(0x1234_5678);
    for _ in 0..total {
        wait_armed(&armed)?;
        std::thread::sleep(rng.pace(gap_us, jitter_us));
        tx.send(Instant::now())
            .map_err(|e| io::Error::other(format!("wake send: {e}")))?;
    }
    waiter
        .join()
        .map_err(|_| io::Error::other("wake waiter panicked"))?
}

pub fn hot(plan: Plan) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let n = plan.n(10_000);
    let warm = n / 20;
    let v = channel(warm + n, plan.band, 50, 100)?;
    Ok(Measured::new(
        Floor::WakeHot,
        format!("mpsc;gap=50+100us;band={}", plan.band.name()),
        v[warm..].to_vec(),
    ))
}

pub fn idle(plan: Plan) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let n = plan.n(2_000);
    let warm = 50.min(n / 10);
    let v = channel(warm + n, plan.band, 2_000, 1_000)?;
    Ok(Measured::new(
        Floor::WakeIdle,
        format!("mpsc;gap=2000+1000us;band={}", plan.band.name()),
        v[warm..].to_vec(),
    ))
}

pub fn run_loop(plan: Plan, from_idle: bool) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let n = plan.n(if from_idle { 2_000 } else { 10_000 });
    let warm = n / 20;
    let (gap, jitter) = if from_idle { (2_000, 1_000) } else { (50, 100) };
    let rng = std::sync::Mutex::new(Lcg(0x5EED));
    let lat = seam::run_loop_wakes(
        warm + n,
        |_| {
            rng.lock()
                .map_or(Duration::from_micros(gap), |mut r| r.pace(gap, jitter))
        },
        plan.band,
    )?;
    Ok(Measured::new(
        Floor::WakeRunLoop,
        format!("cfrunloop;gap={gap}+{jitter}us;band={}", plan.band.name()),
        lat[warm.min(lat.len())..]
            .iter()
            .map(|v| *v as f64)
            .collect(),
    ))
}
