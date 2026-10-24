#![cfg(feature = "testing")]

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

use tear_core::inproc::InProcess;
use tear_daemon::testing::DaemonHarness;

#[test]
fn a_second_daemon_refuses_a_socket_a_live_daemon_answers_on() {
    let first = DaemonHarness::new("single-daemon");
    match tear_daemon::start(first.socket().to_path_buf(), Arc::new(InProcess::new())) {
        Ok(second) => {
            std::mem::forget(second);
            std::mem::forget(first);
            panic!("second daemon took a socket a live daemon answers on");
        }
        Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse),
    }
    assert!(UnixStream::connect(first.socket()).is_ok());
}

#[test]
fn a_stale_socket_file_is_replaced() {
    let mut path = std::env::temp_dir();
    path.push(format!("tear-stale-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    drop(UnixListener::bind(&path).expect("bind stale"));
    assert!(path.exists());
    let _daemon = tear_daemon::start(path.clone(), Arc::new(InProcess::new()))
        .expect("start over stale socket");
    assert!(UnixStream::connect(&path).is_ok());
}
