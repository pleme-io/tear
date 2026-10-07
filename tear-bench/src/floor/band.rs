use std::io;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::matrix::Band;
use crate::seam;

tear_types::closed_vocabulary! {
    SetBy { Itself => "itself", Outside => "outside" }
}

#[derive(Clone, Debug, Serialize)]
pub struct Clearing {
    pub set_by: &'static str,
    pub pid: i32,
    pub before: Option<i32>,
    pub after: Option<i32>,
    pub ps_before: String,
    pub ps_after: String,
    pub threads_after: Vec<(String, i32)>,
    pub cleared: bool,
    pub error: Option<String>,
}

fn settle(child: &mut Child, pid: i32, want_bg: bool) -> Option<i32> {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let p = seam::task(pid).map(|t| t.priority);
        if p.is_some_and(|p| (p <= 4) == want_bg) || Instant::now() > until {
            return p;
        }
        if child.try_wait().ok().flatten().is_some() {
            return p;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn ps_priority(pid: i32) -> String {
    std::process::Command::new("ps")
        .args(["-o", "pri=", "-p", &pid.to_string()])
        .output()
        .map_or_else(
            |e| format!("unreadable: {e}"),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
        )
}

pub fn clear_from_outside(set_by: SetBy) -> io::Result<Clearing> {
    let me = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(&me);
    if set_by == SetBy::Itself {
        cmd.arg("band-exec")
            .arg(Band::Background.name())
            .arg("--")
            .arg(&me);
    }
    let mut child = cmd
        .args(["floor-peer", "idle", "--secs", "10"])
        .stdin(Stdio::null())
        .spawn()?;
    let pid = i32::try_from(child.id()).unwrap_or(-1);
    if set_by == SetBy::Outside {
        seam::set_background_of(pid, true)?;
    }
    let before = settle(&mut child, pid, true);
    let ps_before = ps_priority(pid);
    let error = seam::set_background_of(pid, false)
        .err()
        .map(|e| e.to_string());
    let after = settle(&mut child, pid, false);
    let ps_after = ps_priority(pid);
    let threads_after = seam::threads(pid)
        .into_iter()
        .map(|t| (t.name, t.current))
        .collect();
    let _ = child.kill();
    let _ = child.wait();
    Ok(Clearing {
        set_by: set_by.name(),
        pid,
        before,
        after,
        ps_before,
        ps_after,
        threads_after,
        cleared: before.is_some_and(|p| p <= 4) && after.is_some_and(|p| p > 4),
        error,
    })
}

pub fn idle_peer(args: &[String]) -> io::Result<()> {
    let secs = super::flag_u64(args, "--secs", 10)?;
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
