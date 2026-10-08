use serde::{Deserializer, Serializer};

pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    #[cfg(feature = "bench-probes")]
    if crate::probes::active(crate::probes::Fault::ArrayEncoder) {
        return serializer.collect_seq(bytes);
    }
    serde_bytes::serialize(bytes, serializer)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    serde_bytes::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use ciborium::Value;

    use crate::graphics::{Graphic, GraphicProtocol};
    use crate::wire::{Request, Response, encode, read_msg, write_msg};
    use crate::{PaneId, PaneSnapshot};

    const MIXED: &[u8] = b"\x1b[31mred\xff\x00\x80\x1b\\ \xe2\x94\x80";

    fn text(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| {
                if i % 80 == 79 {
                    b'\n'
                } else {
                    b'!' + u8::try_from(i % 94).unwrap()
                }
            })
            .collect()
    }

    fn graphic(data: Vec<u8>) -> Graphic {
        Graphic {
            protocol: GraphicProtocol::Kitty,
            params: "a=T,f=100".into(),
            data,
            at_row: 3,
            at_col: 7,
            truncated: false,
        }
    }

    fn snapshot_with(g: Graphic) -> PaneSnapshot {
        let mut s = PaneSnapshot::blank(2, 4);
        s.graphics.push(g);
        s
    }

    fn cbor<T: serde::Serialize>(v: &T) -> Vec<u8> {
        encode(v).unwrap()
    }

    fn legacy(v: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::ser::into_writer(v, &mut out).unwrap();
        out
    }

    fn as_array(bytes: &[u8]) -> Value {
        Value::Array(bytes.iter().map(|b| Value::Integer((*b).into())).collect())
    }

    #[test]
    fn a_64_kib_pane_bytes_frame_is_65_552_bytes_and_a_1_kib_one_1_038() {
        assert_eq!(cbor(&Response::PaneBytes(text(65_536))).len(), 65_552);
        assert_eq!(cbor(&Response::PaneBytes(text(1_024))).len(), 1_038);
        let mut framed = Vec::new();
        write_msg(&mut framed, &Response::PaneBytes(text(65_536))).unwrap();
        assert_eq!(framed.len(), 65_556);
    }

    #[test]
    fn every_byte_field_costs_one_byte_a_byte_plus_its_header() {
        let id = PaneId::from_seed("bytes");
        let keys = |b: Vec<u8>| cbor(&Request::SendKeys { id, bytes: b });
        assert_eq!(
            keys(text(65_536)).len() - keys(Vec::new()).len(),
            65_536 + 4
        );
        let snap = |b: Vec<u8>| cbor(&Response::PaneSnapshot(snapshot_with(graphic(b))));
        assert_eq!(
            snap(text(65_536)).len() - snap(Vec::new()).len(),
            65_536 + 4
        );
        let pane = |b: Vec<u8>| cbor(&Response::PaneBytes(b));
        assert_eq!(pane(vec![0xff; 23]).len() - pane(Vec::new()).len(), 23);
    }

    #[test]
    fn the_three_byte_fields_round_trip_any_byte() {
        let all: Vec<u8> = (0..=255u8).chain(MIXED.iter().copied()).collect();
        let mut buf = Vec::new();
        write_msg(&mut buf, &Response::PaneBytes(all.clone())).unwrap();
        let id = PaneId::from_seed("rt");
        write_msg(
            &mut buf,
            &Request::SendKeys {
                id,
                bytes: all.clone(),
            },
        )
        .unwrap();
        write_msg(
            &mut buf,
            &Response::PaneSnapshot(snapshot_with(graphic(all.clone()))),
        )
        .unwrap();
        let mut cur = std::io::Cursor::new(buf);
        match read_msg::<_, Response>(&mut cur).unwrap() {
            Response::PaneBytes(b) => assert_eq!(b, all),
            other => panic!("{other:?}"),
        }
        match read_msg::<_, Request>(&mut cur).unwrap() {
            Request::SendKeys { id: got, bytes } => {
                assert_eq!(got, id);
                assert_eq!(bytes, all);
            }
            other => panic!("{other:?}"),
        }
        match read_msg::<_, Response>(&mut cur).unwrap() {
            Response::PaneSnapshot(s) => assert_eq!(s.graphics, vec![graphic(all)]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_frame_from_a_peer_that_still_sends_integer_arrays_decodes() {
        let payload = MIXED.to_vec();
        let pane = legacy(&Value::Map(vec![(
            Value::Text("PaneBytes".into()),
            as_array(&payload),
        )]));
        let got: Response = ciborium::de::from_reader(pane.as_slice()).unwrap();
        assert!(matches!(got, Response::PaneBytes(b) if b == payload));

        let id = PaneId::from_seed("legacy");
        let mut keys: Value = ciborium::de::from_reader(
            cbor(&Request::SendKeys {
                id,
                bytes: payload.clone(),
            })
            .as_slice(),
        )
        .unwrap();
        let Value::Map(outer) = &mut keys else {
            panic!("SendKeys is a map")
        };
        let Value::Map(fields) = &mut outer[0].1 else {
            panic!("SendKeys carries its fields as a map")
        };
        let bytes = fields
            .iter_mut()
            .find(|(k, _)| k.as_text() == Some("bytes"))
            .expect("the bytes field");
        assert!(bytes.1.is_bytes(), "the new encoder writes a byte string");
        bytes.1 = as_array(&payload);
        let got: Request = ciborium::de::from_reader(legacy(&keys).as_slice()).unwrap();
        assert!(matches!(got, Request::SendKeys { bytes, .. } if bytes == payload));

        let mut g: Value =
            ciborium::de::from_reader(cbor(&graphic(payload.clone())).as_slice()).unwrap();
        let Value::Map(fields) = &mut g else {
            panic!("Graphic is a map")
        };
        let data = fields
            .iter_mut()
            .find(|(k, _)| k.as_text() == Some("data"))
            .expect("the data field");
        assert!(data.1.is_bytes(), "the new encoder writes a byte string");
        data.1 = as_array(&payload);
        let got: Graphic = ciborium::de::from_reader(legacy(&g).as_slice()).unwrap();
        assert_eq!(got, graphic(payload));
    }

    #[test]
    fn a_graphic_at_the_payload_cap_round_trips_inside_its_snapshot() {
        let big = text(crate::graphics::GRAPHIC_PAYLOAD_MAX);
        let body = cbor(&Response::PaneSnapshot(snapshot_with(graphic(big.clone()))));
        assert!(body.len() < crate::graphics::GRAPHIC_PAYLOAD_MAX + 4_096);
        match ciborium::de::from_reader::<Response, _>(body.as_slice()).unwrap() {
            Response::PaneSnapshot(s) => assert_eq!(s.graphics[0].data, big),
            other => panic!("{other:?}"),
        }
    }

    fn serde_items(src: &str) -> Vec<(String, Vec<String>)> {
        let lines: Vec<&str> = src.lines().collect();
        let mut out = Vec::new();
        let mut serializes = false;
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let item = ["pub struct ", "pub enum ", "struct ", "enum "]
                .iter()
                .find_map(|k| line.strip_prefix(k));
            if line.trim_start().starts_with("#[derive(") {
                serializes = line.contains("Serialize");
            } else if let (true, Some(rest)) = (serializes, item) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let mut body = vec![line.to_string()];
                if !line.trim_end().ends_with(';') {
                    i += 1;
                    while i < lines.len() && lines[i] != "}" {
                        body.push(lines[i].to_string());
                        i += 1;
                    }
                }
                out.push((name, body));
                serializes = false;
            } else if !line.trim_start().starts_with("#[")
                && !line.trim_start().starts_with("///")
                && !line.trim().is_empty()
            {
                serializes = false;
            }
            i += 1;
        }
        out
    }

    fn unmarked_byte_fields(src: &str) -> (usize, Vec<String>) {
        let marked = |l: &str| l.contains("serde(with = \"crate::byte_string\")");
        let mut found = 0;
        let mut bare = Vec::new();
        for (item, body) in serde_items(src) {
            for (n, line) in body.iter().enumerate() {
                if !line.contains("Vec<u8>") {
                    continue;
                }
                found += 1;
                let attribute_above = n > 0
                    && body[n - 1].trim_start().starts_with("#[")
                    && !body[n - 1].contains("Vec<u8>")
                    && marked(&body[n - 1]);
                if !(marked(line) || attribute_above) {
                    bare.push(format!("{item}: {}", line.trim()));
                }
            }
        }
        (found, bare)
    }

    fn sources(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|x| x == "rs") {
                out.push((
                    path.display().to_string(),
                    std::fs::read_to_string(&path).unwrap(),
                ));
            }
        }
    }

    #[test]
    fn every_byte_field_of_a_serde_type_travels_as_a_byte_string() {
        let mut files = Vec::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        assert!(files.len() > 20, "the scan read {} files", files.len());
        let mut found = 0;
        let mut bare = Vec::new();
        for (file, src) in &files {
            let (n, b) = unmarked_byte_fields(src);
            found += n;
            bare.extend(b.into_iter().map(|b| format!("{file} {b}")));
        }
        assert_eq!(
            found, 3,
            "the scan found {found} byte fields where PaneBytes, SendKeys.bytes and \
             Graphic.data are three: either a byte field was added, which needs a row here, \
             or the parser has broken and would report false safety"
        );
        assert!(
            bare.is_empty(),
            "a byte field without #[serde(with = \"crate::byte_string\")] encodes as a CBOR \
             integer array, ~2x the bytes and 60-173x the round trip: {bare:?}"
        );
    }

    #[test]
    fn the_scan_sees_a_bare_byte_field() {
        let src = "#[derive(Clone, Serialize, Deserialize)]\npub enum Probe {\n    \
                   Marked(#[serde(with = \"crate::byte_string\")] Vec<u8>),\n    \
                   Bare(Vec<u8>),\n}\n\n#[derive(Serialize)]\npub struct Field {\n    \
                   #[serde(with = \"crate::byte_string\")]\n    pub a: Vec<u8>,\n    \
                   pub b: Vec<u8>,\n}\n\n#[derive(Clone)]\npub struct NotWire {\n    \
                   pub c: Vec<u8>,\n}\n\n#[derive(Serialize)]\n\
                   pub struct Tuple(pub Vec<u8>);\n\nfn after() {}\n";
        let (found, bare) = unmarked_byte_fields(src);
        assert_eq!(found, 5);
        assert_eq!(
            bare,
            vec![
                "Probe: Bare(Vec<u8>),",
                "Field: pub b: Vec<u8>,",
                "Tuple: pub struct Tuple(pub Vec<u8>);"
            ]
        );
    }
}
