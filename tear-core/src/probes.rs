use std::time::{Duration, Instant};

use crate::pane_grid::PaneGrid;

#[must_use]
pub fn screen_parse(cols: usize, rows: usize, vt: &[u8]) -> Duration {
    let mut grid = PaneGrid::new(cols, rows);
    let t0 = Instant::now();
    grid.feed(vt);
    let dt = t0.elapsed();
    std::hint::black_box(&grid);
    dt
}

#[derive(Copy, Clone, Debug)]
pub struct FeedPlan {
    pub cols: usize,
    pub rows: usize,
    pub scrollback: usize,
    pub chunk: usize,
    pub warm_chunks: usize,
    pub measured_chunks: usize,
}

pub fn feed_allocations(
    plan: &FeedPlan,
    data: &[u8],
    allocations_during: fn(&mut dyn FnMut()) -> u64,
) -> u64 {
    let mut grid = PaneGrid::with_scrollback(plan.cols, plan.rows, plan.scrollback);
    let mut chunks = data.chunks(plan.chunk.max(1));
    for c in chunks.by_ref().take(plan.warm_chunks) {
        grid.feed(c);
    }
    let measured: Vec<&[u8]> = chunks.take(plan.measured_chunks).collect();
    let n = allocations_during(&mut || {
        for c in &measured {
            grid.feed(c);
        }
    });
    std::hint::black_box(&grid);
    n
}
