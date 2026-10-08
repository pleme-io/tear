use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::atomic;

const LEASE: &str = "authority.json";
const LEASE_LOCK: &str = "authority.lock";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct LeaseDoc {
    incarnation: u64,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    taken_unix: u64,
}

#[derive(Debug)]
pub struct Lease {
    doc: PathBuf,
    lock: PathBuf,
    incarnation: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raised {
    To(u64),
    Lost { by: u64 },
}

impl Lease {
    pub(crate) fn take(root: &Path) -> io::Result<Self> {
        let lease = Self {
            doc: root.join(LEASE),
            lock: root.join(LEASE_LOCK),
            incarnation: AtomicU64::new(0),
        };
        let _guard = lease.exclusive()?;
        let next = lease.recorded() + 1;
        lease.write(next)?;
        lease.incarnation.store(next, Ordering::Release);
        Ok(lease)
    }

    #[must_use]
    pub fn incarnation(&self) -> u64 {
        self.incarnation.load(Ordering::Acquire)
    }

    pub fn current(&self) -> io::Result<Option<u64>> {
        Ok(self.read()?.map(|d| d.incarnation))
    }

    #[must_use]
    pub fn held(&self) -> bool {
        match self.current() {
            Ok(Some(current)) => current <= self.incarnation(),
            Ok(None) | Err(_) => true,
        }
    }

    pub fn raise_above(&self, seen: u64) -> io::Result<Raised> {
        let _guard = self.exclusive()?;
        let mine = self.incarnation();
        let current = self.recorded();
        if current > mine {
            return Ok(Raised::Lost { by: current });
        }
        let next = current.max(seen).max(mine) + 1;
        self.write(next)?;
        self.incarnation.store(next, Ordering::Release);
        Ok(Raised::To(next))
    }

    fn exclusive(&self) -> io::Result<File> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.lock)?;
        f.lock()?;
        Ok(f)
    }

    fn read(&self) -> io::Result<Option<LeaseDoc>> {
        atomic::read_json(&self.doc)
    }

    fn recorded(&self) -> u64 {
        match self.read() {
            Ok(doc) => doc.map_or(0, |d| d.incarnation),
            Err(e) => {
                tracing::warn!(
                    lease = %self.doc.display(),
                    error = %e,
                    "makimono: the session store's authority lease is unreadable; it is read as incarnation 0 and rewritten"
                );
                0
            }
        }
    }

    fn write(&self, incarnation: u64) -> io::Result<()> {
        atomic::write_json(
            &self.doc,
            &LeaseDoc {
                incarnation,
                pid: Some(std::process::id()),
                taken_unix: crate::ending::now_unix(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use crate::testdir::TempDir;

    #[test]
    fn each_take_is_a_newer_incarnation_and_the_newest_holds() {
        let t = TempDir::new("lease");
        let store = Store::open(t.path()).unwrap();
        let first = store.take_lease().unwrap();
        assert_eq!(first.incarnation(), 1);
        assert!(first.held());
        let second = store.take_lease().unwrap();
        assert_eq!(second.incarnation(), 2);
        assert!(second.held());
        assert!(
            !first.held(),
            "a newer start takes the store from the older one"
        );
        let reopened = Store::open(t.path()).unwrap().take_lease().unwrap();
        assert_eq!(reopened.incarnation(), 3, "the incarnation is persisted");
    }

    #[test]
    fn concurrent_takes_never_share_an_incarnation() {
        let t = TempDir::new("lease-race");
        let store = Store::open(t.path()).unwrap();
        let seen: Vec<u64> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|_| s.spawn(|| store.take_lease().unwrap().incarnation()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "{seen:?}");
        assert_eq!(sorted, (1..=16).collect::<Vec<_>>());
    }

    #[test]
    fn a_lost_counter_is_raised_above_what_a_holder_saw_unless_a_newer_daemon_took_it() {
        let t = TempDir::new("lease-raise");
        let store = Store::open(t.path()).unwrap();
        let lease = store.take_lease().unwrap();
        assert_eq!(lease.raise_above(7).unwrap(), Raised::To(8));
        assert_eq!(lease.incarnation(), 8);
        assert!(lease.held());
        let newer = store.take_lease().unwrap();
        assert_eq!(newer.incarnation(), 9);
        assert_eq!(lease.raise_above(9).unwrap(), Raised::Lost { by: 9 });
        assert!(!lease.held());
    }

    #[test]
    fn a_corrupt_lease_is_rewritten_and_never_refuses_the_store() {
        let t = TempDir::new("lease-corrupt");
        let store = Store::open(t.path()).unwrap();
        std::fs::write(t.path().join(LEASE), b"{not json").unwrap();
        let lease = store.take_lease().unwrap();
        assert_eq!(lease.incarnation(), 1);
        assert_eq!(lease.current().unwrap(), Some(1), "the doc is rewritten");
        assert!(lease.held());
        std::fs::write(t.path().join(LEASE), b"{not json").unwrap();
        assert_eq!(lease.raise_above(4).unwrap(), Raised::To(5));
        assert_eq!(lease.current().unwrap(), Some(5));
        assert_eq!(store.take_lease().unwrap().incarnation(), 6);
    }

    #[test]
    fn an_unreadable_or_missing_lease_is_not_read_as_lost() {
        let t = TempDir::new("lease-missing");
        let store = Store::open(t.path()).unwrap();
        let lease = store.take_lease().unwrap();
        std::fs::remove_file(t.path().join(LEASE)).unwrap();
        assert!(lease.held());
        std::fs::write(t.path().join(LEASE), b"{not json").unwrap();
        assert!(lease.held());
    }
}
