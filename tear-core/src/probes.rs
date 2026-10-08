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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Splitter {
    Feeder,
    Legacy,
}

pub const SPLIT_READ: usize = 1024;
pub const SPLIT_TAILS: [&str; 3] = ["ã ✓ end", "ñoño end", "é.… end"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitLoss {
    pub tail: &'static str,
    pub whole: String,
    pub read: String,
}

fn text_after_reads(data: &[u8], read: usize, splitter: Splitter) -> String {
    let mut grid = PaneGrid::with_scrollback(2 * SPLIT_READ, 4, 4);
    let mut legacy = crate::feeder::legacy::Splitter::default();
    for c in data.chunks(read.max(1)) {
        match splitter {
            Splitter::Feeder => grid.feed(c),
            Splitter::Legacy => grid.feed_legacy(&mut legacy, c),
        }
    }
    grid.snapshot()
        .to_text_rows()
        .join("|")
        .trim_end_matches([' ', '|'])
        .to_owned()
}

fn last_chars(text: &str, n: usize) -> String {
    let skip = text.chars().count().saturating_sub(n);
    text.chars().skip(skip).collect()
}

#[must_use]
pub fn split_losses(splitter: Splitter) -> Vec<SplitLoss> {
    SPLIT_TAILS
        .iter()
        .filter_map(|tail| {
            let mut data = vec![b'a'; SPLIT_READ - 1];
            data.extend_from_slice(tail.as_bytes());
            data.extend_from_slice(b"\r\n");
            let whole = text_after_reads(&data, data.len(), splitter);
            let read = text_after_reads(&data, SPLIT_READ, splitter);
            (whole != read).then(|| SplitLoss {
                tail,
                whole: last_chars(&whole, 12),
                read: last_chars(&read, 12),
            })
        })
        .collect()
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
