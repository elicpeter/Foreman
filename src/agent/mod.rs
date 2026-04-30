//! Agent abstraction — the single pluggable surface for every role.
//!
//! Phase 7 nails the trait shape down once. Concrete implementations
//! ([`dry_run::DryRunAgent`] for tests, [`claude_code::ClaudeCodeAgent`] for
//! production in phase 8) plug into the same [`Agent::run`] contract and the
//! runner driving them stays identical.
//!
//! ## Shape
//!
//! - [`AgentRequest`] is the per-dispatch input. Composed once by the runner
//!   from `pitboss.toml`, the active phase, and the prompt template.
//! - [`AgentEvent`] is streamed on the caller-supplied
//!   [`tokio::sync::mpsc::Sender`] while the agent runs. Events are best-effort
//!   — if the receiver is dropped, the agent keeps running and continues to
//!   write to its log file.
//! - [`AgentOutcome`] is the terminal value. [`StopReason`] tells the runner
//!   which terminator fired (natural exit, timeout, cancel, internal error).
//!
//! Implementations **must** honor both the supplied `cancel`
//! [`tokio_util::sync::CancellationToken`] and `req.timeout`.

pub mod aider;
pub mod backend;
pub mod claude_code;
pub mod codex;
pub mod dry_run;
pub mod gemini;
pub mod subprocess;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::state::TokenUsage;

pub use subprocess::{run_logged, run_logged_with_stdin, SubprocessOutcome};

/// Which agent role is being dispatched.
///
/// Round-trips through serde as the lowercase string used in `pitboss.toml`'s
/// `[models]` keys, so a single source of truth covers config and runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// `pitboss plan` — generates a fresh `plan.md` from a goal.
    Planner,
    /// Per-phase implementation pass — the bulk of token spend.
    Implementer,
    /// Post-phase, pre-commit audit pass.
    Auditor,
    /// Test-failure fix-up pass; bounded by `retries.fixer_max_attempts`.
    Fixer,
}

impl Role {
    /// String name matching the `pitboss.toml` `[models]` key. Stable.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Planner => "planner",
            Role::Implementer => "implementer",
            Role::Auditor => "auditor",
            Role::Fixer => "fixer",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything the runner hands an agent to dispatch it once.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// Which role this dispatch fills.
    pub role: Role,
    /// Model identifier passed verbatim to the underlying agent (e.g., the
    /// `--model` flag of the `claude` CLI). Validation is the agent's job.
    pub model: String,
    /// System prompt template, fully substituted.
    pub system_prompt: String,
    /// User prompt body, fully substituted.
    pub user_prompt: String,
    /// Working directory the agent should operate in.
    pub workdir: PathBuf,
    /// Per-attempt log file the agent must tee its output into. The agent
    /// creates this file (and any parent dirs) if it does not exist.
    pub log_path: PathBuf,
    /// Hard wall-clock cap. If the agent is still running when this elapses
    /// the impl must terminate it and return [`StopReason::Timeout`].
    pub timeout: Duration,
}

/// Streaming events emitted while an agent runs.
///
/// Implementations are responsible for ordering and channel delivery. Sends
/// are best-effort: a closed receiver does not abort the run.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// One line of standard output from the underlying process.
    Stdout(String),
    /// One line of standard error from the underlying process.
    Stderr(String),
    /// Incremental token usage update — runner sums these into the running
    /// [`TokenUsage`] total for the active role.
    TokenDelta(TokenUsage),
    /// Tool invocation announced by the agent (used by the dashboard/logger).
    ToolUse(String),
}

/// Final result of a single agent dispatch.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    /// Underlying process exit code. `-1` for non-process outcomes
    /// (timeout, cancel, internal errors).
    pub exit_code: i32,
    /// Why the agent stopped.
    pub stop_reason: StopReason,
    /// Total token usage observed across the run, attributable to
    /// `req.role`. `by_role` may be left empty by impls that only know
    /// totals; the runner re-keys before persisting into [`crate::state::RunState`].
    pub tokens: TokenUsage,
    /// Echo of the request's `log_path`, returned for convenience so callers
    /// don't have to plumb the request through to where the log is consumed.
    pub log_path: PathBuf,
}

/// Why an agent stopped running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Agent ran to natural completion. `exit_code` may still be non-zero.
    Completed,
    /// Agent exceeded `AgentRequest::timeout` and was terminated.
    Timeout,
    /// Caller's [`CancellationToken`] was triggered and the agent terminated.
    Cancelled,
    /// Internal error preventing normal completion (failed to spawn, agent
    /// protocol parse error, etc.). Carries a human-readable message.
    Error(String),
}

/// Single pluggable abstraction for every agent role.
///
/// Implementations must:
/// 1. Stream `AgentEvent`s on `events` while running (best-effort sends).
/// 2. Honor `cancel` — `CancellationToken::cancelled()` resolves means stop.
/// 3. Honor `req.timeout` — internal wall clock, not the runner's job.
/// 4. Return an [`AgentOutcome`] with a [`StopReason`] reflecting which
///    terminator fired. Internal errors return `Ok(outcome)` with
///    [`StopReason::Error`] rather than `Err(_)`; the `Err` channel is for
///    setup failures (couldn't open log file, couldn't spawn subprocess at
///    all, etc.).
#[async_trait]
pub trait Agent: Send + Sync {
    /// Short identifier for log lines (e.g., `"claude-code"`, `"dry-run"`).
    fn name(&self) -> &str;

    /// Run the agent to completion (or until cancelled / timed out).
    async fn run(
        &self,
        req: AgentRequest,
        events: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<AgentOutcome>;
}

/// Blanket impl so `Box<dyn Agent + Send + Sync>` satisfies the `Agent`
/// bound the runner and CLI helpers carry. Enables [`build_agent`] to return
/// a heap-allocated trait object that flows through generic call sites
/// (`Runner::new<A: Agent + 'static>`, `run_with_agent<A: Agent>`) without
/// every caller having to depend on the concrete backend type.
#[async_trait]
impl<A: Agent + ?Sized> Agent for Box<A> {
    fn name(&self) -> &str {
        (**self).name()
    }

    async fn run(
        &self,
        req: AgentRequest,
        events: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<AgentOutcome> {
        (**self).run(req, events, cancel).await
    }
}

/// Construct the agent the runner should dispatch through, based on
/// `pitboss.toml`'s `[agent] backend` selector.
///
/// A missing or absent `backend` falls back to [`backend::BackendKind::default`]
/// (Claude Code) so workspaces without an `[agent]` section keep today's
/// behavior. Unknown backend strings surface a parse error from
/// [`backend::BackendKind`]'s [`std::str::FromStr`] impl. Backends other than `claude_code` return a
/// clear "not yet implemented" error pending their adapter phases — the
/// dispatch path is in place so wiring those up is a single match arm.
pub fn build_agent(cfg: &crate::config::Config) -> Result<Box<dyn Agent + Send + Sync>> {
    let kind = match cfg.agent.backend.as_deref() {
        None => backend::BackendKind::default(),
        Some(s) => s.parse::<backend::BackendKind>()?,
    };
    match kind {
        backend::BackendKind::ClaudeCode => Ok(Box::new(claude_code::ClaudeCodeAgent::new())),
        backend::BackendKind::Codex => {
            let overrides = &cfg.agent.codex;
            let mut agent = match overrides.binary.as_ref() {
                Some(path) => codex::CodexAgent::with_binary(path),
                None => codex::CodexAgent::new(),
            };
            if !overrides.extra_args.is_empty() {
                agent = agent.with_extra_args(overrides.extra_args.clone());
            }
            if let Some(model) = overrides.model.as_deref() {
                agent = agent.with_model_override(model);
            }
            Ok(Box::new(agent))
        }
        backend::BackendKind::Aider => {
            let overrides = &cfg.agent.aider;
            let mut agent = match overrides.binary.as_ref() {
                Some(path) => aider::AiderAgent::with_binary(path),
                None => aider::AiderAgent::new(),
            };
            if !overrides.extra_args.is_empty() {
                agent = agent.with_extra_args(overrides.extra_args.clone());
            }
            if let Some(model) = overrides.model.as_deref() {
                agent = agent.with_model_override(model);
            }
            Ok(Box::new(agent))
        }
        backend::BackendKind::Gemini => {
            let overrides = &cfg.agent.gemini;
            let mut agent = match overrides.binary.as_ref() {
                Some(path) => gemini::GeminiAgent::with_binary(path),
                None => gemini::GeminiAgent::new(),
            };
            if !overrides.extra_args.is_empty() {
                agent = agent.with_extra_args(overrides.extra_args.clone());
            }
            if let Some(model) = overrides.model.as_deref() {
                agent = agent.with_model_override(model);
            }
            Ok(Box::new(agent))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_as_str_matches_config_keys() {
        assert_eq!(Role::Planner.as_str(), "planner");
        assert_eq!(Role::Implementer.as_str(), "implementer");
        assert_eq!(Role::Auditor.as_str(), "auditor");
        assert_eq!(Role::Fixer.as_str(), "fixer");
    }

    #[test]
    fn role_serde_round_trips_through_lowercase_string() {
        let json = serde_json::to_string(&Role::Implementer).unwrap();
        assert_eq!(json, "\"implementer\"");
        let back: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Role::Implementer);
    }

    #[test]
    fn stop_reason_equality_ignores_completion_payload() {
        assert_eq!(StopReason::Completed, StopReason::Completed);
        assert_ne!(StopReason::Completed, StopReason::Timeout);
        assert_eq!(StopReason::Error("x".into()), StopReason::Error("x".into()));
        assert_ne!(StopReason::Error("x".into()), StopReason::Error("y".into()));
    }

    #[test]
    fn build_agent_defaults_to_claude_code_when_unspecified() {
        let cfg = crate::config::Config::default();
        match build_agent(&cfg) {
            Ok(agent) => assert_eq!(agent.name(), "claude-code"),
            Err(e) => panic!("default config must build the claude_code agent: {e:#}"),
        }
    }

    #[test]
    fn build_agent_dispatches_explicit_claude_code() {
        let mut cfg = crate::config::Config::default();
        cfg.agent.backend = Some("claude_code".to_string());
        match build_agent(&cfg) {
            Ok(agent) => assert_eq!(agent.name(), "claude-code"),
            Err(e) => panic!("explicit claude_code must build: {e:#}"),
        }
    }

    #[test]
    fn build_agent_has_no_pending_backends() {
        // Every named backend in [`backend::BackendKind`] must construct an
        // adapter — pitboss shipped its full backend trio in phases 02–04
        // (codex, aider, gemini) on top of the default claude_code. If a new
        // backend is added to the enum without a matching factory arm this
        // test fails and forces the wiring to land in the same change.
        for name in ["claude_code", "codex", "aider", "gemini"] {
            let mut cfg = crate::config::Config::default();
            cfg.agent.backend = Some(name.to_string());
            assert!(
                build_agent(&cfg).is_ok(),
                "backend {name} must build a concrete agent"
            );
        }
    }

    #[test]
    fn build_agent_dispatches_explicit_codex() {
        // Phase 02 acceptance: setting `[agent] backend = "codex"` must build
        // the CodexAgent adapter rather than the default Claude Code one.
        // `Box<dyn Agent>` hides the concrete type, so we verify via
        // `Agent::name`, which is the same surface the runner logs use.
        let mut cfg = crate::config::Config::default();
        cfg.agent.backend = Some("codex".to_string());
        match build_agent(&cfg) {
            Ok(agent) => assert_eq!(agent.name(), "codex"),
            Err(e) => panic!("explicit codex must build: {e:#}"),
        }
    }

    #[test]
    fn build_agent_dispatches_explicit_aider() {
        // Phase 03 acceptance: setting `[agent] backend = "aider"` must build
        // the AiderAgent adapter rather than the default Claude Code one or
        // erroring out as a not-yet-implemented backend.
        let mut cfg = crate::config::Config::default();
        cfg.agent.backend = Some("aider".to_string());
        match build_agent(&cfg) {
            Ok(agent) => assert_eq!(agent.name(), "aider"),
            Err(e) => panic!("explicit aider must build: {e:#}"),
        }
    }

    #[test]
    fn build_agent_dispatches_explicit_gemini() {
        // Phase 04 acceptance: setting `[agent] backend = "gemini"` must build
        // the GeminiAgent adapter rather than the default Claude Code one or
        // erroring out as a not-yet-implemented backend.
        let mut cfg = crate::config::Config::default();
        cfg.agent.backend = Some("gemini".to_string());
        match build_agent(&cfg) {
            Ok(agent) => assert_eq!(agent.name(), "gemini"),
            Err(e) => panic!("explicit gemini must build: {e:#}"),
        }
    }

    #[test]
    fn build_agent_gemini_honors_overrides() {
        // The `[agent.gemini]` table must reach the constructed agent so tests
        // (and real installs in non-standard locations) can point at a stub
        // script and apply per-backend `extra_args` / `model`.
        let mut cfg = crate::config::Config::default();
        cfg.agent.backend = Some("gemini".to_string());
        cfg.agent.gemini.binary = Some(std::path::PathBuf::from("/tmp/fake-gemini"));
        cfg.agent.gemini.extra_args = vec!["--include-directories".into(), "src".into()];
        cfg.agent.gemini.model = Some("gemini-2.5-flash".into());
        match build_agent(&cfg) {
            Ok(agent) => assert_eq!(agent.name(), "gemini"),
            Err(e) => panic!("gemini with overrides must build: {e:#}"),
        }
    }

    #[test]
    fn build_agent_aider_honors_overrides() {
        // The `[agent.aider]` table must reach the constructed agent so
        // tests (and real installs in non-standard locations) can point at a
        // stub script and apply per-backend `extra_args` / `model`.
        let mut cfg = crate::config::Config::default();
        cfg.agent.backend = Some("aider".to_string());
        cfg.agent.aider.binary = Some(std::path::PathBuf::from("/tmp/fake-aider"));
        cfg.agent.aider.extra_args = vec!["--no-auto-commits".into()];
        cfg.agent.aider.model = Some("anthropic/sonnet-4.5".into());
        match build_agent(&cfg) {
            Ok(agent) => assert_eq!(agent.name(), "aider"),
            Err(e) => panic!("aider with overrides must build: {e:#}"),
        }
    }

    #[test]
    fn build_agent_codex_honors_binary_override() {
        // The `[agent.codex] binary = "..."` override must reach the
        // constructed agent so tests (and real installs in non-standard
        // locations) can point at a stub script. The dispatch path doesn't
        // spawn the binary, so an obviously-fake path is fine here.
        let mut cfg = crate::config::Config::default();
        cfg.agent.backend = Some("codex".to_string());
        cfg.agent.codex.binary = Some(std::path::PathBuf::from("/tmp/fake-codex"));
        cfg.agent.codex.extra_args = vec!["--quiet".into()];
        cfg.agent.codex.model = Some("gpt-5-codex".into());
        match build_agent(&cfg) {
            Ok(agent) => assert_eq!(agent.name(), "codex"),
            Err(e) => panic!("codex with overrides must build: {e:#}"),
        }
    }

    #[test]
    fn build_agent_rejects_unknown_backend() {
        let mut cfg = crate::config::Config::default();
        cfg.agent.backend = Some("ollama".into());
        match build_agent(&cfg) {
            Ok(_) => panic!("unknown backend must not build"),
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("ollama"),
                    "expected unknown-backend error to echo the input, got: {msg}"
                );
            }
        }
    }
}
