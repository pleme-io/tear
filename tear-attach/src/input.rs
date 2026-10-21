use tear_picker::{PickerKey, PickerKeyClass};

use crate::bindings::Bindings;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputAction {
    Forward(Vec<u8>),
    OpenPicker,
    Detach,
    Rest(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct InputFilter {
    bindings: Bindings,
    pending_prefix: bool,
}

impl InputFilter {
    #[must_use]
    pub fn new(bindings: Bindings) -> Self {
        Self {
            bindings,
            pending_prefix: false,
        }
    }

    #[must_use]
    pub fn prefix_pending(&self) -> bool {
        self.pending_prefix
    }

    pub fn reset(&mut self) {
        self.pending_prefix = false;
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputAction> {
        let mut out = Vec::new();
        let mut run: Vec<u8> = Vec::new();
        let flush = |run: &mut Vec<u8>, out: &mut Vec<InputAction>| {
            if !run.is_empty() {
                out.push(InputAction::Forward(std::mem::take(run)));
            }
        };
        for (i, &b) in bytes.iter().enumerate() {
            if self.pending_prefix {
                self.pending_prefix = false;
                if b == self.bindings.detach.0 {
                    flush(&mut run, &mut out);
                    out.push(InputAction::Detach);
                    return out;
                }
                if b == self.bindings.prefix.0 {
                    run.push(b);
                } else {
                    run.push(self.bindings.prefix.0);
                    run.push(b);
                }
                continue;
            }
            if Some(b) == self.bindings.picker.map(|k| k.0) {
                flush(&mut run, &mut out);
                out.push(InputAction::OpenPicker);
                let rest = bytes[i + 1..].to_vec();
                if !rest.is_empty() {
                    out.push(InputAction::Rest(rest));
                }
                return out;
            }
            if b == self.bindings.prefix.0 {
                self.pending_prefix = true;
                continue;
            }
            run.push(b);
        }
        flush(&mut run, &mut out);
        out
    }
}

#[must_use]
pub fn decode_picker_keys(bytes: &[u8]) -> Vec<PickerKey> {
    let mut keys = Vec::new();
    if bytes == [0x1b] {
        keys.push(PickerKey::class(PickerKeyClass::Escape));
        return keys;
    }
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => {
                if bytes.get(i + 1) == Some(&b'[') || bytes.get(i + 1) == Some(&b'O') {
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    match bytes.get(j) {
                        Some(b'A') => keys.push(PickerKey::class(PickerKeyClass::Up)),
                        Some(b'B') => keys.push(PickerKey::class(PickerKeyClass::Down)),
                        _ => {}
                    }
                    i = j + 1;
                } else {
                    keys.push(PickerKey::class(PickerKeyClass::Escape));
                    i += 1;
                }
            }
            0x0d | 0x0a => {
                keys.push(PickerKey::class(PickerKeyClass::Enter));
                i += 1;
            }
            0x7f | 0x08 => {
                keys.push(PickerKey::class(PickerKeyClass::Backspace));
                i += 1;
            }
            0x10 => {
                keys.push(PickerKey::class(PickerKeyClass::CtrlP));
                i += 1;
            }
            0x0e => {
                keys.push(PickerKey::class(PickerKeyClass::CtrlN));
                i += 1;
            }
            0x05 => {
                keys.push(PickerKey::class(PickerKeyClass::CtrlE));
                i += 1;
            }
            0x04 => {
                keys.push(PickerKey::class(PickerKeyClass::CtrlD));
                i += 1;
            }
            0x12 => {
                keys.push(PickerKey::class(PickerKeyClass::CtrlR));
                i += 1;
            }
            0x00..=0x1f => {
                keys.push(PickerKey {
                    class: PickerKeyClass::Other,
                    plain: false,
                    text: None,
                });
                i += 1;
            }
            _ => {
                let len = utf8_len(b).min(bytes.len() - i);
                if let Ok(s) = std::str::from_utf8(&bytes[i..i + len]) {
                    keys.push(PickerKey::text(s));
                }
                i += len.max(1);
            }
        }
    }
    keys
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tear_config::TearConfig;

    fn filter() -> InputFilter {
        InputFilter::new(Bindings::from_config(&TearConfig::default()).unwrap())
    }

    #[test]
    fn plain_bytes_are_forwarded_untouched() {
        assert_eq!(
            filter().feed(b"ls -la\r"),
            vec![InputAction::Forward(b"ls -la\r".to_vec())]
        );
    }

    #[test]
    fn the_picker_key_opens_the_picker_and_is_not_forwarded() {
        let acts = filter().feed(&[b'a', 0x13, b'b']);
        assert_eq!(
            acts,
            vec![
                InputAction::Forward(b"a".to_vec()),
                InputAction::OpenPicker,
                InputAction::Rest(b"b".to_vec())
            ]
        );
    }

    #[test]
    fn prefix_then_d_detaches() {
        assert_eq!(filter().feed(&[0x02, b'd']), vec![InputAction::Detach]);
    }

    #[test]
    fn the_prefix_state_survives_a_split_read() {
        let mut f = filter();
        assert!(f.feed(&[0x02]).is_empty());
        assert!(f.prefix_pending());
        assert_eq!(f.feed(&[b'd']), vec![InputAction::Detach]);
    }

    #[test]
    fn prefix_twice_sends_one_literal_prefix() {
        assert_eq!(
            filter().feed(&[0x02, 0x02]),
            vec![InputAction::Forward(vec![0x02])]
        );
    }

    #[test]
    fn prefix_then_other_forwards_both_never_swallows() {
        assert_eq!(
            filter().feed(&[0x02, b'x']),
            vec![InputAction::Forward(vec![0x02, b'x'])]
        );
    }

    #[test]
    fn a_refused_key_reaches_the_pane() {
        assert_eq!(
            filter().feed(&[0x03, 0x04]),
            vec![InputAction::Forward(vec![0x03, 0x04])]
        );
    }

    #[test]
    fn a_disabled_picker_key_passes_through() {
        let mut c = TearConfig::default();
        c.attach.picker_enabled = false;
        let mut f = InputFilter::new(Bindings::from_config(&c).unwrap());
        assert_eq!(f.feed(&[0x13]), vec![InputAction::Forward(vec![0x13])]);
    }

    #[test]
    fn decode_arrows_enter_and_text() {
        let k = decode_picker_keys(b"\x1b[Ahi\r");
        assert_eq!(k[0].class, PickerKeyClass::Up);
        assert_eq!(k[1].text.as_deref(), Some("h"));
        assert_eq!(k[2].text.as_deref(), Some("i"));
        assert_eq!(k[3].class, PickerKeyClass::Enter);
    }

    #[test]
    fn a_lone_escape_is_escape_but_a_csi_is_not() {
        assert_eq!(decode_picker_keys(&[0x1b])[0].class, PickerKeyClass::Escape);
        assert!(decode_picker_keys(b"\x1b[3~").is_empty());
    }

    #[test]
    fn utf8_text_decodes_whole() {
        let k = decode_picker_keys("é".as_bytes());
        assert_eq!(k.len(), 1);
        assert_eq!(k[0].text.as_deref(), Some("é"));
    }
}
