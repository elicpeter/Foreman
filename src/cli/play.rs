//! `pitboss play` — execute the plan against the configured agent.
//!
//! Loads the workspace's `.pitboss/config.toml`, `.pitboss/play/plan.md`,
//! `.pitboss/play/deferred.md`, and `.pitboss/play/state.json`; ensures a
//! per-run branch exists; spawns a [`tokio::sync::broadcast`] subscriber that
//! streams [`runner::Event`]s to stderr; then drives the runner until the plan
//! completes or a phase halts.
//!
//! On a fresh run (state file is `null` or missing) this command derives a new
//! `run_id` and per-run branch from the current UTC timestamp, captures the
//! current branch as `original_branch` for `pitboss fold --checkout-original`,
//! and creates the branch in git. On a continuation (state present) the
//! existing branch is checked out instead. Phase 17's `pitboss rebuy` reuses
//! [`execute`] with [`StartMode::Resume`] to require an existing state file.
//!
//! Folded runs (`state.aborted == true`) are refused, the user must clear
//! `.pitboss/play/state.json` to start a new run.
//!
//! `pitboss run` is kept as a clap alias of `pitboss play`, so existing
//! scripts and muscle memory continue to work unchanged.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use tokio::task::JoinHandle;

use crate::agent::dry_run::{DryRunAgent, DryRunFinal};
use crate::agent::{self, Agent, AgentEvent};
use crate::config;
use crate::deferred::{self, DeferredDoc};
use crate::git::{self, Git, PrSummary, ShellGit};
use crate::plan::{self, Plan};
use crate::runner::{self, RunSummary, Runner};
use crate::state::{self, TokenUsage};
use crate::tui;
use crate::util::paths;

/// Whether [`execute`] is allowed to start a fresh run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    /// `pitboss play`: a missing or `null` state file kicks off a fresh run.
    Fresh,
    /// `pitboss rebuy`: a missing state file is an error.
    Resume,
}

/// Top-level entry point for the `play` subcommand.
///
/// `tui` toggles between the plain stderr logger (default) and the
/// `ratatui` dashboard. `pr` opts into the post-run pull-request creation
/// step described in [`execute`]; either it or `git.create_pr = true` in
/// `.pitboss/config.toml` enables the step. `dry_run` swaps the configured
/// agent for the deterministic [`DryRunAgent`] so the run can be exercised
/// end-to-end without any model spend. `no_sweep` and `force_sweep` map to
/// the matching runner overrides — the clap layer enforces mutual
/// exclusion so at most one is `true`.
pub async fn run(
    workspace: PathBuf,
    tui: bool,
    pr: bool,
    dry_run: bool,
    no_sweep: bool,
    force_sweep: bool,
) -> Result<()> {
    execute(
        workspace,
        tui,
        pr,
        dry_run,
        no_sweep,
        force_sweep,
        StartMode::Fresh,
    )
    .await
}

/// Shared runner driver used by both `pitboss play` and `pitboss rebuy`.
///
/// `mode` selects fresh-start vs. resume semantics. The function loads config,
/// the plan, and the deferred doc; reconciles state with `mode`; ensures the
/// per-run branch is checked out; spawns the configured event subscriber
/// (logger or TUI); then drives [`Runner::run`] to completion or halt.
///
/// When the run finishes (no halt) and either `pr_flag` is set or
/// `git.create_pr = true` in `config.toml`, the function shells out to
/// `gh pr create` via [`Git::open_pr`] using a title and body generated from
/// the completed phases plus any remaining deferred work. PR creation
/// failures are reported but do not change the function's exit status — the
/// underlying run already succeeded.
///
/// When `dry_run` is `true` the configured backend is swapped for a
/// scripted [`DryRunAgent`] that emits a single stdout marker and returns
/// success with zero tokens. The runner is also told to
/// [`Runner::skip_tests`], because the no-op agent never modifies the
/// working tree and a flaky test suite would otherwise halt the dry-run
/// after one phase. Per-phase commits are still attempted; they no-op
/// because nothing was staged. The post-run PR step is suppressed in
/// dry-run mode regardless of `pr_flag` / `git.create_pr` — opening a PR
/// for a no-op branch would be a footgun.
pub async fn execute(
    workspace: PathBuf,
    tui: bool,
    pr_flag: bool,
    dry_run: bool,
    no_sweep: bool,
    force_sweep: bool,
    mode: StartMode,
) -> Result<()> {
    let config = config::load(&workspace)
        .with_context(|| format!("run: loading config in {:?}", workspace))?;
    if dry_run {
        execute_with_agent(
            workspace,
            config,
            tui,
            false,
            no_sweep,
            force_sweep,
            mode,
            dry_run_agent(),
        )
        .await
    } else {
        let agent = agent::build_agent(&config)?;
        execute_with_agent(
            workspace,
            config,
            tui,
            pr_flag,
            no_sweep,
            force_sweep,
            mode,
            agent,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_with_agent<A: Agent + 'static>(
    workspace: PathBuf,
    config: config::Config,
    tui: bool,
    pr_flag: bool,
    no_sweep: bool,
    force_sweep: bool,
    mode: StartMode,
    agent: A,
) -> Result<()> {
    let dry_run = is_dry_run_agent(&agent);

    let plan = load_plan(&workspace)?;
    let deferred = load_deferred(&workspace)?;

    let existing_state = state::load(&workspace)
        .with_context(|| format!("run: loading state in {:?}", workspace))?;

    let git = ShellGit::new(workspace.clone());

    let (state, is_fresh_run) = match (existing_state, mode) {
        (Some(s), _) => {
            if s.aborted {
                bail!(
                    "run {} was folded; remove .pitboss/play/state.json to start over",
                    s.run_id
                );
            }
            (s, false)
        }
        (None, StartMode::Fresh) => {
            let original_branch = git.current_branch().await.ok();
            let mut s = runner::fresh_run_state(&plan, &config, Utc::now());
            s.original_branch = original_branch;
            (s, true)
        }
        (None, StartMode::Resume) => {
            bail!(
                "no run to rebuy: .pitboss/play/state.json is empty; use `pitboss play` to start a fresh run"
            );
        }
    };

    if is_fresh_run {
        git.create_branch(&state.branch).await.with_context(|| {
            format!(
                "run: creating per-run branch {:?} (workspace must already be a git repo)",
                state.branch
            )
        })?;
    }
    git.checkout(&state.branch)
        .await
        .with_context(|| format!("run: checking out {:?}", state.branch))?;
    state::save(&workspace, Some(&state))
        .with_context(|| format!("run: persisting initial state in {:?}", workspace))?;

    let want_pr = pr_flag || config.git.create_pr;

    let mut runner = Runner::new(workspace, config, plan, deferred, state, agent, git)
        .skip_tests(dry_run)
        .skip_sweep(no_sweep)
        .force_sweep(force_sweep);

    let summary = if tui {
        tui::run(&mut runner).await?
    } else {
        let logger = spawn_logger(&runner);
        let result = runner.run().await;
        let _ = logger.await;
        Some(result?)
    };

    match summary {
        None => Ok(()),
        Some(RunSummary::Finished) => {
            if want_pr {
                use crate::style::{self, col};
                match open_post_run_pr(&runner).await {
                    Ok(url) => {
                        let c = style::use_color_stdout();
                        let stdout = std::io::stdout();
                        let mut h = stdout.lock();
                        let _ = writeln!(
                            h,
                            "{} opened PR: {}",
                            col(c, style::BOLD_CYAN, "[pitboss]"),
                            col(c, style::CYAN, &url)
                        );
                    }
                    Err(e) => {
                        let c = style::use_color_stderr();
                        eprintln!(
                            "{} PR creation failed: {e:#}",
                            col(c, style::BOLD_RED, "[pitboss]")
                        );
                    }
                }
            }
            Ok(())
        }
        Some(RunSummary::Halted { phase_id, reason }) => {
            Err(anyhow!("run halted at phase {phase_id}: {reason}"))
        }
    }
}

/// Identifier the dry-run agent advertises via [`Agent::name`]. Used by the
/// CLI layer to detect "is this a dry-run run?" without threading a separate
/// boolean through every helper.
const DRY_RUN_AGENT_NAME: &str = "pitboss-dry-run";

fn dry_run_agent() -> DryRunAgent {
    DryRunAgent::new(DRY_RUN_AGENT_NAME)
        .emit(AgentEvent::Stdout(
            "[dry-run] no-op agent dispatched; making no edits".to_string(),
        ))
        .finish(DryRunFinal::Success {
            exit_code: 0,
            tokens: TokenUsage::default(),
        })
}

fn is_dry_run_agent<A: Agent>(agent: &A) -> bool {
    agent.name() == DRY_RUN_AGENT_NAME
}

/// Build a [`PrSummary`] from the just-finished runner and shell out to
/// [`Git::open_pr`]. Returns the URL `gh pr create` printed on success.
/// Lives here rather than in the runner because PR creation is a CLI-layer
/// concern — the runner is plan-agnostic and never talks to GitHub.
pub async fn open_post_run_pr<A, G>(runner: &Runner<A, G>) -> Result<String>
where
    A: crate::agent::Agent,
    G: Git,
{
    let summary = PrSummary {
        plan: runner.plan(),
        state: runner.state(),
        deferred: runner.deferred(),
    };
    let title = git::pr_title(&summary);
    let body = git::pr_body(&summary);
    let url = runner
        .git_handle()
        .open_pr(&title, &body)
        .await
        .context("opening PR via gh pr create")?;
    Ok(url)
}

fn load_plan(workspace: &Path) -> Result<Plan> {
    let path = paths::plan_path(workspace);
    let text = fs::read_to_string(&path).with_context(|| format!("run: reading {:?}", path))?;
    plan::parse(&text).with_context(|| format!("run: parsing {:?}", path))
}

fn load_deferred(workspace: &Path) -> Result<DeferredDoc> {
    let path = paths::deferred_path(workspace);
    match fs::read_to_string(&path) {
        Ok(text) => {
            if text.trim().is_empty() {
                Ok(DeferredDoc::empty())
            } else {
                deferred::parse(&text).with_context(|| format!("run: parsing {:?}", path))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeferredDoc::empty()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("run: reading {:?}", path))),
    }
}

fn spawn_logger<A, G>(runner: &Runner<A, G>) -> JoinHandle<()>
where
    A: crate::agent::Agent + 'static,
    G: Git + 'static,
{
    let rx = runner.subscribe();
    tokio::spawn(runner::log_events(rx))
}
