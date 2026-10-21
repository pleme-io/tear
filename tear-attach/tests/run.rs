use std::io::Write;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tear_attach::io::{IoError, PaneIo, Sink, Subscription};
use tear_attach::{Attach, AttachError, Bindings, Event, Exit, Start};
use tear_config::{AttachSizing, TearConfig};
use tear_daemon::testing::DaemonHarness;
use tear_types::{MultiplexerControl, PaneId, SessionId, SessionSource};

#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

struct Recorder {
    log: Mutex<Vec<String>>,
    carries: bool,
    alive: Mutex<bool>,
}

impl Recorder {
    fn new(carries: bool) -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(Vec::new()),
            carries,
            alive: Mutex::new(true),
        })
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn subscribed(&self) -> usize {
        self.log().iter().filter(|l| *l == "subscribe").count()
    }
}

impl PaneIo for Recorder {
    fn resize(&self, _: PaneId, c: u16, r: u16) -> Result<(), IoError> {
        self.log.lock().unwrap().push(format!("resize {c}x{r}"));
        Ok(())
    }
    fn subscribe(&self, _: PaneId, _: Sink) -> Result<Subscription, IoError> {
        self.log.lock().unwrap().push("subscribe".into());
        Ok(Box::new(()))
    }
    fn send(&self, _: PaneId, b: &[u8]) -> Result<(), IoError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("send {}", String::from_utf8_lossy(b)));
        Ok(())
    }
    fn stream_carries_replay(&self) -> bool {
        self.carries
    }
    fn snapshot_paint(&self, _: PaneId) -> Result<Vec<u8>, IoError> {
        self.log.lock().unwrap().push("snapshot".into());
        Ok(b"SNAP".to_vec())
    }
    fn pane_alive(&self, _: PaneId) -> Result<bool, IoError> {
        Ok(*self.alive.lock().unwrap())
    }
}

fn attach(control: Arc<dyn MultiplexerControl>, io: Arc<dyn PaneIo>, cfg: &TearConfig) -> Attach {
    Attach {
        control,
        io,
        bindings: Bindings::from_config(cfg).unwrap(),
        sizing: cfg.attach.sizing,
        shortcuts: cfg.attach.picker_shortcuts,
        shell: "/bin/sh".into(),
        tick: Duration::from_millis(50),
        clock: Box::new(|| 1_000_000),
    }
}

struct Running {
    tx: Sender<Event>,
    out: Out,
    join: JoinHandle<Result<Exit, AttachError>>,
}

fn start(a: Attach, start: Start, size: (u16, u16)) -> Running {
    let (tx, rx) = channel();
    let out = Out::default();
    let o = out.clone();
    let txc = tx.clone();
    let join = std::thread::spawn(move || {
        let mut o = o;
        a.run(start, size, &rx, &txc, &mut o)
    });
    Running { tx, out, join }
}

fn type_bytes(r: &Running, b: &[u8]) {
    r.tx.send(Event::Stdin(b.to_vec())).unwrap();
}

fn detach(r: &Running) {
    type_bytes(r, &[0x02, b'd']);
}

fn session(inproc: &tear_core::InProcess, name: &str) -> (SessionId, PaneId) {
    let id = inproc
        .new_session_with_source_and_size(name, "/bin/sh", &[], SessionSource::Human, (80, 24))
        .unwrap();
    let pane = inproc
        .get_session(id)
        .unwrap()
        .windows
        .values()
        .next()
        .unwrap()
        .active_pane;
    (id, pane)
}

fn inproc() -> Arc<tear_core::InProcess> {
    let i = Arc::new(tear_core::InProcess::new());
    i.set_spawn_env(tear_types::SpawnEnv::none());
    i
}

fn target(
    c: &Arc<tear_core::InProcess>,
    io: &Arc<Recorder>,
    cfg: &TearConfig,
    t: &str,
    size: (u16, u16),
) -> Running {
    start(
        attach(c.clone(), io.clone(), cfg),
        Start::Target(t.into()),
        size,
    )
}

#[test]
fn resize_then_attach_sizes_the_pane_before_the_replay() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(true);
    let r = target(&c, &io, &TearConfig::default(), "alpha", (60, 20));
    wait_for("subscribe", || io.subscribed() == 1);
    detach(&r);
    assert_eq!(r.join.join().unwrap().unwrap(), Exit::Detached);
    assert_eq!(io.log()[..2], ["resize 60x20", "subscribe"]);
}

#[test]
fn the_after_replay_knob_reverses_the_order() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(true);
    let mut cfg = TearConfig::default();
    cfg.attach.sizing = AttachSizing::AfterReplay;
    let r = target(&c, &io, &cfg, "alpha", (60, 20));
    wait_for("resize", || io.log().len() >= 2);
    detach(&r);
    r.join.join().unwrap().unwrap();
    assert_eq!(io.log()[..2], ["subscribe", "resize 60x20"]);
}

#[test]
fn an_old_daemon_gets_the_snapshot_painted_after_sizing() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(false);
    let r = target(&c, &io, &TearConfig::default(), "alpha", (60, 20));
    wait_for("snapshot", || io.log().contains(&"snapshot".to_owned()));
    detach(&r);
    r.join.join().unwrap().unwrap();
    assert_eq!(io.log()[..3], ["resize 60x20", "subscribe", "snapshot"]);
    assert!(r.out.text().contains("SNAP"));
}

#[test]
fn a_window_resize_reaches_the_pane() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(true);
    let r = target(&c, &io, &TearConfig::default(), "alpha", (60, 20));
    wait_for("subscribe", || io.subscribed() == 1);
    r.tx.send(Event::Resize(132, 43)).unwrap();
    wait_for("resize", || io.log().contains(&"resize 132x43".to_owned()));
    detach(&r);
    r.join.join().unwrap().unwrap();
}

#[test]
fn typed_bytes_reach_the_pane_but_the_detach_chord_does_not() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(true);
    let r = target(&c, &io, &TearConfig::default(), "alpha", (60, 20));
    wait_for("subscribe", || io.subscribed() == 1);
    type_bytes(&r, b"ls\r");
    type_bytes(&r, &[0x02, b'd']);
    r.join.join().unwrap().unwrap();
    let log = io.log();
    assert!(log.contains(&"send ls\r".to_owned()), "{log:?}");
    assert!(!log.iter().any(|l| l.contains('\u{2}')), "{log:?}");
}

#[test]
fn detaching_leaves_the_session_and_its_shell_running() {
    let c = inproc();
    let (id, pane) = session(&c, "alpha");
    let io = Recorder::new(true);
    let r = target(&c, &io, &TearConfig::default(), "alpha", (60, 20));
    wait_for("subscribe", || io.subscribed() == 1);
    detach(&r);
    r.join.join().unwrap().unwrap();
    assert!(c.get_session(id).is_ok());
    assert!(matches!(
        c.get_pane(pane).unwrap().state,
        tear_types::PaneState::Running
    ));
    assert!(r.out.text().contains("\x1b[?1049l"));
}

#[test]
fn the_picker_key_opens_the_overlay_and_escape_returns_to_the_same_pane() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(true);
    let r = target(&c, &io, &TearConfig::default(), "alpha", (60, 20));
    wait_for("subscribe", || io.subscribed() == 1);
    type_bytes(&r, &[0x13]);
    wait_for("picker", || r.out.text().contains("tear sessions"));
    type_bytes(&r, &[0x1b]);
    wait_for("second subscribe", || io.subscribed() == 2);
    detach(&r);
    assert_eq!(r.join.join().unwrap().unwrap(), Exit::Detached);
}

#[test]
fn the_picker_overlay_hops_to_another_session() {
    let c = inproc();
    session(&c, "alpha");
    session(&c, "bravo");
    let io = Recorder::new(true);
    let r = target(&c, &io, &TearConfig::default(), "alpha", (60, 20));
    wait_for("subscribe", || io.subscribed() == 1);
    type_bytes(&r, &[0x13]);
    wait_for("picker", || r.out.text().contains("bravo"));
    type_bytes(&r, b"brav");
    type_bytes(&r, b"\r");
    wait_for("hop", || io.subscribed() == 2);
    detach(&r);
    r.join.join().unwrap().unwrap();
    assert_eq!(io.log().iter().filter(|l| *l == "resize 60x20").count(), 2);
}

#[test]
fn a_disabled_picker_key_reaches_the_pane_instead() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(true);
    let mut cfg = TearConfig::default();
    cfg.attach.picker_enabled = false;
    let r = target(&c, &io, &cfg, "alpha", (60, 20));
    wait_for("subscribe", || io.subscribed() == 1);
    type_bytes(&r, &[0x13]);
    wait_for("forwarded", || {
        io.log().iter().any(|l| l.starts_with("send "))
    });
    detach(&r);
    r.join.join().unwrap().unwrap();
    assert!(!r.out.text().contains("tear sessions"));
}

#[test]
fn standalone_picker_quits_on_escape_without_touching_any_pane() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(true);
    let r = start(
        attach(c.clone(), io.clone(), &TearConfig::default()),
        Start::Picker,
        (60, 20),
    );
    wait_for("picker", || r.out.text().contains("alpha"));
    type_bytes(&r, &[0x1b]);
    assert_eq!(r.join.join().unwrap().unwrap(), Exit::Quit);
    assert!(io.log().is_empty());
}

#[test]
fn the_picker_creates_a_session_with_n_and_attaches_to_it() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(true);
    let r = start(
        attach(c.clone(), io.clone(), &TearConfig::default()),
        Start::Picker,
        (70, 22),
    );
    wait_for("picker", || r.out.text().contains("alpha"));
    type_bytes(&r, b"n");
    type_bytes(&r, b"work\r");
    wait_for("attach", || io.subscribed() == 1);
    detach(&r);
    r.join.join().unwrap().unwrap();
    let names: Vec<String> = c
        .list_sessions()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(names.contains(&"work".to_owned()), "{names:?}");
    assert_eq!(io.log()[0], "resize 70x22");
}

#[test]
fn killing_from_the_picker_needs_the_confirmation() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(true);
    let r = start(
        attach(c.clone(), io.clone(), &TearConfig::default()),
        Start::Picker,
        (70, 22),
    );
    wait_for("picker", || r.out.text().contains("alpha"));
    type_bytes(&r, b"x");
    wait_for("prompt", || r.out.text().contains("kill `alpha`?"));
    type_bytes(&r, b"n");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(c.list_sessions().unwrap().len(), 1);
    type_bytes(&r, b"x");
    type_bytes(&r, b"y");
    wait_for("gone", || c.list_sessions().unwrap().is_empty());
    type_bytes(&r, &[0x1b]);
    r.join.join().unwrap().unwrap();
}

fn run_once(control: Arc<dyn MultiplexerControl>, start: Start) -> Result<Exit, AttachError> {
    let io = Recorder::new(true);
    let (tx, rx) = channel();
    let mut out = Out::default();
    attach(control, io, &TearConfig::default()).run(start, (80, 24), &rx, &tx, &mut out)
}

#[test]
fn an_unknown_target_is_a_typed_error() {
    let c = inproc();
    session(&c, "alpha");
    let err = run_once(c, Start::Target("nope".into())).unwrap_err();
    assert!(matches!(err, AttachError::TargetNotFound(_)), "{err}");
}

#[test]
fn an_ambiguous_prefix_names_the_candidates() {
    let c = inproc();
    session(&c, "work-a");
    session(&c, "work-b");
    match run_once(c, Start::Target("work".into())).unwrap_err() {
        AttachError::TargetAmbiguous { matches, .. } => assert_eq!(matches.len(), 2),
        other => panic!("{other}"),
    }
}

#[test]
fn a_unique_prefix_attaches() {
    let c = inproc();
    session(&c, "work-a");
    session(&c, "play");
    let io = Recorder::new(true);
    let r = target(&c, &io, &TearConfig::default(), "wo", (80, 24));
    wait_for("subscribe", || io.subscribed() == 1);
    detach(&r);
    r.join.join().unwrap().unwrap();
}

#[test]
fn an_unreachable_daemon_is_a_typed_state_not_a_panic() {
    let dead: Arc<dyn MultiplexerControl> = Arc::new(tear_picker::testing::Unreachable);
    for s in [Start::Picker, Start::Target("x".into())] {
        let err = run_once(dead.clone(), s).unwrap_err();
        assert!(matches!(err, AttachError::DaemonUnreachable(_)), "{err}");
    }
}

#[test]
fn a_pane_that_ends_while_attached_exits_cleanly() {
    let c = inproc();
    session(&c, "alpha");
    let io = Recorder::new(true);
    let r = target(&c, &io, &TearConfig::default(), "alpha", (80, 24));
    wait_for("subscribe", || io.subscribed() == 1);
    *io.alive.lock().unwrap() = false;
    assert_eq!(r.join.join().unwrap().unwrap(), Exit::PaneEnded);
}

#[test]
fn a_hangup_on_the_input_detaches_without_ending_the_pane() {
    let c = inproc();
    let (_id, pane) = session(&c, "alpha");
    let io = Recorder::new(true);
    let r = target(&c, &io, &TearConfig::default(), "alpha", (80, 24));
    wait_for("subscribe", || io.subscribed() == 1);
    r.tx.send(Event::StdinClosed).unwrap();
    assert_eq!(r.join.join().unwrap().unwrap(), Exit::InputClosed);
    assert!(c.get_pane(pane).is_ok());
}

mod daemon {
    use super::*;
    use tear_client::Client;

    fn make(h: &DaemonHarness, name: &str) -> PaneId {
        let c = Client::connect(h.socket()).unwrap();
        let id = c
            .new_session_with_source_and_size(name, "/bin/sh", &[], SessionSource::Human, (80, 24))
            .unwrap();
        c.get_session(id)
            .unwrap()
            .windows
            .values()
            .next()
            .unwrap()
            .active_pane
    }

    fn attach_to(h: &DaemonHarness, target: &str, size: (u16, u16)) -> Running {
        let client = Arc::new(Client::connect(h.socket()).unwrap());
        start(
            attach(client.clone(), client, &TearConfig::default()),
            Start::Target(target.into()),
            size,
        )
    }

    #[test]
    fn type_a_command_see_output_detach_reattach_and_see_the_history() {
        let h = DaemonHarness::new("attach-e2e");
        let pane = make(&h, "work");
        let first = attach_to(&h, "work", (100, 30));
        wait_for("prompt output", || !first.out.text().is_empty());
        type_bytes(&first, b"echo MARK$((20+22))\r");
        wait_for("command output", || first.out.text().contains("MARK42"));
        detach(&first);
        assert_eq!(first.join.join().unwrap().unwrap(), Exit::Detached);

        let c = Client::connect(h.socket()).unwrap();
        assert!(matches!(
            c.get_pane(pane).unwrap().state,
            tear_types::PaneState::Running
        ));

        let second = attach_to(&h, "work", (100, 30));
        wait_for("replayed history", || second.out.text().contains("MARK42"));
        type_bytes(&second, b"echo AGAIN$((1+1))\r");
        wait_for("second command", || second.out.text().contains("AGAIN2"));
        detach(&second);
        second.join.join().unwrap().unwrap();
    }

    #[test]
    fn the_pane_is_sized_to_the_client_and_follows_resizes() {
        let h = DaemonHarness::new("attach-size");
        let pane = make(&h, "work");
        let r = attach_to(&h, "work", (50, 15));
        wait_for("output", || !r.out.text().is_empty());
        let c = Client::connect(h.socket()).unwrap();
        assert_eq!(c.get_pane(pane).unwrap().size_cells, (50, 15));
        r.tx.send(Event::Resize(90, 33)).unwrap();
        wait_for("resize", || {
            c.get_pane(pane).unwrap().size_cells == (90, 33)
        });
        detach(&r);
        r.join.join().unwrap().unwrap();
    }

    #[test]
    fn a_second_client_does_not_end_the_first_and_the_last_attacher_sets_the_size() {
        let h = DaemonHarness::new("attach-multi");
        let pane = make(&h, "work");
        let first = attach_to(&h, "work", (100, 30));
        wait_for("first output", || !first.out.text().is_empty());
        let second = attach_to(&h, "work", (70, 20));
        wait_for("second output", || !second.out.text().is_empty());
        let c = Client::connect(h.socket()).unwrap();
        wait_for("last attacher sizes", || {
            c.get_pane(pane).unwrap().size_cells == (70, 20)
        });
        detach(&second);
        second.join.join().unwrap().unwrap();
        type_bytes(&first, b"echo ALIVE$((3+4))\r");
        wait_for("first still streams", || {
            first.out.text().contains("ALIVE7")
        });
        detach(&first);
        assert_eq!(first.join.join().unwrap().unwrap(), Exit::Detached);
    }

    #[test]
    fn both_attached_clients_see_the_same_output() {
        let h = DaemonHarness::new("attach-both");
        make(&h, "work");
        let first = attach_to(&h, "work", (80, 24));
        let second = attach_to(&h, "work", (80, 24));
        wait_for("both subscribed", || {
            !first.out.text().is_empty() && !second.out.text().is_empty()
        });
        type_bytes(&first, b"echo BOTH$((5+5))\r");
        wait_for("first sees", || first.out.text().contains("BOTH10"));
        wait_for("second sees", || second.out.text().contains("BOTH10"));
        detach(&first);
        detach(&second);
        first.join.join().unwrap().unwrap();
        second.join.join().unwrap().unwrap();
    }

    #[test]
    fn a_locked_pane_refuses_input_visibly() {
        let h = DaemonHarness::new("attach-locked");
        let pane = make(&h, "work");
        let c = Client::connect(h.socket()).unwrap();
        c.set_input_policy(pane, tear_types::InputPolicy::Locked)
            .unwrap();
        let r = attach_to(&h, "work", (80, 24));
        wait_for("output", || !r.out.text().is_empty());
        type_bytes(&r, b"x");
        wait_for("notice", || r.out.text().contains("input refused"));
        detach(&r);
        r.join.join().unwrap().unwrap();
    }
}
