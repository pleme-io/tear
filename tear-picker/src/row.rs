use std::convert::Infallible;

use tear_types::{DefinitionId, SessionId};

use crate::state::FuzzyPicker;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateSpec {
    Preset { pool_index: usize },
    Named { name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind<S = Infallible> {
    Switch(SessionId),
    Instantiate(DefinitionId),
    Suggestion(S),
    Create(CreateSpec),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPickerRow<S = Infallible, U = Infallible> {
    pub label: String,
    pub kind: RowKind<S>,
    pub urgency: Option<U>,
}

pub type SessionPickerState<S = Infallible, U = Infallible> = FuzzyPicker<SessionPickerRow<S, U>>;
