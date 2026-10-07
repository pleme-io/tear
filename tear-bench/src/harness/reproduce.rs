use std::io;
use std::time::Duration;

use serde::Serialize;

use super::cases::{self, FLOOD_BYTES, HZ_GAP_MS, KEYLOSS_KEYS, KEYLOSS_ROWS, OFF_LAG_MS};
use super::{Harness, isolated_rig};
use crate::matrix::{JournalSync, Variant};
use crate::receipt::{Sample, Unit};

pub const FLOOD_FRAMES: u64 = 65_537;
pub const SPIKE_FLOOR: usize = 4;

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub receipt: &'static str,
    pub expected: String,
    pub measured: String,
    pub reproduced: bool,
}

pub fn key_loss(h: &Harness) -> io::Result<Check> {
    let (mut d, rig) = isolated_rig(h, Variant::BOUND, "reproduce-keyloss")?;
    let r = cases::keyloss(h, &rig, KEYLOSS_ROWS);
    rig.kill_all();
    drop(rig);
    d.stop(h);
    let r = r?;
    Ok(Check {
        name: "keys delivered at 3,000 rows via the mado-shaped path",
        receipt: "§2 C3: 0 of 20 (H)",
        expected: format!("0 of {KEYLOSS_KEYS}, with the send-only control delivered"),
        measured: format!(
            "{} of {}; control {} of {}; snapshot frame {} B",
            r.delivered, r.sent, r.control_delivered, r.control_sent, r.snapshot_frame
        ),
        reproduced: r.sent == KEYLOSS_KEYS
            && r.delivered == 0
            && r.control_sent > 0
            && r.control_delivered == r.control_sent,
    })
}

pub fn flood_frames(h: &Harness, reps: usize) -> io::Result<Check> {
    let (mut d, rig) = isolated_rig(h, Variant::BOUND, "reproduce-flood")?;
    let r = cases::flood(h, &rig, reps);
    rig.kill_all();
    drop(rig);
    d.stop(h);
    let r = r?;
    let frames: Vec<u64> = r.iter().map(|f| f.frames).collect();
    let bytes: Vec<u64> = r.iter().map(|f| f.bytes).collect();
    Ok(Check {
        name: "frames per 64 MiB flood",
        receipt: "§2 C9: 65,537 frames of 1,024 B per 64 MiB (H-d)",
        expected: format!("{FLOOD_FRAMES} in every rep"),
        measured: format!("frames {frames:?} for bytes {bytes:?}"),
        reproduced: !r.is_empty()
            && r.iter().all(|f| f.complete && f.frames == FLOOD_FRAMES)
            && FLOOD_BYTES <= r[0].bytes,
    })
}

pub const ALLOCATIONS_PER_KIB: u64 = 513;

pub fn allocations(h: &Harness) -> io::Result<Check> {
    let allocs = cases::allocations(h)?;
    let want = ALLOCATIONS_PER_KIB * cases::ALLOC_MEASURED_CHUNKS as u64;
    Ok(Check {
        name: "allocations per KiB of `yes` at 163 columns",
        receipt: "§2 C9: 513.0 allocations per 1 KiB chunk, steady state after 20,000 chunks, scrollback 10k (G)",
        expected: format!(
            "{ALLOCATIONS_PER_KIB} per KiB: {want} over {} chunks",
            cases::ALLOC_MEASURED_CHUNKS
        ),
        measured: format!("{allocs} over {} chunks", cases::ALLOC_MEASURED_CHUNKS),
        reproduced: allocs == want,
    })
}

pub fn held_spikes(h: &Harness, secs: u64) -> io::Result<Check> {
    let period = Duration::from_millis(10);
    let mut runs = Vec::new();
    for variant in [
        Variant::HELD,
        Variant {
            journal: JournalSync::PageCache,
            ..Variant::HELD
        },
    ] {
        let (mut d, rig) = isolated_rig(h, variant, "reproduce-series")?;
        let r = cases::series(h, &rig, secs, period);
        rig.kill_all();
        drop(rig);
        d.stop(h);
        runs.push(r?);
    }
    let held = &runs[0];
    let control = &runs[1];
    let spikes = held.spikes().len();
    let control_spikes = control.spikes().len();
    let (hz, gaps) = held.gaps_at_one_hertz();
    let (chz, cgaps) = control.gaps_at_one_hertz();
    let bound = SPIKE_FLOOR.max(2 * control_spikes);
    let at_1s = held.pairs_at(HZ_GAP_MS);
    let off_lag = held.pairs_at(OFF_LAG_MS);
    let control_at_1s = control.pairs_at(HZ_GAP_MS);
    let red_by_r17 = hz > 0 || spikes > bound;
    let periodic = at_1s > off_lag && at_1s > control_at_1s;
    h.emit(
        Sample::new(
            "reproduce",
            "held",
            "spike-pairs-at-1s",
            0,
            at_1s as f64,
            Unit::Count,
        )
        .detail(format!(
            "pairs of spikes 990-1040 ms apart: held {at_1s}, held at 1490-1540 ms {off_lag}, page-cache control {control_at_1s}; spikes held {spikes}, control {control_spikes}, bound max(4, 2x) = {bound}"
        )),
    );
    Ok(Check {
        name: "the ~1 Hz held echo spikes",
        receipt: "§2 C4: 24 and 19 of 1,500 echoes over 3 ms; 10 of 23 and 12 of 18 inter-spike gaps at 990–1,040 ms (H-c)",
        expected: "red by R17's gate (an inter-spike gap at 990–1,040 ms, or spikes over max(4, 2× the page-cache control)) and periodic at 1 s (more spike pairs 990–1,040 ms apart than 1,490–1,540 ms apart, and than the control at 1 s)".to_string(),
        measured: format!(
            "held {spikes} of {} over 3 ms, {hz} of {gaps} gaps at ~1 s, {at_1s} pairs at 1 s against {off_lag} at 1.5 s; page-cache control {control_spikes} of {}, {chz} of {cgaps} gaps at ~1 s, {control_at_1s} pairs at 1 s",
            held.sent, control.sent
        ),
        reproduced: red_by_r17 && periodic,
    })
}

pub fn run(h: &Harness, flood_reps: usize, series_secs: u64) -> Vec<Result<Check, String>> {
    let checks = vec![
        key_loss(h).map_err(|e| format!("key loss: {e}")),
        allocations(h).map_err(|e| format!("allocations: {e}")),
        flood_frames(h, flood_reps).map_err(|e| format!("flood: {e}")),
        held_spikes(h, series_secs).map_err(|e| format!("held spikes: {e}")),
    ];
    for (i, c) in checks.iter().enumerate() {
        let s = match c {
            Ok(c) => Sample::new(
                "reproduce",
                "receipt",
                c.name,
                i,
                if c.reproduced { 1.0 } else { 0.0 },
                Unit::Flag,
            )
            .ok(c.reproduced)
            .detail(format!(
                "expected {}; measured {}; receipt {}",
                c.expected, c.measured, c.receipt
            )),
            Err(e) => Sample::new("reproduce", "receipt", "errored", i, f64::NAN, Unit::Flag)
                .ok(false)
                .detail(e.clone()),
        };
        h.emit(s);
    }
    checks
}
