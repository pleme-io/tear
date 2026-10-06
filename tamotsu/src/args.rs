use std::path::PathBuf;

use makimono::JournalBounds;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldArgs {
    pub pane_dir: PathBuf,
    pub socket: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
    pub resurrected: bool,
    pub bounds: JournalBounds,
    pub program: String,
    pub args: Vec<String>,
}

impl HoldArgs {
    #[must_use]
    pub fn to_argv(&self) -> Vec<String> {
        let mut v = vec![
            "--pane-dir".into(),
            self.pane_dir.to_string_lossy().into_owned(),
            "--socket".into(),
            self.socket.to_string_lossy().into_owned(),
            "--cols".into(),
            self.cols.to_string(),
            "--rows".into(),
            self.rows.to_string(),
            "--max-bytes".into(),
            self.bounds.max_bytes.to_string(),
            "--segment-bytes".into(),
            self.bounds.segment_bytes.to_string(),
            "--fsync-ms".into(),
            self.bounds.fsync_interval_ms.to_string(),
        ];
        if let Some(cwd) = &self.cwd {
            v.push("--cwd".into());
            v.push(cwd.clone());
        }
        if self.resurrected {
            v.push("--resurrected".into());
        }
        v.push("--".into());
        v.push(self.program.clone());
        v.extend(self.args.iter().cloned());
        v
    }

    pub fn parse(argv: &[String]) -> anyhow::Result<Self> {
        let mut pane_dir = None;
        let mut socket = None;
        let mut cols = 80;
        let mut rows = 24;
        let mut cwd = None;
        let mut resurrected = false;
        let mut bounds = JournalBounds::default();
        let mut it = argv.iter();
        let mut rest: Vec<String> = Vec::new();
        while let Some(a) = it.next() {
            let mut value = |name: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
            };
            match a.as_str() {
                "--pane-dir" => pane_dir = Some(PathBuf::from(value("--pane-dir")?)),
                "--socket" => socket = Some(PathBuf::from(value("--socket")?)),
                "--cols" => cols = value("--cols")?.parse()?,
                "--rows" => rows = value("--rows")?.parse()?,
                "--cwd" => cwd = Some(value("--cwd")?),
                "--max-bytes" => bounds.max_bytes = value("--max-bytes")?.parse()?,
                "--segment-bytes" => bounds.segment_bytes = value("--segment-bytes")?.parse()?,
                "--fsync-ms" => bounds.fsync_interval_ms = value("--fsync-ms")?.parse()?,
                "--resurrected" => resurrected = true,
                "--" => {
                    rest = it.cloned().collect();
                    break;
                }
                other => anyhow::bail!("unknown argument `{other}`"),
            }
        }
        let mut rest = rest.into_iter();
        let program = rest
            .next()
            .ok_or_else(|| anyhow::anyhow!("no program after `--`"))?;
        Ok(Self {
            pane_dir: pane_dir.ok_or_else(|| anyhow::anyhow!("--pane-dir is required"))?,
            socket: socket.ok_or_else(|| anyhow::anyhow!("--socket is required"))?,
            cols,
            rows,
            cwd,
            resurrected,
            bounds,
            program,
            args: rest.collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HoldArgs {
        HoldArgs {
            pane_dir: "/s/panes/p".into(),
            socket: "/r/h/p.sock".into(),
            cols: 132,
            rows: 43,
            cwd: Some("/home/x/code".into()),
            resurrected: true,
            bounds: JournalBounds {
                max_bytes: 1 << 20,
                segment_bytes: 1 << 16,
                fsync_interval_ms: 250,
            },
            program: "/bin/zsh".into(),
            args: vec!["-l".into(), "--".into(), "--cols".into()],
        }
    }

    #[test]
    fn argv_round_trips_including_hyphenated_child_args() {
        let a = sample();
        assert_eq!(HoldArgs::parse(&a.to_argv()).unwrap(), a);
        let mut b = sample();
        b.cwd = None;
        b.resurrected = false;
        b.args.clear();
        assert_eq!(HoldArgs::parse(&b.to_argv()).unwrap(), b);
    }

    #[test]
    fn missing_required_fields_are_refused() {
        assert!(HoldArgs::parse(&["--".into(), "/bin/sh".into()]).is_err());
        assert!(
            HoldArgs::parse(&[
                "--pane-dir".into(),
                "/x".into(),
                "--socket".into(),
                "/y".into()
            ])
            .is_err()
        );
        assert!(HoldArgs::parse(&["--bogus".into()]).is_err());
    }
}
