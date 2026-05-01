//! Orchestration loop and event channel.
//!
//! The runner owns the per-phase state machine. It snapshots the planning
//! artifacts, dispatches the implementer agent, validates the agent's output,
//! runs the project tests, and lands a per-phase commit. Every observable
//! transition is broadcast on a [`tokio::sync::broadcast`] channel so the CLI
//! logger and the (later) TUI can subscribe without changing the runner.
//!
//! Phase 12 wired the implementer-only flow: agent → validate → tests → commit.
//! Phase 13 layers a bounded fixer loop on top: when the project tests fail,
//! the runner dispatches the fixer agent up to
//! [`crate::config::RetryBudgets::fixer_max_attempts`] times, re-running tests
//! after each attempt, before halting. Phase 14 inserts the auditor pass
//! between the (passing) test run and the per-phase commit: when
//! [`crate::config::AuditConfig::enabled`] is on the runner stages the
//! implementer's diff, hands it to the auditor agent, re-validates the
//! planning artifacts, and re-runs the tests before letting the commit land.

pub mod sweep;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::agent::{Agent, AgentEvent, AgentRequest, Role, StopReason};
use crate::config::Config;
use crate::deferred::{self, DeferredDoc};
use crate::git::{self, CommitId, Git};
use crate::plan::{self, PhaseId, Plan, Snapshot};
use crate::prompts;
use crate::state::{self, RunState, TokenUsage};
use crate::tests as project_tests;
use crate::util::{paths, write_atomic};

/// Default agent wall-clock cap. Conservative so a stuck agent does not strand
/// a run for an unbounded time; phase 18 makes this configurable.
const DEFAULT_AGENT_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Capacity of the broadcast channel events fan out on. Enough that a slow
/// subscriber falls behind by hundreds of events before lagging; sends are
/// best-effort so a slow subscriber never blocks the runner.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Per-dispatch capacity of the mpsc channel between the agent and the
/// runner's forwarder task. Bounded to apply backpressure on a misbehaving
/// agent that floods events.
const AGENT_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Cap on the number of stale `## Deferred items` entries fed into the sweep
/// and sweep-auditor prompts. Past this, the prompt would balloon without
/// adding value: the agent can only meaningfully address a handful of
/// "high-stakes" items per dispatch, and a runaway map (hundreds of stuck
/// items) almost always means the staleness signal needs operator attention,
/// not more agent passes.
pub const STALE_ITEMS_PROMPT_CAP: usize = 10;

/// Why the runner stopped advancing the plan.
///
/// Each variant carries enough context for the CLI logger (and the eventual
/// TUI) to render a useful single-line message without needing to re-read
/// log files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaltReason {
    /// The agent modified `plan.md`. The runner restored from the pre-agent
    /// snapshot before halting.
    PlanTampered,
    /// The agent left `deferred.md` in an unparsable state. The runner
    /// restored from the pre-agent snapshot before halting. The string is
    /// the parser's diagnostic.
    DeferredInvalid(String),
    /// The project's test suite failed. Holds the short summary captured by
    /// [`crate::tests::TestRunner::run`].
    TestsFailed(String),
    /// The agent exited via timeout, cancellation, or an internal error.
    AgentFailure(String),
    /// A configured budget was exhausted before the next agent dispatch
    /// could fire. Carries a human-readable summary (`token budget exceeded:
    /// 105000 >= cap 100000`, `USD budget exceeded: $5.0234 >= cap $5.0000`).
    BudgetExceeded(String),
}

impl std::fmt::Display for HaltReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HaltReason::PlanTampered => f.write_str("plan.md was modified by the agent"),
            HaltReason::DeferredInvalid(msg) => write!(f, "deferred.md is invalid: {msg}"),
            HaltReason::TestsFailed(summary) => write!(f, "tests failed: {summary}"),
            HaltReason::AgentFailure(msg) => write!(f, "agent failure: {msg}"),
            HaltReason::BudgetExceeded(msg) => write!(f, "budget exceeded: {msg}"),
        }
    }
}

/// Compute `(total_tokens, total_usd)` for a [`TokenUsage`] under the supplied
/// [`Config`].
///
/// `total_tokens` is the simple `input + output` sum from the top-level
/// counter — that's the figure compared against
/// [`crate::config::Budgets::max_total_tokens`].
///
/// `total_usd` walks each role in [`crate::config::ModelRoles`], looks up the
/// per-role tokens in `usage.by_role`, and prices them against
/// [`crate::config::Budgets::pricing`] keyed by the role's configured model
/// id. Roles whose model is missing from the pricing table contribute zero —
/// the token-count budget still applies. Agent-supplied `by_role` keys that
/// don't match a configured role are ignored here, since we have no model
/// assignment to price them under.
pub fn budget_totals(config: &Config, usage: &TokenUsage) -> (u64, f64) {
    let total_tokens = usage.input.saturating_add(usage.output);
    let mut total_usd = 0.0;
    let role_models: [(&str, &str); 4] = [
        ("planner", config.models.planner.as_str()),
        ("implementer", config.models.implementer.as_str()),
        ("auditor", config.models.auditor.as_str()),
        ("fixer", config.models.fixer.as_str()),
    ];
    for (role_key, model) in role_models {
        let Some(role_usage) = usage.by_role.get(role_key) else {
            continue;
        };
        let Some(price) = config.budgets.pricing.get(model) else {
            continue;
        };
        total_usd += price.cost_usd(role_usage.input, role_usage.output);
    }
    (total_tokens, total_usd)
}

/// Discriminator on [`AuditContext`]. Tells subscribers whether the audit
/// firing belongs to a regular plan phase or a deferred sweep so the TUI and
/// loggers can render the right header text without inspecting state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditContextKind {
    /// Audit pass for a regular plan-phase implementer dispatch.
    Phase,
    /// Audit pass for a deferred-sweep dispatch.
    Sweep,
}

/// Payload threaded through the auditor events. Carries the phase id under
/// which the audit's attempts are accounted plus the kind discriminator. Sweep
/// audits set `phase_id` to the most recently completed real phase (the same
/// id [`Event::SweepStarted`] uses) so the running attempts counter still keys
/// on a real phase id.
#[derive(Debug, Clone)]
pub struct AuditContext {
    /// Phase id under which the audit's attempts are tallied.
    pub phase_id: PhaseId,
    /// Whether this is a phase audit or a sweep audit.
    pub kind: AuditContextKind,
}

/// Streaming events the runner broadcasts to subscribers. Sends are
/// best-effort: a lagging or absent subscriber never blocks the runner.
#[derive(Debug, Clone)]
pub enum Event {
    /// A phase began. Emitted once per implementer dispatch — fixer
    /// re-dispatches inside the same phase emit [`Event::FixerStarted`] instead
    /// so subscribers can distinguish them.
    PhaseStarted {
        /// Phase being entered.
        phase_id: PhaseId,
        /// Phase title from the heading.
        title: String,
        /// 1-based total agent dispatch counter at this phase, mirrored into
        /// [`crate::state::RunState::attempts`]. Stays at `1` for the
        /// implementer dispatch that fires this event.
        attempt: u32,
    },
    /// The runner dispatched the fixer agent after a test failure.
    FixerStarted {
        /// Phase the fixer is operating on.
        phase_id: PhaseId,
        /// 1-based fixer attempt within this phase
        /// (`1..=fixer_max_attempts`).
        fixer_attempt: u32,
        /// Total agent-dispatch counter at this phase (mirrors
        /// [`crate::state::RunState::attempts`]).
        attempt: u32,
    },
    /// The runner dispatched the auditor agent after the test suite passed.
    /// Fires at most once per phase or sweep, and only when the relevant
    /// audit toggle ([`crate::config::AuditConfig::enabled`] for phases,
    /// [`crate::config::SweepConfig::audit_enabled`] for sweeps) is on and
    /// the dispatch produced staged code changes. `context.kind` discriminates
    /// the two so the TUI can render the right header text.
    AuditorStarted {
        /// Audit-pass context: phase id + whether this is a phase or sweep
        /// audit. Phase id is exposed via [`AuditContext::phase_id`] so plain
        /// log consumers don't need to match on the kind.
        context: AuditContext,
        /// Total agent-dispatch counter at this phase (mirrors
        /// [`crate::state::RunState::attempts`]).
        attempt: u32,
    },
    /// The auditor dispatched without finding code changes worth auditing
    /// (the index was empty after staging excluded paths).
    AuditorSkippedNoChanges {
        /// Audit-pass context: phase id + whether this is a phase or sweep
        /// audit.
        context: AuditContext,
    },
    /// One line of agent stdout.
    AgentStdout(String),
    /// One line of agent stderr.
    AgentStderr(String),
    /// Agent invoked a tool. Carries the tool name.
    AgentToolUse(String),
    /// The runner began running the project test suite.
    TestStarted,
    /// The test suite finished with the carried summary.
    TestFinished {
        /// Whether the run exited zero.
        passed: bool,
        /// Short summary suitable for inline display.
        summary: String,
    },
    /// The runner skipped tests because no runner was detected and no
    /// `[tests] command = "..."` override was configured.
    TestsSkipped,
    /// A phase's code changes were committed (or skipped because the only
    /// changes were to excluded planning artifacts).
    PhaseCommitted {
        /// Phase that completed.
        phase_id: PhaseId,
        /// Resulting commit, or `None` when only excluded paths changed.
        commit: Option<CommitId>,
    },
    /// The runner stopped without advancing.
    PhaseHalted {
        /// Phase that halted.
        phase_id: PhaseId,
        /// Why the runner halted.
        reason: HaltReason,
    },
    /// The runner advanced past the final phase. No further phases remain.
    RunFinished,
    /// Aggregated token usage was updated after an agent dispatch finished.
    /// Carries a snapshot of [`crate::state::RunState::token_usage`] so
    /// subscribers can show running cost / token totals without needing a
    /// reference to the runner state.
    UsageUpdated(crate::state::TokenUsage),
    /// A deferred-sweep dispatch began. Fires once per sweep step, between
    /// the [`Event::PhaseCommitted`] of the preceding phase and the
    /// [`Event::PhaseStarted`] of the next regular phase. The sweep does not
    /// introduce a synthetic phase id — `after` is the most recently completed
    /// phase under which the sweep's attempts are accounted.
    SweepStarted {
        /// Phase the sweep is firing after.
        after: PhaseId,
        /// Unchecked-item count observed before the sweep dispatched.
        items_pending: usize,
        /// Total agent-dispatch counter under `after` after the sweep's
        /// implementer dispatch is recorded.
        attempt: u32,
    },
    /// A deferred-sweep dispatch finished and its commit (or empty diff) has
    /// landed. `commit` is `None` when the sweep produced no code changes.
    SweepCompleted {
        /// Phase the sweep fired after.
        after: PhaseId,
        /// Number of `## Deferred items` entries the sweep flipped from
        /// `- [ ]` to `- [x]` (and which the post-sweep `DeferredDoc::sweep`
        /// then dropped). New items the agent appended don't pollute this
        /// count.
        resolved: usize,
        /// Resulting commit, or `None` for the no-code-changes case.
        commit: Option<CommitId>,
    },
    /// A deferred-sweep dispatch halted before it could land a commit. Mirrors
    /// [`Event::PhaseHalted`] for the sweep entry point so subscribers can
    /// distinguish "phase failed" from "sweep failed."
    SweepHalted {
        /// Phase the sweep fired after.
        after: PhaseId,
        /// Why the sweep halted.
        reason: HaltReason,
    },
    /// A `## Deferred items` entry's per-sweep attempt counter just crossed
    /// [`crate::config::SweepConfig::escalate_after`]. Transition-only — the
    /// next sweep that increments the same item's counter further does *not*
    /// re-emit, so subscribers (the activity log, [`pitboss status`], the TUI)
    /// can treat each occurrence as a fresh "needs human attention" signal.
    DeferredItemStale {
        /// The item text from `deferred.md` (the bytes after `- [ ]`).
        text: String,
        /// The new attempt count, equal to or greater than `escalate_after`.
        attempts: u32,
    },
}

/// Outcome of [`Runner::run_phase`].
#[derive(Debug, Clone)]
pub enum PhaseResult {
    /// Phase completed and the runner advanced. `commit` is `None` when the
    /// agent only modified excluded paths.
    Advanced {
        /// Phase that just completed.
        phase_id: PhaseId,
        /// Phase the runner advanced to, or `None` if no phases remain.
        next_phase: Option<PhaseId>,
        /// Resulting commit, or `None` for the excluded-only case.
        commit: Option<CommitId>,
    },
    /// Runner halted; no phase advance.
    Halted {
        /// Phase that was active when the halt fired.
        phase_id: PhaseId,
        /// Why the halt fired.
        reason: HaltReason,
    },
}

/// Outcome of [`Runner::run`].
#[derive(Debug, Clone)]
pub enum RunSummary {
    /// All phases completed.
    Finished,
    /// The run halted at the carried phase for the carried reason.
    Halted {
        /// Phase that halted.
        phase_id: PhaseId,
        /// Why the halt fired.
        reason: HaltReason,
    },
}

/// Per-phase orchestrator.
///
/// One `Runner` drives a single workspace through its plan. Construct with
/// [`Runner::new`], subscribe one or more receivers via [`Runner::subscribe`],
/// then call [`Runner::run`] (or [`Runner::run_phase`] for tests).
pub struct Runner<A: Agent, G: Git> {
    workspace: PathBuf,
    config: Config,
    plan: Plan,
    deferred: DeferredDoc,
    state: RunState,
    agent: A,
    git: G,
    events_tx: broadcast::Sender<Event>,
    /// When `true`, [`Runner::run_phase`] skips test detection and execution.
    /// Set by `pitboss play --dry-run`, which dispatches the no-op
    /// [`crate::agent::dry_run::DryRunAgent`]: since the agent never modifies
    /// the working tree, running tests would only re-confirm whatever the
    /// pre-run state was and risk halting the dry-run on a flaky suite.
    skip_tests: bool,
    /// Operator-supplied override for the deferred-sweep gate. Driven by
    /// `pitboss play --no-sweep` and `--sweep`; defaults to
    /// [`SweepOverride::None`] which lets the configured trigger run.
    sweep_override: SweepOverride,
}

/// Operator-supplied override for the deferred-sweep trigger.
///
/// Set via [`Runner::skip_sweep`] / [`Runner::force_sweep`] (mirrored on the
/// CLI as `pitboss play --no-sweep` / `--sweep`). The two flags are mutually
/// exclusive at the clap level, so the runner only ever sees one of these
/// applied per invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepOverride {
    /// Honor the configured sweep trigger (default).
    None,
    /// Suppress sweeps for the duration of the run. Clears `pending_sweep`
    /// at the top of the run and refuses to arm it from any subsequent phase
    /// commit. The override is not persisted to `pitboss.toml`.
    Skip,
    /// Force a sweep before the next phase even if the trigger threshold
    /// isn't met. Sets `pending_sweep = true` at the top of the run and
    /// bypasses the trigger re-evaluation in the gate so the sweep dispatches
    /// once. After it lands `pending_sweep` clears normally and the
    /// post-phase trigger reverts to the configured behavior.
    Force,
}

impl<A: Agent, G: Git> Runner<A, G> {
    /// Build a new runner. The caller has already loaded `config`, `plan`,
    /// `deferred`, and `state` from the workspace and is responsible for
    /// having checked out the per-run branch (via [`crate::git::Git`]) before
    /// calling [`Runner::run`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace: impl Into<PathBuf>,
        config: Config,
        plan: Plan,
        deferred: DeferredDoc,
        state: RunState,
        agent: A,
        git: G,
    ) -> Self {
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            workspace: workspace.into(),
            config,
            plan,
            deferred,
            state,
            agent,
            git,
            events_tx,
            skip_tests: false,
            sweep_override: SweepOverride::None,
        }
    }

    /// Skip the per-phase test invocation entirely. Used by
    /// `pitboss play --dry-run` so a no-op agent does not get halted by a
    /// pre-existing red test suite. The runner emits [`Event::TestsSkipped`]
    /// in place of [`Event::TestStarted`] / [`Event::TestFinished`] so
    /// subscribers (logger, TUI) still get a clear signal that tests were
    /// considered.
    pub fn skip_tests(mut self, skip: bool) -> Self {
        self.skip_tests = skip;
        self
    }

    /// Suppress deferred sweeps for the duration of this runner. Mirrors the
    /// CLI flag `pitboss play --no-sweep`: clears `state.pending_sweep`
    /// immediately so an inherited obligation from a prior run is dropped,
    /// and refuses to arm the gate from any subsequent phase commit. The
    /// override is in-memory only — it does not write to `pitboss.toml`.
    pub fn skip_sweep(mut self, skip: bool) -> Self {
        if skip {
            self.sweep_override = SweepOverride::Skip;
            self.state.pending_sweep = false;
        }
        self
    }

    /// Force a sweep before the next phase, regardless of the configured
    /// trigger threshold. Mirrors `pitboss play --sweep`: sets
    /// `state.pending_sweep = true` so the gate fires on the next
    /// [`Runner::run_phase`], and bypasses the gate's trigger re-evaluation
    /// so a backlog below `trigger_min_items` still sweeps once. Mutually
    /// exclusive with [`Runner::skip_sweep`] (enforced at the CLI layer).
    pub fn force_sweep(mut self, force: bool) -> Self {
        if force {
            self.sweep_override = SweepOverride::Force;
            self.state.pending_sweep = true;
        }
        self
    }

    /// Workspace this runner operates on.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Borrow the loaded plan. Useful for tests asserting state advance.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Borrow the loaded deferred doc.
    pub fn deferred(&self) -> &DeferredDoc {
        &self.deferred
    }

    /// Borrow the run state.
    pub fn state(&self) -> &RunState {
        &self.state
    }

    /// Borrow the loaded config. Used by the TUI to populate the agent /
    /// per-role model header chip.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Borrow the dispatching agent. Used by the TUI to read
    /// [`Agent::name`] for the header chip.
    pub fn agent(&self) -> &A {
        &self.agent
    }

    /// Borrow the git handle the runner is using. CLI code that needs to call
    /// post-run git operations (e.g., `gh pr create` after a successful run)
    /// reaches in through here so the same shell-vs-mock implementation the
    /// runner used during the run is reused.
    pub fn git_handle(&self) -> &G {
        &self.git
    }

    /// Subscribe to the runner's event stream. Returns a fresh receiver each
    /// call; existing subscribers are unaffected.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events_tx.subscribe()
    }

    /// Snapshot of `## Deferred items` entries whose per-sweep attempt counter
    /// is at or above [`crate::config::SweepConfig::escalate_after`]. Sorted
    /// by descending attempts (text ascending as a deterministic tiebreaker)
    /// and capped at [`STALE_ITEMS_PROMPT_CAP`] entries so the sweep prompt
    /// stays bounded.
    ///
    /// The runner consults this every time it builds a sweep or sweep-auditor
    /// prompt; `pitboss status` and the TUI also call it to surface the
    /// "needs human attention" list. Returning a fresh `Vec` keeps the caller
    /// from leaking the underlying `RunState` map.
    pub fn stale_items(&self) -> Vec<prompts::StaleItem> {
        let escalate = self.config.sweep.escalate_after.max(1);
        let mut items: Vec<prompts::StaleItem> = self
            .state
            .deferred_item_attempts
            .iter()
            .filter(|(_, &n)| n >= escalate)
            .map(|(text, &attempts)| prompts::StaleItem {
                text: text.clone(),
                attempts,
            })
            .collect();
        items.sort_by(|a, b| b.attempts.cmp(&a.attempts).then(a.text.cmp(&b.text)));
        items.truncate(STALE_ITEMS_PROMPT_CAP);
        items
    }

    /// Drive the runner until the plan completes or a phase halts.
    ///
    /// Always emits exactly one terminal event before returning:
    /// [`Event::RunFinished`] on the success path, [`Event::PhaseHalted`] on
    /// the halt path. Subscribers (logger, TUI) treat either event as the
    /// signal that no further events will arrive — the broadcast channel
    /// itself does not close because `Runner` keeps owning the sender for
    /// post-run lookups (e.g., PR creation reads `runner.state()`).
    pub async fn run(&mut self) -> Result<RunSummary> {
        // Resume guard: a previous run drove past the final phase and either
        // halted inside the final-sweep loop or interrupted before the loop
        // could clear `pending_sweep`. Re-enter the loop directly so we don't
        // accidentally re-run the final phase below.
        if self.is_post_final_phase_state() {
            return self.finish_or_run_final_sweep_loop().await;
        }
        loop {
            let result = self.run_phase().await?;
            match result {
                PhaseResult::Halted { phase_id, reason } => {
                    let _ = self.events_tx.send(Event::PhaseHalted {
                        phase_id: phase_id.clone(),
                        reason: reason.clone(),
                    });
                    return Ok(RunSummary::Halted { phase_id, reason });
                }
                PhaseResult::Advanced {
                    next_phase: None, ..
                } => {
                    return self.finish_or_run_final_sweep_loop().await;
                }
                PhaseResult::Advanced { .. } => {}
            }
        }
    }

    /// True when state + plan describe "the final regular phase has committed
    /// but `Runner::run` hasn't yet emitted [`Event::RunFinished`]". The plan
    /// never advances `current_phase` past the last phase, so after a final
    /// commit `state.completed.last() == plan.current_phase`. A halted phase
    /// or sweep before the final commit lands does not satisfy this — its
    /// `state.completed.last()` is the *previous* phase id.
    fn is_post_final_phase_state(&self) -> bool {
        let Some(last_completed) = self.state.completed.last() else {
            return false;
        };
        last_completed == &self.plan.current_phase
            && self.next_phase_id_after(last_completed).is_none()
    }

    /// Shared tail used by both the success path of [`Runner::run`]'s phase
    /// loop and the post-final-phase resume guard. Either dispatches the
    /// bounded final-sweep drain loop (when sweeps are enabled, the trailing
    /// drain is enabled, the operator did not pass `--no-sweep`, and at least
    /// one unchecked item remains) or emits [`Event::RunFinished`] and
    /// returns.
    async fn finish_or_run_final_sweep_loop(&mut self) -> Result<RunSummary> {
        let after = self
            .state
            .completed
            .last()
            .cloned()
            .unwrap_or_else(|| self.plan.current_phase.clone());
        if self.should_run_final_sweep_loop() {
            return self.run_final_sweep_loop(after).await;
        }
        // No drain loop: clear any inherited `pending_sweep` (a `--no-sweep`
        // resume of a halted final-sweep run gets here) before declaring the
        // run finished, so a follow-up `pitboss play` doesn't see stale state.
        if self.state.pending_sweep {
            self.state.pending_sweep = false;
            state::save(&self.workspace, Some(&self.state))
                .context("runner: clearing pending_sweep at run finish")?;
        }
        let _ = self.events_tx.send(Event::RunFinished);
        Ok(RunSummary::Finished)
    }

    /// Decide whether the final-sweep drain loop should run. The master
    /// `[sweep] enabled` switch dominates the trailing-drain
    /// `[sweep] final_sweep_enabled` toggle so an operator with sweeps
    /// disabled never sees a surprise drain pass. `--no-sweep` suppresses the
    /// loop the same way it suppresses between-phase sweeps.
    fn should_run_final_sweep_loop(&self) -> bool {
        if matches!(self.sweep_override, SweepOverride::Skip) {
            return false;
        }
        if !self.config.sweep.enabled {
            return false;
        }
        if !self.config.sweep.final_sweep_enabled {
            return false;
        }
        sweep::unchecked_count(&self.deferred) > 0
    }

    /// Bounded final-sweep drain. Runs after the final regular phase commits
    /// (or when a previous run halted inside this loop) and dispatches at most
    /// [`crate::config::SweepConfig::final_sweep_max_iterations`] sweeps in a
    /// row. Each iteration must resolve at least one item or the loop exits;
    /// items the agent genuinely can't fix fall through to phase 05's
    /// staleness machinery.
    ///
    /// `after` is the phase id every dispatch anchors on — the last completed
    /// regular phase. Each iteration calls [`Runner::run_sweep_step_inner`] so
    /// the existing sweep/auditor/staleness pipeline runs unchanged; the loop
    /// adds only the iteration cap, the no-progress exit, and the per-iter
    /// `pre_unchecked == 0` short-circuit.
    async fn run_final_sweep_loop(&mut self, after: PhaseId) -> Result<RunSummary> {
        // The post-final-phase commit reset `consecutive_sweeps` to 0 already,
        // but a halted-then-resumed loop might inherit a stale value. We
        // bypass the gate in [`Runner::run_phase_inner`] and dispatch
        // [`Runner::run_sweep_step_inner`] directly, so the clamp can't pre-empt
        // the loop — but resetting keeps the persisted counter honest for
        // observers (`pitboss status`, the TUI).
        self.state.consecutive_sweeps = 0;

        let max_iters = self.config.sweep.final_sweep_max_iterations.max(1);
        for _iter in 1..=max_iters {
            let pre_unchecked = sweep::unchecked_count(&self.deferred);
            if pre_unchecked == 0 {
                // Drain succeeded (either the previous iteration cleared the
                // last item, or we entered with an already-empty backlog).
                break;
            }
            self.state.pending_sweep = true;
            state::save(&self.workspace, Some(&self.state))
                .context("runner: persisting pending_sweep for final-sweep iter")?;
            let after_clone = after.clone();
            let result = self
                .run_sweep_step_inner(after_clone.clone(), Some(after_clone), None)
                .await?;
            match result {
                PhaseResult::Halted { reason, .. } => {
                    // `pending_sweep` stays true on the halt path so a resume
                    // re-enters this loop from iteration 1. Persist here
                    // because `run_sweep_step_inner`'s halt path doesn't save
                    // (the regular gate relies on `run_phase`'s save wrapper,
                    // which doesn't run on this code path), and phase 05's
                    // staleness counter increments must survive the halt so
                    // cumulative progress carries across resumes.
                    state::save(&self.workspace, Some(&self.state))
                        .context("runner: persisting state at final-sweep halt")?;
                    let _ = self.events_tx.send(Event::PhaseHalted {
                        phase_id: after.clone(),
                        reason: reason.clone(),
                    });
                    return Ok(RunSummary::Halted {
                        phase_id: after,
                        reason,
                    });
                }
                PhaseResult::Advanced { .. } => {
                    let post_unchecked = sweep::unchecked_count(&self.deferred);
                    let resolved = pre_unchecked.saturating_sub(post_unchecked);
                    if resolved == 0 {
                        // Stuck items survive the loop and surface via phase
                        // 05's staleness machinery.
                        break;
                    }
                }
            }
        }
        // Clean exit — `run_sweep_step_inner` already cleared `pending_sweep`
        // on each successful iteration, but a `pre_unchecked == 0` short
        // circuit at the top of iteration 1 (resume on a backlog the user
        // drained by hand) needs us to clear it here too.
        if self.state.pending_sweep {
            self.state.pending_sweep = false;
            state::save(&self.workspace, Some(&self.state))
                .context("runner: clearing pending_sweep after final-sweep loop")?;
        }
        let _ = self.events_tx.send(Event::RunFinished);
        Ok(RunSummary::Finished)
    }

    /// Execute the current phase to completion (success or halt).
    ///
    /// Persists [`RunState`] to `.pitboss/play/state.json` on every exit — including
    /// halts — so the attempts counter and accumulated token usage survive a
    /// halted phase and a subsequent `pitboss play` (or `pitboss rebuy`)
    /// invocation can pick them up.
    pub async fn run_phase(&mut self) -> Result<PhaseResult> {
        let result = self.run_phase_inner().await;
        if let Err(e) = state::save(&self.workspace, Some(&self.state)) {
            tracing::error!("runner: failed to persist state.json: {e:#}");
        }
        result
    }

    async fn run_phase_inner(&mut self) -> Result<PhaseResult> {
        // Pending sweep gate. A regular phase that closed above the trigger
        // threshold sets `state.pending_sweep = true`; we re-evaluate the
        // trigger here against the current on-disk deferred so a manual
        // cleanup between resumes (the user clearing items by hand) drains
        // the obligation cleanly rather than firing a no-op sweep.
        if self.state.pending_sweep {
            // Re-read from disk so external edits between resumes are
            // observed, not just the cached `self.deferred`.
            let deferred_path = paths::deferred_path(&self.workspace);
            let on_disk = match std::fs::read_to_string(&deferred_path) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => {
                    return Err(anyhow::Error::new(e)
                        .context(format!("runner: reading {:?}", &deferred_path)))
                }
            };
            let parsed = deferred::parse(&on_disk).unwrap_or_else(|_| self.deferred.clone());
            // `--no-sweep` shouldn't have left `pending_sweep` armed (the
            // builder clears it), but defend against an externally set flag
            // by short-circuiting here. `--sweep` bypasses the trigger so a
            // forced sweep below the threshold still fires once.
            let allow = match self.sweep_override {
                SweepOverride::Skip => false,
                SweepOverride::Force => true,
                SweepOverride::None => sweep::should_run_deferred_sweep(
                    &parsed,
                    &self.config.sweep,
                    self.state.consecutive_sweeps,
                ),
            };
            if allow {
                self.deferred = parsed;
                // For runs that have at least one completed phase, anchor
                // the sweep on it. For a forced sweep before the very
                // first phase commits (a `--sweep` on a fresh run), there
                // is nothing to anchor on, so accounting falls back to the
                // plan's current phase and the prompt label renders the
                // standalone "no preceding phase" variant.
                let prompt_after = self.state.completed.last().cloned();
                let accounting = prompt_after
                    .clone()
                    .unwrap_or_else(|| self.plan.current_phase.clone());
                return self
                    .run_sweep_step_inner(accounting, prompt_after, None)
                    .await;
            } else {
                self.state.pending_sweep = false;
                state::save(&self.workspace, Some(&self.state))
                    .context("runner: persisting state.json after sweep gate cleared")?;
                self.deferred = parsed;
            }
        }

        let phase = self
            .plan
            .phase(&self.plan.current_phase)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "plan.current_phase {:?} is not present in plan.phases",
                    self.plan.current_phase.as_str()
                )
            })?;
        let phase_id = phase.id.clone();

        if let Some(reason) = self.check_budget() {
            return Ok(PhaseResult::Halted { phase_id, reason });
        }

        let attempt = self.bump_attempts(&phase_id);
        let _ = self.events_tx.send(Event::PhaseStarted {
            phase_id: phase_id.clone(),
            title: phase.title.clone(),
            attempt,
        });

        let plan_path = paths::plan_path(&self.workspace);
        let deferred_path = paths::deferred_path(&self.workspace);
        // Every pitboss artifact lives under `.pitboss/`, which is gitignored,
        // so one exclude entry covers plan.md, deferred.md, state.json, logs,
        // snapshots, grind state, etc.
        let exclude: [&Path; 1] = [Path::new(".pitboss")];

        let spec = DispatchSpec {
            request: self.implementer_request(&phase, attempt),
            phase_id: phase_id.clone(),
            phase: Some(&phase),
            plan_path: &plan_path,
            deferred_path: &deferred_path,
            exclude_paths: &exclude,
            audit: self
                .config
                .audit
                .enabled
                .then_some(AuditKind::Phase { phase: &phase }),
        };

        let has_changes = match self.run_dispatch_pipeline(spec).await? {
            PipelineOutcome::Halted(reason) => return Ok(PhaseResult::Halted { phase_id, reason }),
            PipelineOutcome::Staged { has_changes } => has_changes,
        };

        let commit = if has_changes {
            let id = self
                .git
                .commit(&git::commit_message(&phase_id, &phase.title))
                .await
                .context("runner: committing phase")?;
            Some(id)
        } else {
            warn!(phase = %phase_id, "phase produced no code changes; skipping commit");
            None
        };

        self.deferred.sweep();
        write_atomic(
            &deferred_path,
            deferred::serialize(&self.deferred).as_bytes(),
        )
        .context("runner: writing deferred.md after sweep")?;

        self.state.completed.push(phase_id.clone());
        // Forward step → re-arm the consecutive-sweep clamp so the next
        // pending sweep is allowed to fire.
        self.state.consecutive_sweeps = 0;

        let next_phase = self.next_phase_id_after(&phase_id);
        if let Some(ref next) = next_phase {
            self.plan.set_current_phase(next.clone());
            write_atomic(&plan_path, plan::serialize(&self.plan).as_bytes())
                .context("runner: writing plan.md with advanced current_phase")?;
            // Only consider scheduling a between-phase sweep when there is a
            // next phase to insert it before. End-of-run sweeps (after the
            // final phase) belong to phase 08. `--no-sweep` suppresses
            // arming entirely; `--sweep` is consumed by the gate above (and
            // would already have fired by the time we get here), so the
            // post-phase trigger reverts to the configured behavior.
            if !matches!(self.sweep_override, SweepOverride::Skip)
                && sweep::should_run_deferred_sweep(
                    &self.deferred,
                    &self.config.sweep,
                    self.state.consecutive_sweeps,
                )
            {
                self.state.pending_sweep = true;
            }
        }

        state::save(&self.workspace, Some(&self.state)).context("runner: persisting state.json")?;

        let _ = self.events_tx.send(Event::PhaseCommitted {
            phase_id: phase_id.clone(),
            commit: commit.clone(),
        });

        Ok(PhaseResult::Advanced {
            phase_id,
            next_phase,
            commit,
        })
    }

    /// Build the implementer [`AgentRequest`] for `phase`. `attempt` is the
    /// 1-based dispatch counter the caller pulled from [`bump_attempts`]; it
    /// flows into the per-attempt log filename so a fixer re-dispatch later in
    /// the phase does not clobber the implementer's log.
    fn implementer_request(&self, phase: &crate::plan::Phase, attempt: u32) -> AgentRequest {
        AgentRequest {
            role: Role::Implementer,
            model: self.config.models.implementer.clone(),
            system_prompt: prompts::caveman::system_prompt(&self.config.caveman),
            user_prompt: prompts::implementer(&self.plan, &self.deferred, phase),
            workdir: self.workspace.clone(),
            log_path: self.attempt_log_path(&phase.id, "implementer", attempt),
            timeout: DEFAULT_AGENT_TIMEOUT,
            env: std::collections::HashMap::new(),
        }
    }

    /// Drive the dispatch → validate → tests → fixer → optional auditor → stage
    /// chain for one already-built [`AgentRequest`]. The caller is responsible
    /// for bumping the attempts counter and emitting the "started" event for
    /// the dispatch type before invoking this; on return either a halt reason
    /// is surfaced or the working tree has been staged and `has_changes`
    /// indicates whether anything outside `exclude_paths` ended up in the
    /// index. Phase 03 reuses this helper from a sweep entry point, which is
    /// why the caller — not the helper — owns the commit decision.
    async fn run_dispatch_pipeline(&mut self, spec: DispatchSpec<'_>) -> Result<PipelineOutcome> {
        let DispatchSpec {
            request,
            phase_id,
            phase,
            plan_path,
            deferred_path,
            exclude_paths,
            audit,
        } = spec;

        let role = request.role;
        match self
            .dispatch_and_validate(request, role, plan_path, deferred_path)
            .await?
        {
            ValidationResult::Continue => {}
            ValidationResult::Halt(reason) => return Ok(PipelineOutcome::Halted(reason)),
        }

        let test_runner = if self.skip_tests {
            debug!("dry-run: skipping test detection and execution");
            None
        } else {
            project_tests::detect(&self.workspace, self.config.tests.command.as_deref())
        };
        if let Some(runner) = &test_runner {
            // The post-dispatch test log shares the agent's attempt number so
            // operators can pair them up at a glance. `bump_attempts` was
            // called by the caller before building `request`, so reading
            // state.attempts here yields exactly that attempt.
            let attempt = self.state.attempts.get(&phase_id).copied().unwrap_or(0);
            let outcome = self.run_tests(runner, &phase_id, "tests", attempt).await?;
            if !outcome.passed {
                match self
                    .run_fixer_loop(
                        &phase_id,
                        phase,
                        runner,
                        plan_path,
                        deferred_path,
                        outcome.summary,
                    )
                    .await?
                {
                    FixerLoopResult::Passed => {}
                    FixerLoopResult::Halted(reason) => {
                        return Ok(PipelineOutcome::Halted(reason));
                    }
                }
            }
        } else {
            if !self.skip_tests {
                debug!("no test runner detected and no override configured; skipping tests");
            }
            let _ = self.events_tx.send(Event::TestsSkipped);
        }

        if let Some(audit) = audit {
            match self
                .run_auditor_pass(
                    audit,
                    test_runner.as_ref(),
                    plan_path,
                    deferred_path,
                    exclude_paths,
                    &phase_id,
                )
                .await?
            {
                AuditPassResult::Continue => {}
                AuditPassResult::Halted(reason) => return Ok(PipelineOutcome::Halted(reason)),
            }
        }

        // Re-stage to capture anything the auditor added or modified. When the
        // auditor was skipped (disabled, or no code changes to audit) this is
        // the first stage call of the phase.
        self.git
            .stage_changes(exclude_paths)
            .await
            .context("runner: staging code-only changes")?;

        let has_changes = self
            .git
            .has_staged_changes()
            .await
            .context("runner: checking for staged changes")?;

        Ok(PipelineOutcome::Staged { has_changes })
    }

    /// Compare the running [`RunState::token_usage`] against the configured
    /// budgets. Returns [`HaltReason::BudgetExceeded`] when either cap has
    /// been met or surpassed, otherwise `None`. Called before every agent
    /// dispatch so a fresh run never exceeds its budget by more than one
    /// dispatch's worth of tokens.
    fn check_budget(&self) -> Option<HaltReason> {
        let (tokens, usd) = budget_totals(&self.config, &self.state.token_usage);
        if let Some(cap) = self.config.budgets.max_total_tokens {
            if tokens >= cap {
                return Some(HaltReason::BudgetExceeded(format!(
                    "token budget reached: {tokens} >= cap {cap}"
                )));
            }
        }
        if let Some(cap) = self.config.budgets.max_total_usd {
            if usd >= cap {
                return Some(HaltReason::BudgetExceeded(format!(
                    "USD budget reached: ${usd:.4} >= cap ${cap:.4}"
                )));
            }
        }
        None
    }

    fn next_phase_id_after(&self, current: &PhaseId) -> Option<PhaseId> {
        self.plan
            .phases
            .iter()
            .find(|p| p.id > *current)
            .map(|p| p.id.clone())
    }

    /// Increment and return the per-phase attempt counter. The counter mirrors
    /// every agent dispatch made for a phase (implementer + each fixer
    /// attempt) so [`crate::state::RunState::attempts`] is the single source
    /// of truth for "how many model dispatches did this phase consume."
    fn bump_attempts(&mut self, phase_id: &PhaseId) -> u32 {
        let entry = self.state.attempts.entry(phase_id.clone()).or_insert(0);
        *entry += 1;
        *entry
    }

    fn attempt_log_path(&self, phase_id: &PhaseId, role: &str, attempt: u32) -> PathBuf {
        paths::play_logs_dir(&self.workspace)
            .join(format!("phase-{}-{}-{}.log", phase_id, role, attempt))
    }

    /// Per-attempt log path for a sweep dispatch. Distinct prefix from
    /// [`Runner::attempt_log_path`] so an operator scanning `.pitboss/play/logs/`
    /// can tell sweep dispatches apart from regular phase dispatches at a
    /// glance even though both account against `state.attempts[after]`.
    fn sweep_log_path(&self, after: &PhaseId, role: &str, attempt: u32) -> PathBuf {
        paths::play_logs_dir(&self.workspace)
            .join(format!("sweep-after-{}-{}-{}.log", after, role, attempt))
    }

    /// Run a one-shot deferred sweep against the loaded state without
    /// advancing the plan state machine. Backs the `pitboss sweep`
    /// subcommand: an operator who edited `deferred.md` by hand or wants to
    /// drain a backlog ahead of the next `pitboss play` can invoke this in
    /// isolation.
    ///
    /// `after` is the prompt's `after_phase` label — when `None`, the sweep
    /// prompt renders the standalone variant ("no preceding phase to anchor
    /// on"). Accounting (attempts counter, log filename, events, commit
    /// message) falls back to the plan's current phase when no `after` is
    /// supplied so the dispatch still has a stable phase id to key on.
    ///
    /// `max_items` clamps the prompt's pending-items list to the first N
    /// items in document order without changing the on-disk file. Use it to
    /// keep a pathological backlog (100+ items) within the agent's effective
    /// context window; remaining items surface on the next sweep.
    pub async fn run_standalone_sweep(
        &mut self,
        after: Option<PhaseId>,
        max_items: Option<usize>,
    ) -> Result<PhaseResult> {
        let accounting = after
            .clone()
            .unwrap_or_else(|| self.plan.current_phase.clone());
        self.run_sweep_step_inner(accounting, after, max_items).await
    }

    /// Shared body of the inter-phase sweep gate (in
    /// [`Runner::run_phase_inner`]) and [`Runner::run_standalone_sweep`].
    ///
    /// The sweep reuses [`Runner::run_dispatch_pipeline`] with `phase: None`
    /// and `audit: Some(AuditKind::Sweep { .. })` when
    /// [`crate::config::SweepConfig::audit_enabled`] is on. The implementer's
    /// prompt is built via [`prompts::sweep`]; if tests fail post-dispatch
    /// the fixer falls back to [`prompts::fixer_for_sweep`] inside the
    /// pipeline.
    ///
    /// `accounting` is the phase id used everywhere a sweep needs a stable
    /// key — `state.attempts`, the per-attempt log filename, the
    /// `Sweep{Started,Halted,Completed}` events, and the sweep commit
    /// message. `prompt_after` is what the implementer / auditor / fixer
    /// prompts read as the `{after}` substitution; `None` selects the
    /// standalone-sweep variant in [`prompts::sweep`]. `max_items`, when
    /// `Some(n)`, clamps the pending-items list passed into
    /// [`prompts::sweep`] to the first `n` pending items in document order.
    /// The on-disk `deferred.md` is unchanged: items dropped from the prompt
    /// view stay in the file and surface on the next sweep.
    async fn run_sweep_step_inner(
        &mut self,
        accounting: PhaseId,
        prompt_after: Option<PhaseId>,
        max_items: Option<usize>,
    ) -> Result<PhaseResult> {
        // Capture pre-sweep accounting. `pre_unchecked` drives the resolved
        // count for the commit message; `pre_texts` is threaded into
        // [`AuditKind::Sweep`] so the auditor pass can diff it against the
        // post-dispatch deferred doc to compute resolved / remaining.
        let pre_unchecked = sweep::unchecked_count(&self.deferred);
        let pre_texts: HashSet<String> = self
            .deferred
            .items
            .iter()
            .filter(|i| !i.done)
            .map(|i| i.text.clone())
            .collect();
        // Snapshot the H3 phases block. The sweep prompt forbids touching
        // `## Deferred phases`; a mismatch on the way out trips the
        // DeferredInvalid guard.
        let pre_phases = phases_block_canonical(&self.deferred);

        let plan_path = paths::plan_path(&self.workspace);
        let deferred_path = paths::deferred_path(&self.workspace);
        let pre_deferred_bytes = match std::fs::read(&deferred_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("runner: reading {:?} before sweep", &deferred_path)))
            }
        };

        if let Some(reason) = self.check_budget() {
            // A halt is not a free pass on the staleness clock — items still
            // pending count against `escalate_after` even when the dispatch
            // never ran. `self.deferred` is unchanged here, so survivors ==
            // pre_texts.
            self.apply_sweep_staleness(&pre_texts);
            return Ok(PhaseResult::Halted {
                phase_id: accounting,
                reason,
            });
        }

        let attempt = self.bump_attempts(&accounting);
        let _ = self.events_tx.send(Event::SweepStarted {
            after: accounting.clone(),
            items_pending: pre_unchecked,
            attempt,
        });

        let stale = self.stale_items();
        let prompt_doc = match max_items {
            Some(n) => clamp_pending_items(&self.deferred, n),
            None => self.deferred.clone(),
        };
        let request = AgentRequest {
            role: Role::Implementer,
            model: self.config.models.implementer.clone(),
            system_prompt: prompts::caveman::system_prompt(&self.config.caveman),
            user_prompt: prompts::sweep(&self.plan, &prompt_doc, prompt_after.as_ref(), &stale),
            workdir: self.workspace.clone(),
            log_path: self.sweep_log_path(&accounting, "implementer", attempt),
            timeout: DEFAULT_AGENT_TIMEOUT,
            env: std::collections::HashMap::new(),
        };

        let exclude: [&Path; 1] = [Path::new(".pitboss")];
        let spec = DispatchSpec {
            request,
            phase_id: accounting.clone(),
            phase: None,
            plan_path: &plan_path,
            deferred_path: &deferred_path,
            exclude_paths: &exclude,
            audit: self.config.sweep.audit_enabled.then(|| AuditKind::Sweep {
                after: accounting.clone(),
                pre_texts: pre_texts.clone(),
            }),
        };

        let has_changes = match self.run_dispatch_pipeline(spec).await? {
            PipelineOutcome::Halted(reason) => {
                // Halt path: bookkeeping fires before the halt event so the
                // staleness counter for surviving items reflects this attempt.
                // For dispatch_and_validate halts, `self.deferred` is the
                // pre-dispatch state (validation rolled back). For halts after
                // a successful implementer dispatch (test failure, audit
                // failure), `self.deferred` reflects the agent's edits — items
                // the agent flipped to `- [x]` are not in post_unchecked and
                // get pruned, items that survived get incremented.
                self.apply_sweep_staleness(&pre_texts);
                let _ = self.events_tx.send(Event::SweepHalted {
                    after: accounting.clone(),
                    reason: reason.clone(),
                });
                return Ok(PhaseResult::Halted {
                    phase_id: accounting,
                    reason,
                });
            }
            PipelineOutcome::Staged { has_changes } => has_changes,
        };

        // H3 invariant: the sweep prompt forbids editing `## Deferred phases`.
        // Restore from the pre-dispatch deferred snapshot when violated so the
        // halt is genuinely recoverable.
        let post_phases = phases_block_canonical(&self.deferred);
        if post_phases != pre_phases {
            warn!(
                after = %accounting,
                "sweep modified ## Deferred phases; restoring deferred.md"
            );
            self.restore_deferred(&deferred_path, &pre_deferred_bytes, true)?;
            // Re-parse the restored bytes so the cached doc agrees with disk.
            let restored = std::fs::read_to_string(&deferred_path).unwrap_or_default();
            if let Ok(parsed) = deferred::parse(&restored) {
                self.deferred = parsed;
            }
            // After restoration `self.deferred` matches pre-state, so the
            // bookkeeping treats every pre-text item as a survivor (+1 attempt).
            self.apply_sweep_staleness(&pre_texts);
            let reason = HaltReason::DeferredInvalid("sweep modified Deferred phases".into());
            let _ = self.events_tx.send(Event::SweepHalted {
                after: accounting.clone(),
                reason: reason.clone(),
            });
            return Ok(PhaseResult::Halted {
                phase_id: accounting,
                reason,
            });
        }

        // `resolved` counts items the sweep flipped from `- [ ]` to `- [x]`.
        // `saturating_sub` keeps new items the agent appended (against the
        // prompt's instruction) from polluting the count into a negative.
        let resolved = pre_unchecked.saturating_sub(sweep::unchecked_count(&self.deferred));

        let commit = if has_changes {
            let id = self
                .git
                .commit(&git::commit_message_sweep(&accounting, resolved))
                .await
                .context("runner: committing sweep")?;
            Some(id)
        } else {
            warn!(after = %accounting, "sweep produced no code changes; skipping commit");
            None
        };

        // Staleness bookkeeping must run before `self.deferred.sweep()` would
        // matter, but since the helper filters on `!i.done` either order
        // produces the same `post_unchecked_texts`. Doing it here keeps the
        // success path symmetrical with the halt paths above.
        self.apply_sweep_staleness(&pre_texts);

        // Drop the items the agent ticked off so a later regular phase doesn't
        // re-render them in the next implementer prompt. Mirrors the
        // post-phase sweep call in `run_phase_inner`.
        self.deferred.sweep();
        write_atomic(
            &deferred_path,
            deferred::serialize(&self.deferred).as_bytes(),
        )
        .context("runner: writing deferred.md after sweep step")?;

        self.state.pending_sweep = false;
        self.state.consecutive_sweeps = self.state.consecutive_sweeps.saturating_add(1);
        // `state.completed` tracks plan progress only; sweeps do not push to
        // it. `current_phase` already advanced when the preceding phase
        // committed, so the runner picks the regular phase up on the next
        // `run_phase` call.
        state::save(&self.workspace, Some(&self.state))
            .context("runner: persisting state.json after sweep")?;

        let _ = self.events_tx.send(Event::SweepCompleted {
            after: accounting.clone(),
            resolved,
            commit: commit.clone(),
        });

        Ok(PhaseResult::Advanced {
            phase_id: accounting,
            next_phase: Some(self.plan.current_phase.clone()),
            commit,
        })
    }

    /// Run a single agent dispatch and validate the planning artifacts on the
    /// way out: snapshot `plan.md` + `deferred.md` first, dispatch, then
    /// require `plan.md` to be byte-identical and `deferred.md` to re-parse.
    /// On any failure mode the artifacts are restored from the pre-dispatch
    /// snapshots before returning [`ValidationResult::Halt`].
    async fn dispatch_and_validate(
        &mut self,
        request: AgentRequest,
        role: Role,
        plan_path: &Path,
        deferred_path: &Path,
    ) -> Result<ValidationResult> {
        let plan_pre =
            std::fs::read(plan_path).with_context(|| format!("runner: reading {:?}", plan_path))?;
        let plan_hash = Snapshot::of_bytes(&plan_pre);
        let (deferred_pre, deferred_existed) = match std::fs::read(deferred_path) {
            Ok(b) => (b, true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("runner: reading {:?}", deferred_path))
                )
            }
        };

        let dispatch = self.dispatch_agent(request).await?;
        // Token usage is folded regardless of whether the dispatch ended
        // cleanly — even an aborted run can incur partial spend.
        self.fold_token_usage(role, &dispatch);

        match &dispatch.stop_reason {
            StopReason::Completed => {}
            StopReason::Timeout => {
                return Ok(ValidationResult::Halt(HaltReason::AgentFailure(format!(
                    "agent {:?} timed out after {:?}",
                    self.agent.name(),
                    DEFAULT_AGENT_TIMEOUT
                ))));
            }
            StopReason::Cancelled => {
                return Ok(ValidationResult::Halt(HaltReason::AgentFailure(format!(
                    "agent {:?} was cancelled",
                    self.agent.name()
                ))));
            }
            StopReason::Error(msg) => {
                return Ok(ValidationResult::Halt(HaltReason::AgentFailure(
                    msg.clone(),
                )));
            }
        }

        let plan_post = std::fs::read(plan_path)
            .with_context(|| format!("runner: reading {:?} after agent", plan_path))?;
        if Snapshot::of_bytes(&plan_post) != plan_hash {
            warn!(role = %role, "agent modified plan.md; restoring from snapshot");
            write_atomic(plan_path, &plan_pre).with_context(|| {
                format!(
                    "runner: restoring {:?} from snapshot after tamper",
                    plan_path
                )
            })?;
            return Ok(ValidationResult::Halt(HaltReason::PlanTampered));
        }

        let deferred_text = match std::fs::read_to_string(deferred_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("runner: reading {:?} after agent", deferred_path)))
            }
        };
        match deferred::parse(&deferred_text) {
            Ok(parsed) => {
                self.deferred = parsed;
            }
            Err(e) => {
                let msg = format!("{e}");
                warn!(role = %role, error = %msg, "deferred.md is invalid; restoring");
                self.restore_deferred(deferred_path, &deferred_pre, deferred_existed)?;
                return Ok(ValidationResult::Halt(HaltReason::DeferredInvalid(msg)));
            }
        }

        Ok(ValidationResult::Continue)
    }

    /// Run the project's test suite once, emitting [`Event::TestStarted`] /
    /// [`Event::TestFinished`] around the call. `log_role` distinguishes the
    /// log filename so a fixer-driven re-run does not clobber the prior log.
    async fn run_tests(
        &self,
        runner: &project_tests::TestRunner,
        phase_id: &PhaseId,
        log_role: &str,
        attempt: u32,
    ) -> Result<project_tests::TestOutcome> {
        let _ = self.events_tx.send(Event::TestStarted);
        let test_log = self.attempt_log_path(phase_id, log_role, attempt);
        let outcome = runner
            .run(test_log)
            .await
            .context("runner: running project tests")?;
        let _ = self.events_tx.send(Event::TestFinished {
            passed: outcome.passed,
            summary: outcome.summary.clone(),
        });
        Ok(outcome)
    }

    /// Drive the bounded fixer loop after the implementer's tests fail.
    ///
    /// Up to [`crate::config::RetryBudgets::fixer_max_attempts`] dispatches:
    /// each one snapshots the planning artifacts, dispatches the fixer agent,
    /// validates, then re-runs the tests. The first attempt that produces a
    /// passing test run resolves to [`FixerLoopResult::Passed`]; once the
    /// budget is exhausted the loop returns the final test summary as
    /// [`HaltReason::TestsFailed`].
    async fn run_fixer_loop(
        &mut self,
        phase_id: &PhaseId,
        phase: Option<&crate::plan::Phase>,
        test_runner: &project_tests::TestRunner,
        plan_path: &Path,
        deferred_path: &Path,
        initial_summary: String,
    ) -> Result<FixerLoopResult> {
        let budget = self.config.retries.fixer_max_attempts;
        if budget == 0 {
            return Ok(FixerLoopResult::Halted(HaltReason::TestsFailed(
                initial_summary,
            )));
        }

        let mut last_summary = initial_summary;

        for fixer_attempt in 1..=budget {
            if let Some(reason) = self.check_budget() {
                return Ok(FixerLoopResult::Halted(reason));
            }
            let total_attempt = self.bump_attempts(phase_id);
            let _ = self.events_tx.send(Event::FixerStarted {
                phase_id: phase_id.clone(),
                fixer_attempt,
                attempt: total_attempt,
            });

            let user_prompt = match phase {
                Some(p) => {
                    prompts::fixer_with_deferred(&self.plan, p, &last_summary, &self.deferred)
                }
                None => prompts::fixer_for_sweep(&self.plan, &self.deferred, &last_summary),
            };
            let log_path = self.attempt_log_path(phase_id, "fix", fixer_attempt);
            let request = AgentRequest {
                role: Role::Fixer,
                model: self.config.models.fixer.clone(),
                system_prompt: prompts::caveman::system_prompt(&self.config.caveman),
                user_prompt,
                workdir: self.workspace.clone(),
                log_path,
                timeout: DEFAULT_AGENT_TIMEOUT,
                env: std::collections::HashMap::new(),
            };

            match self
                .dispatch_and_validate(request, Role::Fixer, plan_path, deferred_path)
                .await?
            {
                ValidationResult::Continue => {}
                ValidationResult::Halt(reason) => return Ok(FixerLoopResult::Halted(reason)),
            }

            let outcome = self
                .run_tests(test_runner, phase_id, "tests", total_attempt)
                .await?;
            if outcome.passed {
                return Ok(FixerLoopResult::Passed);
            }
            last_summary = outcome.summary;
        }

        Ok(FixerLoopResult::Halted(HaltReason::TestsFailed(
            last_summary,
        )))
    }

    /// Run the auditor agent. Gating on
    /// [`crate::config::AuditConfig::enabled`] is the caller's responsibility:
    /// [`Runner::run_dispatch_pipeline`] only invokes this helper when its
    /// `audit` field is `Some`, so callers that build the spec without
    /// populating `audit` get the disabled-by-default behavior for free.
    ///
    /// Slots in after the test suite (and any fixer dispatches) passes and
    /// before the per-phase commit. The runner stages the implementer's code
    /// changes so it can hand the resulting `git diff --cached` to the auditor
    /// as input; if the staged diff is empty (e.g. the implementer only edited
    /// planning artifacts) the audit is skipped. The auditor may inline small
    /// fixes or extend `deferred.md`; either way the post-dispatch validation
    /// re-parses `deferred.md` and re-runs the project tests since the auditor
    /// may have edited code. A test failure post-audit halts the phase rather
    /// than re-entering the fixer loop — auditor edits are scoped enough that
    /// breaking the build deserves operator attention.
    async fn run_auditor_pass(
        &mut self,
        audit: AuditKind<'_>,
        test_runner: Option<&project_tests::TestRunner>,
        plan_path: &Path,
        deferred_path: &Path,
        exclude: &[&Path],
        phase_id: &PhaseId,
    ) -> Result<AuditPassResult> {
        // Stage so we can sample the diff. `stage_changes` is idempotent so
        // calling it again post-audit picks up anything the auditor adds.
        self.git
            .stage_changes(exclude)
            .await
            .context("runner: staging for audit diff")?;
        let diff = self
            .git
            .staged_diff()
            .await
            .context("runner: capturing staged diff for auditor")?;

        let kind = match &audit {
            AuditKind::Phase { .. } => AuditContextKind::Phase,
            AuditKind::Sweep { .. } => AuditContextKind::Sweep,
        };
        let context = AuditContext {
            phase_id: phase_id.clone(),
            kind,
        };

        if diff.trim().is_empty() {
            let _ = self.events_tx.send(Event::AuditorSkippedNoChanges {
                context: context.clone(),
            });
            return Ok(AuditPassResult::Continue);
        }

        if let Some(reason) = self.check_budget() {
            return Ok(AuditPassResult::Halted(reason));
        }

        let total_attempt = self.bump_attempts(phase_id);
        let _ = self.events_tx.send(Event::AuditorStarted {
            context: context.clone(),
            attempt: total_attempt,
        });

        let (user_prompt, log_path) = match audit {
            AuditKind::Phase { phase } => (
                prompts::auditor_with_deferred(
                    &self.plan,
                    phase,
                    &diff,
                    &self.deferred,
                    self.config.audit.small_fix_line_limit,
                ),
                // Phase auditor only ever runs once per phase, so the per-role
                // attempt counter in the log filename stays at 1; the global
                // `attempt` counter still bumps so [`RunState::attempts`]
                // reflects the spend.
                self.attempt_log_path(phase_id, "audit", 1),
            ),
            AuditKind::Sweep { after, pre_texts } => {
                // Resolved: items present in `pre_texts` (unchecked at sweep
                // start) that are now done in the post-dispatch parse. The
                // sweep prompt forbids rewording, so matching by exact text
                // is sound. Remaining: still-unchecked items in the current
                // deferred doc.
                let resolved: Vec<String> = self
                    .deferred
                    .items
                    .iter()
                    .filter(|i| i.done && pre_texts.contains(&i.text))
                    .map(|i| i.text.clone())
                    .collect();
                let remaining: Vec<String> = self
                    .deferred
                    .items
                    .iter()
                    .filter(|i| !i.done)
                    .map(|i| i.text.clone())
                    .collect();
                let stale = self.stale_items();
                (
                    prompts::sweep_auditor(
                        &self.plan,
                        &self.deferred,
                        &after,
                        &diff,
                        &resolved,
                        &remaining,
                        &stale,
                        self.config.audit.small_fix_line_limit as usize,
                    ),
                    // Sweep audits get a sweep-prefix log path so an operator
                    // scanning `.pitboss/play/logs/` can tell sweep audits
                    // apart from regular phase audits at a glance.
                    self.sweep_log_path(&after, "audit", 1),
                )
            }
        };
        let request = AgentRequest {
            role: Role::Auditor,
            model: self.config.models.auditor.clone(),
            system_prompt: prompts::caveman::system_prompt(&self.config.caveman),
            user_prompt,
            workdir: self.workspace.clone(),
            log_path,
            timeout: DEFAULT_AGENT_TIMEOUT,
            env: std::collections::HashMap::new(),
        };

        match self
            .dispatch_and_validate(request, Role::Auditor, plan_path, deferred_path)
            .await?
        {
            ValidationResult::Continue => {}
            ValidationResult::Halt(reason) => return Ok(AuditPassResult::Halted(reason)),
        }

        if let Some(test_runner) = test_runner {
            let outcome = self
                .run_tests(test_runner, phase_id, "tests", total_attempt)
                .await?;
            if !outcome.passed {
                return Ok(AuditPassResult::Halted(HaltReason::TestsFailed(
                    outcome.summary,
                )));
            }
        }

        Ok(AuditPassResult::Continue)
    }

    /// Run the per-item staleness bookkeeping after a sweep dispatch, success
    /// or halt. Reads `post_unchecked_texts` straight from `self.deferred`
    /// (after restoration, on halt paths) so the same code path serves every
    /// sweep exit. Emits [`Event::DeferredItemStale`] for items whose counter
    /// just crossed [`crate::config::SweepConfig::escalate_after`] (transition
    /// only — items already at or above the threshold do not re-emit).
    fn apply_sweep_staleness(&mut self, pre_texts: &HashSet<String>) {
        let post_unchecked_texts: HashSet<String> = self
            .deferred
            .items
            .iter()
            .filter(|i| !i.done)
            .map(|i| i.text.clone())
            .collect();
        let crossed = sweep::update_sweep_staleness(
            &mut self.state.deferred_item_attempts,
            pre_texts,
            &post_unchecked_texts,
            self.config.sweep.escalate_after,
        );
        for (text, attempts) in crossed {
            let _ = self
                .events_tx
                .send(Event::DeferredItemStale { text, attempts });
        }
    }

    fn fold_token_usage(&mut self, role: Role, dispatch: &AgentDispatch) {
        let tokens = &dispatch.outcome_tokens;
        self.state.token_usage.input += tokens.input;
        self.state.token_usage.output += tokens.output;
        let entry = self
            .state
            .token_usage
            .by_role
            .entry(role.as_str().to_string())
            .or_default();
        entry.input += tokens.input;
        entry.output += tokens.output;
        for (k, v) in &tokens.by_role {
            let e = self.state.token_usage.by_role.entry(k.clone()).or_default();
            e.input += v.input;
            e.output += v.output;
        }
        let _ = self
            .events_tx
            .send(Event::UsageUpdated(self.state.token_usage.clone()));
    }

    fn restore_deferred(
        &self,
        deferred_path: &Path,
        pre_bytes: &[u8],
        existed: bool,
    ) -> Result<()> {
        if existed {
            write_atomic(deferred_path, pre_bytes).with_context(|| {
                format!(
                    "runner: restoring {:?} from snapshot after parse failure",
                    deferred_path
                )
            })?;
        } else if deferred_path.exists() {
            std::fs::remove_file(deferred_path).with_context(|| {
                format!(
                    "runner: removing agent-created {:?} after parse failure",
                    deferred_path
                )
            })?;
        }
        Ok(())
    }

    async fn dispatch_agent(&self, request: AgentRequest) -> Result<AgentDispatch> {
        let role = request.role;
        let (mpsc_tx, mpsc_rx) = mpsc::channel(AGENT_EVENT_CHANNEL_CAPACITY);
        let cancel = CancellationToken::new();
        let events_tx = self.events_tx.clone();

        let forward = tokio::spawn(forward_agent_events(mpsc_rx, events_tx));

        let outcome = self
            .agent
            .run(request, mpsc_tx, cancel)
            .await
            .with_context(|| format!("runner: agent {:?} failed to run", self.agent.name()))?;
        let _ = forward.await;

        Ok(AgentDispatch {
            stop_reason: outcome.stop_reason,
            outcome_tokens: outcome.tokens,
            _role: role,
        })
    }
}

/// Clone `doc` and keep only the first `max` pending items (in document
/// order). Already-checked items are filtered out by the sweep prompt
/// renderer, so they're left alone here. `## Deferred phases` are preserved
/// verbatim — the sweep prompt forbids editing them and the auditor cross-
/// checks against the full file.
///
/// Used by [`Runner::run_standalone_sweep`] (and any future
/// `--max-items`-style truncation) to keep a pathological backlog within
/// the agent's effective context window without touching disk: the original
/// `deferred.md` is unchanged, so items not in the prompt view stay pending
/// for the next sweep.
fn clamp_pending_items(doc: &DeferredDoc, max: usize) -> DeferredDoc {
    let mut out = DeferredDoc {
        items: Vec::with_capacity(doc.items.len().min(max + 1)),
        phases: doc.phases.clone(),
    };
    let mut pending_kept = 0usize;
    for item in &doc.items {
        if !item.done {
            if pending_kept >= max {
                continue;
            }
            pending_kept += 1;
        }
        out.items.push(item.clone());
    }
    out
}

/// Canonical serialization of just the `## Deferred phases` block of a
/// [`DeferredDoc`]. Used by the sweep step to detect whether the agent edited
/// any H3 entries — the sweep prompt forbids it, so any difference between the
/// pre- and post-dispatch hash trips a halt and a deferred.md rollback.
fn phases_block_canonical(doc: &DeferredDoc) -> String {
    let phases_only = DeferredDoc {
        items: Vec::new(),
        phases: doc.phases.clone(),
    };
    deferred::serialize(&phases_only)
}

/// Snapshot of the agent dispatch the runner needs after the call returns.
struct AgentDispatch {
    stop_reason: StopReason,
    outcome_tokens: crate::state::TokenUsage,
    _role: Role,
}

/// Outcome of [`Runner::dispatch_and_validate`]. Either continue with the
/// post-dispatch flow, or short-circuit with a halt reason already populated
/// (snapshots restored, tokens folded).
enum ValidationResult {
    Continue,
    Halt(HaltReason),
}

/// Outcome of [`Runner::run_fixer_loop`]. `Passed` means a fixer attempt
/// produced a passing test run; `Halted` carries the reason — either an agent
/// failure during a fixer dispatch, a planning-artifact tamper, or budget
/// exhaustion (in which case the variant is [`HaltReason::TestsFailed`]).
enum FixerLoopResult {
    Passed,
    Halted(HaltReason),
}

/// Outcome of [`Runner::run_auditor_pass`]. `Continue` covers all the keep-
/// going cases (no diff to audit, audit ran and tests still pass); `Halted`
/// carries an agent-side failure, a planning-artifact tamper, or post-audit
/// test breakage. The "audit disabled" case is handled upstream by
/// [`Runner::run_dispatch_pipeline`] only invoking the auditor when the spec
/// carries an [`AuditKind`].
enum AuditPassResult {
    Continue,
    Halted(HaltReason),
}

/// Inputs to [`Runner::run_dispatch_pipeline`]. The pipeline is the shared
/// dispatch → validate → tests → fixer → optional auditor → stage chain that
/// both the per-phase implementer dispatch (today) and the per-sweep
/// implementer dispatch (phase 03) hand off to. Lifetimes tie everything to
/// the caller's stack frame so the caller keeps ownership of the source
/// [`crate::plan::Phase`] and path buffers.
struct DispatchSpec<'a> {
    /// The pre-built agent request to dispatch first. The caller is
    /// responsible for having bumped [`RunState::attempts`] for `phase_id`
    /// before constructing this so the request's `log_path` reflects the
    /// dispatch's attempt number.
    request: AgentRequest,
    /// Phase id under which this dispatch is recorded — drives the fixer
    /// loop's attempt tracking, the auditor's "started" event, and the
    /// post-dispatch test log filename. For phase-driven dispatches this is
    /// the current phase. For sweep dispatches (phase 03) this is the most
    /// recently completed `after_phase`, since `state.attempts` keys on real
    /// phase ids.
    phase_id: PhaseId,
    /// The current phase, when there is one. The fixer loop uses this to
    /// render `prompts::fixer_with_deferred`; sweep dispatches pass `None` to
    /// fall back to `prompts::fixer_for_sweep`.
    phase: Option<&'a crate::plan::Phase>,
    /// Path to `plan.md` for snapshot validation.
    plan_path: &'a Path,
    /// Path to `deferred.md` for snapshot + parse validation.
    deferred_path: &'a Path,
    /// Paths to exclude from `git add` so planning artifacts under
    /// `.pitboss/` never end up in the per-phase commit.
    exclude_paths: &'a [&'a Path],
    /// Whether to run the auditor pass after tests pass. `None` skips the
    /// auditor entirely; `Some(kind)` selects the prompt variant.
    audit: Option<AuditKind<'a>>,
}

/// Selects the auditor prompt variant.
///
/// `Phase` runs the regular auditor against an implementer diff for a plan
/// phase. `Sweep` runs the sweep-specific auditor against a deferred-sweep
/// dispatch; its contract is "for each item the implementer marked `- [x]`,
/// does the diff actually do that work? revert anything unrelated."
enum AuditKind<'a> {
    /// Audit a regular plan-phase implementer dispatch. Renders
    /// [`crate::prompts::auditor_with_deferred`] for the carried phase.
    Phase {
        /// The phase whose implementer diff is under review.
        phase: &'a crate::plan::Phase,
    },
    /// Audit a deferred-sweep dispatch. Renders [`crate::prompts::sweep_auditor`]
    /// with the sweep's resolved / remaining item lists threaded through.
    ///
    /// Resolved / remaining lists are derived inside [`Runner::run_auditor_pass`]
    /// from the post-dispatch parse of `deferred.md` against `pre_texts`, so
    /// the call site doesn't need to defer pipeline construction until after
    /// the implementer dispatch. It can build the spec up front and let the
    /// auditor pass compute the lists once `self.deferred` reflects the
    /// implementer's edits.
    Sweep {
        /// Most recently completed plan phase the sweep fired after. Becomes
        /// the `{after}` substitution and selects the sweep-prefix log path.
        after: PhaseId,
        /// Unchecked-item text snapshot taken before the sweep dispatched.
        /// Used to diff against the post-dispatch deferred state to compute
        /// the resolved / remaining lists fed to the auditor prompt.
        pre_texts: HashSet<String>,
    },
}

/// Outcome of [`Runner::run_dispatch_pipeline`]. The pipeline never commits;
/// `Staged { has_changes }` hands back whether the staged index is non-empty
/// so the caller (today: [`Runner::run_phase_inner`]) can decide whether to
/// commit.
enum PipelineOutcome {
    Halted(HaltReason),
    Staged {
        /// `true` when `git.has_staged_changes()` returned true after the
        /// final stage call — i.e. the dispatch (and any auditor edits)
        /// produced code outside `exclude_paths`. `false` when the only
        /// changes were inside `.pitboss/` and got excluded.
        has_changes: bool,
    },
}

async fn forward_agent_events(mut rx: mpsc::Receiver<AgentEvent>, tx: broadcast::Sender<Event>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::Stdout(line) => {
                let _ = tx.send(Event::AgentStdout(line));
            }
            AgentEvent::Stderr(line) => {
                let _ = tx.send(Event::AgentStderr(line));
            }
            AgentEvent::ToolUse(name) => {
                let _ = tx.send(Event::AgentToolUse(name));
            }
            AgentEvent::TokenDelta(_) => {
                // Token deltas are folded into [`RunState::token_usage`] from
                // the final outcome, not the stream — folding here would
                // double-count any agent that emits both intermediate deltas
                // and a totals report.
            }
        }
    }
}

/// Build a fresh [`RunState`] for a workspace that has not started a run yet.
///
/// `now` is the timestamp used to derive both the run id and the per-run
/// branch (`<config.git.branch_prefix><utc_timestamp>`). Keeping the timestamp
/// explicit makes startup deterministic in tests.
pub fn fresh_run_state(plan: &Plan, config: &Config, now: chrono::DateTime<Utc>) -> RunState {
    let run_id = now.format("%Y%m%dT%H%M%SZ").to_string();
    let branch = git::branch_name(&config.git.branch_prefix, now);
    let mut s = RunState::new(run_id, branch, plan.current_phase.clone());
    s.started_at = now;
    s
}

/// Subscribe to a runner's event stream and print a human-readable line per
/// event to stderr until a terminal event arrives or the channel closes.
///
/// This is the "no TUI" CLI experience: progress is rendered via plain
/// `tracing::info`-style lines so log piping and CI logs work out of the box.
///
/// The runner emits [`Event::RunFinished`] on the success path and
/// [`Event::PhaseHalted`] on the halt path; this function logs the terminal
/// event and then returns. Otherwise the broadcast channel would block
/// forever after `Runner::run()` returns, because the runner itself keeps
/// holding the [`broadcast::Sender`] for post-run lookups.
pub async fn log_events(mut rx: broadcast::Receiver<Event>) {
    use broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(event) => {
                let terminal = matches!(event, Event::RunFinished | Event::PhaseHalted { .. });
                log_event_line(&event);
                if terminal {
                    return;
                }
            }
            Err(RecvError::Closed) => return,
            Err(RecvError::Lagged(n)) => {
                eprintln!("[pitboss] (logger lagged: dropped {n} events)");
            }
        }
    }
}

fn log_event_line(event: &Event) {
    use crate::style::{self, col};
    let c = style::use_color_stderr();

    let fm = col(c, style::BOLD_CYAN, "[pitboss]");

    match event {
        Event::PhaseStarted {
            phase_id,
            title,
            attempt,
        } => {
            let rule = col(c, style::DARK_GRAY, &"─".repeat(60));
            if c {
                eprintln!("{rule}");
            }
            eprintln!(
                "{} {}",
                col(c, style::BOLD_CYAN, "[pitboss]"),
                col(
                    c,
                    style::BOLD_WHITE,
                    &format!("phase {phase_id} ({title}), attempt {attempt}")
                )
            );
            if c {
                eprintln!("{rule}");
            }
        }
        Event::FixerStarted {
            phase_id,
            fixer_attempt,
            attempt,
        } => {
            eprintln!(
                "{fm} {}",
                col(
                    c,
                    style::YELLOW,
                    &format!(
                        "phase {phase_id} fixer attempt {fixer_attempt} (total dispatch {attempt})"
                    )
                )
            );
        }
        Event::AuditorStarted { context, attempt } => {
            let label = match context.kind {
                AuditContextKind::Phase => format!(
                    "phase {} auditor (total dispatch {attempt})",
                    context.phase_id
                ),
                AuditContextKind::Sweep => format!(
                    "sweep after phase {} auditor (total dispatch {attempt})",
                    context.phase_id
                ),
            };
            eprintln!("{fm} {}", col(c, style::BLUE, &label));
        }
        Event::AuditorSkippedNoChanges { context } => {
            let label = match context.kind {
                AuditContextKind::Phase => format!(
                    "phase {} auditor skipped: no code changes to audit",
                    context.phase_id
                ),
                AuditContextKind::Sweep => format!(
                    "sweep after phase {} auditor skipped: no code changes to audit",
                    context.phase_id
                ),
            };
            eprintln!("{fm} {}", col(c, style::DIM, &label));
        }
        Event::AgentStdout(line) => {
            eprintln!("{} {line}", col(c, style::DIM, "[agent]"));
        }
        Event::AgentStderr(line) => {
            eprintln!(
                "{} {}",
                col(c, style::RED, "[agent:err]"),
                col(c, style::RED, line)
            );
        }
        Event::AgentToolUse(name) => {
            eprintln!(
                "{}",
                col(c, style::DARK_GRAY, &format!("[agent:tool] {name}"))
            );
        }
        Event::TestStarted => {
            eprintln!("{fm} {}", col(c, style::MAGENTA, "running tests"));
        }
        Event::TestFinished { passed, summary } => {
            if *passed {
                eprintln!(
                    "{fm} {}",
                    col(c, style::BOLD_GREEN, &format!("tests passed: {summary}"))
                );
            } else {
                eprintln!(
                    "{fm} {}",
                    col(c, style::BOLD_RED, &format!("tests failed: {summary}"))
                );
            }
        }
        Event::TestsSkipped => {
            eprintln!(
                "{fm} {}",
                col(c, style::DIM, "no test runner detected; skipping")
            );
        }
        Event::PhaseCommitted {
            phase_id,
            commit: Some(hash),
        } => {
            eprintln!(
                "{fm} {}",
                col(
                    c,
                    style::GREEN,
                    &format!("phase {phase_id} committed: {hash}")
                )
            );
        }
        Event::PhaseCommitted {
            phase_id,
            commit: None,
        } => {
            eprintln!(
                "{fm} {}",
                col(
                    c,
                    style::DIM,
                    &format!("phase {phase_id} produced no code changes; no commit")
                )
            );
        }
        Event::PhaseHalted { phase_id, reason } => {
            eprintln!(
                "{} {}",
                col(c, style::BOLD_RED, "[pitboss]"),
                col(
                    c,
                    style::BOLD_RED,
                    &format!("phase {phase_id} halted: {reason}")
                )
            );
        }
        Event::RunFinished => {
            let rule = col(c, style::BOLD_GREEN, &"─".repeat(60));
            if c {
                eprintln!("{rule}");
            }
            eprintln!("{}", col(c, style::BOLD_GREEN, "[pitboss] run finished"));
            if c {
                eprintln!("{rule}");
            }
        }
        Event::UsageUpdated(_) => {
            // Snapshot consumed by the TUI; the plain logger doesn't surface
            // running token totals to keep stderr clean.
        }
        Event::SweepStarted {
            after,
            items_pending,
            attempt,
        } => {
            eprintln!(
                "{fm} {}",
                col(
                    c,
                    style::BOLD_CYAN,
                    &format!(
                        "sweep after phase {after} ({items_pending} pending, total dispatch {attempt})"
                    )
                )
            );
        }
        Event::SweepCompleted {
            after,
            resolved,
            commit: Some(hash),
        } => {
            eprintln!(
                "{fm} {}",
                col(
                    c,
                    style::GREEN,
                    &format!("sweep after phase {after} committed: {resolved} resolved ({hash})")
                )
            );
        }
        Event::SweepCompleted {
            after,
            resolved,
            commit: None,
        } => {
            eprintln!(
                "{fm} {}",
                col(
                    c,
                    style::DIM,
                    &format!(
                        "sweep after phase {after}: {resolved} resolved; no code changes to commit"
                    )
                )
            );
        }
        Event::SweepHalted { after, reason } => {
            eprintln!(
                "{} {}",
                col(c, style::BOLD_RED, "[pitboss]"),
                col(
                    c,
                    style::BOLD_RED,
                    &format!("sweep after phase {after} halted: {reason}")
                )
            );
        }
        Event::DeferredItemStale { text, attempts } => {
            eprintln!(
                "{fm} {}",
                col(
                    c,
                    style::BOLD_YELLOW,
                    &format!(
                        "deferred item stale ({attempts} sweep attempts; needs human attention): {text}"
                    )
                )
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    #[test]
    fn fresh_run_state_uses_branch_prefix_and_timestamp() {
        let plan = Plan::new(
            pid("01"),
            vec![crate::plan::Phase {
                id: pid("01"),
                title: "First".into(),
                body: String::new(),
            }],
        );
        let cfg = Config::default();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-29T14:30:22Z")
            .unwrap()
            .with_timezone(&Utc);
        let state = fresh_run_state(&plan, &cfg, now);
        assert_eq!(state.run_id, "20260429T143022Z");
        assert_eq!(state.branch, "pitboss/play/20260429T143022Z");
        assert_eq!(state.started_phase, pid("01"));
        assert_eq!(state.started_at, now);
        assert!(state.completed.is_empty());
    }

    #[test]
    fn halt_reason_display_summaries_are_human_readable() {
        assert_eq!(
            HaltReason::PlanTampered.to_string(),
            "plan.md was modified by the agent"
        );
        assert!(HaltReason::DeferredInvalid("bad".into())
            .to_string()
            .contains("deferred.md"));
        assert!(HaltReason::TestsFailed("nope".into())
            .to_string()
            .contains("tests failed"));
        assert!(HaltReason::AgentFailure("boom".into())
            .to_string()
            .contains("boom"));
    }
}
