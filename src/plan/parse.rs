//! Strict parser and serializer for `plan.md`.
//!
//! The parser is intentionally narrow: it accepts a YAML frontmatter delimited
//! by `---` fences, an optional preamble, and one or more `# Phase NN: Title`
//! blocks. Anything outside that shape is rejected with a typed
//! [`PlanParseError`] so the runner can react (e.g., halt the run and restore
//! a snapshot if the agent produced an invalid plan).
//!
//! [`serialize`] is a pure inverse of [`parse`] for canonical inputs:
//! `serialize(parse(s)?) == s` for every fixture in our corpus. Set
//! [`super::Plan::set_current_phase`] before serializing to update the pointer.

use std::collections::HashSet;

use thiserror::Error;
use tracing::warn;

use super::{Phase, PhaseId, PhaseIdParseError, Plan};

/// Errors produced by [`parse`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlanParseError {
    /// The opening `---\n` fence was missing or unterminated.
    #[error("plan.md is missing a YAML frontmatter (expected ---\\n…\\n---\\n)")]
    MissingFrontmatter,
    /// The frontmatter parsed as YAML but was structurally invalid (not a
    /// mapping, missing `current_phase`, wrong type, etc.) or YAML itself was
    /// malformed.
    #[error("plan.md frontmatter is invalid: {0}")]
    BadFrontmatter(String),
    /// `current_phase` was a string but did not parse as a [`PhaseId`].
    #[error("plan.md current_phase is not a valid phase id: {0}")]
    BadCurrentPhase(#[source] PhaseIdParseError),
    /// `current_phase` referenced a phase that does not exist in the file.
    #[error("plan.md current_phase {0:?} does not match any # Phase heading")]
    UnknownCurrentPhase(String),
    /// The file contained no `# Phase NN:` headings.
    #[error("plan.md contains no # Phase NN: headings")]
    NoPhases,
    /// A `# Phase` heading line could not be split into `<id>: <title>`.
    #[error("plan.md heading on line {line} is malformed: {raw:?} (expected `# Phase <id>: <title>`)")]
    BadHeading {
        /// 1-based line number of the offending heading.
        line: usize,
        /// The raw heading line (without trailing newline).
        raw: String,
    },
    /// A `# Phase` heading carried an unparseable id.
    #[error("plan.md heading on line {line} has invalid phase id: {source}")]
    BadHeadingId {
        /// 1-based line number of the offending heading.
        line: usize,
        /// The underlying [`PhaseIdParseError`].
        #[source]
        source: PhaseIdParseError,
    },
    /// Two `# Phase` headings shared the same id.
    #[error("plan.md contains duplicate phase id {0:?}")]
    DuplicatePhaseId(String),
}

const FENCE: &str = "---\n";

/// Parse `plan.md` content into a [`Plan`].
///
/// CRLF line endings are normalized to LF before parsing. Unknown frontmatter
/// keys (anything other than `current_phase`) are accepted with a tracing
/// warning so we don't break on benign user additions.
pub fn parse(input: &str) -> Result<Plan, PlanParseError> {
    let normalized = if input.contains('\r') {
        input.replace("\r\n", "\n")
    } else {
        input.to_string()
    };

    let after_open = normalized
        .strip_prefix(FENCE)
        .ok_or(PlanParseError::MissingFrontmatter)?;
    let close_idx = find_closing_fence(after_open).ok_or(PlanParseError::MissingFrontmatter)?;

    let frontmatter_raw = &after_open[..close_idx];
    let body = &after_open[close_idx + FENCE.len()..];

    let current_phase_str = parse_frontmatter(frontmatter_raw)?;
    let current_phase =
        PhaseId::parse(&current_phase_str).map_err(PlanParseError::BadCurrentPhase)?;

    let (preamble, phases) = split_phases(body)?;

    let mut seen = HashSet::new();
    for phase in &phases {
        if !seen.insert(phase.id.clone()) {
            return Err(PlanParseError::DuplicatePhaseId(phase.id.to_string()));
        }
    }

    if !phases.iter().any(|p| p.id == current_phase) {
        return Err(PlanParseError::UnknownCurrentPhase(
            current_phase.to_string(),
        ));
    }

    Ok(Plan {
        current_phase,
        frontmatter: frontmatter_raw.trim_end_matches('\n').to_string(),
        preamble,
        phases,
    })
}

/// Serialize a [`Plan`] back to `plan.md` text. Always emits LF line endings.
pub fn serialize(plan: &Plan) -> String {
    let mut out = String::with_capacity(plan.frontmatter.len() + plan.preamble.len() + 256);
    out.push_str(FENCE);
    out.push_str(&plan.frontmatter);
    if !plan.frontmatter.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(FENCE);
    out.push_str(&plan.preamble);
    for phase in &plan.phases {
        out.push_str(&format!("# Phase {}: {}\n", phase.id, phase.title));
        out.push_str(&phase.body);
    }
    out
}

fn find_closing_fence(after_open: &str) -> Option<usize> {
    // Closing fence is a `---\n` line that is itself preceded by `\n` (i.e., it
    // begins a fresh line). Empty frontmatter is permitted, so a body that
    // starts with `---\n` immediately also counts.
    if after_open.starts_with(FENCE) {
        return Some(0);
    }
    let needle = "\n---\n";
    after_open.find(needle).map(|idx| idx + 1)
}

fn parse_frontmatter(raw: &str) -> Result<String, PlanParseError> {
    let value: serde_yaml::Value = serde_yaml::from_str(raw)
        .map_err(|e| PlanParseError::BadFrontmatter(format!("invalid YAML: {e}")))?;
    let mapping = value.as_mapping().ok_or_else(|| {
        PlanParseError::BadFrontmatter("frontmatter must be a YAML mapping".to_string())
    })?;

    let mut current_phase: Option<String> = None;
    for (key, val) in mapping {
        let Some(key_str) = key.as_str() else {
            return Err(PlanParseError::BadFrontmatter(
                "frontmatter keys must be strings".to_string(),
            ));
        };
        if key_str == "current_phase" {
            let s = val.as_str().ok_or_else(|| {
                PlanParseError::BadFrontmatter("current_phase must be a string".to_string())
            })?;
            current_phase = Some(s.to_string());
        } else {
            warn!(
                key = key_str,
                "unknown frontmatter key — accepting, but ignoring"
            );
        }
    }

    current_phase.ok_or_else(|| {
        PlanParseError::BadFrontmatter("frontmatter is missing current_phase".to_string())
    })
}

fn split_phases(body: &str) -> Result<(String, Vec<Phase>), PlanParseError> {
    struct Heading {
        line_start: usize,
        body_start: usize,
        id: PhaseId,
        title: String,
    }

    let mut headings: Vec<Heading> = Vec::new();
    let mut byte_pos = 0usize;

    for (idx, line) in body.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let line_no_eol = line.strip_suffix('\n').unwrap_or(line);
        if let Some(rest) = line_no_eol.strip_prefix("# Phase ") {
            let (id_str, title) =
                rest.split_once(": ")
                    .ok_or_else(|| PlanParseError::BadHeading {
                        line: line_no,
                        raw: line_no_eol.to_string(),
                    })?;
            let id = PhaseId::parse(id_str).map_err(|source| PlanParseError::BadHeadingId {
                line: line_no,
                source,
            })?;
            headings.push(Heading {
                line_start: byte_pos,
                body_start: byte_pos + line.len(),
                id,
                title: title.to_string(),
            });
        }
        byte_pos += line.len();
    }

    if headings.is_empty() {
        return Err(PlanParseError::NoPhases);
    }

    let preamble = body[..headings[0].line_start].to_string();
    let mut phases = Vec::with_capacity(headings.len());
    for i in 0..headings.len() {
        let body_end = if i + 1 < headings.len() {
            headings[i + 1].line_start
        } else {
            body.len()
        };
        let h = &headings[i];
        phases.push(Phase {
            id: h.id.clone(),
            title: h.title.clone(),
            body: body[h.body_start..body_end].to_string(),
        });
    }

    Ok((preamble, phases))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_minimal() -> &'static str {
        "---\ncurrent_phase: \"01\"\n---\n\n# Phase 01: First\n\nHello world.\n"
    }

    fn fixture_with_preamble_and_two_phases() -> &'static str {
        "---\n\
         current_phase: \"02\"\n\
         project: foreman\n\
         ---\n\
         \n\
         # Foreman\n\
         \n\
         Intro paragraph.\n\
         \n\
         # Phase 01: Foundation\n\
         \n\
         Body of phase one.\n\
         \n\
         # Phase 02: Domain types\n\
         \n\
         Body of phase two.\n"
    }

    #[test]
    fn parses_minimal_fixture() {
        let plan = parse(fixture_minimal()).unwrap();
        assert_eq!(plan.current_phase.as_str(), "01");
        assert_eq!(plan.phases.len(), 1);
        assert_eq!(plan.phases[0].id.as_str(), "01");
        assert_eq!(plan.phases[0].title, "First");
        assert_eq!(plan.phases[0].body, "\nHello world.\n");
        assert_eq!(plan.preamble, "\n");
        assert_eq!(plan.frontmatter, "current_phase: \"01\"");
    }

    #[test]
    fn round_trip_minimal() {
        let s = fixture_minimal();
        assert_eq!(serialize(&parse(s).unwrap()), s);
    }

    #[test]
    fn round_trip_with_preamble_and_two_phases() {
        let s = fixture_with_preamble_and_two_phases();
        let plan = parse(s).unwrap();
        assert_eq!(plan.phases.len(), 2);
        assert_eq!(plan.preamble, "\n# Foreman\n\nIntro paragraph.\n\n");
        assert_eq!(plan.phases[0].title, "Foundation");
        assert_eq!(plan.phases[1].title, "Domain types");
        assert_eq!(serialize(&plan), s);
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(parse(""), Err(PlanParseError::MissingFrontmatter));
    }

    #[test]
    fn rejects_missing_frontmatter() {
        assert_eq!(
            parse("# Phase 01: Hello\n\nbody\n"),
            Err(PlanParseError::MissingFrontmatter)
        );
    }

    #[test]
    fn rejects_unterminated_frontmatter() {
        assert_eq!(
            parse("---\ncurrent_phase: \"01\"\n"),
            Err(PlanParseError::MissingFrontmatter)
        );
    }

    #[test]
    fn rejects_frontmatter_only() {
        assert_eq!(
            parse("---\ncurrent_phase: \"01\"\n---\n"),
            Err(PlanParseError::NoPhases)
        );
    }

    #[test]
    fn rejects_missing_current_phase() {
        let err = parse("---\nproject: foreman\n---\n\n# Phase 01: Hi\n\nbody\n").unwrap_err();
        assert!(matches!(err, PlanParseError::BadFrontmatter(_)));
    }

    #[test]
    fn rejects_non_string_current_phase() {
        let err = parse("---\ncurrent_phase: 1\n---\n\n# Phase 01: Hi\n\nbody\n").unwrap_err();
        assert!(matches!(err, PlanParseError::BadFrontmatter(_)));
    }

    #[test]
    fn rejects_duplicate_phase_ids() {
        let s = "---\ncurrent_phase: \"01\"\n---\n\n# Phase 01: A\n\nbody\n# Phase 01: B\n\nmore\n";
        assert_eq!(
            parse(s),
            Err(PlanParseError::DuplicatePhaseId("01".to_string()))
        );
    }

    #[test]
    fn rejects_unknown_current_phase() {
        let s = "---\ncurrent_phase: \"99\"\n---\n\n# Phase 01: Hi\n\nbody\n";
        assert_eq!(
            parse(s),
            Err(PlanParseError::UnknownCurrentPhase("99".to_string()))
        );
    }

    #[test]
    fn rejects_bad_heading_format() {
        let s = "---\ncurrent_phase: \"01\"\n---\n\n# Phase 01 missing colon\n\nbody\n";
        let err = parse(s).unwrap_err();
        assert!(matches!(err, PlanParseError::BadHeading { .. }));
    }

    #[test]
    fn rejects_bad_heading_id() {
        let s = "---\ncurrent_phase: \"01\"\n---\n\n# Phase abc: oops\n\nbody\n";
        let err = parse(s).unwrap_err();
        assert!(matches!(err, PlanParseError::BadHeadingId { .. }));
    }

    #[test]
    fn unknown_frontmatter_keys_are_accepted_with_warning() {
        let s = "---\ncurrent_phase: \"01\"\nproject: foreman\nweird_key: 42\n---\n\n# Phase 01: A\n\nbody\n";
        let plan = parse(s).unwrap();
        assert_eq!(plan.current_phase.as_str(), "01");
        // round-trip preserves the unknown keys verbatim
        assert_eq!(serialize(&plan), s);
    }

    #[test]
    fn normalizes_crlf_to_lf_on_parse() {
        let crlf = "---\r\ncurrent_phase: \"01\"\r\n---\r\n\r\n# Phase 01: A\r\n\r\nbody\r\n";
        let plan = parse(crlf).unwrap();
        // Output is LF only.
        let out = serialize(&plan);
        assert!(!out.contains('\r'));
        assert!(out.contains("# Phase 01: A\n"));
    }

    #[test]
    fn set_current_phase_rewrites_only_that_line() {
        let s = fixture_with_preamble_and_two_phases();
        let mut plan = parse(s).unwrap();
        let new_id = PhaseId::parse("01").unwrap();
        plan.set_current_phase(new_id.clone());
        assert_eq!(plan.current_phase, new_id);
        // Frontmatter still has project: foreman.
        assert!(plan.frontmatter.contains("project: foreman"));
        assert!(plan.frontmatter.contains("current_phase: \"01\""));
        assert!(!plan.frontmatter.contains("current_phase: \"02\""));
        // Round-trips against itself after mutation.
        let out = serialize(&plan);
        let plan2 = parse(&out).unwrap();
        assert_eq!(plan2.current_phase.as_str(), "01");
    }

    #[test]
    fn set_current_phase_appends_when_missing() {
        let mut plan = Plan {
            current_phase: PhaseId::parse("01").unwrap(),
            frontmatter: "project: foreman".to_string(),
            preamble: String::new(),
            phases: vec![Phase {
                id: PhaseId::parse("01").unwrap(),
                title: "x".into(),
                body: String::new(),
            }],
        };
        plan.set_current_phase(PhaseId::parse("02").unwrap());
        assert!(plan.frontmatter.contains("project: foreman"));
        assert!(plan.frontmatter.contains("current_phase: \"02\""));
    }
}
