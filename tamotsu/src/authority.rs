use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use makimono::{Lease, Raised};
use parking_lot::Mutex;

type OnLost = Box<dyn FnOnce(u64) + Send>;

#[derive(Clone)]
pub struct Authority {
    inner: Arc<Inner>,
}

struct Inner {
    lease: Option<Lease>,
    lost: AtomicBool,
    on_lost: Mutex<Option<OnLost>>,
}

pub(crate) enum Verdict {
    Retry,
    Lost,
}

impl Authority {
    #[must_use]
    pub fn new(lease: Lease) -> Self {
        Self::with(Some(lease))
    }

    #[must_use]
    pub fn undeclared() -> Self {
        Self::with(None)
    }

    fn with(lease: Option<Lease>) -> Self {
        Self {
            inner: Arc::new(Inner {
                lease,
                lost: AtomicBool::new(false),
                on_lost: Mutex::new(None),
            }),
        }
    }

    #[must_use]
    pub fn incarnation(&self) -> u64 {
        self.inner.lease.as_ref().map_or(0, Lease::incarnation)
    }

    #[must_use]
    pub fn is_declared(&self) -> bool {
        self.lease().is_some()
    }

    fn lease(&self) -> Option<&Lease> {
        if lease_off() {
            return None;
        }
        self.inner.lease.as_ref()
    }

    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.inner.lost.load(Ordering::Acquire)
    }

    pub fn on_lost(&self, f: impl FnOnce(u64) + Send + 'static) {
        *self.inner.on_lost.lock() = Some(Box::new(f));
    }

    pub(crate) fn declared(&self) -> Option<u64> {
        self.lease().map(Lease::incarnation)
    }

    pub(crate) fn holds(&self) -> bool {
        let Some(lease) = self.lease() else {
            return true;
        };
        if self.is_lost() {
            return false;
        }
        if lease.held() {
            return true;
        }
        let by = lease.current().ok().flatten().unwrap_or(0);
        self.lose(by);
        false
    }

    pub fn require(&self) -> io::Result<()> {
        if self.holds() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "incarnation {} no longer holds the session store",
                    self.incarnation()
                ),
            ))
        }
    }

    pub(crate) fn displaced(&self, by: u64) -> Verdict {
        if self.is_lost() {
            return Verdict::Lost;
        }
        let Some(lease) = self.lease() else {
            return Verdict::Lost;
        };
        match lease.raise_above(by) {
            Ok(Raised::To(n)) => {
                tracing::warn!(
                    was = self.incarnation(),
                    now = n,
                    holder_saw = by,
                    "tamotsu: a holder had seen a newer incarnation than the store's lease records; the lease is raised above it"
                );
                Verdict::Retry
            }
            Ok(Raised::Lost { by }) => {
                self.lose(by);
                Verdict::Lost
            }
            Err(e) => {
                tracing::warn!(error = %e, "tamotsu: the session store's lease is unreadable; treated as lost");
                self.lose(by);
                Verdict::Lost
            }
        }
    }

    fn lose(&self, by: u64) {
        if self.inner.lost.swap(true, Ordering::AcqRel) {
            return;
        }
        tracing::warn!(
            incarnation = self.incarnation(),
            by,
            "tamotsu: Displaced — incarnation {by} holds the session store; this daemon stops following its held panes"
        );
        if let Some(f) = self.inner.on_lost.lock().take() {
            let _ = thread::Builder::new()
                .name("tamotsu-displaced".into())
                .spawn(move || f(by));
        }
    }
}

impl fmt::Debug for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authority")
            .field("declared", &self.is_declared())
            .field("incarnation", &self.incarnation())
            .field("lost", &self.is_lost())
            .finish()
    }
}

#[cfg(feature = "bench-probes")]
fn lease_off() -> bool {
    tear_types::probes::active(tear_types::probes::Fault::StoreLeaseOff)
}

#[cfg(not(feature = "bench-probes"))]
const fn lease_off() -> bool {
    false
}
