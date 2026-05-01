//! Auto-generated pull-request title and body for `pitboss play --pr`.
//!
//! After a run finishes successfully, the runner hands the loaded [`Plan`],
//! the final [`RunState`], and the current [`DeferredDoc`] to [`pr_body`] /
//! [`pr_title`] to produce a single-line title and a multi-section markdown
//! body. The body lists the phases the run committed (with their titles
//! resolved against the plan), any unchecked deferred items still outstanding,
//! any deferred phase blocks the agent emitted, and the run's accumulated
//! token usage. The text is intentionally plain markdown — no HTML, no GH
//! mentions — so it survives `gh pr create --body` verbatim.

use anyhow::{Context, Result};

use crate::deferred::DeferredDoc;
use crate::git::Git;
use crate::plan::Plan;
use crate::state::RunState;

/// Aggregated inputs for [`pr_body`] / [`pr_title`]. Built by the runner
/// caller and passed in as a single value so the API stays stable as we add
/// more rendered sections later (e.g., test summaries).
pub struct PrSummary<'a> {
    /// The plan the run was driven against. Used to resolve phase titles.
    pub plan: &'a Plan,
    /// State at the moment the run finished — completed phases and token
    /// totals come from here.
    pub state: &'a RunState,
    /// Deferred document at the moment the run finished. Unchecked items and
    /// any deferred phase blocks are surfaced in the PR body so the reviewer
    /// sees follow-up work the run punted on.
    pub deferred: &'a DeferredDoc,
}

/// One-line PR title summarizing the run's scope.
///
/// Format: `pitboss: <N> phase(s) — <first title> … <last title>`. The single
/// quote keeps the title under GitHub's 256-char limit even on long plans;
/// reviewers get a quick "what scope did this PR cover" read at the top of
/// the issues list. A run that committed zero phases (only excluded paths
/// touched) collapses to `pitboss: no phases committed`.
pub fn pr_title(summary: &PrSummary<'_>) -> String {
    let count = summary.state.completed.len();
    if count == 0 {
        return "pitboss: no phases committed".to_string();
    }
    let first_id = &summary.state.completed[0];
    let last_id = &summary.state.completed[count - 1];
    let first_title = summary
        .plan
        .phase(first_id)
        .map(|p| p.title.as_str())
        .unwrap_or("(unknown)");
    if count == 1 {
        return format!("pitboss: phase {first_id} — {first_title}");
    }
    let last_title = summary
        .plan
        .phase(last_id)
        .map(|p| p.title.as_str())
        .unwrap_or("(unknown)");
    format!("pitboss: {count} phases ({first_id}–{last_id}) — {first_title} … {last_title}")
}

/// Multi-section markdown PR body. Sections are emitted in this order:
///
/// 1. `## Run` — run id, branch, original branch (when known).
/// 2. `## Completed phases` — bulleted list of `phase <id>: <title>`.
/// 3. `## Deferred items` — only emitted if any unchecked items remain.
/// 4. `## Deferred phases` — only emitted if the run produced any.
/// 5. `## Token usage` — input/output totals plus per-role breakdown.
///
/// Empty sections are omitted entirely so a clean run produces a tight body.
pub fn pr_body(summary: &PrSummary<'_>) -> String {
    let mut out = String::new();

    out.push_str("## Run\n\n");
    out.push_str(&format!("- run id: `{}`\n", summary.state.run_id));
    out.push_str(&format!("- branch: `{}`\n", summary.state.branch));
    if let Some(original) = &summary.state.original_branch {
        out.push_str(&format!("- original branch: `{}`\n", original));
    }
    out.push('\n');

    out.push_str("## Completed phases\n\n");
    if summary.state.completed.is_empty() {
        out.push_str("_None — the run produced no per-phase commits._\n\n");
    } else {
        for phase_id in &summary.state.completed {
            let title = summary
                .plan
                .phase(phase_id)
                .map(|p| p.title.as_str())
                .unwrap_or("(unknown)");
            out.push_str(&format!("- phase {phase_id}: {title}\n"));
        }
        out.push('\n');
    }

    let unchecked: Vec<&str> = summary
        .deferred
        .items
        .iter()
        .filter(|i| !i.done)
        .map(|i| i.text.as_str())
        .collect();
    if !unchecked.is_empty() {
        out.push_str("## Deferred items\n\n");
        for text in &unchecked {
            out.push_str(&format!("- [ ] {text}\n"));
        }
        out.push('\n');
    }

    if !summary.deferred.phases.is_empty() {
        out.push_str("## Deferred phases\n\n");
        for phase in &summary.deferred.phases {
            out.push_str(&format!(
                "- from phase {}: {}\n",
                phase.source_phase, phase.title
            ));
        }
        out.push('\n');
    }

    let usage = &summary.state.token_usage;
    out.push_str("## Token usage\n\n");
    out.push_str(&format!(
        "- input: {}\n- output: {}\n",
        usage.input, usage.output
    ));
    if !usage.by_role.is_empty() {
        let mut roles: Vec<(&String, &crate::state::RoleUsage)> = usage.by_role.iter().collect();
        roles.sort_by(|a, b| a.0.cmp(b.0));
        for (role, ru) in roles {
            out.push_str(&format!(
                "  - {}: input={} output={}\n",
                role, ru.input, ru.output
            ));
        }
    }

    out
}

/// One-line PR title for a `pitboss grind --pr` run.
///
/// Format: `grind/<plan-or-default>: <run-id>`. Stable so a script watching
/// the PR queue can match on it. The run id is already a UTC timestamp + hex
/// suffix, so the title is unique per run.
pub fn grind_pr_title(plan_name: &str, run_id: &str) -> String {
    format!("grind/{plan_name}: {run_id}")
}

/// Open a pull request for a finished `pitboss grind` run. The body is the
/// run's `sessions.md` verbatim — the markdown projection of `sessions.jsonl`
/// is already a reviewable per-session table (see
/// [`crate::grind::render_sessions_md`]). Returns the URL `gh pr create`
/// printed on success.
///
/// Lives next to [`pr_title`] / [`pr_body`] (both `pitboss play` helpers) so
/// the two subcommands share one PR module rather than each carrying their
/// own gh shell-out wrapper.
pub async fn open_grind_pr<G: Git + ?Sized>(
    git: &G,
    plan_name: &str,
    run_id: &str,
    sessions_md: &str,
) -> Result<String> {
    let title = grind_pr_title(plan_name, run_id);
    git.open_pr(&title, sessions_md)
        .await
        .context("opening PR via gh pr create")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deferred::{DeferredItem, DeferredPhase};
    use crate::plan::{Phase, PhaseId};
    use crate::state::{RoleUsage, RunState, TokenUsage};
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    fn three_phase_plan() -> Plan {
        Plan::new(
            pid("03"),
            vec![
                Phase {
                    id: pid("01"),
                    title: "Foundation".into(),
                    body: String::new(),
                },
                Phase {
                    id: pid("02"),
                    title: "Domain types".into(),
                    body: String::new(),
                },
                Phase {
                    id: pid("03"),
                    title: "Plan parser".into(),
                    body: String::new(),
                },
            ],
        )
    }

    fn sample_state(completed: Vec<PhaseId>) -> RunState {
        let mut by_role = HashMap::new();
        by_role.insert(
            "implementer".to_string(),
            RoleUsage {
                input: 1234,
                output: 567,
            },
        );
        RunState {
            run_id: "20260429T143022Z".into(),
            branch: "pitboss/run-20260429T143022Z".into(),
            original_branch: Some("main".into()),
            started_at: DateTime::parse_from_rfc3339("2026-04-29T14:30:22Z")
                .unwrap()
                .with_timezone(&Utc),
            started_phase: pid("01"),
            completed,
            attempts: HashMap::new(),
            token_usage: TokenUsage {
                input: 1234,
                output: 567,
                by_role,
            },
            aborted: false,
            pending_sweep: false,
            consecutive_sweeps: 0,
        }
    }

    #[test]
    fn title_for_zero_committed_phases_is_explicit() {
        let plan = three_phase_plan();
        let state = sample_state(vec![]);
        let deferred = DeferredDoc::empty();
        let summary = PrSummary {
            plan: &plan,
            state: &state,
            deferred: &deferred,
        };
        assert_eq!(pr_title(&summary), "pitboss: no phases committed");
    }

    #[test]
    fn title_for_single_phase_uses_that_phase() {
        let plan = three_phase_plan();
        let state = sample_state(vec![pid("02")]);
        let deferred = DeferredDoc::empty();
        let summary = PrSummary {
            plan: &plan,
            state: &state,
            deferred: &deferred,
        };
        assert_eq!(pr_title(&summary), "pitboss: phase 02 — Domain types");
    }

    #[test]
    fn title_for_multi_phase_run_lists_first_and_last() {
        let plan = three_phase_plan();
        let state = sample_state(vec![pid("01"), pid("02"), pid("03")]);
        let deferred = DeferredDoc::empty();
        let summary = PrSummary {
            plan: &plan,
            state: &state,
            deferred: &deferred,
        };
        assert_eq!(
            pr_title(&summary),
            "pitboss: 3 phases (01–03) — Foundation … Plan parser"
        );
    }

    #[test]
    fn body_includes_run_metadata_and_completed_phases() {
        let plan = three_phase_plan();
        let state = sample_state(vec![pid("01"), pid("02")]);
        let deferred = DeferredDoc::empty();
        let summary = PrSummary {
            plan: &plan,
            state: &state,
            deferred: &deferred,
        };
        let body = pr_body(&summary);
        assert!(body.contains("## Run\n"), "body: {body}");
        assert!(
            body.contains("- run id: `20260429T143022Z`"),
            "body: {body}"
        );
        assert!(
            body.contains("- branch: `pitboss/run-20260429T143022Z`"),
            "body: {body}"
        );
        assert!(body.contains("- original branch: `main`"), "body: {body}");
        assert!(body.contains("## Completed phases\n"), "body: {body}");
        assert!(body.contains("- phase 01: Foundation"), "body: {body}");
        assert!(body.contains("- phase 02: Domain types"), "body: {body}");
        // No deferred sections when there's nothing to report.
        assert!(!body.contains("## Deferred items"), "body: {body}");
        assert!(!body.contains("## Deferred phases"), "body: {body}");
        // Token usage always renders.
        assert!(body.contains("## Token usage\n"), "body: {body}");
        assert!(body.contains("- input: 1234"), "body: {body}");
        assert!(body.contains("- output: 567"), "body: {body}");
        assert!(
            body.contains("- implementer: input=1234 output=567"),
            "body: {body}"
        );
    }

    #[test]
    fn body_emits_deferred_sections_only_when_present() {
        let plan = three_phase_plan();
        let state = sample_state(vec![pid("01")]);
        let deferred = DeferredDoc {
            items: vec![
                DeferredItem {
                    text: "polish error message".into(),
                    done: false,
                },
                DeferredItem {
                    text: "completed item should not show".into(),
                    done: true,
                },
            ],
            phases: vec![DeferredPhase {
                source_phase: pid("01"),
                title: "rework agent trait".into(),
                body: String::new(),
            }],
        };
        let summary = PrSummary {
            plan: &plan,
            state: &state,
            deferred: &deferred,
        };
        let body = pr_body(&summary);
        assert!(body.contains("## Deferred items"), "body: {body}");
        assert!(body.contains("- [ ] polish error message"), "body: {body}");
        // Done items are filtered before rendering — they were swept.
        assert!(
            !body.contains("completed item should not show"),
            "body: {body}"
        );
        assert!(body.contains("## Deferred phases"), "body: {body}");
        assert!(
            body.contains("- from phase 01: rework agent trait"),
            "body: {body}"
        );
    }

    #[test]
    fn body_for_zero_committed_phases_says_so() {
        let plan = three_phase_plan();
        let state = sample_state(vec![]);
        let deferred = DeferredDoc::empty();
        let summary = PrSummary {
            plan: &plan,
            state: &state,
            deferred: &deferred,
        };
        let body = pr_body(&summary);
        assert!(
            body.contains("_None — the run produced no per-phase commits._"),
            "body: {body}"
        );
    }

    #[test]
    fn body_omits_original_branch_line_when_unset() {
        let plan = three_phase_plan();
        let mut state = sample_state(vec![pid("01")]);
        state.original_branch = None;
        let deferred = DeferredDoc::empty();
        let summary = PrSummary {
            plan: &plan,
            state: &state,
            deferred: &deferred,
        };
        let body = pr_body(&summary);
        assert!(!body.contains("original branch"), "body: {body}");
    }
}
