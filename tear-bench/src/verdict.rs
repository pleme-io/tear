use std::collections::BTreeMap;

use serde::Serialize;

use crate::matrix::{Budget, Cell, Control, Floor, Rung, Stat, red_set};
use crate::stats;

pub const SENTINEL_TOLERANCE: f64 = 1.25;
pub const BLIND_STREAK: usize = 3;

tear_types::closed_vocabulary! {
    Sentinel {
        FramedRoundTrip => "framed-round-trip",
        ChannelWake => "channel-wake",
        UdsOneWay => "uds-one-way",
        RunLoopWake => "run-loop-wake",
    }
}

impl Sentinel {
    #[must_use]
    pub const fn floor(self) -> Floor {
        match self {
            Sentinel::FramedRoundTrip => Floor::UdsRoundTripFramed,
            Sentinel::ChannelWake => Floor::WakeHot,
            Sentinel::UdsOneWay => Floor::UdsOneWay,
            Sentinel::RunLoopWake => Floor::WakeRunLoop,
        }
    }
}

tear_types::closed_vocabulary! {
    HostClass { ReferenceMac => "reference-mac" }
}

impl HostClass {
    #[must_use]
    pub const fn reference_p50_ns(self, sentinel: Sentinel) -> f64 {
        match (self, sentinel) {
            (HostClass::ReferenceMac, Sentinel::FramedRoundTrip) => 7_380.0,
            (HostClass::ReferenceMac, Sentinel::ChannelWake) => 5_790.0,
            (HostClass::ReferenceMac, Sentinel::UdsOneWay) => 7_080.0,
            (HostClass::ReferenceMac, Sentinel::RunLoopWake) => 6_460.0,
        }
    }

    #[must_use]
    pub const fn receipt(self) -> &'static str {
        match self {
            HostClass::ReferenceMac => {
                "Mac16,7 M4 Pro, macOS 26.7.1, interactive band, 2026-10-07: framed 64 B UDS round trip 7.38 µs (F-a), mpsc hot wake 5.79 µs (F-d), 64 B UDS one-way 7.08 µs (F-e), CFRunLoop hot wake 6.46 µs (F-d)"
            }
        }
    }
}

pub fn sentinel_gate(
    host: Option<HostClass>,
    readings: &[(Sentinel, Option<f64>)],
) -> Result<(), String> {
    let Some(host) = host else {
        return Err("no reference table for this host class: --host-class names one".into());
    };
    let mut problems = Vec::new();
    for s in Sentinel::ALL {
        let reference = host.reference_p50_ns(*s);
        match readings.iter().find(|(r, _)| r == s).and_then(|(_, v)| *v) {
            None => problems.push(format!("sentinel {} was not measured", s.name())),
            Some(v) => {
                let ratio = v / reference;
                if !(1.0 / SENTINEL_TOLERANCE..=SENTINEL_TOLERANCE).contains(&ratio) {
                    problems.push(format!(
                        "sentinel {} p50 {:.0} ns is {ratio:.2}× the {} reference {:.0} ns",
                        s.name(),
                        v,
                        host.name(),
                        reference
                    ));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

#[derive(Clone, Debug, Default)]
pub struct FloorSet {
    samples: BTreeMap<Floor, Vec<f64>>,
}

impl FloorSet {
    pub fn add(&mut self, floor: Floor, samples: impl IntoIterator<Item = f64>) {
        self.samples.entry(floor).or_default().extend(samples);
    }

    #[must_use]
    pub fn samples(&self, floor: Floor) -> Option<&[f64]> {
        self.samples.get(&floor).map(Vec::as_slice)
    }

    #[must_use]
    pub fn stat(&self, floor: Floor, stat: Stat) -> Option<f64> {
        let parts = floor.parts();
        if parts.is_empty() {
            return stats::stat(self.samples.get(&floor)?, stat);
        }
        parts.iter().map(|p| self.stat(*p, stat)).sum()
    }

    #[must_use]
    pub fn sentinel_readings(&self) -> Vec<(Sentinel, Option<f64>)> {
        Sentinel::ALL
            .iter()
            .map(|s| (*s, self.stat(s.floor(), Stat::P50)))
            .collect()
    }

    pub fn floors(&self) -> impl Iterator<Item = (&Floor, &Vec<f64>)> {
        self.samples.iter()
    }
}

#[derive(Clone, Debug)]
pub struct Preconditions {
    pub quiet: Result<(), String>,
    pub peer: Result<(), String>,
    pub band: Result<(), String>,
    pub probes: Result<(), String>,
    pub min_samples: usize,
}

impl Preconditions {
    #[must_use]
    pub fn met(min_samples: usize) -> Self {
        Self {
            quiet: Ok(()),
            peer: Ok(()),
            band: Ok(()),
            probes: Ok(()),
            min_samples,
        }
    }

    fn blind(&self) -> Option<String> {
        [&self.quiet, &self.peer, &self.band, &self.probes]
            .into_iter()
            .filter_map(|r| r.as_ref().err().cloned())
            .reduce(|a, b| format!("{a}; {b}"))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "kebab-case")]
pub enum Verdict {
    Within { n: usize, value: f64, limit: f64 },
    Over { n: usize, value: f64, limit: f64 },
    Pending { rung: &'static str, n: usize },
    Blind { reason: String },
    Errored { reason: String },
    NotApplicable { why: &'static str },
}

impl Verdict {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Verdict::Within { .. } => "within",
            Verdict::Over { .. } => "over",
            Verdict::Pending { .. } => "pending",
            Verdict::Blind { .. } => "blind",
            Verdict::Errored { .. } => "errored",
            Verdict::NotApplicable { .. } => "not-applicable",
        }
    }

    #[must_use]
    pub const fn is_red(&self) -> bool {
        matches!(self, Verdict::Over { .. })
    }

    #[must_use]
    pub fn answer(&self, cell: Cell) -> kotae::Answer {
        self.answer_named(&cell.name())
    }

    #[must_use]
    pub fn answer_named(&self, name: &str) -> kotae::Answer {
        match self {
            Verdict::Blind { reason } => kotae::Answer::blind(format!("{name}: {reason}")),
            Verdict::Errored { reason } => {
                kotae::Answer::blind(format!("{name}: errored, {reason}"))
            }
            other => kotae::Answer::found(&serde_json::json!({
                "cell": name,
                "verdict": other,
            })),
        }
    }
}

#[must_use]
pub fn derive(budget: Budget, samples: &[f64], floors: &FloorSet, pre: &Preconditions) -> Verdict {
    let finite: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    match budget {
        Budget::NotApplicable { why } => return Verdict::NotApplicable { why },
        Budget::Pending { rung, .. } => {
            return Verdict::Pending {
                rung: Rung::name(rung),
                n: finite.len(),
            };
        }
        _ => {}
    }
    if let Some(reason) = pre.blind() {
        return Verdict::Blind { reason };
    }
    if finite.is_empty() {
        return Verdict::Errored {
            reason: "zero samples".into(),
        };
    }
    if finite.len() < pre.min_samples {
        return Verdict::Blind {
            reason: format!("{} samples, fewer than {}", finite.len(), pre.min_samples),
        };
    }
    let n = finite.len();
    let (value, limit) = match budget {
        Budget::Floor { floor, stat, k } => {
            let Some(value) = stats::stat(&finite, stat) else {
                return Verdict::Errored {
                    reason: "no statistic".into(),
                };
            };
            let Some(base) = floors.stat(floor, stat) else {
                return Verdict::Blind {
                    reason: format!("floor {} was not measured in this run", floor.name()),
                };
            };
            (value, k * base)
        }
        Budget::Count { max } | Budget::Bytes { max } => {
            let worst = finite.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            (worst, max as f64)
        }
        Budget::Exactly { n: want } => {
            let want = want as f64;
            let worst = finite.iter().copied().fold(want, |w, x| {
                if (x - want).abs() > (w - want).abs() {
                    x
                } else {
                    w
                }
            });
            return if (worst - want).abs() < 0.5 {
                Verdict::Within {
                    n,
                    value: worst,
                    limit: want,
                }
            } else {
                Verdict::Over {
                    n,
                    value: worst,
                    limit: want,
                }
            };
        }
        Budget::NotApplicable { .. } | Budget::Pending { .. } => unreachable!(),
    };
    if value <= limit {
        Verdict::Within { n, value, limit }
    } else {
        Verdict::Over { n, value, limit }
    }
}

#[must_use]
pub fn blind_streak_is_red(history_newest_last: &[bool]) -> bool {
    history_newest_last
        .iter()
        .rev()
        .take_while(|blind| **blind)
        .count()
        >= BLIND_STREAK
}

#[must_use]
pub const fn noise_band(stat: Stat) -> f64 {
    match stat {
        Stat::P50 | Stat::P90 => 1.42,
        Stat::P99 => 5.3,
    }
}

pub fn reddened(budget: Budget, v: &Verdict) -> Result<(), String> {
    match (v, budget) {
        (Verdict::Over { value, limit, .. }, Budget::Floor { stat, .. })
            if *value < limit * noise_band(stat) =>
        {
            Err(format!(
                "over by less than the noise band: {value:.0} against {limit:.0} × {}",
                noise_band(stat)
            ))
        }
        (Verdict::Over { .. }, _) => Ok(()),
        (other, _) => Err(format!("read {}, not over", other.name())),
    }
}

pub fn audit_control(control: Control, with_control_on: &[(Cell, Verdict)]) -> Result<(), String> {
    let declared = red_set(control);
    if declared.is_empty() {
        return Err(format!("control {} reddens nothing", control.name()));
    }
    let mut problems = Vec::new();
    for cell in &declared {
        let Some((_, v)) = with_control_on.iter().find(|(c, _)| c == cell) else {
            problems.push(format!(
                "{} was not measured with {} on",
                cell.name(),
                control.name()
            ));
            continue;
        };
        if let Err(e) = reddened(cell.budget(), v) {
            problems.push(format!("{} with {} on: {e}", cell.name(), control.name()));
        }
    }
    for (cell, v) in with_control_on {
        if matches!(v, Verdict::Over { .. })
            && !declared.contains(cell)
            && matches!(
                cell.budget(),
                Budget::Count { .. } | Budget::Bytes { .. } | Budget::Exactly { .. }
            )
        {
            problems.push(format!(
                "{} reddened {}, which it does not declare",
                control.name(),
                cell.name()
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::{Case, Metric};

    fn floors() -> FloorSet {
        let mut f = FloorSet::default();
        f.add(Floor::PtyEcho, [5_000.0, 5_200.0, 5_400.0]);
        f.add(Floor::WakeHot, [6_000.0]);
        f
    }

    #[test]
    fn a_composite_floor_is_the_sum_of_its_hops() {
        let f = floors();
        assert_eq!(f.stat(Floor::EchoEmbedded, Stat::P50), Some(11_200.0));
        assert_eq!(f.stat(Floor::EchoDaemon, Stat::P50), None);
    }

    #[test]
    fn a_floor_budget_grades_against_the_same_run() {
        let b = Budget::Floor {
            floor: Floor::EchoEmbedded,
            stat: Stat::P50,
            k: 2.0,
        };
        let pre = Preconditions::met(3);
        match derive(b, &[18_900.0, 19_000.0, 19_100.0], &floors(), &pre) {
            Verdict::Within { limit, .. } => assert!((limit - 22_400.0).abs() < 1e-6),
            other => panic!("{other:?}"),
        }
        assert!(derive(b, &[40_000.0, 40_000.0, 40_000.0], &floors(), &pre).is_red());
    }

    #[test]
    fn an_exact_budget_is_red_on_either_side_of_its_count() {
        let b = Budget::Exactly { n: 1 };
        let pre = Preconditions::met(1);
        let graded = |x: f64| derive(b, &[x], &floors(), &pre);
        assert!(matches!(graded(1.0), Verdict::Within { .. }));
        for off in [0.0, 2.0] {
            match graded(off) {
                Verdict::Over { value, limit, .. } => {
                    assert!(
                        (value - off).abs() < f64::EPSILON && (limit - 1.0).abs() < f64::EPSILON
                    );
                }
                other => panic!("{off}: {other:?}"),
            }
            assert!(reddened(b, &graded(off)).is_ok());
        }
        assert!(matches!(
            derive(b, &[1.0, 0.0, 1.0], &floors(), &Preconditions::met(3)),
            Verdict::Over { value, .. } if value == 0.0
        ));
    }

    #[test]
    fn preconditions_make_a_cell_blind_and_zero_samples_errored() {
        let b = Budget::Count { max: 0 };
        let mut pre = Preconditions::met(1);
        assert!(matches!(
            derive(b, &[], &floors(), &pre),
            Verdict::Errored { .. }
        ));
        pre.quiet = Err("sentinel drifted".into());
        assert!(matches!(
            derive(b, &[0.0], &floors(), &pre),
            Verdict::Blind { .. }
        ));
    }

    #[test]
    fn an_unmeasured_floor_is_blind_never_within() {
        let b = Budget::Floor {
            floor: Floor::UdsOneWay,
            stat: Stat::P50,
            k: 2.0,
        };
        assert!(matches!(
            derive(b, &[1.0], &floors(), &Preconditions::met(1)),
            Verdict::Blind { .. }
        ));
    }

    #[test]
    fn pending_and_not_applicable_come_from_the_budget_alone() {
        let pre = Preconditions {
            quiet: Err("loud".into()),
            ..Preconditions::met(1)
        };
        let cell = Cell::new(Case::C9, Metric::Frames);
        assert!(matches!(
            derive(cell.budget(), &[1024.0], &floors(), &pre),
            Verdict::Pending { n: 1, .. }
        ));
        let na = Cell::new(Case::C1, Metric::RoundTrip);
        assert!(matches!(
            derive(na.budget(), &[], &floors(), &pre),
            Verdict::NotApplicable { .. }
        ));
    }

    #[test]
    fn sentinels_need_a_reference_and_stay_within_tolerance() {
        let quiet: Vec<(Sentinel, Option<f64>)> = Sentinel::ALL
            .iter()
            .map(|s| (*s, Some(HostClass::ReferenceMac.reference_p50_ns(*s) * 1.1)))
            .collect();
        assert!(sentinel_gate(Some(HostClass::ReferenceMac), &quiet).is_ok());
        assert!(sentinel_gate(None, &quiet).is_err());
        let mut loud = quiet.clone();
        loud[2].1 = Some(HostClass::ReferenceMac.reference_p50_ns(Sentinel::UdsOneWay) * 1.4);
        assert!(sentinel_gate(Some(HostClass::ReferenceMac), &loud).is_err());
        let mut missing = quiet;
        missing[3].1 = None;
        assert!(sentinel_gate(Some(HostClass::ReferenceMac), &missing).is_err());
    }

    #[test]
    fn three_blind_runs_in_a_row_are_red() {
        assert!(!blind_streak_is_red(&[true, true]));
        assert!(blind_streak_is_red(&[false, true, true, true]));
        assert!(!blind_streak_is_red(&[true, true, true, false]));
    }

    #[test]
    fn a_control_passes_its_audit_only_when_every_declared_cell_reads_over() {
        let cell = Cell::new(Case::C4, Metric::Loss);
        let within = Verdict::Within {
            n: 1,
            value: 0.0,
            limit: 0.0,
        };
        assert!(audit_control(Control::MuteSink, &[(cell, within)]).is_err());
        let pending = Verdict::Pending { rung: "R4", n: 1 };
        assert!(audit_control(Control::MuteSink, &[(cell, pending)]).is_err());
        let muted = derive(
            cell.budget(),
            &[2_880_000.0],
            &floors(),
            &Preconditions::met(1),
        );
        assert!(audit_control(Control::MuteSink, &[(cell, muted)]).is_ok());
        for cannot_see in [
            Verdict::Blind {
                reason: "loud".into(),
            },
            Verdict::Errored {
                reason: "zero samples".into(),
            },
        ] {
            assert!(audit_control(Control::MuteSink, &[(cell, cannot_see)]).is_err());
        }
        assert!(audit_control(Control::MuteSink, &[]).is_err());
        let over = Verdict::Over {
            n: 1,
            value: 4_587_520.0,
            limit: 0.0,
        };
        assert!(audit_control(Control::MuteSink, &[(cell, over.clone())]).is_ok());
        let stray = Cell::new(Case::C4, Metric::Flushes);
        assert!(matches!(stray.budget(), Budget::Pending { .. }));
        assert!(audit_control(Control::MuteSink, &[(cell, over.clone()), (stray, over)]).is_ok());
    }

    #[test]
    fn a_timing_control_must_clear_the_noise_band() {
        let b = Budget::Floor {
            floor: Floor::PtyEcho,
            stat: Stat::P50,
            k: 2.0,
        };
        let over = |value: f64| Verdict::Over {
            n: 20,
            value,
            limit: 10_000.0,
        };
        assert!(reddened(b, &over(14_000.0)).is_err());
        assert!(reddened(b, &over(14_300.0)).is_ok());
        let p99 = Budget::Floor {
            stat: Stat::P99,
            k: 3.0,
            floor: Floor::PtyEcho,
        };
        assert!(reddened(p99, &over(50_000.0)).is_err());
        assert!(reddened(p99, &over(53_000.0)).is_ok());
        assert!(reddened(Budget::Count { max: 0 }, &over(1.0)).is_ok());
    }
}
