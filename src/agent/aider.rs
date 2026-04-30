//! `AiderAgent` — production [`Agent`] that drives the `aider` CLI in
//! non-interactive (`--message`) mode and parses its plain-text stdout into
//! pitboss's [`AgentEvent`] / [`AgentOutcome`] vocabulary.
//!
//! ## How to install / configure `aider`
//!
//! Pitboss shells out to whatever `aider` binary is on `PATH` (or the path you
//! pass via `[agent.aider] binary` in `pitboss.toml`). Install per Aider's
//! docs (`pip install aider-chat` or `pipx install aider-chat`) and configure
//! whichever `*_API_KEY` env var matches the active model before running
//! pitboss.
//!
//! Pitboss runs the agent under `--yes-always` so it never blocks on a
//! confirmation prompt, `--no-pretty --no-stream` so stdout is line-oriented
//! plain text suitable for parsing, and `--no-check-update
//! --no-show-model-warnings --analytics-disable` so the very first run on a
//! fresh machine doesn't stall on a one-time interactive prompt. Override via
//! `[agent.aider] extra_args = […]` if a workspace needs different defaults.
//!
//! ## Prompt assembly
//!
//! Aider has no separate system-prompt channel (see `deferred.md`'s phase 19
//! audit note), so [`AgentRequest::system_prompt`] and
//! [`AgentRequest::user_prompt`] are concatenated — system first, blank line,
//! then user — and the whole payload is passed via `--message <body>`. We use
//! the inline `--message` flag rather than `--message-file` so there's no temp
//! file to clean up; the OS's `ARG_MAX` (≥256 KB on every platform pitboss
//! supports) is comfortably above the prompts the runner produces.
//!
//! ## Event mapping
//!
//! Aider's stdout is plain text — there is no structured/JSON output mode at
//! the time of writing. Every line is forwarded as an [`AgentEvent::Stdout`]
//! verbatim so the dashboard surfaces the run's narrative. In addition, three
//! prefix patterns are recognized as side-channel events:
//!
//! - `Applied edit to <path>` → [`AgentEvent::ToolUse("edit")`]. Mirrors the
//!   way [`super::claude_code`] emits Claude's `Edit` tool and
//!   [`super::codex`] emits `patch` — runs that touch files surface in the
//!   dashboard alongside other backends.
//! - `Commit <sha> <message>` → [`AgentEvent::ToolUse("commit")`]. Aider does
//!   its own `git commit` after applying edits unless `--no-auto-commits` is
//!   passed via `extra_args`; surfacing those commits as tool-use events
//!   keeps parity with the runner's other dashboards.
//! - `Tokens: <n> sent, <n> received` (with optional `k` / `M` suffixes) →
//!   folded into a running [`TokenUsage`] total. One [`AgentEvent::TokenDelta`]
//!   is emitted at the end so the runner doesn't double-count cross-turn
//!   reports.
//!
//! Errors: aider has no structured error event. A non-zero exit code or a
//! drained stderr tail produces a [`StopReason::Error`] formatted with the
//! same shape Codex/Claude use; the parsed text on stdout is left untouched.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::state::TokenUsage;

use super::{
    subprocess::{self, SubprocessOutcome},
    Agent, AgentEvent, AgentOutcome, AgentRequest, StopReason,
};

/// Default binary name. Resolved against `PATH` by the OS.
const DEFAULT_BINARY: &str = "aider";

/// How many trailing stderr lines to attach to a [`StopReason::Error`] when
/// the process exits non-zero. Bounded so a chatty error doesn't flood the
/// runner log.
const ERROR_TAIL_LINES: usize = 8;

/// Production [`Agent`] that drives the `aider` CLI.
#[derive(Debug, Clone)]
pub struct AiderAgent {
    binary: PathBuf,
    extra_args: Vec<String>,
    model_override: Option<String>,
}

impl AiderAgent {
    /// Construct an agent that resolves `aider` from `PATH`.
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

    /// Append extra argv that gets spliced in just before the `--message`
    /// flag on every invocation. Mirrors `[agent.aider] extra_args` in
    /// `pitboss.toml`. Use this to enable `--no-auto-commits`, scope the run
    /// to specific files (`--file path`), or pass any flag the default set
    /// doesn't cover.
    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    /// Override the model identifier with a value from `[agent.aider] model`.
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

impl Default for AiderAgent {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Agent for AiderAgent {
    fn name(&self) -> &str {
        "aider"
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
        // each stdout line into the appropriate semantic AgentEvent. Stderr is
        // forwarded verbatim and tee'd to a buffer so we can quote it back in
        // the StopReason::Error message on a bad exit.
        let (raw_tx, mut raw_rx) = mpsc::channel::<AgentEvent>(64);
        let outbound = events.clone();
        let forwarder = tokio::spawn(async move {
            let mut tokens = TokenUsage::default();
            let mut stderr_tail: Vec<String> = Vec::new();
            while let Some(ev) = raw_rx.recv().await {
                match ev {
                    AgentEvent::Stdout(line) => {
                        handle_stdout_line(&line, &outbound, &mut tokens).await;
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
            // runner's accumulator doesn't double-count interim "Tokens:"
            // reports the model prints across turns.
            if tokens.input > 0 || tokens.output > 0 {
                let _ = outbound.send(AgentEvent::TokenDelta(tokens.clone())).await;
            }
            ForwarderResult {
                tokens,
                stderr_tail,
            }
        });

        let sub_outcome: SubprocessOutcome =
            subprocess::run_logged(cmd, &log_path, raw_tx, cancel, req.timeout).await?;
        let ForwarderResult {
            mut tokens,
            stderr_tail,
        } = forwarder.await.unwrap_or(ForwarderResult {
            tokens: TokenUsage::default(),
            stderr_tail: Vec::new(),
        });
        // by_role isn't populated by the model itself — re-key once here so
        // the runner doesn't have to special-case Aider's outcome shape.
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
                if sub_outcome.exit_code == 0 {
                    StopReason::Completed
                } else {
                    StopReason::Error(format_error_message(sub_outcome.exit_code, &stderr_tail))
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

impl AiderAgent {
    fn build_command(&self, req: &AgentRequest) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&req.workdir);
        // Unattended-friendly defaults: no TTY-only formatting, no streaming
        // chunks (we want whole lines), auto-confirm every prompt, suppress
        // first-run / version-check / model-warning interactivity. Anything a
        // workspace needs to flip can be supplied via [`Self::with_extra_args`].
        cmd.args([
            "--no-pretty",
            "--no-stream",
            "--yes-always",
            "--no-check-update",
            "--no-show-model-warnings",
            "--analytics-disable",
        ]);
        let model = self.model_override.as_deref().unwrap_or(&req.model);
        cmd.args(["--model", model]);
        for arg in &self.extra_args {
            cmd.arg(arg);
        }
        cmd.arg("--message").arg(build_message_payload(req));
        cmd
    }
}

fn build_message_payload(req: &AgentRequest) -> String {
    let mut out = String::new();
    if !req.system_prompt.is_empty() {
        out.push_str(&req.system_prompt);
        out.push_str("\n\n");
    }
    out.push_str(&req.user_prompt);
    out
}

struct ForwarderResult {
    tokens: TokenUsage,
    stderr_tail: Vec<String>,
}

async fn handle_stdout_line(
    line: &str,
    outbound: &mpsc::Sender<AgentEvent>,
    tokens: &mut TokenUsage,
) {
    // Forward the raw line verbatim regardless — aider's plain-text output is
    // the user-facing narrative and the dashboard wants it all. The structured
    // events below are emitted *in addition* to the Stdout event so the runner
    // sees both.
    let _ = outbound.send(AgentEvent::Stdout(line.to_string())).await;

    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("Applied edit to ") {
        if !rest.trim().is_empty() {
            let _ = outbound.send(AgentEvent::ToolUse("edit".to_string())).await;
        }
        return;
    }
    if let Some(rest) = trimmed.strip_prefix("Commit ") {
        if !rest.trim().is_empty() {
            let _ = outbound
                .send(AgentEvent::ToolUse("commit".to_string()))
                .await;
        }
        return;
    }
    if let Some(rest) = trimmed.strip_prefix("Tokens:") {
        if let Some((sent, received)) = parse_token_report(rest) {
            // Aider prints a cumulative-per-turn total. The runner's outer
            // aggregator sums `TokenDelta`s, so we overwrite the running
            // counter here and emit a single delta at end-of-stream.
            tokens.input = sent;
            tokens.output = received;
        }
    }
}

/// Parse the body of an `Tokens: <sent> sent, <received> received` line.
///
/// Returns `(sent, received)` on success, or `None` if either count is missing
/// or unrecognized. Accepts plain integers (`800`), `k` suffix (`1.2k`), and
/// `M` suffix (`1.5M`); aider prints decimals with one fractional digit so we
/// support that. Whitespace and a trailing `.` are tolerated.
fn parse_token_report(rest: &str) -> Option<(u64, u64)> {
    let cleaned: String = rest
        .chars()
        .filter(|c| !matches!(c, '\u{1b}'))
        .collect::<String>();
    let normalized = cleaned.trim().trim_end_matches('.');
    // Pattern shape: "<sent> sent, <received> received"
    let (sent_part, rest_part) = normalized.split_once(" sent")?;
    let sent = parse_token_count(sent_part.trim())?;
    let received_part = rest_part
        .trim_start_matches(',')
        .trim_start()
        .split_once(" received")
        .map(|(n, _)| n)?;
    let received = parse_token_count(received_part.trim())?;
    Some((sent, received))
}

fn parse_token_count(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (digits, multiplier) = match raw.as_bytes().last().copied() {
        Some(b'k') | Some(b'K') => (&raw[..raw.len() - 1], 1_000.0_f64),
        Some(b'm') | Some(b'M') => (&raw[..raw.len() - 1], 1_000_000.0_f64),
        _ => (raw, 1.0_f64),
    };
    let digits = digits.replace(',', "");
    let value: f64 = digits.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some((value * multiplier).round() as u64)
}

fn push_tail(buf: &mut Vec<String>, line: String, max: usize) {
    if buf.len() == max {
        buf.remove(0);
    }
    buf.push(line);
}

fn format_error_message(exit_code: i32, stderr_tail: &[String]) -> String {
    let mut out = format!("aider exited with code {}", exit_code);
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
            .join("aider")
            .join(name)
    }

    fn req_with_log(log_path: PathBuf, timeout: Duration) -> AgentRequest {
        AgentRequest {
            role: Role::Implementer,
            model: "anthropic/sonnet-4.5".into(),
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
    async fn parses_edits_commit_and_token_report() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = AiderAgent::with_binary(fixture_path("fake-aider-success.sh"));
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

        // Plain-text narrative is forwarded verbatim. The exact framing lines
        // (banner, model line) are loose, so we just check the assistant
        // sentence reaches the dashboard.
        assert!(
            stdouts.iter().any(|s| s.contains("Hello from Aider")),
            "missing assistant text: {stdouts:?}"
        );
        // Two `Applied edit to` lines → two "edit" ToolUse events; one
        // `Commit ...` line → one "commit" ToolUse event, in document order.
        assert_eq!(tool_uses, vec!["edit", "edit", "commit"]);
        assert_eq!(token_deltas.len(), 1);
        let total = token_deltas[0];
        // From fixture: "Tokens: 1.2k sent, 800 received" → 1200 / 800.
        assert_eq!(total.input, 1200);
        assert_eq!(total.output, 800);

        // by_role re-keyed onto Role::Implementer at the agent level.
        assert_eq!(outcome.tokens.input, 1200);
        assert_eq!(outcome.tokens.output, 800);
        let role_usage = outcome
            .tokens
            .by_role
            .get("implementer")
            .expect("implementer role usage");
        assert_eq!(role_usage.input, 1200);
        assert_eq!(role_usage.output, 800);

        // Log file should contain the raw plain-text output for post-mortem.
        let log_text = std::fs::read_to_string(&log).unwrap();
        assert!(log_text.contains("Applied edit to src/foo.rs"), "{log_text}");
        assert!(log_text.contains("Commit a1b2c3d"), "{log_text}");
    }

    #[tokio::test]
    async fn noop_run_emits_no_tool_use_but_token_delta_still_fires() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = AiderAgent::with_binary(fixture_path("fake-aider-noop.sh"));
        let (tx, rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let outcome = agent
            .run(req_with_log(log, Duration::from_secs(5)), tx, cancel)
            .await
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Completed);

        let evs = drain(rx).await;
        let tool_uses: Vec<&str> = evs
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolUse(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            tool_uses.is_empty(),
            "no-op run should produce no tool-use events, got {tool_uses:?}"
        );
        // Still expect a single TokenDelta with the small-but-nonzero counts.
        let token_deltas: Vec<&TokenUsage> = evs
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TokenDelta(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(token_deltas.len(), 1);
        assert_eq!(token_deltas[0].input, 320);
        assert_eq!(token_deltas[0].output, 45);
    }

    #[tokio::test]
    async fn nonzero_exit_maps_to_error_with_stderr_tail() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = AiderAgent::with_binary(fixture_path("fake-aider-crash.sh"));
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let outcome = agent
            .run(req_with_log(log, Duration::from_secs(5)), tx, cancel)
            .await
            .unwrap();
        match outcome.stop_reason {
            StopReason::Error(msg) => {
                assert!(msg.contains("exit"), "{msg}");
                assert!(
                    msg.contains("ANTHROPIC_API_KEY"),
                    "expected stderr tail in error message, got: {msg}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(outcome.exit_code, 1);
    }

    #[tokio::test]
    async fn cancellation_propagates_to_child_process() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = AiderAgent::with_binary(fixture_path("fake-aider-hang.sh"));
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
        let agent = AiderAgent::with_binary("/usr/local/bin/aider")
            .with_extra_args(vec!["--no-auto-commits".into(), "--map-tokens".into(), "0".into()])
            .with_model_override("anthropic/opus-4.5");
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
        // Unattended defaults are present.
        assert!(args.iter().any(|a| a == "--no-pretty"));
        assert!(args.iter().any(|a| a == "--no-stream"));
        assert!(args.iter().any(|a| a == "--yes-always"));
        assert!(args.iter().any(|a| a == "--no-check-update"));
        assert!(args.iter().any(|a| a == "--no-show-model-warnings"));
        assert!(args.iter().any(|a| a == "--analytics-disable"));
        // Model override beats AgentRequest::model.
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--model" && w[1] == "anthropic/opus-4.5"));
        assert!(!args.iter().any(|a| a == "ignored-because-override"));
        // Extra args spliced in before --message, in declared order.
        assert!(args.iter().any(|a| a == "--no-auto-commits"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--map-tokens" && w[1] == "0"));
        // The prompt body is concatenated system + blank line + user, passed
        // via the trailing `--message` flag.
        let msg_idx = args
            .iter()
            .position(|a| a == "--message")
            .expect("--message flag must be present");
        let body = &args[msg_idx + 1];
        assert!(body.starts_with("system body\n\n"));
        assert!(body.ends_with("user body"));
        assert_eq!(std_cmd.get_program(), "/usr/local/bin/aider");
        assert_eq!(std_cmd.get_current_dir(), Some(dir.path()));
    }

    #[tokio::test]
    async fn build_command_uses_request_model_when_no_override() {
        let agent = AiderAgent::with_binary("aider");
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let req = AgentRequest {
            role: Role::Implementer,
            model: "anthropic/sonnet-4.5".into(),
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
            .any(|w| w[0] == "--model" && w[1] == "anthropic/sonnet-4.5"));
    }

    #[test]
    fn build_message_payload_concatenates_system_and_user_with_blank_line() {
        let req = AgentRequest {
            role: Role::Implementer,
            model: "x".into(),
            system_prompt: "you are a careful engineer".into(),
            user_prompt: "implement phase 03".into(),
            workdir: std::env::temp_dir(),
            log_path: std::env::temp_dir().join("never.log"),
            timeout: Duration::from_secs(1),
        };
        let payload = build_message_payload(&req);
        assert!(payload.starts_with("you are a careful engineer\n\n"));
        assert!(payload.contains("implement phase 03"));
    }

    #[test]
    fn build_message_payload_omits_system_when_empty() {
        let req = AgentRequest {
            role: Role::Implementer,
            model: "x".into(),
            system_prompt: String::new(),
            user_prompt: "just the user body".into(),
            workdir: std::env::temp_dir(),
            log_path: std::env::temp_dir().join("never.log"),
            timeout: Duration::from_secs(1),
        };
        let payload = build_message_payload(&req);
        assert_eq!(payload, "just the user body");
    }

    #[test]
    fn parses_token_count_with_k_and_m_suffixes_and_decimals() {
        assert_eq!(parse_token_count("800"), Some(800));
        assert_eq!(parse_token_count("1.2k"), Some(1200));
        assert_eq!(parse_token_count("3K"), Some(3000));
        assert_eq!(parse_token_count("1.5M"), Some(1_500_000));
        assert_eq!(parse_token_count("2,400"), Some(2400));
        assert_eq!(parse_token_count(""), None);
        assert_eq!(parse_token_count("abc"), None);
    }

    #[test]
    fn parses_token_report_full_line() {
        // Body of the `Tokens:` line (parser strips the prefix before calling).
        let (s, r) = parse_token_report(" 1.2k sent, 800 received.").unwrap();
        assert_eq!(s, 1200);
        assert_eq!(r, 800);
        let (s, r) = parse_token_report(" 320 sent, 45 received.").unwrap();
        assert_eq!(s, 320);
        assert_eq!(r, 45);
        // Missing parts → None.
        assert!(parse_token_report(" garbage").is_none());
        assert!(parse_token_report(" 100 sent").is_none());
    }

    /// Real end-to-end test against the actual `aider` binary on PATH.
    /// Skipped unless `PITBOSS_REAL_AGENT_TESTS=1` so CI doesn't burn tokens.
    #[tokio::test]
    async fn real_aider_smoke_test() {
        if std::env::var("PITBOSS_REAL_AGENT_TESTS").ok().as_deref() != Some("1") {
            eprintln!("skipping real_aider_smoke_test (set PITBOSS_REAL_AGENT_TESTS=1 to run)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let agent = AiderAgent::new().with_extra_args(vec!["--no-auto-commits".into(), "--no-git".into()]);
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let req = AgentRequest {
            role: Role::Implementer,
            model: "anthropic/sonnet-4.5".into(),
            system_prompt: String::new(),
            user_prompt: "respond with the single word OK".into(),
            workdir: dir.path().to_path_buf(),
            log_path: log,
            timeout: Duration::from_secs(120),
        };
        let outcome = agent.run(req, tx, cancel).await.unwrap();
        assert!(
            matches!(outcome.stop_reason, StopReason::Completed),
            "real aider run did not complete: {:?}",
            outcome.stop_reason
        );
        assert_eq!(outcome.exit_code, 0);
    }
}
