//! `foreman run` — execute the plan against the configured agent.
//!
//! Loads the workspace's `foreman.toml`, `plan.md`, `deferred.md`, and
//! `state.json`; ensures a per-run branch exists; spawns a [`broadcast`]
//! subscriber that streams [`runner::Event`]s to stderr; then drives the
//! runner until the plan completes or a phase halts.
//!
//! On a fresh run (state file is `null` or missing) this command derives a new
//! `run_id` and per-run branch from the current UTC timestamp and creates the
//! branch in git. On a continuation (state present) the existing branch is
//! checked out instead. Phase 17 layers `foreman resume` on top of the same
//! state file.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use tokio::task::JoinHandle;

use crate::agent::claude_code::ClaudeCodeAgent;
use crate::config;
use crate::deferred::{self, DeferredDoc};
use crate::git::{Git, ShellGit};
use crate::plan::{self, Plan};
use crate::runner::{self, RunSummary, Runner};
use crate::state::{self, RunState};

/// Top-level entry point for the `run` subcommand.
pub async fn run(workspace: PathBuf) -> Result<()> {
    let config = config::load(&workspace)
        .with_context(|| format!("run: loading config in {:?}", workspace))?;
    let plan = load_plan(&workspace)?;
    let deferred = load_deferred(&workspace)?;

    let existing_state = state::load(&workspace)
        .with_context(|| format!("run: loading state in {:?}", workspace))?;
    let is_fresh_run = existing_state.is_none();
    let state = match existing_state {
        Some(s) => s,
        None => runner::fresh_run_state(&plan, &config, Utc::now()),
    };

    let git = ShellGit::new(workspace.clone());
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

    let agent = ClaudeCodeAgent::new();
    let mut runner = Runner::new(workspace, config, plan, deferred, state, agent, git);

    let logger = spawn_logger(&runner);
    let summary = runner.run().await;

    drop(runner);
    let _ = logger.await;

    match summary? {
        RunSummary::Finished => Ok(()),
        RunSummary::Halted { phase_id, reason } => {
            Err(anyhow!("run halted at phase {phase_id}: {reason}"))
        }
    }
}

fn load_plan(workspace: &Path) -> Result<Plan> {
    let path = workspace.join("plan.md");
    let text = fs::read_to_string(&path).with_context(|| format!("run: reading {:?}", path))?;
    plan::parse(&text).with_context(|| format!("run: parsing {:?}", path))
}

fn load_deferred(workspace: &Path) -> Result<DeferredDoc> {
    let path = workspace.join("deferred.md");
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

#[allow(dead_code)]
fn _ensure_state_consumed(_: &RunState) {
    // Compile-time anchor for `RunState`'s presence in the public surface; the
    // CLI does not currently mutate the state outside the runner.
}
