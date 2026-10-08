use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub use crate::probe_env::{DUMP as DUMP_ENV, FAULTS as FAULTS_ENV};

crate::closed_vocabulary! {
    Fault {
        MuteSink => "mute-sink",
        SnapshotHistoryAll => "snapshot-history-all",
        UnboundedSubscriberQueue => "unbounded-subscriber-queue",
        AuditEveryKey => "audit-every-key",
        ResponseSizeUnchecked => "response-size-unchecked",
        LegacyReplay => "legacy-replay",
        RawSubscribe => "raw-subscribe",
        UnchunkedInput => "unchunked-input",
    }
}

crate::closed_vocabulary! {
    Counter {
        ClientRpcs => "client-rpcs",
        ClientRedials => "client-redials",
        ClientReplays => "client-replays",
        JournalSyncs => "journal-syncs",
        JournalSyncsInAppend => "journal-syncs-in-append",
        JournalUnlinksInAppend => "journal-unlinks-in-append",
        HolderSinksMuted => "holder-sinks-muted",
        SnapshotRows => "snapshot-rows",
        AuditWrites => "audit-writes",
    }
}

crate::closed_vocabulary! {
    Gauge {
        SubscriberBacklog => "subscriber-backlog",
    }
}

const FAULT_SLOTS: usize = Fault::ALL.len();
const COUNTER_SLOTS: usize = Counter::ALL.len();
const GAUGE_SLOTS: usize = Gauge::ALL.len();

static ARMED: [AtomicBool; FAULT_SLOTS] = [const { AtomicBool::new(false) }; FAULT_SLOTS];
static COUNTS: [AtomicU64; COUNTER_SLOTS] = [const { AtomicU64::new(0) }; COUNTER_SLOTS];
static LEVELS: [AtomicU64; GAUGE_SLOTS] = [const { AtomicU64::new(0) }; GAUGE_SLOTS];
static PEAKS: [AtomicU64; GAUGE_SLOTS] = [const { AtomicU64::new(0) }; GAUGE_SLOTS];
static TOTALS: [AtomicU64; GAUGE_SLOTS] = [const { AtomicU64::new(0) }; GAUGE_SLOTS];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FaultList {
    pub faults: Vec<Fault>,
    pub refused: Vec<String>,
}

impl FaultList {
    #[must_use]
    pub fn parse(list: &str) -> Self {
        let mut out = Self::default();
        for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            match Fault::parse(entry) {
                Some(f) if !out.faults.contains(&f) => out.faults.push(f),
                Some(_) => {}
                None => out.refused.push(entry.to_string()),
            }
        }
        out
    }
}

pub fn arm(faults: &[Fault]) {
    for f in Fault::ALL {
        ARMED[f.index()].store(faults.contains(f), Ordering::SeqCst);
    }
}

#[must_use]
pub fn active(fault: Fault) -> bool {
    ARMED[fault.index()].load(Ordering::Relaxed)
}

pub fn bump(counter: Counter, n: u64) {
    COUNTS[counter.index()].fetch_add(n, Ordering::Relaxed);
}

#[must_use]
pub fn count(counter: Counter) -> u64 {
    COUNTS[counter.index()].load(Ordering::Relaxed)
}

pub fn raise(gauge: Gauge, n: u64) {
    TOTALS[gauge.index()].fetch_add(n, Ordering::Relaxed);
    let now = LEVELS[gauge.index()].fetch_add(n, Ordering::Relaxed) + n;
    PEAKS[gauge.index()].fetch_max(now, Ordering::Relaxed);
}

pub fn lower(gauge: Gauge, n: u64) {
    let _ = LEVELS[gauge.index()].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        Some(v.saturating_sub(n))
    });
}

#[must_use]
pub fn level(gauge: Gauge) -> u64 {
    LEVELS[gauge.index()].load(Ordering::Relaxed)
}

#[must_use]
pub fn peak(gauge: Gauge) -> u64 {
    PEAKS[gauge.index()].load(Ordering::Relaxed)
}

#[must_use]
pub fn total(gauge: Gauge) -> u64 {
    TOTALS[gauge.index()].load(Ordering::Relaxed)
}

pub fn reset() {
    for c in &COUNTS {
        c.store(0, Ordering::SeqCst);
    }
    for g in LEVELS.iter().chain(PEAKS.iter()).chain(TOTALS.iter()) {
        g.store(0, Ordering::SeqCst);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading {
    Counter(Counter, u64),
    GaugeLevel(Gauge, u64),
    GaugePeak(Gauge, u64),
    GaugeTotal(Gauge, u64),
    Armed(Fault),
}

impl Reading {
    #[must_use]
    pub fn row(&self) -> String {
        match self {
            Reading::Counter(c, v) => format!("counter\t{}\t{v}", c.name()),
            Reading::GaugeLevel(g, v) => format!("gauge-level\t{}\t{v}", g.name()),
            Reading::GaugePeak(g, v) => format!("gauge-peak\t{}\t{v}", g.name()),
            Reading::GaugeTotal(g, v) => format!("gauge-total\t{}\t{v}", g.name()),
            Reading::Armed(f) => format!("fault\t{}\t1", f.name()),
        }
    }

    #[must_use]
    pub fn parse_row(line: &str) -> Option<Self> {
        let mut cols = line.split('\t');
        let kind = cols.next()?;
        let name = cols.next()?;
        let value: u64 = cols.next()?.parse().ok()?;
        match kind {
            "counter" => Some(Reading::Counter(Counter::parse(name)?, value)),
            "gauge-level" => Some(Reading::GaugeLevel(Gauge::parse(name)?, value)),
            "gauge-peak" => Some(Reading::GaugePeak(Gauge::parse(name)?, value)),
            "gauge-total" => Some(Reading::GaugeTotal(Gauge::parse(name)?, value)),
            "fault" => Some(Reading::Armed(Fault::parse(name)?)),
            _ => None,
        }
    }
}

#[must_use]
pub fn readings() -> Vec<Reading> {
    let mut out = Vec::with_capacity(COUNTER_SLOTS + 3 * GAUGE_SLOTS + FAULT_SLOTS);
    out.extend(Counter::ALL.iter().map(|c| Reading::Counter(*c, count(*c))));
    for g in Gauge::ALL {
        out.push(Reading::GaugeLevel(*g, level(*g)));
        out.push(Reading::GaugePeak(*g, peak(*g)));
        out.push(Reading::GaugeTotal(*g, total(*g)));
    }
    out.extend(
        Fault::ALL
            .iter()
            .filter(|f| active(**f))
            .map(|f| Reading::Armed(*f)),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_round_trips_and_is_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for f in Fault::ALL {
            assert_eq!(Fault::parse(f.name()), Some(*f));
            assert!(seen.insert(f.name()));
        }
        for c in Counter::ALL {
            assert_eq!(Counter::parse(c.name()), Some(*c));
            assert!(seen.insert(c.name()));
        }
        for g in Gauge::ALL {
            assert_eq!(Gauge::parse(g.name()), Some(*g));
            assert!(seen.insert(g.name()));
        }
    }

    #[test]
    fn a_fault_list_refuses_only_the_unknown_entry() {
        let l = FaultList::parse("mute-sink, bogus,,snapshot-history-all,mute-sink");
        assert_eq!(l.faults, vec![Fault::MuteSink, Fault::SnapshotHistoryAll]);
        assert_eq!(l.refused, vec!["bogus".to_string()]);
    }

    #[test]
    fn readings_round_trip_through_their_rows() {
        let rows = [
            Reading::Counter(Counter::ClientRpcs, 7),
            Reading::GaugeLevel(Gauge::SubscriberBacklog, 3),
            Reading::GaugePeak(Gauge::SubscriberBacklog, 9),
            Reading::GaugeTotal(Gauge::SubscriberBacklog, 12),
            Reading::Armed(Fault::MuteSink),
        ];
        for r in rows {
            assert_eq!(Reading::parse_row(&r.row()), Some(r));
        }
        assert_eq!(Reading::parse_row("counter\tnope\t1"), None);
    }

    #[test]
    fn a_gauge_keeps_its_peak_and_never_underflows() {
        raise(Gauge::SubscriberBacklog, 5);
        lower(Gauge::SubscriberBacklog, 2);
        raise(Gauge::SubscriberBacklog, 1);
        lower(Gauge::SubscriberBacklog, 100);
        assert_eq!(level(Gauge::SubscriberBacklog), 0);
        assert!(peak(Gauge::SubscriberBacklog) >= 5);
        assert!(total(Gauge::SubscriberBacklog) >= 6);
    }
}
