#![allow(dead_code)]

use serde::{Deserialize, Serialize};

pub const SEED: &str = "cross-version";
pub const PUBLISHED: &str = "tear-types 0.1.35";
pub const FIXTURE: &str = "tests/fixtures/published-tear-types.json";
pub const SHAPES: [&str; 3] = ["PaneBytes", "SendKeys.bytes", "Graphic.data"];
pub const ROWS: usize = 3;
pub const COLS: usize = 5;

fn byte(i: u32, modulus: u32) -> u8 {
    u8::try_from(i % modulus).expect("a modulus below 256")
}

#[must_use]
pub fn payloads() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one", b"x".to_vec()),
        ("esc+high", b"\x1b[31mred\xff\x00\x80\x1b\\".to_vec()),
        ("every-byte", (0..=255u8).collect()),
        ("64KiB", (0..65_536u32).map(|i| byte(i, 251)).collect()),
        (
            "graphic-cap",
            (0..8 * 1024 * 1024u32).map(|i| byte(i, 253)).collect(),
        ),
    ]
}

#[must_use]
pub fn graphics(data: &[u8]) -> &'static [&'static str] {
    if data.len() > 65_536 {
        &["kitty"]
    } else {
        &["kitty", "sixel"]
    }
}

pub type Seen = Vec<(&'static str, String, Vec<u8>, usize, usize, bool)>;

pub const PARAMS: &str = "a=T,f=100,m=0";
pub const AT: (usize, usize) = (1, 2);

#[must_use]
pub fn head_snapshot(data: &[u8]) -> tear_types::PaneSnapshot {
    use tear_types::graphics::{Graphic, GraphicProtocol};
    let mut s = tear_types::PaneSnapshot::blank(ROWS, COLS);
    for kind in graphics(data) {
        s.graphics.push(Graphic {
            protocol: if *kind == "kitty" {
                GraphicProtocol::Kitty
            } else {
                GraphicProtocol::Sixel
            },
            params: PARAMS.into(),
            data: data.to_vec(),
            at_row: AT.0,
            at_col: AT.1,
            truncated: false,
        });
    }
    s
}

#[must_use]
pub fn seen_head(s: &tear_types::PaneSnapshot) -> Seen {
    use tear_types::graphics::GraphicProtocol;
    s.graphics
        .iter()
        .map(|g| {
            let kind = match g.protocol {
                GraphicProtocol::Kitty => "kitty",
                GraphicProtocol::Sixel => "sixel",
            };
            (
                kind,
                g.params.clone(),
                g.data.clone(),
                g.at_row,
                g.at_col,
                g.truncated,
            )
        })
        .collect()
}

pub fn head_body(shape: &str, data: &[u8]) -> std::io::Result<Vec<u8>> {
    use tear_types::wire::{Request, Response, encode};
    match shape {
        "PaneBytes" => encode(&Response::PaneBytes(data.to_vec())),
        "SendKeys.bytes" => encode(&Request::SendKeys {
            id: tear_types::PaneId::from_seed(SEED),
            bytes: data.to_vec(),
        }),
        _ => encode(&Response::PaneSnapshot(head_snapshot(data))),
    }
}

pub fn head_reads(shape: &str, body: &[u8], data: &[u8]) -> Result<(), String> {
    use tear_types::wire::{Request, Response};
    let ok = match shape {
        "PaneBytes" => matches!(
            ciborium::de::from_reader(body),
            Ok(Response::PaneBytes(b)) if b == data
        ),
        "SendKeys.bytes" => matches!(
            ciborium::de::from_reader(body),
            Ok(Request::SendKeys { id, bytes })
                if bytes == data && id == tear_types::PaneId::from_seed(SEED)
        ),
        _ => matches!(
            ciborium::de::from_reader(body),
            Ok(Response::PaneSnapshot(s)) if seen_head(&s) == seen_head(&head_snapshot(data))
        ),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("this tree did not read {PUBLISHED}'s {shape}"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub shape: String,
    pub payload: String,
    pub payload_len: usize,
    pub payload_blake3: String,
    pub published_len: usize,
    pub published_blake3: String,
    pub published_parts: Vec<String>,
    pub head_len: usize,
    pub head_blake3: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fixture {
    pub published: String,
    pub rows: Vec<Row>,
}

#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[must_use]
pub fn integer_array(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(&payload.to_vec(), &mut out).expect("a Vec<u8> encodes");
    out
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

#[must_use]
pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

#[must_use]
pub fn split_around(body: &[u8], needle: &[u8]) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + needle.len() <= body.len() {
        if body[i..i + needle.len()] == *needle {
            parts.push(hex(&body[start..i]));
            i += needle.len();
            start = i;
        } else {
            i += 1;
        }
    }
    parts.push(hex(&body[start..]));
    parts
}

#[must_use]
pub fn join_around(parts: &[String], needle: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(needle);
        }
        out.extend(unhex(p));
    }
    out
}

#[must_use]
pub fn mismatch(got: &[u8], want_len: usize, want_blake3: &str) -> Option<String> {
    if got.len() != want_len {
        Some(format!("{} B against {want_len} B", got.len()))
    } else if digest(got) != want_blake3 {
        Some(format!("the same {want_len} B with another BLAKE3"))
    } else {
        None
    }
}
