//! `foreman status` — print a summary of the current run.
//!
//! Loads `state.json`, `plan.md`, and `deferred.md` and renders a multi-line
//! report covering the run id and branch, the active phase against the
//! plan's phase count, completed phases, deferred work, accumulated token
//! usage, and the last commit on the run branch.
//!
//! `status` is read-only: it never mutates state, never creates branches, and
//! is safe to invoke at any time. A workspace with no started run prints a
//! single line indicating that fact (and the seed plan's current phase) so
//! `foreman init && foreman status` is meaningful.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::{self, Config};
use crate::deferred::{self, DeferredDoc};
use crate::plan::{self, Plan};
use crate::runner;
use crate::state::{self, RunState};

/// Top-level entry point for the `status` subcommand. Prints to stdout.
pub fn run(workspace: PathBuf) -> Result<()> {
    let plan = load_plan(&workspace)?;
    let deferred = load_deferred(&workspace)?;
    let state = state::load(&workspace)
        .with_context(|| format!("status: loading state in {:?}", workspace))?;
    let config = config::load(&workspace)
        .with_context(|| format!("status: loading config in {:?}", workspace))?;

    let report = render_report(&workspace, &plan, &deferred, state.as_ref(), &config);
    print!("{}", report);
    Ok(())
}

/// Build the human-readable status report. Pure function over the loaded
/// state so tests can exercise it without shelling out to git for the
/// last-commit lookup; the workspace is only used to query git, and
/// [`last_commit_subject`] swallows errors so a non-git workspace still
/// produces a useful report.
pub fn render_report(
    workspace: &Path,
    plan: &Plan,
    deferred: &DeferredDoc,
    state: Option<&RunState>,
    config: &Config,
) -> String {
    let mut out = String::new();

    let total_phases = plan.phases.len();
    let current_phase_index = plan
        .phases
        .iter()
        .position(|p| p.id == plan.current_phase)
        .map(|i| i + 1);
    let current_phase_title = plan
        .phase(&plan.current_phase)
        .map(|p| p.title.as_str())
        .unwrap_or("(unknown)");

    match state {
        None => {
            out.push_str("run: not started (no .foreman/state.json)\n");
        }
        Some(s) if s.aborted => {
            out.push_str(&format!(
                "run: {} (aborted, started {})\n",
                s.run_id,
                s.started_at.to_rfc3339()
            ));
            out.push_str(&format!("branch: {}\n", s.branch));
            if let Some(orig) = &s.original_branch {
                out.push_str(&format!("original branch: {}\n", orig));
            }
        }
        Some(s) => {
            out.push_str(&format!(
                "run: {} (started {})\n",
                s.run_id,
                s.started_at.to_rfc3339()
            ));
            out.push_str(&format!("branch: {}\n", s.branch));
            if let Some(orig) = &s.original_branch {
                out.push_str(&format!("original branch: {}\n", orig));
            }
        }
    }

    out.push_str(&match current_phase_index {
        Some(i) => format!(
            "plan: phase {} of {} — {} ({})\n",
            plan.current_phase, total_phases, current_phase_title, i
        ),
        None => format!(
            "plan: current phase {} not found in plan ({} phases total)\n",
            plan.current_phase, total_phases
        ),
    });

    if let Some(s) = state {
        if s.completed.is_empty() {
            out.push_str("completed: (none)\n");
        } else {
            let joined: Vec<&str> = s.completed.iter().map(|p| p.as_str()).collect();
            out.push_str(&format!("completed: {}\n", joined.join(", ")));
        }
    }

    let unchecked = deferred.items.iter().filter(|i| !i.done).count();
    let checked = deferred.items.len() - unchecked;
    out.push_str(&format!(
        "deferred items: {} ({} unchecked, {} checked)\n",
        deferred.items.len(),
        unchecked,
        checked
    ));
    out.push_str(&format!("deferred phases: {}\n", deferred.phases.len()));

    if let Some(s) = state {
        let usage = &s.token_usage;
        out.push_str(&format!(
            "tokens: input={} output={}\n",
            usage.input, usage.output
        ));
        if !usage.by_role.is_empty() {
            let mut roles: Vec<(&String, &state::RoleUsage)> = usage.by_role.iter().collect();
            roles.sort_by(|a, b| a.0.cmp(b.0));
            for (role, ru) in roles {
                out.push_str(&format!(
                    "  {}: input={} output={}\n",
                    role, ru.input, ru.output
                ));
            }
        }
        out.push_str(&render_budgets(config, usage));
    }

    if let Some(s) = state {
        match last_commit_subject(workspace, &s.branch) {
            Some(line) => out.push_str(&format!("last commit: {}\n", line)),
            None => out.push_str("last commit: (none)\n"),
        }
    }

    out
}

/// Render the budget block of the status report.
///
/// Always prints the running USD cost (computed from
/// [`crate::runner::budget_totals`]) so users can see spend at a glance even
/// without budgets configured. When either budget cap is set, an extra line
/// per cap reports usage against that cap.
fn render_budgets(config: &Config, usage: &crate::state::TokenUsage) -> String {
    let (total_tokens, total_usd) = runner::budget_totals(config, usage);
    let mut out = format!("cost: ${:.4} ({} tokens)\n", total_usd, total_tokens);
    if let Some(cap) = config.budgets.max_total_tokens {
        let remaining = cap.saturating_sub(total_tokens);
        out.push_str(&format!(
            "  token budget: {}/{} used, {} remaining\n",
            total_tokens, cap, remaining
        ));
    }
    if let Some(cap) = config.budgets.max_total_usd {
        let remaining = (cap - total_usd).max(0.0);
        out.push_str(&format!(
            "  USD budget: ${:.4}/${:.4} used, ${:.4} remaining\n",
            total_usd, cap, remaining
        ));
    }
    out
}

fn load_plan(workspace: &Path) -> Result<Plan> {
    let path = workspace.join("plan.md");
    let text = fs::read_to_string(&path)
        .with_context(|| format!("status: reading {:?}", path))?;
    plan::parse(&text).with_context(|| format!("status: parsing {:?}", path))
}

fn load_deferred(workspace: &Path) -> Result<DeferredDoc> {
    let path = workspace.join("deferred.md");
    match fs::read_to_string(&path) {
        Ok(text) => {
            if text.trim().is_empty() {
                Ok(DeferredDoc::empty())
            } else {
                deferred::parse(&text).with_context(|| format!("status: parsing {:?}", path))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeferredDoc::empty()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("status: reading {:?}", path))),
    }
}

/// Best-effort lookup of `<short hash> <subject>` for the tip of `branch`.
/// Returns `None` if the workspace isn't a git repo, the branch doesn't
/// exist, or git is otherwise unhappy. Status is informational so we
/// degrade silently rather than failing the whole command.
fn last_commit_subject(workspace: &Path, branch: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["log", "-1", "--pretty=format:%h %s", branch])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deferred::{DeferredItem, DeferredPhase};
    use crate::plan::{Phase, PhaseId};
    use crate::state::{RoleUsage, TokenUsage};
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    fn three_phase_plan() -> Plan {
        Plan::new(
            pid("02"),
            vec![
                Phase {
                    id: pid("01"),
                    title: "First".into(),
                    body: String::new(),
                },
                Phase {
                    id: pid("02"),
                    title: "Second".into(),
                    body: String::new(),
                },
                Phase {
                    id: pid("03"),
                    title: "Third".into(),
                    body: String::new(),
                },
            ],
        )
    }

    fn sample_state() -> RunState {
        let mut by_role = HashMap::new();
        by_role.insert(
            "implementer".to_string(),
            RoleUsage {
                input: 100,
                output: 50,
            },
        );
        RunState {
            run_id: "20260429T143022Z".into(),
            branch: "foreman/run-20260429T143022Z".into(),
            original_branch: Some("main".into()),
            started_at: DateTime::parse_from_rfc3339("2026-04-29T14:30:22Z")
                .unwrap()
                .with_timezone(&Utc),
            started_phase: pid("01"),
            completed: vec![pid("01")],
            attempts: HashMap::new(),
            token_usage: TokenUsage {
                input: 100,
                output: 50,
                by_role,
            },
            aborted: false,
        }
    }

    #[test]
    fn report_for_no_run_says_not_started() {
        let dir = tempdir().unwrap();
        let plan = three_phase_plan();
        let deferred = DeferredDoc::empty();
        let config = Config::default();
        let report = render_report(dir.path(), &plan, &deferred, None, &config);
        assert!(report.contains("run: not started"), "report: {report}");
        // Plan header still rendered so users see what the seed plan looks like.
        assert!(report.contains("plan: phase 02 of 3"), "report: {report}");
        // No tokens / completed / cost lines when no state.
        assert!(!report.contains("tokens"), "report: {report}");
        assert!(!report.contains("completed:"), "report: {report}");
        assert!(!report.contains("cost:"), "report: {report}");
    }

    #[test]
    fn report_for_active_run_includes_branch_completed_and_tokens() {
        let dir = tempdir().unwrap();
        let plan = three_phase_plan();
        let deferred = DeferredDoc {
            items: vec![
                DeferredItem {
                    text: "open".into(),
                    done: false,
                },
                DeferredItem {
                    text: "done".into(),
                    done: true,
                },
            ],
            phases: vec![DeferredPhase {
                source_phase: pid("01"),
                title: "rework".into(),
                body: String::new(),
            }],
        };
        let state = sample_state();
        let config = Config::default();
        let report = render_report(dir.path(), &plan, &deferred, Some(&state), &config);

        assert!(report.contains("run: 20260429T143022Z"), "report: {report}");
        assert!(
            report.contains("branch: foreman/run-20260429T143022Z"),
            "report: {report}"
        );
        assert!(report.contains("original branch: main"), "report: {report}");
        assert!(report.contains("plan: phase 02 of 3 — Second"), "report: {report}");
        assert!(report.contains("completed: 01"), "report: {report}");
        assert!(
            report.contains("deferred items: 2 (1 unchecked, 1 checked)"),
            "report: {report}"
        );
        assert!(report.contains("deferred phases: 1"), "report: {report}");
        assert!(report.contains("tokens: input=100 output=50"), "report: {report}");
        assert!(
            report.contains("implementer: input=100 output=50"),
            "report: {report}"
        );
        // 100 input + 50 output at default opus rate ($15/M in, $75/M out)
        // == 100*15/1e6 + 50*75/1e6 = 0.0015 + 0.00375 = 0.00525.
        // Rust's `{:.4}` rounds-half-to-even, so 0.00525 → 0.0052.
        assert!(report.contains("cost: $0.0052"), "report: {report}");
        // No budget caps configured → no per-cap line.
        assert!(!report.contains("token budget"), "report: {report}");
        assert!(!report.contains("USD budget"), "report: {report}");
        // No git in tempdir → last commit is "(none)".
        assert!(report.contains("last commit: (none)"), "report: {report}");
    }

    #[test]
    fn report_marks_aborted_run() {
        let dir = tempdir().unwrap();
        let plan = three_phase_plan();
        let deferred = DeferredDoc::empty();
        let mut state = sample_state();
        state.aborted = true;
        let config = Config::default();
        let report = render_report(dir.path(), &plan, &deferred, Some(&state), &config);
        assert!(report.contains("aborted"), "report: {report}");
    }

    #[test]
    fn report_with_empty_completed_says_none() {
        let dir = tempdir().unwrap();
        let plan = three_phase_plan();
        let deferred = DeferredDoc::empty();
        let mut state = sample_state();
        state.completed.clear();
        let config = Config::default();
        let report = render_report(dir.path(), &plan, &deferred, Some(&state), &config);
        assert!(report.contains("completed: (none)"), "report: {report}");
    }

    #[test]
    fn report_includes_budget_remaining_when_configured() {
        let dir = tempdir().unwrap();
        let plan = three_phase_plan();
        let deferred = DeferredDoc::empty();
        let state = sample_state();
        let mut config = Config::default();
        config.budgets.max_total_tokens = Some(10_000);
        config.budgets.max_total_usd = Some(1.00);
        let report = render_report(dir.path(), &plan, &deferred, Some(&state), &config);
        // 100 input + 50 output = 150 tokens used; cap 10000; 9850 remaining.
        assert!(
            report.contains("token budget: 150/10000 used, 9850 remaining"),
            "report: {report}"
        );
        // Cost is the same $0.0052 figure as the prior test; cap $1.0000.
        assert!(
            report.contains("USD budget: $0.0052/$1.0000 used, $0.9948 remaining"),
            "report: {report}"
        );
    }
}
