use tear_client::Transport;
use tear_config::SessionDurability;
use tear_types::{Durability, HostRole};

tear_types::closed_vocabulary! {
    Rung {
        R0 => "R0", R1 => "R1", R2 => "R2", R3 => "R3", R4 => "R4", R5 => "R5", R6 => "R6",
        R7 => "R7", R8 => "R8", R9 => "R9", R10 => "R10", R11 => "R11", R12 => "R12",
        R13 => "R13", R14 => "R14", R15 => "R15", R16 => "R16", R17 => "R17", R18 => "R18",
        R19 => "R19", R20 => "R20", R21 => "R21", R22 => "R22", R23 => "R23", R24 => "R24",
        R25 => "R25", R26 => "R26", R27 => "R27", R28 => "R28", R29 => "R29", R30 => "R30",
        R31 => "R31", R32 => "R32", R33 => "R33", R34 => "R34", R35 => "R35", R36 => "R36",
        R37 => "R37", R38 => "R38", R39 => "R39", R40 => "R40", R41 => "R41", R42 => "R42",
        R43 => "R43",
    }
}

pub const LANDED: &[Rung] = &[Rung::R1, Rung::R3, Rung::R4, Rung::R7, Rung::R10];

#[must_use]
pub const fn landed(rung: Rung) -> bool {
    let mut i = 0;
    while i < LANDED.len() {
        if LANDED[i].index() == rung.index() {
            return true;
        }
        i += 1;
    }
    false
}

tear_types::closed_vocabulary! {
    Remote { Tcp => "tcp", Ssh => "ssh", WsBridge => "ws-bridge" }
}

tear_types::closed_vocabulary! {
    Handover { Attach => "attach", Switch => "switch", Reattach => "reattach", Readopt => "readopt" }
}

tear_types::closed_vocabulary! {
    Input { Keys => "keys", Paste => "paste", Mouse => "mouse" }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Case {
    C1,
    C2,
    C3,
    C4,
    C5,
    C6(Remote),
    C7(Handover),
    C8,
    C9,
    C10,
    C11,
    C12(Input),
    C13,
}

impl Case {
    pub const ALL: [Case; 20] = [
        Case::C1,
        Case::C2,
        Case::C3,
        Case::C4,
        Case::C5,
        Case::C6(Remote::Tcp),
        Case::C6(Remote::Ssh),
        Case::C6(Remote::WsBridge),
        Case::C7(Handover::Attach),
        Case::C7(Handover::Switch),
        Case::C7(Handover::Reattach),
        Case::C7(Handover::Readopt),
        Case::C8,
        Case::C9,
        Case::C10,
        Case::C11,
        Case::C12(Input::Keys),
        Case::C12(Input::Paste),
        Case::C12(Input::Mouse),
        Case::C13,
    ];

    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Case::C1 => 1,
            Case::C2 => 2,
            Case::C3 => 3,
            Case::C4 => 4,
            Case::C5 => 5,
            Case::C6(_) => 6,
            Case::C7(_) => 7,
            Case::C8 => 8,
            Case::C9 => 9,
            Case::C10 => 10,
            Case::C11 => 11,
            Case::C12(_) => 12,
            Case::C13 => 13,
        }
    }

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Case::C1 => "embedded",
            Case::C2 => "daemon, window-owned",
            Case::C3 => "resident",
            Case::C4 => "held",
            Case::C5 => "multi-attach",
            Case::C6(_) => "remote",
            Case::C7(_) => "attach, switch, reattach",
            Case::C8 => "startup",
            Case::C9 => "flood",
            Case::C10 => "idle",
            Case::C11 => "resize",
            Case::C12(_) => "input",
            Case::C13 => "observers",
        }
    }

    #[must_use]
    pub fn name(self) -> String {
        match self {
            Case::C6(r) => format!("C6-{}", r.name()),
            Case::C7(h) => format!("C7-{}", h.name()),
            Case::C12(i) => format!("C12-{}", i.name()),
            other => format!("C{}", other.number()),
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Case::ALL.into_iter().find(|c| c.name() == s)
    }

    #[must_use]
    pub const fn index(self) -> usize {
        let mut i = 0;
        while i < Case::ALL.len() {
            if Case::ALL[i].same(self) {
                return i;
            }
            i += 1;
        }
        usize::MAX
    }

    const fn same(self, other: Case) -> bool {
        match (self, other) {
            (Case::C6(a), Case::C6(b)) => a.index() == b.index(),
            (Case::C7(a), Case::C7(b)) => a.index() == b.index(),
            (Case::C12(a), Case::C12(b)) => a.index() == b.index(),
            (a, b) => a.number() == b.number(),
        }
    }
}

tear_types::closed_vocabulary! {
    Stat { P50 => "p50", P90 => "p90", P99 => "p99" }
}

impl Stat {
    #[must_use]
    pub const fn quantile(self) -> f64 {
        match self {
            Stat::P50 => 0.5,
            Stat::P90 => 0.9,
            Stat::P99 => 0.99,
        }
    }
}

tear_types::closed_vocabulary! {
    Floor {
        UdsRoundTripRaw => "uds-round-trip-raw",
        UdsRoundTripFramed => "uds-round-trip-framed",
        UdsOneWay => "uds-one-way",
        UdsSend => "uds-send",
        TcpRoundTrip => "tcp-round-trip",
        PtyEcho => "pty-echo",
        WakeHot => "wake-hot",
        WakeIdle => "wake-idle",
        WakeRunLoop => "wake-run-loop",
        PtyOutputCeiling => "pty-output-ceiling",
        PtyInputCeiling => "pty-input-ceiling",
        UdsThroughput => "uds-throughput",
        FlushPersisted => "flush-persisted",
        FlushOrdered => "flush-ordered",
        FlushHandedToDevice => "flush-handed-to-device",
        SerializeBytes => "serialize-bytes",
        Timer => "timer",
        SpawnOpenpty => "spawn-openpty",
        ScreenParse => "screen-parse",
        Present => "present",
        ParkedWindow => "parked-window",
        EchoEmbedded => "echo-embedded",
        EchoDaemon => "echo-daemon",
        EchoHeld => "echo-held",
        EchoDaemonIdle => "echo-daemon-idle",
        EchoHeldIdle => "echo-held-idle",
        PlainKeyEcho => "plain-key-echo",
        PageCacheSeries => "page-cache-series",
        BoundNewSession => "bound-new-session",
    }
}

impl Floor {
    #[must_use]
    pub const fn parts(self) -> &'static [Floor] {
        match self {
            Floor::EchoEmbedded => &[Floor::PtyEcho, Floor::WakeHot],
            Floor::EchoDaemon => &[Floor::UdsOneWay, Floor::UdsOneWay, Floor::PtyEcho],
            Floor::EchoHeld => &[
                Floor::UdsOneWay,
                Floor::UdsOneWay,
                Floor::UdsOneWay,
                Floor::UdsOneWay,
                Floor::PtyEcho,
            ],
            Floor::EchoDaemonIdle => &[
                Floor::UdsSend,
                Floor::WakeIdle,
                Floor::UdsSend,
                Floor::WakeIdle,
                Floor::PtyEcho,
            ],
            Floor::EchoHeldIdle => &[
                Floor::UdsSend,
                Floor::WakeIdle,
                Floor::UdsSend,
                Floor::WakeIdle,
                Floor::UdsSend,
                Floor::WakeIdle,
                Floor::UdsSend,
                Floor::WakeIdle,
                Floor::PtyEcho,
            ],
            _ => &[],
        }
    }

    #[must_use]
    pub const fn source(self) -> FloorSource {
        match self {
            Floor::Present | Floor::ParkedWindow => FloorSource::Mado,
            Floor::PlainKeyEcho | Floor::PageCacheSeries | Floor::BoundNewSession => {
                FloorSource::SameRunControl
            }
            Floor::EchoEmbedded
            | Floor::EchoDaemon
            | Floor::EchoHeld
            | Floor::EchoDaemonIdle
            | Floor::EchoHeldIdle => FloorSource::Composite,
            Floor::ScreenParse => FloorSource::Probe,
            _ => FloorSource::Primitive,
        }
    }
}

tear_types::closed_vocabulary! {
    FloorSource {
        Primitive => "primitive",
        Composite => "composite",
        SameRunControl => "same-run-control",
        Probe => "probe",
        Mado => "mado",
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Budget {
    Floor {
        floor: Floor,
        stat: Stat,
        k: f64,
    },
    Count {
        max: u64,
    },
    Bytes {
        max: u64,
    },
    Exactly {
        n: u64,
    },
    NotApplicable {
        why: &'static str,
    },
    Pending {
        rung: Rung,
        today: &'static str,
        receipt: &'static str,
    },
}

impl Budget {
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Budget::Floor { .. } => "floor",
            Budget::Count { .. } => "count",
            Budget::Bytes { .. } => "bytes",
            Budget::Exactly { .. } => "exactly",
            Budget::NotApplicable { .. } => "not-applicable",
            Budget::Pending { .. } => "pending",
        }
    }

    #[must_use]
    pub const fn is_budgeted(self) -> bool {
        matches!(
            self,
            Budget::Floor { .. }
                | Budget::Count { .. }
                | Budget::Bytes { .. }
                | Budget::Exactly { .. }
        )
    }
}

#[must_use]
pub const fn pending(rung: Rung, today: &'static str, receipt: &'static str) -> Budget {
    Budget::Pending {
        rung,
        today,
        receipt,
    }
}

#[must_use]
pub const fn na(why: &'static str) -> Budget {
    Budget::NotApplicable { why }
}

pub const MADO: Budget =
    na("a present-path cell: measured by mado's half of the matrix, in mado's bench (§6)");
pub const IN_PROCESS: Budget = na("in process: a call, no wire and no socket");
pub const NO_JOURNAL: Budget = na("no holder and no journal in this case");
pub const ELSEWHERE: Budget = na("this case does not exercise the metric; its own case owns it");

macro_rules! metrics {
    ($($field:ident => $variant:ident, $wire:literal, $describe:literal;)+) => {
        tear_types::closed_vocabulary! {
            Metric { $($variant => $wire),+ }
        }

        impl Metric {
            #[must_use]
            pub const fn describe(self) -> &'static str {
                match self {
                    $(Metric::$variant => $describe),+
                }
            }
        }

        #[derive(Copy, Clone, Debug, PartialEq)]
        pub struct Budgets {
            $(pub $field: Budget),+
        }

        impl Budgets {
            #[must_use]
            pub const fn get(&self, metric: Metric) -> Budget {
                match metric {
                    $(Metric::$variant => self.$field),+
                }
            }
        }
    };
}

metrics! {
    echo_warm => EchoWarm, "echo-warm", "warm echo, keys back to back, p50";
    echo_gap => EchoGap, "echo-gap", "echo after a 2 ms gap";
    echo_pause => EchoPause, "echo-pause", "first echo after a 1.5 s pause, full chain";
    echo_pause_hop => EchoPauseHop, "echo-pause-hop", "first echo after a 1.5 s pause, holder hop only";
    round_trip => RoundTrip, "round-trip", "a control round trip, p50 (get_pane, connect and subscribe, one MCP call)";
    round_trip_tail => RoundTripTail, "round-trip-tail", "a control round trip, p99";
    key => Key, "key", "a mado-shaped key (DECCKM read, then SendKeys) to its echo, p50";
    throughput => Throughput, "throughput", "ns per MiB delivered under a saturating flood (lower is faster)";
    attach => Attach, "attach", "attach to the first live key or frame";
    startup => Startup, "startup", "daemon start to an answering socket";
    new_session => NewSession, "new-session", "a NewSession round trip";
    restart => Restart, "restart", "restart or re-adoption to accepting and live";
    spikes => Spikes, "spikes", "echoes over 3 ms in a 15 s open-loop typing series at 100 keys/s";
    paste => Paste, "paste", "UI-blocked time for a paste, and its arrival rate";
    observer => Observer, "observer", "an observer read (MCP pane_snapshot_text) at depth";
    resize => Resize, "resize", "per drag step: resizes, reflows, SIGWINCH";
    memory => Memory, "memory", "bytes held: RSS per flood, memory behind a stalled viewer";
    keyframe => Keyframe, "keyframe", "bytes moved to attach to or read a screen";
    rpcs => Rpcs, "rpcs", "RPCs per key or per idle second, or blocking tear calls on the UI thread";
    wire_bytes => WireBytes, "wire-bytes", "wire bytes per KiB of output, or per typed key";
    frames => Frames, "frames", "frames per MiB under a saturating flood";
    flushes => Flushes, "flushes", "flushes or audit writes on the byte or key path";
    parses => Parses, "parses", "VT parses per output byte";
    encodes => Encodes, "encodes", "encodes per chunk, or daemon CPU per extra subscriber";
    allocations => Allocations, "allocations", "allocations per KiB parsed";
    idle_ticks => IdleTicks, "idle-ticks", "GUI loop ticks per idle second";
    idle_wakeups => IdleWakeups, "idle-wakeups", "context switches or timer wakeups per idle second";
    paints => Paints, "paints", "painted frames that repeat content";
    present => Present, "present", "a byte's arrival in the window to the end of the frame that shows it, p50 (mado's frame_perf byte_to_present)";
    loss => Loss, "loss", "keys or bytes lost, or delivered twice";
    answers => Answers, "answers", "answers per terminal query";
    replays => Replays, "replays", "history replays per attach";
    modes => Modes, "modes", "ModeSet fields that differ from the authority's after a replay";
    authorities => Authorities, "authorities", "daemons a held pane still takes input through after a second daemon on the same store adopts it: exactly one, so none and two are both red";
}

tear_types::closed_vocabulary! {
    Control {
        BandBackground => "band-background",
        JournalWriteAhead => "journal-write-ahead",
        CursorKeysViaRpc => "cursor-keys-via-rpc",
        PaneFatePoll => "pane-fate-poll",
        ArrayEncoder => "array-encoder",
        UdsBufferOsDefault => "uds-buffer-os-default",
        TwoWriteFraming => "two-write-framing",
        MuteSink => "mute-sink",
        LeaseOff => "lease-off",
        SnapshotHistoryAll => "snapshot-history-all",
        UnboundedSubscriberQueue => "unbounded-subscriber-queue",
        AllocatingRow => "allocating-row",
        AuditEveryKey => "audit-every-key",
        ResponseSizeUnchecked => "response-size-unchecked",
        LegacyReplay => "legacy-replay",
        RawSubscribe => "raw-subscribe",
        UnchunkedInput => "unchunked-input",
        OldSplitter => "old-splitter",
        StoreLeaseOff => "store-lease-off",
        WakeOff => "wake-off",
    }
}

tear_types::closed_vocabulary! {
    ControlKind {
        KeptConfiguration => "kept-configuration",
        Fault => "fault",
        BenchOnly => "bench-only",
    }
}

impl Control {
    #[must_use]
    pub const fn kind(self) -> ControlKind {
        match self {
            Control::BandBackground
            | Control::JournalWriteAhead
            | Control::CursorKeysViaRpc
            | Control::PaneFatePoll
            | Control::UdsBufferOsDefault
            | Control::TwoWriteFraming => ControlKind::KeptConfiguration,
            Control::ArrayEncoder | Control::OldSplitter => ControlKind::BenchOnly,
            Control::MuteSink
            | Control::LeaseOff
            | Control::SnapshotHistoryAll
            | Control::UnboundedSubscriberQueue
            | Control::AllocatingRow
            | Control::AuditEveryKey
            | Control::ResponseSizeUnchecked
            | Control::LegacyReplay
            | Control::RawSubscribe
            | Control::UnchunkedInput
            | Control::StoreLeaseOff
            | Control::WakeOff => ControlKind::Fault,
        }
    }

    #[must_use]
    pub const fn rung(self) -> Rung {
        match self {
            Control::BandBackground => Rung::R2,
            Control::JournalWriteAhead => Rung::R17,
            Control::CursorKeysViaRpc => Rung::R5,
            Control::PaneFatePoll | Control::WakeOff => Rung::R10,
            Control::ArrayEncoder => Rung::R6,
            Control::UdsBufferOsDefault | Control::TwoWriteFraming => Rung::R8,
            Control::MuteSink | Control::StoreLeaseOff => Rung::R4,
            Control::LeaseOff => Rung::R42,
            Control::SnapshotHistoryAll => Rung::R9,
            Control::UnboundedSubscriberQueue => Rung::R22,
            Control::AllocatingRow => Rung::R24,
            Control::AuditEveryKey => Rung::R1,
            Control::ResponseSizeUnchecked
            | Control::LegacyReplay
            | Control::RawSubscribe
            | Control::UnchunkedInput => Rung::R3,
            Control::OldSplitter => Rung::R7,
        }
    }

    #[must_use]
    pub const fn today(self) -> &'static str {
        match self {
            Control::BandBackground => {
                "launchd Adaptive puts the daemon and holders at PRI 4; the bench reproduces it with PRIO_DARWIN_BG, as taskpolicy -b does"
            }
            Control::JournalWriteAhead => {
                "sessions.journal.fsync_interval_ms: 0 flushes on every PTY read (S journal.rs:149-158)"
            }
            Control::CursorKeysViaRpc => {
                "the mado-shaped key: pane_cursor_keys_mode over RPC, then SendKeys (S mado gui_tear_attach.rs:891-899)"
            }
            Control::PaneFatePoll => {
                "mado's tear.pane_fate: poll, one get_pane per idle tick as before R10 (S mado gui_tear_attach.rs:1372 at ec50bfb)"
            }
            Control::ArrayEncoder => "byte payloads as CBOR integer arrays (S tear-types wire.rs)",
            Control::UdsBufferOsDefault => "8 KiB AF_UNIX buffers (net.local.stream.sendspace)",
            Control::TwoWriteFraming => "a length write, then a body write (S wire.rs:523-533)",
            Control::MuteSink => {
                "a failed sink is dropped while its socket stays open, as every holder before R4 does (S holder.rs:181-183 at v0.1.34)"
            }
            Control::LeaseOff => {
                "no lease: a pane no window shows answers nothing (S pane_grid.rs:1615)"
            }
            Control::SnapshotHistoryAll => {
                "every snapshot carries all history (S pane_grid.rs:1634): today's only behaviour"
            }
            Control::UnboundedSubscriberQueue => {
                "an unbounded mpsc per subscriber (S inproc.rs:346): today's only behaviour"
            }
            Control::AllocatingRow => "513 allocations per KiB of yes (G): today's only behaviour",
            Control::AuditEveryKey => {
                "no audit write on the key path (S daemon audit.rs:65-78); the fault writes one per SendKeys"
            }
            Control::ResponseSizeUnchecked => {
                "the daemon clones and encodes a response of any size and writes a frame its peer refuses to read (S tear-daemon dispatch, tear-types wire.rs read_frame): today's only behaviour"
            }
            Control::LegacyReplay => {
                "tear-client reuses a connection left mid-frame and replays a failed read whatever failed (S tear-client lib.rs:658-681): today's only behaviour; its run arms response-size-unchecked in the daemon too, as an old daemon behaves"
            }
            Control::RawSubscribe => {
                "a subscription dials a bare connection, so a daemon that requires a token refuses it (S tear-client lib.rs:525-528): today's only behaviour"
            }
            Control::UnchunkedInput => {
                "SendKeys carries any input in one frame, so input past ~8.1 MiB crosses the 16 MiB cap and is lost (W): today's only behaviour"
            }
            Control::OldSplitter => {
                "the pre-R7 APC scanner hands vte each read's tail, and vte 0.15 drops what follows a completed partial character: 3 of 3 corpora lose one (G)"
            }
            Control::StoreLeaseOff => {
                "no store lease: both daemons keep the pane, and either one's mute check re-attaches it from the other (SESSION-DURABILITY §7)"
            }
            Control::WakeOff => {
                "no wake from output: a byte waits for mado's next Capped(60) tick, 0–16.7 ms (S mado gui_tear_attach.rs at de5949e); the fault hands mado's attach a no-op waker"
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Red {
    pub control: Control,
    pub metrics: &'static [Metric],
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Row {
    pub receipt: &'static str,
    pub budgets: Budgets,
    pub controls: &'static [Red],
}

#[macro_export]
macro_rules! bench_matrix {
    ($($case:pat => $row:expr),+ $(,)?) => {
        #[must_use]
        pub const fn row(case: $crate::matrix::Case) -> $crate::matrix::Row {
            match case {
                $($case => $row),+
            }
        }
    };
}

#[macro_export]
macro_rules! product_rows {
    ($vis:vis fn $name:ident($ty:ty) -> $out:ty { $($pat:pat => $val:expr),+ $(,)? }) => {
        #[must_use]
        $vis const fn $name(variant: $ty) -> $out {
            match variant {
                $($pat => $val),+
            }
        }
    };
}

pub use crate::table::row;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub case: Case,
    pub metric: Metric,
}

impl Cell {
    #[must_use]
    pub const fn new(case: Case, metric: Metric) -> Self {
        Self { case, metric }
    }

    #[must_use]
    pub const fn budget(self) -> Budget {
        row(self.case).budgets.get(self.metric)
    }

    #[must_use]
    pub fn name(self) -> String {
        format!("{}/{}", self.case.name(), self.metric.name())
    }
}

#[must_use]
pub fn cells() -> Vec<Cell> {
    Case::ALL
        .iter()
        .flat_map(|c| Metric::ALL.iter().map(move |m| Cell::new(*c, *m)))
        .collect()
}

pub const WINDOW_CELLS: &[Cell] = &[
    Cell::new(Case::C3, Metric::Rpcs),
    Cell::new(Case::C3, Metric::Present),
];

#[must_use]
pub fn red_set(control: Control) -> Vec<Cell> {
    let mut out = Vec::new();
    for case in Case::ALL {
        for red in row(case).controls {
            if red.control == control {
                out.extend(red.metrics.iter().map(|m| Cell::new(case, *m)));
            }
        }
    }
    out
}

pub(crate) const fn check_budget(b: Budget) {
    match b {
        Budget::Floor { floor, stat, k } => {
            assert!(
                k > 0.0 && k < 1.0e6,
                "a floor multiple must be positive and finite"
            );
            assert!(
                !matches!(stat, Stat::P99) || k >= 3.0,
                "a p99 budget needs a multiple of at least 3 (§6 noise)"
            );
            assert!(
                !matches!(floor.source(), FloorSource::Mado),
                "a mado floor cannot grade a tear cell"
            );
        }
        Budget::Count { .. } | Budget::Bytes { .. } | Budget::Exactly { .. } => {}
        Budget::NotApplicable { why } => {
            assert!(!why.is_empty(), "NotApplicable needs a reason");
        }
        Budget::Pending {
            rung,
            today,
            receipt,
        } => {
            assert!(
                !landed(rung),
                "a Pending cell names a landed rung: give it a real budget"
            );
            assert!(!today.is_empty(), "Pending needs today's value");
            assert!(!receipt.is_empty(), "Pending needs its receipt");
        }
    }
}

const fn check_matrix() {
    let mut case = 0;
    while case < Case::ALL.len() {
        let current = row(Case::ALL[case]);
        assert!(!current.receipt.is_empty(), "every row carries a receipt");
        let mut metric = 0;
        while metric < Metric::ALL.len() {
            check_budget(current.budgets.get(Metric::ALL[metric]));
            metric += 1;
        }
        let mut control = 0;
        while control < current.controls.len() {
            let red = current.controls[control];
            assert!(
                !red.metrics.is_empty(),
                "a control row reddens at least one metric"
            );
            let mut reddened = 0;
            while reddened < red.metrics.len() {
                assert!(
                    !matches!(
                        current.budgets.get(red.metrics[reddened]),
                        Budget::NotApplicable { .. }
                    ),
                    "a control cannot redden a NotApplicable cell"
                );
                reddened += 1;
            }
            control += 1;
        }
        case += 1;
    }
}

const _: () = check_matrix();

tear_types::closed_vocabulary! {
    Runtime { Embedded => "embedded", Daemon => "daemon" }
}

tear_types::closed_vocabulary! {
    TransportKind { Unix => "unix", Tcp => "tcp" }
}

tear_types::closed_vocabulary! {
    Band { Interactive => "interactive", Default => "default", Background => "background" }
}

tear_types::closed_vocabulary! {
    Answerer { Consumer => "consumer", Authority => "authority" }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum JournalSync {
    WriteAhead,
    GroupCommit { interval_ms: u64 },
    PageCache,
}

impl JournalSync {
    pub const PAGE_CACHE_INTERVAL_MS: u64 = 86_400_000;

    #[must_use]
    pub const fn fsync_interval_ms(self) -> u64 {
        match self {
            JournalSync::WriteAhead => 0,
            JournalSync::GroupCommit { interval_ms } => interval_ms,
            JournalSync::PageCache => Self::PAGE_CACHE_INTERVAL_MS,
        }
    }

    #[must_use]
    pub fn label(self) -> String {
        match self {
            JournalSync::WriteAhead => "write-ahead".into(),
            JournalSync::GroupCommit { interval_ms } => format!("group-commit-{interval_ms}ms"),
            JournalSync::PageCache => "page-cache".into(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DurabilityRow {
    pub config: SessionDurability,
    pub yaml: &'static str,
    pub label: &'static str,
    pub cases: &'static [Case],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HostRoleRow {
    pub answerer: Answerer,
    pub cells: &'static [Metric],
    pub cases: &'static [Case],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TransportRow {
    pub kind: TransportKind,
    pub cases: &'static [Case],
}

product_rows! {
    pub fn durability_row(Durability) -> DurabilityRow {
        Durability::ProcessBound => DurabilityRow {
            config: SessionDurability::ProcessBound,
            yaml: "process_bound",
            label: "bound",
            cases: &[Case::C2, Case::C3, Case::C5, Case::C7(Handover::Attach), Case::C8, Case::C9],
        },
        Durability::Held => DurabilityRow {
            config: SessionDurability::Held,
            yaml: "held",
            label: "held",
            cases: &[Case::C3, Case::C4, Case::C7(Handover::Readopt), Case::C8],
        },
    }
}

product_rows! {
    pub fn durability_of(SessionDurability) -> Durability {
        SessionDurability::ProcessBound => Durability::ProcessBound,
        SessionDurability::Held => Durability::Held,
    }
}

product_rows! {
    pub fn host_role_row(HostRole) -> HostRoleRow {
        HostRole::Relay => HostRoleRow {
            answerer: Answerer::Consumer,
            cells: &[Metric::Answers],
            cases: &[Case::C3, Case::C5],
        },
        HostRole::Host => HostRoleRow {
            answerer: Answerer::Authority,
            cells: &[Metric::Answers],
            cases: &[Case::C3, Case::C5],
        },
    }
}

product_rows! {
    pub fn transport_row(&Transport) -> TransportRow {
        Transport::Unix(_) => TransportRow {
            kind: TransportKind::Unix,
            cases: &[Case::C2, Case::C3, Case::C4, Case::C5, Case::C7(Handover::Attach), Case::C8, Case::C13],
        },
        Transport::Tcp(_) => TransportRow {
            kind: TransportKind::Tcp,
            cases: &[Case::C6(Remote::Tcp)],
        },
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Variant {
    pub runtime: Runtime,
    pub durability: Durability,
    pub transport: TransportKind,
    pub daemon_band: Band,
    pub client_band: Band,
    pub journal: JournalSync,
}

impl Variant {
    pub const EMBEDDED: Variant = Variant {
        runtime: Runtime::Embedded,
        durability: Durability::ProcessBound,
        transport: TransportKind::Unix,
        daemon_band: Band::Default,
        client_band: Band::Default,
        journal: JournalSync::GroupCommit { interval_ms: 1000 },
    };

    pub const BOUND: Variant = Variant {
        runtime: Runtime::Daemon,
        ..Variant::EMBEDDED
    };

    pub const HELD: Variant = Variant {
        durability: Durability::Held,
        ..Variant::BOUND
    };

    pub const TCP: Variant = Variant {
        transport: TransportKind::Tcp,
        ..Variant::BOUND
    };

    pub const PRESETS: &'static [(&'static str, Variant)] = &[
        ("embedded", Variant::EMBEDDED),
        ("bound", Variant::BOUND),
        ("held", Variant::HELD),
        (
            "held-page-cache",
            Variant {
                journal: JournalSync::PageCache,
                ..Variant::HELD
            },
        ),
        (
            "held-write-ahead",
            Variant {
                journal: JournalSync::WriteAhead,
                ..Variant::HELD
            },
        ),
        (
            "bound-bgd",
            Variant {
                daemon_band: Band::Background,
                ..Variant::BOUND
            },
        ),
        (
            "held-bgd",
            Variant {
                daemon_band: Band::Background,
                ..Variant::HELD
            },
        ),
        (
            "held-bg",
            Variant {
                daemon_band: Band::Background,
                client_band: Band::Background,
                ..Variant::HELD
            },
        ),
        ("tcp", Variant::TCP),
    ];

    #[must_use]
    pub fn preset(name: &str) -> Option<Variant> {
        Self::PRESETS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
    }

    #[must_use]
    pub fn label(self) -> String {
        Self::PRESETS.iter().find(|(_, v)| *v == self).map_or_else(
            || {
                format!(
                    "{}-{}-{}-d{}-c{}-{}",
                    self.runtime.name(),
                    durability_row(self.durability).label,
                    self.transport.name(),
                    self.daemon_band.name(),
                    self.client_band.name(),
                    self.journal.label()
                )
            },
            |(n, _)| (*n).to_string(),
        )
    }

    #[must_use]
    pub fn config_yaml(self) -> String {
        let row = durability_row(self.durability);
        match row.config {
            SessionDurability::Held => format!(
                "sessions:\n  durability: {}\n  journal:\n    fsync_interval_ms: {}\n",
                row.yaml,
                self.journal.fsync_interval_ms()
            ),
            SessionDurability::ProcessBound => format!("sessions:\n  durability: {}\n", row.yaml),
        }
    }
}

#[cfg(feature = "bench-probes")]
product_rows! {
    pub fn control_of_fault(tear_types::probes::Fault) -> Control {
        tear_types::probes::Fault::MuteSink => Control::MuteSink,
        tear_types::probes::Fault::SnapshotHistoryAll => Control::SnapshotHistoryAll,
        tear_types::probes::Fault::UnboundedSubscriberQueue => Control::UnboundedSubscriberQueue,
        tear_types::probes::Fault::AuditEveryKey => Control::AuditEveryKey,
        tear_types::probes::Fault::ResponseSizeUnchecked => Control::ResponseSizeUnchecked,
        tear_types::probes::Fault::LegacyReplay => Control::LegacyReplay,
        tear_types::probes::Fault::RawSubscribe => Control::RawSubscribe,
        tear_types::probes::Fault::UnchunkedInput => Control::UnchunkedInput,
        tear_types::probes::Fault::StoreLeaseOff => Control::StoreLeaseOff,
        tear_types::probes::Fault::WakeOff => Control::WakeOff,
    }
}
