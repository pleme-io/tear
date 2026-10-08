use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use crate::proto::{FromHolder, write_from_holder};

pub struct Sink {
    stream: Option<UnixStream>,
}

impl Sink {
    #[must_use]
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream: Some(stream),
        }
    }

    pub fn send(&mut self, msg: &FromHolder) -> io::Result<()> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "sink already closed"))?;
        write_from_holder(stream, msg)
    }

    pub fn leave_silent(mut self) {
        drop(self.stream.take());
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn dropping_a_sink_ends_the_peer_s_stream_even_while_another_handle_lives() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let reader_half = ours.try_clone().unwrap();
        let mut sink = Sink::new(ours);
        sink.send(&FromHolder::Displaced { by: 3 }).unwrap();
        drop(sink);
        theirs
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut got = Vec::new();
        let read = theirs.read_to_end(&mut got);
        assert!(
            read.is_ok(),
            "the peer reads EOF once the sink is dropped, not a timeout: {read:?}"
        );
        assert!(!got.is_empty(), "a frame written before the drop arrives");
        drop(reader_half);
    }

    #[test]
    fn a_silenced_sink_leaves_the_peer_attached() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let reader_half = ours.try_clone().unwrap();
        Sink::new(ours).leave_silent();
        theirs
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        let mut buf = [0u8; 8];
        let err = theirs.read(&mut buf).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "{err:?}"
        );
        drop(reader_half);
    }
}
