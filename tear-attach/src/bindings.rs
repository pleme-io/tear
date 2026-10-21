use tear_config::TearConfig;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Key(pub u8);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BindingError {
    #[error(
        "{role} key `{spec}` is not a key tear understands (use ctrl+<letter>, a single character, esc, enter, tab, space or backspace)"
    )]
    Unparseable { role: &'static str, spec: String },
    #[error(
        "{role} key `{spec}` is on the refuse list: the pane needs it, so tear will not bind it"
    )]
    Refused { role: &'static str, spec: String },
    #[error(
        "the detach prefix is empty, so there is no way to detach; set attach.detach_prefix or prefix"
    )]
    NoPrefix,
    #[error("the picker key and the detach prefix are both `{spec}`")]
    Conflict { spec: String },
}

pub fn parse_key(role: &'static str, spec: &str) -> Result<Key, BindingError> {
    let bad = || BindingError::Unparseable {
        role,
        spec: spec.to_owned(),
    };
    let s = spec.trim().to_ascii_lowercase();
    let named = match s.as_str() {
        "esc" | "escape" => Some(27),
        "enter" | "return" => Some(13),
        "tab" => Some(9),
        "space" => Some(32),
        "backspace" => Some(127),
        _ => None,
    };
    if let Some(b) = named {
        return Ok(Key(b));
    }
    let ctrl_target = s
        .strip_prefix("ctrl+")
        .or_else(|| s.strip_prefix("c-"))
        .or_else(|| s.strip_prefix('^'));
    if let Some(t) = ctrl_target {
        if t == "space" {
            return Ok(Key(0));
        }
        let mut chars = t.chars();
        let c = chars.next().ok_or_else(bad)?;
        if chars.next().is_some() {
            return Err(bad());
        }
        return match c {
            'a'..='z' => Ok(Key(c as u8 - b'a' + 1)),
            '@' => Ok(Key(0)),
            '[' => Ok(Key(27)),
            '\\' => Ok(Key(28)),
            ']' => Ok(Key(29)),
            '^' => Ok(Key(30)),
            '_' => Ok(Key(31)),
            _ => Err(bad()),
        };
    }
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_graphic() => Ok(Key(c as u8)),
        _ => Err(bad()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bindings {
    pub picker: Option<Key>,
    pub prefix: Key,
    pub detach: Key,
    pub refuse: Vec<Key>,
}

impl Bindings {
    pub fn from_config(cfg: &TearConfig) -> Result<Self, BindingError> {
        let a = &cfg.attach;
        let refuse = a
            .refuse
            .iter()
            .map(|s| parse_key("refuse", s))
            .collect::<Result<Vec<_>, _>>()?;
        let prefix_spec = a
            .detach_prefix
            .clone()
            .unwrap_or_else(|| cfg.prefix.clone());
        if prefix_spec.trim().is_empty() {
            return Err(BindingError::NoPrefix);
        }
        let prefix = parse_key("detach prefix", &prefix_spec)?;
        if refuse.contains(&prefix) {
            return Err(BindingError::Refused {
                role: "detach prefix",
                spec: prefix_spec,
            });
        }
        let detach = parse_key("detach", &a.detach_key)?;
        let picker = if a.picker_enabled {
            let k = parse_key("picker", &a.picker_key)?;
            if refuse.contains(&k) {
                return Err(BindingError::Refused {
                    role: "picker",
                    spec: a.picker_key.clone(),
                });
            }
            if k == prefix {
                return Err(BindingError::Conflict {
                    spec: a.picker_key.clone(),
                });
            }
            Some(k)
        } else {
            None
        };
        Ok(Self {
            picker,
            prefix,
            detach,
            refuse,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TearConfig {
        TearConfig::default()
    }

    #[test]
    fn the_defaults_are_ctrl_s_and_ctrl_b_d() {
        let b = Bindings::from_config(&cfg()).unwrap();
        assert_eq!(b.picker, Some(Key(0x13)));
        assert_eq!(b.prefix, Key(0x02));
        assert_eq!(b.detach, Key(b'd'));
    }

    #[test]
    fn key_forms_parse() {
        assert_eq!(parse_key("t", "ctrl+a").unwrap(), Key(1));
        assert_eq!(parse_key("t", "C-b").unwrap(), Key(2));
        assert_eq!(parse_key("t", "^z").unwrap(), Key(26));
        assert_eq!(parse_key("t", "ctrl+space").unwrap(), Key(0));
        assert_eq!(parse_key("t", "ctrl+]").unwrap(), Key(29));
        assert_eq!(parse_key("t", "d").unwrap(), Key(b'd'));
        assert_eq!(parse_key("t", "esc").unwrap(), Key(27));
    }

    #[test]
    fn garbage_is_a_typed_error() {
        assert!(matches!(
            parse_key("picker", "ctrl+ab"),
            Err(BindingError::Unparseable { .. })
        ));
        assert!(matches!(
            parse_key("picker", ""),
            Err(BindingError::Unparseable { .. })
        ));
    }

    #[test]
    fn a_picker_key_on_the_refuse_list_is_refused() {
        let mut c = cfg();
        c.attach.picker_key = "ctrl+c".into();
        assert!(matches!(
            Bindings::from_config(&c),
            Err(BindingError::Refused { role: "picker", .. })
        ));
    }

    #[test]
    fn a_prefix_on_the_refuse_list_is_refused() {
        let mut c = cfg();
        c.attach.detach_prefix = Some("ctrl+d".into());
        assert!(matches!(
            Bindings::from_config(&c),
            Err(BindingError::Refused {
                role: "detach prefix",
                ..
            })
        ));
    }

    #[test]
    fn removing_a_key_from_the_refuse_list_allows_it() {
        let mut c = cfg();
        c.attach.picker_key = "ctrl+c".into();
        c.attach.refuse.retain(|k| k != "ctrl+c");
        assert_eq!(Bindings::from_config(&c).unwrap().picker, Some(Key(3)));
    }

    #[test]
    fn an_empty_prefix_leaves_no_way_to_detach() {
        let mut c = cfg();
        c.prefix = String::new();
        assert_eq!(Bindings::from_config(&c), Err(BindingError::NoPrefix));
    }

    #[test]
    fn picker_and_prefix_on_one_key_conflict() {
        let mut c = cfg();
        c.attach.picker_key = "ctrl+b".into();
        assert!(matches!(
            Bindings::from_config(&c),
            Err(BindingError::Conflict { .. })
        ));
    }

    #[test]
    fn a_disabled_picker_binds_nothing_and_skips_its_checks() {
        let mut c = cfg();
        c.attach.picker_enabled = false;
        c.attach.picker_key = "ctrl+c".into();
        assert_eq!(Bindings::from_config(&c).unwrap().picker, None);
    }

    #[test]
    fn the_detach_prefix_falls_back_to_the_tear_prefix() {
        let mut c = cfg();
        c.prefix = "ctrl+a".into();
        assert_eq!(Bindings::from_config(&c).unwrap().prefix, Key(1));
        c.attach.detach_prefix = Some("ctrl+x".into());
        assert_eq!(Bindings::from_config(&c).unwrap().prefix, Key(24));
    }
}
