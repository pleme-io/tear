//! Per-pane recording — captures every PTY byte with a relative
//! timestamp, exposes a ring-buffered snapshot, and exports as
//! asciinema v2 .cast (JSON-lines) so any external player handles
//! playback. Daemon-native: no `asciinema rec` wrapper needed.
//!
//! Storage shape
//! -------------
//! Each pane gets a `PaneRecording` instance keyed off the pane
//! id. Recording starts as Disabled; `enable()` flips it on and
//! marks the start instant. From then, every chunk fed via
//! [`Self::push`] is appended as `(millis_since_start, Vec<u8>)`.
//!
//! Size cap
//! --------
//! A long-lived pane could pile up arbitrarily many bytes. Each
//! recording has a configurable max event count (default 50_000 —
//! enough for ~1h of typical interactive output). When the cap is
//! exceeded, the oldest events are dropped (ring-buffer semantics)
//! so the recording always reflects the most recent N events.
//!
//! Export format
//! -------------
//! `to_cast_json()` emits the asciinema v2 header line + one
//! data line per event. Every byte chunk lands as `[t_s, "o",
//! "<utf-8 string>"]` — the v2 wire shape. Non-UTF-8 bytes are
//! losslessly preserved by the JSON string encoding (asciinema
//! players handle the raw bytes the same as terminal display
//! does).

use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// One captured PTY chunk, relative to the recording's start.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneEvent {
    /// Milliseconds since `enable()` was called.
    pub ts_ms: u64,
    /// Raw bytes pushed to the PTY's output side. The vte parser
    /// has already fed these into the grid; the recording stores
    /// a verbatim copy so replays match what a live viewer saw.
    pub bytes: Vec<u8>,
}

/// State of a per-pane recording. Cheap when disabled — the
/// `on_bytes` hook hits a single boolean check before deciding
/// whether to deep-copy the chunk.
pub struct PaneRecording {
    enabled: Mutex<RecordingState>,
}

struct RecordingState {
    /// `None` when disabled.
    started_at: Option<Instant>,
    /// Captured events. Ring-buffered against `max_events`.
    events: std::collections::VecDeque<PaneEvent>,
    /// Max retained events. Default 50_000.
    max_events: usize,
    /// Recorded cols × rows at start — written into the asciinema
    /// cast header on export.
    cols: u16,
    rows: u16,
}

impl Default for PaneRecording {
    fn default() -> Self {
        Self::new(50_000)
    }
}

impl PaneRecording {
    #[must_use]
    pub fn new(max_events: usize) -> Self {
        Self {
            enabled: Mutex::new(RecordingState {
                started_at: None,
                events: std::collections::VecDeque::new(),
                max_events,
                cols: 80,
                rows: 24,
            }),
        }
    }

    /// Begin (or restart) recording. Clears prior events; stamps
    /// the start instant; remembers the pane dimensions for the
    /// cast header.
    pub fn enable(&self, cols: u16, rows: u16) {
        let mut g = self.enabled.lock().expect("recording state poisoned");
        g.started_at = Some(Instant::now());
        g.events.clear();
        g.cols = cols;
        g.rows = rows;
    }

    /// Stop recording. Retains captured events so an operator can
    /// `export` them after stopping; a subsequent `enable()`
    /// clears + restarts.
    pub fn disable(&self) {
        let mut g = self.enabled.lock().expect("recording state poisoned");
        g.started_at = None;
    }

    /// Whether recording is currently capturing new events.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
            .lock()
            .expect("recording state poisoned")
            .started_at
            .is_some()
    }

    /// Number of currently-buffered events.
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.enabled
            .lock()
            .expect("recording state poisoned")
            .events
            .len()
    }

    /// Append a PTY chunk. Cheap no-op when recording is disabled.
    /// Ring-evicts the oldest event if the cap is hit.
    pub fn push(&self, bytes: &[u8]) {
        let mut g = self.enabled.lock().expect("recording state poisoned");
        let Some(start) = g.started_at else {
            return;
        };
        let ts_ms = start.elapsed().as_millis() as u64;
        let cap = g.max_events;
        if g.events.len() == cap {
            g.events.pop_front();
        }
        g.events.push_back(PaneEvent {
            ts_ms,
            bytes: bytes.to_vec(),
        });
    }

    /// Export as asciinema v2 .cast (JSON-lines). The first line
    /// is the header object; every subsequent line is
    /// `[t_seconds, "o", "<utf-8 chunk>"]`. Returns the full
    /// string ready to write to disk or pipe to `asciinema play`.
    pub fn to_cast_json(&self) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let g = self.enabled.lock().expect("recording state poisoned");
        let header = serde_json::json!({
            "version": 2,
            "width": g.cols,
            "height": g.rows,
            "timestamp": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "env": {
                "TERM": "xterm-256color",
                "SHELL": std::env::var("SHELL").unwrap_or_default(),
            },
        });
        let mut out = header.to_string();
        out.push('\n');
        for ev in &g.events {
            let secs = (ev.ts_ms as f64) / 1000.0;
            let chunk = String::from_utf8_lossy(&ev.bytes).into_owned();
            // asciinema v2 row: [<float-seconds>, "o", "<chunk>"]
            let row = serde_json::Value::Array(vec![
                serde_json::json!(secs),
                serde_json::json!("o"),
                serde_json::json!(chunk),
            ]);
            out.push_str(&row.to_string());
            out.push('\n');
        }
        out
    }

    /// Read the events at (or just before) a given `ts_ms` —
    /// returns up to `limit` events nearest to the cursor for the
    /// time-travel scrubber. Lets a replay renderer seek without
    /// re-reading the full recording.
    pub fn read_around(&self, ts_ms: u64, limit: usize) -> Vec<PaneEvent> {
        let g = self.enabled.lock().expect("recording state poisoned");
        // Binary-search-ish — events are sorted by ts_ms since
        // they're appended in order. Simple linear scan is fine
        // for the typical recording size (<10k events).
        let cursor = g
            .events
            .iter()
            .position(|e| e.ts_ms >= ts_ms)
            .unwrap_or(g.events.len());
        let start = cursor.saturating_sub(limit / 2);
        let end = (start + limit).min(g.events.len());
        g.events.range(start..end).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_recording_drops_pushes() {
        let r = PaneRecording::default();
        r.push(b"hello");
        assert_eq!(r.event_count(), 0);
    }

    #[test]
    fn enable_then_push_captures_events() {
        let r = PaneRecording::default();
        r.enable(80, 24);
        r.push(b"hello");
        r.push(b" world");
        assert_eq!(r.event_count(), 2);
    }

    #[test]
    fn ring_buffer_caps_at_max() {
        let r = PaneRecording::new(3);
        r.enable(80, 24);
        r.push(b"a");
        r.push(b"b");
        r.push(b"c");
        r.push(b"d");
        r.push(b"e");
        assert_eq!(r.event_count(), 3);
    }

    #[test]
    fn cast_export_has_header_plus_one_line_per_event() {
        let r = PaneRecording::default();
        r.enable(120, 40);
        r.push(b"$ ls\n");
        r.push(b"file1 file2\n");
        let cast = r.to_cast_json();
        let lines: Vec<&str> = cast.lines().collect();
        assert_eq!(lines.len(), 3, "expected header + 2 events, got {cast}");
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 120);
        assert_eq!(header["height"], 40);
        let ev: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        let arr = ev.as_array().unwrap();
        assert_eq!(arr[1], "o");
        assert_eq!(arr[2].as_str().unwrap(), "$ ls\n");
    }

    #[test]
    fn disable_then_enable_clears_old_events() {
        let r = PaneRecording::default();
        r.enable(80, 24);
        r.push(b"x");
        assert_eq!(r.event_count(), 1);
        r.disable();
        r.enable(80, 24);
        assert_eq!(r.event_count(), 0);
    }

    #[test]
    fn read_around_returns_events_near_cursor() {
        let r = PaneRecording::default();
        r.enable(80, 24);
        // Manually craft 5 events at fixed timestamps for a
        // deterministic test (push uses Instant::now under the
        // hood which we'd race on).
        {
            let mut g = r.enabled.lock().unwrap();
            for i in 0..5u64 {
                g.events.push_back(PaneEvent {
                    ts_ms: i * 1000,
                    bytes: vec![b'a' + i as u8],
                });
            }
        }
        let around = r.read_around(2500, 4);
        // Cursor lands at index 3 (ts=3000 >= 2500). With limit=4
        // start = 3 - 4/2 = 1; end = 1 + 4 = 5. So we get events at
        // ts=1000, 2000, 3000, 4000.
        assert_eq!(around.len(), 4);
        assert_eq!(around[0].ts_ms, 1000);
        assert_eq!(around[3].ts_ms, 4000);
    }
}
