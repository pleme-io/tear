use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::compat::CompatCell;
use crate::matrix::Cell;
use crate::verdict::Verdict;

tear_types::closed_vocabulary! {
    Unit {
        Ns => "ns",
        Bytes => "bytes",
        Count => "count",
        Ratio => "ratio",
        NsPerMib => "ns-per-mib",
        Flag => "flag",
        PerSecond => "per-second",
    }
}

pub const SAMPLES: &str = "samples.tsv";
pub const VERDICTS: &str = "verdicts.tsv";
pub const RUNS: &str = "runs.tsv";
pub const PROBES: &str = "probes.tsv";

const SAMPLE_HEADER: &[&str] = &[
    "run", "bench", "variant", "case", "metric", "i", "name", "value", "unit", "ok", "detail",
];
const VERDICT_HEADER: &[&str] = &[
    "run", "case", "metric", "budget", "verdict", "n", "value", "limit", "detail",
];
const RUN_HEADER: &[&str] = &["run", "key", "value"];
const PROBE_HEADER: &[&str] = &["run", "role", "pid", "kind", "name", "value"];

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub bench: String,
    pub variant: String,
    pub cell: Option<Cell>,
    pub compat: Option<CompatCell>,
    pub i: usize,
    pub name: String,
    pub value: f64,
    pub unit: Unit,
    pub ok: bool,
    pub detail: String,
}

impl Sample {
    #[must_use]
    pub fn new(bench: &str, variant: &str, name: &str, i: usize, value: f64, unit: Unit) -> Self {
        Self {
            bench: bench.to_string(),
            variant: variant.to_string(),
            cell: None,
            compat: None,
            i,
            name: name.to_string(),
            value,
            unit,
            ok: true,
            detail: String::new(),
        }
    }

    #[must_use]
    pub fn cell(mut self, cell: Cell) -> Self {
        self.cell = Some(cell);
        self
    }

    #[must_use]
    pub fn compat(mut self, cell: CompatCell) -> Self {
        self.compat = Some(cell);
        self
    }

    #[must_use]
    pub fn ok(mut self, ok: bool) -> Self {
        self.ok = ok;
        self
    }

    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

struct Tsv {
    w: BufWriter<File>,
}

impl Tsv {
    fn open(path: &Path, header: &[&str]) -> io::Result<Self> {
        let fresh = !path.exists();
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        let mut w = BufWriter::new(f);
        if fresh {
            writeln!(w, "{}", header.join("\t"))?;
        }
        Ok(Self { w })
    }

    fn row(&mut self, cols: &[String]) -> io::Result<()> {
        let cols: Vec<String> = cols.iter().map(|c| clean(c)).collect();
        writeln!(self.w, "{}", cols.join("\t"))
    }
}

pub struct Receipt {
    pub run: String,
    pub dir: PathBuf,
    samples: Mutex<Tsv>,
    verdicts: Mutex<Tsv>,
    runs: Mutex<Tsv>,
    probes: Mutex<Tsv>,
}

impl Receipt {
    pub fn open(dir: &Path, run: Option<String>) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let run = run.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis().to_string())
                .unwrap_or_default()
        });
        Ok(Self {
            samples: Mutex::new(Tsv::open(&dir.join(SAMPLES), SAMPLE_HEADER)?),
            verdicts: Mutex::new(Tsv::open(&dir.join(VERDICTS), VERDICT_HEADER)?),
            runs: Mutex::new(Tsv::open(&dir.join(RUNS), RUN_HEADER)?),
            probes: Mutex::new(Tsv::open(&dir.join(PROBES), PROBE_HEADER)?),
            run,
            dir: dir.to_path_buf(),
        })
    }

    pub fn sample(&self, s: &Sample) -> io::Result<()> {
        let (case, metric) = match (s.cell, s.compat) {
            (Some(c), _) => (c.case.name(), c.metric.name().to_string()),
            (None, Some(c)) => (c.case_name(), c.check.name().to_string()),
            (None, None) => (String::new(), String::new()),
        };
        self.samples
            .lock()
            .map_err(|_| io::Error::other("samples lock poisoned"))?
            .row(&[
                self.run.clone(),
                s.bench.clone(),
                s.variant.clone(),
                case,
                metric,
                s.i.to_string(),
                s.name.clone(),
                fmt_value(s.value),
                s.unit.name().to_string(),
                s.ok.to_string(),
                s.detail.clone(),
            ])
    }

    pub fn samples(&self, all: &[Sample]) -> io::Result<()> {
        for s in all {
            self.sample(s)?;
        }
        Ok(())
    }

    pub fn verdict(&self, cell: Cell, v: &Verdict) -> io::Result<()> {
        self.verdict_row(
            cell.case.name(),
            cell.metric.name(),
            cell.budget().kind(),
            v,
        )
    }

    pub fn compat_verdict(&self, cell: CompatCell, v: &Verdict) -> io::Result<()> {
        self.verdict_row(cell.case_name(), cell.check.name(), cell.budget().kind(), v)
    }

    fn verdict_row(&self, case: String, metric: &str, kind: &str, v: &Verdict) -> io::Result<()> {
        let (n, value, limit, detail) = match v {
            Verdict::Within { n, value, limit } | Verdict::Over { n, value, limit } => (
                n.to_string(),
                fmt_value(*value),
                fmt_value(*limit),
                String::new(),
            ),
            Verdict::Pending { rung, n } => (
                n.to_string(),
                String::new(),
                String::new(),
                (*rung).to_string(),
            ),
            Verdict::Blind { reason } | Verdict::Errored { reason } => {
                ("0".into(), String::new(), String::new(), reason.clone())
            }
            Verdict::NotApplicable { why } => {
                ("0".into(), String::new(), String::new(), (*why).to_string())
            }
        };
        self.verdicts
            .lock()
            .map_err(|_| io::Error::other("verdicts lock poisoned"))?
            .row(&[
                self.run.clone(),
                case,
                metric.to_string(),
                kind.to_string(),
                v.name().to_string(),
                n,
                value,
                limit,
                detail,
            ])
    }

    pub fn fact(&self, key: &str, value: &str) -> io::Result<()> {
        self.runs
            .lock()
            .map_err(|_| io::Error::other("runs lock poisoned"))?
            .row(&[self.run.clone(), key.to_string(), value.to_string()])
    }

    pub fn probe(
        &self,
        role: &str,
        pid: &str,
        kind: &str,
        name: &str,
        value: u64,
    ) -> io::Result<()> {
        self.probes
            .lock()
            .map_err(|_| io::Error::other("probes lock poisoned"))?
            .row(&[
                self.run.clone(),
                role.to_string(),
                pid.to_string(),
                kind.to_string(),
                name.to_string(),
                value.to_string(),
            ])
    }

    pub fn flush(&self) -> io::Result<()> {
        for t in [&self.samples, &self.verdicts, &self.runs, &self.probes] {
            t.lock()
                .map_err(|_| io::Error::other("receipt lock poisoned"))?
                .w
                .flush()?;
        }
        Ok(())
    }
}

impl Drop for Receipt {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[must_use]
pub fn fmt_value(v: f64) -> String {
    if v.is_finite() {
        if v.fract() == 0.0 && v.abs() < 1e15 {
            format!("{v:.0}")
        } else {
            format!("{v:.3}")
        }
    } else {
        "NaN".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::{Case, Metric};

    #[test]
    fn a_receipt_writes_one_row_per_sample_with_its_header_once() {
        let dir = std::env::temp_dir().join(format!("tearbench-receipt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let r = Receipt::open(&dir, Some("t1".into())).unwrap();
            let s = Sample::new("flood", "bound", "frames", 0, 65_537.0, Unit::Count)
                .cell(Cell::new(Case::C9, Metric::Frames))
                .detail("a\tb");
            r.sample(&s).unwrap();
            r.sample(&s).unwrap();
            r.verdict(s.cell.unwrap(), &Verdict::Pending { rung: "R21", n: 1 })
                .unwrap();
        }
        {
            let r = Receipt::open(&dir, Some("t2".into())).unwrap();
            r.fact("k", "v").unwrap();
        }
        let samples = fs::read_to_string(dir.join(SAMPLES)).unwrap();
        let lines: Vec<&str> = samples.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("run\tbench"));
        assert_eq!(lines[1].split('\t').count(), SAMPLE_HEADER.len());
        assert!(lines[1].contains("C9\tframes\t0\tframes\t65537\tcount"));
        let verdicts = fs::read_to_string(dir.join(VERDICTS)).unwrap();
        assert!(verdicts.contains("C9\tframes\tpending\tpending\t1"));
        let _ = fs::remove_dir_all(&dir);
    }
}
