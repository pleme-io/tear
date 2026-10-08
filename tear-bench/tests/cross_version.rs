mod published;

use published::{
    Fixture, PUBLISHED, Row, SHAPES, digest, head_body, head_reads, integer_array, join_around,
    payloads,
};

const REGENERATE: &str = "cargo run --release --manifest-path tear-bench/compat/Cargo.toml";

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/published-tear-types.json"))
        .expect("the published fixture parses")
}

fn payload(row: &Row) -> Vec<u8> {
    let data = payloads()
        .into_iter()
        .find(|(name, _)| *name == row.payload)
        .map_or_else(
            || panic!("no payload named {}", row.payload),
            |(_, data)| data,
        );
    assert_eq!(
        (data.len(), digest(&data)),
        (row.payload_len, row.payload_blake3.clone()),
        "payload {} drifted from the one {PUBLISHED} encoded",
        row.payload
    );
    data
}

fn head_to_published(row: &Row, data: &[u8]) -> Result<(), String> {
    let head = head_body(&row.shape, data).map_err(|e| e.to_string())?;
    if (head.len(), digest(&head)) == (row.head_len, row.head_blake3.clone()) {
        Ok(())
    } else {
        Err(format!(
            "this tree writes other bytes ({} B) than the {} B {PUBLISHED} read, as a field added to PaneSnapshot or a type it carries does: re-verify against {PUBLISHED} from crates.io with `{REGENERATE}` and commit the fixture it writes",
            head.len(),
            row.head_len
        ))
    }
}

fn published_to_head(row: &Row, data: &[u8]) -> Result<(), String> {
    let body = join_around(&row.published_parts, &integer_array(data));
    if (body.len(), digest(&body)) != (row.published_len, row.published_blake3.clone()) {
        return Err(format!(
            "the rebuilt body is not the one {PUBLISHED} wrote: {} B against {} B",
            body.len(),
            row.published_len
        ));
    }
    head_reads(&row.shape, &body, data)
}

#[test]
fn the_fixture_covers_every_byte_field_and_payload_and_crosses_encodings() {
    let f = fixture();
    assert_eq!(f.published, PUBLISHED);
    let mut want: Vec<(String, String)> = Vec::new();
    for (name, _) in payloads() {
        for shape in SHAPES {
            want.push((shape.to_string(), name.to_string()));
        }
    }
    let have: Vec<(String, String)> = f
        .rows
        .iter()
        .map(|r| (r.shape.clone(), r.payload.clone()))
        .collect();
    assert_eq!(have, want);
    for r in f.rows.iter().filter(|r| r.payload_len > 0) {
        assert!(
            r.published_len > r.head_len,
            "{} {}: {PUBLISHED} wrote {} B and this tree {} B, so the probe would read a byte \
             string with a byte-string reader and prove nothing",
            r.shape,
            r.payload,
            r.published_len,
            r.head_len
        );
    }
    let pane = f
        .rows
        .iter()
        .find(|r| r.shape == "PaneBytes" && r.payload == "64KiB")
        .expect("the 64 KiB PaneBytes row");
    assert_eq!((pane.head_len, pane.published_len), (65_552, 124_800));
}

#[test]
fn every_byte_field_decodes_across_versions_in_both_directions() {
    let mut rows = Vec::new();
    for row in fixture().rows {
        let data = payload(&row);
        rows.push((
            row.shape.clone(),
            row.payload.clone(),
            "head->published",
            head_to_published(&row, &data),
        ));
        rows.push((
            row.shape.clone(),
            row.payload.clone(),
            "published->head",
            published_to_head(&row, &data),
        ));
    }
    for (shape, name, direction, r) in &rows {
        println!(
            "{shape:<15} {name:<12} {direction:<16} {}",
            r.as_ref()
                .map_or_else(|e| format!("FAIL {e}"), |()| "ok".into())
        );
    }
    assert_eq!(rows.len(), 2 * SHAPES.len() * payloads().len());
    let failed: Vec<_> = rows.iter().filter(|r| r.3.is_err()).collect();
    assert!(failed.is_empty(), "{failed:?}");
}
