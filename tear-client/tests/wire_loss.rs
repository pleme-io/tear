use std::io::{self, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard};
use std::thread;
use std::time::Duration;

use tear_client::{Client, Transport};
use tear_types::wire::{
    Framed, INPUT_CHUNK_BYTES, MAX_FRAME_BYTES, Request, Response, WireError, read_frame, write_msg,
};
use tear_types::{
    BRACKETED_PASTE_CLOSE, BRACKETED_PASTE_OPEN, ControlError, DaemonHello, MultiplexerControl,
    PaneId,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Hello,
    Authenticate,
    Identify(u64),
    Snapshot,
    Keys(Vec<u8>),
    List,
    Other(String),
}

#[derive(Default)]
struct Script {
    refuse_keys_at: Option<usize>,
    token: Option<String>,
}

struct Fake {
    socket: PathBuf,
    seen: Arc<Mutex<Vec<(usize, Seen)>>>,
    open: Arc<Mutex<Vec<UnixStream>>>,
}

impl Fake {
    fn start(label: &str, script: Script) -> Self {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let socket = std::env::temp_dir().join(format!(
            "tear-r3-{label}-{}-{}.sock",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let open = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(script);
        let keys = Arc::new(AtomicUsize::new(0));
        let (seen_a, open_a) = (Arc::clone(&seen), Arc::clone(&open));
        thread::spawn(move || {
            for (conn, stream) in listener.incoming().enumerate() {
                let Ok(stream) = stream else { return };
                open_a.lock().unwrap().push(stream.try_clone().unwrap());
                let (seen, script, keys) =
                    (Arc::clone(&seen_a), Arc::clone(&script), Arc::clone(&keys));
                thread::spawn(move || serve(conn, stream, &seen, &script, &keys));
            }
        });
        Self { socket, seen, open }
    }

    fn sever(&self) {
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        thread::sleep(Duration::from_millis(50));
    }

    fn seen(&self) -> Vec<(usize, Seen)> {
        self.seen.lock().unwrap().clone()
    }

    fn on(&self, conn: usize) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|(c, _)| *c == conn)
            .map(|(_, s)| s)
            .collect()
    }

    fn keys(&self) -> Vec<(usize, Vec<u8>)> {
        self.seen()
            .into_iter()
            .filter_map(|(c, s)| match s {
                Seen::Keys(b) => Some((c, b)),
                _ => None,
            })
            .collect()
    }

    fn client(&self) -> Client {
        Client::connect(&self.socket).unwrap()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.sever();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn serve(
    conn: usize,
    mut stream: UnixStream,
    seen: &Mutex<Vec<(usize, Seen)>>,
    script: &Script,
    keys: &AtomicUsize,
) {
    let mut authed = script.token.is_none();
    loop {
        let req: Request = match read_frame(&mut stream) {
            Ok(Framed::Msg(r)) => r,
            _ => return,
        };
        let (what, resp) = match req {
            Request::Hello { .. } => (
                Seen::Hello,
                if authed {
                    Response::Hello(DaemonHello::for_this_build(&format!("fake-{conn}")))
                } else {
                    Response::Err(WireError::Rejected("authentication required".into()))
                },
            ),
            Request::Authenticate(t) => {
                authed = script.token.as_deref() == Some(t.as_str()) || script.token.is_none();
                (Seen::Authenticate, Response::Ok)
            }
            Request::IdentifyClient(id) => (Seen::Identify(id), Response::Ok),
            Request::ListSessions => (Seen::List, Response::Sessions(Vec::new())),
            Request::PaneSnapshot(_) => {
                seen.lock().unwrap().push((conn, Seen::Snapshot));
                let len = u32::try_from(MAX_FRAME_BYTES + 1).unwrap();
                let _ = stream.write_all(&len.to_be_bytes());
                let _ = stream.write_all(&vec![0u8; MAX_FRAME_BYTES + 1]);
                return;
            }
            Request::SendKeys { bytes, .. } => {
                let n = keys.fetch_add(1, Ordering::SeqCst) + 1;
                let resp = if script.refuse_keys_at == Some(n) {
                    Response::Err(WireError::Rejected("leader policy".into()))
                } else {
                    Response::Ok
                };
                (Seen::Keys(bytes), resp)
            }
            other => (Seen::Other(format!("{other:?}")), Response::Ok),
        };
        seen.lock().unwrap().push((conn, what));
        if write_msg(&mut stream, &resp).is_err() {
            return;
        }
    }
}

fn pane() -> PaneId {
    PaneId::from_seed("r3")
}

static CLIENT_FAULTS: RwLock<()> = RwLock::new(());

fn unarmed() -> RwLockReadGuard<'static, ()> {
    CLIENT_FAULTS.read().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(feature = "bench-probes")]
fn arming() -> std::sync::RwLockWriteGuard<'static, ()> {
    CLIENT_FAULTS
        .write()
        .unwrap_or_else(PoisonError::into_inner)
}

#[test]
fn an_oversized_reply_is_never_read_past_nor_replayed_and_the_next_key_lands() {
    let _faults = unarmed();
    let fake = Fake::start("oversized", Script::default());
    let client = fake.client();
    let err = client.pane_snapshot(pane()).unwrap_err();
    assert!(err.is_response_too_large(), "{err}");
    client.send_keys(pane(), b"k").unwrap();
    let snapshots: Vec<usize> = fake
        .seen()
        .into_iter()
        .filter(|(_, s)| *s == Seen::Snapshot)
        .map(|(c, _)| c)
        .collect();
    assert_eq!(
        snapshots,
        vec![0],
        "a refused frame is deterministic: never replayed"
    );
    assert_eq!(fake.keys(), vec![(1, b"k".to_vec())]);
    assert_eq!(fake.on(1), vec![Seen::Hello, Seen::Keys(b"k".to_vec())]);
}

#[cfg(feature = "bench-probes")]
#[test]
fn today_s_replay_policy_loses_the_key_after_an_oversized_reply() {
    use tear_types::probes::{Fault, arm};
    let _faults = arming();
    let fake = Fake::start("legacy", Script::default());
    let client = fake.client();
    arm(&[Fault::LegacyReplay]);
    let snapshot = client.pane_snapshot(pane());
    let keys = client.send_keys(pane(), b"k");
    arm(&[]);
    assert!(snapshot.is_err());
    assert!(keys.is_err(), "the reused connection was read mid-frame");
    assert!(fake.keys().is_empty(), "the key never reached the daemon");
}

#[test]
fn a_redial_re_identifies_and_re_probes() {
    let _faults = unarmed();
    let fake = Fake::start("redial", Script::default());
    let mut client = fake.client();
    client.identify_as(42).unwrap();
    assert_eq!(client.daemon().version(), Some("fake-0"));
    fake.sever();
    assert!(client.list_sessions().unwrap().is_empty());
    assert_eq!(
        fake.on(1),
        vec![Seen::Hello, Seen::Identify(42), Seen::List]
    );
    assert_eq!(client.daemon().version(), Some("fake-1"));
    fake.sever();
    assert!(
        client.send_keys(pane(), b"a").is_err(),
        "a write on a lost connection is never replayed"
    );
    client.send_keys(pane(), b"b").unwrap();
    assert_eq!(
        fake.on(2),
        vec![Seen::Hello, Seen::Identify(42), Seen::Keys(b"b".to_vec())]
    );
}

#[test]
fn a_write_that_sets_absolute_state_is_replayed_on_a_fresh_connection() {
    let _faults = unarmed();
    let fake = Fake::start("absolute", Script::default());
    let mut client = fake.client();
    client.identify_as(7).unwrap();
    fake.sever();
    client.pane_resize_absolute(pane(), 120, 40).unwrap();
    let redialed = fake.on(1);
    assert_eq!(&redialed[..2], &[Seen::Hello, Seen::Identify(7)]);
    assert!(
        matches!(&redialed[2..], [Seen::Other(req)] if req.starts_with("PaneResizeAbsolute")),
        "{redialed:?}"
    );
}

#[test]
fn input_travels_in_64_kib_chunks_back_to_back_on_one_connection() {
    let _faults = unarmed();
    let fake = Fake::start("chunks", Script::default());
    let client = fake.client();
    let input: Vec<u8> = (b'a'..=b'z').cycle().take(200 * 1024).collect();
    client.send_keys(pane(), &input).unwrap();
    let keys = fake.keys();
    let sizes: Vec<usize> = keys.iter().map(|(_, b)| b.len()).collect();
    assert_eq!(
        sizes,
        vec![
            INPUT_CHUNK_BYTES,
            INPUT_CHUNK_BYTES,
            INPUT_CHUNK_BYTES,
            8_192
        ]
    );
    assert!(keys.iter().all(|(c, _)| *c == 0));
    assert_eq!(
        keys.into_iter().flat_map(|(_, b)| b).collect::<Vec<u8>>(),
        input
    );
    assert_eq!(fake.on(0).len(), 1 + 4);
}

#[test]
fn a_chunk_refused_mid_way_reports_what_was_delivered() {
    let _faults = unarmed();
    let fake = Fake::start(
        "partial",
        Script {
            refuse_keys_at: Some(3),
            ..Script::default()
        },
    );
    let client = fake.client();
    let input = vec![b'x'; 200 * 1024];
    match client.send_keys(pane(), &input) {
        Err(ControlError::PartialInput {
            delivered,
            total,
            cause,
        }) => {
            assert_eq!((delivered, total), (2 * INPUT_CHUNK_BYTES, 200 * 1024));
            assert!(matches!(*cause, ControlError::Rejected(_)), "{cause}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        fake.keys().len(),
        3,
        "nothing is sent after the refused chunk"
    );
}

#[test]
fn a_bracketed_paste_that_fails_closes_its_own_bracket() {
    let _faults = unarmed();
    let fake = Fake::start(
        "paste",
        Script {
            refuse_keys_at: Some(3),
            ..Script::default()
        },
    );
    let client = fake.client();
    let body = vec![b'p'; 200 * 1024];
    assert!(client.send_paste(pane(), &body, true).is_err());
    let keys = fake.keys();
    assert_eq!(keys.len(), 4);
    assert!(keys[0].1.starts_with(BRACKETED_PASTE_OPEN));
    assert_eq!(keys[3].1, BRACKETED_PASTE_CLOSE);
}

#[test]
fn a_paste_refused_before_any_byte_landed_sends_no_stray_close() {
    let _faults = unarmed();
    let fake = Fake::start(
        "paste-refused",
        Script {
            refuse_keys_at: Some(1),
            ..Script::default()
        },
    );
    let client = fake.client();
    assert!(
        client
            .send_paste(pane(), &vec![b'p'; 200 * 1024], true)
            .is_err()
    );
    assert_eq!(fake.keys().len(), 1);
}

#[test]
fn a_whole_paste_arrives_framed_once() {
    let _faults = unarmed();
    let fake = Fake::start("paste-whole", Script::default());
    let client = fake.client();
    client.send_paste(pane(), b"hello", true).unwrap();
    client.send_paste(pane(), b"plain", false).unwrap();
    let keys: Vec<Vec<u8>> = fake.keys().into_iter().map(|(_, b)| b).collect();
    assert_eq!(
        keys,
        vec![b"\x1b[200~hello\x1b[201~".to_vec(), b"plain".to_vec()]
    );
}

#[test]
fn a_request_too_large_to_send_is_refused_before_any_io() {
    let _faults = unarmed();
    let fake = Fake::start("too-large-request", Script::default());
    let client = fake.client();
    let err = client
        .set_config_yaml("x".repeat(MAX_FRAME_BYTES))
        .unwrap_err();
    assert!(err.is_response_too_large(), "{err}");
    assert!(client.list_sessions().unwrap().is_empty());
    assert_eq!(fake.on(0), vec![Seen::Hello, Seen::List]);
}

#[test]
fn a_subscription_authenticates_and_probes_like_the_control_connection() {
    let _faults = unarmed();
    let fake = Fake::start(
        "tokened",
        Script {
            token: Some("secret".into()),
            ..Script::default()
        },
    );
    let client = Client::connect_transport_with_auth(
        Transport::Unix(fake.socket.clone()),
        Some("secret".into()),
    )
    .unwrap();
    assert_eq!(client.daemon().version(), Some("fake-0"));
    let _ = client.subscribe_pane_bytes(pane(), |_| {});
    let sub = fake.on(1);
    assert_eq!(
        &sub[..2],
        &[Seen::Authenticate, Seen::Hello],
        "the subscription's handshake: {sub:?}"
    );
}

#[test]
fn a_tokened_daemon_streams_bytes_to_a_subscriber() -> io::Result<()> {
    let _faults = unarmed();
    let socket = std::env::temp_dir().join(format!("tear-r3-auth-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    let inproc = Arc::new(tear_core::InProcess::new());
    let inproc_for_accept = Arc::clone(&inproc);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let inproc = Arc::clone(&inproc_for_accept);
            thread::spawn(move || {
                let _ = tear_daemon::serve_connection_with_auth(
                    stream,
                    inproc,
                    Arc::new(tear_config::LiveConfig::default()),
                    None,
                    Some("secret".into()),
                );
            });
        }
    });
    let client = Client::connect_transport_with_auth(
        Transport::Unix(socket.clone()),
        Some("secret".into()),
    )?;
    let sid = client.new_session("tokened", "/bin/sh").unwrap();
    let pane = *client
        .get_session(sid)
        .unwrap()
        .panes
        .keys()
        .next()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let handle = client
        .subscribe_pane_bytes(pane, move |b| {
            let _ = tx.send(b.to_vec());
        })
        .expect("a daemon that requires a token must stream to a client that has it");
    let marker = "TEAR_R3_TOKENED_7319";
    client
        .send_keys(pane, format!("printf '{marker}\\n'\n").as_bytes())
        .unwrap();
    let mut got = Vec::new();
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while !String::from_utf8_lossy(&got).contains(marker) && std::time::Instant::now() < until {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            got.extend(chunk);
        }
    }
    assert!(String::from_utf8_lossy(&got).contains(marker));
    handle.stop();
    client.kill_session(sid).unwrap();
    let _ = std::fs::remove_file(&socket);
    Ok(())
}
