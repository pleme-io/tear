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
    use std::collections::BTreeSet;

    use ciborium::Value;
    use syn::punctuated::Punctuated;
    use syn::{
        Attribute, Expr, Fields, GenericArgument, Item, Lit, Meta, PathArguments, Token, Type,
    };

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

    fn derive_lists(meta: &Meta) -> Vec<syn::MetaList> {
        match meta {
            Meta::List(l) if l.path.is_ident("derive") => vec![l.clone()],
            Meta::List(l) if l.path.is_ident("cfg_attr") => l
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .expect("a cfg_attr parses")
                .iter()
                .skip(1)
                .flat_map(derive_lists)
                .collect(),
            _ => Vec::new(),
        }
    }

    fn derives_serde(attrs: &[Attribute]) -> bool {
        attrs.iter().flat_map(|a| derive_lists(&a.meta)).any(|l| {
            l.parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)
                .expect("a derive list parses")
                .iter()
                .filter_map(|p| p.segments.last())
                .any(|s| s.ident == "Serialize" || s.ident == "Deserialize")
        })
    }

    fn test_only(attrs: &[Attribute]) -> bool {
        attrs.iter().any(|a| {
            a.path().is_ident("cfg") && a.parse_args::<syn::Ident>().is_ok_and(|i| i == "test")
        })
    }

    fn through_the_module(attrs: &[Attribute]) -> bool {
        let module = |m: &Meta| {
            let Meta::NameValue(nv) = m else { return false };
            let Expr::Lit(e) = &nv.value else {
                return false;
            };
            nv.path.is_ident("with")
                && matches!(&e.lit, Lit::Str(s) if s.value() == "crate::byte_string")
        };
        attrs
            .iter()
            .filter(|a| a.path().is_ident("serde"))
            .any(|a| {
                a.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                    .expect("a serde attribute parses")
                    .iter()
                    .any(module)
            })
    }

    fn is_u8(ty: &Type) -> bool {
        matches!(ty, Type::Path(p) if p.qself.is_none() && p.path.is_ident("u8"))
    }

    fn carries_bytes(ty: &Type, aliases: &BTreeSet<String>) -> bool {
        let element = |t: &Type| is_u8(t) || carries_bytes(t, aliases);
        match ty {
            Type::Slice(s) => element(&s.elem),
            Type::Array(a) => element(&a.elem),
            Type::Reference(r) => carries_bytes(&r.elem, aliases),
            Type::Paren(p) => carries_bytes(&p.elem, aliases),
            Type::Group(g) => carries_bytes(&g.elem, aliases),
            Type::Tuple(t) => t.elems.iter().any(|e| carries_bytes(e, aliases)),
            Type::Path(p) => {
                p.path
                    .segments
                    .last()
                    .is_some_and(|s| aliases.contains(&s.ident.to_string()))
                    || p.path.segments.iter().any(|s| {
                        let PathArguments::AngleBracketed(args) = &s.arguments else {
                            return false;
                        };
                        let sequence = s.ident == "Vec" || s.ident == "VecDeque";
                        args.args.iter().any(|g| {
                            matches!(g, GenericArgument::Type(t)
                                if (sequence && is_u8(t)) || carries_bytes(t, aliases))
                        })
                    })
            }
            _ => false,
        }
    }

    fn items_of<'a>(items: &'a [Item], out: &mut Vec<&'a Item>) {
        for item in items {
            match item {
                Item::Mod(m) => {
                    if let (false, Some((_, inner))) = (test_only(&m.attrs), &m.content) {
                        items_of(inner, out);
                    }
                }
                other => out.push(other),
            }
        }
    }

    fn scan(files: &[syn::File]) -> (usize, Vec<String>) {
        let mut items = Vec::new();
        for f in files {
            items_of(&f.items, &mut items);
        }
        let mut aliases = BTreeSet::new();
        loop {
            let before = aliases.len();
            for item in &items {
                if let Item::Type(t) = item
                    && carries_bytes(&t.ty, &aliases)
                {
                    aliases.insert(t.ident.to_string());
                }
            }
            if aliases.len() == before {
                break;
            }
        }
        let mut found = 0;
        let mut bare = Vec::new();
        let mut fields = |owner: String, fs: &Fields| {
            for (n, f) in fs.iter().enumerate() {
                if !carries_bytes(&f.ty, &aliases) {
                    continue;
                }
                found += 1;
                if !through_the_module(&f.attrs) {
                    let name = f
                        .ident
                        .as_ref()
                        .map_or_else(|| n.to_string(), ToString::to_string);
                    bare.push(format!("{owner}.{name}"));
                }
            }
        };
        for item in items {
            match item {
                Item::Struct(s) if derives_serde(&s.attrs) && !test_only(&s.attrs) => {
                    fields(s.ident.to_string(), &s.fields);
                }
                Item::Enum(e) if derives_serde(&e.attrs) && !test_only(&e.attrs) => {
                    for v in &e.variants {
                        fields(format!("{}::{}", e.ident, v.ident), &v.fields);
                    }
                }
                _ => {}
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
        let parsed: Vec<syn::File> = files
            .iter()
            .map(|(file, src)| syn::parse_file(src).unwrap_or_else(|e| panic!("{file}: {e}")))
            .collect();
        let (found, bare) = scan(&parsed);
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

    const PROBE: &str = r#"
#[derive(Clone, Serialize, Deserialize)]
pub enum Probe {
    Marked(#[serde(with = "crate::byte_string")] Vec<u8>),
    Bare(Vec<u8>),
}

#[derive(Serialize)]
pub struct Field {
    #[serde(with = "crate::byte_string")]
    pub a: Vec<u8>,
    pub b: Vec<u8>,
}

#[derive(Clone)]
pub struct NotWire {
    pub c: Vec<u8>,
}

#[derive(Serialize)]
pub struct Tuple(pub Vec<u8>);

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
)]
pub(crate) struct SplitDerive {
    pub d: Vec<u8>,
}

#[derive(Serialize)]
#[serde(
    rename_all = "snake_case",
    default
)]
struct AttributeBetween {
    e: Box<[u8]>,
}

#[cfg_attr(feature = "wire", derive(Serialize))]
// a comment between the derive and the item
struct Conditional {
    f: Option<Vec<u8>>,
    #[serde(with = "serde_bytes")]
    g: Vec<u8>,
}

pub type Blob = Vec<u8>;

mod nested {
    #[derive(Deserialize)]
    pub struct Inner {
        pub h: [u8; 16],
        pub i: super::Blob,
        pub j: Vec<String>,
    }
}

#[cfg(test)]
mod tests {
    #[derive(Serialize)]
    struct TestOnly {
        k: Vec<u8>,
    }
}

fn after() {}
"#;

    #[test]
    fn the_scan_sees_a_bare_byte_field() {
        let (found, bare) = scan(&[syn::parse_file(PROBE).unwrap()]);
        assert_eq!(
            bare,
            vec![
                "Probe::Bare.0",
                "Field.b",
                "Tuple.0",
                "SplitDerive.d",
                "AttributeBetween.e",
                "Conditional.f",
                "Conditional.g",
                "Inner.h",
                "Inner.i",
            ]
        );
        assert_eq!(found, 11);
    }
}
