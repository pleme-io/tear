use std::sync::mpsc;
use std::task::Waker;

pub struct WakingSender<T> {
    tx: Option<mpsc::Sender<T>>,
    waker: Waker,
}

impl<T> std::fmt::Debug for WakingSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WakingSender").finish_non_exhaustive()
    }
}

impl<T> WakingSender<T> {
    #[must_use]
    pub fn new(tx: mpsc::Sender<T>, waker: Waker) -> Self {
        Self {
            tx: Some(tx),
            waker,
        }
    }

    #[must_use]
    pub fn channel(waker: Waker) -> (Self, mpsc::Receiver<T>) {
        let (tx, rx) = mpsc::channel();
        (Self::new(tx, waker), rx)
    }

    pub fn send(&self, item: T) -> Result<(), mpsc::SendError<T>> {
        let sent = match &self.tx {
            Some(tx) => tx.send(item),
            None => Err(mpsc::SendError(item)),
        };
        if sent.is_ok() {
            self.waker.wake_by_ref();
        }
        sent
    }

    pub fn queue(&self, item: T) -> Result<Ring, mpsc::SendError<T>> {
        match &self.tx {
            Some(tx) => tx.send(item).map(|()| Ring(self.waker.clone())),
            None => Err(mpsc::SendError(item)),
        }
    }
}

#[must_use]
pub struct Ring(Waker);

impl std::fmt::Debug for Ring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ring").finish_non_exhaustive()
    }
}

impl Ring {
    pub fn ring(self) {
        self.0.wake();
    }
}

impl<T> Drop for WakingSender<T> {
    fn drop(&mut self) {
        drop(self.tx.take());
        self.waker.wake_by_ref();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::task::Wake;

    struct Witness {
        rx: Mutex<mpsc::Receiver<u8>>,
        seen: Mutex<Vec<Result<u8, mpsc::TryRecvError>>>,
    }

    impl Wake for Witness {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let got = self.rx.lock().unwrap().try_recv();
            self.seen.lock().unwrap().push(got);
        }
    }

    #[test]
    fn every_send_rings_after_the_item_is_queued_and_the_end_rings_after_the_stream_closed() {
        let (tx, rx) = mpsc::channel();
        let witness = Arc::new(Witness {
            rx: Mutex::new(rx),
            seen: Mutex::new(Vec::new()),
        });
        let sender = WakingSender::new(tx, Waker::from(Arc::clone(&witness)));
        sender.send(4).unwrap();
        sender.send(5).unwrap();
        drop(sender);
        assert_eq!(
            *witness.seen.lock().unwrap(),
            vec![Ok(4), Ok(5), Err(mpsc::TryRecvError::Disconnected)]
        );
    }

    #[test]
    fn a_queued_item_rings_only_when_its_ring_is_rung() {
        let (tx, rx) = mpsc::channel();
        let witness = Arc::new(Witness {
            rx: Mutex::new(rx),
            seen: Mutex::new(Vec::new()),
        });
        let sender = WakingSender::new(tx, Waker::from(Arc::clone(&witness)));
        let ring = sender.queue(9).unwrap();
        assert!(witness.seen.lock().unwrap().is_empty());
        ring.ring();
        assert_eq!(*witness.seen.lock().unwrap(), vec![Ok(9)]);
    }

    #[test]
    fn a_send_nobody_receives_rings_nothing() {
        struct Count(Mutex<usize>);
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                *self.0.lock().unwrap() += 1;
            }
        }
        let count = Arc::new(Count(Mutex::new(0)));
        let (sender, rx) = WakingSender::<u8>::channel(Waker::from(Arc::clone(&count)));
        drop(rx);
        assert!(sender.send(1).is_err());
        assert!(sender.queue(2).is_err());
        assert_eq!(*count.0.lock().unwrap(), 0);
    }
}
