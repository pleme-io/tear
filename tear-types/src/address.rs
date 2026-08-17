//! Addresses — the *mutable* half of a session's two keys.
//!
//! A session carries two independent keys and they answer two
//! different questions:
//!
//! | key | question | mutable? | derived from |
//! |---|---|---|---|
//! | [`crate::Guid`] | *which session is this, forever?* | no | its [`crate::Genesis`] |
//! | [`Address`] | *what do I call it today?* | yes | operator intent |
//!
//! An `Address` is a NATS-style, dot-separated alias —
//! `work.acme.helm-charts.build` — used for lookup, aggregation,
//! and assignment. Because every durable record keys on the `Guid` and
//! never on the `Address`, renaming or re-parenting a session cannot
//! orphan its data: the alias moves, the identity does not.
//!
//! ## Parse, don't validate
//!
//! [`Segment`] is the one label type, and the ONLY way to obtain one
//! is [`Segment::parse`] (or its [`FromStr`] / serde equivalents,
//! which call it). There is no public field, no `Segment::new`, no
//! `From<String>`. A `Segment` in hand is therefore a *proof* that the
//! label already passed every rule below — no downstream re-checking,
//! no "did someone forget to validate this" question.
//!
//! ## The charset rule
//!
//! A segment is **ASCII alphanumeric plus `-` and `_`**, 1 to
//! [`SEGMENT_MAX_LEN`] bytes, and is **case-sensitive** (no
//! normalisation — parsing never silently rewrites its input).
//!
//! Deliberately conservative. That set is exactly what survives a
//! filesystem path, a NATS subject token, a DNS-ish label, a shell
//! word, and a URL path element without quoting, so an address can be
//! pasted into any of those without an escaping layer. Everything else
//! is rejected rather than escaped, including:
//!
//! - the empty string (an address never has a hole in it)
//! - `.` — the separator itself, which would silently re-shape the tree
//! - any whitespace — invisible differences must never be two names
//! - `*` and `>` — those are [`Pattern`] syntax, never labels
//! - every non-ASCII character (no homoglyph confusables in a key)
//!
//! Widening the set later is additive and safe; narrowing it would
//! invalidate stored addresses. Start narrow.
//!
//! ## Naming note
//!
//! [`Segment`] is reachable as `tear_types::address::Segment` and is
//! deliberately NOT re-exported at the crate root: the root already
//! binds `Segment` to [`crate::statusbar::Segment`], the status-bar
//! widget. Two unrelated meanings, one word; the module path keeps
//! them apart instead of renaming a shipped type.

use core::fmt;
use core::num::NonZeroUsize;
use core::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The character between segments.
pub const SEPARATOR: char = '.';

/// The wildcard token matching exactly one segment.
pub const WILDCARD_ONE: &str = "*";

/// The wildcard token matching one-or-more remaining segments. Legal
/// only as the final token of a [`Pattern`].
pub const WILDCARD_TAIL: &str = ">";

/// Longest segment accepted, in bytes. Segments are ASCII so bytes ==
/// characters.
pub const SEGMENT_MAX_LEN: usize = 64;

/// Why a candidate label is not a [`Segment`].
///
/// Every arm names one rule from the module's charset section, so a
/// caller can report *which* rule the operator tripped rather than a
/// generic "bad address".
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum SegmentError {
    /// The label was the empty string.
    #[error("segment is empty")]
    Empty,
    /// The label contained `.`, which would silently re-shape the tree.
    #[error("segment `{0}` contains the `{SEPARATOR}` separator")]
    ContainsSeparator(String),
    /// The label contained whitespace — invisible differences must
    /// never produce two distinct names.
    #[error("segment `{0}` contains whitespace")]
    ContainsWhitespace(String),
    /// The label was exactly `*` or `>`. Those are [`Pattern`] syntax
    /// and can never be a literal label.
    #[error("`{0}` is a pattern wildcard, not a label")]
    WildcardToken(String),
    /// The label held a character outside the accepted charset.
    #[error("segment `{segment}`: illegal character `{ch}` at byte {index}; allowed: a-z A-Z 0-9 `-` `_`")]
    IllegalChar {
        /// The rejected label.
        segment: String,
        /// The first offending character.
        ch: char,
        /// Its byte offset within the label.
        index: usize,
    },
    /// The label was longer than [`SEGMENT_MAX_LEN`].
    #[error("segment `{segment}` is {len} bytes, over the {} byte maximum", SEGMENT_MAX_LEN)]
    TooLong {
        /// The rejected label.
        segment: String,
        /// Its length in bytes.
        len: usize,
    },
}

/// One address label — a validated, non-empty, separator-free token.
///
/// Constructed only through [`Segment::parse`]; see the module docs
/// for the charset rule and why it is narrow.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Segment(String);

impl Segment {
    /// The single fallible constructor. Rules are checked in the order
    /// documented on [`SegmentError`]'s arms, so the error names the
    /// most specific failure (`ContainsSeparator` beats a generic
    /// "illegal character").
    pub fn parse(s: &str) -> Result<Self, SegmentError> {
        if s.is_empty() {
            return Err(SegmentError::Empty);
        }
        if s == WILDCARD_ONE || s == WILDCARD_TAIL {
            return Err(SegmentError::WildcardToken(s.to_string()));
        }
        if s.contains(SEPARATOR) {
            return Err(SegmentError::ContainsSeparator(s.to_string()));
        }
        if s.chars().any(char::is_whitespace) {
            return Err(SegmentError::ContainsWhitespace(s.to_string()));
        }
        if s.len() > SEGMENT_MAX_LEN {
            return Err(SegmentError::TooLong {
                segment: s.to_string(),
                len: s.len(),
            });
        }
        if let Some((index, ch)) = s.char_indices().find(|(_, c)| !Self::is_legal(*c)) {
            return Err(SegmentError::IllegalChar {
                segment: s.to_string(),
                ch,
                index,
            });
        }
        Ok(Self(s.to_string()))
    }

    /// Borrow the validated label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_legal(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '-' || c == '_'
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Segment {
    type Err = SegmentError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Segment {
    type Error = SegmentError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Segment> for String {
    fn from(v: Segment) -> Self {
        v.0
    }
}

/// Why a candidate string is not an [`Address`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum AddressError {
    /// The address had no segments at all.
    #[error("address is empty")]
    Empty,
    /// One segment failed [`Segment::parse`]. The index is positional
    /// from the left, so the caller can point at the offending token.
    #[error("address segment {index}: {source}")]
    Segment {
        /// Zero-based position of the failing segment.
        index: usize,
        /// Why that segment was rejected.
        #[source]
        source: SegmentError,
    },
}

/// An ordered, non-empty sequence of [`Segment`]s — a session's alias.
///
/// Displays and parses dot-joined (`work.acme.build`). Non-empty
/// is an invariant of the type, not a runtime check: every constructor
/// is fallible or takes at least one segment, and [`Address::depth`]
/// returns [`NonZeroUsize`] so the guarantee is visible in the
/// signature.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Address(Vec<Segment>);

impl Address {
    /// Parse a dot-joined address. An empty string is
    /// [`AddressError::Empty`]; any bad segment surfaces with its
    /// position.
    pub fn parse(s: &str) -> Result<Self, AddressError> {
        if s.is_empty() {
            return Err(AddressError::Empty);
        }
        let mut segments = Vec::new();
        for (index, part) in s.split(SEPARATOR).enumerate() {
            let seg = Segment::parse(part)
                .map_err(|source| AddressError::Segment { index, source })?;
            segments.push(seg);
        }
        Ok(Self(segments))
    }

    /// Build from already-validated segments. Fails only on an empty
    /// iterator — there is nothing left to check.
    pub fn from_segments<I>(segments: I) -> Result<Self, AddressError>
    where
        I: IntoIterator<Item = Segment>,
    {
        let segments: Vec<Segment> = segments.into_iter().collect();
        if segments.is_empty() {
            return Err(AddressError::Empty);
        }
        Ok(Self(segments))
    }

    /// A one-segment address. Infallible: one segment is already
    /// non-empty.
    #[must_use]
    pub fn root(segment: Segment) -> Self {
        Self(vec![segment])
    }

    /// The segments, left to right. Never empty.
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.0
    }

    /// The rightmost segment.
    #[must_use]
    pub fn leaf(&self) -> &Segment {
        self.0.last().expect("Address is non-empty by construction")
    }

    /// How many segments. Typed [`NonZeroUsize`] because an address
    /// with zero segments does not exist.
    #[must_use]
    pub fn depth(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.0.len()).expect("Address is non-empty by construction")
    }

    /// This address with its last segment dropped. `None` at depth 1 —
    /// a root has no parent, and the empty address is unrepresentable.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.0.len() == 1 {
            return None;
        }
        Some(Self(self.0[..self.0.len() - 1].to_vec()))
    }

    /// This address extended by one segment.
    #[must_use]
    pub fn child(&self, segment: Segment) -> Self {
        let mut segments = self.0.clone();
        segments.push(segment);
        Self(segments)
    }

    /// Whether `prefix` is a segment-wise prefix of this address.
    /// Segment-wise, never string-wise: `work.helm` is NOT a prefix of
    /// `work.helm-charts`.
    #[must_use]
    pub fn starts_with(&self, prefix: &Self) -> bool {
        self.0.len() >= prefix.0.len() && self.0[..prefix.0.len()] == prefix.0[..]
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for seg in &self.0 {
            if !first {
                f.write_str(".")?;
            }
            f.write_str(seg.as_str())?;
            first = false;
        }
        Ok(())
    }
}

impl FromStr for Address {
    type Err = AddressError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Address {
    type Error = AddressError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Address> for String {
    fn from(v: Address) -> Self {
        v.to_string()
    }
}

/// One token of a [`Pattern`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PatternToken {
    /// A literal label — matches itself and nothing else.
    Literal(Segment),
    /// `*` — matches exactly one segment, whatever it is.
    One,
    /// `>` — matches one-or-more remaining segments. Only ever the
    /// final token; a `>` elsewhere is a parse error, so a `Pattern`
    /// with a mid-sequence `Tail` is unrepresentable.
    Tail,
}

impl fmt::Display for PatternToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatternToken::Literal(s) => f.write_str(s.as_str()),
            PatternToken::One => f.write_str(WILDCARD_ONE),
            PatternToken::Tail => f.write_str(WILDCARD_TAIL),
        }
    }
}

/// Why a candidate string is not a [`Pattern`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PatternError {
    /// The pattern had no tokens at all.
    #[error("pattern is empty")]
    Empty,
    /// A `>` appeared somewhere other than the final position. This is
    /// a *parse* error, which is why [`Pattern::matches`] never has to
    /// consider the case.
    #[error("`{WILDCARD_TAIL}` at token {index} is not final; the tail wildcard is legal only last")]
    TailNotFinal {
        /// Zero-based position of the offending `>`.
        index: usize,
    },
    /// A literal token failed [`Segment::parse`].
    #[error("pattern token {index}: {source}")]
    Token {
        /// Zero-based position of the failing token.
        index: usize,
        /// Why that token was rejected as a literal.
        #[source]
        source: SegmentError,
    },
}

/// A NATS-style matcher over [`Address`].
///
/// - a literal segment matches itself
/// - `*` matches exactly one segment
/// - `>` matches one-or-more remaining segments, and is legal only as
///   the final token
///
/// The "only final" rule is enforced at [`Pattern::parse`], not at
/// match time: an ill-formed pattern never becomes a value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Pattern(Vec<PatternToken>);

impl Pattern {
    /// Parse a dot-joined pattern.
    pub fn parse(s: &str) -> Result<Self, PatternError> {
        if s.is_empty() {
            return Err(PatternError::Empty);
        }
        let parts: Vec<&str> = s.split(SEPARATOR).collect();
        let last = parts.len() - 1;
        let mut tokens = Vec::with_capacity(parts.len());
        for (index, part) in parts.into_iter().enumerate() {
            let token = match part {
                WILDCARD_ONE => PatternToken::One,
                WILDCARD_TAIL => {
                    if index != last {
                        return Err(PatternError::TailNotFinal { index });
                    }
                    PatternToken::Tail
                }
                literal => PatternToken::Literal(
                    Segment::parse(literal)
                        .map_err(|source| PatternError::Token { index, source })?,
                ),
            };
            tokens.push(token);
        }
        Ok(Self(tokens))
    }

    /// The exact-match pattern for one address — every token literal,
    /// no wildcards.
    #[must_use]
    pub fn exact(address: &Address) -> Self {
        Self(
            address
                .segments()
                .iter()
                .cloned()
                .map(PatternToken::Literal)
                .collect(),
        )
    }

    /// The tokens, left to right.
    #[must_use]
    pub fn tokens(&self) -> &[PatternToken] {
        &self.0
    }

    /// Whether this pattern matches `address`.
    ///
    /// Linear, single pass: because `>` can only be final, there is no
    /// backtracking to do.
    #[must_use]
    pub fn matches(&self, address: &Address) -> bool {
        let segments = address.segments();
        for (i, token) in self.0.iter().enumerate() {
            match token {
                // Final by construction; needs at least one segment
                // left to consume, so `>` never matches zero.
                PatternToken::Tail => return segments.len() > i,
                PatternToken::One => {
                    if segments.len() <= i {
                        return false;
                    }
                }
                PatternToken::Literal(want) => match segments.get(i) {
                    Some(have) if have == want => {}
                    _ => return false,
                },
            }
        }
        segments.len() == self.0.len()
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for token in &self.0 {
            if !first {
                f.write_str(".")?;
            }
            write!(f, "{token}")?;
            first = false;
        }
        Ok(())
    }
}

impl FromStr for Pattern {
    type Err = PatternError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Pattern {
    type Error = PatternError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Pattern> for String {
    fn from(v: Pattern) -> Self {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(s: &str) -> Segment {
        Segment::parse(s).expect("test fixture must be a legal segment")
    }

    fn addr(s: &str) -> Address {
        Address::parse(s).expect("test fixture must be a legal address")
    }

    fn pat(s: &str) -> Pattern {
        Pattern::parse(s).expect("test fixture must be a legal pattern")
    }

    // ── Segment: every rejection path ──────────────────────────────

    #[test]
    fn segment_rejects_empty() {
        assert_eq!(Segment::parse("").unwrap_err(), SegmentError::Empty);
    }

    #[test]
    fn segment_rejects_the_separator() {
        let err = Segment::parse("work.build").unwrap_err();
        assert_eq!(err, SegmentError::ContainsSeparator("work.build".into()));
        // A leading/trailing dot is the same rule, not a special case.
        assert!(matches!(
            Segment::parse(".x"),
            Err(SegmentError::ContainsSeparator(_))
        ));
        assert!(matches!(
            Segment::parse("x."),
            Err(SegmentError::ContainsSeparator(_))
        ));
    }

    #[test]
    fn segment_rejects_whitespace() {
        for bad in ["a b", " a", "a ", "a\tb", "a\nb", "\u{a0}a"] {
            assert!(
                matches!(Segment::parse(bad), Err(SegmentError::ContainsWhitespace(_))),
                "expected whitespace rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn segment_rejects_the_wildcard_tokens() {
        assert_eq!(
            Segment::parse("*").unwrap_err(),
            SegmentError::WildcardToken("*".into())
        );
        assert_eq!(
            Segment::parse(">").unwrap_err(),
            SegmentError::WildcardToken(">".into())
        );
    }

    #[test]
    fn segment_rejects_wildcard_characters_inside_a_label() {
        // `a*b` is not the wildcard TOKEN, so it falls through to the
        // charset rule — still rejected, with a precise position.
        match Segment::parse("a*b").unwrap_err() {
            SegmentError::IllegalChar { ch, index, .. } => {
                assert_eq!(ch, '*');
                assert_eq!(index, 1);
            }
            other => panic!("expected IllegalChar, got {other:?}"),
        }
        assert!(matches!(
            Segment::parse("a>b"),
            Err(SegmentError::IllegalChar { ch: '>', .. })
        ));
    }

    #[test]
    fn segment_rejects_characters_outside_the_charset() {
        for (bad, ch, index) in [
            ("work/build", '/', 4),
            ("caf\u{e9}", '\u{e9}', 3),
            ("a:b", ':', 1),
            ("a+b", '+', 1),
            ("a$b", '$', 1),
        ] {
            match Segment::parse(bad).unwrap_err() {
                SegmentError::IllegalChar {
                    ch: got_ch,
                    index: got_index,
                    ..
                } => {
                    assert_eq!(got_ch, ch, "wrong char for {bad:?}");
                    assert_eq!(got_index, index, "wrong index for {bad:?}");
                }
                other => panic!("expected IllegalChar for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn segment_rejects_over_the_length_maximum() {
        let ok = "a".repeat(SEGMENT_MAX_LEN);
        assert!(Segment::parse(&ok).is_ok());
        let too_long = "a".repeat(SEGMENT_MAX_LEN + 1);
        match Segment::parse(&too_long).unwrap_err() {
            SegmentError::TooLong { len, .. } => assert_eq!(len, SEGMENT_MAX_LEN + 1),
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn segment_accepts_the_documented_charset() {
        for good in ["a", "Z", "0", "helm-charts", "build_2", "MixedUp", "x-_-x"] {
            assert_eq!(seg(good).as_str(), good);
        }
    }

    #[test]
    fn segment_is_case_sensitive_and_never_normalises() {
        assert_ne!(seg("Build"), seg("build"));
        assert_eq!(seg("Build").as_str(), "Build");
    }

    #[test]
    fn segment_display_round_trips_through_from_str() {
        let s = seg("helm-charts");
        let back: Segment = s.to_string().parse().unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn segment_serde_is_a_plain_string_and_validates_on_the_way_in() {
        let s = seg("build");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"build\"");
        let back: Segment = serde_json::from_str("\"build\"").unwrap();
        assert_eq!(s, back);
        // The border holds through serde too — a wildcard cannot be
        // smuggled in as a label.
        assert!(serde_json::from_str::<Segment>("\"*\"").is_err());
        assert!(serde_json::from_str::<Segment>("\"a.b\"").is_err());
    }

    // ── Address ────────────────────────────────────────────────────

    #[test]
    fn address_parses_and_displays_dot_joined() {
        let a = addr("work.acme.helm-charts.build");
        assert_eq!(a.to_string(), "work.acme.helm-charts.build");
        assert_eq!(a.depth().get(), 4);
        assert_eq!(a.leaf().as_str(), "build");
    }

    #[test]
    fn address_rejects_the_empty_string() {
        assert_eq!(Address::parse("").unwrap_err(), AddressError::Empty);
    }

    #[test]
    fn address_reports_the_failing_segment_and_its_index() {
        match Address::parse("work..build").unwrap_err() {
            AddressError::Segment { index, source } => {
                assert_eq!(index, 1);
                assert_eq!(source, SegmentError::Empty);
            }
            other => panic!("expected Segment error, got {other:?}"),
        }
        match Address::parse("work.a b.c").unwrap_err() {
            AddressError::Segment { index, source } => {
                assert_eq!(index, 1);
                assert!(matches!(source, SegmentError::ContainsWhitespace(_)));
            }
            other => panic!("expected Segment error, got {other:?}"),
        }
        // A wildcard is not an address token.
        match Address::parse("work.*.build").unwrap_err() {
            AddressError::Segment { index, source } => {
                assert_eq!(index, 1);
                assert!(matches!(source, SegmentError::WildcardToken(_)));
            }
            other => panic!("expected Segment error, got {other:?}"),
        }
    }

    #[test]
    fn address_from_segments_rejects_an_empty_sequence() {
        assert_eq!(
            Address::from_segments(Vec::new()).unwrap_err(),
            AddressError::Empty
        );
        let a = Address::from_segments(vec![seg("work"), seg("build")]).unwrap();
        assert_eq!(a.to_string(), "work.build");
    }

    #[test]
    fn address_parent_child_and_depth_compose() {
        let a = addr("work.acme");
        let child = a.child(seg("build"));
        assert_eq!(child.to_string(), "work.acme.build");
        assert_eq!(child.depth().get(), 3);
        assert_eq!(child.parent().unwrap(), a);
        assert_eq!(a.parent().unwrap(), Address::root(seg("work")));
    }

    #[test]
    fn address_root_has_no_parent() {
        let root = Address::root(seg("work"));
        assert_eq!(root.depth().get(), 1);
        assert!(root.parent().is_none());
    }

    #[test]
    fn address_starts_with_is_segment_wise_not_string_wise() {
        let a = addr("work.helm-charts.build");
        assert!(a.starts_with(&addr("work")));
        assert!(a.starts_with(&addr("work.helm-charts")));
        assert!(a.starts_with(&a));
        // string-prefix but NOT a segment prefix
        assert!(!a.starts_with(&addr("work.helm")));
        assert!(!addr("work").starts_with(&a));
    }

    #[test]
    fn address_serde_is_a_plain_string_and_validates_on_the_way_in() {
        let a = addr("work.build");
        assert_eq!(serde_json::to_string(&a).unwrap(), "\"work.build\"");
        let back: Address = serde_json::from_str("\"work.build\"").unwrap();
        assert_eq!(a, back);
        assert!(serde_json::from_str::<Address>("\"\"").is_err());
        assert!(serde_json::from_str::<Address>("\"work..build\"").is_err());
    }

    // ── Pattern ────────────────────────────────────────────────────

    #[test]
    fn pattern_rejects_empty() {
        assert_eq!(Pattern::parse("").unwrap_err(), PatternError::Empty);
    }

    #[test]
    fn pattern_tail_wildcard_must_be_final() {
        assert_eq!(
            Pattern::parse("work.>.build").unwrap_err(),
            PatternError::TailNotFinal { index: 1 }
        );
        assert_eq!(
            Pattern::parse(">.work").unwrap_err(),
            PatternError::TailNotFinal { index: 0 }
        );
        assert_eq!(
            Pattern::parse("a.>.>").unwrap_err(),
            PatternError::TailNotFinal { index: 1 }
        );
        // Final is fine, including on its own.
        assert!(Pattern::parse("work.>").is_ok());
        assert!(Pattern::parse(">").is_ok());
    }

    #[test]
    fn pattern_reports_the_failing_token_and_its_index() {
        match Pattern::parse("work.a b.>").unwrap_err() {
            PatternError::Token { index, source } => {
                assert_eq!(index, 1);
                assert!(matches!(source, SegmentError::ContainsWhitespace(_)));
            }
            other => panic!("expected Token error, got {other:?}"),
        }
        match Pattern::parse("work..build").unwrap_err() {
            PatternError::Token { index, source } => {
                assert_eq!(index, 1);
                assert_eq!(source, SegmentError::Empty);
            }
            other => panic!("expected Token error, got {other:?}"),
        }
    }

    #[test]
    fn pattern_literal_matches_itself_and_nothing_else() {
        let p = pat("work.build");
        assert!(p.matches(&addr("work.build")));
        assert!(!p.matches(&addr("work.Build")));
        assert!(!p.matches(&addr("work")));
        assert!(!p.matches(&addr("work.build.extra")));
        assert!(!p.matches(&addr("other.build")));
    }

    #[test]
    fn pattern_star_matches_exactly_one_segment() {
        let p = pat("work.*.build");
        assert!(p.matches(&addr("work.acme.build")));
        assert!(p.matches(&addr("work.x.build")));
        // zero segments in the slot
        assert!(!p.matches(&addr("work.build")));
        // two segments in the slot
        assert!(!p.matches(&addr("work.a.b.build")));

        let trailing = pat("work.*");
        assert!(trailing.matches(&addr("work.build")));
        assert!(!trailing.matches(&addr("work")));
        assert!(!trailing.matches(&addr("work.build.deep")));
    }

    #[test]
    fn pattern_tail_matches_one_or_more_but_never_zero() {
        let p = pat("work.>");
        assert!(p.matches(&addr("work.build")));
        assert!(p.matches(&addr("work.acme.helm-charts.build")));
        // one-or-more, so the bare prefix does NOT match
        assert!(!p.matches(&addr("work")));
        assert!(!p.matches(&addr("other.build")));

        let everything = pat(">");
        assert!(everything.matches(&addr("work")));
        assert!(everything.matches(&addr("work.a.b.c")));
    }

    #[test]
    fn pattern_mixes_literals_stars_and_a_tail() {
        let p = pat("work.*.helm-charts.>");
        assert!(p.matches(&addr("work.acme.helm-charts.build")));
        assert!(p.matches(&addr("work.pleme.helm-charts.a.b")));
        assert!(!p.matches(&addr("work.acme.helm-charts")));
        assert!(!p.matches(&addr("work.acme.other.build")));
        assert!(!p.matches(&addr("work.helm-charts.build")));
    }

    #[test]
    fn pattern_display_round_trips_through_from_str() {
        for src in ["work.build", "work.*.build", "work.>", ">", "*"] {
            let p = pat(src);
            assert_eq!(p.to_string(), src);
            let back: Pattern = p.to_string().parse().unwrap();
            assert_eq!(p, back);
        }
    }

    #[test]
    fn pattern_exact_matches_only_its_own_address() {
        let a = addr("work.acme.build");
        let p = Pattern::exact(&a);
        assert_eq!(p.to_string(), a.to_string());
        assert!(p.matches(&a));
        assert!(!p.matches(&addr("work.acme")));
        assert!(!p.matches(&addr("work.acme.build.x")));
        assert_eq!(p.tokens().len(), 3);
    }

    #[test]
    fn pattern_serde_is_a_plain_string_and_validates_on_the_way_in() {
        let p = pat("work.*.>");
        assert_eq!(serde_json::to_string(&p).unwrap(), "\"work.*.>\"");
        let back: Pattern = serde_json::from_str("\"work.*.>\"").unwrap();
        assert_eq!(p, back);
        assert!(serde_json::from_str::<Pattern>("\"work.>.build\"").is_err());
    }

    // ── structural forcing function ────────────────────────────────

    /// Parse-don't-validate is a claim about the *absence* of other
    /// ways in, which cannot be asserted at runtime. Comment-stripped
    /// source scan, same construction as `shutai.rs`'s
    /// `shutai_never_becomes_deserializable`.
    #[test]
    fn the_only_way_in_is_a_fallible_parse() {
        let src = include_str!("address.rs");
        let code: String = src
            .lines()
            .map(str::trim_start)
            .filter(|l| !l.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let code = code.split("mod tests").next().unwrap_or(&code);

        // Private inner fields: no other module can build one of these
        // without going through a constructor in this file.
        for decl in [
            "pub struct Segment(String);",
            "pub struct Address(Vec<Segment>);",
            "pub struct Pattern(Vec<PatternToken>);",
        ] {
            assert!(code.contains(decl), "inner state must stay private: {decl}");
        }
        assert!(
            !code.contains("pub fn new("),
            "an infallible `new` would be a second, unchecked way in"
        );
        assert!(
            !code.contains("impl From<String> for"),
            "an infallible `From<String>` would bypass parsing"
        );

        // Anti-vacuity: the scan must be looking at real code.
        assert!(code.contains("pub fn parse(s: &str) -> Result<Self, SegmentError>"));
        assert!(code.contains("try_from = \"String\""));
    }
}
