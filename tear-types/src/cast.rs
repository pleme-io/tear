//! Typed parser for asciinema v2 .cast rows.
//!
//! The asciinema v2 spec is JSON-lines: a single JSON-object header
//! followed by N JSON-array event rows of shape `[t_seconds, kind,
//! payload]`. `kind` is `"o"` (output written to stdout) or `"i"`
//! (input typed by the user). This module lifts row parsing into a
//! typed [`CastRow`] so every consumer of asciinema captures (`tear
//! replay`, future TUI scrubbers, web playback bridges) shares one
//! representation.
//!
//! Errors are intentionally non-fatal — malformed rows are surfaced
//! as `Err(CastParseError::*)` so callers can choose to skip vs
//! abort. `tear replay` skips silently to match `asciinema play`'s
//! resilience.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// What the row represents in the underlying terminal stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CastRowKind {
    /// `"o"` — bytes the terminal emitted to stdout. Replay players
    /// re-emit these to recreate the visual.
    Output,
    /// `"i"` — bytes the user typed. Replay players IGNORE these
    /// because the recorded output already reflects the side
    /// effects.
    Input,
}

impl CastRowKind {
    /// The single-character on-the-wire tag.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CastRowKind::Output => "o",
            CastRowKind::Input => "i",
        }
    }
}

/// One row of an asciinema v2 .cast.
#[derive(Clone, Debug, PartialEq)]
pub struct CastRow {
    /// Wall-clock-seconds since the recording began.
    pub t: f64,
    pub kind: CastRowKind,
    /// Raw bytes written/typed at `t`. Stored as `String` because
    /// the asciinema v2 wire is JSON strings; callers that need to
    /// re-emit invoke `.as_bytes()`.
    pub payload: String,
}

#[derive(Debug, Error)]
pub enum CastParseError {
    #[error("row is not valid json: {0}")]
    InvalidJson(String),
    #[error("row is not a JSON array")]
    NotAnArray,
    #[error("row has {0} elements, expected at least 3")]
    TooFewElements(usize),
    #[error("row[0] (time) is not a number")]
    BadTime,
    #[error("row[1] (kind) is not a string")]
    BadKindShape,
    #[error("row[1] kind `{0}` is not 'o' or 'i'")]
    UnknownKind(String),
    #[error("row[2] (payload) is not a string")]
    BadPayload,
    #[error("row is the asciinema header (JSON object, not array)")]
    HeaderRow,
}

impl CastRow {
    /// Parse one JSON-encoded line. Returns `Err(HeaderRow)` for
    /// lines that look like the asciinema header (JSON object); the
    /// caller decides whether to skip silently or surface.
    pub fn parse(line: &str) -> Result<Self, CastParseError> {
        let trimmed = line.trim_start();
        if trimmed.starts_with('{') {
            return Err(CastParseError::HeaderRow);
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| CastParseError::InvalidJson(e.to_string()))?;
        let arr = v.as_array().ok_or(CastParseError::NotAnArray)?;
        if arr.len() < 3 {
            return Err(CastParseError::TooFewElements(arr.len()));
        }
        let t = arr[0].as_f64().ok_or(CastParseError::BadTime)?;
        let kind_str = arr[1].as_str().ok_or(CastParseError::BadKindShape)?;
        let kind = match kind_str {
            "o" => CastRowKind::Output,
            "i" => CastRowKind::Input,
            other => return Err(CastParseError::UnknownKind(other.into())),
        };
        let payload = arr[2]
            .as_str()
            .ok_or(CastParseError::BadPayload)?
            .to_string();
        Ok(CastRow { t, kind, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_output_row() {
        let r = CastRow::parse(r#"[0.5,"o","hi"]"#).unwrap();
        assert_eq!(r.t, 0.5);
        assert_eq!(r.kind, CastRowKind::Output);
        assert_eq!(r.payload, "hi");
    }

    #[test]
    fn parses_input_row() {
        let r = CastRow::parse(r#"[1.0,"i","x"]"#).unwrap();
        assert_eq!(r.kind, CastRowKind::Input);
    }

    #[test]
    fn header_row_returns_specific_error() {
        let err = CastRow::parse(r#"{"version":2}"#).unwrap_err();
        assert!(matches!(err, CastParseError::HeaderRow));
    }

    #[test]
    fn malformed_json_returns_invalid_json() {
        let err = CastRow::parse("not json").unwrap_err();
        assert!(matches!(err, CastParseError::InvalidJson(_)));
    }

    #[test]
    fn too_few_elements_returns_specific_error() {
        let err = CastRow::parse(r#"[0.0, "o"]"#).unwrap_err();
        assert!(matches!(err, CastParseError::TooFewElements(2)));
    }

    #[test]
    fn unknown_kind_surfaces_value() {
        let err = CastRow::parse(r#"[0.0,"r","payload"]"#).unwrap_err();
        match err {
            CastParseError::UnknownKind(s) => assert_eq!(s, "r"),
            other => panic!("expected UnknownKind, got {other:?}"),
        }
    }

    #[test]
    fn bad_time_when_string_in_time_slot() {
        let err = CastRow::parse(r#"["zero","o","x"]"#).unwrap_err();
        assert!(matches!(err, CastParseError::BadTime));
    }

    #[test]
    fn bad_payload_when_array_in_payload_slot() {
        let err = CastRow::parse(r#"[0.0,"o",[1,2,3]]"#).unwrap_err();
        assert!(matches!(err, CastParseError::BadPayload));
    }

    #[test]
    fn kind_label_round_trip() {
        assert_eq!(CastRowKind::Output.as_str(), "o");
        assert_eq!(CastRowKind::Input.as_str(), "i");
    }

    #[test]
    fn not_an_array_when_top_level_is_string() {
        let err = CastRow::parse(r#""hi""#).unwrap_err();
        assert!(matches!(err, CastParseError::NotAnArray));
    }
}
