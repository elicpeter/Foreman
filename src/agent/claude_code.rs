//! `ClaudeCodeAgent` — production [`Agent`] that drives Anthropic's `claude`
//! CLI in non-interactive (`-p` / `--print`) mode and parses its streaming
//! JSON event protocol.
//!
//! ## How to install / configure `claude`
//!
//! Foreman shells out to whatever `claude` binary is on `PATH` (or the path
//! you pass to [`ClaudeCodeAgent::with_binary`]). Install per Anthropic's
//! Claude Code docs and authenticate (`claude auth login`) before running
//! foreman. To verify the install, run `claude -p "hello" --output-format
//! stream-json --verbose --model haiku` from a shell — foreman invokes the
//! binary with the same flag set.
//!
//! Foreman runs the agent under `--permission-mode auto` for safe unattended
//! orchestration. If you want a different mode, construct the agent with
//! [`ClaudeCodeAgent::with_permission_mode`].
//!
//! ## Event mapping
//!
//! `claude --output-format stream-json` emits one JSON object per line. We
//! map the subset we care about to [`AgentEvent`]:
//!
//! - `assistant` messages — for each block in `message.content`:
//!   - `text` → [`AgentEvent::Stdout`] carrying the text body.
//!   - `tool_use` → [`AgentEvent::ToolUse`] carrying the tool name.
//!   - `thinking` → dropped (noisy; still in the log file for post-mortem).
//! - `result` (final event) — token totals are extracted into a single
//!   [`AgentEvent::TokenDelta`] and folded into the returned [`AgentOutcome`].
//!   `is_error: true` results map to [`StopReason::Error`].
//! - Everything else (`system`, `user`, `rate_limit_event`, …) is ignored at
//!   the event channel; the raw line still hits `log_path` via
//!   [`subprocess::run_logged`].
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
const DEFAULT_BINARY: &str = "claude";

/// How many trailing stderr lines to attach to a [`StopReason::Error`] when
/// the process exits non-zero. Bounded so a chatty error doesn't flood the
/// runner log.
const ERROR_TAIL_LINES: usize = 8;

/// Production [`Agent`] that drives the `claude` CLI.
#[derive(Debug, Clone)]
pub struct ClaudeCodeAgent {
    binary: PathBuf,
    permission_mode: String,
}

impl ClaudeCodeAgent {
    /// Construct an agent that resolves `claude` from `PATH`.
    pub fn new() -> Self {
        Self {
            binary: PathBuf::from(DEFAULT_BINARY),
            permission_mode: "auto".to_string(),
        }
    }

    /// Construct an agent that invokes a specific binary path. Useful for
    /// tests (point at a fixture script) and for users with a non-standard
    /// install location.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            permission_mode: "auto".to_string(),
        }
    }

    /// Override the `--permission-mode` flag passed to `claude`. The default
    /// is `auto`; other valid values are `acceptEdits`, `bypassPermissions`,
    /// `default`, `dontAsk`, `plan`.
    pub fn with_permission_mode(mut self, mode: impl Into<String>) -> Self {
        self.permission_mode = mode.into();
        self
    }

    /// Path to the binary this agent will invoke.
    pub fn binary(&self) -> &PathBuf {
        &self.binary
    }
}

impl Default for ClaudeCodeAgent {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Agent for ClaudeCodeAgent {
    fn name(&self) -> &str {
        "claude-code"
    }

    async fn run(
        &self,
        req: AgentRequest,
        events: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<AgentOutcome> {
        let log_path = req.log_path.clone();
        let cmd = self.build_command(&req);

        // Raw subprocess events flow through `raw_*` then a forwarder turns
        // each stdout line into the appropriate semantic AgentEvent. Stderr
        // is forwarded verbatim and tee'd to a buffer so we can quote it back
        // in the StopReason::Error message on a bad exit.
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
            ForwarderResult {
                tokens,
                error_message,
                stderr_tail,
            }
        });

        let sub_outcome: SubprocessOutcome =
            subprocess::run_logged(cmd, &log_path, raw_tx, cancel, req.timeout).await?;
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
        // the runner doesn't have to special-case Claude's outcome shape.
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

impl ClaudeCodeAgent {
    fn build_command(&self, req: &AgentRequest) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&req.workdir);
        cmd.args(["--print", "--output-format", "stream-json", "--verbose"]);
        cmd.args(["--model", &req.model]);
        cmd.args(["--permission-mode", &self.permission_mode]);
        if !req.system_prompt.is_empty() {
            cmd.arg("--append-system-prompt").arg(&req.system_prompt);
        }
        // Positional prompt argument comes last so flag parsing doesn't get
        // confused if the prompt happens to start with `--`.
        cmd.arg("--").arg(&req.user_prompt);
        cmd
    }
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

    let kind = parsed.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "assistant" => {
            if let Some(content) = parsed
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for block in content {
                    let bk = block.get("type").and_then(Value::as_str).unwrap_or("");
                    match bk {
                        "text" => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    let _ =
                                        outbound.send(AgentEvent::Stdout(text.to_string())).await;
                                }
                            }
                        }
                        "tool_use" => {
                            if let Some(name) = block.get("name").and_then(Value::as_str) {
                                let _ = outbound.send(AgentEvent::ToolUse(name.to_string())).await;
                            }
                        }
                        // "thinking" and unknown blocks: log file already has
                        // them, no point flooding the event channel.
                        _ => {}
                    }
                }
            }
        }
        "result" => {
            // Final event — pull totals out and emit one TokenDelta with the
            // grand total. The runner aggregates by summing TokenDeltas, so
            // we only emit once and never double-count.
            if let Some(usage) = parsed.get("usage") {
                let new_input = sum_input_tokens(usage);
                let new_output = read_u64(usage, "output_tokens");
                if new_input != tokens.input || new_output != tokens.output {
                    tokens.input = new_input;
                    tokens.output = new_output;
                    let _ = outbound.send(AgentEvent::TokenDelta(tokens.clone())).await;
                }
            }
            let is_error = parsed
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_error {
                let msg = parsed
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        parsed
                            .get("subtype")
                            .and_then(Value::as_str)
                            .unwrap_or("claude reported an error")
                            .to_string()
                    });
                *error_message = Some(msg);
            }
        }
        // Other event kinds (system init, rate_limit_event, user tool
        // results, etc.) are intentionally dropped at the event channel.
        _ => {}
    }
}

fn sum_input_tokens(usage: &Value) -> u64 {
    read_u64(usage, "input_tokens")
        + read_u64(usage, "cache_creation_input_tokens")
        + read_u64(usage, "cache_read_input_tokens")
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
        Some(m) if !m.is_empty() => format!("claude: {} (exit {})", m, exit_code),
        _ => format!("claude exited with code {}", exit_code),
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
        manifest.join("tests").join("fixtures").join(name)
    }

    fn req_with_log(log_path: PathBuf, timeout: Duration) -> AgentRequest {
        AgentRequest {
            role: Role::Implementer,
            model: "claude-haiku-test".into(),
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
    async fn parses_assistant_text_and_tool_use_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = ClaudeCodeAgent::with_binary(fixture_path("fake-claude-success.sh"));
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
            stdouts.iter().any(|s| s.contains("Hello from Claude")),
            "missing assistant text: {stdouts:?}"
        );
        assert_eq!(tool_uses, vec!["Bash", "Read"]);
        assert_eq!(token_deltas.len(), 1);
        let total = token_deltas[0];
        // From fixture: input_tokens=10, cache_creation=20, cache_read=5,
        // output_tokens=51 — input total is 35.
        assert_eq!(total.input, 35);
        assert_eq!(total.output, 51);

        // by_role re-keyed onto Role::Implementer at the agent level.
        assert_eq!(outcome.tokens.input, 35);
        assert_eq!(outcome.tokens.output, 51);
        let role_usage = outcome
            .tokens
            .by_role
            .get("implementer")
            .expect("implementer role usage");
        assert_eq!(role_usage.input, 35);
        assert_eq!(role_usage.output, 51);

        // Log file should contain raw JSON for post-mortem.
        let log_text = std::fs::read_to_string(&log).unwrap();
        assert!(log_text.contains("\"type\":\"assistant\""), "{log_text}");
        assert!(log_text.contains("\"type\":\"result\""), "{log_text}");
    }

    #[tokio::test]
    async fn maps_is_error_result_to_error_stop_reason() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = ClaudeCodeAgent::with_binary(fixture_path("fake-claude-error.sh"));
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
        let agent = ClaudeCodeAgent::with_binary(fixture_path("fake-claude-nonjson.sh"));
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
    async fn nonzero_exit_without_result_event_maps_to_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = ClaudeCodeAgent::with_binary(fixture_path("fake-claude-crash.sh"));
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
        let agent = ClaudeCodeAgent::with_binary(fixture_path("fake-claude-hang.sh"));
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
        // No subprocess spawn here — we just inspect the constructed Command.
        let agent = ClaudeCodeAgent::with_binary("/usr/local/bin/claude")
            .with_permission_mode("acceptEdits");
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let req = AgentRequest {
            role: Role::Auditor,
            model: "claude-opus-4-7".into(),
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
        assert!(args.iter().any(|a| a == "--print"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--output-format" && w[1] == "stream-json"));
        assert!(args.iter().any(|a| a == "--verbose"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--model" && w[1] == "claude-opus-4-7"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--permission-mode" && w[1] == "acceptEdits"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--append-system-prompt" && w[1] == "system body"));
        // Positional prompt fenced behind `--` so it can't be misread.
        assert!(args.windows(2).any(|w| w[0] == "--" && w[1] == "user body"));
        assert_eq!(std_cmd.get_program(), "/usr/local/bin/claude");
        assert_eq!(std_cmd.get_current_dir(), Some(dir.path()));
    }

    #[tokio::test]
    async fn build_command_omits_append_system_prompt_when_empty() {
        let agent = ClaudeCodeAgent::with_binary("claude");
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let req = AgentRequest {
            role: Role::Implementer,
            model: "claude-sonnet".into(),
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
        assert!(!args.iter().any(|a| a == "--append-system-prompt"));
    }

    /// Real end-to-end test against the actual `claude` binary on PATH.
    /// Skipped unless `FOREMAN_REAL_AGENT_TESTS=1` so CI doesn't burn tokens.
    #[tokio::test]
    async fn real_claude_smoke_test() {
        if std::env::var("FOREMAN_REAL_AGENT_TESTS").ok().as_deref() != Some("1") {
            eprintln!("skipping real_claude_smoke_test (set FOREMAN_REAL_AGENT_TESTS=1 to run)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = ClaudeCodeAgent::new();
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let req = AgentRequest {
            role: Role::Implementer,
            model: "haiku".into(),
            system_prompt: String::new(),
            user_prompt: "respond with the single word OK".into(),
            workdir: dir.path().to_path_buf(),
            log_path: log,
            timeout: Duration::from_secs(120),
        };
        let outcome = agent.run(req, tx, cancel).await.unwrap();
        assert!(
            matches!(outcome.stop_reason, StopReason::Completed),
            "real claude run did not complete: {:?}",
            outcome.stop_reason
        );
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.tokens.output > 0, "no output tokens reported");
    }
}
