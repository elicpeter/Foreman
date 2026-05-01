//! Sequential grind orchestrator: pick a prompt, dispatch the agent, capture
//! the summary, commit, repeat.
//!
//! [`GrindRunner`] wires together the artifacts assembled in phases 01-06:
//! discovered prompts, a [`GrindPlan`], a [`Scheduler`], and an open
//! [`RunDir`]. One [`GrindRunner::run`] call drives the loop until the
//! scheduler is exhausted, the run is drained (one Ctrl-C), or the run is
//! aborted (second Ctrl-C, or any other [`CancellationToken::cancel`]).
//!
//! The runner is intentionally agnostic to the surface that signals a stop:
//! it takes a [`GrindShutdown`] handle that carries an
//! [`AtomicBool`](std::sync::atomic::AtomicBool) drain flag and a
//! [`CancellationToken`] abort token. The CLI binds those to live `Ctrl-C`
//! events; the integration tests flip them by hand.
//!
//! Sequential only — Phase 11 wires worktrees + a semaphore on top.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::agent::{Agent, AgentEvent, AgentRequest, Role, StopReason};
use crate::config::Config;
use crate::git::{CommitId, Git};
use crate::state::TokenUsage;
use crate::tests as project_tests;

use super::budget::{session_cost_usd, BudgetCheck, BudgetReason, BudgetTracker};
use super::hooks::{run_hook, HookKind};
use super::plan::{GrindPlan, PlanBudgets};
use super::prompt::PromptDoc;
use super::run_dir::{RunDir, SessionRecord, SessionStatus};
use super::scheduler::{Scheduler, SchedulerState};
use super::state::{build_state, RunStatus};

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
/// leaves `max_session_seconds` unset. Generous so a normal session has
/// headroom; per-prompt enforcement lands in Phase 08.
const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(60 * 30);

/// Standing-instruction text rendered into the agent's prompt body. Stable so
/// callers (and tests) can grep for it. Public for snapshot tests; not part of
/// the supported API surface.
pub fn standing_instruction_block() -> &'static str {
    STANDING_INSTRUCTION_TEMPLATE
}

/// Two-stage shutdown handle.
///
/// `drain` flips on the first Ctrl-C: the runner finishes the in-flight
/// session cleanly, then exits. `abort` flips on the second Ctrl-C (or any
/// caller-driven cancel): the in-flight agent is cancelled and the session
/// is recorded with [`SessionStatus::Aborted`].
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
    /// All session records appended during this run, in dispatch order.
    pub sessions: Vec<SessionRecord>,
    /// Why the loop exited.
    pub stop_reason: GrindStopReason,
}

/// Sequential grind orchestrator. See module docs.
pub struct GrindRunner<A: Agent, G: Git> {
    workspace: PathBuf,
    config: Config,
    run_id: String,
    branch: String,
    plan: GrindPlan,
    scheduler: Scheduler,
    run_dir: RunDir,
    agent: A,
    git: G,
    next_seq: u32,
    budgets: PlanBudgets,
    consecutive_failure_limit: u32,
    started_at: DateTime<Utc>,
    initial_budget: super::budget::BudgetSnapshot,
}

impl<A: Agent, G: Git> GrindRunner<A, G> {
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
        Self {
            workspace,
            config,
            run_id,
            branch,
            plan,
            scheduler,
            run_dir,
            agent,
            git,
            next_seq: 1,
            budgets,
            consecutive_failure_limit,
            started_at: Utc::now(),
            initial_budget: super::budget::BudgetSnapshot::default(),
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
        Self {
            workspace,
            config,
            run_id,
            branch,
            plan,
            scheduler,
            run_dir,
            agent,
            git,
            next_seq: last_session_seq.saturating_add(1),
            budgets,
            consecutive_failure_limit,
            started_at,
            initial_budget,
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
        // Stamp the initial state.json so a resume target exists from the
        // first moment a run is on disk, even if the host process dies before
        // a single session lands.
        if let Err(e) = self.write_state(&tracker, self.next_seq.saturating_sub(1), RunStatus::Active)
        {
            warn!(
                run_id = %self.run_id,
                error = %format!("{e:#}"),
                "grind: initial state.json write failed"
            );
        }

        loop {
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

            let Some(prompt) = self.scheduler.next() else {
                break;
            };

            let seq = self.next_seq;
            self.next_seq += 1;
            info!(
                run_id = %self.run_id,
                seq,
                prompt = %prompt.meta.name,
                "grind: dispatching session"
            );

            let record = self.run_session(seq, &prompt, &shutdown).await?;
            self.scheduler.record_run(&prompt.meta.name);
            self.run_dir
                .log()
                .append(&record)
                .with_context(|| format!("grind: appending session {seq} record to log"))?;
            tracker.record_session(&record);
            sessions.push(record.clone());
            // Persist resume state right after the session record lands. The
            // session log is the source of truth; state.json is a derived
            // cache, so writing it second keeps the two consistent under
            // crash: if the process dies between the log append and this
            // write, resume will replay the last persisted scheduler state
            // (which is one session behind sessions.jsonl) — the next
            // dispatch will be the prompt we were already going to pick.
            if let Err(e) = self.write_state(&tracker, seq, RunStatus::Active) {
                warn!(
                    run_id = %self.run_id,
                    seq,
                    error = %format!("{e:#}"),
                    "grind: state.json write failed"
                );
            }

            if record.status == SessionStatus::Aborted {
                stop_reason = GrindStopReason::Aborted;
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

            if let BudgetCheck::Exhausted(reason) = tracker.check() {
                info!(
                    run_id = %self.run_id,
                    reason = %reason,
                    "grind: BudgetExhausted"
                );
                stop_reason = GrindStopReason::BudgetExhausted(reason);
                break;
            }
        }

        // Stamp the terminal state.json so resume callers can tell at a
        // glance whether a run is still live. Use the seq of the last
        // recorded session (or 0 when none ran).
        let terminal_seq = sessions.last().map(|r| r.seq).unwrap_or(0);
        let terminal_status = match &stop_reason {
            GrindStopReason::Completed => RunStatus::Completed,
            GrindStopReason::Drained | GrindStopReason::Aborted => RunStatus::Aborted,
            GrindStopReason::BudgetExhausted(_)
            | GrindStopReason::ConsecutiveFailureLimit { .. } => RunStatus::Failed,
        };
        if let Err(e) = self.write_state(&tracker, terminal_seq, terminal_status) {
            warn!(
                run_id = %self.run_id,
                error = %format!("{e:#}"),
                "grind: terminal state.json write failed"
            );
        }

        Ok(GrindRunOutcome {
            run_id: self.run_id.clone(),
            branch: self.branch.clone(),
            sessions,
            stop_reason,
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

    async fn run_session(
        &self,
        seq: u32,
        prompt: &PromptDoc,
        shutdown: &GrindShutdown,
    ) -> Result<SessionRecord> {
        let started_at = Utc::now();
        let transcript_path = self.run_dir.paths().transcript_for(seq);
        let transcript_rel = relative_to(&self.workspace, &transcript_path);

        let summary_path = self
            .run_dir
            .paths()
            .root
            .join(format!(".pitboss-summary-{:04}.txt", seq));
        // Make sure the summary path is empty so a stale value from a prior
        // session can never be misread as the agent's current output.
        let _ = std::fs::remove_file(&summary_path);

        let scratchpad_path = self.run_dir.scratchpad().path_for_agent().to_path_buf();

        let mut base_env: HashMap<String, String> = HashMap::new();
        base_env.insert("PITBOSS_RUN_ID".into(), self.run_id.clone());
        base_env.insert("PITBOSS_PROMPT_NAME".into(), prompt.meta.name.clone());
        base_env.insert(
            "PITBOSS_SUMMARY_FILE".into(),
            summary_path.display().to_string(),
        );
        base_env.insert(
            "PITBOSS_SCRATCHPAD".into(),
            scratchpad_path.display().to_string(),
        );
        base_env.insert("PITBOSS_SESSION_SEQ".into(), seq.to_string());

        let hook_timeout =
            Duration::from_secs(self.config.grind.hook_timeout_secs.max(1));

        // ---- pre_session hook ---------------------------------------------
        // Runs before the agent dispatch. Non-zero exit / timeout / spawn
        // failure all skip dispatch and resolve the session as `Error`.
        let mut skip_dispatch_reason: Option<String> = None;
        if let Some(cmd) = self.plan.hooks.pre_session.as_deref() {
            let mut env = base_env.clone();
            env.insert("PITBOSS_SESSION_PROMPT".into(), prompt.meta.name.clone());
            let outcome = run_hook(
                HookKind::PreSession,
                cmd,
                &env,
                hook_timeout,
                &transcript_path,
            )
            .await;
            if !outcome.is_success() {
                warn!(
                    run_id = %self.run_id,
                    seq,
                    outcome = %outcome.description(),
                    "grind: pre_session hook failed; skipping dispatch"
                );
                skip_dispatch_reason =
                    Some(format!("pre_session hook {}", outcome.description()));
            }
        }

        let mut status: SessionStatus;
        let summary: String;
        let mut commit: Option<CommitId> = None;
        let mut tokens: TokenUsage = TokenUsage::default();
        let mut cost_usd: f64 = 0.0;
        let ended_at: DateTime<Utc>;

        if let Some(reason) = skip_dispatch_reason {
            status = SessionStatus::Error;
            summary = reason;
            ended_at = Utc::now();
        } else {
            let session_log_tail = self
                .read_session_log_tail()
                .unwrap_or_else(|e| format!("(failed to read sessions.md: {e})"));
            let scratchpad_text = self
                .run_dir
                .scratchpad()
                .read()
                .unwrap_or_else(|e| format!("(failed to read scratchpad: {e})"));

            let user_prompt = compose_user_prompt(
                STANDING_INSTRUCTION_TEMPLATE,
                &session_log_tail,
                &scratchpad_text,
                &prompt.body,
            );

            let timeout = prompt
                .meta
                .max_session_seconds
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_SESSION_TIMEOUT);

            let model = self.config.models.implementer.clone();
            let request = AgentRequest {
                role: Role::Implementer,
                model: model.clone(),
                system_prompt: String::new(),
                user_prompt,
                workdir: self.workspace.clone(),
                log_path: transcript_path.clone(),
                timeout,
                env: base_env.clone(),
            };

            // Outer wall-clock guard. The agent itself honors `request.timeout`
            // (subprocess kill, etc.), but a misbehaving or stub agent might
            // not — wrapping the dispatch in `tokio::time::timeout` makes the
            // cap enforceable from pitboss's side. A timeout drops the agent
            // future, which cancels the underlying subprocess by destruction,
            // and we synthesize a `Timeout` outcome so the session record
            // reflects what happened.
            let mut summary_override: Option<String> = None;
            let dispatch = match tokio::time::timeout(
                timeout,
                self.dispatch_agent(request, shutdown.cancel_token()),
            )
            .await
            {
                Ok(res) => res?,
                Err(_) => {
                    warn!(
                        run_id = %self.run_id,
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
                &self.config,
                &model,
                dispatch.tokens.input,
                dispatch.tokens.output,
            );

            // Post-hoc cost cap. The agent can't know its rolling spend during
            // a dispatch, so the per-prompt cost limit fires after the session
            // completes: if the final cost is over the cap, the session is
            // recorded as `Error` with a clear summary rather than letting the
            // agent's own report stand.
            if status == SessionStatus::Ok {
                if let Some(cap) = prompt.meta.max_session_cost_usd {
                    if cost_usd > cap {
                        warn!(
                            run_id = %self.run_id,
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

            if status == SessionStatus::Ok && prompt.meta.verify {
                status = self.verify_session(seq, prompt, &transcript_path).await?;
            }

            commit = match status {
                SessionStatus::Ok | SessionStatus::Error => {
                    // We try to land whatever changes the agent produced even
                    // on Error so partial work isn't lost; the session record
                    // carries the status verbatim. Aborted/Timeout sessions
                    // skip the commit step because the working tree state is
                    // undefined. Dirty is set later (after the stash) so it
                    // never reaches this match.
                    self.try_commit_session(seq, prompt).await?
                }
                _ => None,
            };

            // Stash any stragglers so the next session starts clean. Sessions
            // that already aborted leave behind whatever the agent produced —
            // stashing it labeled is preferable to discarding. `.pitboss/` is
            // excluded so the run's own bookkeeping (sessions.jsonl,
            // scratchpad, transcripts) stays in place between sessions.
            let stash_label = format!("grind/{}/session-{:04}-leftover", self.run_id, seq);
            let pitboss_rel = Path::new(".pitboss");
            match self.git.stash_push(&stash_label, &[pitboss_rel]).await {
                Ok(true) => {
                    warn!(
                        run_id = %self.run_id,
                        seq,
                        stash = %stash_label,
                        "grind: leftover changes stashed"
                    );
                    // The session itself was otherwise clean — the dirty
                    // marker is a triage hint, not an outright failure.
                    // Aborted / Timeout / Error stay as-is so the failure mode
                    // is preserved.
                    if status == SessionStatus::Ok {
                        status = SessionStatus::Dirty;
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(
                        run_id = %self.run_id,
                        seq,
                        error = %format!("{e:#}"),
                        "grind: stash_push failed"
                    );
                }
            }

            tokens = dispatch.tokens;
        }

        // ---- post_session / on_failure hooks ------------------------------
        // post_session fires for every resolved session regardless of status;
        // on_failure fires only when the status is non-`Ok`. Both receive
        // `PITBOSS_SESSION_STATUS` and `PITBOSS_SESSION_SUMMARY` on top of the
        // shared agent env, plus `PITBOSS_SESSION_PROMPT`. Their exit codes
        // are logged but do not influence the recorded session status.
        let mut hook_env = base_env.clone();
        hook_env.insert("PITBOSS_SESSION_PROMPT".into(), prompt.meta.name.clone());
        hook_env.insert("PITBOSS_SESSION_STATUS".into(), status.as_str().to_string());
        hook_env.insert("PITBOSS_SESSION_SUMMARY".into(), summary.clone());
        if let Some(cmd) = self.plan.hooks.post_session.as_deref() {
            let _ = run_hook(
                HookKind::PostSession,
                cmd,
                &hook_env,
                hook_timeout,
                &transcript_path,
            )
            .await;
        }
        if status != SessionStatus::Ok {
            if let Some(cmd) = self.plan.hooks.on_failure.as_deref() {
                let _ = run_hook(
                    HookKind::OnFailure,
                    cmd,
                    &hook_env,
                    hook_timeout,
                    &transcript_path,
                )
                .await;
            }
        }

        Ok(SessionRecord {
            seq,
            run_id: self.run_id.clone(),
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

    /// Commit any code changes the session produced. Returns the new commit
    /// id, or `None` if there was nothing code-side to commit (e.g., the
    /// agent only edited `.pitboss/`).
    async fn try_commit_session(&self, seq: u32, prompt: &PromptDoc) -> Result<Option<CommitId>> {
        // `.pitboss/` is excluded the same way `pitboss play` does — sessions.jsonl
        // and friends live under there and would otherwise pollute every grind
        // commit.
        let pitboss_rel = Path::new(".pitboss");
        self.git
            .stage_changes(&[pitboss_rel])
            .await
            .with_context(|| format!("grind: staging session {seq} changes"))?;

        let has_staged = self
            .git
            .has_staged_changes()
            .await
            .with_context(|| format!("grind: checking staged changes for session {seq}"))?;
        if !has_staged {
            debug!(seq, prompt = %prompt.meta.name, "grind: no code changes to commit");
            return Ok(None);
        }

        let message = format!(
            "[pitboss/grind] {} session-{:04} ({})",
            prompt.meta.name, seq, self.run_id,
        );
        let id = self
            .git
            .commit(&message)
            .await
            .with_context(|| format!("grind: committing session {seq}"))?;
        Ok(Some(id))
    }

    /// Auto-detect the project's test runner and run it once. Returns
    /// [`SessionStatus::Ok`] when tests pass and [`SessionStatus::Error`] when
    /// they fail. The fixer cycle is deferred to a follow-up; see deferred.md.
    async fn verify_session(
        &self,
        seq: u32,
        prompt: &PromptDoc,
        transcript_path: &Path,
    ) -> Result<SessionStatus> {
        let Some(runner) =
            project_tests::detect(&self.workspace, self.config.tests.command.as_deref())
        else {
            debug!(
                seq,
                prompt = %prompt.meta.name,
                "grind: verify requested but no test runner detected"
            );
            return Ok(SessionStatus::Ok);
        };
        // Sibling log next to the agent transcript: `TestRunner::run` opens
        // its log_path with `truncate(true)`, so reusing the transcript path
        // would erase the agent's session output.
        let verify_log = transcript_path.with_extension("verify.log");
        let outcome = runner
            .run(verify_log)
            .await
            .with_context(|| format!("grind: verify run for session {seq}"))?;
        if outcome.passed {
            Ok(SessionStatus::Ok)
        } else {
            warn!(
                seq,
                prompt = %prompt.meta.name,
                summary = %outcome.summary,
                "grind: verify failed"
            );
            Ok(SessionStatus::Error)
        }
    }

    async fn dispatch_agent(
        &self,
        request: AgentRequest,
        cancel: &CancellationToken,
    ) -> Result<AgentDispatch> {
        let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(64);
        let cancel_clone = cancel.clone();
        let drain_task = tokio::spawn(async move {
            while events_rx.recv().await.is_some() {
                // Phase 13 wires these into the TUI; for now we drop them so
                // the channel doesn't apply backpressure on the agent.
            }
        });

        let outcome = self
            .agent
            .run(request, events_tx, cancel_clone)
            .await
            .context("grind: agent dispatch failed")?;
        let _ = drain_task.await;

        Ok(AgentDispatch {
            stop_reason: outcome.stop_reason,
            tokens: outcome.tokens,
        })
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

/// Format the per-run branch name. Stable so worktree branches in Phase 11
/// can derive their names from the same prefix.
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
}
