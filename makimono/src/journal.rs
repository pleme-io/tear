use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const SEGMENT_PREFIX: &str = "seg-";
const SEGMENT_SUFFIX: &str = ".raw";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalBounds {
    pub max_bytes: u64,
    pub segment_bytes: u64,
    pub fsync_interval_ms: u64,
}

impl Default for JournalBounds {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            segment_bytes: 4 * 1024 * 1024,
            fsync_interval_ms: 1000,
        }
    }
}

impl JournalBounds {
    #[must_use]
    pub fn normalized(self) -> Self {
        let segment_bytes = self.segment_bytes.max(4096);
        Self {
            max_bytes: self.max_bytes.max(segment_bytes),
            segment_bytes,
            fsync_interval_ms: self.fsync_interval_ms,
        }
    }

    fn fsync_interval(&self) -> Duration {
        Duration::from_millis(self.fsync_interval_ms)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub at: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    start: u64,
    len: u64,
}

impl Segment {
    fn end(self) -> u64 {
        self.start + self.len
    }
}

pub struct Journal {
    dir: PathBuf,
    bounds: JournalBounds,
    segments: VecDeque<Segment>,
    file: Option<File>,
    lost: u64,
    unsynced: u64,
    last_sync: Instant,
}

impl Journal {
    pub fn open(dir: &Path, bounds: JournalBounds) -> io::Result<Self> {
        crate::atomic::create_private_dir(dir)?;
        let mut segments: Vec<Segment> = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(start) = name.to_str().and_then(parse_segment_name) else {
                continue;
            };
            segments.push(Segment {
                start,
                len: entry.metadata()?.len(),
            });
        }
        segments.sort_by_key(|s| s.start);
        let mut journal = Self {
            dir: dir.to_path_buf(),
            bounds: bounds.normalized(),
            segments: segments.into(),
            file: None,
            lost: 0,
            unsynced: 0,
            last_sync: Instant::now(),
        };
        if let Some(last) = journal.segments.back().copied() {
            journal.file = Some(open_append(&journal.segment_path(last.start))?);
        }
        Ok(journal)
    }

    #[must_use]
    pub fn start(&self) -> u64 {
        self.segments.front().map_or(0, |s| s.start)
    }

    #[must_use]
    pub fn end(&self) -> u64 {
        self.segments.back().map_or(0, |s| s.end()) + self.lost
    }

    pub fn note_lost(&mut self, bytes: u64) {
        self.lost += bytes;
    }

    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.len).sum()
    }

    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let rotate = self.lost > 0
            || match self.segments.back() {
                None => true,
                Some(last) => last.len >= self.bounds.segment_bytes,
            };
        if rotate {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("journal has no open segment"))?;
        file.write_all(bytes)?;
        if let Some(last) = self.segments.back_mut() {
            last.len += bytes.len() as u64;
        }
        self.unsynced += bytes.len() as u64;
        self.evict()?;
        self.sync_if_due()
    }

    pub fn sync_if_due(&mut self) -> io::Result<()> {
        if self.unsynced == 0 {
            return Ok(());
        }
        if self.unsynced >= 1024 * 1024 || self.last_sync.elapsed() >= self.bounds.fsync_interval() {
            self.sync()?;
        }
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            f.sync_data()?;
        }
        self.unsynced = 0;
        self.last_sync = Instant::now();
        Ok(())
    }

    pub fn read_from(&self, from: u64, max: usize) -> io::Result<Option<Chunk>> {
        let at = from.max(self.start());
        if at >= self.end() || max == 0 {
            return Ok(None);
        }
        let Some(seg) = self
            .segments
            .iter()
            .copied()
            .find(|s| at < s.end() && s.len > 0)
        else {
            return Ok(None);
        };
        let at = at.max(seg.start);
        let mut f = File::open(self.segment_path(seg.start))?;
        f.seek(SeekFrom::Start(at - seg.start))?;
        let want = usize::try_from(seg.end() - at).unwrap_or(usize::MAX).min(max);
        let mut data = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            let n = f.read(&mut data[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        data.truncate(filled);
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(Chunk { at, data }))
    }

    pub fn read_all(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut at = self.start();
        while let Some(chunk) = self.read_from(at, 1024 * 1024)? {
            at = chunk.at + chunk.data.len() as u64;
            out.extend_from_slice(&chunk.data);
        }
        Ok(out)
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            f.sync_data()?;
        }
        let start = self.end();
        let file = open_append(&self.segment_path(start))?;
        self.segments.push_back(Segment { start, len: 0 });
        self.file = Some(file);
        self.lost = 0;
        Ok(())
    }

    fn evict(&mut self) -> io::Result<()> {
        while self.segments.len() > 1 && self.retained_bytes() > self.bounds.max_bytes {
            let Some(oldest) = self.segments.pop_front() else {
                break;
            };
            match fs::remove_file(self.segment_path(oldest.start)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn segment_path(&self, start: u64) -> PathBuf {
        self.dir
            .join(format!("{SEGMENT_PREFIX}{start:020}{SEGMENT_SUFFIX}"))
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn parse_segment_name(name: &str) -> Option<u64> {
    name.strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(SEGMENT_SUFFIX)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TempDir;

    fn small() -> JournalBounds {
        JournalBounds {
            max_bytes: 3 * 4096,
            segment_bytes: 4096,
            fsync_interval_ms: 0,
        }
    }

    #[test]
    fn append_then_reopen_reads_back_every_byte_in_order() {
        let t = TempDir::new("journal-reopen");
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        {
            let mut j = Journal::open(t.path(), small()).unwrap();
            for chunk in payload.chunks(777) {
                j.append(chunk).unwrap();
            }
            assert_eq!(j.end(), payload.len() as u64);
        }
        let j = Journal::open(t.path(), small()).unwrap();
        assert_eq!(j.start(), 0);
        assert_eq!(j.end(), payload.len() as u64);
        assert_eq!(j.read_all().unwrap(), payload);
    }

    #[test]
    fn eviction_keeps_the_bound_and_offsets_stay_absolute() {
        let t = TempDir::new("journal-evict");
        let mut j = Journal::open(t.path(), small()).unwrap();
        let mut written = 0u64;
        for i in 0..40u32 {
            let block = vec![(i % 256) as u8; 1000];
            j.append(&block).unwrap();
            written += 1000;
        }
        assert_eq!(j.end(), written);
        assert!(j.retained_bytes() <= small().max_bytes + small().segment_bytes);
        assert!(j.start() > 0, "oldest segments must have been evicted");
        let tail = j.read_all().unwrap();
        assert_eq!(tail.len() as u64, j.end() - j.start());
        let first = j.read_from(0, 10).unwrap().unwrap();
        assert_eq!(first.at, j.start(), "a read below the horizon starts at the oldest byte");
    }

    #[test]
    fn appending_after_reopen_continues_the_same_offset_space() {
        let t = TempDir::new("journal-continue");
        {
            let mut j = Journal::open(t.path(), small()).unwrap();
            j.append(b"hello ").unwrap();
        }
        let mut j = Journal::open(t.path(), small()).unwrap();
        j.append(b"world").unwrap();
        assert_eq!(j.read_all().unwrap(), b"hello world");
        let mid = j.read_from(6, 100).unwrap().unwrap();
        assert_eq!(mid, Chunk { at: 6, data: b"world".to_vec() });
        assert!(j.read_from(11, 100).unwrap().is_none());
    }

    #[test]
    fn a_lost_write_leaves_a_gap_that_reads_skip_and_offsets_never_reuse() {
        let t = TempDir::new("journal-gap");
        let mut j = Journal::open(t.path(), small()).unwrap();
        j.append(b"abc").unwrap();
        j.note_lost(5);
        assert_eq!(j.end(), 8);
        j.append(b"xyz").unwrap();
        assert_eq!(j.end(), 11);
        assert_eq!(j.read_all().unwrap(), b"abcxyz");
        let after_gap = j.read_from(4, 100).unwrap().unwrap();
        assert_eq!(after_gap, Chunk { at: 8, data: b"xyz".to_vec() });
        drop(j);
        let j = Journal::open(t.path(), small()).unwrap();
        assert_eq!(j.end(), 11);
    }

    #[test]
    fn unrelated_files_in_the_directory_are_ignored() {
        let t = TempDir::new("journal-ignore");
        fs::write(t.path().join("meta.json"), b"{}").unwrap();
        fs::write(t.path().join("seg-notanumber.raw"), b"zzz").unwrap();
        let mut j = Journal::open(t.path(), small()).unwrap();
        j.append(b"ok").unwrap();
        assert_eq!(j.read_all().unwrap(), b"ok");
    }
}
