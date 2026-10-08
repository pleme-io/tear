#![forbid(unsafe_code)]

pub mod atomic;
mod ending;
mod journal;
mod lease;
#[cfg(feature = "bench-probes")]
pub mod probes;
mod store;

pub use ending::{Ending, Human, Verdict, classify, now_unix};
pub use journal::{Chunk, Journal, JournalBounds};
pub use lease::{Lease, Raised};
pub use store::{PaneDir, PaneMeta, SessionDir, Store};

#[cfg(test)]
pub(crate) mod testdir {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "makimono-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }

        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
