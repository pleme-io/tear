use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use makimono::{JournalBounds, PaneDir, Store};
use tamotsu::proto::{
    FromHolder, HolderStatus, PROTO, ToHolder, read_to_holder, write_from_holder,
};
use tamotsu::{Authority, HeldPty, OnBytes};

#[allow(dead_code)]
mod common;

use common::*;

const FLOOD: &[u8] = b"yes | head -c 6000000; echo done-$((2+3))\n";

fn end_holder(rv: &tamotsu::Revival, mut child: std::process::Child) {
    if let Ok(mut s) = UnixStream::connect(&rv.args.socket) {
        let _ = tamotsu::proto::write_to_holder(&mut s, &ToHolder::Hello { proto: PROTO });
        let _ = tamotsu::proto::write_to_holder(&mut s, &ToHolder::End);
    }
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn drained(raw: &mut Raw, deadline: Duration) -> Frame {
    raw.until_quiet_or_eof(Duration::from_millis(500), deadline)
}

#[test]
fn a_sink_that_stops_reading_is_shut_down_both_ways_and_resumes_by_offset() {
    let t = Scratch::new("shut");
    let rv = revival(&t.0);
    let holder = spawn_holder(&rv);
    let mut stalled = Raw::attached(&rv.args.socket, 0, Some(1));
    stalled.write(FLOOD);
    thread::sleep(Duration::from_secs(4));
    let until = Instant::now() + Duration::from_secs(30);
    let mut ended = false;
    while Instant::now() < until {
        if let Frame::Eof = stalled.frame(Duration::from_secs(3)) {
            ended = true;
            break;
        }
    }
    assert!(
        ended,
        "a sink that stopped reading must end the daemon's stream, not leave it open and silent"
    );
    assert!(
        holder_log(&rv).contains("its connection is shut down both ways"),
        "{}",
        holder_log(&rv)
    );
    let mut resumed = Raw::attached(&rv.args.socket, stalled.next, Some(1));
    assert!(
        resumed.until_text("done-5", Duration::from_secs(60)),
        "re-attaching by offset delivers the rest"
    );
    assert!(matches!(
        drained(&mut resumed, Duration::from_secs(10)),
        Frame::Quiet
    ));
    assert_eq!(
        resumed.next,
        status_end(&rv.args.socket),
        "every journaled byte was delivered exactly once, in order"
    );
    assert_eq!(attaches(&rv), 2);
    end_holder(&rv, holder);
}

#[test]
fn a_stalled_daemon_loses_nothing_and_resumes_by_offset() {
    let t = Scratch::new("stall");
    let rv = revival(&t.0);
    let seen: Seen = Arc::default();
    let stall_once = Arc::new(AtomicBool::new(true));
    let (s, stall) = (Arc::clone(&seen), Arc::clone(&stall_once));
    let on_bytes: OnBytes = Box::new(move |b: &[u8]| {
        let total = {
            let mut v = s.lock().unwrap();
            v.extend_from_slice(b);
            v.len()
        };
        if total > 1_000_000 && stall.swap(false, Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(3));
        }
    });
    let held = HeldPty::launch(rv.clone(), &authority(&t.0), on_bytes, Box::new(|_| {})).unwrap();
    held.write(FLOOD).unwrap();
    let tail_has = |needle: &str| {
        let v = seen.lock().unwrap();
        let from = v.len().saturating_sub(4096);
        String::from_utf8_lossy(&v[from..]).contains(needle)
    };
    let until = Instant::now() + Duration::from_secs(60);
    while Instant::now() < until && !tail_has("done-5") {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !stall_once.load(Ordering::SeqCst),
        "the daemon side stalled"
    );
    assert!(
        tail_has("done-5"),
        "the stream resumed after the stall ({} B seen)",
        seen.lock().unwrap().len()
    );
    let end = || usize::try_from(status_end(&rv.args.socket)).unwrap();
    assert!(
        eventually(|| seen.lock().unwrap().len() == end()),
        "delivered bytes equal journal growth: {} of {}",
        seen.lock().unwrap().len(),
        end()
    );
    let journal = PaneDir::at(&rv.args.pane_dir)
        .open_journal(JournalBounds::default())
        .unwrap()
        .read_all()
        .unwrap();
    assert!(
        *seen.lock().unwrap() == journal,
        "byte-identical to the journal"
    );
    assert!(holder_log(&rv).contains("its connection is shut down both ways"));
    held.end();
}

#[test]
fn an_older_incarnation_is_displaced_and_told_so_and_never_re_admitted() {
    let t = Scratch::new("displace");
    let rv = revival(&t.0);
    let holder = spawn_holder(&rv);
    let mut older = Raw::attached(&rv.args.socket, 0, Some(1));
    assert!(eventually(|| attaches(&rv) == 1), "{}", holder_log(&rv));
    let mut newer = Raw::attached(&rv.args.socket, 0, Some(2));
    assert!(eventually(|| attaches(&rv) == 2), "{}", holder_log(&rv));
    let until = Instant::now() + Duration::from_secs(5);
    let mut told = None;
    while Instant::now() < until && told.is_none() {
        match older.frame(Duration::from_millis(500)) {
            Frame::Msg(FromHolder::Displaced { by }) => told = Some(by),
            Frame::Eof => break,
            _ => {}
        }
    }
    assert_eq!(told, Some(2), "the displaced daemon is told by whom");
    assert!(matches!(older.frame(Duration::from_secs(2)), Frame::Eof));
    let mut again = Raw::attached(&rv.args.socket, 0, Some(1));
    match again.frame(Duration::from_secs(2)) {
        Frame::Msg(FromHolder::Displaced { by }) => assert_eq!(by, 2),
        other => panic!("an older incarnation is refused before any replay, got {other:?}"),
    }
    assert!(matches!(again.frame(Duration::from_secs(2)), Frame::Eof));
    newer.write(b"echo still-$((6*7))\n");
    assert!(newer.until_text("still-42", Duration::from_secs(10)));
    assert_eq!(attaches(&rv), 2, "{}", holder_log(&rv));
    assert!(holder_log(&rv).contains("Displaced by incarnation 2"));
    end_holder(&rv, holder);
}

#[test]
fn a_daemon_that_declares_no_incarnation_is_left_attached_and_silent_beside_a_newer_one() {
    let t = Scratch::new("silent");
    let rv = revival(&t.0);
    let holder = spawn_holder(&rv);
    let mut newer = Raw::attached(&rv.args.socket, 0, Some(2));
    assert!(eventually(|| attaches(&rv) == 1), "{}", holder_log(&rv));
    let mut old = Raw::attached(&rv.args.socket, 0, None);
    newer.write(b"echo beside-$((3+4))\n");
    assert!(newer.until_text("beside-7", Duration::from_secs(10)));
    assert!(
        matches!(old.frame(Duration::from_millis(1500)), Frame::Quiet),
        "a pre-R4 daemon cannot decode Displaced, so it is neither fed nor closed"
    );
    assert!(holder_log(&rv).contains("left attached and silent"));
    end_holder(&rv, holder);
}

#[test]
fn two_daemons_that_declare_no_incarnation_keep_last_attach_wins() {
    let t = Scratch::new("lastwins");
    let rv = revival(&t.0);
    let holder = spawn_holder(&rv);
    let mut first = Raw::attached(&rv.args.socket, 0, None);
    first.write(b"echo one-$((0+1))\n");
    assert!(first.until_text("one-1", Duration::from_secs(10)));
    let mut second = Raw::attached(&rv.args.socket, 0, None);
    assert!(second.until_text("one-1", Duration::from_secs(10)));
    assert!(matches!(
        drained(&mut first, Duration::from_secs(3)),
        Frame::Quiet
    ));
    second.write(b"echo two-$((1+1))\n");
    assert!(second.until_text("two-2", Duration::from_secs(10)));
    assert!(
        matches!(first.frame(Duration::from_millis(1000)), Frame::Quiet),
        "the replaced daemon is left silent, never closed, so it cannot spin"
    );
    end_holder(&rv, holder);
}

#[test]
fn a_rolled_back_daemon_is_admitted_once_the_newer_one_has_gone() {
    let t = Scratch::new("rollback");
    let rv = revival(&t.0);
    let holder = spawn_holder(&rv);
    let mut newer = Raw::attached(&rv.args.socket, 0, Some(5));
    newer.write(b"echo seen-$((2*4))\n");
    assert!(newer.until_text("seen-8", Duration::from_secs(10)));
    drop(newer);
    thread::sleep(Duration::from_millis(300));
    let mut rolled = Raw::attached(&rv.args.socket, 0, None);
    assert!(
        rolled.until_text("seen-8", Duration::from_secs(10)),
        "the previous release reads the pane after a rollback"
    );
    end_holder(&rv, holder);
}

#[test]
fn a_daemon_that_loses_the_lease_stops_following_and_never_ends_the_shell() {
    let t = Scratch::new("lease");
    let rv = revival(&t.0);
    let store = Store::open(t.0.join("store")).unwrap();
    let first = Authority::new(store.take_lease().unwrap());
    let lost_to = Arc::new(AtomicU64::new(0));
    let l = Arc::clone(&lost_to);
    first.on_lost(move |by| l.store(by, Ordering::SeqCst));
    let (seen_a, bytes_a, exit_a, on_exit_a) = sinks();
    let held_a = HeldPty::launch(rv.clone(), &first, bytes_a, on_exit_a).unwrap();
    held_a.write(b"echo one-$((0+1))\n").unwrap();
    assert!(wait_for(&seen_a, "one-1"));

    let second = Authority::new(store.take_lease().unwrap());
    let (seen_b, bytes_b, exit_b, on_exit_b) = sinks();
    let held_b = HeldPty::adopt(rv.clone(), &second, bytes_b, on_exit_b).unwrap();
    assert!(
        wait_for(&seen_b, "one-1"),
        "the newer daemon replays the pane"
    );
    assert!(
        eventually(|| first.is_lost()),
        "the older daemon learns it lost"
    );
    assert!(eventually(|| lost_to.load(Ordering::SeqCst) == 2));
    assert!(!second.is_lost());
    assert!(
        held_a.write(b"echo stray\n").is_err(),
        "a displaced daemon takes no more input"
    );
    held_a.end();
    held_b.write(b"echo two-$((1+1))\n").unwrap();
    assert!(wait_for(&seen_b, "two-2"), "the shell outlives the loser");
    assert!(
        exit_a.recv_timeout(Duration::from_secs(1)).is_err(),
        "losing the lease is not the pane's end"
    );
    thread::sleep(Duration::from_secs(3));
    assert_eq!(attaches(&rv), 2, "{}", holder_log(&rv));
    held_b.end();
    assert!(exit_b.recv_timeout(Duration::from_secs(10)).is_ok());
}

#[test]
fn a_lease_counter_behind_a_holder_is_raised_above_it_instead_of_losing() {
    let t = Scratch::new("raise");
    let rv = revival(&t.0);
    let holder = spawn_holder(&rv);
    let mut seen_five = Raw::attached(&rv.args.socket, 0, Some(5));
    seen_five.write(b"echo before-$((9*9))\n");
    assert!(seen_five.until_text("before-81", Duration::from_secs(10)));
    drop(seen_five);
    let fresh = Authority::new(
        Store::open(t.0.join("store"))
            .unwrap()
            .take_lease()
            .unwrap(),
    );
    assert_eq!(fresh.incarnation(), 1);
    let (seen, on_bytes, _exit, on_exit) = sinks();
    let held = HeldPty::adopt(rv.clone(), &fresh, on_bytes, on_exit).unwrap();
    assert!(wait_for(&seen, "before-81"));
    assert!(!fresh.is_lost());
    assert_eq!(fresh.incarnation(), 6);
    held.write(b"echo after-$((2+2))\n").unwrap();
    assert!(wait_for(&seen, "after-4"));
    drop(held);
    end_holder(&rv, holder);
}

struct FakePrev {
    content: Vec<u8>,
    muted_at: usize,
    attaches: AtomicUsize,
}

impl FakePrev {
    fn serve(self: &Arc<Self>, socket: &Path) {
        let listener = UnixListener::bind(socket).unwrap();
        let me = Arc::clone(self);
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let me = Arc::clone(&me);
                thread::spawn(move || me.conn(conn));
            }
        });
    }

    fn conn(&self, stream: UnixStream) {
        let mut w = stream.try_clone().unwrap();
        let mut r = BufReader::new(stream);
        while let Ok(msg) = read_to_holder(&mut r) {
            match msg {
                ToHolder::Hello { .. } => {
                    let _ = write_from_holder(
                        &mut w,
                        &FromHolder::Hello {
                            proto: PROTO,
                            pid: 1,
                            child_pid: None,
                        },
                    );
                }
                ToHolder::Status => {
                    let _ = write_from_holder(
                        &mut w,
                        &FromHolder::Status(HolderStatus {
                            pid: 1,
                            child_pid: None,
                            start: 0,
                            end: self.content.len() as u64,
                        }),
                    );
                }
                ToHolder::Attach { from, .. } => {
                    let first = self.attaches.fetch_add(1, Ordering::SeqCst) == 0;
                    let stop = if first {
                        self.muted_at
                    } else {
                        self.content.len()
                    };
                    let from = usize::try_from(from).unwrap();
                    for (i, chunk) in self.content[from..stop].chunks(1000).enumerate() {
                        let _ = write_from_holder(
                            &mut w,
                            &FromHolder::Bytes {
                                at: (from + i * 1000) as u64,
                                data: chunk.to_vec(),
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

fn fake_prev(t: &Scratch) -> (Arc<FakePrev>, tamotsu::Revival) {
    let mut rv = revival(&t.0);
    rv.args.socket = t.0.join("prev.sock");
    let content: Vec<u8> = (0..120_000u32)
        .map(|i| b'a' + u8::try_from(i % 26).unwrap())
        .collect();
    let fake = Arc::new(FakePrev {
        content,
        muted_at: 40_000,
        attaches: AtomicUsize::new(0),
    });
    fake.serve(&rv.args.socket);
    (fake, rv)
}

fn seen_len(seen: &Seen) -> usize {
    seen.lock().unwrap().len()
}

#[test]
fn a_muted_pre_r4_holder_stays_muted_without_an_edge_and_recovers_on_a_read() {
    let t = Scratch::new("prevread");
    let (fake, rv) = fake_prev(&t);
    let (seen, on_bytes, _exit, on_exit) = sinks();
    let held = HeldPty::adopt(rv, &authority(&t.0), on_bytes, on_exit).unwrap();
    assert!(eventually(|| seen_len(&seen) == fake.muted_at));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(
        seen_len(&seen),
        fake.muted_at,
        "nothing polls: without an edge the daemon does not probe"
    );
    held.read_edge();
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until && seen_len(&seen) < fake.content.len() {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        *seen.lock().unwrap() == fake.content,
        "Status.end exposed the mute and the daemon re-attached by offset: {} of {} B",
        seen_len(&seen),
        fake.content.len()
    );
    assert_eq!(fake.attaches.load(Ordering::SeqCst), 2);
}

#[test]
fn a_key_with_no_echo_within_two_seconds_recovers_a_muted_pre_r4_holder() {
    let t = Scratch::new("prevkey");
    let (fake, rv) = fake_prev(&t);
    let (seen, on_bytes, _exit, on_exit) = sinks();
    let held = HeldPty::adopt(rv, &authority(&t.0), on_bytes, on_exit).unwrap();
    assert!(eventually(|| seen_len(&seen) == fake.muted_at));
    held.write(b"x").unwrap();
    let until = Instant::now() + Duration::from_secs(12);
    while Instant::now() < until && seen_len(&seen) < fake.content.len() {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        *seen.lock().unwrap() == fake.content,
        "the key edge recovered the pane: {} of {} B",
        seen_len(&seen),
        fake.content.len()
    );
    assert_eq!(fake.attaches.load(Ordering::SeqCst), 2);
}

#[test]
fn a_healthy_link_is_never_re_attached_by_an_edge() {
    let t = Scratch::new("healthy");
    let rv = revival(&t.0);
    let (seen, on_bytes, _exit, on_exit) = sinks();
    let held = HeldPty::launch(rv.clone(), &authority(&t.0), on_bytes, on_exit).unwrap();
    held.write(b"echo ok-$((5*5))\n").unwrap();
    assert!(wait_for(&seen, "ok-25"));
    held.read_edge();
    held.write(b"echo k-$((1+2))\n").unwrap();
    assert!(wait_for(&seen, "k-3"));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(attaches(&rv), 1, "{}", holder_log(&rv));
    held.end();
}

#[cfg(feature = "bench-probes")]
#[test]
fn the_mute_sink_fault_reproduces_the_pre_r4_holder_and_a_read_edge_recovers_it() {
    let t = Scratch::new("fault");
    let mut rv = revival(&t.0);
    rv.env.push((
        tear_types::probe_env::FAULTS.to_string(),
        "mute-sink".to_string(),
    ));
    let holder = spawn_holder(&rv);
    let mut stalled = Raw::attached(&rv.args.socket, 0, Some(1));
    stalled.write(FLOOD);
    thread::sleep(Duration::from_secs(4));
    assert!(
        matches!(drained(&mut stalled, Duration::from_secs(20)), Frame::Quiet),
        "with the fault armed a failed sink keeps its socket open and goes silent"
    );
    drop(stalled);
    end_holder(&rv, holder);

    let t = Scratch::new("faultedge");
    let mut rv = revival(&t.0);
    rv.env.push((
        tear_types::probe_env::FAULTS.to_string(),
        "mute-sink".to_string(),
    ));
    let seen: Seen = Arc::default();
    let stall_once = Arc::new(AtomicBool::new(true));
    let (s, stall) = (Arc::clone(&seen), Arc::clone(&stall_once));
    let on_bytes: OnBytes = Box::new(move |b: &[u8]| {
        let total = {
            let mut v = s.lock().unwrap();
            v.extend_from_slice(b);
            v.len()
        };
        if total > 1_000_000 && stall.swap(false, Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(3));
        }
    });
    let held = HeldPty::launch(rv.clone(), &authority(&t.0), on_bytes, Box::new(|_| {})).unwrap();
    held.write(FLOOD).unwrap();
    let journal_end = || usize::try_from(status_end(&rv.args.socket)).unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    while Instant::now() < until && (stall_once.load(Ordering::SeqCst) || journal_end() < 6_000_000)
    {
        thread::sleep(Duration::from_millis(100));
    }
    thread::sleep(Duration::from_secs(2));
    assert!(
        seen_len(&seen) < journal_end(),
        "the faulted holder muted the pane"
    );
    held.read_edge();
    assert!(
        eventually(|| seen_len(&seen) == journal_end()),
        "the read edge recovered it: {} of {}",
        seen_len(&seen),
        journal_end()
    );
    held.end();
}
