use std::io;

use super::{Measured, Plan};

#[must_use]
pub fn screen_vt(cols: usize, rows: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for line in crate::harness::files::log_lines(0x9E37_79B9_7F4A_7C15).take(rows) {
        let mut l = line;
        l.truncate(cols.saturating_mul(2));
        out.extend_from_slice(&l);
    }
    out
}

#[cfg(feature = "bench-probes")]
pub fn screen(plan: Plan) -> io::Result<Measured> {
    crate::seam::apply_process_band(plan.band)?;
    let (cols, rows) = (163, 48);
    let vt = screen_vt(cols, rows);
    let n = plan.n(2_000);
    let ns = (0..n)
        .map(|_| tear_core::probes::screen_parse(cols, rows, &vt).as_nanos() as f64)
        .collect();
    Ok(Measured::new(
        crate::matrix::Floor::ScreenParse,
        format!(
            "PaneGrid::feed;{cols}x{rows};{}B;band={}",
            vt.len(),
            plan.band.name()
        ),
        ns,
    ))
}

#[cfg(not(feature = "bench-probes"))]
pub fn screen(_plan: Plan) -> io::Result<Measured> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the screen-parse floor reaches PaneGrid through tear-core's bench-probes feature; this build has none",
    ))
}
