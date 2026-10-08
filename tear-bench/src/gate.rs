use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use serde::Serialize;

use crate::compat::{self, CompatCell, HolderPeer};
use crate::floor::{self, Plan, Suite};
use crate::harness::cases::{self, AfterStall};
use crate::harness::daemon::Daemon;
use crate::harness::daemon::Options;
use crate::harness::rig::Rig;
use crate::harness::window::{self, PaneFate, WindowOptions, WindowRun};
use crate::harness::{Harness, Tag, arm_client, isolated_rig_with, procs};
use crate::matrix::{
    Band, Budget, Case, Cell, Control, Floor, Handover, Input, JournalSync, Metric, Remote,
    Variant, WINDOW_CELLS, cells,
};
use crate::receipt::{RUNS, Sample, Unit};
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
    pub compat: Vec<(CompatCell, Verdict)>,
    pub sentinels: Result<(), String>,
    pub blind_streak: bool,
    pub controls: Vec<(Control, ControlRun, String)>,
}

impl Outcome {
    #[must_use]
    pub fn red(&self) -> bool {
        self.blind_streak
            || self.verdicts.iter().any(|(_, v)| v.is_red())
            || self.compat.iter().any(|(_, v)| v.is_red())
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
        ..Options::default()
    }
}

pub const BENCH_TOKEN: &str = "tearbench-r3-token";

#[must_use]
pub fn tokened() -> Options {
    Options {
        auth_token: Some(BENCH_TOKEN.to_string()),
        ..Options::default()
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

pub struct ControlSpec {
    pub control: Control,
    pub variant: Variant,
    pub opts: Options,
    pub daemon: &'static [Control],
    pub client: &'static [Control],
}

impl ControlSpec {
    #[must_use]
    pub fn daemon_options(&self) -> Options {
        Options {
            faults: Some(
                self.daemon
                    .iter()
                    .map(|c| c.name())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            ..self.opts.clone()
        }
    }
}

fn graded(h: &Harness, control: Control, cell: Cell, mut sample: Sample) -> (Cell, f64) {
    sample.bench = format!("control:{}", control.name());
    let value = sample.value;
    h.emit(sample);
    (cell, value)
}

pub fn control_run(
    h: &Harness,
    bands: &mut Vec<String>,
    spec: &ControlSpec,
    run: impl FnOnce(&Daemon, &Rig) -> io::Result<Vec<(Cell, f64)>>,
) -> (ControlRun, String) {
    let control = spec.control;
    if !cfg!(feature = "bench-probes") {
        return (
            ControlRun::Blind,
            "this tearbench has no bench-probes, so it cannot arm a fault".into(),
        );
    }
    let opts = spec.daemon_options();
    arm_client(spec.client);
    let res = with_daemon_opts(
        h,
        spec.variant,
        &format!("control-{}", control.name()),
        &opts,
        bands,
        run,
    );
    arm_client(&[]);
    let (values, d) = match res {
        Ok(v) => v,
        Err(e) => return (ControlRun::Failed, format!("the control run errored: {e}")),
    };
    if !spec.daemon.is_empty() && d.daemon_dump_rows == 0 {
        return (
            ControlRun::Blind,
            "the daemon wrote no probe dump: its build has no bench-probes, so the fault was never armed".into(),
        );
    }
    let verdicts: Vec<(Cell, Verdict)> = crate::matrix::red_set(control)
        .into_iter()
        .map(|cell| {
            let of_cell: Vec<f64> = values
                .iter()
                .filter(|(c, _)| *c == cell)
                .map(|(_, v)| *v)
                .collect();
            let v = derive(
                cell.budget(),
                &of_cell,
                &FloorSet::default(),
                &Preconditions::met(1),
            );
            (cell, v)
        })
        .collect();
    match crate::verdict::audit_control(control, &verdicts) {
        Ok(()) => (
            ControlRun::Reddened,
            verdicts
                .iter()
                .map(|(cell, v)| {
                    format!(
                        "{} read {} with {} on",
                        cell.name(),
                        v.name(),
                        control.name()
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        ),
        Err(e) => (ControlRun::Failed, e),
    }
}

pub fn audit_control(h: &Harness, bands: &mut Vec<String>) -> (ControlRun, String) {
    let control = Control::AuditEveryKey;
    let spec = ControlSpec {
        control,
        variant: Variant::BOUND,
        opts: audited(),
        daemon: &[Control::AuditEveryKey],
        client: &[],
    };
    control_run(h, bands, &spec, |d, rig| {
        let count = cases::audit(h, d, rig, AUDIT_KEYS)?;
        let samples = count.samples(
            &format!("control:{}", control.name()),
            &Variant::BOUND.label(),
        );
        h.emit_all(samples.to_vec());
        let cell = Cell::new(Case::C13, Metric::Flushes);
        Ok(samples.iter().map(|s| (cell, s.value)).collect())
    })
}

fn keyloss_cells(h: &Harness, rig: &Rig, tag: Tag) -> io::Result<Vec<(Cell, f64)>> {
    let k = cases::keyloss(h, rig, cases::KEYLOSS_ROWS, tag)?;
    let cell = Cell::new(Case::C3, Metric::Loss);
    Ok(std::iter::once(k.lost())
        .chain(k.redials)
        .map(|v| (cell, v as f64))
        .collect())
}

pub fn wire_controls(h: &Harness, bands: &mut Vec<String>) -> Vec<(Control, ControlRun, String)> {
    let mut out = Vec::new();
    for (control, client) in [
        (Control::ResponseSizeUnchecked, &[][..]),
        (Control::LegacyReplay, &[Control::LegacyReplay][..]),
    ] {
        let spec = ControlSpec {
            control,
            variant: Variant::BOUND,
            opts: Options::default(),
            daemon: &[Control::ResponseSizeUnchecked],
            client,
        };
        let (run, detail) = control_run(h, bands, &spec, |_, rig| {
            keyloss_cells(h, rig, Tag::Control(control))
        });
        out.push((control, run, detail));
    }
    let control = Control::RawSubscribe;
    let spec = ControlSpec {
        control,
        variant: Variant::TCP,
        opts: tokened(),
        daemon: &[],
        client: &[Control::RawSubscribe],
    };
    let (run, detail) = control_run(h, bands, &spec, |_, rig| {
        let t = cases::tokened(h, rig, cases::TOKENED_KEYS, Tag::Control(control))?;
        Ok(vec![(
            Cell::new(Case::C6(Remote::Tcp), Metric::Loss),
            t.lost() as f64,
        )])
    });
    out.push((control, run, detail));
    let control = Control::UnchunkedInput;
    let spec = ControlSpec {
        control,
        variant: Variant::BOUND,
        opts: Options::default(),
        daemon: &[],
        client: &[Control::UnchunkedInput],
    };
    let (run, detail) = control_run(h, bands, &spec, |_, rig| {
        let p = cases::paste(h, rig, cases::PASTE_BYTES, Tag::Control(control))?;
        let cell = Cell::new(Case::C12(Input::Paste), Metric::Loss);
        Ok(vec![
            (cell, p.missing() as f64),
            (cell, p.mismatched() as f64),
        ])
    });
    out.push((control, run, detail));
    out
}

fn report_control(h: &Harness, control: Control, run: ControlRun, detail: &str) {
    h.log(&format!(
        "control {}: {} — {detail}",
        control.name(),
        run.name()
    ));
    let _ = h.receipt.fact(
        &format!("control:{}", control.name()),
        &format!("{}: {detail}", run.name()),
    );
}

pub fn split_control(h: &Harness) -> (ControlRun, String) {
    let control = Control::OldSplitter;
    let (lost, detail) = match cases::split_losses(cases::Split::OldSplitter) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            return (ControlRun::Blind, e.to_string());
        }
        Err(e) => return (ControlRun::Failed, format!("the control run errored: {e}")),
    };
    h.emit(
        Sample::new(
            &format!("control:{}", control.name()),
            "tear-core",
            "corpora-losing-a-character",
            0,
            lost as f64,
            Unit::Count,
        )
        .detail(detail),
    );
    let cell = Cell::new(Case::C9, Metric::Loss);
    let v = derive(
        cell.budget(),
        &[lost as f64],
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

pub const STALL: Duration = Duration::from_secs(3);
pub const TWIN_WINDOW: Duration = Duration::from_secs(10);

pub fn stall(h: &Harness, bands: &mut Vec<String>) -> io::Result<()> {
    with_daemon(h, Variant::HELD, "gate-stall", bands, |d, rig| {
        let s = cases::stall(h, d, rig, STALL, AfterStall::Silence)?;
        h.emit(
            s.sample(&rig.label, AfterStall::Silence)
                .cell(Cell::new(Case::C4, Metric::Loss)),
        );
        Ok(())
    })
}

pub fn mute_sink_control(h: &Harness, bands: &mut Vec<String>) -> (ControlRun, String) {
    let control = Control::MuteSink;
    let spec = ControlSpec {
        control,
        variant: Variant::HELD,
        opts: Options::default(),
        daemon: &[Control::MuteSink],
        client: &[],
    };
    control_run(h, bands, &spec, |d, rig| {
        let s = cases::stall(h, d, rig, STALL, AfterStall::Silence)?;
        Ok(vec![graded(
            h,
            control,
            Cell::new(Case::C4, Metric::Loss),
            s.sample(&rig.label, AfterStall::Silence),
        )])
    })
}

pub fn twin(h: &Harness, bands: &mut Vec<String>) -> io::Result<()> {
    with_daemon(h, Variant::HELD, "gate-twin", bands, |d, rig| {
        let t = cases::twin(h, d, rig, &Options::default(), TWIN_WINDOW)?;
        h.emit(
            t.sample(&rig.label, TWIN_WINDOW)
                .cell(Cell::new(Case::C7(Handover::Readopt), Metric::Authorities)),
        );
        Ok(())
    })
}

pub fn store_lease_off_control(h: &Harness, bands: &mut Vec<String>) -> (ControlRun, String) {
    let control = Control::StoreLeaseOff;
    let spec = ControlSpec {
        control,
        variant: Variant::HELD,
        opts: Options::default(),
        daemon: &[Control::StoreLeaseOff],
        client: &[],
    };
    control_run(h, bands, &spec, |d, rig| {
        let t = cases::twin(h, d, rig, &spec.daemon_options(), TWIN_WINDOW)?;
        Ok(vec![graded(
            h,
            control,
            Cell::new(Case::C7(Handover::Readopt), Metric::Authorities),
            t.sample(&rig.label, TWIN_WINDOW),
        )])
    })
}

#[must_use]
pub fn holder_program(h: &Harness, peer: HolderPeer) -> Option<Option<std::path::PathBuf>> {
    match peer {
        HolderPeer::Head => Some(None),
        HolderPeer::Prev => h.settings.prev_tear_bin.clone().map(Some),
        HolderPeer::Oldest => h.settings.oldest_tear_bin.clone().map(Some),
    }
}

pub fn muted_holders(h: &Harness, bands: &mut Vec<String>) {
    for peer in HolderPeer::ALL {
        let Some(program) = holder_program(h, *peer) else {
            h.log(&format!(
                "compat muted-holder/{}: no {} build was given, so its cell reads blind",
                peer.name(),
                peer.name()
            ));
            continue;
        };
        let opts = Options {
            holder_program: program,
            ..Options::default()
        };
        let cell = CompatCell::muted_holder(*peer);
        step(
            h,
            &format!("muted-holder-{}", peer.name()),
            with_daemon_opts(
                h,
                Variant::HELD,
                &format!("gate-compat-{}", peer.name()),
                &opts,
                bands,
                |d, rig| {
                    let s = cases::stall(h, d, rig, STALL, AfterStall::ReadEdges)?;
                    h.emit(s.sample(&rig.label, AfterStall::ReadEdges).compat(cell));
                    Ok(())
                },
            )
            .map(drop),
        );
    }
}

fn mado_window(
    h: &Harness,
    bands: &mut Vec<String>,
    tag: &str,
    opts: &WindowOptions,
) -> Result<WindowRun, String> {
    let Some(mado) = h.settings.mado_bin.clone() else {
        return Err("no --mado-bin: a resident-window cell needs a mado to measure".into());
    };
    let (run, _) = with_daemon_opts(
        h,
        Variant::BOUND,
        tag,
        &Options::default(),
        bands,
        |d, rig| window::run_window(h, d, rig, &mado, tag, opts),
    )
    .map_err(|e| format!("the resident window could not be measured: {e}"))?;
    Ok(run)
}

fn window_samples(run: &WindowRun, bench: &str, opts: &WindowOptions) -> Vec<Sample> {
    let variant = Variant::BOUND.label();
    let mut out = Vec::new();
    if let Some(idle) = &run.idle {
        out.push(
            Sample::new(
                bench,
                &variant,
                "ui-get-pane-at-idle",
                0,
                idle.get_pane as f64,
                Unit::Count,
            )
            .cell(Cell::new(Case::C3, Metric::Rpcs))
            .detail(format!(
                "pane_fate: {}, fate_backstop_secs: {}, {:.1} s idle, mado pid {}",
                opts.pane_fate.name(),
                window::FATE_BACKSTOP_SECS,
                idle.secs,
                run.pid
            )),
        );
    }
    out.extend(run.presents_ns.iter().enumerate().map(|(i, ns)| {
        Sample::new(bench, &variant, "byte-to-present", i, *ns, Unit::Ns)
            .cell(Cell::new(Case::C3, Metric::Present))
            .detail(format!(
                "{} of {} isolated echoes presented ({} ms apart), the histogram bucket's upper bound; faults: [{}], mado pid {}",
                run.presents_ns.len(),
                run.echoes,
                opts.echo_gap.as_millis(),
                run.armed.join(","),
                run.pid
            ))
    }));
    out
}

pub fn window(h: &Harness, bands: &mut Vec<String>) -> Result<(), String> {
    let opts = WindowOptions::clean();
    let run = mado_window(h, bands, "gate-window", &opts)?;
    h.emit_all(window_samples(&run, "window", &opts));
    Ok(())
}

fn window_control(
    h: &Harness,
    bands: &mut Vec<String>,
    control: Control,
    opts: &WindowOptions,
    floors: &FloorSet,
    quiet: &Result<(), String>,
) -> (ControlRun, String) {
    let run = match mado_window(h, bands, &format!("control-{}", control.name()), opts) {
        Ok(r) => r,
        Err(e) => return (ControlRun::Blind, e),
    };
    let unarmed = run.unarmed(opts.faults);
    if !unarmed.is_empty() {
        return (
            ControlRun::Blind,
            format!(
                "the mado window reported {} unarmed: its build has no bench-probes",
                unarmed.join(", ")
            ),
        );
    }
    let samples = window_samples(&run, &format!("control:{}", control.name()), opts);
    let values: Vec<(Cell, f64)> = samples
        .iter()
        .filter_map(|s| s.cell.map(|c| (c, s.value)))
        .collect();
    h.emit_all(
        samples
            .into_iter()
            .map(|s| Sample { cell: None, ..s })
            .collect(),
    );
    let mut verdicts = Vec::new();
    for cell in crate::matrix::red_set(control) {
        let budget = cell.budget();
        let pre = Preconditions {
            quiet: if matches!(budget, Budget::Floor { .. }) {
                quiet.clone()
            } else {
                Ok(())
            },
            min_samples: min_samples(budget),
            ..Preconditions::met(1)
        };
        let of_cell: Vec<f64> = values
            .iter()
            .filter(|(c, _)| *c == cell)
            .map(|(_, v)| *v)
            .collect();
        let v = derive(budget, &of_cell, floors, &pre);
        if let Verdict::Blind { reason } = &v {
            return (
                ControlRun::Blind,
                format!(
                    "{} read blind with {} on: {reason}",
                    cell.name(),
                    control.name()
                ),
            );
        }
        verdicts.push((cell, v));
    }
    match crate::verdict::audit_control(control, &verdicts) {
        Ok(()) => (
            ControlRun::Reddened,
            verdicts
                .iter()
                .map(|(cell, v)| {
                    format!(
                        "{} read {} with {} on: {v:?}",
                        cell.name(),
                        v.name(),
                        control.name()
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        ),
        Err(e) => (ControlRun::Failed, e),
    }
}

pub fn window_controls(
    h: &Harness,
    bands: &mut Vec<String>,
    floors: &FloorSet,
    quiet: &Result<(), String>,
) -> Vec<(Control, ControlRun, String)> {
    [
        (
            Control::PaneFatePoll,
            WindowOptions::idle_only(PaneFate::Poll),
        ),
        (
            Control::WakeOff,
            WindowOptions::echoes_only(&[Control::WakeOff]),
        ),
    ]
    .into_iter()
    .map(|(control, opts)| {
        let (run, detail) = window_control(h, bands, control, &opts, floors, quiet);
        (control, run, detail)
    })
    .collect()
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

pub struct Structural {
    pub controls: Vec<(Control, ControlRun, String)>,
    pub window: Result<(), String>,
}

impl Structural {
    fn skipped() -> Self {
        Self {
            controls: Vec::new(),
            window: Err(
                "the structural tier, which measures the resident window, did not run".into(),
            ),
        }
    }
}

fn window_peer(cell: Cell, window: &Result<(), String>) -> Result<(), String> {
    if WINDOW_CELLS.contains(&cell) {
        window.clone()
    } else {
        Ok(())
    }
}

pub fn structural(
    h: &Harness,
    bands: &mut Vec<String>,
    floors: &FloorSet,
    quiet: &Result<(), String>,
) -> Structural {
    step(h, "wire", cases::wire(h));
    step(h, "allocations", cases::allocations(h).map(drop));
    step(h, "split", cases::split(h).map(drop));
    let (split_run, split_detail) = split_control(h);
    report_control(h, Control::OldSplitter, split_run, &split_detail);
    step(h, "audit", audit(h, bands));
    let (run, detail) = audit_control(h, bands);
    report_control(h, Control::AuditEveryKey, run, &detail);
    let mut controls = vec![
        (Control::OldSplitter, split_run, split_detail),
        (Control::AuditEveryKey, run, detail),
    ];
    step(
        h,
        "keyloss",
        with_daemon(h, Variant::BOUND, "gate-keyloss", bands, |_, rig| {
            cases::keyloss(h, rig, cases::KEYLOSS_ROWS, Tag::Cells).map(drop)
        }),
    );
    step(
        h,
        "tokened",
        with_daemon_opts(
            h,
            Variant::TCP,
            "gate-tokened",
            &tokened(),
            bands,
            |_, rig| cases::tokened(h, rig, cases::TOKENED_KEYS, Tag::Cells).map(drop),
        )
        .map(drop),
    );
    step(
        h,
        "paste",
        with_daemon(h, Variant::BOUND, "gate-paste", bands, |_, rig| {
            cases::paste(h, rig, cases::PASTE_BYTES, Tag::Cells).map(drop)
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
    step(h, "stall", stall(h, bands));
    step(h, "twin", twin(h, bands));
    muted_holders(h, bands);
    let r4 = [
        (Control::MuteSink, mute_sink_control(h, bands)),
        (Control::StoreLeaseOff, store_lease_off_control(h, bands)),
    ]
    .into_iter()
    .map(|(control, (run, detail))| (control, run, detail));
    for (control, run, detail) in wire_controls(h, bands).into_iter().chain(r4) {
        report_control(h, control, run, &detail);
        controls.push((control, run, detail));
    }
    let window = window(h, bands);
    h.log(&format!(
        "case window: {}",
        window
            .as_ref()
            .map_or_else(Clone::clone, |()| "done".into())
    ));
    for (control, run, detail) in window_controls(h, bands, floors, quiet) {
        report_control(h, control, run, &detail);
        controls.push((control, run, detail));
    }
    Structural { controls, window }
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
    step(
        h,
        "connect",
        with_daemon(h, Variant::TCP, "gate-connect", bands, |_, rig| {
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
    let Structural { controls, window } = if matches!(tier, Tier::Structural | Tier::All) {
        structural(h, &mut bands, &floors, &quiet)
    } else {
        Structural::skipped()
    };
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
            peer: window_peer(cell, &window),
            band: band.clone(),
            probes: cases::probes_reach(cell),
            min_samples: min_samples(budget),
        };
        let v = derive(budget, &samples_for(&all, cell), &floors, &pre);
        h.receipt.verdict(cell, &v)?;
        verdicts.push((cell, v));
    }
    let mut compat_verdicts = Vec::new();
    for cell in compat::cells() {
        let budget = cell.budget();
        let peer = if holder_program(h, cell.pairing.holder).is_some() {
            Ok(())
        } else {
            Err(format!(
                "no {} holder build was given (--{}-tear-bin); `nix run .#bench` builds none yet",
                cell.pairing.holder.name(),
                cell.pairing.holder.name()
            ))
        };
        let pre = Preconditions {
            quiet: Ok(()),
            peer,
            band: band.clone(),
            probes: Ok(()),
            min_samples: 1,
        };
        let values: Vec<f64> = all
            .iter()
            .filter(|s| s.compat == Some(cell))
            .map(|s| s.value)
            .collect();
        let v = derive(budget, &values, &floors, &pre);
        h.receipt.compat_verdict(cell, &v)?;
        compat_verdicts.push((cell, v));
    }
    h.receipt.flush()?;
    let mut history = blind_history(h, host);
    if history.is_empty() {
        history.push(quiet.is_err());
    }
    Ok(Outcome {
        verdicts,
        compat: compat_verdicts,
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
    let compat: Vec<serde_json::Value> = outcome
        .compat
        .iter()
        .filter(|(_, v)| !matches!(v, Verdict::NotApplicable { .. } | Verdict::Pending { .. }))
        .map(|(cell, v)| {
            serde_json::json!({
                "cell": cell.name(),
                "outcome": v.answer_named(&cell.name()).outcome(),
                "verdict": v,
            })
        })
        .collect();
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
        "compat": compat,
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
        Budget::Exactly { n } => serde_json::json!({"exactly": n}),
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
