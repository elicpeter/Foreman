//! `plan.md` domain types and (de)serialization.
//!
//! Phase 2 introduced the type vocabulary; phase 3 adds parsing, serialization,
//! and snapshot/verify utilities on top.

use std::cmp::Ordering;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod parse;
mod snapshot;

pub use parse::{parse, serialize, PlanParseError};
pub use snapshot::{snapshot, verify_unchanged, Snapshot, SnapshotError};

/// A phase identifier such as `"02"` or `"10b"`.
///
/// The raw string is preserved (leading zeros, casing, suffix). Equality is by
/// raw form so `"01"` and `"1"` are distinct identifiers, but ordering is
/// semantic: leading digits compare numerically (`"02" < "10"`) and any
/// alphanumeric suffix compares lexicographically *after* (`"10" < "10b"`).
/// The raw string is used as a final tiebreaker so the total ordering stays
/// consistent with equality.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PhaseId(String);

impl PhaseId {
    /// Parse a string into a `PhaseId`. Must begin with at least one ASCII
    /// digit; any trailing suffix may contain `[A-Za-z0-9_-]`.
    pub fn parse(raw: impl Into<String>) -> Result<Self, PhaseIdParseError> {
        let raw = raw.into();
        Self::components(&raw)?;
        Ok(PhaseId(raw))
    }

    /// Borrow the raw string form.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn components(raw: &str) -> Result<(u64, &str), PhaseIdParseError> {
        if raw.is_empty() {
            return Err(PhaseIdParseError::Empty);
        }
        let split_at = raw
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(raw.len());
        if split_at == 0 {
            return Err(PhaseIdParseError::MissingNumericPrefix(raw.to_string()));
        }
        let (digits, suffix) = raw.split_at(split_at);
        let numeric = digits
            .parse::<u64>()
            .map_err(|_| PhaseIdParseError::NumericOverflow(raw.to_string()))?;
        if !suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(PhaseIdParseError::InvalidCharacters(raw.to_string()));
        }
        Ok((numeric, suffix))
    }
}

impl FromStr for PhaseId {
    type Err = PhaseIdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s.to_string())
    }
}

impl std::fmt::Display for PhaseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialOrd for PhaseId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PhaseId {
    fn cmp(&self, other: &Self) -> Ordering {
        let (an, asfx) = Self::components(&self.0).expect("validated at construction");
        let (bn, bsfx) = Self::components(&other.0).expect("validated at construction");
        an.cmp(&bn)
            .then_with(|| asfx.cmp(bsfx))
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl Serialize for PhaseId {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PhaseId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        PhaseId::parse(raw).map_err(serde::de::Error::custom)
    }
}

/// Errors produced by [`PhaseId::parse`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PhaseIdParseError {
    /// Input string was empty.
    #[error("phase id is empty")]
    Empty,
    /// Input did not start with at least one ASCII digit.
    #[error("phase id {0:?} must begin with at least one digit")]
    MissingNumericPrefix(String),
    /// Suffix contained characters outside `[A-Za-z0-9_-]`.
    #[error(
        "phase id {0:?} contains invalid characters; only [A-Za-z0-9_-] allowed in the suffix"
    )]
    InvalidCharacters(String),
    /// Leading digit run did not fit in a `u64`.
    #[error("phase id {0:?} numeric prefix overflows u64")]
    NumericOverflow(String),
}

/// A single `# Phase NN: Title` block from `plan.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    /// Identifier from the heading (the `NN` portion).
    pub id: PhaseId,
    /// Heading title (everything after `# Phase NN:`).
    pub title: String,
    /// Raw markdown body following the heading line, preserved verbatim.
    pub body: String,
}

/// Parsed `plan.md`: the current phase pointer, raw frontmatter and preamble,
/// plus all phase blocks.
///
/// Frontmatter and preamble are kept as raw text so [`serialize`] can reproduce
/// the input byte-for-byte. `phases` is held in `PhaseId` order; use
/// [`Plan::new`] to enforce the sort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The phase the runner is currently working on. Mirrors the
    /// `current_phase` key inside [`Plan::frontmatter`].
    pub current_phase: PhaseId,
    /// Raw YAML frontmatter text — the bytes between the opening and closing
    /// `---` fences, with no surrounding markers and no trailing newline.
    #[serde(default)]
    pub frontmatter: String,
    /// Raw markdown text between the closing `---` of the frontmatter and the
    /// first `# Phase NN:` heading. Preserved verbatim, including the leading
    /// newline immediately after the closing fence.
    #[serde(default)]
    pub preamble: String,
    /// All phases, sorted by `PhaseId`.
    pub phases: Vec<Phase>,
}

impl Plan {
    /// Build a new `Plan`, sorting `phases` by `PhaseId` and seeding a minimal
    /// frontmatter that names the current phase. The preamble is empty.
    pub fn new(current_phase: PhaseId, mut phases: Vec<Phase>) -> Self {
        phases.sort_by(|a, b| a.id.cmp(&b.id));
        let frontmatter = format!("current_phase: \"{}\"", current_phase);
        Plan {
            current_phase,
            frontmatter,
            preamble: String::new(),
            phases,
        }
    }

    /// Find a phase by id.
    pub fn phase(&self, id: &PhaseId) -> Option<&Phase> {
        self.phases.iter().find(|p| &p.id == id)
    }

    /// Update [`Plan::current_phase`] and rewrite the matching key in the raw
    /// frontmatter. The only mutator the runner uses on `plan.md`.
    ///
    /// The line is rewritten in canonical form (`current_phase: "<id>"`); any
    /// custom quoting style on the original line is replaced. If the
    /// frontmatter contained no `current_phase:` line (e.g., for a `Plan`
    /// constructed by hand without it), the key is appended on a new line.
    pub fn set_current_phase(&mut self, id: PhaseId) {
        let mut out = String::with_capacity(self.frontmatter.len() + 16);
        let mut replaced = false;
        for segment in self.frontmatter.split_inclusive('\n') {
            let (content, eol) = match segment.strip_suffix('\n') {
                Some(c) => (c, "\n"),
                None => (segment, ""),
            };
            if !replaced && is_current_phase_key(content) {
                out.push_str(&format!("current_phase: \"{}\"{}", id, eol));
                replaced = true;
            } else {
                out.push_str(segment);
            }
        }
        if !replaced {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("current_phase: \"{}\"", id));
        }
        self.frontmatter = out;
        self.current_phase = id;
    }
}

fn is_current_phase_key(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("current_phase")
        .is_some_and(|rest| rest.trim_start().starts_with(':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> PhaseId {
        PhaseId::parse(s).expect("test id must parse")
    }

    #[test]
    fn parses_pure_numeric() {
        assert_eq!(id("02").as_str(), "02");
        assert_eq!(id("10").as_str(), "10");
        assert_eq!(id("0").as_str(), "0");
    }

    #[test]
    fn parses_suffixed() {
        assert_eq!(id("10b").as_str(), "10b");
        assert_eq!(id("12abc").as_str(), "12abc");
        assert_eq!(id("3-rerun").as_str(), "3-rerun");
        assert_eq!(id("4_v2").as_str(), "4_v2");
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(PhaseId::parse(""), Err(PhaseIdParseError::Empty));
    }

    #[test]
    fn rejects_missing_numeric_prefix() {
        assert!(matches!(
            PhaseId::parse("abc"),
            Err(PhaseIdParseError::MissingNumericPrefix(_))
        ));
        assert!(matches!(
            PhaseId::parse("a01"),
            Err(PhaseIdParseError::MissingNumericPrefix(_))
        ));
    }

    #[test]
    fn rejects_invalid_chars_in_suffix() {
        assert!(matches!(
            PhaseId::parse("01.2"),
            Err(PhaseIdParseError::InvalidCharacters(_))
        ));
        assert!(matches!(
            PhaseId::parse("1 b"),
            Err(PhaseIdParseError::InvalidCharacters(_))
        ));
    }

    #[test]
    fn rejects_numeric_overflow() {
        let too_big = "9".repeat(40);
        assert!(matches!(
            PhaseId::parse(too_big),
            Err(PhaseIdParseError::NumericOverflow(_))
        ));
    }

    #[test]
    fn ordering_is_numeric_then_suffix_then_raw() {
        assert!(id("02") < id("10"));
        assert!(id("10") < id("10b"));
        assert!(id("10b") < id("10c"));
        assert_eq!(id("10").cmp(&id("10")), Ordering::Equal);
        // "1" and "10" — purely numeric, "1" sorts first.
        assert!(id("1") < id("10"));
        // "01" and "1" — same numeric, no suffix; tie-break by raw lex order.
        assert!(id("01") < id("1"));
        // Eq agrees with Ord:
        assert_ne!(id("01"), id("1"));
    }

    #[test]
    fn serde_round_trip() {
        let pid = id("10b");
        let json = serde_json::to_string(&pid).unwrap();
        assert_eq!(json, "\"10b\"");
        let back: PhaseId = serde_json::from_str(&json).unwrap();
        assert_eq!(pid, back);
    }

    #[test]
    fn serde_rejects_invalid() {
        let err = serde_json::from_str::<PhaseId>("\"abc\"").unwrap_err();
        assert!(err.to_string().contains("must begin with"));
    }

    #[test]
    fn plan_new_sorts_phases() {
        let p10 = Phase {
            id: id("10"),
            title: "ten".into(),
            body: String::new(),
        };
        let p2 = Phase {
            id: id("02"),
            title: "two".into(),
            body: String::new(),
        };
        let p10b = Phase {
            id: id("10b"),
            title: "ten-b".into(),
            body: String::new(),
        };
        let plan = Plan::new(id("02"), vec![p10.clone(), p2.clone(), p10b.clone()]);
        assert_eq!(plan.phases, vec![p2, p10, p10b]);
    }

    #[test]
    fn plan_phase_lookup() {
        let plan = Plan::new(
            id("01"),
            vec![Phase {
                id: id("01"),
                title: "first".into(),
                body: "body".into(),
            }],
        );
        assert_eq!(plan.phase(&id("01")).map(|p| p.title.as_str()), Some("first"));
        assert!(plan.phase(&id("99")).is_none());
    }

    #[test]
    fn plan_serde_round_trip() {
        let plan = Plan::new(
            id("02"),
            vec![
                Phase {
                    id: id("01"),
                    title: "foundation".into(),
                    body: "scope...\n".into(),
                },
                Phase {
                    id: id("02"),
                    title: "domain types".into(),
                    body: String::new(),
                },
            ],
        );
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }
}
