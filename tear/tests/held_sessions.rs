use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tamotsu::proto::{FromHolder, PROTO, ToHolder, read_from_holder, write_to_holder};
use tear_client::Client;
use tear_config::{LiveConfig, SessionDurability, SessionsConfig, TearConfig};
use tear_core::InProcess;
use tear_daemon::DaemonHandle;
use tear_types::{MultiplexerControl, PaneId, SessionId, SessionSource};

const WAIT: Duration = Duration::from_secs(15);

struct Place {
    root: PathBuf,
}

impl Place {
    fn new(tag: &str) -> Self {
        let root = PathBuf::from("/tmp").join(format!("tear-held-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("tear.sock")
    }

    fn store(&self) -> PathBuf {
        self.root.join("store")
    }

    fn start(&self) -> (DaemonHandle, Arc<InProcess>) {
        let live = Arc::new(LiveConfig::default());
        live.replace(TearConfig {
            sessions: SessionsConfig {
                durability: SessionDurability::Held,
                holder_program: Some(vec![env!("CARGO_BIN_EXE_tear").into(), "hold".into()]),
                store_dir: Some(self.store().to_string_lossy().into_owned()),
                ..SessionsConfig::default()
            },
            ..TearConfig::default()
        });
        let inproc = Arc::new(InProcess::new());
        let handle = tear_daemon::start_with_config(self.socket(), Arc::clone(&inproc), live)
            .expect("daemon start");
        std::thread::sleep(Duration::from_millis(50));
        (handle, inproc)
    }

    fn holder_socket(&self, pane: PaneId) -> PathBuf {
        self.root.join("h").join(format!("{pane}.sock"))
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(self.root.join("h")) {
            for e in entries.flatten() {
                if let Some(pid) = holder_pid(&e.path()) {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn holder_pid(socket: &Path) -> Option<i32> {
    let mut s = UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write_to_holder(&mut s, &ToHolder::Hello { proto: PROTO }).ok()?;
    match read_from_holder(&mut s).ok()? {
        FromHolder::Hello { pid, .. } => i32::try_from(pid).ok(),
        _ => None,
    }
}

fn screen_has(client: &Client, pane: PaneId, needle: &str) -> bool {
    let until = Instant::now() + WAIT;
    while Instant::now() < until {
        if let Ok(snap) = client.pane_snapshot(pane) {
            if snap.to_text().contains(needle) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn shell_pid(client: &Client, pane: PaneId, tag: &str) -> String {
    client
        .send_keys(pane, format!("echo {tag}-$$-end\r").as_bytes())
        .unwrap();
    let until = Instant::now() + WAIT;
    while Instant::now() < until {
        if let Ok(snap) = client.pane_snapshot(pane) {
            let text = snap.to_text();
            if let Some(line) = text
                .lines()
                .map(str::trim)
                .find(|l| l.starts_with(&format!("{tag}-")) && l.ends_with("-end"))
            {
                return line.trim_start_matches(&format!("{tag}-")).trim_end_matches("-end").to_string();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let screen = client.pane_snapshot(pane).map(|s| s.to_text()).unwrap_or_default();
    panic!("{tag}: the shell never reported its pid; screen:\n{screen}");
}

fn new_session(client: &Client, name: &str) -> (SessionId, PaneId) {
    let sid = client
        .new_session_with_source_and_size(name, "/bin/sh", &[], SessionSource::Human, (80, 24))
        .unwrap();
    let pane = *client.get_session(sid).unwrap().panes.keys().next().unwrap();
    (sid, pane)
}

fn stop(handle: DaemonHandle, inproc: Arc<InProcess>) {
    handle.stop();
    drop(inproc);
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
fn a_daemon_restart_reattaches_the_same_shell_with_its_screen() {
    let place = Place::new("restart");
    let (handle, inproc) = place.start();
    let (sid, pane) = {
        let c = Client::connect(place.socket()).unwrap();
        let (sid, pane) = new_session(&c, "kept");
        c.send_keys(pane, b"echo marker-$((6*7))\r").unwrap();
        assert!(screen_has(&c, pane, "marker-42"));
        (sid, pane)
    };
    let before = {
        let c = Client::connect(place.socket()).unwrap();
        shell_pid(&c, pane, "before")
    };
    stop(handle, inproc);
    assert!(holder_pid(&place.holder_socket(pane)).is_some(), "the holder outlives the daemon");

    let (handle, inproc) = place.start();
    let c = Client::connect(place.socket()).unwrap();
    let sessions = c.list_sessions().unwrap();
    assert_eq!(sessions.iter().map(|s| s.id).collect::<Vec<_>>(), vec![sid], "the same session id comes back");
    assert!(screen_has(&c, pane, "marker-42"), "the screen is rebuilt from the journal");
    assert_eq!(shell_pid(&c, pane, "after"), before, "the very same shell process is still running");

    c.kill_session(sid).unwrap();
    drop(c);
    stop(handle, inproc);
    let (handle, inproc) = place.start();
    let c = Client::connect(place.socket()).unwrap();
    assert!(c.list_sessions().unwrap().is_empty(), "a human end stays ended across a restart");
    drop(c);
    stop(handle, inproc);
}

#[test]
fn a_lost_holder_is_resurrected_in_place_on_the_next_start() {
    let place = Place::new("resurrect");
    let (handle, inproc) = place.start();
    let (sid, pane, before) = {
        let c = Client::connect(place.socket()).unwrap();
        let (sid, pane) = new_session(&c, "revived");
        c.send_keys(pane, b"echo old-life-$((2+2))\r").unwrap();
        assert!(screen_has(&c, pane, "old-life-4"));
        let before = shell_pid(&c, pane, "before");
        (sid, pane, before)
    };
    stop(handle, inproc);
    let hp = holder_pid(&place.holder_socket(pane)).expect("holder alive after daemon stop");
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(hp), nix::sys::signal::Signal::SIGKILL).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let (handle, inproc) = place.start();
    let c = Client::connect(place.socket()).unwrap();
    let sessions = c.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, sid);
    assert!(screen_has(&c, pane, "old-life-4"), "scrollback survives the lost processes");
    assert!(screen_has(&c, pane, "session resurrected"), "the new incarnation is announced");
    assert_ne!(shell_pid(&c, pane, "after"), before, "a resurrection never claims the old process");
    c.kill_session(sid).unwrap();
    drop(c);
    stop(handle, inproc);
}

#[test]
fn a_shell_that_exits_on_its_own_is_not_revived() {
    let place = Place::new("exited");
    let (handle, inproc) = place.start();
    {
        let c = Client::connect(place.socket()).unwrap();
        let (_sid, pane) = new_session(&c, "short");
        c.send_keys(pane, b"exit 0\r").unwrap();
        let until = Instant::now() + WAIT;
        while Instant::now() < until && !c.list_sessions().unwrap().is_empty() {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(c.list_sessions().unwrap().is_empty(), "an unwatched exited session is reaped");
    }
    stop(handle, inproc);
    let (handle, inproc) = place.start();
    let c = Client::connect(place.socket()).unwrap();
    assert!(c.list_sessions().unwrap().is_empty());
    drop(c);
    stop(handle, inproc);
}
