//! Grind orchestrator: pick a prompt, dispatch the agent, capture the
//! summary, commit, repeat — sequentially or across parallel worktrees.
//!
//! [`GrindRunner`] wires together the artifacts assembled in phases 01-10:
//! discovered prompts, a [`GrindPlan`], a [`Scheduler`], and an open
//! [`RunDir`]. One [`GrindRunner::run`] call drives the loop until the
//! scheduler is exhausted, the run is drained (one Ctrl-C), or the run is
//! aborted (second Ctrl-C, or any other [`CancellationToken::cancel`]).
//!
//! Phase 11 lifts the runner from "one session at a time" to a real
//! concurrency gate. Each prompt declares whether it is `parallel_safe`; the
//! plan declares its `max_parallel` ceiling. A `tokio::sync::Semaphore` of
//! `max_parallel` permits guards dispatch:
//!
//! - A `parallel_safe: true` prompt grabs one permit, runs in its own
//!   [`SessionWorktree`] off `<run-root>/worktrees/session-NNNN/`, and
//!   fast-forwards the run branch to its session tip on completion.
//! - A `parallel_safe: false` prompt grabs *all* `max_parallel` permits,
//!   effectively serializing it against any in-flight parallel sessions.
//!   It runs in the main workspace exactly the way phase 07 wired it up.
//!
//! The runner is intentionally agnostic to the surface that signals a stop:
//! it takes a [`GrindShutdown`] handle that carries an
//! [`AtomicBool`](std::sync::atomic::AtomicBool) drain flag and a
//! [`CancellationToken`] abort token. The CLI binds those to live `Ctrl-C`
//! events; the integration tests flip them by hand.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::agent::{Agent, AgentEvent, AgentRequest, Role, StopReason};
use crate::config::Config;
use crate::git::{CommitId, Git};
use crate::state::TokenUsage;
use crate::tests as project_tests;

use super::budget::{session_cost_usd, BudgetCheck, BudgetReason, BudgetTracker};
use super::hooks::{run_hook, HookKind, HookOutcome};
use super::plan::{GrindPlan, Hooks, PlanBudgets};
use super::prompt::PromptDoc;
use super::run_dir::{RunDir, RunPaths, SessionRecord, SessionStatus};
use super::scheduler::{Scheduler, SchedulerState};
use super::state::{build_state, RunStatus};
use super::worktree::{
    merge_conflict_summary, merge_scratchpad_into_run, try_commit_session, SessionWorktree,
};

/// Capacity of the grind runner's event broadcast channel. Sized so a slow
/// subscriber falls behind by a few hundred events before lagging; sends are
/// best-effort so a missing or lagging subscriber never blocks the runner.
pub const GRIND_EVENT_CHANNEL_CAPACITY: usize = 256;

/// Threshold at which a budget tips into [`GrindEvent::BudgetWarning`]. Once a
/// run-level cap is at or beyond 80% consumed, the runner emits exactly one
/// warning per budget kind so the TUI can flash without spamming on every
/// session record.
const BUDGET_WARN_FRACTION: f64 = 0.80;

/// Streaming events the grind runner broadcasts to subscribers. Sends are
/// best-effort: a lagging or absent subscriber never blocks the runner.
///
/// Phase 13 wires this into a TUI dashboard. The plain logger keeps using
/// `tracing` and ignores this channel.
#[derive(Debug, Clone)]
pub enum GrindEvent {
    /// A session is about to dispatch its agent. Carries the seq, the prompt
    /// name, and whether the prompt is `parallel_safe` so the dashboard can
    /// distinguish fanned-out worktree sessions from sequential ones.
    SessionStarted {
        /// 1-based session sequence within the run.
        seq: u32,
        /// Name of the prompt being dispatched.
        prompt: String,
        /// Whether the dispatch took the parallel-worktree path.
        parallel_safe: bool,
    },
    /// One line of agent stdout from the dispatched session.
    AgentStdout {
        /// Owning session sequence.
        seq: u32,
        /// Raw line emitted by the agent.
        line: String,
    },
    /// One line of agent stderr from the dispatched session.
    AgentStderr {
        /// Owning session sequence.
        seq: u32,
        /// Raw line emitted by the agent.
        line: String,
    },
    /// Agent invoked a tool inside the dispatched session.
    AgentToolUse {
        /// Owning session sequence.
        seq: u32,
        /// Tool name reported by the agent backend.
        name: String,
    },
    /// A plan-level shell hook just resolved.
    HookFired {
        /// Owning session sequence.
        seq: u32,
        /// Which hook fired.
        kind: HookKind,
        /// Whether the hook resolved as [`HookOutcome::Success`].
        success: bool,
        /// One-line description of the hook outcome (passes through
        /// [`HookOutcome::description`]).
        description: String,
    },
    /// The agent's `$PITBOSS_SUMMARY_FILE` was read for this session.
    SummaryCaptured {
        /// Owning session sequence.
        seq: u32,
        /// Captured summary text.
        summary: String,
    },
    /// A session resolved with a final record. Fired exactly once per
    /// dispatched session.
    SessionFinished {
        /// The final session record about to be persisted.
        record: SessionRecord,
    },
    /// A run-level budget reached at least 80% of its configured cap. Fired at
    /// most once per [`BudgetWarningKind`] per run.
    BudgetWarning {
        /// Which budget tripped the threshold and how far it has consumed.
        kind: BudgetWarningKind,
    },
    /// The scheduler's `next()` call resolved. `pick = None` means the
    /// scheduler is exhausted (or this rotation is gated out and the runner
    /// will exit on the next iteration).
    SchedulerPicked {
        /// The scheduler's rotation counter after the pick.
        rotation: u64,
        /// Picked prompt name, or `None` if the scheduler returned no pick.
        pick: Option<String>,
    },
    /// The run loop exited. Carries the resolved [`GrindStopReason`] so the
    /// TUI can render the final state and the same payload the CLI gets back.
    RunFinished {
        /// Why the loop exited.
        stop_reason: GrindStopReason,
    },
}

/// Which run-level budget tipped past [`BUDGET_WARN_FRACTION`]. Carries the
/// observed counter and the configured cap so the TUI can render the percent
/// without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BudgetWarningKind {
    /// `max_iterations` is at >=80% of cap.
    Iterations {
        /// Sessions dispatched so far.
        used: u32,
        /// Configured cap.
        cap: u32,
    },
    /// `max_tokens` is at >=80% of cap.
    Tokens {
        /// Cumulative tokens (input + output) so far.
        used: u64,
        /// Configured cap.
        cap: u64,
    },
    /// `max_cost_usd` is at >=80% of cap.
    Cost {
        /// Cumulative cost in USD so far.
        used: f64,
        /// Configured cap.
        cap: f64,
    },
    /// Wall clock has consumed >=80% of the (`started_at`, `until`) window.
    Until {
        /// Seconds elapsed since the run started.
        elapsed_secs: i64,
        /// Total seconds in the budget window.
        window_secs: i64,
    },
}

/// Per-budget bookkeeping so the runner emits exactly one warning per kind.
#[derive(Debug, Default)]
struct BudgetWarnFlags {
    iterations: bool,
    tokens: bool,
    cost: bool,
    until: bool,
}

/// Raw markdown standing-instruction block prepended to every grind prompt.
/// Embedded at compile time so users do not have to author it themselves and
/// so the markers can be located, updated, or stripped later without parsing
/// the whole prompt body.
const STANDING_INSTRUCTION_TEMPLATE: &str = include_str!("standing_instruction.md");

/// Marker tag wrapping the auto-injected sessions.md tail in the user-prompt
/// the agent receives. Stable so a downstream parser (or the agent itself) can
/// locate or strip it.
const SESSION_LOG_OPEN: &str = "<!-- pitboss:session-log -->";
const SESSION_LOG_CLOSE: &str = "<!-- /pitboss:session-log -->";
const SCRATCHPAD_OPEN: &str = "<!-- pitboss:scratchpad -->";
const SCRATCHPAD_CLOSE: &str = "<!-- /pitboss:scratchpad -->";

/// Number of trailing `sessions.md` lines auto-injected as context. Bounded so
/// long-running grinds don't quietly consume the agent's whole context window
/// with backlog.
const SESSION_LOG_TAIL_LINES: usize = 50;

/// Default per-session wall-clock cap, applied when the prompt frontmatter
/// leaves `max_session_seconds` unset.
const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(60 * 30);

/// Fixer-role prompt rendered for `verify: true` sessions whose post-dispatch
/// test run fails. Mirrors the runner's fixer prompt in shape but is scoped to
/// the grind context (no plan, no phase, no `deferred.md`). Placeholders are
/// substituted by [`render_grind_fixer_prompt`].
const GRIND_FIXER_PROMPT_TEMPLATE: &str = "You are the fixer agent for pitboss grind. \
The implementer agent just finished session prompt {prompt_name} and the project's test \
suite failed. Your job is to fix the code so the suite passes, without expanding scope.

# Hard rules

1. Never edit anything under `.pitboss/`.
2. Stay focused on the failing tests below. Do not refactor passing code.
3. Default assumption: the code is wrong, not the test. If a test asserts the wrong \
invariant, fix it only when you can articulate why in a comment.

# Original prompt body

````
{prompt_body}
````

# Test output

````
{test_output}
````
";

/// Render the grind fixer prompt for a failing verify cycle. Public for
/// snapshot tests; not part of the supported API surface.
pub fn render_grind_fixer_prompt(prompt_name: &str, prompt_body: &str, test_output: &str) -> String {
    GRIND_FIXER_PROMPT_TEMPLATE
        .replace("{prompt_name}", prompt_name)
        .replace("{prompt_body}", prompt_body)
        .replace("{test_output}", test_output)
}

/// Standing-instruction text rendered into the agent's prompt body. Stable so
/// callers (and tests) can grep for it. Public for snapshot tests; not part of
/// the supported API surface.
pub fn standing_instruction_block() -> &'static str {
    STANDING_INSTRUCTION_TEMPLATE
}

/// Two-stage shutdown handle.
///
/// `drain` flips on the first Ctrl-C: the runner finishes in-flight sessions
/// cleanly, then exits. `abort` flips on the second Ctrl-C (or any
/// caller-driven cancel): the in-flight agents are cancelled and their
/// sessions are recorded with [`SessionStatus::Aborted`].
///
/// Cloning is cheap — both handles share state across clones.
#[derive(Debug, Clone)]
pub struct GrindShutdown {
    drain: Arc<AtomicBool>,
    abort: CancellationToken,
}

impl GrindShutdown {
    /// Build a shutdown handle with both signals cleared.
    pub fn new() -> Self {
        Self {
            drain: Arc::new(AtomicBool::new(false)),
            abort: CancellationToken::new(),
        }
    }

    /// Has the drain signal fired?
    pub fn is_draining(&self) -> bool {
        self.drain.load(Ordering::Relaxed)
    }

    /// Trip the drain signal. Idempotent.
    pub fn drain(&self) {
        self.drain.store(true, Ordering::Relaxed);
    }

    /// Trip the abort signal. Idempotent. Implicitly trips drain too so the
    /// outer loop exits regardless of which path saw the abort first.
    pub fn abort(&self) {
        self.drain.store(true, Ordering::Relaxed);
        self.abort.cancel();
    }

    /// Borrow the cancel token the agent dispatch should honor.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.abort
    }
}

impl Default for GrindShutdown {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a [`GrindRunner::run`] call returned.
#[derive(Debug, Clone, PartialEq)]
pub enum GrindStopReason {
    /// The scheduler had no further prompts to dispatch.
    Completed,
    /// The drain signal fired between sessions; the in-flight session (if any)
    /// finished cleanly.
    Drained,
    /// The abort signal fired during a session; the session was recorded as
    /// [`SessionStatus::Aborted`].
    Aborted,
    /// A run-level budget tripped. Carries the exhausted reason for log /
    /// CLI output.
    BudgetExhausted(BudgetReason),
    /// The consecutive-failure escape valve fired (see
    /// [`crate::config::GrindConfig::consecutive_failure_limit`]).
    ConsecutiveFailureLimit {
        /// The configured limit that was just reached.
        limit: u32,
    },
}

/// Outcome of a full [`GrindRunner::run`] invocation.
#[derive(Debug, Clone)]
pub struct GrindRunOutcome {
    /// The id of the run on disk under `.pitboss/grind/<run-id>/`.
    pub run_id: String,
    /// The git branch the runner committed on.
    pub branch: String,
    /// All session records appended during this run, in completion order.
    pub sessions: Vec<SessionRecord>,
    /// Why the loop exited.
    pub stop_reason: GrindStopReason,
}

/// Grind orchestrator. See module docs.
pub struct GrindRunner<A: Agent, G: Git> {
    workspace: PathBuf,
    config: Arc<Config>,
    run_id: String,
    branch: String,
    plan: GrindPlan,
    scheduler: Scheduler,
    run_dir: RunDir,
    agent: Arc<A>,
    git: Arc<G>,
    next_seq: u32,
    budgets: PlanBudgets,
    consecutive_failure_limit: u32,
    started_at: DateTime<Utc>,
    initial_budget: super::budget::BudgetSnapshot,
    /// Serializes operations that touch the run branch from the main
    /// workspace's checkout: ff-merge of session branches, scratchpad merge,
    /// and the post-merge cleanup. Sequential sessions also hold this lock
    /// for the duration of their commit step so a parallel session waiting
    /// on the run branch can't interleave.
    run_branch_lock: Arc<TokioMutex<()>>,
    /// Broadcast channel for [`GrindEvent`]s. Subscribers (the TUI) attach
    /// via [`GrindRunner::subscribe`]; the runner keeps the sender so
    /// post-run lookups can still emit events without re-creating the
    /// channel.
    events_tx: broadcast::Sender<GrindEvent>,
}

impl<A: Agent + 'static, G: Git + 'static> GrindRunner<A, G> {
    /// Build a runner ready to dispatch its first session. Caller has already
    /// created the per-run branch and checked it out.
    ///
    /// `budgets` holds the run-wide caps already resolved from
    /// `pitboss.toml`'s `[grind.budgets]`, the plan's `PlanBudgets`, and any
    /// CLI overrides via [`crate::grind::resolve_budgets`].
    /// `consecutive_failure_limit` defaults to
    /// [`crate::config::GrindConfig::consecutive_failure_limit`] (`3`) and
    /// trips [`GrindStopReason::ConsecutiveFailureLimit`] once that many
    /// failed sessions land in a row.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace: PathBuf,
        config: Config,
        run_id: String,
        branch: String,
        plan: GrindPlan,
        prompts: BTreeMap<String, PromptDoc>,
        run_dir: RunDir,
        agent: A,
        git: G,
        budgets: PlanBudgets,
        consecutive_failure_limit: u32,
    ) -> Self {
        let scheduler = Scheduler::new(plan.clone(), prompts);
        let (events_tx, _) = broadcast::channel(GRIND_EVENT_CHANNEL_CAPACITY);
        Self {
            workspace,
            config: Arc::new(config),
            run_id,
            branch,
            plan,
            scheduler,
            run_dir,
            agent: Arc::new(agent),
            git: Arc::new(git),
            next_seq: 1,
            budgets,
            consecutive_failure_limit,
            started_at: Utc::now(),
            initial_budget: super::budget::BudgetSnapshot::default(),
            run_branch_lock: Arc::new(TokioMutex::new(())),
            events_tx,
        }
    }

    /// Build a runner from a previously persisted resume state. The caller
    /// has already validated the prompt set, opened the existing run
    /// directory, and re-checked out the run branch.
    ///
    /// `scheduler_state` is fed straight into [`Scheduler::with_state`] so
    /// the next [`Scheduler::next`] call deterministically returns the same
    /// prompt the original loop would have. `initial_budget` seeds the
    /// in-memory [`BudgetTracker`] so cumulative caps stay accurate across
    /// the resume boundary.
    #[allow(clippy::too_many_arguments)]
    pub fn resume(
        workspace: PathBuf,
        config: Config,
        run_id: String,
        branch: String,
        plan: GrindPlan,
        prompts: BTreeMap<String, PromptDoc>,
        run_dir: RunDir,
        agent: A,
        git: G,
        budgets: PlanBudgets,
        consecutive_failure_limit: u32,
        scheduler_state: SchedulerState,
        initial_budget: super::budget::BudgetSnapshot,
        last_session_seq: u32,
        started_at: DateTime<Utc>,
    ) -> Self {
        let scheduler = Scheduler::with_state(plan.clone(), prompts, scheduler_state);
        let (events_tx, _) = broadcast::channel(GRIND_EVENT_CHANNEL_CAPACITY);
        Self {
            workspace,
            config: Arc::new(config),
            run_id,
            branch,
            plan,
            scheduler,
            run_dir,
            agent: Arc::new(agent),
            git: Arc::new(git),
            next_seq: last_session_seq.saturating_add(1),
            budgets,
            consecutive_failure_limit,
            started_at,
            initial_budget,
            run_branch_lock: Arc::new(TokioMutex::new(())),
            events_tx,
        }
    }

    /// Workspace this runner is rooted at.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Run id under `.pitboss/grind/<run-id>/`.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Branch the runner is committing on.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Current plan.
    pub fn plan(&self) -> &GrindPlan {
        &self.plan
    }

    /// Wall-clock instant the run was originally started. Resumed runs
    /// inherit the original start; fresh runs use the construction time.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    /// Borrow the run-wide budgets (resolved from `pitboss.toml`, plan, and
    /// CLI flags before the runner was built). The TUI footer reads this for
    /// the budget headers.
    pub fn budgets(&self) -> &PlanBudgets {
        &self.budgets
    }

    /// Borrow the dispatching agent. The TUI uses this for [`Agent::name`].
    pub fn agent(&self) -> &A {
        self.agent.as_ref()
    }

    /// Subscribe to the runner's [`GrindEvent`] stream. Returns a fresh
    /// receiver each call; existing subscribers are unaffected. Lagging
    /// subscribers see [`broadcast::error::RecvError::Lagged`] and miss
    /// intermediate events — they never block the runner.
    pub fn subscribe(&self) -> broadcast::Receiver<GrindEvent> {
        self.events_tx.subscribe()
    }

    /// Drive the loop. Returns once the scheduler exhausts, drain trips,
    /// abort trips, a run-level budget exhausts, or the consecutive-failure
    /// limit is reached.
    pub async fn run(&mut self, shutdown: GrindShutdown) -> Result<GrindRunOutcome> {
        let mut sessions: Vec<SessionRecord> = Vec::new();
        let mut tracker = BudgetTracker::from_snapshot(
            self.budgets.clone(),
            self.consecutive_failure_limit,
            self.initial_budget,
        );
        let mut stop_reason = GrindStopReason::Completed;
        let max_parallel = self.plan.max_parallel.max(1);
        let semaphore = Arc::new(Semaphore::new(max_parallel as usize));
        let mut tasks: JoinSet<Result<SessionRecord>> = JoinSet::new();
        let mut max_completed_seq: u32 = self.next_seq.saturating_sub(1);
        let mut warn_flags = BudgetWarnFlags::default();

        // Stamp the initial state.json so a resume target exists from the
        // first moment a run is on disk, even if the host process dies before
        // a single session lands.
        if let Err(e) = self.write_state(&tracker, max_completed_seq, RunStatus::Active) {
            warn!(
                run_id = %self.run_id,
                error = %format!("{e:#}"),
                "grind: initial state.json write failed"
            );
        }

        'outer: loop {
            if shutdown.is_draining() {
                stop_reason = if shutdown.cancel_token().is_cancelled() {
                    GrindStopReason::Aborted
                } else {
                    GrindStopReason::Drained
                };
                break;
            }

            // Pre-flight budget check so we never start a session that would
            // immediately blow a cap that was already reached by a previous
            // session.
            if let BudgetCheck::Exhausted(reason) = tracker.check() {
                info!(
                    run_id = %self.run_id,
                    reason = %reason,
                    "grind: BudgetExhausted (pre-dispatch)"
                );
                stop_reason = GrindStopReason::BudgetExhausted(reason);
                break;
            }

            if tracker.consecutive_failure_limit_reached() {
                warn!(
                    run_id = %self.run_id,
                    limit = self.consecutive_failure_limit,
                    "grind: consecutive-failure limit reached"
                );
                stop_reason = GrindStopReason::ConsecutiveFailureLimit {
                    limit: self.consecutive_failure_limit,
                };
                break;
            }

            let pick = self.scheduler.next();
            let _ = self.events_tx.send(GrindEvent::SchedulerPicked {
                rotation: self.scheduler.state().rotation,
                pick: pick.as_ref().map(|p| p.meta.name.clone()),
            });
            let Some(prompt) = pick else {
                break;
            };

            let seq = self.next_seq;
            self.next_seq += 1;

            // `parallel_safe: true` takes one permit; everything else takes
            // every permit so it can't overlap any concurrent session.
            let permits_needed = if prompt.meta.parallel_safe {
                1
            } else {
                max_parallel
            };

            let permit_outcome = self
                .acquire_permit(
                    semaphore.clone(),
                    permits_needed,
                    &mut tasks,
                    &mut sessions,
                    &mut tracker,
                    &mut max_completed_seq,
                    &mut warn_flags,
                    &shutdown,
                )
                .await?;
            let permit = match permit_outcome {
                AcquireOutcome::Got(p) => p,
                AcquireOutcome::ShutdownTripped => {
                    stop_reason = if shutdown.cancel_token().is_cancelled() {
                        GrindStopReason::Aborted
                    } else {
                        GrindStopReason::Drained
                    };
                    break 'outer;
                }
                AcquireOutcome::BudgetTripped(reason) => {
                    info!(
                        run_id = %self.run_id,
                        reason = %reason,
                        "grind: BudgetExhausted (mid-acquire)"
                    );
                    stop_reason = GrindStopReason::BudgetExhausted(reason);
                    break 'outer;
                }
                AcquireOutcome::ConsecutiveFailures => {
                    warn!(
                        run_id = %self.run_id,
                        limit = self.consecutive_failure_limit,
                        "grind: consecutive-failure limit reached"
                    );
                    stop_reason = GrindStopReason::ConsecutiveFailureLimit {
                        limit: self.consecutive_failure_limit,
                    };
                    break 'outer;
                }
            };

            // Now committed to dispatch — bump the scheduler so the next
            // pick reflects this run.
            self.scheduler.record_run(&prompt.meta.name);

            info!(
                run_id = %self.run_id,
                seq,
                prompt = %prompt.meta.name,
                parallel_safe = prompt.meta.parallel_safe,
                "grind: dispatching session"
            );

            let _ = self.events_tx.send(GrindEvent::SessionStarted {
                seq,
                prompt: prompt.meta.name.clone(),
                parallel_safe: prompt.meta.parallel_safe,
            });

            let input = self
                .prepare_session_input(seq, prompt, permit, &shutdown)
                .await
                .with_context(|| format!("grind: preparing session {seq}"))?;
            tasks.spawn(run_session_task(input));
        }

        // Drain in-flight tasks.
        while let Some(res) = tasks.join_next().await {
            match res {
                Ok(Ok(rec)) => self.handle_completion(
                    rec,
                    &mut sessions,
                    &mut tracker,
                    &mut max_completed_seq,
                    &mut warn_flags,
                )?,
                Ok(Err(e)) => return Err(e),
                Err(je) => {
                    return Err(anyhow::anyhow!("session task panicked: {je}"));
                }
            }
        }

        // Aborted sessions in the drain phase should propagate as Aborted
        // stop reason if we weren't already Aborted/Drained.
        if matches!(stop_reason, GrindStopReason::Completed)
            && sessions.iter().any(|r| r.status == SessionStatus::Aborted)
        {
            stop_reason = GrindStopReason::Aborted;
        }

        // Stamp the terminal state.json.
        let terminal_status = match &stop_reason {
            GrindStopReason::Completed => RunStatus::Completed,
            GrindStopReason::Drained | GrindStopReason::Aborted => RunStatus::Aborted,
            GrindStopReason::BudgetExhausted(_)
            | GrindStopReason::ConsecutiveFailureLimit { .. } => RunStatus::Failed,
        };
        if let Err(e) = self.write_state(&tracker, max_completed_seq, terminal_status) {
            warn!(
                run_id = %self.run_id,
                error = %format!("{e:#}"),
                "grind: terminal state.json write failed"
            );
        }

        let _ = self.events_tx.send(GrindEvent::RunFinished {
            stop_reason: stop_reason.clone(),
        });
        Ok(GrindRunOutcome {
            run_id: self.run_id.clone(),
            branch: self.branch.clone(),
            sessions,
            stop_reason,
        })
    }

    /// Wait until `permits_needed` permits are available, draining session
    /// completions in the meantime so each finished record lands in the
    /// session log + state.json before the next dispatch is committed.
    #[allow(clippy::too_many_arguments)]
    async fn acquire_permit(
        &self,
        semaphore: Arc<Semaphore>,
        permits_needed: u32,
        tasks: &mut JoinSet<Result<SessionRecord>>,
        sessions: &mut Vec<SessionRecord>,
        tracker: &mut BudgetTracker,
        max_completed_seq: &mut u32,
        warn_flags: &mut BudgetWarnFlags,
        shutdown: &GrindShutdown,
    ) -> Result<AcquireOutcome> {
        loop {
            if shutdown.is_draining() {
                return Ok(AcquireOutcome::ShutdownTripped);
            }

            if let Ok(p) = semaphore.clone().try_acquire_many_owned(permits_needed) {
                return Ok(AcquireOutcome::Got(p));
            }

            if tasks.is_empty() {
                // Permits requested exceed the configured ceiling — block
                // unconditionally so the test for `permits_needed >
                // max_parallel` still resolves rather than spinning.
                return Ok(AcquireOutcome::Got(
                    semaphore.acquire_many_owned(permits_needed).await?,
                ));
            }

            let Some(res) = tasks.join_next().await else {
                continue;
            };
            match res {
                Ok(Ok(rec)) => {
                    self.handle_completion(
                        rec,
                        sessions,
                        tracker,
                        max_completed_seq,
                        warn_flags,
                    )?
                }
                Ok(Err(e)) => return Err(e),
                Err(je) => return Err(anyhow::anyhow!("session task panicked: {je}")),
            }

            if let BudgetCheck::Exhausted(reason) = tracker.check() {
                return Ok(AcquireOutcome::BudgetTripped(reason));
            }
            if tracker.consecutive_failure_limit_reached() {
                return Ok(AcquireOutcome::ConsecutiveFailures);
            }
        }
    }

    fn handle_completion(
        &self,
        record: SessionRecord,
        sessions: &mut Vec<SessionRecord>,
        tracker: &mut BudgetTracker,
        max_completed_seq: &mut u32,
        warn_flags: &mut BudgetWarnFlags,
    ) -> Result<()> {
        let seq = record.seq;
        self.run_dir
            .log()
            .append(&record)
            .with_context(|| format!("grind: appending session {seq} record to log"))?;
        tracker.record_session(&record);
        let _ = self
            .events_tx
            .send(GrindEvent::SessionFinished { record: record.clone() });
        if seq > *max_completed_seq {
            *max_completed_seq = seq;
        }
        sessions.push(record);
        self.emit_budget_warnings(tracker, warn_flags, Utc::now());
        if let Err(e) = self.write_state(tracker, *max_completed_seq, RunStatus::Active) {
            warn!(
                run_id = %self.run_id,
                seq,
                error = %format!("{e:#}"),
                "grind: state.json write failed"
            );
        }
        Ok(())
    }

    /// Emit a [`GrindEvent::BudgetWarning`] for each configured budget that
    /// just crossed [`BUDGET_WARN_FRACTION`]. Pure on `tracker` and
    /// `warn_flags`; idempotent — once a flag is set the runner never re-emits
    /// for that budget.
    fn emit_budget_warnings(
        &self,
        tracker: &BudgetTracker,
        flags: &mut BudgetWarnFlags,
        now: DateTime<Utc>,
    ) {
        if let Some(cap) = self.budgets.max_iterations {
            if !flags.iterations
                && cap > 0
                && fraction_used_u64(u64::from(tracker.iterations()), u64::from(cap))
                    >= BUDGET_WARN_FRACTION
            {
                flags.iterations = true;
                let _ = self.events_tx.send(GrindEvent::BudgetWarning {
                    kind: BudgetWarningKind::Iterations {
                        used: tracker.iterations(),
                        cap,
                    },
                });
            }
        }
        if let Some(cap) = self.budgets.max_tokens {
            let used = tracker.total_tokens();
            if !flags.tokens && cap > 0 && fraction_used_u64(used, cap) >= BUDGET_WARN_FRACTION {
                flags.tokens = true;
                let _ = self.events_tx.send(GrindEvent::BudgetWarning {
                    kind: BudgetWarningKind::Tokens { used, cap },
                });
            }
        }
        if let Some(cap) = self.budgets.max_cost_usd {
            let used = tracker.total_cost_usd();
            if !flags.cost && cap > 0.0 && (used / cap) >= BUDGET_WARN_FRACTION {
                flags.cost = true;
                let _ = self.events_tx.send(GrindEvent::BudgetWarning {
                    kind: BudgetWarningKind::Cost { used, cap },
                });
            }
        }
        if let Some(until) = self.budgets.until {
            let window = (until - self.started_at).num_seconds();
            let elapsed = (now - self.started_at).num_seconds();
            if !flags.until && window > 0 {
                let frac = (elapsed as f64) / (window as f64);
                if frac >= BUDGET_WARN_FRACTION {
                    flags.until = true;
                    let _ = self.events_tx.send(GrindEvent::BudgetWarning {
                        kind: BudgetWarningKind::Until {
                            elapsed_secs: elapsed,
                            window_secs: window,
                        },
                    });
                }
            }
        }
    }

    async fn prepare_session_input(
        &self,
        seq: u32,
        prompt: PromptDoc,
        permit: OwnedSemaphorePermit,
        shutdown: &GrindShutdown,
    ) -> Result<SessionTaskInput<A, G>> {
        let transcript_path = self.run_dir.paths().transcript_for(seq);
        let summary_path = self
            .run_dir
            .paths()
            .root
            .join(format!(".pitboss-summary-{seq:04}.txt"));
        // Make sure the summary path is empty so a stale value from a prior
        // session can never be misread as the agent's current output.
        let _ = std::fs::remove_file(&summary_path);

        let session_log_tail = self
            .read_session_log_tail()
            .unwrap_or_else(|e| format!("(failed to read sessions.md: {e})"));
        let scratchpad_seed = self
            .run_dir
            .scratchpad()
            .read()
            .unwrap_or_else(|e| format!("(failed to read scratchpad: {e})"));

        let mut base_env: HashMap<String, String> = HashMap::new();
        base_env.insert("PITBOSS_RUN_ID".into(), self.run_id.clone());
        base_env.insert("PITBOSS_PROMPT_NAME".into(), prompt.meta.name.clone());
        base_env.insert(
            "PITBOSS_SUMMARY_FILE".into(),
            summary_path.display().to_string(),
        );
        base_env.insert("PITBOSS_SESSION_SEQ".into(), seq.to_string());

        let (workdir_for_agent, scratchpad_path_for_agent, worktree_opt) =
            if prompt.meta.parallel_safe {
                let wt = SessionWorktree::create(
                    &*self.git,
                    self.run_dir.paths(),
                    &self.run_id,
                    &self.branch,
                    seq,
                    &scratchpad_seed,
                )
                .await
                .with_context(|| format!("grind: creating worktree for session {seq}"))?;
                let path = wt.path().to_path_buf();
                let pad = wt.scratchpad_path().to_path_buf();
                (path, pad, Some(wt))
            } else {
                let pad = self.run_dir.scratchpad().path_for_agent().to_path_buf();
                (self.workspace.clone(), pad, None)
            };
        base_env.insert(
            "PITBOSS_SCRATCHPAD".into(),
            scratchpad_path_for_agent.display().to_string(),
        );
        base_env.insert(
            "PITBOSS_WORKTREE".into(),
            workdir_for_agent.display().to_string(),
        );
        // Parallel sessions: point cargo at the main workspace's `target/`
        // so the worktree's `cargo` invocations share the incremental cache
        // instead of paying a full rebuild. Sequential sessions already run
        // in the main workspace so the override would be a no-op.
        if worktree_opt.is_some() {
            base_env.insert(
                "CARGO_TARGET_DIR".into(),
                self.workspace.join("target").display().to_string(),
            );
        }

        Ok(SessionTaskInput {
            repo_root: self.workspace.clone(),
            workdir_for_agent,
            config: self.config.clone(),
            run_id: self.run_id.clone(),
            run_branch: self.branch.clone(),
            plan_hooks: self.plan.hooks.clone(),
            run_paths: self.run_dir.paths().clone(),
            transcript_path,
            summary_path,
            seq,
            prompt,
            agent: self.agent.clone(),
            repo_git: self.git.clone(),
            worktree: worktree_opt,
            run_branch_lock: self.run_branch_lock.clone(),
            shutdown: shutdown.clone(),
            permit,
            session_log_tail,
            scratchpad_seed,
            base_env,
            events_tx: self.events_tx.clone(),
        })
    }

    fn write_state(
        &self,
        tracker: &BudgetTracker,
        last_session_seq: u32,
        status: RunStatus,
    ) -> Result<()> {
        let prompt_names: Vec<String> = self
            .plan
            .prompts
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let state = build_state(
            self.run_id.clone(),
            self.branch.clone(),
            self.plan.name.clone(),
            prompt_names,
            self.scheduler.state().clone(),
            tracker.snapshot(),
            last_session_seq,
            self.started_at,
            status,
        );
        state.write(self.run_dir.paths())
    }

    fn read_session_log_tail(&self) -> Result<String> {
        let path = &self.run_dir.paths().sessions_md;
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("grind: reading {:?}", path)))
            }
        };
        Ok(tail_lines(&raw, SESSION_LOG_TAIL_LINES))
    }
}

/// Resolution of [`GrindRunner::acquire_permit`]. Distinguishes the various
/// "could not acquire" branches so the main loop maps each to the right
/// [`GrindStopReason`].
enum AcquireOutcome {
    Got(OwnedSemaphorePermit),
    ShutdownTripped,
    BudgetTripped(BudgetReason),
    ConsecutiveFailures,
}

/// Bundle of everything a spawned session task needs. Owned/`Arc` so the
/// task body has no borrow back to the runner.
struct SessionTaskInput<A: Agent, G: Git> {
    /// Path of the main workspace. Used for `relative_to` on transcripts and
    /// for ff-merge into the run branch.
    repo_root: PathBuf,
    /// Where the agent should run — main workspace for sequential, the
    /// session worktree for parallel.
    workdir_for_agent: PathBuf,
    config: Arc<Config>,
    run_id: String,
    run_branch: String,
    plan_hooks: Hooks,
    run_paths: RunPaths,
    transcript_path: PathBuf,
    summary_path: PathBuf,
    seq: u32,
    prompt: PromptDoc,
    agent: Arc<A>,
    repo_git: Arc<G>,
    worktree: Option<SessionWorktree>,
    run_branch_lock: Arc<TokioMutex<()>>,
    shutdown: GrindShutdown,
    permit: OwnedSemaphorePermit,
    session_log_tail: String,
    /// Snapshot of the run-level scratchpad at session start. Embedded into
    /// the agent's user prompt and used as the seed for the per-session
    /// scratchpad merge in parallel mode.
    scratchpad_seed: String,
    base_env: HashMap<String, String>,
    /// Broadcast handle the session uses to publish [`GrindEvent`]s as the
    /// dispatch progresses. Cloned from the runner at task spawn.
    events_tx: broadcast::Sender<GrindEvent>,
}

/// Body of one dispatched session. Returns the resulting [`SessionRecord`];
/// the runner appends it to the log and folds it into the budget tracker.
async fn run_session_task<A: Agent + 'static, G: Git + 'static>(
    input: SessionTaskInput<A, G>,
) -> Result<SessionRecord> {
    let SessionTaskInput {
        repo_root,
        workdir_for_agent,
        config,
        run_id,
        run_branch,
        plan_hooks,
        run_paths,
        transcript_path,
        summary_path,
        seq,
        prompt,
        agent,
        repo_git,
        worktree,
        run_branch_lock,
        shutdown,
        permit,
        session_log_tail,
        scratchpad_seed,
        base_env,
        events_tx,
    } = input;

    let started_at = Utc::now();
    let transcript_rel = relative_to(&repo_root, &transcript_path);

    let hook_timeout = Duration::from_secs(config.grind.hook_timeout_secs.max(1));
    let hook_passthrough: Vec<String> = config.grind.hook_env_passthrough.clone();

    // ---- pre_session hook ---------------------------------------------
    let mut skip_dispatch_reason: Option<String> = None;
    if let Some(cmd) = plan_hooks.pre_session.as_deref() {
        let mut env = base_env.clone();
        env.insert("PITBOSS_SESSION_PROMPT".into(), prompt.meta.name.clone());
        let outcome = run_hook(
            HookKind::PreSession,
            cmd,
            &env,
            hook_timeout,
            &transcript_path,
            &hook_passthrough,
        )
        .await;
        emit_hook_event(&events_tx, seq, HookKind::PreSession, &outcome);
        if !outcome.is_success() {
            warn!(
                run_id = %run_id,
                seq,
                outcome = %outcome.description(),
                "grind: pre_session hook failed; skipping dispatch"
            );
            skip_dispatch_reason = Some(format!("pre_session hook {}", outcome.description()));
        }
    }

    let mut status: SessionStatus;
    let mut summary: String;
    let mut commit: Option<CommitId> = None;
    let mut tokens: TokenUsage = TokenUsage::default();
    let mut cost_usd: f64 = 0.0;
    let ended_at: DateTime<Utc>;

    if let Some(reason) = skip_dispatch_reason {
        status = SessionStatus::Error;
        summary = reason;
        ended_at = Utc::now();
    } else {
        let user_prompt = compose_user_prompt(
            STANDING_INSTRUCTION_TEMPLATE,
            &session_log_tail,
            &scratchpad_seed,
            &prompt.body,
        );

        let timeout = prompt
            .meta
            .max_session_seconds
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_SESSION_TIMEOUT);

        let model = config.models.implementer.clone();
        let request = AgentRequest {
            role: Role::Implementer,
            model: model.clone(),
            system_prompt: String::new(),
            user_prompt,
            workdir: workdir_for_agent.clone(),
            log_path: transcript_path.clone(),
            timeout,
            env: base_env.clone(),
        };

        let mut summary_override: Option<String> = None;
        let dispatch = match tokio::time::timeout(
            timeout,
            dispatch_agent_with_events(
                &*agent,
                request,
                shutdown.cancel_token(),
                Some((events_tx.clone(), seq)),
            ),
        )
        .await
        {
            Ok(res) => res?,
            Err(_) => {
                warn!(
                    run_id = %run_id,
                    seq,
                    timeout_secs = timeout.as_secs(),
                    "grind: per-prompt timeout fired"
                );
                summary_override = Some(format!(
                    "session exceeded max_session_seconds ({}s)",
                    timeout.as_secs()
                ));
                AgentDispatch {
                    stop_reason: StopReason::Timeout,
                    tokens: TokenUsage::default(),
                }
            }
        };
        ended_at = Utc::now();

        status = match &dispatch.stop_reason {
            StopReason::Completed => SessionStatus::Ok,
            StopReason::Timeout => SessionStatus::Timeout,
            StopReason::Cancelled => SessionStatus::Aborted,
            StopReason::Error(_) => SessionStatus::Error,
        };

        cost_usd = session_cost_usd(
            &config,
            &model,
            dispatch.tokens.input,
            dispatch.tokens.output,
        );

        if status == SessionStatus::Ok {
            if let Some(cap) = prompt.meta.max_session_cost_usd {
                if cost_usd > cap {
                    warn!(
                        run_id = %run_id,
                        seq,
                        cost = cost_usd,
                        cap,
                        "grind: per-prompt max_session_cost_usd exceeded"
                    );
                    status = SessionStatus::Error;
                    summary_override = Some(format!(
                        "session exceeded max_session_cost_usd: ${cost_usd:.4} > ${cap:.4}"
                    ));
                }
            }
        }

        summary = match summary_override {
            Some(s) => s,
            None => read_summary_or_fallback(&summary_path),
        };
        let _ = events_tx.send(GrindEvent::SummaryCaptured {
            seq,
            summary: summary.clone(),
        });

        let mut verify_extra_tokens = TokenUsage::default();
        if status == SessionStatus::Ok && prompt.meta.verify {
            // Parallel sessions run in their own worktree; pointing
            // CARGO_TARGET_DIR back at the main workspace's `target/` lets
            // them share the cargo cache instead of paying a full rebuild
            // per worktree. Sequential sessions live in the main workspace
            // already, so the override is unnecessary.
            let shared_target_dir = worktree
                .as_ref()
                .map(|_| repo_root.join("target"));
            let verify = verify_with_fixer_loop(
                seq,
                &prompt,
                &workdir_for_agent,
                &config,
                &transcript_path,
                shared_target_dir.as_deref(),
                &*agent,
                shutdown.cancel_token(),
                &base_env,
            )
            .await?;
            status = verify.status;
            if let Some(s) = verify.summary_override {
                summary = s;
            }
            verify_extra_tokens = verify.extra_tokens;
            cost_usd += verify.extra_cost_usd;
        }

        // Commit + stash. Sequential and parallel sessions share the same
        // commit / stash logic but run it against different git handles —
        // sequential against the workspace-rooted runner git, parallel
        // against a worktree-scoped ShellGit owned by the SessionWorktree.
        // Parallel sessions delegate the whole sync → commit → ff-merge →
        // stash dance to [`SessionWorktree::merge_into`], which holds the
        // run-branch lock for the entire window so a sibling session cannot
        // interleave between the steps.
        if let Some(wt) = &worktree {
            let outcome = wt
                .merge_into(
                    &*repo_git,
                    &run_branch,
                    &run_branch_lock,
                    &prompt,
                    &run_id,
                    status,
                    summary.clone(),
                )
                .await
                .with_context(|| format!("grind: merge_into for session {seq}"))?;
            status = outcome.status;
            summary = outcome.summary;
            commit = outcome.commit;
        } else {
            // Sequential: hold the run-branch lock so a concurrent parallel
            // session cannot ff-merge while we're staging / committing.
            let _guard = run_branch_lock.lock().await;
            let pitboss_rel = Path::new(".pitboss");
            let sequential_exclusions: [&Path; 1] = [pitboss_rel];
            commit = match status {
                SessionStatus::Ok | SessionStatus::Error => {
                    try_commit_session(&*repo_git, seq, &prompt, &run_id, &sequential_exclusions)
                        .await?
                }
                _ => None,
            };
            let stash_label = format!("grind/{}/session-{:04}-leftover", run_id, seq);
            match repo_git.stash_push(&stash_label, &sequential_exclusions).await {
                Ok(true) => {
                    warn!(
                        run_id = %run_id,
                        seq,
                        stash = %stash_label,
                        "grind: leftover changes stashed"
                    );
                    if status == SessionStatus::Ok {
                        status = SessionStatus::Dirty;
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(
                        run_id = %run_id,
                        seq,
                        error = %format!("{e:#}"),
                        "grind: stash_push failed — treating as merge conflict"
                    );
                    if status == SessionStatus::Ok || status == SessionStatus::Dirty {
                        status = SessionStatus::Error;
                        summary = merge_conflict_summary(&prompt.meta.name, &e);
                    }
                }
            }
        }

        tokens = dispatch.tokens;
        // Fold any tokens consumed by the verify cycle's fixer dispatches so
        // the session record carries a single, accurate tokens / cost figure
        // covering the implementer plus every fixer attempt.
        tokens.input = tokens.input.saturating_add(verify_extra_tokens.input);
        tokens.output = tokens.output.saturating_add(verify_extra_tokens.output);
        for (k, v) in verify_extra_tokens.by_role {
            let entry = tokens.by_role.entry(k).or_default();
            entry.input = entry.input.saturating_add(v.input);
            entry.output = entry.output.saturating_add(v.output);
        }
    }

    // ---- per-session scratchpad merge (parallel only) ----------------
    if let Some(wt) = &worktree {
        // The agent wrote into the per-session scratchpad view; fold those
        // edits back into the run-level scratchpad. Held under the run
        // branch lock to avoid interleaving with another session's merge.
        // Always attempted (even on Error) so an agent that produced useful
        // notes before failing doesn't silently lose them.
        let session_view = std::fs::read_to_string(wt.scratchpad_path()).unwrap_or_default();
        let _guard = run_branch_lock.lock().await;
        if let Err(e) = merge_scratchpad_into_run(
            &run_paths.scratchpad,
            &session_view,
            wt.scratchpad_seed(),
            seq,
        ) {
            warn!(
                run_id = %run_id,
                seq,
                error = %format!("{e:#}"),
                "grind: scratchpad merge failed"
            );
        }
        drop(_guard);
    }

    // ---- worktree teardown -------------------------------------------
    if let Some(wt) = worktree {
        let cleaned = if status == SessionStatus::Ok {
            wt.cleanup(&*repo_git).await
        } else {
            // Leave the worktree behind under worktrees/failed/ for triage.
            wt.quarantine(&*repo_git).await.map(|_| ())
        };
        if let Err(e) = cleaned {
            warn!(
                run_id = %run_id,
                seq,
                error = %format!("{e:#}"),
                "grind: worktree teardown failed"
            );
        }
    }

    // ---- post_session / on_failure hooks ------------------------------
    // An Aborted session is the user's exit signal (Ctrl-C → Ctrl-C). Running
    // post_session / on_failure hooks here would block the process for up to
    // hook_timeout_secs apiece — exactly the opposite of what the second
    // Ctrl-C asked for. Skip both kinds on Aborted; pre_session already ran
    // (or didn't) before the abort fired.
    if status != SessionStatus::Aborted {
        let mut hook_env = base_env.clone();
        hook_env.insert("PITBOSS_SESSION_PROMPT".into(), prompt.meta.name.clone());
        hook_env.insert("PITBOSS_SESSION_STATUS".into(), status.as_str().to_string());
        hook_env.insert("PITBOSS_SESSION_SUMMARY".into(), summary.clone());
        if let Some(cmd) = plan_hooks.post_session.as_deref() {
            let outcome = run_hook(
                HookKind::PostSession,
                cmd,
                &hook_env,
                hook_timeout,
                &transcript_path,
                &hook_passthrough,
            )
            .await;
            emit_hook_event(&events_tx, seq, HookKind::PostSession, &outcome);
        }
        if status != SessionStatus::Ok {
            if let Some(cmd) = plan_hooks.on_failure.as_deref() {
                let outcome = run_hook(
                    HookKind::OnFailure,
                    cmd,
                    &hook_env,
                    hook_timeout,
                    &transcript_path,
                    &hook_passthrough,
                )
                .await;
                emit_hook_event(&events_tx, seq, HookKind::OnFailure, &outcome);
            }
        }
    }

    // The run_branch is unused beyond this point; consume it to silence the
    // unused-variable warning while keeping the field on `SessionTaskInput`
    // for symmetry with other run-level handles.
    let _ = run_branch;

    drop(permit);

    Ok(SessionRecord {
        seq,
        run_id,
        prompt: prompt.meta.name.clone(),
        started_at,
        ended_at,
        status,
        summary: Some(summary),
        commit,
        tokens,
        cost_usd,
        transcript_path: transcript_rel,
    })
}

/// Outcome of [`verify_with_fixer_loop`]. Folded into the surrounding session
/// record so the fixer's tokens / cost / final status all show up in the
/// `sessions.jsonl` line for the original prompt.
#[derive(Debug)]
struct VerifyOutcome {
    /// Final session status after any fixer dispatches.
    status: SessionStatus,
    /// Replacement summary when the verify cycle ends in failure. `None`
    /// leaves the original session summary intact.
    summary_override: Option<String>,
    /// Tokens consumed by every fixer dispatch in the loop, summed across
    /// attempts. Folded onto the implementer's tokens by the caller.
    extra_tokens: TokenUsage,
    /// Cost added by every fixer dispatch in the loop.
    extra_cost_usd: f64,
}

impl VerifyOutcome {
    fn ok() -> Self {
        Self {
            status: SessionStatus::Ok,
            summary_override: None,
            extra_tokens: TokenUsage::default(),
            extra_cost_usd: 0.0,
        }
    }
}

/// Run the project's test suite for a `verify: true` session and, on failure,
/// dispatch the fixer agent up to [`crate::config::RetryBudgets::fixer_max_attempts`]
/// times before recording the session as [`SessionStatus::Error`].
///
/// `shared_target_dir`, when `Some`, layers `CARGO_TARGET_DIR=<path>` onto the
/// test process. Used by parallel sessions to point each worktree's
/// `cargo test` at the main workspace's `target/` so they share the
/// incremental cache instead of paying a full rebuild per worktree. Non-cargo
/// runners ignore the env var.
///
/// The fixer prompt is rendered from [`GRIND_FIXER_PROMPT_TEMPLATE`] and
/// dispatched against [`crate::config::ModelRoles::fixer`]. Each attempt
/// re-runs the test suite; the first passing run resolves the verify cycle as
/// `Ok`. Tokens consumed by every fixer dispatch are accumulated in the
/// returned [`VerifyOutcome`] so the caller can fold them onto the session
/// total. A cancelled fixer dispatch surfaces as `Aborted`; an erroring
/// dispatch (timeout / non-zero exit / agent failure) bails the loop with
/// `Error`.
#[allow(clippy::too_many_arguments)]
async fn verify_with_fixer_loop<A: Agent + ?Sized>(
    seq: u32,
    prompt: &PromptDoc,
    workdir: &Path,
    config: &Config,
    transcript_path: &Path,
    shared_target_dir: Option<&Path>,
    agent: &A,
    cancel: &CancellationToken,
    base_env: &HashMap<String, String>,
) -> Result<VerifyOutcome> {
    let Some(test_runner) = project_tests::detect(workdir, config.tests.command.as_deref()) else {
        debug!(
            seq,
            prompt = %prompt.meta.name,
            "grind: verify requested but no test runner detected"
        );
        return Ok(VerifyOutcome::ok());
    };
    let test_runner = match shared_target_dir {
        Some(target) => {
            let mut env = HashMap::new();
            env.insert("CARGO_TARGET_DIR".to_string(), target.display().to_string());
            test_runner.with_env(env)
        }
        None => test_runner,
    };

    let verify_log = transcript_path.with_extension("verify.log");
    let mut outcome = test_runner
        .run(verify_log)
        .await
        .with_context(|| format!("grind: verify run for session {seq}"))?;
    if outcome.passed {
        return Ok(VerifyOutcome::ok());
    }

    let max_attempts = config.retries.fixer_max_attempts;
    if max_attempts == 0 {
        warn!(
            seq,
            prompt = %prompt.meta.name,
            summary = %outcome.summary,
            "grind: verify failed (fixer disabled)"
        );
        return Ok(VerifyOutcome {
            status: SessionStatus::Error,
            summary_override: Some(format!("verify failed: {}", outcome.summary)),
            extra_tokens: TokenUsage::default(),
            extra_cost_usd: 0.0,
        });
    }

    let mut total_tokens = TokenUsage::default();
    let mut total_cost = 0.0;
    let model = config.models.fixer.clone();
    let dispatch_timeout = prompt
        .meta
        .max_session_seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_SESSION_TIMEOUT);

    for attempt in 1..=max_attempts {
        if cancel.is_cancelled() {
            return Ok(VerifyOutcome {
                status: SessionStatus::Aborted,
                summary_override: Some("verify aborted before fixer attempt".into()),
                extra_tokens: total_tokens,
                extra_cost_usd: total_cost,
            });
        }
        info!(
            seq,
            prompt = %prompt.meta.name,
            attempt,
            "grind: verify failed; dispatching fixer"
        );

        let user_prompt =
            render_grind_fixer_prompt(&prompt.meta.name, &prompt.body, &outcome.summary);
        let log_path = transcript_path.with_extension(format!("fix-{:02}.log", attempt));
        let request = AgentRequest {
            role: Role::Fixer,
            model: model.clone(),
            system_prompt: String::new(),
            user_prompt,
            workdir: workdir.to_path_buf(),
            log_path,
            timeout: dispatch_timeout,
            env: base_env.clone(),
        };

        let dispatch = dispatch_agent(agent, request, cancel).await?;
        total_tokens.input = total_tokens.input.saturating_add(dispatch.tokens.input);
        total_tokens.output = total_tokens.output.saturating_add(dispatch.tokens.output);
        for (k, v) in &dispatch.tokens.by_role {
            let entry = total_tokens.by_role.entry(k.clone()).or_default();
            entry.input = entry.input.saturating_add(v.input);
            entry.output = entry.output.saturating_add(v.output);
        }
        total_cost += session_cost_usd(config, &model, dispatch.tokens.input, dispatch.tokens.output);

        match &dispatch.stop_reason {
            StopReason::Completed => {}
            StopReason::Cancelled => {
                return Ok(VerifyOutcome {
                    status: SessionStatus::Aborted,
                    summary_override: Some("verify aborted during fixer dispatch".into()),
                    extra_tokens: total_tokens,
                    extra_cost_usd: total_cost,
                });
            }
            StopReason::Timeout => {
                warn!(
                    seq,
                    prompt = %prompt.meta.name,
                    attempt,
                    "grind: fixer dispatch timed out"
                );
                return Ok(VerifyOutcome {
                    status: SessionStatus::Error,
                    summary_override: Some(format!("fixer attempt {attempt} timed out")),
                    extra_tokens: total_tokens,
                    extra_cost_usd: total_cost,
                });
            }
            StopReason::Error(msg) => {
                warn!(
                    seq,
                    prompt = %prompt.meta.name,
                    attempt,
                    error = %msg,
                    "grind: fixer dispatch failed"
                );
                return Ok(VerifyOutcome {
                    status: SessionStatus::Error,
                    summary_override: Some(format!("fixer attempt {attempt} failed: {msg}")),
                    extra_tokens: total_tokens,
                    extra_cost_usd: total_cost,
                });
            }
        }

        let attempt_log = transcript_path.with_extension(format!("verify-{:02}.log", attempt));
        outcome = test_runner
            .run(attempt_log)
            .await
            .with_context(|| {
                format!("grind: verify re-run after fixer attempt {attempt} for session {seq}")
            })?;
        if outcome.passed {
            info!(
                seq,
                prompt = %prompt.meta.name,
                attempt,
                "grind: verify passed after fixer"
            );
            return Ok(VerifyOutcome {
                status: SessionStatus::Ok,
                summary_override: None,
                extra_tokens: total_tokens,
                extra_cost_usd: total_cost,
            });
        }
    }

    warn!(
        seq,
        prompt = %prompt.meta.name,
        max_attempts,
        summary = %outcome.summary,
        "grind: verify still failing after fixer budget"
    );
    Ok(VerifyOutcome {
        status: SessionStatus::Error,
        summary_override: Some(format!(
            "verify failed after {max_attempts} fixer attempts: {}",
            outcome.summary
        )),
        extra_tokens: total_tokens,
        extra_cost_usd: total_cost,
    })
}

async fn dispatch_agent<A: Agent + ?Sized>(
    agent: &A,
    request: AgentRequest,
    cancel: &CancellationToken,
) -> Result<AgentDispatch> {
    dispatch_agent_with_events(agent, request, cancel, None).await
}

/// Variant of [`dispatch_agent`] that forwards each [`AgentEvent`] to the
/// runner's broadcast channel as a [`GrindEvent`]. The implementer dispatch
/// inside [`run_session_task`] threads the seq + sender through here; the
/// fixer-loop dispatches in [`verify_with_fixer_loop`] keep using the
/// no-forward variant since the TUI's right pane reflects the originating
/// session's transcript regardless of which sub-dispatch produced a line.
async fn dispatch_agent_with_events<A: Agent + ?Sized>(
    agent: &A,
    request: AgentRequest,
    cancel: &CancellationToken,
    forward: Option<(broadcast::Sender<GrindEvent>, u32)>,
) -> Result<AgentDispatch> {
    let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(64);
    let cancel_clone = cancel.clone();
    let drain_task = tokio::spawn(async move {
        while let Some(ev) = events_rx.recv().await {
            let Some((sender, seq)) = forward.as_ref() else {
                continue;
            };
            match ev {
                AgentEvent::Stdout(line) => {
                    let _ = sender.send(GrindEvent::AgentStdout { seq: *seq, line });
                }
                AgentEvent::Stderr(line) => {
                    let _ = sender.send(GrindEvent::AgentStderr { seq: *seq, line });
                }
                AgentEvent::ToolUse(name) => {
                    let _ = sender.send(GrindEvent::AgentToolUse { seq: *seq, name });
                }
                AgentEvent::TokenDelta(_) => {
                    // Token deltas are folded from the dispatch outcome; the
                    // TUI does not surface intermediate deltas.
                }
            }
        }
    });

    let outcome = agent
        .run(request, events_tx, cancel_clone)
        .await
        .context("grind: agent dispatch failed")?;
    let _ = drain_task.await;

    Ok(AgentDispatch {
        stop_reason: outcome.stop_reason,
        tokens: outcome.tokens,
    })
}

/// Forward a hook outcome onto the runner's event broadcast. Errors from a
/// closed broadcast channel are swallowed — the runner keeps running even
/// when no subscriber is attached.
fn emit_hook_event(
    events_tx: &broadcast::Sender<GrindEvent>,
    seq: u32,
    kind: HookKind,
    outcome: &HookOutcome,
) {
    let _ = events_tx.send(GrindEvent::HookFired {
        seq,
        kind,
        success: outcome.is_success(),
        description: outcome.description(),
    });
}

/// `f64` quotient with a zero-cap guard. Used by the budget-warning emission
/// to avoid `0/0` when a cap is unset (already filtered upstream) or when the
/// counter and cap are both zero.
fn fraction_used_u64(used: u64, cap: u64) -> f64 {
    if cap == 0 {
        0.0
    } else {
        used as f64 / cap as f64
    }
}

struct AgentDispatch {
    stop_reason: StopReason,
    tokens: TokenUsage,
}

/// Compose the full user-prompt body the agent sees: standing instruction,
/// session-log tail, scratchpad snapshot, and the user-authored prompt body.
/// Each block is fenced with stable markers so a downstream tool (or another
/// agent) can locate or strip individual sections without reparsing.
pub fn compose_user_prompt(
    standing_instruction: &str,
    session_log: &str,
    scratchpad: &str,
    prompt_body: &str,
) -> String {
    let mut out = String::with_capacity(
        standing_instruction.len() + session_log.len() + scratchpad.len() + prompt_body.len() + 256,
    );
    out.push_str(standing_instruction.trim_end_matches('\n'));
    out.push_str("\n\n");
    out.push_str(SESSION_LOG_OPEN);
    out.push('\n');
    if session_log.trim().is_empty() {
        out.push_str("(no prior sessions)\n");
    } else {
        out.push_str(session_log.trim_end_matches('\n'));
        out.push('\n');
    }
    out.push_str(SESSION_LOG_CLOSE);
    out.push_str("\n\n");
    out.push_str(SCRATCHPAD_OPEN);
    out.push('\n');
    if scratchpad.trim().is_empty() {
        out.push_str("(scratchpad is empty)\n");
    } else {
        out.push_str(scratchpad.trim_end_matches('\n'));
        out.push('\n');
    }
    out.push_str(SCRATCHPAD_CLOSE);
    out.push_str("\n\n");
    out.push_str(prompt_body.trim_start_matches('\n'));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn read_summary_or_fallback(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        Ok(_) => {
            warn!(path = %path.display(), "grind: agent left $PITBOSS_SUMMARY_FILE empty");
            "(no summary provided)".to_string()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            warn!(path = %path.display(), "grind: agent did not write $PITBOSS_SUMMARY_FILE");
            "(no summary provided)".to_string()
        }
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %format!("{e:#}"),
                "grind: failed to read $PITBOSS_SUMMARY_FILE"
            );
            "(no summary provided)".to_string()
        }
    }
}

fn relative_to(base: &Path, full: &Path) -> PathBuf {
    full.strip_prefix(base)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| full.to_path_buf())
}

fn tail_lines(text: &str, n: usize) -> String {
    let mut buf: Vec<&str> = Vec::new();
    for line in text.lines() {
        buf.push(line);
        if buf.len() > n {
            buf.remove(0);
        }
    }
    let mut out = buf.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Format the per-run branch name. Stable so worktree branches in
/// [`super::worktree::session_branch_name`] derive their names from the same
/// prefix.
pub fn run_branch_name(run_id: &str) -> String {
    format!("pitboss/grind/{run_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_branch_name_uses_canonical_prefix() {
        assert_eq!(
            run_branch_name("20260430T120000Z-1a2b"),
            "pitboss/grind/20260430T120000Z-1a2b"
        );
    }

    #[test]
    fn compose_user_prompt_stamps_all_three_marker_blocks() {
        let out = compose_user_prompt("STANDING", "session log row", "scratchpad note", "body");
        assert!(out.starts_with("STANDING\n\n"));
        assert!(out.contains(SESSION_LOG_OPEN));
        assert!(out.contains(SESSION_LOG_CLOSE));
        assert!(out.contains(SCRATCHPAD_OPEN));
        assert!(out.contains(SCRATCHPAD_CLOSE));
        assert!(out.contains("session log row"));
        assert!(out.contains("scratchpad note"));
        assert!(out.contains("body"));
        // Body lands at the end.
        let body_pos = out.find("body").unwrap();
        assert!(body_pos > out.find(SCRATCHPAD_CLOSE).unwrap());
    }

    #[test]
    fn compose_user_prompt_substitutes_empty_blocks() {
        let out = compose_user_prompt("STANDING", "", "", "do the thing");
        assert!(out.contains("(no prior sessions)"));
        assert!(out.contains("(scratchpad is empty)"));
        assert!(out.contains("do the thing"));
    }

    #[test]
    fn standing_instruction_block_carries_markers() {
        let s = standing_instruction_block();
        assert!(
            s.contains("<!-- pitboss:standing-instruction:start -->"),
            "instruction missing start marker"
        );
        assert!(
            s.contains("<!-- pitboss:standing-instruction:end -->"),
            "instruction missing end marker"
        );
        assert!(s.contains("$PITBOSS_SUMMARY_FILE"));
        assert!(s.contains("$PITBOSS_SCRATCHPAD"));
        assert!(s.contains("$PITBOSS_RUN_ID"));
    }

    #[test]
    fn tail_lines_keeps_last_n() {
        let text = "a\nb\nc\nd\ne\n";
        assert_eq!(tail_lines(text, 3), "c\nd\ne\n");
        assert_eq!(tail_lines(text, 10), "a\nb\nc\nd\ne\n");
        assert_eq!(tail_lines("", 5), "");
    }

    #[test]
    fn read_summary_falls_back_when_file_is_missing_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        assert_eq!(read_summary_or_fallback(&missing), "(no summary provided)");

        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, "   \n  \n").unwrap();
        assert_eq!(read_summary_or_fallback(&empty), "(no summary provided)");

        let real = dir.path().join("real.txt");
        std::fs::write(&real, "did the thing\n").unwrap();
        assert_eq!(read_summary_or_fallback(&real), "did the thing");
    }

    #[test]
    fn shutdown_drain_and_abort_signals_propagate() {
        let s = GrindShutdown::new();
        assert!(!s.is_draining());
        assert!(!s.cancel_token().is_cancelled());

        s.drain();
        assert!(s.is_draining());
        assert!(!s.cancel_token().is_cancelled());

        let s2 = s.clone();
        s2.abort();
        assert!(s.is_draining());
        assert!(s.cancel_token().is_cancelled());
    }

    #[test]
    fn grind_fixer_prompt_substitutes_all_placeholders() {
        let rendered = render_grind_fixer_prompt(
            "fp-hunter",
            "Find every false-positive lint warning and silence it.",
            "test_widget ... FAILED\n  expected 5, got 6",
        );
        assert!(rendered.contains("fp-hunter"));
        assert!(rendered.contains("Find every false-positive"));
        assert!(rendered.contains("test_widget ... FAILED"));
        // No placeholder leakage.
        assert!(!rendered.contains("{prompt_name}"));
        assert!(!rendered.contains("{prompt_body}"));
        assert!(!rendered.contains("{test_output}"));
    }
}
