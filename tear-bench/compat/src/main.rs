#[path = "../../tests/published/mod.rs"]
mod published;

use std::process::ExitCode;

use published::{
    AT, COLS, FIXTURE, Fixture, PARAMS, PUBLISHED, ROWS, Row, SEED, SHAPES, Seen, digest, graphics,
    head_body, head_reads, head_snapshot, integer_array, join_around, payloads, seen_head,
    split_around,
};
use tear_types_published as prev;

fn prev_snapshot(data: &[u8]) -> prev::PaneSnapshot {
    let mut s = prev::PaneSnapshot::blank(ROWS, COLS);
    for kind in graphics(data) {
        s.graphics.push(prev::graphics::Graphic {
            protocol: if *kind == "kitty" {
                prev::graphics::GraphicProtocol::Kitty
            } else {
                prev::graphics::GraphicProtocol::Sixel
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

fn seen_prev(s: &prev::PaneSnapshot) -> Seen {
    s.graphics
        .iter()
        .map(|g| {
            let kind = match g.protocol {
                prev::graphics::GraphicProtocol::Kitty => "kitty",
                prev::graphics::GraphicProtocol::Sixel => "sixel",
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

fn prev_body(shape: &str, data: &[u8]) -> std::io::Result<Vec<u8>> {
    match shape {
        "PaneBytes" => prev::wire::encode(&prev::wire::Response::PaneBytes(data.to_vec())),
        "SendKeys.bytes" => prev::wire::encode(&prev::wire::Request::SendKeys {
            id: prev::PaneId::from_seed(SEED),
            bytes: data.to_vec(),
        }),
        _ => prev::wire::encode(&prev::wire::Response::PaneSnapshot(prev_snapshot(data))),
    }
}

fn published_reads(shape: &str, body: &[u8], data: &[u8]) -> Result<(), String> {
    let ok = match shape {
        "PaneBytes" => matches!(
            ciborium::de::from_reader(body),
            Ok(prev::wire::Response::PaneBytes(b)) if b == data
        ),
        "SendKeys.bytes" => matches!(
            ciborium::de::from_reader(body),
            Ok(prev::wire::Request::SendKeys { id, bytes })
                if bytes == data && id == prev::PaneId::from_seed(SEED)
        ),
        _ => matches!(
            ciborium::de::from_reader(body),
            Ok(prev::wire::Response::PaneSnapshot(s))
                if seen_prev(&s) == seen_head(&head_snapshot(data))
        ),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{PUBLISHED} did not read this tree's {shape}"))
    }
}

fn row(shape: &str, payload: &str, data: &[u8]) -> Result<Row, String> {
    let head = head_body(shape, data).map_err(|e| e.to_string())?;
    let published = prev_body(shape, data).map_err(|e| e.to_string())?;
    published_reads(shape, &head, data)?;
    head_reads(shape, &published, data)?;
    let array = integer_array(data);
    let parts = split_around(&published, &array);
    if join_around(&parts, &array) != published {
        return Err(format!(
            "{shape} {payload}: the published body does not rebuild"
        ));
    }
    Ok(Row {
        shape: shape.into(),
        payload: payload.into(),
        payload_len: data.len(),
        payload_blake3: digest(data),
        published_len: published.len(),
        published_blake3: digest(&published),
        published_parts: parts,
        head_len: head.len(),
        head_blake3: digest(&head),
    })
}

fn main() -> ExitCode {
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(FIXTURE);
    let mut rows = Vec::new();
    let mut failed = Vec::new();
    for (payload, data) in payloads() {
        for shape in SHAPES {
            match row(shape, payload, &data) {
                Ok(r) => {
                    println!(
                        "{shape:<15} {payload:<12} published {:>9} B  head {:>9} B  both ways ok",
                        r.published_len, r.head_len
                    );
                    rows.push(r);
                }
                Err(e) => failed.push(format!("{shape} {payload}: {e}")),
            }
        }
    }
    if !failed.is_empty() {
        eprintln!("refusing to write {}: {failed:?}", out.display());
        return ExitCode::FAILURE;
    }
    let fixture = Fixture {
        published: PUBLISHED.into(),
        rows,
    };
    let body = serde_json::to_string_pretty(&fixture).expect("the fixture serializes") + "\n";
    match std::fs::write(&out, body) {
        Ok(()) => {
            println!("wrote {}", out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}: {e}", out.display());
            ExitCode::FAILURE
        }
    }
}
