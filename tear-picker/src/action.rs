#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PickerKeyClass {
    Escape,
    Enter,
    Up,
    Down,
    CtrlP,
    CtrlN,
    Backspace,
    CtrlE,
    CtrlD,
    CtrlR,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerKey {
    pub class: PickerKeyClass,
    pub plain: bool,
    pub text: Option<String>,
}

impl PickerKey {
    #[must_use]
    pub fn class(class: PickerKeyClass) -> Self {
        Self {
            class,
            plain: true,
            text: None,
        }
    }

    #[must_use]
    pub fn text(text: &str) -> Self {
        Self {
            class: PickerKeyClass::Other,
            plain: true,
            text: Some(text.to_owned()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerAction {
    Close,
    Accept,
    MoveUp,
    MoveDown,
    Backspace,
    Push(String),
    RenameBegin,
    Delete,
    Reattach,
    Ignore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameAction {
    Cancel,
    Commit,
    Backspace,
    Push(String),
    Ignore,
}

#[must_use]
pub fn picker_action(key: &PickerKey) -> PickerAction {
    match key.class {
        PickerKeyClass::Escape => PickerAction::Close,
        PickerKeyClass::Enter => PickerAction::Accept,
        PickerKeyClass::Up | PickerKeyClass::CtrlP => PickerAction::MoveUp,
        PickerKeyClass::Down | PickerKeyClass::CtrlN => PickerAction::MoveDown,
        PickerKeyClass::Backspace if key.plain => PickerAction::Backspace,
        PickerKeyClass::Backspace => PickerAction::Ignore,
        PickerKeyClass::CtrlE => PickerAction::RenameBegin,
        PickerKeyClass::CtrlD => PickerAction::Delete,
        PickerKeyClass::CtrlR => PickerAction::Reattach,
        PickerKeyClass::Other => match (&key.text, key.plain) {
            (Some(t), true) => PickerAction::Push(t.clone()),
            _ => PickerAction::Ignore,
        },
    }
}

#[must_use]
pub fn rename_action(key: &PickerKey) -> RenameAction {
    match key.class {
        PickerKeyClass::Escape => RenameAction::Cancel,
        PickerKeyClass::Enter => RenameAction::Commit,
        PickerKeyClass::Backspace => RenameAction::Backspace,
        PickerKeyClass::Other => match (&key.text, key.plain) {
            (Some(t), true) => RenameAction::Push(t.clone()),
            _ => RenameAction::Ignore,
        },
        _ => RenameAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_keys_map_to_moves() {
        for c in [PickerKeyClass::Up, PickerKeyClass::CtrlP] {
            assert_eq!(picker_action(&PickerKey::class(c)), PickerAction::MoveUp);
        }
        for c in [PickerKeyClass::Down, PickerKeyClass::CtrlN] {
            assert_eq!(picker_action(&PickerKey::class(c)), PickerAction::MoveDown);
        }
    }

    #[test]
    fn escape_closes_and_enter_accepts() {
        assert_eq!(
            picker_action(&PickerKey::class(PickerKeyClass::Escape)),
            PickerAction::Close
        );
        assert_eq!(
            picker_action(&PickerKey::class(PickerKeyClass::Enter)),
            PickerAction::Accept
        );
    }

    #[test]
    fn text_pushes_only_when_plain() {
        assert_eq!(
            picker_action(&PickerKey::text("a")),
            PickerAction::Push("a".into())
        );
        let mut modified = PickerKey::text("a");
        modified.plain = false;
        assert_eq!(picker_action(&modified), PickerAction::Ignore);
    }

    #[test]
    fn a_modified_backspace_is_ignored() {
        let mut k = PickerKey::class(PickerKeyClass::Backspace);
        assert_eq!(picker_action(&k), PickerAction::Backspace);
        k.plain = false;
        assert_eq!(picker_action(&k), PickerAction::Ignore);
    }

    #[test]
    fn curation_keys_map_to_their_verbs() {
        assert_eq!(
            picker_action(&PickerKey::class(PickerKeyClass::CtrlE)),
            PickerAction::RenameBegin
        );
        assert_eq!(
            picker_action(&PickerKey::class(PickerKeyClass::CtrlD)),
            PickerAction::Delete
        );
        assert_eq!(
            picker_action(&PickerKey::class(PickerKeyClass::CtrlR)),
            PickerAction::Reattach
        );
    }

    #[test]
    fn rename_ignores_navigation() {
        for c in [
            PickerKeyClass::Up,
            PickerKeyClass::Down,
            PickerKeyClass::CtrlN,
            PickerKeyClass::CtrlP,
            PickerKeyClass::CtrlE,
            PickerKeyClass::CtrlD,
            PickerKeyClass::CtrlR,
        ] {
            assert_eq!(rename_action(&PickerKey::class(c)), RenameAction::Ignore);
        }
        assert_eq!(
            rename_action(&PickerKey::class(PickerKeyClass::Enter)),
            RenameAction::Commit
        );
        assert_eq!(
            rename_action(&PickerKey::class(PickerKeyClass::Escape)),
            RenameAction::Cancel
        );
    }
}
