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
    /// Fires at most once per phase and only when
    /// [`crate::config::AuditConfig::enabled`] is `true` and the implementer /
    /// fixer dispatches produced staged code changes.
    AuditorStarted {
        /// Phase the auditor is operating on.
        phase_id: PhaseId,
        /// Total agent-dispatch counter at this phase (mirrors
        /// [`crate::state::RunState::attempts`]).
        attempt: u32,
    },
    /// The auditor dispatched without finding code changes worth auditing
    /// (the index was empty after staging excluded paths).
    AuditorSkippedNoChanges {
        /// Phase whose audit was skipped.
        phase_id: PhaseId,
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

    /// Drive the runner until the plan completes or a phase halts.
    ///
    /// Always emits exactly one terminal event before returning:
    /// [`Event::RunFinished`] on the success path, [`Event::PhaseHalted`] on
    /// the halt path. Subscribers (logger, TUI) treat either event as the
    /// signal that no further events will arrive — the broadcast channel
    /// itself does not close because `Runner` keeps owning the sender for
    /// post-run lookups (e.g., PR creation reads `runner.state()`).
    pub async fn run(&mut self) -> Result<RunSummary> {
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
                    let _ = self.events_tx.send(Event::RunFinished);
                    return Ok(RunSummary::Finished);
                }
                PhaseResult::Advanced { .. } => {}
            }
        }
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
        write_atomic(&deferred_path, deferred::serialize(&self.deferred).as_bytes())
            .context("runner: writing deferred.md after sweep")?;

        self.state.completed.push(phase_id.clone());

        let next_phase = self.next_phase_id_after(&phase_id);
        if let Some(ref next) = next_phase {
            self.plan.set_current_phase(next.clone());
            write_atomic(&plan_path, plan::serialize(&self.plan).as_bytes())
                .context("runner: writing plan.md with advanced current_phase")?;
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
                Some(p) => prompts::fixer_with_deferred(
                    &self.plan,
                    p,
                    &last_summary,
                    &self.deferred,
                ),
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

        if diff.trim().is_empty() {
            let _ = self.events_tx.send(Event::AuditorSkippedNoChanges {
                phase_id: phase_id.clone(),
            });
            return Ok(AuditPassResult::Continue);
        }

        if let Some(reason) = self.check_budget() {
            return Ok(AuditPassResult::Halted(reason));
        }

        let total_attempt = self.bump_attempts(phase_id);
        let _ = self.events_tx.send(Event::AuditorStarted {
            phase_id: phase_id.clone(),
            attempt: total_attempt,
        });

        let user_prompt = match audit {
            AuditKind::Phase { phase } => prompts::auditor_with_deferred(
                &self.plan,
                phase,
                &diff,
                &self.deferred,
                self.config.audit.small_fix_line_limit,
            ),
        };
        // Auditor only ever runs once per phase, so the per-role attempt
        // counter in the log filename stays at 1; the global `attempt`
        // counter still bumps so [`RunState::attempts`] reflects the spend.
        let log_path = self.attempt_log_path(phase_id, "audit", 1);
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
/// Phase 04 extends this with a `Sweep` variant that drives a sweep-specific
/// auditor prompt; for now `Phase` is the only constructor and behaves
/// identically to the pre-refactor auditor pass.
enum AuditKind<'a> {
    /// Audit a regular plan-phase implementer dispatch. Renders
    /// [`crate::prompts::auditor_with_deferred`] for the carried phase.
    Phase {
        /// The phase whose implementer diff is under review.
        phase: &'a crate::plan::Phase,
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
        Event::AuditorStarted { phase_id, attempt } => {
            eprintln!(
                "{fm} {}",
                col(
                    c,
                    style::BLUE,
                    &format!("phase {phase_id} auditor (total dispatch {attempt})")
                )
            );
        }
        Event::AuditorSkippedNoChanges { phase_id } => {
            eprintln!(
                "{fm} {}",
                col(
                    c,
                    style::DIM,
                    &format!("phase {phase_id} auditor skipped: no code changes to audit")
                )
            );
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
