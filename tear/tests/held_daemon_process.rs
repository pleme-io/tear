use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tear_client::Client;
use tear_types::{MultiplexerControl, PaneId, SessionSource};

const WAIT: Duration = Duration::from_secs(15);

struct Place {
    root: PathBuf,
}

impl Place {
    fn new(tag: &str) -> Self {
        let root = PathBuf::from("/tmp").join(format!("tear-proc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("tear.yaml"), "sessions:\n  durability: held\n").unwrap();
        Self { root }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("tear.sock")
    }

    fn daemon(&self) -> Child {
        self.daemon_with(&[])
    }

    fn daemon_with(&self, env: &[(&str, &str)]) -> Child {
        let child = Command::new(env!("CARGO_BIN_EXE_tear"))
            .args(["daemon", "--socket"])
            .arg(self.socket())
            .env("TEAR_CONFIG_FILE", self.root.join("tear.yaml"))
            .env("TEAR_STATE_DIR", self.root.join("state"))
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tear daemon");
        let until = Instant::now() + WAIT;
        while Instant::now() < until {
            if Client::connect(self.socket()).is_ok() {
                return child;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("tear daemon never answered");
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        if let Ok(c) = Client::connect(self.socket()) {
            for s in c.list_sessions().unwrap_or_default() {
                let _ = c.kill_session(s.id);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn shell_pid(client: &Client, pane: PaneId, tag: &str) -> String {
    client
        .send_keys(pane, format!("echo {tag}-$$-end\r").as_bytes())
        .unwrap();
    let until = Instant::now() + WAIT;
    while Instant::now() < until {
        if let Ok(snap) = client.pane_snapshot(pane) {
            if let Some(line) = snap
                .to_text()
                .lines()
                .map(str::trim)
                .find(|l| l.starts_with(&format!("{tag}-")) && l.ends_with("-end"))
            {
                return line
                    .trim_start_matches(&format!("{tag}-"))
                    .trim_end_matches("-end")
                    .to_string();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{tag}: the shell never reported its pid");
}

fn kill9(child: &mut Child) {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).unwrap();
    let _ = child.wait();
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

#[test]
fn a_sigkilled_daemon_process_comes_back_to_the_same_shells() {
    let place = Place::new("sigkill");
    let mut daemon = place.daemon();
    let (sid, pane, before) = {
        let c = Client::connect(place.socket()).unwrap();
        let sid = c
            .new_session_with_source_and_size(
                "proc",
                "/bin/sh",
                &[],
                SessionSource::Human,
                (80, 24),
            )
            .unwrap();
        let pane = *c.get_session(sid).unwrap().panes.keys().next().unwrap();
        let before = shell_pid(&c, pane, "before");
        (sid, pane, before)
    };
    assert!(
        exists(&place.root.join("h").join(format!("{pane}.sock"))),
        "the pane is held by a `tear hold` process beside the daemon socket"
    );

    kill9(&mut daemon);
    let mut daemon = place.daemon();
    let c = Client::connect(place.socket()).unwrap();
    assert_eq!(
        c.list_sessions()
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect::<Vec<_>>(),
        vec![sid]
    );
    assert_eq!(shell_pid(&c, pane, "after"), before);
    c.kill_session(sid).unwrap();
    drop(c);
    kill9(&mut daemon);
}

#[cfg(feature = "bench-probes")]
fn signal(child: &Child, sig: nix::sys::signal::Signal) {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
    nix::sys::signal::kill(pid, sig).unwrap();
}

#[cfg(feature = "bench-probes")]
fn journaled(holder: &Path) -> Option<u64> {
    use tamotsu::proto::{FromHolder, PROTO, ToHolder, read_from_holder, write_to_holder};
    let mut s = std::os::unix::net::UnixStream::connect(holder).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write_to_holder(&mut s, &ToHolder::Hello { proto: PROTO }).ok()?;
    write_to_holder(&mut s, &ToHolder::Status).ok()?;
    loop {
        if let FromHolder::Status(st) = read_from_holder(&mut s).ok()? {
            return Some(st.end);
        }
    }
}

#[cfg(feature = "bench-probes")]
#[test]
fn a_wire_snapshot_recovers_a_pane_whose_holder_left_it_mute() {
    let place = Place::new("mute");
    let mut daemon = place.daemon_with(&[(tear_types::probe_env::FAULTS, "mute-sink")]);
    let c = Client::connect(place.socket()).unwrap();
    let sid = c
        .new_session_with_source_and_size("mute", "/bin/sh", &[], SessionSource::Human, (80, 24))
        .unwrap();
    let pane = *c.get_session(sid).unwrap().panes.keys().next().unwrap();
    shell_pid(&c, pane, "ready");
    c.send_keys(
        pane,
        b"sleep 1; head -c 3000000 /dev/zero | tr '\\0' '\\r'; echo; echo flood-$((6*7))-done\r",
    )
    .unwrap();
    signal(&daemon, nix::sys::signal::Signal::SIGSTOP);
    let holder = place.root.join("h").join(format!("{pane}.sock"));
    let stopped = Instant::now() + Duration::from_secs(60);
    while Instant::now() < stopped && journaled(&holder).is_none_or(|end| end < 3_000_000) {
        std::thread::sleep(Duration::from_millis(100));
    }
    signal(&daemon, nix::sys::signal::Signal::SIGCONT);
    let until = Instant::now() + Duration::from_secs(30);
    let mut seen = false;
    while Instant::now() < until {
        if c.pane_snapshot(pane)
            .is_ok_and(|s| s.to_text().contains("flood-42-done"))
        {
            seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let log = std::fs::read_to_string(
        place
            .root
            .join("state/tear/makimono/sessions")
            .join(sid.to_string())
            .join("panes")
            .join(pane.to_string())
            .join("holder.log"),
    )
    .unwrap_or_default();
    let _ = c.kill_session(sid);
    drop(c);
    kill9(&mut daemon);
    assert!(
        seen,
        "a holder that kept a failed sink's socket open muted the pane, and the daemon's wire snapshots never recovered it through Status.end: {log}"
    );
    assert!(
        !log.contains("shut down both ways"),
        "the fault left the failed sink open, so only Status.end could recover the pane: {log}"
    );
    assert_eq!(
        log.matches("tamotsu: attached from offset").count(),
        2,
        "the pane was muted and re-attached by offset, once: {log}"
    );
}
