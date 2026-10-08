use std::hint::black_box;
use std::io;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::{Measured, Plan};
use crate::matrix::Floor;
use crate::seam;

#[derive(Serialize, Deserialize)]
pub enum AsArray {
    Ok,
    PaneBytes(Vec<u8>),
}

#[derive(Serialize, Deserialize)]
pub enum AsBytes {
    Ok,
    PaneBytes(#[serde(with = "serde_bytes")] Vec<u8>),
}

#[must_use]
pub fn payload(size: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(size);
    let mut k = 0u8;
    while v.len() < size {
        v.push(if k % 80 == 79 {
            b'\n'
        } else {
            b' ' + 1 + (k % 94)
        });
        k = k.wrapping_add(1);
    }
    v
}

pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let mut b = Vec::new();
    ciborium::ser::into_writer(msg, &mut b).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(b)
}

#[must_use]
pub fn array_inflation(size: usize) -> Option<f64> {
    let p = payload(size);
    let a = encode(&AsArray::PaneBytes(p.clone())).ok()?;
    Some(a.len() as f64 / p.len() as f64)
}

fn round_trip_ns(size: usize, n: usize) -> io::Result<Vec<f64>> {
    let msg = AsBytes::PaneBytes(payload(size));
    timed(size, n, || {
        let e = encode(black_box(&msg))?;
        let back: AsBytes = ciborium::de::from_reader(e.as_slice())
            .map_err(|err| io::Error::other(err.to_string()))?;
        black_box(back);
        Ok(())
    })
}

pub const FRAME_BYTES: usize = 65_536;
const TAG_DATA: u8 = 2;

#[must_use]
pub fn raw_encode(at: u64, data: &[u8]) -> Vec<u8> {
    let len = 1 + 8 + data.len();
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&u32::try_from(len).unwrap_or(u32::MAX).to_be_bytes());
    buf.push(TAG_DATA);
    buf.extend_from_slice(&at.to_be_bytes());
    buf.extend_from_slice(data);
    buf
}

pub fn raw_decode(frame: &[u8]) -> io::Result<(u64, Vec<u8>)> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "short raw frame");
    let len = u32::from_be_bytes(
        frame
            .get(..4)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?,
    ) as usize;
    if frame.get(4) != Some(&TAG_DATA) || len < 9 {
        return Err(bad());
    }
    let at = u64::from_be_bytes(
        frame
            .get(5..13)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?,
    );
    Ok((at, frame.get(13..4 + len).ok_or_else(bad)?.to_vec()))
}

#[must_use]
pub const fn batch(size: usize) -> usize {
    if size >= FRAME_BYTES { 16 } else { 256 }
}

pub fn timed<F: FnMut() -> io::Result<()>>(
    size: usize,
    n: usize,
    mut f: F,
) -> io::Result<Vec<f64>> {
    let batch = batch(size);
    let mut ns = Vec::with_capacity(n);
    for _ in 0..n {
        let t0 = Instant::now();
        for _ in 0..batch {
            f()?;
        }
        ns.push(t0.elapsed().as_nanos() as f64 / batch as f64);
    }
    Ok(ns)
}

pub fn raw_round_trips(n: usize) -> io::Result<Vec<f64>> {
    let p = payload(FRAME_BYTES);
    timed(FRAME_BYTES, n, || {
        let frame = raw_encode(7, black_box(&p));
        black_box(raw_decode(&frame)?);
        Ok(())
    })
}

pub fn raw_frames(plan: Plan) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let n = plan.n(2_000);
    Ok(Measured::new(
        Floor::SerializeRaw,
        format!("raw-lean-frame-round-trip;64KiB;band={}", plan.band.name()),
        raw_round_trips(n)?,
    ))
}

pub fn byte_strings(plan: Plan) -> io::Result<Measured> {
    seam::apply_process_band(plan.band)?;
    let n = plan.n(2_000);
    Ok(Measured::new(
        Floor::SerializeBytes,
        format!(
            "cbor-byte-string-round-trip;64KiB;band={}",
            plan.band.name()
        ),
        round_trip_ns(65_536, n)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_strings_cost_one_header_and_arrays_cost_about_double() {
        let p = payload(1024);
        let bytes = encode(&AsBytes::PaneBytes(p.clone())).unwrap();
        let array = encode(&AsArray::PaneBytes(p)).unwrap();
        assert!(bytes.len() <= 1024 + 16);
        let ratio = array.len() as f64 / 1024.0;
        assert!(ratio > 1.5 && ratio < 2.5, "array inflation {ratio}");
    }

    #[test]
    fn a_raw_lean_frame_carries_its_offset_and_its_bytes_and_nothing_else() {
        let p = payload(FRAME_BYTES);
        let frame = raw_encode(42, &p);
        assert_eq!(frame.len(), 4 + 1 + 8 + FRAME_BYTES);
        assert_eq!(raw_decode(&frame).unwrap(), (42, p));
        assert!(raw_decode(&frame[..12]).is_err());
        let mut wrong = frame;
        wrong[4] = 1;
        assert!(raw_decode(&wrong).is_err());
    }
}
