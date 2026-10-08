use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tear_bench::floor::{self, Plan, Suite};
use tear_bench::gate::{self, Tier};
use tear_bench::harness::cases;
use tear_bench::harness::daemon::{Daemon, band_exec};
use tear_bench::harness::isolation::{self, DEFAULT_PATH, FORBID_ENV, PATH_ENV};
use tear_bench::harness::reproduce;
use tear_bench::harness::rig::Rig;
use tear_bench::harness::{Harness, Settings, Tag};
use tear_bench::matrix::{Band, Variant};
use tear_bench::verdict::HostClass;

#[derive(Parser)]
#[command(
    name = "tearbench",
    version,
    about = "tear's all-variants gate: the matrix, the floors, the isolated harness"
)]
struct Cli {
    #[arg(long, global = true)]
    run_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    tear_bin: Option<PathBuf>,
    #[arg(long, global = true)]
    prev_tear_bin: Option<PathBuf>,
    #[arg(long, global = true)]
    oldest_tear_bin: Option<PathBuf>,
    #[arg(long, global = true)]
    mado_bin: Option<PathBuf>,
    #[arg(long, global = true)]
    parked_window_bin: Option<PathBuf>,
    #[arg(long, global = true)]
    forbid: Option<PathBuf>,
    #[arg(long, global = true)]
    path_env: Option<String>,
    #[arg(long, global = true, default_value = "")]
    faults: String,
    #[arg(long, global = true)]
    run: Option<String>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Matrix,
    Floors {
        #[arg(long, default_value = "standard")]
        suite: String,
        #[arg(long, default_value_t = 1)]
        scale: u64,
        #[arg(long, default_value = "interactive")]
        band: String,
    },
    Reproduce {
        #[arg(long, default_value_t = 1)]
        flood_reps: usize,
        #[arg(long, default_value_t = 15)]
        series_secs: u64,
    },
    Case {
        names: String,
        #[arg(long, default_value = "bound")]
        variant: String,
    },
    Gate {
        #[arg(long, default_value = "all")]
        tier: String,
        #[arg(long)]
        host_class: Option<String>,
        #[arg(long, default_value_t = 1)]
        scale: u64,
    },
    ReplayFile {
        path: PathBuf,
    },
    BandProbe,
    Status,
    Cleanup,
    #[command(hide = true)]
    FloorPeer {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    BandExec {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

impl Cmd {
    const fn isolated(&self) -> bool {
        !matches!(
            self,
            Cmd::Matrix | Cmd::FloorPeer { .. } | Cmd::BandExec { .. }
        )
    }
}

fn default_tear_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("tear")))
        .unwrap_or_else(|| PathBuf::from("tear"))
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("tearbench: {msg}");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Cmd::FloorPeer { args } => {
            return floor::peer(args).map_or_else(
                |e| fail(&format!("floor peer: {e}")),
                |()| ExitCode::SUCCESS,
            );
        }
        Cmd::BandExec { args } => {
            return band_exec(args)
                .map_or_else(|e| fail(&format!("band-exec: {e}")), |()| ExitCode::SUCCESS);
        }
        Cmd::Matrix => {
            println!(
                "{}",
                serde_json::to_string_pretty(&gate::matrix_json()).unwrap_or_default()
            );
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let path_env = cli
        .path_env
        .clone()
        .or_else(|| std::env::var(PATH_ENV).ok())
        .unwrap_or_else(|| DEFAULT_PATH.to_string());
    if cli.command.isolated() && std::env::var_os(isolation::ROOT_ENV).is_none() {
        let root = cli
            .run_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("tearbench"));
        let forbid = cli
            .forbid
            .clone()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_default();
        if let Err(e) = isolation::root_allowed(&root, &forbid) {
            eprintln!("tearbench: refusing to run: {e}");
            return ExitCode::from(2);
        }
        let mut args: Vec<String> = std::env::args().skip(1).collect();
        if cli.tear_bin.is_none() {
            args.push("--tear-bin".into());
            args.push(default_tear_bin().to_string_lossy().into_owned());
        }
        return match isolation::relaunch(&root, &path_env, &forbid, &args) {
            Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
            Err(e) => fail(&format!(
                "could not re-launch into isolation under {}: {e}",
                root.display()
            )),
        };
    }
    let root = match isolation::guard() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("tearbench: refusing to run: {e}");
            return ExitCode::from(2);
        }
    };
    let forbid = std::env::var_os(FORBID_ENV)
        .map(PathBuf::from)
        .unwrap_or_default();
    let settings = Settings {
        root,
        tear_bin: cli.tear_bin.clone().unwrap_or_else(default_tear_bin),
        prev_tear_bin: cli.prev_tear_bin.clone(),
        oldest_tear_bin: cli.oldest_tear_bin.clone(),
        mado_bin: cli.mado_bin.clone(),
        parked_window_bin: cli.parked_window_bin.clone(),
        forbid,
        path_env,
        faults: cli.faults.clone(),
        size: (tear_bench::harness::COLS, tear_bench::harness::ROWS),
    };
    let h = match Harness::open(settings, cli.run.clone()) {
        Ok(h) => h,
        Err(e) => return fail(&format!("could not open the run: {e}")),
    };
    let needs_tear = matches!(
        cli.command,
        Cmd::Reproduce { .. } | Cmd::Case { .. } | Cmd::Gate { .. }
    );
    if needs_tear && let Err(e) = h.require_workspace_build() {
        return fail(&e.to_string());
    }
    let code = dispatch(&h, &cli.command);
    if needs_tear {
        h.cleanup();
    }
    let _ = h.receipt.flush();
    eprintln!("tearbench: receipts in {}", h.receipt.dir.display());
    code
}

fn floors_cmd(h: &Harness, suite: &str, scale: u64, band: &str) -> ExitCode {
    let Some(band) = Band::parse(band) else {
        return fail(&format!(
            "--band {band}: one of interactive, default, background"
        ));
    };
    let suite = if suite == "sentinels" {
        Suite::Sentinels
    } else {
        Suite::Standard
    };
    match floor::run_suite(suite, Plan { scale, band }, &h.receipt) {
        Ok(set) => {
            for (f, v) in set.floors() {
                if let Some(s) = tear_bench::stats::summarize(v) {
                    println!(
                        "{:<24} n={:<6} p50={:>12.0} p90={:>12.0} p99={:>12.0} max={:>12.0}",
                        f.name(),
                        s.n,
                        s.p50,
                        s.p90,
                        s.p99,
                        s.max
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("floors: {e}")),
    }
}

fn reproduce_cmd(h: &Harness, flood_reps: usize, series_secs: u64) -> ExitCode {
    let mut all = true;
    for c in reproduce::run(h, flood_reps, series_secs) {
        match c {
            Ok(c) => {
                all &= c.reproduced;
                println!(
                    "{} {}\n    receipt  {}\n    expected {}\n    measured {}",
                    if c.reproduced {
                        "REPRODUCED"
                    } else {
                        "DIFFERS   "
                    },
                    c.name,
                    c.receipt,
                    c.expected,
                    c.measured
                );
            }
            Err(e) => {
                all = false;
                println!("ERRORED    {e}");
            }
        }
    }
    if all {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn gate_cmd(h: &Harness, tier: &str, host_class: Option<&str>, scale: u64) -> ExitCode {
    let Some(tier) = Tier::parse(tier) else {
        return fail(&format!("--tier {tier}: one of structural, timing, all"));
    };
    let host = match host_class.map(HostClass::parse) {
        Some(None) => return fail("--host-class names no reference table (reference-mac)"),
        Some(Some(c)) => Some(c),
        None => None,
    };
    match gate::run(h, tier, host, scale) {
        Ok(outcome) => {
            println!("{}", gate::summary(&outcome).render());
            if outcome.red() {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => fail(&format!("gate: {e}")),
    }
}

fn band_probe(h: &Harness) -> ExitCode {
    let mut code = ExitCode::SUCCESS;
    for set_by in floor::band::SetBy::ALL {
        match floor::band::clear_from_outside(*set_by) {
            Ok(c) => {
                let _ = h
                    .receipt
                    .fact("band-probe", &serde_json::to_string(&c).unwrap_or_default());
                println!("{}", kotae::Answer::found(&c).render());
            }
            Err(e) => {
                println!("{}", kotae::Answer::blind(e.to_string()).render());
                code = ExitCode::from(1);
            }
        }
    }
    code
}

fn dispatch(h: &Harness, cmd: &Cmd) -> ExitCode {
    match cmd {
        Cmd::Floors { suite, scale, band } => floors_cmd(h, suite, *scale, band),
        Cmd::Reproduce {
            flood_reps,
            series_secs,
        } => reproduce_cmd(h, *flood_reps, *series_secs),
        Cmd::Case { names, variant } => run_cases(h, names, variant),
        Cmd::Gate {
            tier,
            host_class,
            scale,
        } => gate_cmd(h, tier, host_class.as_deref(), *scale),
        Cmd::ReplayFile { path } => cases::replay_file(h, path)
            .map_or_else(|e| fail(&e.to_string()), |()| ExitCode::SUCCESS),
        Cmd::BandProbe => band_probe(h),
        Cmd::Status => {
            for p in gate::status(h) {
                println!("{} {} {}", p.pid, p.ppid, p.cmd);
            }
            ExitCode::SUCCESS
        }
        Cmd::Cleanup => {
            h.cleanup();
            ExitCode::SUCCESS
        }
        Cmd::Matrix | Cmd::FloorPeer { .. } | Cmd::BandExec { .. } => ExitCode::SUCCESS,
    }
}

fn note(h: &Harness, name: &str, r: std::io::Result<()>) -> bool {
    match r {
        Ok(()) => {
            h.log(&format!("case {name}: done"));
            true
        }
        Err(e) => {
            h.log(&format!("case {name}: FAILED: {e}"));
            false
        }
    }
}

const STANDALONE: &[&str] = &[
    "wire",
    "codec",
    "ptyraw",
    "startup",
    "restart",
    "allocations",
    "split",
    "audit",
    "stall",
    "twin",
    "muted-holder",
    "mute-sink",
    "store-lease-off",
    "window",
    "cursor-keys-via-rpc",
    "replay-controls",
    "quiet-window",
];

fn control_case(name: &str, (run, detail): (gate::ControlRun, String)) -> std::io::Result<()> {
    println!("control {name}: {} — {detail}", run.name());
    if run == gate::ControlRun::Reddened {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "control {name}: {}",
            run.name()
        )))
    }
}

fn standalone_case(h: &Harness, v: Variant, name: &str) -> std::io::Result<()> {
    match name {
        "wire" => cases::wire(h, Tag::Cells).map(drop),
        "codec" => cases::codec(h, Tag::Cells, cases::CODEC_SAMPLES).map(drop),
        "ptyraw" => cases::ptyraw(h, 3),
        "startup" => cases::startup(h, v, 5),
        "restart" => cases::restart(h, v, 8_388_608, 3),
        "allocations" => cases::allocations(h).map(drop),
        "split" => cases::split(h).map(drop),
        "audit" => gate::audit(h, &mut Vec::new()),
        "stall" => gate::stall(h, &mut Vec::new()),
        "twin" => gate::twin(h, &mut Vec::new()),
        "muted-holder" => {
            gate::muted_holders(h, &mut Vec::new());
            Ok(())
        }
        "mute-sink" => control_case(name, gate::mute_sink_control(h, &mut Vec::new())),
        "store-lease-off" => control_case(name, gate::store_lease_off_control(h, &mut Vec::new())),
        "window" => gate::window(h, &mut Vec::new()).map_err(std::io::Error::other),
        "cursor-keys-via-rpc" => control_case(name, gate::cursor_keys_control(h, &mut Vec::new())),
        "replay-controls" => gate::replay_controls(h, &mut Vec::new())
            .into_iter()
            .try_for_each(|(c, run, detail)| control_case(c.name(), (run, detail))),
        "quiet-window" => {
            let mut floors = tear_bench::verdict::FloorSet::default();
            let floor = gate::parked_floor(h, &mut floors);
            if let Err(e) = &floor {
                println!("case quiet-window: floor blind — {e}");
            }
            gate::quiet_window(h, &mut Vec::new()).map_err(std::io::Error::other)
        }
        other => Err(std::io::Error::other(format!("unknown case {other}"))),
    }
}

fn run_cases(h: &Harness, names: &str, variant: &str) -> ExitCode {
    let Some(v) = Variant::preset(variant) else {
        let names: Vec<&str> = Variant::PRESETS.iter().map(|(n, _)| *n).collect();
        return fail(&format!("--variant {variant}: one of {}", names.join(", ")));
    };
    let names: Vec<&str> = names.split(',').collect();
    let mut failed = false;
    for name in names.iter().filter(|n| STANDALONE.contains(n)) {
        failed |= !note(h, name, standalone_case(h, v, name));
    }
    let per_rig: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !STANDALONE.contains(n))
        .collect();
    if per_rig.is_empty() {
        return if failed {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        };
    }
    if let Err(e) = tear_bench::seam::apply_process_band(v.client_band) {
        h.log(&format!("client band {}: {e}", v.client_band.name()));
    }
    let mut daemon: Option<Daemon> = None;
    let opts = if per_rig.contains(&"tokened") {
        gate::tokened()
    } else {
        tear_bench::harness::daemon::Options::default()
    };
    let rig = if v == Variant::EMBEDDED {
        Rig::embedded("embedded")
    } else {
        match tear_bench::harness::isolated_rig_with(h, v, "case", &opts) {
            Ok((d, rig)) => {
                daemon = Some(d);
                rig
            }
            Err(e) => return fail(&format!("daemon: {e}")),
        }
    };
    for name in per_rig {
        let r = match name {
            "rpc" => cases::rpc(h, &rig),
            "echo" => cases::echo(h, &rig),
            "series" => cases::series(h, &rig, 15, std::time::Duration::from_millis(10)).map(drop),
            "contention" => cases::contention(h, &rig, 300),
            "flood" => cases::flood(h, &rig, 3).map(drop),
            "attach" => cases::attach(
                h,
                &rig,
                &[262_144, 1_048_576, 2_097_152, 4_194_304, 8_388_608],
                3,
            ),
            "cliff" => cases::cliff(
                h,
                &rig,
                &[0, 100, 250, 500, 750, 1_000, 1_500, 2_000, 3_000],
            ),
            "keyloss" => cases::keyloss(h, &rig, cases::KEYLOSS_ROWS, Tag::Cells).map(drop),
            "replay" => cases::replay(h, &rig, Tag::Cells).map(drop),
            "replay-snapshot" => {
                rig.configure_snapshot_replay(true);
                cases::replay(h, &rig, Tag::Cells).map(drop)
            }
            "keys" => cases::keys_at_depths(h, &rig, gate::KEYS_PER_DEPTH, Tag::Cells).map(drop),
            "tokened" => cases::tokened(h, &rig, cases::TOKENED_KEYS, Tag::Cells).map(drop),
            "paste" => cases::paste(h, &rig, cases::PASTE_BYTES, Tag::Cells).map(drop),
            "connect" => cases::connect(h, &rig, 30),
            other => Err(std::io::Error::other(format!("unknown case {other}"))),
        };
        failed |= !note(h, name, r);
    }
    rig.kill_all();
    drop(rig);
    if let Some(mut d) = daemon {
        d.stop(h);
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
