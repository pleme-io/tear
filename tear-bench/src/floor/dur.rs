use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::time::Instant;

use super::{Lcg, Measured, Plan, scratch_path};
use crate::matrix::Floor;
use crate::seam;

tear_types::closed_vocabulary! {
    Mode {
        Persisted => "persisted",
        Ordered => "ordered",
        HandedToDevice => "handed-to-device",
        PageCache => "page-cache",
    }
}

impl Mode {
    #[must_use]
    pub const fn floor(self) -> Option<Floor> {
        match self {
            Mode::Persisted => Some(Floor::FlushPersisted),
            Mode::Ordered => Some(Floor::FlushOrdered),
            Mode::HandedToDevice => Some(Floor::FlushHandedToDevice),
            Mode::PageCache => None,
        }
    }

    #[must_use]
    pub const fn primitive(self) -> &'static str {
        if cfg!(target_os = "macos") {
            match self {
                Mode::Persisted => "F_FULLFSYNC",
                Mode::Ordered => "F_BARRIERFSYNC",
                Mode::HandedToDevice => "fsync",
                Mode::PageCache => "none",
            }
        } else {
            match self {
                Mode::Persisted | Mode::Ordered | Mode::HandedToDevice => "fdatasync",
                Mode::PageCache => "none",
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn sync(f: &std::fs::File, mode: Mode) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, fcntl};
    match mode {
        Mode::Persisted => fcntl(f.as_fd(), FcntlArg::F_FULLFSYNC).map(drop),
        Mode::Ordered => fcntl(f.as_fd(), FcntlArg::F_BARRIERFSYNC).map(drop),
        Mode::HandedToDevice => nix::unistd::fsync(f.as_fd()),
        Mode::PageCache => Ok(()),
    }
    .map_err(io::Error::from)
}

#[cfg(not(target_os = "macos"))]
fn sync(f: &std::fs::File, mode: Mode) -> io::Result<()> {
    match mode {
        Mode::Persisted | Mode::Ordered | Mode::HandedToDevice => {
            nix::unistd::fdatasync(f.as_fd()).map_err(io::Error::from)
        }
        Mode::PageCache => Ok(()),
    }
}

pub fn flush(mode: Mode, plan: Plan) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let size = 4096usize;
    let n = plan.n(2_000);
    let warm = 10;
    let path = scratch_path(&format!("dur-{}.dat", mode.name()));
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    let mut buf = vec![0u8; size];
    for b in &mut buf {
        *b = rng.draw().to_ne_bytes()[0];
    }
    let mut ns = Vec::with_capacity(n);
    for i in 0..(warm + n) {
        let k = (i * 8) % size;
        buf[k] = buf[k].wrapping_add(1);
        f.write_all(&buf)?;
        let t0 = Instant::now();
        sync(&f, mode)?;
        if i >= warm {
            ns.push(t0.elapsed().as_nanos() as f64);
        }
    }
    drop(f);
    let _ = std::fs::remove_file(&path);
    let floor = mode
        .floor()
        .ok_or_else(|| io::Error::other("page-cache has no flush floor"))?;
    Ok(Measured::new(
        floor,
        format!("{};{size}B;band={}", mode.primitive(), plan.band.name()),
        ns,
    ))
}
