//! `pitboss sweep` — one-shot deferred-sweep dispatch.
//!
//! Runs the same sweep pipeline `pitboss play` schedules between phases,
//! but without advancing the plan state machine. Useful after editing
//! `deferred.md` by hand, or to drain a backlog ahead of the next
//! `pitboss play`.
//!
//! Spec lives in phase 06 of `plan.md`. Behavior summary:
//!
//! - Loads workspace state when present; otherwise synthesizes a fresh
//!   in-memory state and unwinds it on the way out so the workspace
//!   isn't accidentally claimed by an empty run.
//! - `--max-items <N>` clamps the prompt's pending-items list to the
//!   first N items in document order. The on-disk file is unchanged;
//!   remaining items surface on the next sweep.
//! - `--audit` / `--no-audit` overrides `[sweep] audit_enabled` for this
//!   invocation only.
//! - `--dry-run` swaps the configured agent for the deterministic
//!   no-op agent, same as `pitboss play --dry-run`.
//! - `--after <phase-id>` overrides the prompt's `after_phase` label.
//!   Defaults to `state.completed.last()`, falling back to `None` when
//!   no run has started yet.
//!
//! Exits 0 on a successful sweep (committed or no-changes) and 1 on a
//! halt; state.json is persisted on the way out so a halt can be retried.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use clap::Parser;

use crate::agent::dry_run::{DryRunAgent, DryRunFinal};
use crate::agent::{self, Agent, AgentEvent};
use crate::cli::ExitCode;
use crate::config;
use crate::deferred::{self, DeferredDoc};
use crate::git::{Git, ShellGit};
use crate::plan::{self, PhaseId, Plan};
use crate::runner::{self, PhaseResult, Runner};
use crate::state::{self, TokenUsage};
use crate::util::paths;

/// Arguments for `pitboss sweep`.
#[derive(Debug, Parser)]
pub struct SweepArgs {
    /// Cap the prompt's pending-items list to the first N items in
    /// document order. For pathological 100+ item backlogs that would
    /// otherwise exceed the agent's effective context. The on-disk
    /// `deferred.md` is unchanged; remaining items surface on the next
    /// sweep.
    #[arg(long = "max-items")]
    pub max_items: Option<usize>,
    /// Force the post-sweep auditor pass on, overriding
    /// `[sweep] audit_enabled` for this invocation only.
    #[arg(long = "audit", conflicts_with = "no_audit")]
    pub audit: bool,
    /// Force the post-sweep auditor pass off, overriding
    /// `[sweep] audit_enabled` for this invocation only.
    #[arg(long = "no-audit")]
    pub no_audit: bool,
    /// Swap the configured agent for the deterministic no-op agent.
    /// Mirrors `pitboss play --dry-run`.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Override the `after_phase` label rendered into the sweep prompt.
    /// Defaults to `state.completed.last()`, falling back to `None` when
    /// no run has started yet.
    #[arg(long = "after")]
    pub after: Option<String>,
}

/// Top-level entry point for the `sweep` subcommand. Returns
/// [`ExitCode::Success`] on a successful sweep (committed or no-changes)
/// and [`ExitCode::Failure`] on a halt.
pub async fn run(workspace: PathBuf, args: SweepArgs) -> Result<ExitCode> {
    let config = config::load(&workspace)
        .with_context(|| format!("sweep: loading config in {:?}", workspace))?;
    if args.dry_run {
        execute_with_agent(workspace, config, args, dry_run_agent()).await
    } else {
        let agent = agent::build_agent(&config)?;
        execute_with_agent(workspace, config, args, agent).await
    }
}

async fn execute_with_agent<A: Agent + 'static>(
    workspace: PathBuf,
    mut config: config::Config,
    args: SweepArgs,
    agent: A,
) -> Result<ExitCode> {
    if args.audit {
        config.sweep.audit_enabled = true;
    } else if args.no_audit {
        config.sweep.audit_enabled = false;
    }

    let plan_obj = load_plan(&workspace)?;
    let deferred_doc = load_deferred(&workspace)?;

    let after_override = args
        .after
        .as_deref()
        .map(|s| {
            PhaseId::parse(s).map_err(|e| anyhow!("sweep: invalid --after phase id {s:?}: {e}"))
        })
        .transpose()?;

    let existing_state = state::load(&workspace)
        .with_context(|| format!("sweep: loading state in {:?}", workspace))?;
    let state_existed = existing_state.is_some();

    let after = after_override.or_else(|| {
        existing_state
            .as_ref()
            .and_then(|s| s.completed.last().cloned())
    });

    let dry_run = is_dry_run_agent(&agent);

    let state = match existing_state {
        Some(s) => {
            if s.aborted {
                anyhow::bail!(
                    "run {} was folded; remove .pitboss/play/state.json to start over",
                    s.run_id
                );
            }
            s
        }
        None => runner::fresh_run_state(&plan_obj, &config, Utc::now()),
    };

    let git = ShellGit::new(workspace.clone());
    if state_existed {
        // A real run is in flight — make sure HEAD is on the run branch
        // before the sweep commits, just like `pitboss rebuy` does.
        git.checkout(&state.branch)
            .await
            .with_context(|| format!("sweep: checking out {:?}", state.branch))?;
    }

    let mut runner = Runner::new(
        workspace.clone(),
        config,
        plan_obj,
        deferred_doc,
        state,
        agent,
        git,
    )
    .skip_tests(dry_run);

    let logger = spawn_logger(&runner);
    let outcome = runner
        .run_standalone_sweep(after, args.max_items, state_existed)
        .await;

    // Persist state.json only when a real run is in flight. The runner
    // honors `persist_state` on its success path; we mirror that on the
    // halt path so a halted sweep can be retried. When `state_existed`
    // is false the synthesized state was in-memory bookkeeping only and
    // never touches disk.
    if state_existed {
        if let Err(e) = state::save(&workspace, Some(runner.state())) {
            eprintln!("[pitboss] failed to persist state.json after sweep: {e:#}");
        }
    }

    // Drop the runner so the broadcast channel closes and the logger
    // task drains. The standalone sweep doesn't emit RunFinished, so the
    // logger only exits on channel close.
    drop(runner);
    let _ = logger.await;

    match outcome? {
        PhaseResult::Halted { phase_id, reason } => {
            eprintln!("[pitboss] sweep halted at phase {phase_id}: {reason}");
            // The shared `ExitCode` enum's `MixedFailures` slot is the
            // documented "exit 1 / operation failed" code; sweep reuses it
            // here per the enum's module-level note.
            Ok(ExitCode::MixedFailures)
        }
        PhaseResult::Advanced { .. } => Ok(ExitCode::Success),
    }
}

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

fn load_plan(workspace: &Path) -> Result<Plan> {
    let path = paths::plan_path(workspace);
    let text = fs::read_to_string(&path).with_context(|| format!("sweep: reading {:?}", path))?;
    plan::parse(&text).with_context(|| format!("sweep: parsing {:?}", path))
}

fn load_deferred(workspace: &Path) -> Result<DeferredDoc> {
    let path = paths::deferred_path(workspace);
    match fs::read_to_string(&path) {
        Ok(text) => {
            if text.trim().is_empty() {
                Ok(DeferredDoc::empty())
            } else {
                deferred::parse(&text).with_context(|| format!("sweep: parsing {:?}", path))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeferredDoc::empty()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("sweep: reading {:?}", path))),
    }
}

fn spawn_logger<A, G>(runner: &Runner<A, G>) -> tokio::task::JoinHandle<()>
where
    A: Agent + 'static,
    G: crate::git::Git + 'static,
{
    let rx = runner.subscribe();
    tokio::spawn(runner::log_events(rx))
}
