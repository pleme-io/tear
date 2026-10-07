use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;

const WORDS: [&str; 8] = [
    "Compiling",
    "Checking",
    "Building",
    "Running",
    "Finished",
    "warning:",
    "Downloaded",
    "Fresh",
];

const CRATES: [&str; 12] = [
    "tear-core",
    "tear-client",
    "portable-pty",
    "ciborium",
    "serde_json",
    "parking_lot",
    "makimono",
    "tamotsu",
    "vte",
    "unicode-width",
    "garasu",
    "madori",
];

pub const LOG_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

fn lcg(x: u64) -> u64 {
    x.wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}

pub fn log_lines(seed: u64) -> impl Iterator<Item = Vec<u8>> {
    let mut seed = seed;
    let mut line = 0u64;
    std::iter::from_fn(move || {
        seed = lcg(seed);
        let word = WORDS[usize::try_from((seed >> 33) % WORDS.len() as u64).unwrap_or(0)];
        let color = if word.starts_with("warn") {
            "\x1b[1;33m"
        } else {
            "\x1b[1;32m"
        };
        let krate = CRATES[usize::try_from((seed >> 41) % CRATES.len() as u64).unwrap_or(0)];
        let pad = (seed >> 13) % 48;
        let tail: String = (0..pad)
            .map(|i| char::from(b'a' + u8::try_from((seed >> (i % 50)) % 26).unwrap_or(0)))
            .collect();
        let l = format!(
            "{color}{word:>12}\x1b[0m {krate} v0.{}.{} (/src/{krate}) #{line} {tail}\r\n",
            (seed >> 7) % 40,
            (seed >> 3) % 99
        );
        line += 1;
        Some(l.into_bytes())
    })
}

#[must_use]
pub fn log_size(target: u64) -> u64 {
    let mut n = 0u64;
    for l in log_lines(LOG_SEED) {
        if n >= target {
            break;
        }
        n += l.len() as u64;
    }
    n
}

pub fn log_file(path: &Path, target: u64) -> io::Result<u64> {
    let want = log_size(target);
    if fs::metadata(path).is_ok_and(|md| md.len() == want) {
        return Ok(want);
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut w = BufWriter::new(File::create(path)?);
    let mut n = 0u64;
    for l in log_lines(LOG_SEED) {
        if n >= target {
            break;
        }
        w.write_all(&l)?;
        n += l.len() as u64;
    }
    w.flush()?;
    Ok(n)
}

pub fn rows_file(path: &Path, lines: usize) -> io::Result<u64> {
    let mut buf = Vec::with_capacity(lines * 82);
    for i in 0..lines {
        let mut l = format!("row {i:06} ");
        while l.len() < 80 {
            l.push(char::from(
                b'a' + u8::try_from((i + l.len()) % 26).unwrap_or(0),
            ));
        }
        buf.extend_from_slice(l.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, &buf)?;
    Ok(buf.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_64_mib_flood_file_is_the_passes_byte_count() {
        assert_eq!(log_size(64 * 1024 * 1024), 67_108_905);
    }

    #[test]
    fn the_first_line_is_the_passes_first_line() {
        let first = log_lines(LOG_SEED).next().unwrap();
        let text = String::from_utf8(first).unwrap();
        assert!(text.ends_with("\r\n"));
        assert!(text.contains(" #0 "));
        assert!(text.starts_with("\x1b[1;3"));
    }
}
