use std::str;

use tear_types::graphics::GRAPHIC_PAYLOAD_MAX;

pub const HOLD_MAX: usize = 4096;

const ESC: u8 = 0x1b;
const APC_RETAIN: usize = 64 * 1024;

#[derive(Debug)]
pub struct Chunk<'a> {
    bytes: &'a [u8],
}

impl<'a> Chunk<'a> {
    #[must_use]
    pub fn into_bytes(self) -> &'a [u8] {
        self.bytes
    }
}

#[derive(Debug)]
pub enum Segment<'a> {
    Text(Chunk<'a>),
    Apc { payload: &'a [u8], cut: bool },
}

#[derive(Default)]
pub struct Parser {
    vte: vte::Parser,
}

impl Parser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance<P: vte::Perform>(&mut self, performer: &mut P, chunk: Chunk<'_>) {
        self.vte.advance(performer, chunk.into_bytes());
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Vt {
    Ground,
    Escape,
    EscInter,
    Csi,
    DcsEntry,
    DcsParam,
    DcsInter,
    DcsIgnore,
    DcsPass,
    Osc,
    Str,
}

const fn step(s: Vt, b: u8) -> Vt {
    match (s, b) {
        (_, ESC) => Vt::Escape,
        (Vt::Escape, 0x20..=0x2f) => Vt::EscInter,
        (Vt::Escape, b'P') => Vt::DcsEntry,
        (Vt::Escape, b'X' | b'^' | b'_') => Vt::Str,
        (Vt::Escape, b'[') => Vt::Csi,
        (Vt::Escape, b']') => Vt::Osc,
        (Vt::Ground, _)
        | (_, 0x18 | 0x1a)
        | (Vt::Escape | Vt::EscInter, 0x30..=0x7e)
        | (Vt::Csi, 0x40..=0x7e)
        | (Vt::DcsPass, 0x9c)
        | (Vt::Osc, 0x07) => Vt::Ground,
        (Vt::DcsEntry | Vt::DcsParam | Vt::DcsInter, 0x40..=0x7e) => Vt::DcsPass,
        (Vt::DcsEntry | Vt::DcsParam, 0x20..=0x2f) => Vt::DcsInter,
        (Vt::DcsEntry, 0x30..=0x3f) => Vt::DcsParam,
        (Vt::DcsParam, 0x3c..=0x3f) | (Vt::DcsInter, 0x30..=0x3f) => Vt::DcsIgnore,
        (s, _) => s,
    }
}

fn first_of(bytes: &[u8], three: [u8; 3], fourth: Option<u8>) -> Option<usize> {
    let [a, b, c] = three;
    let found = memchr::memchr3(a, b, c, bytes);
    let Some(d) = fourth else { return found };
    let end = found.unwrap_or(bytes.len());
    memchr::memchr(d, &bytes[..end]).or(found)
}

fn next_transition(s: Vt, bytes: &[u8]) -> Option<usize> {
    match s {
        Vt::Ground => memchr::memchr(ESC, bytes),
        Vt::Osc => first_of(bytes, [ESC, 0x07, 0x18], Some(0x1a)),
        Vt::Str | Vt::DcsIgnore => first_of(bytes, [ESC, 0x18, 0x1a], None),
        Vt::DcsPass => first_of(bytes, [ESC, 0x18, 0x1a], Some(0x9c)),
        _ => (!bytes.is_empty()).then_some(0),
    }
}

fn utf8_tail(bytes: &[u8]) -> usize {
    let n = bytes.len();
    let Some(lead) = (1..=n.min(3))
        .map(|back| n - back)
        .find(|&i| bytes[i] & 0xc0 != 0x80)
    else {
        return 0;
    };
    match str::from_utf8(&bytes[lead..]) {
        Err(e) if e.valid_up_to() == 0 && e.error_len().is_none() => n - lead,
        _ => 0,
    }
}

struct Scan {
    end: Vt,
    ground_from: usize,
    departure: Option<(usize, usize)>,
}

fn forward(start: Vt, run: &[u8], from: usize, to: usize) -> (Vt, Option<usize>) {
    let mut s = start;
    let mut i = from;
    let mut entered = None;
    while let Some(off) = next_transition(s, &run[i..to]) {
        let at = i + off;
        let next = step(s, run[at]);
        if next == Vt::Ground && s != Vt::Ground {
            entered = Some(at + 1);
        }
        s = next;
        i = at + 1;
    }
    (s, entered)
}

fn scan(start: Vt, run: &[u8], hold_max: usize) -> Scan {
    let Some(last) = memchr::memrchr(ESC, run) else {
        let (end, entered) = forward(start, run, 0, run.len());
        return Scan {
            end,
            ground_from: entered.unwrap_or(0),
            departure: None,
        };
    };
    let (end, entered) = forward(Vt::Escape, run, last + 1, run.len());
    if end == Vt::Ground {
        return Scan {
            end,
            ground_from: entered.unwrap_or(last + 1),
            departure: None,
        };
    }
    let mut esc = last;
    let departure = loop {
        if run.len() - esc > hold_max {
            break None;
        }
        if let Some(prev) = memchr::memrchr(ESC, &run[..esc]) {
            let (before, entered) = forward(Vt::Escape, run, prev + 1, esc);
            if before == Vt::Ground {
                break Some((entered.unwrap_or(prev + 1), esc));
            }
            esc = prev;
        } else {
            let (before, entered) = forward(start, run, 0, esc);
            break (before == Vt::Ground).then(|| (entered.unwrap_or(0), esc));
        }
    };
    Scan {
        end,
        ground_from: 0,
        departure,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Lift {
    Text,
    Esc,
    Apc,
    ApcEsc,
}

pub struct Feeder {
    lift: Lift,
    apc: Vec<u8>,
    apc_cut: bool,
    held: Vec<u8>,
    vt: Vt,
    ground_from: usize,
    hold_max: usize,
    apc_max: usize,
}

impl Default for Feeder {
    fn default() -> Self {
        Self::with_bounds(HOLD_MAX, GRAPHIC_PAYLOAD_MAX)
    }
}

impl Feeder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_bounds(hold_max: usize, apc_max: usize) -> Self {
        Self {
            lift: Lift::Text,
            apc: Vec::new(),
            apc_cut: false,
            held: Vec::new(),
            vt: Vt::Ground,
            ground_from: 0,
            hold_max: hold_max.max(4),
            apc_max,
        }
    }

    #[must_use]
    pub fn at_rest(&self) -> bool {
        self.lift == Lift::Text && self.held.is_empty() && self.vt == Vt::Ground
    }

    pub fn feed(&mut self, input: &[u8], mut sink: impl FnMut(Segment<'_>)) {
        let mut i = 0;
        while i < input.len() {
            match self.lift {
                Lift::Esc => {
                    self.lift = Lift::Text;
                    if input[i] == b'_' {
                        self.open_apc();
                        i += 1;
                    } else {
                        self.text(&[ESC], &mut sink);
                    }
                }
                Lift::ApcEsc => {
                    if input[i] == b'\\' {
                        self.close_apc(&mut sink);
                        i += 1;
                    } else {
                        self.apc.clear();
                        self.lift = Lift::Esc;
                    }
                }
                Lift::Apc => i = self.apc_run(input, i, &mut sink),
                Lift::Text => i = self.text_run(input, i, &mut sink),
            }
        }
    }

    fn open_apc(&mut self) {
        self.apc.clear();
        self.apc_cut = false;
        self.lift = Lift::Apc;
    }

    fn close_apc(&mut self, sink: &mut impl FnMut(Segment<'_>)) {
        sink(Segment::Apc {
            payload: &self.apc,
            cut: self.apc_cut,
        });
        self.apc.clear();
        self.apc.shrink_to(APC_RETAIN);
        self.lift = Lift::Text;
    }

    fn apc_push(&mut self, bytes: &[u8]) {
        let room = self.apc_max.saturating_sub(self.apc.len());
        if bytes.len() > room {
            self.apc.extend_from_slice(&bytes[..room]);
            self.apc_cut = true;
        } else {
            self.apc.extend_from_slice(bytes);
        }
    }

    fn apc_run(&mut self, input: &[u8], i: usize, sink: &mut impl FnMut(Segment<'_>)) -> usize {
        let Some(off) = memchr::memchr3(ESC, 0x07, 0x9c, &input[i..]) else {
            self.apc_push(&input[i..]);
            return input.len();
        };
        let at = i + off;
        self.apc_push(&input[i..at]);
        if input[at] != ESC {
            self.close_apc(sink);
            return at + 1;
        }
        match input.get(at + 1) {
            None => {
                self.lift = Lift::ApcEsc;
                input.len()
            }
            Some(b'\\') => {
                self.close_apc(sink);
                at + 2
            }
            Some(_) => {
                self.apc.clear();
                self.lift = Lift::Text;
                at
            }
        }
    }

    fn text_run(&mut self, input: &[u8], i: usize, sink: &mut impl FnMut(Segment<'_>)) -> usize {
        let rest = &input[i..];
        if let Some(at) = memchr::memmem::find(rest, b"\x1b_") {
            self.text(&rest[..at], sink);
            self.open_apc();
            return i + at + 2;
        }
        if rest.last() == Some(&ESC) {
            self.text(&rest[..rest.len() - 1], sink);
            self.lift = Lift::Esc;
        } else {
            self.text(rest, sink);
        }
        input.len()
    }

    fn text(&mut self, run: &[u8], sink: &mut impl FnMut(Segment<'_>)) {
        let run = self.resolve(run, sink);
        if run.is_empty() {
            return;
        }
        let s = scan(self.vt, run, self.hold_max);
        let mut cut = match (s.end, s.departure) {
            (Vt::Ground, _) => run.len() - utf8_tail(&run[s.ground_from..]),
            (_, Some((ground_from, esc))) => {
                let from = esc - utf8_tail(&run[ground_from..esc]);
                if run.len() - from <= self.hold_max {
                    from
                } else {
                    run.len()
                }
            }
            (_, None) => run.len(),
        };
        if cut == run.len() && s.end != Vt::Ground && run[cut - 1] == ESC {
            cut -= 1;
        }
        if cut > 0 {
            sink(Segment::Text(Chunk { bytes: &run[..cut] }));
        }
        self.held.extend_from_slice(&run[cut..]);
        self.vt = s.end;
        self.ground_from = 0;
    }

    fn resolve<'r>(&mut self, run: &'r [u8], sink: &mut impl FnMut(Segment<'_>)) -> &'r [u8] {
        if self.held.is_empty() {
            return run;
        }
        for (k, &b) in run.iter().enumerate() {
            if self.vt == Vt::Ground && b & 0xc0 != 0x80 && b != ESC {
                sink(Segment::Text(Chunk { bytes: &self.held }));
                self.held.clear();
                self.ground_from = 0;
                return &run[k..];
            }
            self.held.push(b);
            let next = step(self.vt, b);
            if next == Vt::Ground && self.vt != Vt::Ground {
                self.ground_from = self.held.len();
            }
            self.vt = next;
            if self.vt == Vt::Ground && utf8_tail(&self.held[self.ground_from..]) == 0 {
                sink(Segment::Text(Chunk { bytes: &self.held }));
                self.held.clear();
                self.ground_from = 0;
                return &run[k + 1..];
            }
            if self.held.len() > self.hold_max {
                let keep = usize::from(b == ESC);
                let emit = self.held.len() - keep;
                sink(Segment::Text(Chunk {
                    bytes: &self.held[..emit],
                }));
                self.held.drain(..emit);
                self.ground_from = 0;
                if self.held.is_empty() {
                    return &run[k + 1..];
                }
            }
        }
        &[]
    }
}

#[derive(Default)]
pub struct Stream {
    feeder: Feeder,
    parser: Parser,
}

impl Stream {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn at_rest(&self) -> bool {
        self.feeder.at_rest()
    }

    pub fn feed<P: vte::Perform>(
        &mut self,
        performer: &mut P,
        bytes: &[u8],
        mut on_apc: impl FnMut(&mut P, &[u8], bool),
    ) {
        let Self { feeder, parser } = self;
        feeder.feed(bytes, |segment| match segment {
            Segment::Text(chunk) => parser.advance(performer, chunk),
            Segment::Apc { payload, cut } => on_apc(performer, payload, cut),
        });
    }
}

#[cfg(any(test, feature = "bench-probes"))]
pub(crate) mod legacy {
    use super::{Chunk, GRAPHIC_PAYLOAD_MAX, Stream};

    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    enum State {
        #[default]
        Idle,
        Escape,
        Inside,
        InsideEscape,
    }

    #[derive(Debug, Default)]
    pub(crate) struct Splitter {
        state: State,
        buf: Vec<u8>,
        cut: bool,
    }

    impl Splitter {
        fn split(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<(Vec<u8>, bool)>) {
            let mut passthrough = Vec::with_capacity(bytes.len());
            let mut done = Vec::new();
            for &b in bytes {
                match self.state {
                    State::Idle => {
                        if b == 0x1b {
                            self.state = State::Escape;
                        } else {
                            passthrough.push(b);
                        }
                    }
                    State::Escape => {
                        if b == b'_' {
                            self.state = State::Inside;
                            self.buf.clear();
                            self.cut = false;
                        } else {
                            passthrough.push(0x1b);
                            if b == 0x1b {
                                self.state = State::Escape;
                            } else {
                                passthrough.push(b);
                                self.state = State::Idle;
                            }
                        }
                    }
                    State::Inside => match b {
                        0x1b => self.state = State::InsideEscape,
                        0x07 => {
                            done.push((std::mem::take(&mut self.buf), self.cut));
                            self.state = State::Idle;
                        }
                        _ => {
                            if self.buf.len() < GRAPHIC_PAYLOAD_MAX {
                                self.buf.push(b);
                            } else {
                                self.cut = true;
                            }
                        }
                    },
                    State::InsideEscape => {
                        if b == b'\\' {
                            done.push((std::mem::take(&mut self.buf), self.cut));
                            self.state = State::Idle;
                        } else {
                            if self.buf.len() < GRAPHIC_PAYLOAD_MAX {
                                self.buf.push(0x1b);
                                self.buf.push(b);
                            } else {
                                self.cut = true;
                            }
                            self.state = State::Inside;
                        }
                    }
                }
            }
            (passthrough, done)
        }

        pub(crate) fn feed<P: vte::Perform>(
            &mut self,
            stream: &mut Stream,
            performer: &mut P,
            bytes: &[u8],
            mut apc: impl FnMut(&mut P, &[u8], bool),
        ) {
            let (passthrough, done) = self.split(bytes);
            stream.parser.advance(
                performer,
                Chunk {
                    bytes: &passthrough,
                },
            );
            for (payload, cut) in done {
                apc(performer, &payload, cut);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Default, PartialEq)]
    struct Trace {
        printed: String,
        events: Vec<String>,
    }

    impl vte::Perform for Trace {
        fn print(&mut self, c: char) {
            self.printed.push(c);
            match self.events.last_mut() {
                Some(e) if e.starts_with("print ") => e.push(c),
                _ => self.events.push(format!("print {c}")),
            }
        }
        fn execute(&mut self, b: u8) {
            self.events.push(format!("x{b:02x}"));
        }
        fn csi_dispatch(&mut self, p: &vte::Params, i: &[u8], ig: bool, c: char) {
            self.events.push(format!("csi {p:?} {i:?} {ig} {c}"));
        }
        fn esc_dispatch(&mut self, i: &[u8], ig: bool, b: u8) {
            self.events.push(format!("esc {i:?} {ig} {b:02x}"));
        }
        fn osc_dispatch(&mut self, p: &[&[u8]], bell: bool) {
            self.events.push(format!("osc {p:?} {bell}"));
        }
        fn hook(&mut self, p: &vte::Params, i: &[u8], ig: bool, c: char) {
            self.events.push(format!("hook {p:?} {i:?} {ig} {c}"));
        }
        fn put(&mut self, b: u8) {
            self.events.push(format!("put {b:02x}"));
        }
        fn unhook(&mut self) {
            self.events.push("unhook".into());
        }
    }

    fn sorted_cuts(len: usize, cuts: &[prop::sample::Index]) -> Vec<usize> {
        let mut out: Vec<usize> = cuts.iter().map(|c| c.index(len + 1)).collect();
        out.sort_unstable();
        out
    }

    fn reads<'a>(input: &'a [u8], cuts: &[usize]) -> Vec<&'a [u8]> {
        let mut out = Vec::new();
        let mut from = 0;
        for &c in cuts {
            let c = c.clamp(from, input.len());
            out.push(&input[from..c]);
            from = c;
        }
        out.push(&input[from..]);
        out
    }

    fn traced(feeder: &mut Feeder, input: &[u8], cuts: &[usize]) -> Trace {
        let mut p = Parser::new();
        let mut t = Trace::default();
        for read in reads(input, cuts) {
            feeder.feed(read, |seg| match seg {
                Segment::Text(c) => p.advance(&mut t, c),
                Segment::Apc { payload, cut } => t
                    .events
                    .push(format!("apc {} {cut}", String::from_utf8_lossy(payload))),
            });
        }
        t
    }

    fn texts(feeder: &mut Feeder, input: &[u8], cuts: &[usize]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for read in reads(input, cuts) {
            feeder.feed(read, |seg| {
                if let Segment::Text(c) = seg {
                    out.push(c.into_bytes().to_vec());
                }
            });
        }
        out
    }

    fn rests_after(input: &[u8]) -> bool {
        let mut f = Feeder::new();
        f.feed(input, |_| {});
        f.at_rest()
    }

    #[test]
    fn a_character_split_anywhere_prints_once_and_loses_nothing() {
        let s = "aã ✓ ñoño é.… 𝄞!".as_bytes();
        for cut in 0..=s.len() {
            assert_eq!(
                traced(&mut Feeder::new(), s, &[cut]).printed,
                "aã ✓ ñoño é.… 𝄞!",
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn the_old_splitter_loses_a_character_at_a_read_boundary() {
        let s = "aã ✓".as_bytes();
        let mut legacy = legacy::Splitter::default();
        let mut stream = Stream::new();
        let mut t = Trace::default();
        for read in reads(s, &[2]) {
            legacy.feed(&mut stream, &mut t, read, |_, _, _| {});
        }
        assert_eq!(t.printed, "aã✓");
    }

    #[test]
    fn no_text_chunk_ends_inside_a_character_or_on_a_lone_esc() {
        let s = "x\u{1F600}y\x1b[1mé\x1b".as_bytes();
        for cut in 0..=s.len() {
            for t in texts(&mut Feeder::new(), s, &[cut]) {
                assert_eq!(utf8_tail(&t), 0, "{t:?} at cut {cut}");
                assert_ne!(t.last(), Some(&ESC), "{t:?} at cut {cut}");
            }
        }
    }

    #[test]
    fn an_unfinished_escape_is_held_until_its_final_byte() {
        let s = b"ab\x1b[38;5;12mc";
        let got = texts(&mut Feeder::new(), s, &[5, 10, 12]);
        assert_eq!(
            got,
            vec![b"ab".to_vec(), b"\x1b[38;5;12m".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn an_escape_longer_than_the_bound_streams() {
        let s = b"\x1b]0;0123456789abc\x07z";
        let mut f = Feeder::with_bounds(8, GRAPHIC_PAYLOAD_MAX);
        let got = texts(&mut f, s, &[8, 14]);
        assert_eq!(
            got,
            vec![
                b"\x1b]0;01234".to_vec(),
                b"56789".to_vec(),
                b"abc\x07z".to_vec()
            ]
        );
        assert!(f.at_rest());
    }

    #[test]
    fn an_overflowing_escape_never_ends_a_chunk_on_its_terminating_esc() {
        let s = b"\x1b]0;0123456789\x1b\\z";
        for cut in 0..=s.len() {
            let mut f = Feeder::with_bounds(4, GRAPHIC_PAYLOAD_MAX);
            for t in texts(&mut f, s, &[cut]) {
                assert_ne!(t.last(), Some(&ESC), "{t:?} at cut {cut}");
            }
            assert!(f.at_rest(), "cut at {cut}");
        }
    }

    #[test]
    fn an_esc_run_past_the_bound_keeps_its_order_and_its_successor() {
        for stream in [
            &b"\x1b\x80\x1b_\x1b\x1b\x1b\x1b\x07\x1b\\\x18"[..],
            b"\xc2\x1b\x1b\x1b\x1b\x1b\x1b\x1b\x07\x1b\\\x18",
            b"\x1b\x1b\x1b\x1b\x1b_\x1b\x1b\x1b\x07\x1b\\\x18",
        ] {
            let mut f = Feeder::with_bounds(1, GRAPHIC_PAYLOAD_MAX);
            let chunks = texts(&mut f, stream, &[]);
            assert!(f.at_rest());
            for pair in chunks.windows(2) {
                if pair[0].last() == Some(&ESC) {
                    assert_eq!(pair[1].first(), Some(&ESC), "{chunks:?}");
                }
            }
            let reference = traced(
                &mut Feeder::with_bounds(1, GRAPHIC_PAYLOAD_MAX),
                stream,
                &[],
            );
            for cut in 0..=stream.len() {
                let mut f = Feeder::with_bounds(1, GRAPHIC_PAYLOAD_MAX);
                assert_eq!(traced(&mut f, stream, &[cut]), reference, "cut at {cut}");
                assert!(f.at_rest(), "cut at {cut}");
            }
        }
    }

    #[test]
    fn apcs_arrive_in_stream_order_whatever_the_split() {
        let s = b"A\x1b_Gone\x1b\\B\x1b_Gtwo\x07C\x1b_Gthree\x9cD";
        let mut f = Feeder::new();
        let reference = traced(&mut f, s, &[]);
        assert!(f.at_rest());
        assert_eq!(reference.printed, "ABCD");
        assert_eq!(
            reference.events,
            vec![
                "print A",
                "apc Gone false",
                "print B",
                "apc Gtwo false",
                "print C",
                "apc Gthree false",
                "print D"
            ]
        );
        for a in 0..=s.len() {
            for b in a..=s.len() {
                assert_eq!(
                    traced(&mut Feeder::new(), s, &[a, b]),
                    reference,
                    "cuts {a} {b}"
                );
            }
        }
    }

    #[test]
    fn an_esc_inside_an_apc_aborts_it_and_is_read_again() {
        let s = b"\x1b_Gabc\x1b[6nZ";
        let t = traced(&mut Feeder::new(), s, &[]);
        assert!(t.events.iter().all(|e| !e.starts_with("apc")));
        assert!(t.events.iter().any(|e| e.ends_with(" n")));
        assert_eq!(t.printed, "Z");
        for cut in 0..=s.len() {
            assert_eq!(traced(&mut Feeder::new(), s, &[cut]), t, "cut at {cut}");
        }
    }

    #[test]
    fn a_payload_past_the_cap_is_cut_and_never_read_as_text() {
        let s = b"\x1b_Gabcdefghij\x1b\\ok";
        let t = traced(&mut Feeder::with_bounds(HOLD_MAX, 4), s, &[9, 13]);
        assert_eq!(t.events, vec!["apc Gabc true", "print ok"]);
    }

    #[test]
    fn the_feeder_rests_where_vte_rests_on_every_escape_class() {
        let cases: &[(&[u8], bool)] = &[
            (b"\x1b[6n", true),
            (b"\x1b[?25", false),
            (b"\x1b[1;\n2", false),
            (b"\x1b[1;\x182", true),
            (b"\x1b]0;title\x07", true),
            (b"\x1b]0;title\x1b\\", true),
            (b"\x1b]0;title\x9c", false),
            (b"\x1bPq#0~~", false),
            (b"\x1bPq#0~~\x9c", true),
            (b"\x1bP1;2$\x9c", false),
            (b"\x1bP>|x\x1b\\", true),
            (b"\x1bX sos", false),
            (b"\x1b^pm\x1a", true),
            (b"\x1b(B", true),
            (b"\x1b(", false),
            (b"\x1b\n", false),
            (b"\x1b\n7", true),
            (b"\x1bc", true),
            (b"\x1b\x1b", false),
            (b"\x9b6n", true),
        ];
        for (s, rest) in cases {
            assert_eq!(rests_after(s), *rest, "{:?}", String::from_utf8_lossy(s));
        }
    }

    fn rich_byte() -> impl Strategy<Value = u8> {
        prop_oneof![
            3 => Just(ESC),
            2 => prop::sample::select(&b"[]P^X_\\;?0123456789mnqc\x07\x18\x1a\x9c\n"[..]),
            3 => 0x20u8..=0x7e,
            1 => 0x80u8..=0xff,
            1 => any::<u8>(),
        ]
    }

    fn vte_rests(bytes: &[u8]) -> bool {
        let mut p = vte::Parser::new();
        let mut t = Trace::default();
        p.advance(&mut t, bytes);
        let before = t.printed.chars().count();
        p.advance(&mut t, b"Z");
        t.printed.ends_with('Z') && t.printed.chars().count() > before
    }

    const SETTLE: &[u8] = b"\x07\x1b\\\x18";

    fn vte_once(bytes: &[u8]) -> Trace {
        let mut p = vte::Parser::new();
        let mut t = Trace::default();
        p.advance(&mut t, bytes);
        t
    }

    fn boundaries_inside_a_character(chunks: &[Vec<u8>]) -> Vec<usize> {
        let all = chunks.concat();
        let mut s = Vt::Ground;
        let mut ground_from = 0;
        let mut at = 0;
        let mut inside = Vec::new();
        for c in &chunks[..chunks.len().saturating_sub(1)] {
            for (i, &b) in c.iter().enumerate() {
                let next = step(s, b);
                if next == Vt::Ground && s != Vt::Ground {
                    ground_from = at + i + 1;
                }
                s = next;
            }
            at += c.len();
            let tail = if s == Vt::Ground {
                utf8_tail(&all[ground_from..at])
            } else {
                0
            };
            if tail > 0 {
                let ended = matches!(
                    str::from_utf8(&all[at - tail..=at]),
                    Err(e) if e.valid_up_to() == 0 && e.error_len() == Some(tail)
                );
                if !ended {
                    inside.push(at);
                }
            }
        }
        inside
    }

    #[test]
    fn a_run_of_lead_bytes_past_the_bound_never_leaves_vte_a_character_to_complete() {
        for hold in [4, HOLD_MAX] {
            let mut stream = vec![0xc3u8; hold + 1];
            stream.extend_from_slice(b"\xa3 \xe2\x82\xac!");
            let raw = vte_once(&stream);
            assert!(raw.printed.ends_with("ã €!"), "{:?}", raw.printed);
            for cuts in [&[][..], &[1], &[1, hold], &[hold + 1]] {
                let mut f = Feeder::with_bounds(hold, GRAPHIC_PAYLOAD_MAX);
                let chunks = texts(&mut f, &stream, cuts);
                assert_eq!(chunks.concat(), stream, "hold {hold}, cuts {cuts:?}");
                assert_eq!(
                    boundaries_inside_a_character(&chunks),
                    Vec::<usize>::new(),
                    "hold {hold}, cuts {cuts:?}"
                );
                let mut f = Feeder::with_bounds(hold, GRAPHIC_PAYLOAD_MAX);
                assert_eq!(
                    traced(&mut f, &stream, cuts),
                    raw,
                    "hold {hold}, cuts {cuts:?}"
                );
            }
        }
    }

    fn lead_heavy_byte() -> impl Strategy<Value = u8> {
        prop_oneof![
            2 => Just(ESC),
            2 => prop::sample::select(&b"[]P^X\\;?0123456789mnqc\x07\x18\x1a\x9c\n "[..]),
            3 => 0x80u8..=0xbf,
            3 => 0xc2u8..=0xf4,
            1 => any::<u8>(),
        ]
    }

    fn stream_token() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            4 => rich_byte().prop_map(|b| vec![b]),
            3 => lead_heavy_byte().prop_map(|b| vec![b]),
            3 => any::<char>().prop_map(|c| c.to_string().into_bytes()),
            1 => "[A-Za-z0-9+/=]{0,6}".prop_map(|p| {
                [b"\x1b_G".as_slice(), p.as_bytes(), b"\x1b\\"].concat()
            }),
        ]
    }

    #[test]
    fn an_apc_after_characters_ended_invalid_stands_where_it_stood_whatever_the_split() {
        let mut leads = vec![0xc2u8; 107];
        leads.extend_from_slice(b"\x1b_G\x1b\\");
        for stream in [
            &b"\xc3\xc3\xe2\x1b_Ga=T,f=100;QUJD\x1b\\\x82\xacX"[..],
            b"ab\xc3 \xe9\xe9\xe9\x1b_Ga=T,f=100;QUJD\x1b\\\xa9X",
            &leads,
        ] {
            let reference = traced(&mut Feeder::new(), stream, &[]);
            for cut in 0..=stream.len() {
                assert_eq!(
                    traced(&mut Feeder::new(), stream, &[cut]),
                    reference,
                    "cut at {cut}"
                );
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]

        #[test]
        fn the_feeder_rests_exactly_where_vte_rests(
            bytes in prop::collection::vec(rich_byte(), 0..64),
        ) {
            prop_assume!(!bytes.windows(2).any(|w| w == b"\x1b_"));
            let mut f = Feeder::new();
            f.feed(&bytes, |_| {});
            let holds_a_character = f.lift == Lift::Text && f.vt == Vt::Ground && !f.held.is_empty();
            if !holds_a_character {
                prop_assert_eq!(f.at_rest(), vte_rests(&bytes), "{:?}", bytes);
            }
        }

        #[test]
        fn chunked_advance_equals_one_advance_over_the_same_bytes(
            bytes in prop::collection::vec(rich_byte(), 0..256),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
            hold in prop_oneof![Just(HOLD_MAX), 1usize..16],
        ) {
            let mut bytes = bytes;
            bytes.extend_from_slice(SETTLE);
            let cuts = sorted_cuts(bytes.len(), &cuts);
            let chunks = texts(&mut Feeder::with_bounds(hold, GRAPHIC_PAYLOAD_MAX), &bytes, &cuts);
            let mut stepwise = (vte::Parser::new(), Trace::default());
            for (n, c) in chunks.iter().enumerate() {
                if c.last() == Some(&ESC) {
                    prop_assert_eq!(chunks.get(n + 1).and_then(|next| next.first()), Some(&ESC));
                }
                stepwise.0.advance(&mut stepwise.1, c);
            }
            let mut once = (vte::Parser::new(), Trace::default());
            once.0.advance(&mut once.1, &chunks.concat());
            prop_assert_eq!(stepwise.1, once.1);
        }

        #[test]
        fn any_bytes_split_anywhere_reach_the_parser_unchanged(
            tokens in prop::collection::vec(stream_token(), 0..128),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
            hold in prop_oneof![Just(HOLD_MAX), 1usize..16],
        ) {
            let mut bytes = tokens.concat();
            bytes.extend_from_slice(SETTLE);
            let cuts = sorted_cuts(bytes.len(), &cuts);
            let mut whole = Feeder::with_bounds(hold, GRAPHIC_PAYLOAD_MAX);
            let reference = traced(&mut whole, &bytes, &[]);
            let mut split = Feeder::with_bounds(hold, GRAPHIC_PAYLOAD_MAX);
            prop_assert_eq!(traced(&mut split, &bytes, &cuts), reference);
            prop_assert!(whole.at_rest() && split.at_rest());
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        #[test]
        fn text_reaches_the_parser_whole_and_in_order(
            bytes in prop::collection::vec(lead_heavy_byte(), 0..256),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
            hold in prop_oneof![Just(HOLD_MAX), 1usize..16],
        ) {
            prop_assume!(!bytes.windows(2).any(|w| w == b"\x1b_"));
            let cuts = sorted_cuts(bytes.len(), &cuts);
            let mut f = Feeder::with_bounds(hold, GRAPHIC_PAYLOAD_MAX);
            let got = texts(&mut f, &bytes, &cuts).concat();
            let held = f.held.len() + usize::from(f.lift == Lift::Esc);
            prop_assert_eq!(&got[..], &bytes[..bytes.len() - held]);
        }

        #[test]
        fn no_chunk_ends_where_the_next_byte_continues_its_character(
            bytes in prop::collection::vec(lead_heavy_byte(), 0..256),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
            hold in prop_oneof![Just(HOLD_MAX), 1usize..16],
        ) {
            let mut bytes = bytes;
            bytes.extend_from_slice(SETTLE);
            let cuts = sorted_cuts(bytes.len(), &cuts);
            let chunks = texts(&mut Feeder::with_bounds(hold, GRAPHIC_PAYLOAD_MAX), &bytes, &cuts);
            prop_assert_eq!(boundaries_inside_a_character(&chunks), Vec::<usize>::new());
        }

        #[test]
        fn lead_heavy_bytes_split_anywhere_read_as_one_raw_vte_advance(
            bytes in prop::collection::vec(lead_heavy_byte(), 0..256),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
            hold in prop_oneof![Just(HOLD_MAX), 1usize..16],
        ) {
            prop_assume!(!bytes.windows(2).any(|w| w == b"\x1b_"));
            let mut bytes = bytes;
            bytes.extend_from_slice(SETTLE);
            let cuts = sorted_cuts(bytes.len(), &cuts);
            let split = traced(&mut Feeder::with_bounds(hold, GRAPHIC_PAYLOAD_MAX), &bytes, &cuts);
            prop_assert_eq!(split, vte_once(&bytes));
        }

        #[test]
        fn chunked_advance_equals_one_advance_over_lead_heavy_bytes(
            bytes in prop::collection::vec(lead_heavy_byte(), 0..256),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
            hold in prop_oneof![Just(HOLD_MAX), 1usize..16],
        ) {
            let mut bytes = bytes;
            bytes.extend_from_slice(SETTLE);
            let cuts = sorted_cuts(bytes.len(), &cuts);
            let chunks = texts(&mut Feeder::with_bounds(hold, GRAPHIC_PAYLOAD_MAX), &bytes, &cuts);
            let mut stepwise = (vte::Parser::new(), Trace::default());
            for c in &chunks {
                stepwise.0.advance(&mut stepwise.1, c);
            }
            prop_assert_eq!(stepwise.1, vte_once(&chunks.concat()));
        }
    }
}
