//! Subprocess helpers shared by every agent that shells out.
//!
//! [`run_logged`] spawns a configured [`tokio::process::Command`], tees each
//! line of stdout/stderr to both the per-attempt log file and the caller's
//! [`mpsc::Sender<AgentEvent>`], and waits for whichever terminates first:
//! the process, the cancel token, or the timeout.
//!
//! Phase 7 keeps the helper deliberately low-level — raw line events with no
//! protocol parsing — so phase 8's `ClaudeCodeAgent` can build the streaming
//! JSON parser on top without further refactoring.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use super::{AgentEvent, StopReason};

/// Result of a [`run_logged`] invocation. Maps directly onto the
/// agent-level vocabulary so trait impls can pass `stop_reason` through.
#[derive(Debug, Clone)]
pub struct SubprocessOutcome {
    /// Process exit code; `-1` when the process did not exit naturally
    /// (cancelled, timed out, or signal death).
    pub exit_code: i32,
    /// Why the process stopped.
    pub stop_reason: StopReason,
}

/// Spawn `cmd`, wire its stdout/stderr to both `log_path` and `events`, and
/// wait for it to exit, be cancelled, or time out.
///
/// `cmd` is reconfigured to use piped stdio and `kill_on_drop`; any prior
/// stdio settings on it are overwritten. Other options (env, working
/// directory, args) are preserved.
///
/// Errors returned via the `Err` channel are *setup* failures (couldn't
/// create the log directory, couldn't spawn). A successfully spawned process
/// always returns `Ok(_)`, with `stop_reason` distinguishing natural exit
/// from cancel/timeout.
pub async fn run_logged(
    cmd: Command,
    log_path: impl AsRef<Path>,
    events: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    timeout: Duration,
) -> Result<SubprocessOutcome> {
    run_logged_with_stdin(cmd, log_path, events, cancel, timeout, None).await
}

/// Variant of [`run_logged`] that writes `stdin_payload` to the child's stdin
/// before draining its output. Used by backends (e.g. `codex`) whose contract
/// expects the prompt body on stdin rather than as positional argv.
///
/// `stdin_payload = None` is exactly equivalent to [`run_logged`] (`Stdio::null`
/// stdin). When `Some(bytes)`, stdin is piped and the payload is written and
/// closed before the helper waits on the child, so the process can read it
/// fully and exit naturally.
pub async fn run_logged_with_stdin(
    mut cmd: Command,
    log_path: impl AsRef<Path>,
    events: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    timeout: Duration,
    stdin_payload: Option<Vec<u8>>,
) -> Result<SubprocessOutcome> {
    let log_path = log_path.as_ref();
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("subprocess: create log dir {:?}", parent))?;
        }
    }
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .await
        .with_context(|| format!("subprocess: open log {:?}", log_path))?;
    let log = Arc::new(Mutex::new(log_file));

    let stdin_setting = if stdin_payload.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    cmd.stdin(stdin_setting)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("subprocess: spawning child process")?;
    if let Some(bytes) = stdin_payload {
        if let Some(mut stdin) = child.stdin.take() {
            // Best-effort write + close. If the child exits before draining
            // stdin (EPIPE) we must still produce a normal SubprocessOutcome
            // so callers see the real exit code and captured stderr — those
            // are the signal that explains *why* the child rejected input.
            // Surfacing the write error here would shadow that diagnosis.
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
        }
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let stdout_task = tokio::spawn(forward_stream(
        stdout,
        StreamKind::Stdout,
        log.clone(),
        events.clone(),
    ));
    let stderr_task = tokio::spawn(forward_stream(
        stderr,
        StreamKind::Stderr,
        log.clone(),
        events.clone(),
    ));

    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            SubprocessOutcome { exit_code: -1, stop_reason: StopReason::Cancelled }
        }
        _ = tokio::time::sleep(timeout) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            SubprocessOutcome { exit_code: -1, stop_reason: StopReason::Timeout }
        }
        status = child.wait() => {
            let status = status.context("subprocess: waiting for child")?;
            let code = status.code().unwrap_or(-1);
            SubprocessOutcome { exit_code: code, stop_reason: StopReason::Completed }
        }
    };

    // Drain both readers (they exit on EOF once the child is gone) and flush
    // the log so `log_path` reflects everything the process emitted.
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    let mut f = log.lock().await;
    let _ = f.flush().await;
    drop(f);
    drop(events); // ensures receivers see channel close once the helper returns.

    Ok(outcome)
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

async fn forward_stream<R>(
    reader: R,
    kind: StreamKind,
    log: Arc<Mutex<File>>,
    events: mpsc::Sender<AgentEvent>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        {
            let mut f = log.lock().await;
            let prefix: &[u8] = match kind {
                StreamKind::Stdout => b"",
                StreamKind::Stderr => b"[stderr] ",
            };
            let _ = f.write_all(prefix).await;
            let _ = f.write_all(line.as_bytes()).await;
            let _ = f.write_all(b"\n").await;
        }
        let event = match kind {
            StreamKind::Stdout => AgentEvent::Stdout(line),
            StreamKind::Stderr => AgentEvent::Stderr(line),
        };
        // A closed receiver isn't fatal; we keep draining so the child's
        // pipe doesn't fill and stall the process, and so the log captures
        // every byte for post-mortem.
        let _ = events.send(event).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn drain<T>(mut rx: mpsc::Receiver<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let Some(v) = rx.recv().await {
            out.push(v);
        }
        out
    }

    #[tokio::test]
    async fn captures_stdout_lines_to_events_and_log() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("nested").join("run.log");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo hello; echo world");
        let (tx, rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let outcome = run_logged(cmd, &log, tx, cancel, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.exit_code, 0);
        let stdout: Vec<_> = drain(rx)
            .await
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::Stdout(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(stdout, vec!["hello".to_string(), "world".to_string()]);
        let log_text = std::fs::read_to_string(&log).unwrap();
        assert!(log_text.contains("hello\n"));
        assert!(log_text.contains("world\n"));
    }

    #[tokio::test]
    async fn captures_stderr_lines_with_log_marker() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("run.log");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo oops 1>&2");
        let (tx, rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let outcome = run_logged(cmd, &log, tx, cancel, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        let stderr: Vec<_> = drain(rx)
            .await
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::Stderr(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(stderr, vec!["oops".to_string()]);
        let log_text = std::fs::read_to_string(&log).unwrap();
        assert!(
            log_text.contains("[stderr] oops\n"),
            "log_text: {log_text:?}"
        );
    }

    #[tokio::test]
    async fn surfaces_nonzero_exit_code_under_completed() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("run.log");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("exit 7");
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let outcome = run_logged(cmd, &log, tx, cancel, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.exit_code, 7);
    }

    #[tokio::test]
    async fn cancellation_terminates_long_running_child() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("run.log");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 30");
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let canceler = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            canceler.cancel();
        });
        let outcome = run_logged(cmd, &log, tx, cancel, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Cancelled);
        assert_eq!(outcome.exit_code, -1);
    }

    #[tokio::test]
    async fn timeout_terminates_long_running_child() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("run.log");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 30");
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let outcome = run_logged(cmd, &log, tx, cancel, Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Timeout);
        assert_eq!(outcome.exit_code, -1);
    }

    #[tokio::test]
    async fn stdin_payload_is_delivered_to_child() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("run.log");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("cat");
        let (tx, rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let outcome = run_logged_with_stdin(
            cmd,
            &log,
            tx,
            cancel,
            Duration::from_secs(5),
            Some(b"first line\nsecond line\n".to_vec()),
        )
        .await
        .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.exit_code, 0);
        let stdout: Vec<_> = drain(rx)
            .await
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::Stdout(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(
            stdout,
            vec!["first line".to_string(), "second line".to_string()]
        );
    }

    #[tokio::test]
    async fn child_exiting_before_draining_stdin_is_non_fatal() {
        // Child exits immediately without reading stdin. With a payload
        // larger than the pipe buffer, the parent's write_all will hit
        // EPIPE — the helper must still report the natural exit code
        // rather than masking it with a setup-style error.
        let dir = tempdir().unwrap();
        let log = dir.path().join("run.log");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("exit 3");
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let payload = vec![b'x'; 1024 * 1024];
        let outcome =
            run_logged_with_stdin(cmd, &log, tx, cancel, Duration::from_secs(5), Some(payload))
                .await
                .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.exit_code, 3);
    }

    #[tokio::test]
    async fn spawn_failure_returns_setup_error() {
        let log = std::env::temp_dir().join("pitboss-spawn-fail.log");
        let cmd = Command::new("/this/binary/does/not/exist");
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let err = run_logged(cmd, &log, tx, cancel, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(
            format!("{:#}", err).contains("spawning child"),
            "err: {err:#}"
        );
    }
}
