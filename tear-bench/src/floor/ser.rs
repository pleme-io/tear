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
    let p = payload(size);
    let msg = AsBytes::PaneBytes(p);
    let batch = if size >= 65_536 { 16 } else { 256 };
    let mut ns = Vec::with_capacity(n);
    for _ in 0..n {
        let t0 = Instant::now();
        for _ in 0..batch {
            let e = encode(black_box(&msg))?;
            let back: AsBytes = ciborium::de::from_reader(e.as_slice())
                .map_err(|err| io::Error::other(err.to_string()))?;
            black_box(back);
        }
        ns.push(t0.elapsed().as_nanos() as f64 / f64::from(batch));
    }
    Ok(ns)
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
}
