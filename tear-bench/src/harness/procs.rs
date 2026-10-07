use std::path::Path;
use std::process::Command;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub cmd: String,
}

#[must_use]
pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let l = line.trim_start();
            let (pid, tail) = l.split_once(char::is_whitespace)?;
            let tail = tail.trim_start();
            let (parent, cmd) = tail.split_once(char::is_whitespace).unwrap_or((tail, ""));
            Some(Proc {
                pid: pid.parse().ok()?,
                ppid: parent.parse().ok()?,
                cmd: cmd.trim().to_string(),
            })
        })
        .collect()
}

#[must_use]
pub fn ps() -> Vec<Proc> {
    Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=,ppid=,command="])
        .output()
        .map(|o| parse_ps(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

#[must_use]
pub fn descendants(all: &[Proc], roots: &[i32]) -> Vec<Proc> {
    let mut set: Vec<i32> = roots.to_vec();
    let mut out = Vec::new();
    let mut grew = true;
    while grew {
        grew = false;
        for p in all {
            if set.contains(&p.ppid) && !set.contains(&p.pid) {
                set.push(p.pid);
                out.push(p.clone());
                grew = true;
            }
        }
    }
    out
}

#[must_use]
pub fn holder_pane_dir(cmd: &str) -> Option<&Path> {
    let (program, after) = cmd.split_once(" hold --pane-dir ")?;
    if program.contains(char::is_whitespace) {
        return None;
    }
    let dir = after.split_once(" --socket ").map_or(after, |(d, _)| d);
    Some(Path::new(dir))
}

#[must_use]
pub fn holders_under(all: &[Proc], dir: &Path) -> Vec<Proc> {
    all.iter()
        .filter(|p| holder_pane_dir(&p.cmd).is_some_and(|d| d.starts_with(dir)))
        .cloned()
        .collect()
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Started {
    pub pid: i32,
    pub start: u64,
}

impl Started {
    #[must_use]
    pub fn of(pid: i32) -> Option<Self> {
        crate::seam::started_at(pid).map(|start| Self { pid, start })
    }

    #[must_use]
    pub fn alive(self) -> bool {
        crate::seam::started_at(self.pid) == Some(self.start)
    }

    #[must_use]
    pub fn row(self, what: &str) -> String {
        format!("{}\t{}\t{what}", self.pid, self.start)
    }

    #[must_use]
    pub fn parse_row(line: &str) -> Option<Self> {
        let mut cols = line.split('\t');
        let pid = cols.next()?.parse().ok()?;
        let start = cols.next()?.parse().ok()?;
        cols.next()?;
        Some(Self { pid, start })
    }
}

pub fn terminate(rows: &[Started], log: &dyn Fn(&str)) {
    for (sig, settle) in [
        (Signal::SIGTERM, Duration::from_millis(400)),
        (Signal::SIGKILL, Duration::from_millis(100)),
    ] {
        let mut sent = false;
        for p in rows {
            if p.alive() {
                log(&format!("{sig:?} pid {} (started {})", p.pid, p.start));
                let _ = kill(Pid::from_raw(p.pid), sig);
                sent = true;
            }
        }
        if !sent {
            return;
        }
        std::thread::sleep(settle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_rows_parse_by_key_and_keep_the_whole_command() {
        let text = "  10     1 /bin/tear daemon --socket run/x/tear.sock\n  11    10 /bin/tear hold --pane /r/state p\n bogus\n  12    11 /bin/sh -c stty raw\n";
        let all = parse_ps(text);
        assert_eq!(all.len(), 3);
        assert_eq!(all[1].cmd, "/bin/tear hold --pane /r/state p");
        let d = descendants(&all, &[10]);
        assert_eq!(d.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![11, 12]);
        assert!(holders_under(&all, Path::new("/r/state")).is_empty());
    }

    #[test]
    fn a_holder_is_ours_only_when_its_pane_dir_sits_under_the_run_root() {
        let text = "  20     1 /bin/tear hold --pane-dir /Users/op/.local/state/tear/p1 --socket /Users/op/.local/state/tear/p1/h.sock --cols 80 -- /bin/sh\n  21     1 /bin/tear hold --pane-dir /tmp/x/iso/held/state/tear/p2 --socket s --cols 80 -- /bin/sh\n  22     1 /bin/tear hold --pane-dir /tmp/x2/iso/held/state/tear/p3 --socket s -- /bin/sh\n  23     1 /bin/sh -c echo hold --pane-dir /tmp/x/iso\n";
        let all = parse_ps(text);
        let mine = holders_under(&all, Path::new("/tmp/x/iso"));
        assert_eq!(mine.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![21]);
        assert!(
            holders_under(&all, Path::new("/Users/op"))
                .iter()
                .all(|p| p.pid == 20)
        );
        assert_eq!(
            holder_pane_dir(&all[0].cmd),
            Some(Path::new("/Users/op/.local/state/tear/p1"))
        );
    }

    #[test]
    fn a_started_row_round_trips_and_a_reused_pid_is_not_ours() {
        let me = i32::try_from(std::process::id()).unwrap();
        let s = Started::of(me).unwrap();
        assert!(s.alive());
        assert_eq!(Started::parse_row(&s.row("tearbench")), Some(s));
        assert_eq!(Started::parse_row("123\tdaemon bound"), None);
        let reused = Started {
            pid: me,
            start: s.start + 1,
        };
        assert!(!reused.alive());
    }
}
