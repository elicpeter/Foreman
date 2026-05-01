//! Plan-level shell hooks fired around each grind session.
//!
//! Three kinds — [`HookKind::PreSession`], [`HookKind::PostSession`], and
//! [`HookKind::OnFailure`] — load from the plan's `[hooks]` table (or
//! inherit from `[grind.hooks]` in `pitboss.toml`) and run as
//! `sh -c "<cmd>"` children of the pitboss process.
//!
//! Each hook receives the same env vars the agent sees plus
//! `PITBOSS_SESSION_PROMPT`. `post_session` and `on_failure` additionally
//! receive `PITBOSS_SESSION_STATUS` (the resolved [`super::SessionStatus`]
//! lower-cased) and `PITBOSS_SESSION_SUMMARY` (the captured summary text or
//! the `(no summary provided)` fallback). All stdout/stderr is forwarded into
//! the per-session transcript with a labeled banner so a post-mortem can
//! reconstruct exactly what the hook saw.
//!
//! Hooks run with a configurable wall-clock cap (`[grind.hook_timeout_secs]`,
//! default 60s). A timeout is recorded as [`HookOutcome::Timeout`]; the child
//! is killed via `kill_on_drop` when the helper drops the [`Child`].

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::warn;

/// Which hook in the session lifecycle is firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    /// Runs before the agent dispatch. Non-zero exit skips dispatch and
    /// records the session as `Error`.
    PreSession,
    /// Runs after the session resolves, regardless of status.
    PostSession,
    /// Runs after the session resolves, only when the status is non-`Ok`.
    OnFailure,
}

impl HookKind {
    /// Stable lower_snake_case label used in transcript banners and tracing
    /// fields. Matches the TOML key the user wrote in `[hooks]`.
    pub const fn label(self) -> &'static str {
        match self {
            HookKind::PreSession => "pre_session",
            HookKind::PostSession => "post_session",
            HookKind::OnFailure => "on_failure",
        }
    }
}

/// Resolved outcome of a single [`run_hook`] invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Process exited with code `0`.
    Success,
    /// Process exited with a non-zero code.
    Failed {
        /// Exit code returned by the child, or `-1` if the child died from a
        /// signal without producing a code.
        exit_code: i32,
    },
    /// Process exceeded the configured wall-clock cap and was killed.
    Timeout {
        /// Cap (in seconds) that fired.
        secs: u64,
    },
    /// `sh -c "<cmd>"` could not be spawned, or the child's status could not
    /// be retrieved. Carries the underlying error message so callers can log
    /// it.
    SpawnError(String),
}

impl HookOutcome {
    /// `true` only for [`HookOutcome::Success`].
    pub fn is_success(&self) -> bool {
        matches!(self, HookOutcome::Success)
    }

    /// One-line description suitable for embedding in a transcript banner or
    /// a tracing field.
    pub fn description(&self) -> String {
        match self {
            HookOutcome::Success => "ok".to_string(),
            HookOutcome::Failed { exit_code } => format!("non-zero exit {exit_code}"),
            HookOutcome::Timeout { secs } => format!("timed out after {secs}s"),
            HookOutcome::SpawnError(msg) => format!("spawn failed: {msg}"),
        }
    }
}

/// Run `cmd` as a `sh -c "<cmd>"` child, forwarding its stdout and stderr
/// into `transcript_path` with labeled banner and per-line prefixes.
///
/// `env` is applied verbatim to the child's environment. Callers assemble the
/// full set (the shared agent env plus the hook-specific extras documented in
/// the module docs) before invoking; this helper does not synthesize keys.
///
/// Errors during transcript IO are *not* fatal: a hook that produced output
/// pitboss could not log still runs to completion and reports its real
/// outcome. Spawn failures are surfaced as [`HookOutcome::SpawnError`] so the
/// caller can decide whether to treat them as a skip-dispatch signal (they
/// behave the same as a non-zero exit on `pre_session`).
pub async fn run_hook(
    kind: HookKind,
    cmd: &str,
    env: &HashMap<String, String>,
    timeout: Duration,
    transcript_path: &Path,
) -> HookOutcome {
    let label = kind.label();
    let log = open_transcript(transcript_path).await;
    write_banner_open(&log, label, cmd).await;

    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Hooks should never inherit the parent's environment beyond what we
        // explicitly hand over, otherwise a stale `PITBOSS_*` from an outer
        // pitboss process could shadow this run's. `env_clear` plus an
        // explicit `PATH` keeps the surface predictable.
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
        );
    for (k, v) in env {
        command.env(k, v);
    }

    let mut child: Child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{e:#}");
            warn!(hook = %label, error = %msg, "grind: hook spawn failed");
            write_banner_close(&log, label, &HookOutcome::SpawnError(msg.clone())).await;
            return HookOutcome::SpawnError(msg);
        }
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_task = tokio::spawn(forward(stdout, label, false, log.clone()));
    let stderr_task = tokio::spawn(forward(stderr, label, true, log.clone()));

    let outcome = tokio::select! {
        _ = tokio::time::sleep(timeout) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            HookOutcome::Timeout { secs: timeout.as_secs() }
        }
        status = child.wait() => match status {
            Ok(s) if s.success() => HookOutcome::Success,
            Ok(s) => HookOutcome::Failed { exit_code: s.code().unwrap_or(-1) },
            Err(e) => HookOutcome::SpawnError(format!("wait failed: {e:#}")),
        }
    };

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    match &outcome {
        HookOutcome::Timeout { secs } => {
            warn!(
                hook = %label,
                timeout_secs = %secs,
                "grind: hook timed out"
            );
        }
        HookOutcome::Failed { exit_code } => {
            warn!(
                hook = %label,
                exit_code = %exit_code,
                "grind: hook exited non-zero"
            );
        }
        HookOutcome::SpawnError(msg) => {
            warn!(hook = %label, error = %msg, "grind: hook errored");
        }
        HookOutcome::Success => {}
    }

    write_banner_close(&log, label, &outcome).await;
    outcome
}

/// Shared writer for the per-session transcript. `Arc<Mutex<File>>` so the
/// stdout and stderr drainers cannot interleave bytes mid-line.
type SharedLog = Option<Arc<Mutex<tokio::fs::File>>>;

async fn open_transcript(path: &Path) -> SharedLog {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                warn!(
                    path = %path.display(),
                    error = %format!("{e:#}"),
                    "grind: failed to create hook transcript directory"
                );
                return None;
            }
        }
    }
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        Ok(f) => Some(Arc::new(Mutex::new(f))),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %format!("{e:#}"),
                "grind: failed to open hook transcript"
            );
            None
        }
    }
}

async fn write_banner_open(log: &SharedLog, label: &str, cmd: &str) {
    let Some(log) = log.as_ref() else { return };
    let ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let banner =
        format!("\n=== pitboss hook: {label} (cmd: {cmd}) [start {ts}] ===\n");
    let mut f = log.lock().await;
    let _ = f.write_all(banner.as_bytes()).await;
    let _ = f.flush().await;
}

async fn write_banner_close(log: &SharedLog, label: &str, outcome: &HookOutcome) {
    let Some(log) = log.as_ref() else { return };
    let ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let banner = format!(
        "=== pitboss hook: {label} {res} [end {ts}] ===\n",
        res = outcome.description()
    );
    let mut f = log.lock().await;
    let _ = f.write_all(banner.as_bytes()).await;
    let _ = f.flush().await;
}

async fn forward<R>(reader: R, label: &'static str, is_stderr: bool, log: SharedLog)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let prefix = if is_stderr {
            format!("[hook:{label}:stderr] ")
        } else {
            format!("[hook:{label}] ")
        };
        if let Some(log) = log.as_ref() {
            let mut f = log.lock().await;
            let _ = f.write_all(prefix.as_bytes()).await;
            let _ = f.write_all(line.as_bytes()).await;
            let _ = f.write_all(b"\n").await;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[tokio::test]
    async fn success_records_stdout_to_transcript_with_banner() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("transcripts").join("session-0001.log");
        let outcome = run_hook(
            HookKind::PreSession,
            "echo hello-from-hook",
            &env(&[("PITBOSS_SESSION_PROMPT", "alpha")]),
            Duration::from_secs(5),
            &log,
        )
        .await;
        assert_eq!(outcome, HookOutcome::Success);
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            body.contains("=== pitboss hook: pre_session"),
            "missing open banner: {body}"
        );
        assert!(
            body.contains("[hook:pre_session] hello-from-hook"),
            "missing forwarded stdout: {body}"
        );
        assert!(
            body.contains("=== pitboss hook: pre_session ok"),
            "missing close banner: {body}"
        );
    }

    #[tokio::test]
    async fn non_zero_exit_yields_failed_outcome() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("transcripts").join("session-0001.log");
        let outcome = run_hook(
            HookKind::PostSession,
            "echo about to fail; exit 9",
            &env(&[]),
            Duration::from_secs(5),
            &log,
        )
        .await;
        assert_eq!(outcome, HookOutcome::Failed { exit_code: 9 });
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(body.contains("about to fail"));
        assert!(body.contains("non-zero exit 9"));
    }

    #[tokio::test]
    async fn stderr_is_forwarded_with_dedicated_prefix() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("session.log");
        let outcome = run_hook(
            HookKind::OnFailure,
            "echo oh-no 1>&2",
            &env(&[]),
            Duration::from_secs(5),
            &log,
        )
        .await;
        assert!(outcome.is_success());
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            body.contains("[hook:on_failure:stderr] oh-no"),
            "expected stderr prefix in transcript: {body}"
        );
    }

    #[tokio::test]
    async fn timeout_kills_long_running_hook() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("session.log");
        let start = std::time::Instant::now();
        let outcome = run_hook(
            HookKind::PreSession,
            "sleep 5",
            &env(&[]),
            Duration::from_secs(1),
            &log,
        )
        .await;
        let elapsed = start.elapsed();
        assert_eq!(outcome, HookOutcome::Timeout { secs: 1 });
        assert!(
            elapsed < Duration::from_secs(4),
            "timeout should kill the child quickly, got elapsed={:?}",
            elapsed
        );
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            body.contains("timed out after 1s"),
            "missing timeout banner: {body}"
        );
    }

    #[tokio::test]
    async fn env_vars_reach_the_child_process() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("session.log");
        let outcome = run_hook(
            HookKind::PostSession,
            "printf 'prompt=%s status=%s summary=%s' \"$PITBOSS_SESSION_PROMPT\" \"$PITBOSS_SESSION_STATUS\" \"$PITBOSS_SESSION_SUMMARY\"",
            &env(&[
                ("PITBOSS_SESSION_PROMPT", "fp-hunter"),
                ("PITBOSS_SESSION_STATUS", "ok"),
                ("PITBOSS_SESSION_SUMMARY", "did the thing"),
            ]),
            Duration::from_secs(5),
            &log,
        )
        .await;
        assert!(outcome.is_success());
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            body.contains("prompt=fp-hunter status=ok summary=did the thing"),
            "env not reaching child: {body}"
        );
    }

    #[tokio::test]
    async fn hook_outcome_descriptions_are_human_readable() {
        assert_eq!(HookOutcome::Success.description(), "ok");
        assert_eq!(
            HookOutcome::Failed { exit_code: 2 }.description(),
            "non-zero exit 2"
        );
        assert_eq!(
            HookOutcome::Timeout { secs: 30 }.description(),
            "timed out after 30s"
        );
        assert_eq!(
            HookOutcome::SpawnError("boom".into()).description(),
            "spawn failed: boom"
        );
    }
}
