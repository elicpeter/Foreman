//! `CodexAgent` — production [`Agent`] that drives OpenAI's `codex` CLI in
//! non-interactive (`exec`) mode and parses its newline-delimited JSON event
//! stream.
//!
//! ## How to install / configure `codex`
//!
//! Pitboss shells out to whatever `codex` binary is on `PATH` (or the path you
//! pass via `[agent.codex] binary` in `pitboss.toml`). Install per OpenAI's
//! Codex CLI docs and authenticate (`codex login`) before running pitboss. To
//! verify the install, `codex exec --json --model o4-mini --skip-git-repo-check -`
//! reading `hello` from stdin works from a shell — pitboss invokes the binary
//! with the same flag shape.
//!
//! Pitboss runs the agent under `--ask-for-approval never` and
//! `--skip-git-repo-check` so it never blocks on an interactive prompt and so
//! it works inside scratch worktrees that aren't full git repos. Override via
//! `[agent.codex] extra_args = […]` if you need different policy flags.
//!
//! ## Prompt assembly
//!
//! Codex has no separate system-prompt channel, so [`AgentRequest::system_prompt`]
//! and [`AgentRequest::user_prompt`] are concatenated — system first, blank line,
//! then user — and the whole payload is written to the child's stdin. The CLI
//! is invoked with the trailing `-` sigil to read its prompt from stdin.
//!
//! ## Event mapping
//!
//! `codex exec --json` emits one JSON object per line. Each object has an `id`
//! and a `msg` field; we dispatch on `msg.type`:
//!
//! - `agent_message` (`message`: string) → [`AgentEvent::Stdout`].
//! - `exec_command_begin` (`command`: array of strings) → [`AgentEvent::ToolUse`]
//!   carrying the first argv element (or the full string if it's a single
//!   `sh -c "…"` form).
//! - `mcp_tool_call_begin` (`server`, `tool`) → [`AgentEvent::ToolUse`] carrying
//!   `<server>.<tool>` so MCP-driven runs surface alongside shell tool calls.
//! - `patch_apply_begin` → [`AgentEvent::ToolUse`]`("patch")` so file edits show
//!   in the dashboard the same way Claude's `Edit` tool does.
//! - `token_count` (`info.total_token_usage` with `input_tokens`,
//!   `cached_input_tokens`, `output_tokens`) → folded into a running
//!   [`TokenUsage`]; one [`AgentEvent::TokenDelta`] is emitted at the end so
//!   the runner doesn't double-count interim updates.
//! - `task_complete` — terminal success marker; the next event will be EOF.
//! - `error` (`message`: string) — terminal failure marker; produces
//!   [`StopReason::Error`] with the supplied message.
//! - `agent_reasoning`, `exec_command_end`, `task_started`, and other event
//!   kinds are intentionally dropped at the channel; they remain in the log
//!   file via [`subprocess::run_logged_with_stdin`].
//!
//! Lines that fail to parse as JSON are forwarded as
//! [`AgentEvent::Stdout`] verbatim so unexpected output stays visible.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::state::TokenUsage;

use super::{
    subprocess::{self, SubprocessOutcome},
    Agent, AgentEvent, AgentOutcome, AgentRequest, StopReason,
};

/// Default binary name. Resolved against `PATH` by the OS.
const DEFAULT_BINARY: &str = "codex";

/// How many trailing stderr lines to attach to a [`StopReason::Error`] when
/// the process exits non-zero. Bounded so a chatty error doesn't flood the
/// runner log.
const ERROR_TAIL_LINES: usize = 8;

/// Production [`Agent`] that drives the `codex` CLI.
#[derive(Debug, Clone)]
pub struct CodexAgent {
    binary: PathBuf,
    extra_args: Vec<String>,
    model_override: Option<String>,
}

impl CodexAgent {
    /// Construct an agent that resolves `codex` from `PATH`.
    pub fn new() -> Self {
        Self {
            binary: PathBuf::from(DEFAULT_BINARY),
            extra_args: Vec::new(),
            model_override: None,
        }
    }

    /// Construct an agent that invokes a specific binary path. Useful for
    /// tests (point at a fixture script) and for users with a non-standard
    /// install location.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            extra_args: Vec::new(),
            model_override: None,
        }
    }

    /// Append extra argv that gets spliced in just before the `-` stdin sigil
    /// on every invocation. Mirrors `[agent.codex] extra_args` in
    /// `pitboss.toml`.
    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    /// Override the model identifier with a value from `[agent.codex] model`.
    /// When set this beats the per-role model in [`AgentRequest::model`] —
    /// users who configure a backend-specific model expect it to be used for
    /// every dispatch through that backend.
    pub fn with_model_override(mut self, model: impl Into<String>) -> Self {
        self.model_override = Some(model.into());
        self
    }

    /// Path to the binary this agent will invoke.
    pub fn binary(&self) -> &PathBuf {
        &self.binary
    }
}

impl Default for CodexAgent {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Agent for CodexAgent {
    fn name(&self) -> &str {
        "codex"
    }

    async fn run(
        &self,
        req: AgentRequest,
        events: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<AgentOutcome> {
        let log_path = req.log_path.clone();
        let stdin_payload = build_stdin_payload(&req);
        let cmd = self.build_command(&req);

        // Raw subprocess events flow through `raw_*` then a forwarder turns
        // each stdout line into the appropriate semantic AgentEvent. Stderr is
        // forwarded verbatim and tee'd to a buffer so we can quote it back in
        // the StopReason::Error message on a bad exit.
        let (raw_tx, mut raw_rx) = mpsc::channel::<AgentEvent>(64);
        let outbound = events.clone();
        let forwarder = tokio::spawn(async move {
            let mut tokens = TokenUsage::default();
            let mut error_message: Option<String> = None;
            let mut stderr_tail: Vec<String> = Vec::new();
            while let Some(ev) = raw_rx.recv().await {
                match ev {
                    AgentEvent::Stdout(line) => {
                        handle_stdout_line(&line, &outbound, &mut tokens, &mut error_message).await;
                    }
                    AgentEvent::Stderr(line) => {
                        push_tail(&mut stderr_tail, line.clone(), ERROR_TAIL_LINES);
                        let _ = outbound.send(AgentEvent::Stderr(line)).await;
                    }
                    other => {
                        let _ = outbound.send(other).await;
                    }
                }
            }
            // Emit one TokenDelta with the grand total at the end so the
            // runner's accumulator doesn't double-count interim `token_count`
            // events from the model.
            if tokens.input > 0 || tokens.output > 0 {
                let _ = outbound.send(AgentEvent::TokenDelta(tokens.clone())).await;
            }
            ForwarderResult {
                tokens,
                error_message,
                stderr_tail,
            }
        });

        let sub_outcome: SubprocessOutcome = subprocess::run_logged_with_stdin(
            cmd,
            &log_path,
            raw_tx,
            cancel,
            req.timeout,
            Some(stdin_payload),
        )
        .await?;
        let ForwarderResult {
            mut tokens,
            error_message,
            stderr_tail,
        } = forwarder.await.unwrap_or(ForwarderResult {
            tokens: TokenUsage::default(),
            error_message: None,
            stderr_tail: Vec::new(),
        });
        // by_role isn't populated by the model itself — re-key once here so
        // the runner doesn't have to special-case Codex's outcome shape.
        if tokens.input > 0 || tokens.output > 0 {
            tokens
                .by_role
                .entry(req.role.as_str().to_string())
                .or_default();
            let entry = tokens
                .by_role
                .get_mut(req.role.as_str())
                .expect("just inserted");
            entry.input = tokens.input;
            entry.output = tokens.output;
        }

        let stop_reason = match sub_outcome.stop_reason {
            StopReason::Completed => {
                if sub_outcome.exit_code == 0 && error_message.is_none() {
                    StopReason::Completed
                } else {
                    StopReason::Error(format_error_message(
                        sub_outcome.exit_code,
                        error_message.as_deref(),
                        &stderr_tail,
                    ))
                }
            }
            // Pass the terminator decided by the subprocess helper through
            // unchanged — timeout and cancel beat any error inferred from
            // partial output.
            other => other,
        };

        Ok(AgentOutcome {
            exit_code: sub_outcome.exit_code,
            stop_reason,
            tokens,
            log_path,
        })
    }
}

impl CodexAgent {
    fn build_command(&self, req: &AgentRequest) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&req.workdir);
        cmd.arg("exec");
        cmd.args(["--json", "--skip-git-repo-check"]);
        cmd.args(["--ask-for-approval", "never"]);
        let model = self.model_override.as_deref().unwrap_or(&req.model);
        cmd.args(["--model", model]);
        for arg in &self.extra_args {
            cmd.arg(arg);
        }
        // Trailing `-` tells `codex exec` to read the prompt from stdin.
        cmd.arg("-");
        cmd
    }
}

fn build_stdin_payload(req: &AgentRequest) -> Vec<u8> {
    let mut out = String::new();
    if !req.system_prompt.is_empty() {
        out.push_str(&req.system_prompt);
        out.push_str("\n\n");
    }
    out.push_str(&req.user_prompt);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.into_bytes()
}

struct ForwarderResult {
    tokens: TokenUsage,
    error_message: Option<String>,
    stderr_tail: Vec<String>,
}

async fn handle_stdout_line(
    line: &str,
    outbound: &mpsc::Sender<AgentEvent>,
    tokens: &mut TokenUsage,
    error_message: &mut Option<String>,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let parsed: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            // Not JSON: surface verbatim so unexpected output stays visible.
            let _ = outbound.send(AgentEvent::Stdout(line.to_string())).await;
            return;
        }
    };

    let msg = match parsed.get("msg") {
        Some(m) => m,
        None => return,
    };
    let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "agent_message" => {
            if let Some(text) = msg.get("message").and_then(Value::as_str) {
                if !text.is_empty() {
                    let _ = outbound.send(AgentEvent::Stdout(text.to_string())).await;
                }
            }
        }
        "exec_command_begin" => {
            let label = msg
                .get("command")
                .and_then(Value::as_array)
                .and_then(|argv| argv.first().and_then(Value::as_str).map(|s| s.to_string()))
                .unwrap_or_else(|| "exec".to_string());
            let _ = outbound.send(AgentEvent::ToolUse(label)).await;
        }
        "mcp_tool_call_begin" => {
            let server = msg.get("server").and_then(Value::as_str).unwrap_or("");
            let tool = msg.get("tool").and_then(Value::as_str).unwrap_or("");
            let label = match (server.is_empty(), tool.is_empty()) {
                (true, true) => "mcp".to_string(),
                (true, false) => tool.to_string(),
                (false, true) => server.to_string(),
                (false, false) => format!("{server}.{tool}"),
            };
            let _ = outbound.send(AgentEvent::ToolUse(label)).await;
        }
        "patch_apply_begin" => {
            let _ = outbound
                .send(AgentEvent::ToolUse("patch".to_string()))
                .await;
        }
        "token_count" => {
            // Sum the cumulative `total_token_usage` and overwrite the running
            // counter — codex emits a running total each turn, not a delta.
            if let Some(usage) = msg
                .get("info")
                .and_then(|info| info.get("total_token_usage"))
            {
                tokens.input = sum_input_tokens(usage);
                tokens.output = read_u64(usage, "output_tokens");
            }
        }
        "error" => {
            let msg_text = msg
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| "codex reported an error".to_string());
            *error_message = Some(msg_text);
        }
        // `task_complete`, `task_started`, `agent_reasoning`,
        // `exec_command_end`, `mcp_tool_call_end`, `patch_apply_end`, etc. are
        // intentionally dropped at the channel — the log file already has them
        // for post-mortem.
        _ => {}
    }
}

fn sum_input_tokens(usage: &Value) -> u64 {
    // Codex's `total_token_usage` separates `input_tokens` (uncached) from
    // `cached_input_tokens` (cache hits). Both bill differently but for the
    // dashboard's running total we sum them — same shape the Claude parser
    // produces from `input_tokens + cache_creation + cache_read`.
    read_u64(usage, "input_tokens") + read_u64(usage, "cached_input_tokens")
}

fn read_u64(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn push_tail(buf: &mut Vec<String>, line: String, max: usize) {
    if buf.len() == max {
        buf.remove(0);
    }
    buf.push(line);
}

fn format_error_message(exit_code: i32, parsed: Option<&str>, stderr_tail: &[String]) -> String {
    let mut out = match parsed {
        Some(m) if !m.is_empty() => format!("codex: {} (exit {})", m, exit_code),
        _ => format!("codex exited with code {}", exit_code),
    };
    if !stderr_tail.is_empty() {
        out.push_str("\nstderr tail:\n");
        for line in stderr_tail {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent::Role;
    use std::path::PathBuf;
    use std::time::Duration;

    fn fixture_path(name: &str) -> PathBuf {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest
            .join("tests")
            .join("fixtures")
            .join("codex")
            .join(name)
    }

    fn req_with_log(log_path: PathBuf, timeout: Duration) -> AgentRequest {
        AgentRequest {
            role: Role::Implementer,
            model: "o4-mini-test".into(),
            system_prompt: "be brief".into(),
            user_prompt: "say hi".into(),
            workdir: std::env::temp_dir(),
            log_path,
            timeout,
        }
    }

    async fn drain<T>(mut rx: mpsc::Receiver<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let Some(v) = rx.recv().await {
            out.push(v);
        }
        out
    }

    #[tokio::test]
    async fn parses_agent_message_and_tool_use_events() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = CodexAgent::with_binary(fixture_path("fake-codex-success.sh"));
        let (tx, rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let outcome = agent
            .run(
                req_with_log(log.clone(), Duration::from_secs(5)),
                tx,
                cancel,
            )
            .await
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.exit_code, 0);

        let evs = drain(rx).await;
        let stdouts: Vec<&str> = evs
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Stdout(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        let tool_uses: Vec<&str> = evs
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolUse(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        let token_deltas: Vec<&TokenUsage> = evs
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TokenDelta(t) => Some(t),
                _ => None,
            })
            .collect();

        assert!(
            stdouts.iter().any(|s| s.contains("Hello from Codex")),
            "missing assistant text: {stdouts:?}"
        );
        assert_eq!(tool_uses, vec!["bash", "patch"]);
        assert_eq!(token_deltas.len(), 1);
        let total = token_deltas[0];
        // From fixture: input_tokens=12, cached_input_tokens=8, output_tokens=37
        // → input total is 20.
        assert_eq!(total.input, 20);
        assert_eq!(total.output, 37);

        // by_role re-keyed onto Role::Implementer at the agent level.
        assert_eq!(outcome.tokens.input, 20);
        assert_eq!(outcome.tokens.output, 37);
        let role_usage = outcome
            .tokens
            .by_role
            .get("implementer")
            .expect("implementer role usage");
        assert_eq!(role_usage.input, 20);
        assert_eq!(role_usage.output, 37);

        // Log file should contain raw JSON for post-mortem.
        let log_text = std::fs::read_to_string(&log).unwrap();
        assert!(
            log_text.contains("\"type\":\"agent_message\""),
            "{log_text}"
        );
        assert!(
            log_text.contains("\"type\":\"task_complete\""),
            "{log_text}"
        );
    }

    #[tokio::test]
    async fn maps_error_event_to_error_stop_reason() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = CodexAgent::with_binary(fixture_path("fake-codex-error.sh"));
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let outcome = agent
            .run(req_with_log(log, Duration::from_secs(5)), tx, cancel)
            .await
            .unwrap();
        match outcome.stop_reason {
            StopReason::Error(msg) => {
                assert!(
                    msg.contains("rate limit"),
                    "expected rate limit message, got: {msg}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(outcome.exit_code, 2);
    }

    #[tokio::test]
    async fn nonjson_stdout_is_forwarded_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = CodexAgent::with_binary(fixture_path("fake-codex-nonjson.sh"));
        let (tx, rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let outcome = agent
            .run(req_with_log(log, Duration::from_secs(5)), tx, cancel)
            .await
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        let evs = drain(rx).await;
        let stdouts: Vec<&str> = evs
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Stdout(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            stdouts.contains(&"not-json output line"),
            "expected raw line, got {stdouts:?}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_without_error_event_maps_to_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = CodexAgent::with_binary(fixture_path("fake-codex-crash.sh"));
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let outcome = agent
            .run(req_with_log(log, Duration::from_secs(5)), tx, cancel)
            .await
            .unwrap();
        match outcome.stop_reason {
            StopReason::Error(msg) => {
                assert!(msg.contains("exit"), "{msg}");
                assert!(msg.contains("authentication required"), "{msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(outcome.exit_code, 1);
    }

    #[tokio::test]
    async fn cancellation_propagates_to_child_process() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = CodexAgent::with_binary(fixture_path("fake-codex-hang.sh"));
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let canceler = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            canceler.cancel();
        });
        let outcome = agent
            .run(req_with_log(log, Duration::from_secs(30)), tx, cancel)
            .await
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Cancelled);
        assert_eq!(outcome.exit_code, -1);
    }

    #[tokio::test]
    async fn build_command_includes_required_flags_and_workdir() {
        let agent = CodexAgent::with_binary("/usr/local/bin/codex")
            .with_extra_args(vec!["--quiet".into(), "--json-trace".into()])
            .with_model_override("gpt-5-codex");
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let req = AgentRequest {
            role: Role::Auditor,
            model: "ignored-because-override".into(),
            system_prompt: "system body".into(),
            user_prompt: "user body".into(),
            workdir: dir.path().to_path_buf(),
            log_path: log,
            timeout: Duration::from_secs(1),
        };
        let cmd = agent.build_command(&req);
        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        assert!(args.iter().any(|a| a == "--json"));
        assert!(args.iter().any(|a| a == "--skip-git-repo-check"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--ask-for-approval" && w[1] == "never"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--model" && w[1] == "gpt-5-codex"));
        assert!(args.iter().any(|a| a == "--quiet"));
        assert!(args.iter().any(|a| a == "--json-trace"));
        // Trailing `-` reads the prompt from stdin.
        assert_eq!(args.last().map(String::as_str), Some("-"));
        assert_eq!(std_cmd.get_program(), "/usr/local/bin/codex");
        assert_eq!(std_cmd.get_current_dir(), Some(dir.path()));
    }

    #[tokio::test]
    async fn build_command_uses_request_model_when_no_override() {
        let agent = CodexAgent::with_binary("codex");
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let req = AgentRequest {
            role: Role::Implementer,
            model: "o4-mini".into(),
            system_prompt: String::new(),
            user_prompt: "u".into(),
            workdir: dir.path().to_path_buf(),
            log_path: log,
            timeout: Duration::from_secs(1),
        };
        let cmd = agent.build_command(&req);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--model" && w[1] == "o4-mini"));
    }

    #[test]
    fn build_stdin_payload_concatenates_system_and_user_with_blank_line() {
        let req = AgentRequest {
            role: Role::Implementer,
            model: "x".into(),
            system_prompt: "you are a careful engineer".into(),
            user_prompt: "implement phase 02".into(),
            workdir: std::env::temp_dir(),
            log_path: std::env::temp_dir().join("never.log"),
            timeout: Duration::from_secs(1),
        };
        let payload = String::from_utf8(build_stdin_payload(&req)).unwrap();
        assert!(payload.starts_with("you are a careful engineer\n\n"));
        assert!(payload.contains("implement phase 02"));
        assert!(payload.ends_with('\n'));
    }

    #[test]
    fn build_stdin_payload_omits_system_when_empty() {
        let req = AgentRequest {
            role: Role::Implementer,
            model: "x".into(),
            system_prompt: String::new(),
            user_prompt: "just the user body".into(),
            workdir: std::env::temp_dir(),
            log_path: std::env::temp_dir().join("never.log"),
            timeout: Duration::from_secs(1),
        };
        let payload = String::from_utf8(build_stdin_payload(&req)).unwrap();
        assert_eq!(payload, "just the user body\n");
    }

    /// Real end-to-end test against the actual `codex` binary on PATH.
    /// Skipped unless `PITBOSS_REAL_AGENT_TESTS=1` so CI doesn't burn tokens.
    #[tokio::test]
    async fn real_codex_smoke_test() {
        if std::env::var("PITBOSS_REAL_AGENT_TESTS").ok().as_deref() != Some("1") {
            eprintln!("skipping real_codex_smoke_test (set PITBOSS_REAL_AGENT_TESTS=1 to run)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = CodexAgent::new();
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let req = AgentRequest {
            role: Role::Implementer,
            model: "o4-mini".into(),
            system_prompt: String::new(),
            user_prompt: "respond with the single word OK".into(),
            workdir: dir.path().to_path_buf(),
            log_path: log,
            timeout: Duration::from_secs(120),
        };
        let outcome = agent.run(req, tx, cancel).await.unwrap();
        assert!(
            matches!(outcome.stop_reason, StopReason::Completed),
            "real codex run did not complete: {:?}",
            outcome.stop_reason
        );
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.tokens.output > 0, "no output tokens reported");
    }
}
