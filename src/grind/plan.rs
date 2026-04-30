//! Grind plan files plus the synthesized default plan.
//!
//! A grind plan pins which prompts participate in a rotation, applies per-plan
//! overrides on top of frontmatter defaults, and carries the hooks and budgets
//! the runner enforces. Plans live at `.pitboss/plans/<name>.toml`; the file
//! stem becomes the plan's [`GrindPlan::name`] (the body never carries a
//! `name` key — that source of truth is the path on disk). When no plan file
//! is selected, [`default_plan_from_dir`] synthesizes one that rotates through
//! every discovered prompt with no overrides.
//!
//! Phase 04 only loads and validates plans. Later phases drive execution.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::prompt::PromptDoc;

/// Synthesized plan name used by [`default_plan_from_dir`] when no plan file
/// has been selected.
pub const DEFAULT_PLAN_NAME: &str = "default";

/// A loaded or synthesized grind plan.
///
/// `PartialEq` only — `PlanBudgets` carries `f64` fields so `Eq` is not
/// available transitively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrindPlan {
    /// Plan name. Always set from the file stem on load (or
    /// [`DEFAULT_PLAN_NAME`] for the synthesized default plan); it is never
    /// read from the TOML body. Marked `serde(skip)` so a stray `name = "..."`
    /// in a plan file is rejected by `deny_unknown_fields`.
    #[serde(skip)]
    pub name: String,
    /// Prompts pinned by this plan, in author-supplied order. Each entry may
    /// override the prompt's frontmatter weight / every / max_runs.
    #[serde(default)]
    pub prompts: Vec<PlanPromptRef>,
    /// Cap on concurrently-running sessions for this plan. Defaults to `1`
    /// (sequential).
    #[serde(default = "default_max_parallel")]
    pub max_parallel: u32,
    /// Plan-level shell hooks fired around each session. Inherited from the
    /// `[grind.hooks]` config block when a field is unset.
    #[serde(default)]
    pub hooks: Hooks,
    /// Plan-level budgets applied across the whole run.
    #[serde(default)]
    pub budgets: PlanBudgets,
}

/// One prompt entry inside a [`GrindPlan`]. Overrides null out per-prompt
/// frontmatter values when explicitly set; `None` means "inherit".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanPromptRef {
    /// Name of the prompt this entry refers to. Must match a discovered prompt
    /// when the plan is later validated against the prompt set.
    pub name: String,
    /// Plan-level override for the prompt's `weight` frontmatter.
    #[serde(default)]
    pub weight_override: Option<u32>,
    /// Plan-level override for the prompt's `every` frontmatter.
    #[serde(default)]
    pub every_override: Option<u32>,
    /// Plan-level override for the prompt's `max_runs` frontmatter.
    #[serde(default)]
    pub max_runs_override: Option<u32>,
}

/// Plan- or config-level shell hooks. Each value is a raw shell string
/// executed by the runner around a session (see Phase 10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hooks {
    /// Runs before the agent dispatch. Non-zero exit skips the session.
    pub pre_session: Option<String>,
    /// Runs after the session resolves, regardless of status.
    pub post_session: Option<String>,
    /// Runs after the session resolves, only when status is non-`Ok`.
    pub on_failure: Option<String>,
}

/// Plan- or config-level budgets. Any combination may be set; the first to
/// trip wins at runtime (Phase 08).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlanBudgets {
    /// Hard cap on number of sessions dispatched.
    pub max_iterations: Option<u32>,
    /// Hard wall-clock cutoff. Once reached, no further sessions are
    /// dispatched.
    pub until: Option<DateTime<Utc>>,
    /// Hard cap on cumulative agent cost in USD.
    pub max_cost_usd: Option<f64>,
    /// Hard cap on cumulative tokens (input + output) across all roles.
    pub max_tokens: Option<u64>,
}

fn default_max_parallel() -> u32 {
    1
}

/// Errors produced by [`load_plan`].
///
/// `PartialEq` is implemented manually so [`Self::Io`] can compare on path
/// alone (`std::io::Error` is not `PartialEq`).
#[derive(Debug, Error)]
pub enum PlanLoadError {
    /// The plan file could not be read from disk.
    #[error("failed to read plan file {path}: {source}")]
    Io {
        /// Display path of the offending file.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The path had no UTF-8 file stem to derive the plan's name from.
    #[error("plan path has no UTF-8 file stem: {path}")]
    MissingName {
        /// Display path of the offending file.
        path: String,
    },
    /// The TOML body did not parse, or did not match the plan schema.
    #[error("{path}: malformed plan: {message}")]
    Malformed {
        /// Display path of the offending file.
        path: String,
        /// One-line diagnostic.
        message: String,
    },
    /// The same `name` appeared in `prompts` more than once.
    #[error("{path}: duplicate prompt entry {name:?}")]
    DuplicatePrompt {
        /// Display path of the offending file.
        path: String,
        /// The duplicated prompt name.
        name: String,
    },
    /// A semantic constraint (e.g., `max_parallel >= 1`) was violated.
    #[error("{path}: invalid plan: {message}")]
    Invalid {
        /// Display path of the offending file.
        path: String,
        /// One-line diagnostic.
        message: String,
    },
}

impl PartialEq for PlanLoadError {
    fn eq(&self, other: &Self) -> bool {
        use PlanLoadError::*;
        match (self, other) {
            (Io { path: a, .. }, Io { path: b, .. }) => a == b,
            (MissingName { path: a }, MissingName { path: b }) => a == b,
            (
                Malformed {
                    path: a,
                    message: am,
                },
                Malformed {
                    path: b,
                    message: bm,
                },
            ) => a == b && am == bm,
            (DuplicatePrompt { path: a, name: an }, DuplicatePrompt { path: b, name: bn }) => {
                a == b && an == bn
            }
            (
                Invalid {
                    path: a,
                    message: am,
                },
                Invalid {
                    path: b,
                    message: bm,
                },
            ) => a == b && am == bm,
            _ => false,
        }
    }
}

/// Errors produced by [`GrindPlan::validate_against`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlanValidationError {
    /// A `prompts` entry referenced a name that was not in the discovered
    /// prompt set.
    #[error("plan {plan:?} references unknown prompt {prompt:?}")]
    UnknownPrompt {
        /// Plan whose entry is dangling.
        plan: String,
        /// Referenced prompt name that does not exist.
        prompt: String,
    },
}

/// Read and parse a plan file. The file stem is taken as the plan's
/// [`GrindPlan::name`].
pub fn load_plan(path: &Path) -> Result<GrindPlan, PlanLoadError> {
    let display = path.display().to_string();
    let raw = fs::read_to_string(path).map_err(|e| PlanLoadError::Io {
        path: display.clone(),
        source: e,
    })?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| PlanLoadError::MissingName {
            path: display.clone(),
        })?
        .to_string();
    parse_plan_str(&raw, name, &display)
}

/// Parse a plan body from an in-memory string. `name` becomes
/// [`GrindPlan::name`]; `display` is used only in error messages so callers
/// like the test suite can pass a fixture path.
pub fn parse_plan_str(raw: &str, name: String, display: &str) -> Result<GrindPlan, PlanLoadError> {
    let mut plan: GrindPlan = toml::from_str(raw).map_err(|e| PlanLoadError::Malformed {
        path: display.to_string(),
        message: one_line(&e.to_string()),
    })?;
    plan.name = name;

    if plan.max_parallel == 0 {
        return Err(PlanLoadError::Invalid {
            path: display.to_string(),
            message: "max_parallel must be >= 1".to_string(),
        });
    }

    let mut seen: HashSet<&str> = HashSet::new();
    for entry in &plan.prompts {
        if !seen.insert(entry.name.as_str()) {
            return Err(PlanLoadError::DuplicatePrompt {
                path: display.to_string(),
                name: entry.name.clone(),
            });
        }
    }

    Ok(plan)
}

fn one_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_string()
}

/// Synthesize a plan that rotates through every discovered prompt once, with
/// no overrides. The resulting plan's `name` is [`DEFAULT_PLAN_NAME`].
///
/// Duplicate prompt names in the input (which discovery already de-dupes by
/// precedence) are silently collapsed to first occurrence so this helper is
/// tolerant of any iterator the caller hands in.
pub fn default_plan_from_dir(prompts: &[PromptDoc]) -> GrindPlan {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut refs: Vec<PlanPromptRef> = Vec::new();
    for p in prompts {
        if seen.insert(p.meta.name.as_str()) {
            refs.push(PlanPromptRef {
                name: p.meta.name.clone(),
                weight_override: None,
                every_override: None,
                max_runs_override: None,
            });
        }
    }
    GrindPlan {
        name: DEFAULT_PLAN_NAME.to_string(),
        prompts: refs,
        max_parallel: 1,
        hooks: Hooks::default(),
        budgets: PlanBudgets::default(),
    }
}

impl GrindPlan {
    /// Cross-check that every `prompts` entry refers to a discovered prompt.
    /// Stops at the first dangling reference.
    pub fn validate_against(&self, prompts: &[PromptDoc]) -> Result<(), PlanValidationError> {
        let names: HashSet<&str> = prompts.iter().map(|p| p.meta.name.as_str()).collect();
        for entry in &self.prompts {
            if !names.contains(entry.name.as_str()) {
                return Err(PlanValidationError::UnknownPrompt {
                    plan: self.name.clone(),
                    prompt: entry.name.clone(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grind::prompt::{PromptMeta, PromptSource};
    use std::path::PathBuf;

    fn fake_prompt(name: &str) -> PromptDoc {
        PromptDoc {
            meta: PromptMeta {
                name: name.to_string(),
                description: "desc".to_string(),
                weight: 1,
                every: 1,
                max_runs: None,
                verify: false,
                parallel_safe: false,
                tags: Vec::new(),
                max_session_seconds: None,
                max_session_cost_usd: None,
            },
            body: String::new(),
            source_path: PathBuf::from(format!("/fixture/{name}.md")),
            source_kind: PromptSource::Project,
        }
    }

    fn parse(raw: &str, name: &str) -> Result<GrindPlan, PlanLoadError> {
        parse_plan_str(raw, name.to_string(), "/fixture/plan.toml")
    }

    #[test]
    fn full_plan_round_trips() {
        let raw = r#"
max_parallel = 4

[[prompts]]
name = "fp-hunter"
weight_override = 5
every_override = 2
max_runs_override = 10

[[prompts]]
name = "triage"

[hooks]
pre_session = "echo start"
post_session = "echo done"
on_failure = "echo fail"

[budgets]
max_iterations = 50
until = "2026-05-01T00:00:00Z"
max_cost_usd = 5.0
max_tokens = 1000000
"#;
        let plan = parse(raw, "fp-cleanup").expect("parse should succeed");
        assert_eq!(plan.name, "fp-cleanup");
        assert_eq!(plan.max_parallel, 4);
        assert_eq!(plan.prompts.len(), 2);
        assert_eq!(plan.prompts[0].name, "fp-hunter");
        assert_eq!(plan.prompts[0].weight_override, Some(5));
        assert_eq!(plan.prompts[0].every_override, Some(2));
        assert_eq!(plan.prompts[0].max_runs_override, Some(10));
        assert_eq!(plan.prompts[1].name, "triage");
        assert_eq!(plan.prompts[1].weight_override, None);
        assert_eq!(plan.hooks.pre_session.as_deref(), Some("echo start"));
        assert_eq!(plan.hooks.post_session.as_deref(), Some("echo done"));
        assert_eq!(plan.hooks.on_failure.as_deref(), Some("echo fail"));
        assert_eq!(plan.budgets.max_iterations, Some(50));
        assert_eq!(plan.budgets.max_cost_usd, Some(5.0));
        assert_eq!(plan.budgets.max_tokens, Some(1_000_000));
        assert!(plan.budgets.until.is_some());
    }

    #[test]
    fn empty_body_yields_defaults_with_supplied_name() {
        let plan = parse("", "empty").expect("parse should succeed");
        assert_eq!(plan.name, "empty");
        assert_eq!(plan.max_parallel, 1);
        assert!(plan.prompts.is_empty());
        assert_eq!(plan.hooks, Hooks::default());
        assert_eq!(plan.budgets, PlanBudgets::default());
    }

    #[test]
    fn duplicate_prompt_name_is_rejected() {
        let raw = r#"
[[prompts]]
name = "fp-hunter"

[[prompts]]
name = "fp-hunter"
"#;
        let err = parse(raw, "p").unwrap_err();
        match err {
            PlanLoadError::DuplicatePrompt { name, .. } => assert_eq!(name, "fp-hunter"),
            other => panic!("expected DuplicatePrompt, got {other:?}"),
        }
    }

    #[test]
    fn name_in_body_is_rejected_as_unknown_field() {
        // The plan's `name` is the file stem; encoding it in the TOML body is
        // a foot-gun (the body's value would silently lose to the path). The
        // schema rejects it via `deny_unknown_fields`.
        let raw = "name = \"oops\"\n";
        let err = parse(raw, "real-name").unwrap_err();
        assert!(matches!(err, PlanLoadError::Malformed { .. }));
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let raw = "frobnicate = 7\n";
        let err = parse(raw, "p").unwrap_err();
        assert!(matches!(err, PlanLoadError::Malformed { .. }));
    }

    #[test]
    fn malformed_toml_is_rejected() {
        let raw = "[[prompts\nname = 'broken'\n";
        let err = parse(raw, "p").unwrap_err();
        assert!(matches!(err, PlanLoadError::Malformed { .. }));
    }

    #[test]
    fn max_parallel_zero_is_rejected() {
        let raw = "max_parallel = 0\n";
        let err = parse(raw, "p").unwrap_err();
        match err {
            PlanLoadError::Invalid { message, .. } => {
                assert!(message.contains("max_parallel"), "msg: {message}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn load_plan_uses_file_stem_as_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nightly-cleanup.toml");
        std::fs::write(&path, "max_parallel = 2\n").unwrap();
        let plan = load_plan(&path).expect("load should succeed");
        assert_eq!(plan.name, "nightly-cleanup");
        assert_eq!(plan.max_parallel, 2);
    }

    #[test]
    fn load_plan_reports_io_error_for_missing_path() {
        let err = load_plan(Path::new("/no/such/plan.toml")).unwrap_err();
        assert!(matches!(err, PlanLoadError::Io { .. }));
    }

    #[test]
    fn default_plan_synthesizes_one_entry_per_prompt() {
        let prompts = vec![
            fake_prompt("alpha"),
            fake_prompt("bravo"),
            fake_prompt("charlie"),
        ];
        let plan = default_plan_from_dir(&prompts);
        assert_eq!(plan.name, DEFAULT_PLAN_NAME);
        assert_eq!(plan.max_parallel, 1);
        assert_eq!(plan.hooks, Hooks::default());
        assert_eq!(plan.budgets, PlanBudgets::default());
        let names: Vec<&str> = plan.prompts.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
        for r in &plan.prompts {
            assert_eq!(r.weight_override, None);
            assert_eq!(r.every_override, None);
            assert_eq!(r.max_runs_override, None);
        }
    }

    #[test]
    fn default_plan_handles_empty_prompt_set() {
        let plan = default_plan_from_dir(&[]);
        assert_eq!(plan.name, DEFAULT_PLAN_NAME);
        assert!(plan.prompts.is_empty());
    }

    #[test]
    fn validate_against_accepts_known_prompts() {
        let prompts = vec![fake_prompt("alpha"), fake_prompt("bravo")];
        let plan = default_plan_from_dir(&prompts);
        plan.validate_against(&prompts).unwrap();
    }

    #[test]
    fn validate_against_rejects_unknown_prompt() {
        let prompts = vec![fake_prompt("alpha")];
        let raw = r#"
[[prompts]]
name = "ghost"
"#;
        let plan = parse(raw, "p").unwrap();
        let err = plan.validate_against(&prompts).unwrap_err();
        assert_eq!(
            err,
            PlanValidationError::UnknownPrompt {
                plan: "p".to_string(),
                prompt: "ghost".to_string(),
            }
        );
    }

    #[test]
    fn plan_round_trips_through_serialize_and_load() {
        // Serialize a plan, write it to disk under the right stem, load it
        // back, and assert no field was lost.
        let original = GrindPlan {
            name: "round-trip".to_string(),
            prompts: vec![
                PlanPromptRef {
                    name: "alpha".to_string(),
                    weight_override: Some(2),
                    every_override: None,
                    max_runs_override: Some(7),
                },
                PlanPromptRef {
                    name: "bravo".to_string(),
                    weight_override: None,
                    every_override: Some(3),
                    max_runs_override: None,
                },
            ],
            max_parallel: 3,
            hooks: Hooks {
                pre_session: Some("setup".to_string()),
                post_session: None,
                on_failure: Some("page-oncall".to_string()),
            },
            budgets: PlanBudgets {
                max_iterations: Some(20),
                until: Some("2026-05-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()),
                max_cost_usd: Some(2.25),
                max_tokens: Some(500_000),
            },
        };
        let body = toml::to_string(&original).expect("serialize plan");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round-trip.toml");
        std::fs::write(&path, body).unwrap();
        let reparsed = load_plan(&path).expect("load round-trip plan");
        assert_eq!(reparsed, original);
    }
}
