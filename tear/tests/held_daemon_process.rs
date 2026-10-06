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
    fn new() -> Self {
        let root = PathBuf::from("/tmp").join(format!("tear-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("tear.yaml"), "sessions:\n  durability: held\n").unwrap();
        Self { root }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("tear.sock")
    }

    fn daemon(&self) -> Child {
        let child = Command::new(env!("CARGO_BIN_EXE_tear"))
            .args(["daemon", "--socket"])
            .arg(self.socket())
            .env("TEAR_CONFIG_FILE", self.root.join("tear.yaml"))
            .env("TEAR_STATE_DIR", self.root.join("state"))
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
    let place = Place::new();
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
