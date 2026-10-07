use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use serde::Serialize;

use crate::floor::{self, Plan, Suite};
use crate::harness::cases;
use crate::harness::daemon::Daemon;
use crate::harness::daemon::Options;
use crate::harness::rig::Rig;
use crate::harness::{Harness, isolated_rig_with, procs};
use crate::matrix::{
    Band, Budget, Case, Cell, Control, Floor, JournalSync, Metric, Variant, cells,
};
use crate::receipt::{RUNS, Sample};
use crate::verdict::{
    FloorSet, HostClass, Preconditions, Verdict, blind_streak_is_red, derive, sentinel_gate,
};

tear_types::closed_vocabulary! {
    Tier { Structural => "structural", Timing => "timing", All => "all" }
}

tear_types::closed_vocabulary! {
    ControlRun { Reddened => "reddened", Failed => "failed", Blind => "blind" }
}

pub struct Outcome {
    pub verdicts: Vec<(Cell, Verdict)>,
    pub sentinels: Result<(), String>,
    pub blind_streak: bool,
    pub controls: Vec<(Control, ControlRun, String)>,
}

impl Outcome {
    #[must_use]
    pub fn red(&self) -> bool {
        self.blind_streak
            || self.verdicts.iter().any(|(_, v)| v.is_red())
            || self
                .controls
                .iter()
                .any(|(_, run, _)| *run == ControlRun::Failed)
    }
}

#[derive(Serialize)]
struct Tally {
    within: usize,
    over: usize,
    pending: usize,
    blind: usize,
    errored: usize,
    not_applicable: usize,
}

fn with_daemon(
    h: &Harness,
    variant: Variant,
    tag: &str,
    band_ok: &mut Vec<String>,
    f: impl FnOnce(&Daemon, &Rig) -> io::Result<()>,
) -> io::Result<()> {
    with_daemon_opts(h, variant, tag, &Options::default(), band_ok, f).map(|_| ())
}

fn with_daemon_opts<T>(
    h: &Harness,
    variant: Variant,
    tag: &str,
    opts: &Options,
    band_ok: &mut Vec<String>,
    f: impl FnOnce(&Daemon, &Rig) -> io::Result<T>,
) -> io::Result<(T, Daemon)> {
    let (mut d, rig) = isolated_rig_with(h, variant, tag, opts)?;
    if let Err(e) = d.band_matches(h) {
        band_ok.push(format!("{}: {e}", d.label));
    }
    let res = f(&d, &rig);
    rig.kill_all();
    drop(rig);
    d.stop(h);
    res.map(|t| (t, d))
}

pub const AUDIT_KEYS: usize = 200;

fn audited() -> Options {
    Options {
        audit_log: true,
        faults: None,
    }
}

pub fn audit(h: &Harness, bands: &mut Vec<String>) -> io::Result<()> {
    let (count, _) = with_daemon_opts(
        h,
        Variant::BOUND,
        "gate-audit",
        &audited(),
        bands,
        |d, rig| cases::audit(h, d, rig, AUDIT_KEYS),
    )?;
    let cell = Cell::new(Case::C13, Metric::Flushes);
    h.emit_all(
        count
            .samples("audit", &Variant::BOUND.label())
            .into_iter()
            .map(|s| s.cell(cell))
            .collect(),
    );
    Ok(())
}

pub fn audit_control(h: &Harness, bands: &mut Vec<String>) -> (ControlRun, String) {
    let control = Control::AuditEveryKey;
    if !cfg!(feature = "bench-probes") {
        return (
            ControlRun::Blind,
            "this tearbench has no bench-probes, so it cannot arm a fault".into(),
        );
    }
    let opts = Options {
        audit_log: true,
        faults: Some(control.name().to_string()),
    };
    let run = with_daemon_opts(
        h,
        Variant::BOUND,
        "control-audit",
        &opts,
        bands,
        |d, rig| cases::audit(h, d, rig, AUDIT_KEYS),
    );
    let (count, d) = match run {
        Ok(v) => v,
        Err(e) => return (ControlRun::Failed, format!("the control run errored: {e}")),
    };
    if d.daemon_dump_rows == 0 {
        return (
            ControlRun::Blind,
            "the daemon wrote no probe dump: its build has no bench-probes, so the fault was never armed".into(),
        );
    }
    let cell = Cell::new(Case::C13, Metric::Flushes);
    let samples = count.samples(
        &format!("control:{}", control.name()),
        &Variant::BOUND.label(),
    );
    h.emit_all(samples.to_vec());
    let values: Vec<f64> = samples.iter().map(|s| s.value).collect();
    let v = derive(
        cell.budget(),
        &values,
        &FloorSet::default(),
        &Preconditions::met(1),
    );
    match crate::verdict::audit_control(control, &[(cell, v.clone())]) {
        Ok(()) => (
            ControlRun::Reddened,
            format!(
                "{} read {} with {} on",
                cell.name(),
                v.name(),
                control.name()
            ),
        ),
        Err(e) => (ControlRun::Failed, e),
    }
}

fn step(h: &Harness, name: &str, r: io::Result<()>) {
    match r {
        Ok(()) => h.log(&format!("case {name}: done")),
        Err(e) => {
            h.log(&format!("case {name}: FAILED: {e}"));
            let _ = h
                .receipt
                .fact(&format!("case-error:{name}"), &e.to_string());
        }
    }
}

pub fn structural(h: &Harness, bands: &mut Vec<String>) -> Vec<(Control, ControlRun, String)> {
    step(h, "wire", cases::wire(h));
    step(h, "allocations", cases::allocations(h).map(drop));
    step(h, "audit", audit(h, bands));
    let (run, detail) = audit_control(h, bands);
    h.log(&format!(
        "control {}: {} — {detail}",
        Control::AuditEveryKey.name(),
        run.name()
    ));
    let _ = h.receipt.fact(
        &format!("control:{}", Control::AuditEveryKey.name()),
        &format!("{}: {detail}", run.name()),
    );
    step(
        h,
        "keyloss",
        with_daemon(h, Variant::BOUND, "gate-keyloss", bands, |_, rig| {
            cases::keyloss(h, rig, cases::KEYLOSS_ROWS).map(drop)
        }),
    );
    step(
        h,
        "flood",
        with_daemon(h, Variant::BOUND, "gate-flood", bands, |_, rig| {
            cases::flood(h, rig, 1).map(drop)
        }),
    );
    step(
        h,
        "cliff",
        with_daemon(h, Variant::BOUND, "gate-cliff", bands, |_, rig| {
            cases::cliff(h, rig, &[0, 1_000, 3_000])
        }),
    );
    step(
        h,
        "stall",
        with_daemon(h, Variant::HELD, "gate-stall", bands, |d, rig| {
            cases::stall(h, d, rig, Duration::from_secs(3)).map(drop)
        }),
    );
    vec![(Control::AuditEveryKey, run, detail)]
}

pub fn timing(h: &Harness, bands: &mut Vec<String>) {
    let embedded = Rig::embedded("embedded");
    step(h, "echo-embedded", cases::echo(h, &embedded));
    step(h, "flood-embedded", cases::flood(h, &embedded, 3).map(drop));
    step(
        h,
        "attach-embedded",
        cases::attach(h, &embedded, &[262_144, 1_048_576], 1),
    );
    embedded.kill_all();
    for variant in [Variant::BOUND, Variant::HELD] {
        let label = variant.label();
        step(
            h,
            &format!("timing-{label}"),
            with_daemon(h, variant, "gate-timing", bands, |_, rig| {
                cases::rpc(h, rig)?;
                cases::echo(h, rig)?;
                cases::flood(h, rig, 3)?;
                if variant == Variant::BOUND {
                    cases::attach(h, rig, &[262_144, 1_048_576], 3)?;
                }
                Ok(())
            }),
        );
    }
    for variant in [
        Variant::HELD,
        Variant {
            journal: JournalSync::PageCache,
            ..Variant::HELD
        },
    ] {
        step(
            h,
            "series",
            with_daemon(h, variant, "gate-series", bands, |_, rig| {
                cases::series(h, rig, 15, Duration::from_millis(10)).map(drop)
            }),
        );
    }
    let tcp = Variant {
        transport: crate::matrix::TransportKind::Tcp,
        ..Variant::BOUND
    };
    step(
        h,
        "connect",
        with_daemon(h, tcp, "gate-connect", bands, |_, rig| {
            cases::connect(h, rig, 30)
        }),
    );
    for variant in [Variant::BOUND, Variant::HELD] {
        step(h, "startup", cases::startup(h, variant, 5));
    }
    step(h, "restart", cases::restart(h, Variant::HELD, 8_388_608, 3));
}

fn same_run_floors(h: &Harness, floors: &mut FloorSet) {
    for f in [
        Floor::PlainKeyEcho,
        Floor::PageCacheSeries,
        Floor::BoundNewSession,
    ] {
        let bench = format!("floor:{}", f.name());
        floors.add(
            f,
            h.collected()
                .into_iter()
                .filter(|s| s.bench == bench)
                .map(|s| s.value),
        );
    }
}

fn samples_for(all: &[Sample], cell: Cell) -> Vec<f64> {
    let source = match cell.metric {
        Metric::RoundTripTail => Cell::new(cell.case, Metric::RoundTrip),
        _ => cell,
    };
    all.iter()
        .filter(|s| s.cell == Some(source))
        .map(|s| s.value)
        .collect()
}

fn min_samples(b: Budget) -> usize {
    match b {
        Budget::Floor { .. } => 20,
        _ => 1,
    }
}

#[must_use]
pub fn blind_history_of(runs_tsv: &str, host: Option<HostClass>) -> Vec<bool> {
    let class = host.map_or("none", HostClass::name);
    let mut host_of: BTreeMap<&str, &str> = BTreeMap::new();
    let mut history = Vec::new();
    for line in runs_tsv.lines().skip(1) {
        let mut cols = line.split('\t');
        let (Some(run), Some(key), Some(value)) = (cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        match key {
            "host-class" => {
                host_of.insert(run, value);
            }
            "sentinels" if host_of.get(run) == Some(&class) => {
                history.push(value.starts_with("blind"));
            }
            _ => {}
        }
    }
    history
}

pub fn blind_history(h: &Harness, host: Option<HostClass>) -> Vec<bool> {
    std::fs::read_to_string(h.receipt.dir.join(RUNS))
        .map(|body| blind_history_of(&body, host))
        .unwrap_or_default()
}

pub fn run(h: &Harness, tier: Tier, host: Option<HostClass>, scale: u64) -> io::Result<Outcome> {
    h.receipt.fact("tier", tier.name())?;
    h.receipt
        .fact("host-class", host.map_or("none", HostClass::name))?;
    let plan = Plan {
        scale,
        band: Band::Interactive,
    };
    let mut floors = floor::run_suite(
        if tier == Tier::Structural {
            Suite::Sentinels
        } else {
            Suite::Standard
        },
        plan,
        &h.receipt,
    )?;
    crate::seam::apply_process_band(Band::Default)?;
    let quiet = sentinel_gate(host, &floors.sentinel_readings());
    h.receipt.fact(
        "sentinels",
        &match &quiet {
            Ok(()) => "quiet".to_string(),
            Err(e) => format!("blind: {e}"),
        },
    )?;
    let mut bands = Vec::new();
    let mut controls = Vec::new();
    if matches!(tier, Tier::Structural | Tier::All) {
        controls = structural(h, &mut bands);
    }
    if matches!(tier, Tier::Timing | Tier::All) {
        timing(h, &mut bands);
    }
    same_run_floors(h, &mut floors);
    let band = if bands.is_empty() {
        Ok(())
    } else {
        Err(bands.join("; "))
    };
    let all = h.collected();
    let mut verdicts = Vec::new();
    for cell in cells() {
        let budget = cell.budget();
        let pre = Preconditions {
            quiet: if matches!(budget, Budget::Floor { .. }) {
                quiet.clone()
            } else {
                Ok(())
            },
            peer: Ok(()),
            band: band.clone(),
            min_samples: min_samples(budget),
        };
        let v = derive(budget, &samples_for(&all, cell), &floors, &pre);
        h.receipt.verdict(cell, &v)?;
        verdicts.push((cell, v));
    }
    h.receipt.flush()?;
    let mut history = blind_history(h, host);
    if history.is_empty() {
        history.push(quiet.is_err());
    }
    Ok(Outcome {
        verdicts,
        sentinels: quiet,
        blind_streak: blind_streak_is_red(&history),
        controls,
    })
}

#[must_use]
pub fn summary(outcome: &Outcome) -> kotae::Answer {
    let mut t = Tally {
        within: 0,
        over: 0,
        pending: 0,
        blind: 0,
        errored: 0,
        not_applicable: 0,
    };
    let mut graded = Vec::new();
    for (cell, v) in &outcome.verdicts {
        match v {
            Verdict::Within { .. } => t.within += 1,
            Verdict::Over { .. } => t.over += 1,
            Verdict::Pending { .. } => t.pending += 1,
            Verdict::Blind { .. } => t.blind += 1,
            Verdict::Errored { .. } => t.errored += 1,
            Verdict::NotApplicable { .. } => t.not_applicable += 1,
        }
        if !matches!(v, Verdict::NotApplicable { .. }) {
            graded.push(serde_json::json!({
                "cell": cell.name(),
                "outcome": v.answer(*cell).outcome(),
                "verdict": v,
            }));
        }
    }
    let controls: Vec<serde_json::Value> = outcome
        .controls
        .iter()
        .map(|(c, run, detail)| serde_json::json!({"control": c.name(), "run": run.name(), "detail": detail}))
        .collect();
    let body = serde_json::json!({
        "sentinels": match &outcome.sentinels { Ok(()) => "quiet".to_string(), Err(e) => format!("blind: {e}") },
        "blind_streak_red": outcome.blind_streak,
        "controls": controls,
        "tally": t,
        "cells": graded,
    });
    if outcome.verdicts.iter().all(|(_, v)| {
        matches!(
            v,
            Verdict::Blind { .. } | Verdict::Errored { .. } | Verdict::NotApplicable { .. }
        )
    }) {
        kotae::Answer::blind(format!("no cell could be graded: {body}"))
    } else {
        kotae::Answer::found(&body)
    }
}

#[must_use]
pub fn matrix_json() -> serde_json::Value {
    let rows: Vec<serde_json::Value> = Case::ALL
        .iter()
        .map(|case| {
            let r = crate::matrix::row(*case);
            let budgets: serde_json::Map<String, serde_json::Value> = Metric::ALL
                .iter()
                .map(|m| (m.name().to_string(), budget_json(r.budgets.get(*m))))
                .collect();
            serde_json::json!({
                "case": case.name(),
                "title": case.title(),
                "receipt": r.receipt,
                "budgets": budgets,
                "controls": r.controls.iter().map(|red| serde_json::json!({
                    "control": red.control.name(),
                    "kind": red.control.kind().name(),
                    "rung": red.control.rung().name(),
                    "metrics": red.metrics.iter().map(|m| m.name()).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::json!({ "landed": crate::matrix::LANDED.iter().map(|r| r.name()).collect::<Vec<_>>(), "rows": rows })
}

fn budget_json(b: Budget) -> serde_json::Value {
    match b {
        Budget::Floor { floor, stat, k } => {
            serde_json::json!({"floor": floor.name(), "stat": stat.name(), "k": k})
        }
        Budget::Count { max } => serde_json::json!({"count_max": max}),
        Budget::Bytes { max } => serde_json::json!({"bytes_max": max}),
        Budget::NotApplicable { why } => serde_json::json!({"not_applicable": why}),
        Budget::Pending {
            rung,
            today,
            receipt,
        } => serde_json::json!({"pending": rung.name(), "today": today, "receipt": receipt}),
    }
}

pub fn status(h: &Harness) -> Vec<procs::Proc> {
    let all = procs::ps();
    let started: Vec<i32> = h
        .started()
        .into_iter()
        .filter(|s| s.alive())
        .map(|s| s.pid)
        .collect();
    let mut mine: Vec<procs::Proc> = all
        .iter()
        .filter(|p| started.contains(&p.pid))
        .cloned()
        .collect();
    let roots: Vec<i32> = mine.iter().map(|p| p.pid).collect();
    mine.extend(procs::descendants(&all, &roots));
    mine.extend(procs::holders_under(&all, &h.iso()));
    mine.sort_by_key(|p| p.pid);
    mine.dedup_by_key(|p| p.pid);
    mine
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(rows: &[(&str, &str, &str)]) -> String {
        let mut out = String::from("run\tkey\tvalue\n");
        for (run, key, value) in rows {
            out.push_str(&[*run, *key, *value].join("\t"));
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_blind_streak_reads_runs_in_the_order_they_ran_not_by_name() {
        let mac = Some(HostClass::ReferenceMac);
        let mut rows = Vec::new();
        for (run, s) in [
            ("gate-8", "quiet"),
            ("gate-9", "blind: loud"),
            ("gate-10", "blind: loud"),
            ("gate-11", "blind: loud"),
        ] {
            rows.push((run, "host-class", "reference-mac"));
            rows.push((run, "sentinels", s));
        }
        let history = blind_history_of(&runs(&rows), mac);
        assert_eq!(history, vec![false, true, true, true]);
        assert!(blind_streak_is_red(&history));
    }

    #[test]
    fn a_rerun_under_a_reused_name_counts_again_and_another_host_class_not_at_all() {
        let mac = Some(HostClass::ReferenceMac);
        let rows = [
            ("1760000000000", "host-class", "reference-mac"),
            ("1760000000000", "sentinels", "blind: loud"),
            ("ci", "host-class", "none"),
            ("ci", "sentinels", "blind: no table"),
            ("named", "host-class", "reference-mac"),
            ("named", "sentinels", "quiet"),
            ("named", "host-class", "reference-mac"),
            ("named", "sentinels", "blind: loud"),
        ];
        assert_eq!(blind_history_of(&runs(&rows), mac), vec![true, false, true]);
        assert_eq!(blind_history_of(&runs(&rows), None), vec![true]);
    }
}
