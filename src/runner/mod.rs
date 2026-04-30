//! Orchestration loop and event channel.
//!
//! The runner owns the per-phase state machine. It snapshots the planning
//! artifacts, dispatches the implementer agent, validates the agent's output,
//! runs the project tests, and lands a per-phase commit. Every observable
//! transition is broadcast on a [`tokio::sync::broadcast`] channel so the CLI
//! logger and the (later) TUI can subscribe without changing the runner.
//!
//! Phase 12 wires the implementer-only flow: agent → validate → tests → commit.
//! Fixer (phase 13) and auditor (phase 14) drop in by extending [`run_phase`]
//! with extra dispatches between "tests" and "commit"; the event channel and
//! state shape are forward-compatible with both.

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
use crate::state::{self, RunState};
use crate::tests as project_tests;
use crate::util::write_atomic;

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
    /// The agent left `deferred.md` in an unparseable state. The runner
    /// restored from the pre-agent snapshot before halting. The string is
    /// the parser's diagnostic.
    DeferredInvalid(String),
    /// The project's test suite failed. Holds the short summary captured by
    /// [`crate::tests::TestRunner::run`].
    TestsFailed(String),
    /// The agent exited via timeout, cancellation, or an internal error.
    AgentFailure(String),
}

impl std::fmt::Display for HaltReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HaltReason::PlanTampered => f.write_str("plan.md was modified by the agent"),
            HaltReason::DeferredInvalid(msg) => write!(f, "deferred.md is invalid: {msg}"),
            HaltReason::TestsFailed(summary) => write!(f, "tests failed: {summary}"),
            HaltReason::AgentFailure(msg) => write!(f, "agent failure: {msg}"),
        }
    }
}

/// Streaming events the runner broadcasts to subscribers. Sends are
/// best-effort: a lagging or absent subscriber never blocks the runner.
#[derive(Debug, Clone)]
pub enum Event {
    /// A phase began. `attempt` starts at 1 and increments on each redispatch
    /// (phase 13's fixer; phase 14's auditor re-run).
    PhaseStarted {
        /// Phase being entered.
        phase_id: PhaseId,
        /// Phase title from the heading.
        title: String,
        /// 1-based attempt counter — total agent dispatches at this phase.
        attempt: u32,
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
        }
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

    /// Subscribe to the runner's event stream. Returns a fresh receiver each
    /// call; existing subscribers are unaffected.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events_tx.subscribe()
    }

    /// Drive the runner until the plan completes or a phase halts. The final
    /// phase emits [`Event::RunFinished`] before `Finished` is returned.
    pub async fn run(&mut self) -> Result<RunSummary> {
        loop {
            let result = self.run_phase().await?;
            match result {
                PhaseResult::Halted { phase_id, reason } => {
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
    /// Persists [`RunState`] to `.foreman/state.json` on every exit — including
    /// halts — so the attempts counter and accumulated token usage survive a
    /// halted phase and a subsequent `foreman run` invocation can pick them up.
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

        let attempt = {
            let entry = self.state.attempts.entry(phase_id.clone()).or_insert(0);
            *entry += 1;
            *entry
        };
        let _ = self.events_tx.send(Event::PhaseStarted {
            phase_id: phase_id.clone(),
            title: phase.title.clone(),
            attempt,
        });

        let plan_path = self.workspace.join("plan.md");
        let deferred_path = self.workspace.join("deferred.md");

        let plan_pre = std::fs::read(&plan_path)
            .with_context(|| format!("runner: reading {:?}", plan_path))?;
        let plan_hash = Snapshot::of_bytes(&plan_pre);
        let (deferred_pre, deferred_existed) = match std::fs::read(&deferred_path) {
            Ok(b) => (b, true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("runner: reading {:?}", deferred_path))
                )
            }
        };

        let user_prompt = prompts::implementer(&self.plan, &self.deferred, &phase);
        let log_path = self.attempt_log_path(&phase_id, "implementer", attempt);
        let request = AgentRequest {
            role: Role::Implementer,
            model: self.config.models.implementer.clone(),
            system_prompt: String::new(),
            user_prompt,
            workdir: self.workspace.clone(),
            log_path,
            timeout: DEFAULT_AGENT_TIMEOUT,
        };

        let dispatch = self.dispatch_agent(request).await?;

        match &dispatch.stop_reason {
            StopReason::Completed => {}
            StopReason::Timeout => {
                self.fold_token_usage(Role::Implementer, &dispatch);
                return Ok(PhaseResult::Halted {
                    phase_id,
                    reason: HaltReason::AgentFailure(format!(
                        "agent {:?} timed out after {:?}",
                        self.agent.name(),
                        DEFAULT_AGENT_TIMEOUT
                    )),
                });
            }
            StopReason::Cancelled => {
                self.fold_token_usage(Role::Implementer, &dispatch);
                return Ok(PhaseResult::Halted {
                    phase_id,
                    reason: HaltReason::AgentFailure(format!(
                        "agent {:?} was cancelled",
                        self.agent.name()
                    )),
                });
            }
            StopReason::Error(msg) => {
                self.fold_token_usage(Role::Implementer, &dispatch);
                return Ok(PhaseResult::Halted {
                    phase_id,
                    reason: HaltReason::AgentFailure(msg.clone()),
                });
            }
        }
        self.fold_token_usage(Role::Implementer, &dispatch);

        let plan_post = std::fs::read(&plan_path)
            .with_context(|| format!("runner: reading {:?} after agent", plan_path))?;
        if Snapshot::of_bytes(&plan_post) != plan_hash {
            warn!(phase = %phase_id, "agent modified plan.md; restoring from snapshot");
            write_atomic(&plan_path, &plan_pre).with_context(|| {
                format!(
                    "runner: restoring {:?} from snapshot after tamper",
                    plan_path
                )
            })?;
            return Ok(PhaseResult::Halted {
                phase_id,
                reason: HaltReason::PlanTampered,
            });
        }

        let deferred_text = match std::fs::read_to_string(&deferred_path) {
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
                warn!(phase = %phase_id, error = %msg, "deferred.md is invalid; restoring");
                self.restore_deferred(&deferred_path, &deferred_pre, deferred_existed)?;
                return Ok(PhaseResult::Halted {
                    phase_id,
                    reason: HaltReason::DeferredInvalid(msg),
                });
            }
        }

        let test_runner = project_tests::detect(
            &self.workspace,
            self.config.tests.command.as_deref(),
        );
        if let Some(runner) = test_runner {
            let _ = self.events_tx.send(Event::TestStarted);
            let test_log = self.attempt_log_path(&phase_id, "tests", attempt);
            let outcome = runner
                .run(test_log)
                .await
                .context("runner: running project tests")?;
            let _ = self.events_tx.send(Event::TestFinished {
                passed: outcome.passed,
                summary: outcome.summary.clone(),
            });
            if !outcome.passed {
                return Ok(PhaseResult::Halted {
                    phase_id,
                    reason: HaltReason::TestsFailed(outcome.summary),
                });
            }
        } else {
            debug!("no test runner detected and no override configured; skipping tests");
            let _ = self.events_tx.send(Event::TestsSkipped);
        }

        let plan_rel = Path::new("plan.md");
        let deferred_rel = Path::new("deferred.md");
        let foreman_rel = Path::new(".foreman");
        self.git
            .stage_changes(&[plan_rel, deferred_rel, foreman_rel])
            .await
            .context("runner: staging code-only changes")?;

        let commit = if self
            .git
            .has_staged_changes()
            .await
            .context("runner: checking for staged changes")?
        {
            let message = git::commit_message(&phase_id, &phase.title);
            let id = self
                .git
                .commit(&message)
                .await
                .context("runner: committing phase")?;
            Some(id)
        } else {
            warn!(phase = %phase_id, "phase produced no code changes; skipping commit");
            None
        };

        self.deferred.sweep();
        let deferred_serialized = deferred::serialize(&self.deferred);
        write_atomic(&deferred_path, deferred_serialized.as_bytes())
            .context("runner: writing deferred.md after sweep")?;

        self.state.completed.push(phase_id.clone());

        let next_phase = self.next_phase_id_after(&phase_id);
        if let Some(ref next) = next_phase {
            self.plan.set_current_phase(next.clone());
            let plan_serialized = plan::serialize(&self.plan);
            write_atomic(&plan_path, plan_serialized.as_bytes())
                .context("runner: writing plan.md with advanced current_phase")?;
        }

        state::save(&self.workspace, Some(&self.state))
            .context("runner: persisting state.json")?;

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

    fn next_phase_id_after(&self, current: &PhaseId) -> Option<PhaseId> {
        self.plan
            .phases
            .iter()
            .find(|p| p.id > *current)
            .map(|p| p.id.clone())
    }

    fn attempt_log_path(&self, phase_id: &PhaseId, role: &str, attempt: u32) -> PathBuf {
        self.workspace
            .join(".foreman")
            .join("logs")
            .join(format!("phase-{}-{}-{}.log", phase_id, role, attempt))
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
            let e = self
                .state
                .token_usage
                .by_role
                .entry(k.clone())
                .or_default();
            e.input += v.input;
            e.output += v.output;
        }
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

async fn forward_agent_events(
    mut rx: mpsc::Receiver<AgentEvent>,
    tx: broadcast::Sender<Event>,
) {
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
/// event to stderr until the channel closes.
///
/// This is the "no TUI" CLI experience: progress is rendered via plain
/// `tracing::info`-style lines so log piping and CI logs work out of the box.
pub async fn log_events(mut rx: broadcast::Receiver<Event>) {
    use broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(event) => log_event_line(&event),
            Err(RecvError::Closed) => return,
            Err(RecvError::Lagged(n)) => {
                eprintln!("[foreman] (logger lagged: dropped {n} events)");
            }
        }
    }
}

fn log_event_line(event: &Event) {
    match event {
        Event::PhaseStarted {
            phase_id,
            title,
            attempt,
        } => {
            eprintln!(
                "[foreman] phase {phase_id} ({title}) — attempt {attempt}",
                phase_id = phase_id,
                title = title,
                attempt = attempt
            );
        }
        Event::AgentStdout(line) => eprintln!("[agent] {line}"),
        Event::AgentStderr(line) => eprintln!("[agent:err] {line}"),
        Event::AgentToolUse(name) => eprintln!("[agent:tool] {name}"),
        Event::TestStarted => eprintln!("[foreman] running tests"),
        Event::TestFinished { passed, summary } => {
            let label = if *passed { "tests passed" } else { "tests failed" };
            eprintln!("[foreman] {label}: {summary}");
        }
        Event::TestsSkipped => eprintln!("[foreman] no test runner detected; skipping"),
        Event::PhaseCommitted {
            phase_id,
            commit: Some(c),
        } => {
            eprintln!("[foreman] phase {phase_id} committed: {c}");
        }
        Event::PhaseCommitted {
            phase_id,
            commit: None,
        } => {
            eprintln!("[foreman] phase {phase_id} produced no code changes; no commit");
        }
        Event::PhaseHalted { phase_id, reason } => {
            eprintln!("[foreman] phase {phase_id} halted: {reason}");
        }
        Event::RunFinished => eprintln!("[foreman] run finished"),
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
        assert_eq!(state.branch, "foreman/run-20260429T143022Z");
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
