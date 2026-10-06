use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ending {
    Exited { code: Option<i32> },
    EndedBy { who: Human, at_unix: u64 },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Human {
    Control,
}

impl Ending {
    #[must_use]
    pub fn by_control() -> Self {
        Self::EndedBy {
            who: Human::Control,
            at_unix: now_unix(),
        }
    }

    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Exited { code } => *code,
            Self::EndedBy { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Held,
    Ended(Ending),
    Orphaned,
}

#[must_use]
pub fn classify(tombstone: Option<Ending>, holder_answers: bool) -> Verdict {
    match (tombstone, holder_answers) {
        (Some(ending), _) => Verdict::Ended(ending),
        (None, true) => Verdict::Held,
        (None, false) => Verdict::Orphaned,
    }
}

#[must_use]
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_table_is_total_and_never_drops_an_unended_session() {
        let ended = Ending::Exited { code: Some(0) };
        let rows = [
            (Some(ended.clone()), true, Verdict::Ended(ended.clone())),
            (Some(ended.clone()), false, Verdict::Ended(ended.clone())),
            (None, true, Verdict::Held),
            (None, false, Verdict::Orphaned),
        ];
        for (tomb, answers, want) in rows {
            assert_eq!(classify(tomb.clone(), answers), want, "{tomb:?} {answers}");
        }
    }

    #[test]
    fn ending_round_trips_through_json() {
        for e in [
            Ending::Exited { code: Some(3) },
            Ending::Exited { code: None },
            Ending::by_control(),
        ] {
            let s = serde_json::to_string(&e).unwrap();
            let back: Ending = serde_json::from_str(&s).unwrap();
            assert_eq!(back, e);
        }
    }
}
