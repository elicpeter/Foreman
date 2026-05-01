//! `pitboss grind` — sequential rotating prompt runner.
//!
//! This is the user-facing front-end on top of [`crate::grind::GrindRunner`]:
//! it loads `pitboss.toml`, discovers prompts from the project / global /
//! `--prompts-dir` precedence chain, picks (or synthesizes) a plan, opens a
//! per-run directory under `.pitboss/grind/<run-id>/`, creates and checks out
//! the run branch (`pitboss/grind/<run-id>`), wires up Ctrl-C handling, and
//! drives the runner to completion.
//!
//! Phase 07 shipped the sequential MVP. Phase 08 adds the run-wide budgets
//! (`--max-iterations`, `--until`, `--max-cost`, `--max-tokens`) and the
//! documented [`crate::grind::ExitCode`] policy.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Args;
use tokio::signal;
use tracing::{info, warn};

use crate::agent::{self, Agent};
use crate::config::{self, Config};
use crate::git::{self, Git, ShellGit};
use crate::grind::{
    default_plan_from_dir, discover_prompts, generate_run_id, load_plan, reconstruct_state_from_log,
    render_dry_run_report, resolve_budgets, resolve_target, run_branch_name,
    sweep_stale_session_worktrees as pitboss_grind_sweep, validate_resume, DiscoveryOptions,
    DryRunInputs, ExitCode, GrindPlan, GrindRunOutcome, GrindRunner, GrindShutdown, GrindStopReason,
    PlanBudgets, PromptDoc, ResumeError, RunDir, RunListing, SessionRecord, SessionStatus,
};
use crate::style::{self, col};
use crate::tui;

/// `pitboss grind [options]` argument surface.
#[derive(Debug, Args)]
#[command(after_help = GRIND_AFTER_HELP)]
pub struct GrindArgs {
    /// Plan name to load. Resolves to `.pitboss/plans/<plan>.toml`. Without
    /// this flag the runner falls back to `[grind] default_plan` from
    /// `pitboss.toml`, then to a synthesized default-rotation plan over every
    /// discovered prompt.
    #[arg(long)]
    pub plan: Option<String>,
    /// Override the prompt discovery directory. Suppresses both project
    /// (`./.pitboss/prompts/`) and global (`~/.pitboss/prompts/`) sources.
    #[arg(long = "prompts-dir")]
    pub prompts_dir: Option<PathBuf>,
    /// Resolve the rotation, print a deterministic dry-run report (discovered
    /// prompts and sources, plan, budgets, hooks, parallelism cap, expected
    /// scheduler picks), then exit without dispatching any agents, creating a
    /// run directory, or touching git.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// On a successful run (exit code 0), open a pull request via
    /// `gh pr create` for the per-run branch. Title is
    /// `grind/<plan>: <run-id>`; body is the run's `sessions.md` verbatim.
    /// Mirrors `pitboss play --pr`. By default a failing PR call is logged but
    /// does not change the exit code; pair with `--require-pr` to enforce.
    #[arg(long)]
    pub pr: bool,
    /// When `--pr` is also set, treat a failed `gh pr create` call as a
    /// run-level failure: the process exits with code 6 instead of 0 even when
    /// every session resolved cleanly. Use this in CI scripts that need to
    /// distinguish "the PR shipped" from "the work shipped but the PR step
    /// failed". No effect without `--pr`.
    #[arg(long = "require-pr")]
    pub require_pr: bool,
    /// Stop after this many sessions have been dispatched. Overrides
    /// `[grind.budgets] max_iterations` from `pitboss.toml` and the plan's
    /// `PlanBudgets`.
    #[arg(long = "max-iterations", value_name = "N")]
    pub max_iterations: Option<u32>,
    /// RFC 3339 wall-clock cutoff. Once `Utc::now() >= until` the runner
    /// finishes any in-flight session and exits with code 3. Overrides
    /// `[grind.budgets] until` from `pitboss.toml` and the plan's
    /// `PlanBudgets`.
    #[arg(long = "until", value_name = "RFC3339", value_parser = parse_rfc3339)]
    pub until: Option<DateTime<Utc>>,
    /// Hard cap on cumulative agent cost in USD. Computed from
    /// `[budgets.pricing]`. Overrides the corresponding `[grind.budgets]` /
    /// plan field.
    #[arg(long = "max-cost", value_name = "USD")]
    pub max_cost: Option<f64>,
    /// Hard cap on cumulative tokens (input + output) across all roles.
    /// Overrides the corresponding `[grind.budgets]` / plan field.
    #[arg(long = "max-tokens", value_name = "N")]
    pub max_tokens: Option<u64>,
    /// Resume a previous grind run instead of starting a new one. Without an
    /// argument, picks the most-recent run whose persisted status is `Active`
    /// or `Aborted`. With an argument, resumes the named run id (the
    /// directory name under `.pitboss/grind/`). Refuses to resume when the
    /// plan or prompt set has changed in a way that would invalidate the
    /// scheduler.
    #[arg(long = "resume", value_name = "RUN_ID", num_args = 0..=1, default_missing_value = "")]
    pub resume: Option<String>,
    /// Render a live `ratatui` dashboard instead of the plain logger.
    /// Mirrors `pitboss play --tui`. The dashboard subscribes to the runner's
    /// [`crate::grind::GrindEvent`] stream and updates per session start,
    /// agent output, hook fires, summary captures, budget warnings, and
    /// scheduler picks.
    #[arg(long)]
    pub tui: bool,
}

/// Help-epilog table. Mirrors `crate::grind::ExitCode` so users can map a
/// process exit code back to a grind outcome without spelunking the source.
const GRIND_AFTER_HELP: &str = "Exit codes:
  0  Success — every dispatched session reported ok.
  1  Mixed failures — at least one session ended in error / timeout / dirty.
  2  Aborted — second Ctrl-C (or external SIGINT) cancelled the run.
  3  Budget exhausted — --max-iterations / --until / --max-cost / --max-tokens hit.
  4  Failed to start — config / discovery / git / resume pre-flight refused the run.
  5  Consecutive failures — `[grind] consecutive_failure_limit` tripped.
  6  PR creation failed — `--pr --require-pr` was set and `gh pr create` failed.";

fn parse_rfc3339(s: &str) -> std::result::Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("not a valid RFC 3339 timestamp ({e}): {s:?}"))
}

/// Entry point invoked from `cli::dispatch`. Returns an [`ExitCode`] mapping
/// to the documented `pitboss grind` exit policy. Setup errors that happen
/// before any session is dispatched surface as
/// [`ExitCode::FailedToStart`] with a stderr message.
pub async fn run(workspace: PathBuf, args: GrindArgs) -> Result<ExitCode> {
    let config = match config::load(&workspace) {
        Ok(c) => c,
        Err(e) => {
            print_failed_to_start(&format!("loading config: {e:#}"));
            return Ok(ExitCode::FailedToStart);
        }
    };
    let prompts = match load_prompts(&workspace, &config, args.prompts_dir.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            print_failed_to_start(&format!("{e:#}"));
            return Ok(ExitCode::FailedToStart);
        }
    };
    let plan = match resolve_plan(&workspace, &config, args.plan.as_deref(), &prompts) {
        Ok(p) => p,
        Err(e) => {
            print_failed_to_start(&format!("{e:#}"));
            return Ok(ExitCode::FailedToStart);
        }
    };
    if let Err(e) = plan.validate_against(&prompts) {
        print_failed_to_start(&format!("validating plan {:?}: {e:#}", plan.name));
        return Ok(ExitCode::FailedToStart);
    }

    if args.dry_run {
        return run_dry_run(&workspace, &config, &plan, &prompts, &args);
    }

    let agent = match agent::build_agent(&config) {
        Ok(a) => a,
        Err(e) => {
            print_failed_to_start(&format!("building agent: {e:#}"));
            return Ok(ExitCode::FailedToStart);
        }
    };

    if let Some(target) = args.resume.as_deref() {
        let requested = if target.is_empty() { None } else { Some(target) };
        return execute_resume(workspace, config, plan, prompts, agent, &args, requested).await;
    }

    execute(workspace, config, plan, prompts, agent, &args).await
}

/// Render and print the `--dry-run` report. When `--resume` is also set, the
/// report is seeded with the persisted scheduler / budget snapshot of the
/// resume target so the preview reflects where the resumed loop would
/// actually pick up — not a fresh rotation. Resume validation errors here
/// surface as `FailedToStart` (exit 4) the same way `execute_resume` would.
fn run_dry_run(
    workspace: &Path,
    config: &Config,
    plan: &GrindPlan,
    prompts: &[PromptDoc],
    args: &GrindArgs,
) -> Result<ExitCode> {
    let cli_budgets = PlanBudgets {
        max_iterations: args.max_iterations,
        until: args.until,
        max_tokens: args.max_tokens,
        max_cost_usd: args.max_cost,
    };
    let budgets = resolve_budgets(&config.grind.budgets, &plan.budgets, &cli_budgets);

    // Resolve the resume target up front so the dry-run preview is seeded
    // from the persisted scheduler / budget snapshot when `--resume` is set.
    let resume_payload = match args.resume.as_deref() {
        None => None,
        Some(target) => {
            let requested = if target.is_empty() { None } else { Some(target) };
            match resolve_resume_for_dry_run(workspace, plan, prompts, requested) {
                Ok(p) => Some(p),
                Err(e) => {
                    print_failed_to_start(&render_resume_error(&e));
                    return Ok(ExitCode::FailedToStart);
                }
            }
        }
    };

    let inputs = DryRunInputs {
        workspace,
        agent_backend: config.agent.backend.as_deref(),
        prompts,
        plan,
        budgets: &budgets,
        consecutive_failure_limit: config.grind.consecutive_failure_limit,
        resume_target: args.resume.as_deref(),
        resume_scheduler_state: resume_payload.as_ref().map(|p| &p.scheduler_state),
        resume_budget_consumed: resume_payload.as_ref().map(|p| &p.budget_consumed),
        resume_last_session_seq: resume_payload.as_ref().map(|p| p.last_session_seq),
    };
    let report = render_dry_run_report(&inputs);
    let stdout = std::io::stdout();
    let mut h = stdout.lock();
    h.write_all(report.as_bytes())
        .context("grind: writing dry-run report to stdout")?;
    Ok(ExitCode::Success)
}

/// Persisted snapshot a `--dry-run --resume` invocation needs to seed its
/// preview. Owned (not borrowed) so the resolver can build the lookup
/// internally and not leak the prompts BTreeMap into the caller.
struct ResumeDryRunPayload {
    scheduler_state: crate::grind::SchedulerState,
    budget_consumed: crate::grind::BudgetSnapshot,
    last_session_seq: u32,
}

fn resolve_resume_for_dry_run(
    workspace: &Path,
    plan: &GrindPlan,
    prompts: &[PromptDoc],
    requested: Option<&str>,
) -> std::result::Result<ResumeDryRunPayload, ResumeError> {
    let listing = resolve_target(workspace, requested)?;
    let current_prompt_names: Vec<String> =
        plan.prompts.iter().map(|p| p.name.clone()).collect();
    let listing = validate_resume(listing, &plan.name, &current_prompt_names)?;

    // Read the source-of-truth log so the preview reflects any sessions that
    // landed in JSONL but didn't make it into state.json before the kill.
    let run_dir = match RunDir::open(workspace, &listing.run_id) {
        Ok(d) => d,
        Err(e) => {
            return Err(ResumeError::StateUnreadable {
                run_id: listing.run_id.clone(),
                source: e,
            })
        }
    };
    let log_records = match run_dir.log().records() {
        Ok(r) => r,
        Err(e) => {
            return Err(ResumeError::StateUnreadable {
                run_id: listing.run_id.clone(),
                source: e,
            })
        }
    };
    let prompts_lookup: BTreeMap<String, PromptDoc> = prompts
        .iter()
        .map(|p| (p.meta.name.clone(), p.clone()))
        .collect();
    let reconciled =
        reconstruct_state_from_log(&listing.state, &log_records, plan, &prompts_lookup)?;
    Ok(ResumeDryRunPayload {
        scheduler_state: reconciled.scheduler_state,
        budget_consumed: reconciled.budget_consumed,
        last_session_seq: reconciled.last_session_seq,
    })
}

async fn execute<A>(
    workspace: PathBuf,
    config: Config,
    plan: GrindPlan,
    prompts: Vec<PromptDoc>,
    agent: A,
    args: &GrindArgs,
) -> Result<ExitCode>
where
    A: Agent + 'static,
{
    let run_id = generate_run_id();
    let branch = run_branch_name(&run_id);

    let git = ShellGit::new(workspace.clone());
    if let Err(e) = git.create_branch(&branch).await {
        print_failed_to_start(&format!(
            "creating run branch {:?} (workspace must be a git repo): {e:#}",
            branch
        ));
        return Ok(ExitCode::FailedToStart);
    }
    if let Err(e) = git.checkout(&branch).await {
        print_failed_to_start(&format!("checking out {:?}: {e:#}", branch));
        return Ok(ExitCode::FailedToStart);
    }

    let run_dir = match RunDir::create(&workspace, &run_id) {
        Ok(d) => d,
        Err(e) => {
            print_failed_to_start(&format!("creating run directory for {run_id}: {e:#}"));
            return Ok(ExitCode::FailedToStart);
        }
    };

    // Layer the budget sources so a CLI flag wins over `[grind.budgets]`
    // which wins over the plan's `PlanBudgets`. Order docs in
    // `crate::grind::resolve_budgets`.
    let cli_budgets = PlanBudgets {
        max_iterations: args.max_iterations,
        until: args.until,
        max_tokens: args.max_tokens,
        max_cost_usd: args.max_cost,
    };
    let budgets = resolve_budgets(&config.grind.budgets, &plan.budgets, &cli_budgets);
    let consecutive_failure_limit = config.grind.consecutive_failure_limit;

    let prompts_map = into_lookup(prompts);
    let runner_git = ShellGit::new(workspace.clone());
    let plan_name = plan.name.clone();
    let mut runner = GrindRunner::new(
        workspace.clone(),
        config,
        run_id.clone(),
        branch.clone(),
        plan,
        prompts_map,
        run_dir,
        agent,
        runner_git,
        budgets,
        consecutive_failure_limit,
    );

    let shutdown = GrindShutdown::new();
    let signal_task = spawn_signal_handler(shutdown.clone());

    announce_start(&run_id, &branch);

    let outcome = drive_runner(&mut runner, args.tui, shutdown.clone()).await?;

    signal_task.abort();
    let _ = signal_task.await;

    let Some(outcome) = outcome else {
        // User quit the TUI before the runner produced an outcome. The TUI
        // tripped the drain signal on its way out; treat this as an aborted
        // session-less run for exit-code purposes.
        return Ok(ExitCode::Aborted);
    };
    announce_finish(
        &outcome.run_id,
        &outcome.branch,
        &outcome.stop_reason,
        outcome.sessions.len(),
    );

    let exit = classify_outcome(&outcome.stop_reason, &outcome.sessions);
    let exit = maybe_open_pr(&workspace, &outcome.run_id, &plan_name, args, exit).await;
    Ok(exit)
}

/// Drive a [`GrindRunner`] either with the plain logger (default) or the
/// `ratatui` dashboard (when `--tui` is set). Returns `None` only when the
/// TUI exited before the runner produced an outcome (user pressed
/// `q`/`a`/Ctrl-C); the logger path always returns `Some`.
async fn drive_runner<A>(
    runner: &mut GrindRunner<A, crate::git::ShellGit>,
    tui_flag: bool,
    shutdown: GrindShutdown,
) -> Result<Option<GrindRunOutcome>>
where
    A: crate::agent::Agent + Send + Sync + 'static,
{
    if tui_flag {
        Ok(tui::grind::run(runner, shutdown).await?)
    } else {
        Ok(Some(runner.run(shutdown).await?))
    }
}

async fn execute_resume<A>(
    workspace: PathBuf,
    config: Config,
    plan: GrindPlan,
    prompts: Vec<PromptDoc>,
    agent: A,
    args: &GrindArgs,
    requested: Option<&str>,
) -> Result<ExitCode>
where
    A: Agent + 'static,
{
    let listing = match resolve_target(&workspace, requested) {
        Ok(l) => l,
        Err(e) => {
            print_failed_to_start(&render_resume_error(&e));
            return Ok(ExitCode::FailedToStart);
        }
    };

    let current_prompt_names: Vec<String> = plan.prompts.iter().map(|p| p.name.clone()).collect();
    let listing = match validate_resume(listing, &plan.name, &current_prompt_names) {
        Ok(l) => l,
        Err(e) => {
            print_failed_to_start(&render_resume_error(&e));
            return Ok(ExitCode::FailedToStart);
        }
    };

    // Spec: "After-resume sanity: re-checkout the run branch; if working tree
    // is dirty, halts with exit code 4." We check `is_clean` *before*
    // checkout so a dirty tree we'd otherwise carry into the resumed branch
    // is surfaced as the failure point.
    let git = ShellGit::new(workspace.clone());
    match git.is_clean().await {
        Ok(true) => {}
        Ok(false) => {
            print_failed_to_start(&format!(
                "resume {:?}: working tree is dirty; commit or stash changes before resuming",
                listing.run_id
            ));
            return Ok(ExitCode::FailedToStart);
        }
        Err(e) => {
            print_failed_to_start(&format!(
                "resume {:?}: checking working tree: {e:#}",
                listing.run_id
            ));
            return Ok(ExitCode::FailedToStart);
        }
    }
    if let Err(e) = git.checkout(&listing.state.branch).await {
        print_failed_to_start(&format!(
            "resume {:?}: checking out {:?}: {e:#}",
            listing.run_id, listing.state.branch
        ));
        return Ok(ExitCode::FailedToStart);
    }

    let run_dir = match RunDir::open(&workspace, &listing.run_id) {
        Ok(d) => d,
        Err(e) => {
            print_failed_to_start(&format!(
                "resume {:?}: opening run directory: {e:#}",
                listing.run_id
            ));
            return Ok(ExitCode::FailedToStart);
        }
    };

    // Reconcile the cached state.json against the source-of-truth
    // sessions.jsonl. If the host died between a JSONL append and the
    // matching state.json write, replay the missing records through the
    // scheduler so a single dropped write doesn't strand the run. The
    // reverse mismatch (state claims more sessions than the log has) is
    // genuinely broken and still refuses.
    let log_records = match run_dir.log().records() {
        Ok(r) => r,
        Err(e) => {
            print_failed_to_start(&format!(
                "resume {:?}: reading sessions.jsonl: {e:#}",
                listing.run_id
            ));
            return Ok(ExitCode::FailedToStart);
        }
    };
    let prompts_map = into_lookup(prompts);
    let reconciled = match reconstruct_state_from_log(
        &listing.state,
        &log_records,
        &plan,
        &prompts_map,
    ) {
        Ok(r) => r,
        Err(e) => {
            print_failed_to_start(&render_resume_error(&e));
            return Ok(ExitCode::FailedToStart);
        }
    };
    if reconciled.records_replayed > 0 {
        info!(
            run_id = %listing.run_id,
            replayed = reconciled.records_replayed,
            "grind: replayed missing JSONL records past state.json snapshot"
        );
    }

    // Sweep parallel-session worktrees the original run left behind. If
    // pitboss died mid-flight the directories at
    // `worktrees/session-NNNN/` and the matching ephemeral branches stay
    // on disk; an untouched resume that picks the same seq numbers (it
    // shouldn't — `next_seq = last_session_seq + 1`) would otherwise
    // collide on the path / branch. Even when there is no collision the
    // stale tree just balloons the run dir forever. Sweep happens after
    // the dirty-tree pre-flight so we never wipe a tree the user is
    // actively triaging from.
    let sweep_git = ShellGit::new(workspace.clone());
    let removed = pitboss_grind_sweep(
        &sweep_git,
        run_dir.paths(),
        &listing.run_id,
        reconciled.last_session_seq,
    )
    .await;
    if removed > 0 {
        info!(
            run_id = %listing.run_id,
            removed,
            "grind: resume swept stale worktrees"
        );
    }

    let cli_budgets = PlanBudgets {
        max_iterations: args.max_iterations,
        until: args.until,
        max_tokens: args.max_tokens,
        max_cost_usd: args.max_cost,
    };
    let budgets = resolve_budgets(&config.grind.budgets, &plan.budgets, &cli_budgets);
    let consecutive_failure_limit = config.grind.consecutive_failure_limit;

    let runner_git = ShellGit::new(workspace.clone());
    let plan_name = plan.name.clone();
    let RunListing {
        run_id,
        state_path: _,
        state,
    } = listing;
    let mut runner = GrindRunner::resume(
        workspace.clone(),
        config,
        run_id.clone(),
        state.branch.clone(),
        plan,
        prompts_map,
        run_dir,
        agent,
        runner_git,
        budgets,
        consecutive_failure_limit,
        reconciled.scheduler_state,
        reconciled.budget_consumed,
        reconciled.last_session_seq,
        state.started_at,
    );

    let shutdown = GrindShutdown::new();
    let signal_task = spawn_signal_handler(shutdown.clone());

    announce_resume(&run_id, &state.branch, reconciled.last_session_seq);

    let outcome = drive_runner(&mut runner, args.tui, shutdown.clone()).await?;

    signal_task.abort();
    let _ = signal_task.await;

    let Some(outcome) = outcome else {
        return Ok(ExitCode::Aborted);
    };
    announce_finish(
        &outcome.run_id,
        &outcome.branch,
        &outcome.stop_reason,
        outcome.sessions.len(),
    );

    let exit = classify_outcome(&outcome.stop_reason, &outcome.sessions);
    let exit = maybe_open_pr(&workspace, &outcome.run_id, &plan_name, args, exit).await;
    Ok(exit)
}

/// Open the post-run PR if requested. Both the fresh-run and resume paths
/// share this logic; centralizing it keeps the `--require-pr` policy in one
/// place. The policy itself lives in [`pr_failure_exit_code`] so tests can
/// pin it without spawning a runner.
async fn maybe_open_pr(
    workspace: &Path,
    run_id: &str,
    plan_name: &str,
    args: &GrindArgs,
    exit: ExitCode,
) -> ExitCode {
    if !args.pr || exit != ExitCode::Success {
        return exit;
    }
    let pr_git = ShellGit::new(workspace.to_path_buf());
    let pr_succeeded = match open_post_run_grind_pr(&pr_git, workspace, run_id, plan_name).await {
        Ok(url) => {
            announce_pr_opened(&url);
            true
        }
        Err(e) => {
            announce_pr_failed(&e);
            false
        }
    };
    pr_failure_exit_code(exit, args.require_pr, pr_succeeded)
}

/// Policy for `--require-pr`: when the underlying run succeeded and the user
/// asked for a strict PR step, a failed `gh pr create` upgrades the exit code
/// to [`ExitCode::PrCreationFailed`]. Without `--require-pr`, a PR failure is
/// logged but the exit code is left untouched (the historical behavior that
/// mirrors `pitboss play --pr`). When the run did not succeed, the original
/// exit code is preserved — the underlying failure outranks any PR-step
/// outcome. Public for the integration test crate.
pub fn pr_failure_exit_code(prior: ExitCode, require_pr: bool, pr_succeeded: bool) -> ExitCode {
    if pr_succeeded || !require_pr || prior != ExitCode::Success {
        prior
    } else {
        ExitCode::PrCreationFailed
    }
}

fn render_resume_error(e: &ResumeError) -> String {
    format!("resume: {e}")
}

/// Map a grind run's [`GrindStopReason`] plus its session list to the
/// documented [`ExitCode`]. Public for the integration test crate.
pub fn classify_outcome(stop_reason: &GrindStopReason, sessions: &[SessionRecord]) -> ExitCode {
    match stop_reason {
        GrindStopReason::Aborted => ExitCode::Aborted,
        GrindStopReason::BudgetExhausted(_) => ExitCode::BudgetExhausted,
        GrindStopReason::ConsecutiveFailureLimit { .. } => ExitCode::ConsecutiveFailures,
        GrindStopReason::Completed | GrindStopReason::Drained => {
            if sessions
                .iter()
                .any(|r| matches!(r.status, SessionStatus::Error | SessionStatus::Timeout))
            {
                ExitCode::MixedFailures
            } else {
                ExitCode::Success
            }
        }
    }
}

fn load_prompts(
    workspace: &Path,
    config: &Config,
    flag_override: Option<&Path>,
) -> Result<Vec<PromptDoc>> {
    let override_dir = flag_override
        .map(|p| p.to_path_buf())
        .or_else(|| config.grind.prompts_dir.clone());
    let opts = DiscoveryOptions {
        project_root: workspace.to_path_buf(),
        home_dir: std::env::var_os("HOME").map(PathBuf::from),
        override_dir,
    };
    let discovered = discover_prompts(opts);
    if !discovered.errors.is_empty() {
        let stderr = std::io::stderr();
        let c = style::use_color_stderr();
        let mut h = stderr.lock();
        for (path, error) in &discovered.errors {
            let _ = writeln!(
                h,
                "{} {}: {}",
                col(c, style::BOLD_RED, "warning:"),
                path.display(),
                error
            );
        }
    }
    if discovered.prompts.is_empty() {
        bail!("grind: no prompts discovered (run `pitboss prompts new <name>` to create one)");
    }
    Ok(discovered.prompts)
}

fn resolve_plan(
    workspace: &Path,
    config: &Config,
    cli_plan: Option<&str>,
    prompts: &[PromptDoc],
) -> Result<GrindPlan> {
    let plan_name = cli_plan.or(config.grind.default_plan.as_deref());
    let Some(name) = plan_name else {
        return Ok(default_plan_from_dir(prompts));
    };
    let path = workspace
        .join(".pitboss")
        .join("plans")
        .join(format!("{name}.toml"));
    load_plan(&path).with_context(|| format!("grind: loading plan {:?}", path))
}

fn into_lookup(prompts: Vec<PromptDoc>) -> BTreeMap<String, PromptDoc> {
    let mut out = BTreeMap::new();
    for p in prompts {
        out.insert(p.meta.name.clone(), p);
    }
    out
}

/// Wire `Ctrl-C` to the runner's two-stage shutdown. First signal sets drain
/// (the in-flight session finishes, then the loop exits). Second signal aborts
/// (the agent's cancel token fires).
fn spawn_signal_handler(shutdown: GrindShutdown) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if signal::ctrl_c().await.is_err() {
            warn!("grind: failed to install Ctrl-C handler; ignoring signal-driven drain");
            return;
        }
        announce_drain();
        shutdown.drain();

        if signal::ctrl_c().await.is_err() {
            return;
        }
        announce_abort();
        shutdown.abort();
    })
}

fn announce_start(run_id: &str, branch: &str) {
    let c = style::use_color_stderr();
    eprintln!(
        "{} grind run {} on branch {}",
        col(c, style::BOLD_CYAN, "[pitboss]"),
        col(c, style::BOLD_WHITE, run_id),
        col(c, style::CYAN, branch),
    );
    info!(run_id, branch, "grind: run started");
}

fn announce_resume(run_id: &str, branch: &str, last_session_seq: u32) {
    let c = style::use_color_stderr();
    eprintln!(
        "{} resuming grind run {} on branch {} (next session-{:04})",
        col(c, style::BOLD_CYAN, "[pitboss]"),
        col(c, style::BOLD_WHITE, run_id),
        col(c, style::CYAN, branch),
        last_session_seq.saturating_add(1),
    );
    info!(
        run_id,
        branch,
        last_session_seq,
        "grind: run resumed"
    );
}

fn announce_drain() {
    let c = style::use_color_stderr();
    eprintln!(
        "{} draining: finishing the current session, press Ctrl-C again to abort",
        col(c, style::BOLD_YELLOW, "[pitboss]"),
    );
}

fn announce_abort() {
    let c = style::use_color_stderr();
    eprintln!(
        "{} aborting: cancelling the current agent",
        col(c, style::BOLD_RED, "[pitboss]"),
    );
}

fn announce_finish(run_id: &str, branch: &str, reason: &GrindStopReason, sessions: usize) {
    let c = style::use_color_stderr();
    let (label, suffix) = match reason {
        GrindStopReason::Completed => (col(c, style::BOLD_GREEN, "completed"), String::new()),
        GrindStopReason::Drained => (col(c, style::BOLD_YELLOW, "drained"), String::new()),
        GrindStopReason::Aborted => (col(c, style::BOLD_RED, "aborted"), String::new()),
        GrindStopReason::BudgetExhausted(reason) => (
            col(c, style::BOLD_YELLOW, "BudgetExhausted"),
            format!(" ({reason})"),
        ),
        GrindStopReason::ConsecutiveFailureLimit { limit } => (
            col(c, style::BOLD_RED, "consecutive-failure-limit"),
            format!(" (limit={limit})"),
        ),
    };
    eprintln!(
        "{} grind run {} {}{} after {} session(s) on {}",
        col(c, style::BOLD_CYAN, "[pitboss]"),
        col(c, style::BOLD_WHITE, run_id),
        label,
        suffix,
        sessions,
        col(c, style::CYAN, branch),
    );
    // The spec requires a final `BudgetExhausted` log line so log scrapers
    // and supervising scripts have a stable string to match on.
    if let GrindStopReason::BudgetExhausted(reason) = reason {
        info!(run_id, %reason, "BudgetExhausted");
    }
}

fn print_failed_to_start(message: &str) {
    let c = style::use_color_stderr();
    eprintln!(
        "{} grind: {} (exit 4)",
        col(c, style::BOLD_RED, "[pitboss]"),
        message,
    );
}

/// Open a pull request for a finished grind run via [`git::open_grind_pr`].
/// Public so the integration test crate can exercise it against a `MockGit`.
/// On failure the error is reported but does not change the run's exit code —
/// the grind run already succeeded; PR creation is the post-step.
pub async fn open_post_run_grind_pr<G: Git + ?Sized>(
    git: &G,
    workspace: &Path,
    run_id: &str,
    plan_name: &str,
) -> Result<String> {
    let sessions_md_path = workspace
        .join(".pitboss")
        .join("grind")
        .join(run_id)
        .join("sessions.md");
    let sessions_md = fs::read_to_string(&sessions_md_path).with_context(|| {
        format!(
            "grind --pr: reading {} for PR body",
            sessions_md_path.display()
        )
    })?;
    git::open_grind_pr(git, plan_name, run_id, &sessions_md).await
}

fn announce_pr_opened(url: &str) {
    let c = style::use_color_stdout();
    let stdout = std::io::stdout();
    let mut h = stdout.lock();
    let _ = writeln!(
        h,
        "{} opened PR: {}",
        col(c, style::BOLD_CYAN, "[pitboss]"),
        col(c, style::CYAN, url)
    );
}

fn announce_pr_failed(err: &anyhow::Error) {
    let c = style::use_color_stderr();
    eprintln!(
        "{} PR creation failed: {err:#}",
        col(c, style::BOLD_RED, "[pitboss]"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_failure_without_require_keeps_prior_exit() {
        // The historical behavior — failure is logged, exit code is whatever
        // the run resolved to.
        assert_eq!(
            pr_failure_exit_code(ExitCode::Success, false, false),
            ExitCode::Success
        );
    }

    #[test]
    fn pr_success_keeps_prior_exit_regardless_of_require_pr() {
        assert_eq!(
            pr_failure_exit_code(ExitCode::Success, false, true),
            ExitCode::Success
        );
        assert_eq!(
            pr_failure_exit_code(ExitCode::Success, true, true),
            ExitCode::Success
        );
    }

    #[test]
    fn require_pr_with_failed_call_upgrades_to_pr_creation_failed() {
        assert_eq!(
            pr_failure_exit_code(ExitCode::Success, true, false),
            ExitCode::PrCreationFailed
        );
    }

    #[test]
    fn require_pr_does_not_overwrite_a_non_success_prior() {
        // The underlying failure outranks the PR step. If the run already
        // failed, the original exit code wins so a CI script can still see the
        // root cause instead of the post-step artifact.
        assert_eq!(
            pr_failure_exit_code(ExitCode::MixedFailures, true, false),
            ExitCode::MixedFailures
        );
        assert_eq!(
            pr_failure_exit_code(ExitCode::Aborted, true, false),
            ExitCode::Aborted
        );
    }
}
