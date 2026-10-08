use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use tamotsu::sink::Sink;

fn main() {
    let (ours, _theirs) = UnixStream::pair().unwrap();
    let shared = Arc::new(Mutex::new(ours));
    let _sink = Sink::new(Arc::clone(&shared));
}
