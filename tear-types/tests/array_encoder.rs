#![cfg(feature = "bench-probes")]

use tear_types::graphics::{Graphic, GraphicProtocol};
use tear_types::probes::{Fault, arm};
use tear_types::wire::{Request, Response, encode};
use tear_types::{PaneId, PaneSnapshot};

fn sizes(body: &[u8]) -> [usize; 3] {
    let mut snap = PaneSnapshot::blank(1, 1);
    snap.graphics.push(Graphic {
        protocol: GraphicProtocol::Sixel,
        params: String::new(),
        data: body.to_vec(),
        at_row: 0,
        at_col: 0,
        truncated: false,
    });
    [
        encode(&Response::PaneBytes(body.to_vec())).unwrap().len(),
        encode(&Request::SendKeys {
            id: PaneId::from_seed("array"),
            bytes: body.to_vec(),
        })
        .unwrap()
        .len(),
        encode(&Response::PaneSnapshot(snap)).unwrap().len(),
    ]
}

#[test]
fn the_array_encoder_fault_writes_every_byte_field_as_integers_and_still_decodes() {
    let body = vec![b'x'; 65_536];
    let clean = sizes(&body);
    assert_eq!(clean[0], 65_552);
    arm(&[Fault::ArrayEncoder]);
    let faulted = sizes(&body);
    let frame = encode(&Response::PaneBytes(body.clone())).unwrap();
    arm(&[]);
    for (c, f) in clean.iter().zip(faulted) {
        assert_eq!(f - c, 65_536, "each 0x78 costs a second byte as an integer");
    }
    let back: Response = ciborium::de::from_reader(frame.as_slice()).unwrap();
    assert!(matches!(back, Response::PaneBytes(b) if b == body));
    assert_eq!(sizes(&body), clean, "disarming restores byte strings");
}
