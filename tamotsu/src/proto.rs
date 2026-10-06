use std::io::{self, Read, Write};

use makimono::Ending;
use serde::{Deserialize, Serialize};

pub const PROTO: u32 = 1;

const TAG_CONTROL: u8 = 1;
const TAG_DATA: u8 = 2;
const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToHolder {
    Hello { proto: u32 },
    Attach { from: u64 },
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    End,
    Status,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderStatus {
    pub pid: u32,
    pub child_pid: Option<u32>,
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromHolder {
    Hello {
        proto: u32,
        pid: u32,
        child_pid: Option<u32>,
    },
    Bytes {
        at: u64,
        data: Vec<u8>,
    },
    Exited {
        ending: Ending,
    },
    Status(HolderStatus),
    Refused {
        reason: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ToControl {
    Hello { proto: u32 },
    Attach { from: u64 },
    Resize { cols: u16, rows: u16 },
    End,
    Status,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum FromControl {
    Hello {
        proto: u32,
        pid: u32,
        child_pid: Option<u32>,
    },
    Exited {
        ending: Ending,
    },
    Status(HolderStatus),
    Refused {
        reason: String,
    },
}

pub fn write_to_holder<W: Write>(w: &mut W, msg: &ToHolder) -> io::Result<()> {
    let control = match msg {
        ToHolder::Write(data) => return write_frame(w, TAG_DATA, &[data]),
        ToHolder::Hello { proto } => ToControl::Hello { proto: *proto },
        ToHolder::Attach { from } => ToControl::Attach { from: *from },
        ToHolder::Resize { cols, rows } => ToControl::Resize {
            cols: *cols,
            rows: *rows,
        },
        ToHolder::End => ToControl::End,
        ToHolder::Status => ToControl::Status,
    };
    write_frame(w, TAG_CONTROL, &[&cbor(&control)?])
}

pub fn read_to_holder<R: Read>(r: &mut R) -> io::Result<ToHolder> {
    let (tag, body) = read_frame(r)?;
    match tag {
        TAG_DATA => Ok(ToHolder::Write(body)),
        TAG_CONTROL => Ok(match uncbor::<ToControl>(&body)? {
            ToControl::Hello { proto } => ToHolder::Hello { proto },
            ToControl::Attach { from } => ToHolder::Attach { from },
            ToControl::Resize { cols, rows } => ToHolder::Resize { cols, rows },
            ToControl::End => ToHolder::End,
            ToControl::Status => ToHolder::Status,
        }),
        other => Err(bad_tag(other)),
    }
}

pub fn write_from_holder<W: Write>(w: &mut W, msg: &FromHolder) -> io::Result<()> {
    let control = match msg {
        FromHolder::Bytes { at, data } => {
            return write_frame(w, TAG_DATA, &[&at.to_be_bytes(), data]);
        }
        FromHolder::Hello {
            proto,
            pid,
            child_pid,
        } => FromControl::Hello {
            proto: *proto,
            pid: *pid,
            child_pid: *child_pid,
        },
        FromHolder::Exited { ending } => FromControl::Exited {
            ending: ending.clone(),
        },
        FromHolder::Status(s) => FromControl::Status(s.clone()),
        FromHolder::Refused { reason } => FromControl::Refused {
            reason: reason.clone(),
        },
    };
    write_frame(w, TAG_CONTROL, &[&cbor(&control)?])
}

pub fn read_from_holder<R: Read>(r: &mut R) -> io::Result<FromHolder> {
    let (tag, body) = read_frame(r)?;
    match tag {
        TAG_DATA => {
            if body.len() < 8 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "data frame shorter than its offset",
                ));
            }
            let mut at = [0u8; 8];
            at.copy_from_slice(&body[..8]);
            Ok(FromHolder::Bytes {
                at: u64::from_be_bytes(at),
                data: body[8..].to_vec(),
            })
        }
        TAG_CONTROL => Ok(match uncbor::<FromControl>(&body)? {
            FromControl::Hello {
                proto,
                pid,
                child_pid,
            } => FromHolder::Hello {
                proto,
                pid,
                child_pid,
            },
            FromControl::Exited { ending } => FromHolder::Exited { ending },
            FromControl::Status(s) => FromHolder::Status(s),
            FromControl::Refused { reason } => FromHolder::Refused { reason },
        }),
        other => Err(bad_tag(other)),
    }
}

fn write_frame<W: Write>(w: &mut W, tag: u8, parts: &[&[u8]]) -> io::Result<()> {
    let len: usize = 1 + parts.iter().map(|p| p.len()).sum::<usize>();
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    let len = u32::try_from(len).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut buf = Vec::with_capacity(4 + len as usize);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(tag);
    for p in parts {
        buf.extend_from_slice(p);
    }
    w.write_all(&buf)?;
    w.flush()
}

fn read_frame<R: Read>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} out of bounds"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let tag = body.remove(0);
    Ok((tag, body))
}

fn cbor<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(out)
}

fn uncbor<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    ciborium::from_reader(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

fn bad_tag(tag: u8) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("unknown frame tag {tag}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_to_the_holder_round_trips() {
        let msgs = [
            ToHolder::Hello { proto: PROTO },
            ToHolder::Attach { from: 42 },
            ToHolder::Write(b"ls -la\r".to_vec()),
            ToHolder::Write(Vec::new()),
            ToHolder::Resize { cols: 120, rows: 40 },
            ToHolder::End,
            ToHolder::Status,
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            write_to_holder(&mut wire, m).unwrap();
        }
        let mut r = wire.as_slice();
        for m in &msgs {
            assert_eq!(&read_to_holder(&mut r).unwrap(), m);
        }
        assert!(r.is_empty());
    }

    #[test]
    fn every_message_from_the_holder_round_trips() {
        let msgs = [
            FromHolder::Hello {
                proto: PROTO,
                pid: 7,
                child_pid: Some(8),
            },
            FromHolder::Bytes {
                at: u64::MAX - 3,
                data: vec![0x1b, b'[', b'm'],
            },
            FromHolder::Exited {
                ending: Ending::Exited { code: Some(2) },
            },
            FromHolder::Exited {
                ending: Ending::by_control(),
            },
            FromHolder::Status(HolderStatus {
                pid: 1,
                child_pid: None,
                start: 5,
                end: 9,
            }),
            FromHolder::Refused {
                reason: "proto".into(),
            },
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            write_from_holder(&mut wire, m).unwrap();
        }
        let mut r = wire.as_slice();
        for m in &msgs {
            assert_eq!(&read_from_holder(&mut r).unwrap(), m);
        }
    }

    #[test]
    fn a_truncated_or_oversized_frame_is_an_error_not_a_message() {
        let mut wire = Vec::new();
        write_to_holder(&mut wire, &ToHolder::Attach { from: 1 }).unwrap();
        wire.truncate(wire.len() - 1);
        assert!(read_to_holder(&mut wire.as_slice()).is_err());
        let huge = (u32::MAX).to_be_bytes();
        assert!(read_to_holder(&mut huge.as_slice()).is_err());
        let zero = 0u32.to_be_bytes();
        assert!(read_from_holder(&mut zero.as_slice()).is_err());
    }
}
