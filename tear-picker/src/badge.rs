use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BadgeMode {
    Off,
    #[default]
    Auto,
    Always,
}

impl BadgeMode {
    #[must_use]
    pub fn badges(self, has_live: bool, has_latent: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Always => true,
            Self::Auto => has_live && has_latent,
        }
    }
}

pub const LIVE_GLYPH: &str = "\u{25cf} ";
pub const LATENT_GLYPH: &str = "\u{25cb} ";

#[must_use]
pub fn row_label(
    display_name: &str,
    project_root: &std::path::Path,
    badge: Option<bool>,
) -> String {
    let mut label = String::new();
    match badge {
        Some(true) => label.push_str(LATENT_GLYPH),
        Some(false) => label.push_str(LIVE_GLYPH),
        None => {}
    }
    label.push_str(display_name);
    let basename = project_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !basename.is_empty() && basename != "/" {
        label.push_str("  ");
        label.push_str(&basename);
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn auto_badges_only_a_mixed_list() {
        assert!(!BadgeMode::Auto.badges(true, false));
        assert!(!BadgeMode::Auto.badges(false, true));
        assert!(BadgeMode::Auto.badges(true, true));
    }

    #[test]
    fn off_never_and_always_always() {
        assert!(!BadgeMode::Off.badges(true, true));
        assert!(BadgeMode::Always.badges(true, false));
    }

    #[test]
    fn label_carries_glyph_and_basename() {
        assert_eq!(
            row_label("tide", Path::new("/code/mado"), Some(false)),
            "\u{25cf} tide  mado"
        );
        assert_eq!(
            row_label("tide", Path::new("/code/mado"), Some(true)),
            "\u{25cb} tide  mado"
        );
        assert_eq!(row_label("tide", Path::new("/"), None), "tide");
    }
}
