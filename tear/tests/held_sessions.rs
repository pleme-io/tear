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
        self.start_at(&self.socket())
    }

    fn start_at(&self, socket: &Path) -> (DaemonHandle, Arc<InProcess>) {
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
        inproc.set_socket_path(socket.to_path_buf());
        tear_daemon::durability::enable_and_restore(
            &inproc,
            &live.load().sessions,
            Some(socket),
            None,
        );
        let handle =
            tear_daemon::start_with_config(socket.to_path_buf(), Arc::clone(&inproc), live)
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
                return line
                    .trim_start_matches(&format!("{tag}-"))
                    .trim_end_matches("-end")
                    .to_string();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let screen = client
        .pane_snapshot(pane)
        .map(|s| s.to_text())
        .unwrap_or_default();
    panic!("{tag}: the shell never reported its pid; screen:\n{screen}");
}

fn new_session(client: &Client, name: &str) -> (SessionId, PaneId) {
    let sid = client
        .new_session_with_source_and_size(name, "/bin/sh", &[], SessionSource::Human, (80, 24))
        .unwrap();
    let pane = *client
        .get_session(sid)
        .unwrap()
        .panes
        .keys()
        .next()
        .unwrap();
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
    assert!(
        holder_pid(&place.holder_socket(pane)).is_some(),
        "the holder outlives the daemon"
    );

    let (handle, inproc) = place.start();
    let c = Client::connect(place.socket()).unwrap();
    let sessions = c.list_sessions().unwrap();
    assert_eq!(
        sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![sid],
        "the same session id comes back"
    );
    assert!(
        screen_has(&c, pane, "marker-42"),
        "the screen is rebuilt from the journal"
    );
    assert_eq!(
        shell_pid(&c, pane, "after"),
        before,
        "the very same shell process is still running"
    );

    c.kill_session(sid).unwrap();
    drop(c);
    stop(handle, inproc);
    let (handle, inproc) = place.start();
    let c = Client::connect(place.socket()).unwrap();
    assert!(
        c.list_sessions().unwrap().is_empty(),
        "a human end stays ended across a restart"
    );
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
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(hp),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let (handle, inproc) = place.start();
    let c = Client::connect(place.socket()).unwrap();
    let sessions = c.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, sid);
    assert!(
        screen_has(&c, pane, "old-life-4"),
        "scrollback survives the lost processes"
    );
    assert!(
        screen_has(&c, pane, "session resurrected"),
        "the new incarnation is announced"
    );
    assert_ne!(
        shell_pid(&c, pane, "after"),
        before,
        "a resurrection never claims the old process"
    );
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
        assert!(
            c.list_sessions().unwrap().is_empty(),
            "an unwatched exited session is reaped"
        );
    }
    stop(handle, inproc);
    let (handle, inproc) = place.start();
    let c = Client::connect(place.socket()).unwrap();
    assert!(c.list_sessions().unwrap().is_empty());
    drop(c);
    stop(handle, inproc);
}

#[test]
fn a_library_started_daemon_never_turns_durable_from_ambient_config() {
    let place = Place::new("library");
    let live = Arc::new(LiveConfig::default());
    live.replace(TearConfig {
        sessions: SessionsConfig {
            durability: SessionDurability::Held,
            store_dir: Some(place.store().to_string_lossy().into_owned()),
            ..SessionsConfig::default()
        },
        ..TearConfig::default()
    });
    let inproc = Arc::new(InProcess::new());
    let handle = tear_daemon::start_with_config(place.socket(), Arc::clone(&inproc), live)
        .expect("daemon start");
    assert!(
        inproc.durability().is_none(),
        "only the tear binary, which knows its own exe answers `hold`, may hold sessions"
    );
    let c = Client::connect(place.socket()).unwrap();
    let (sid, _) = new_session(&c, "embedded");
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !place.store().join("sessions").exists(),
        "a test or embedded daemon must never write into a session store"
    );
    c.kill_session(sid).unwrap();
    drop(c);
    stop(handle, inproc);
}

#[test]
fn a_lease_that_is_corrupt_or_cannot_be_taken_never_costs_the_held_store() {
    let place = Place::new("lease");
    let (handle, inproc) = place.start();
    let (sid, pane, before) = {
        let c = Client::connect(place.socket()).unwrap();
        let (sid, pane) = new_session(&c, "leased");
        let before = shell_pid(&c, pane, "before");
        (sid, pane, before)
    };
    stop(handle, inproc);

    std::fs::write(place.store().join("authority.json"), b"{not json").unwrap();
    let (handle, inproc) = place.start();
    assert!(
        inproc
            .durability()
            .is_some_and(|d| d.authority.is_declared()),
        "a corrupt lease is rewritten, and the daemon still declares an incarnation"
    );
    {
        let c = Client::connect(place.socket()).unwrap();
        assert_eq!(
            c.list_sessions()
                .unwrap()
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            vec![sid]
        );
        assert_eq!(
            shell_pid(&c, pane, "rewritten"),
            before,
            "the pane was adopted, not resurrected"
        );
    }
    stop(handle, inproc);

    let _ = std::fs::remove_file(place.store().join("authority.lock"));
    std::fs::create_dir(place.store().join("authority.lock")).unwrap();
    let (handle, inproc) = place.start();
    assert!(
        inproc
            .durability()
            .is_some_and(|d| !d.authority.is_declared()),
        "a lease that cannot be taken leaves the store held, declaring no incarnation"
    );
    let c = Client::connect(place.socket()).unwrap();
    assert_eq!(
        shell_pid(&c, pane, "undeclared"),
        before,
        "the pane was adopted, not resurrected"
    );
    c.kill_session(sid).unwrap();
    drop(c);
    stop(handle, inproc);
}

#[test]
fn two_daemons_on_one_store_leave_each_pane_one_authority_and_the_loser_ends_nothing() {
    let place = Place::new("twin");
    let (h1, first) = place.start();
    let c1 = Client::connect(place.socket()).unwrap();
    let (sid, pane) = new_session(&c1, "shared");
    c1.send_keys(pane, b"while :; do echo tick-$$; sleep 0.05; done\r")
        .unwrap();
    assert!(screen_has(&c1, pane, "tick-"));

    std::fs::create_dir_all(place.root.join("b")).unwrap();
    let second_socket = place.root.join("b").join("tear.sock");
    let (h2, second) = place.start_at(&second_socket);
    let c2 = Client::connect(&second_socket).unwrap();
    assert!(
        screen_has(&c2, pane, "tick-"),
        "the second daemon adopted the pane"
    );
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        let _ = c1.pane_snapshot(pane);
        let _ = c2.pane_snapshot(pane);
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        first.durability().is_none(),
        "the older daemon learnt it was displaced and released its held panes"
    );
    assert!(second.durability().is_some());
    assert!(
        c1.send_keys(pane, b"").is_err(),
        "the pane takes no input through the displaced daemon"
    );
    c1.kill_session(sid).unwrap();
    c2.send_keys(pane, b"\x03").unwrap();
    c2.send_keys(pane, b"echo after-$((7*6))\r").unwrap();
    assert!(
        screen_has(&c2, pane, "after-42"),
        "the displaced daemon's kill reached neither the shell nor its tombstone"
    );
    let log = std::fs::read_to_string(
        place
            .store()
            .join("sessions")
            .join(sid.to_string())
            .join("panes")
            .join(pane.to_string())
            .join("holder.log"),
    )
    .unwrap_or_default();
    assert_eq!(
        log.matches("tamotsu: attached from offset").count(),
        2,
        "{log}"
    );
    assert!(
        log.contains("incarnation 1 Displaced by incarnation 2"),
        "{log}"
    );

    c2.kill_session(sid).unwrap();
    drop(c1);
    drop(c2);
    stop(h2, second);
    stop(h1, first);
}
