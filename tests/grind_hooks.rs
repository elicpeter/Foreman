//! Phase 10 grind acceptance: plan-level shell hooks fire at the documented
//! points around each session, capture their output into the transcript with
//! a labeled banner, and apply the right env vars at each kind.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use pitboss::agent::{Agent, AgentEvent, AgentOutcome, AgentRequest, StopReason};
use pitboss::config::Config;
use pitboss::git::{Git, ShellGit};
use pitboss::grind::{
    default_plan_from_dir, GrindRunner, GrindShutdown, Hooks, PlanBudgets, PromptDoc, PromptMeta,
    PromptSource, RunDir, SessionStatus,
};

const RUN_ID: &str = "20260430T180000Z-hooks";

struct MockAgent {
    invocations: Arc<AtomicU32>,
}

impl MockAgent {
    fn new(invocations: Arc<AtomicU32>) -> Self {
        Self { invocations }
    }
}

#[async_trait]
impl Agent for MockAgent {
    fn name(&self) -> &str {
        "grind-hooks-mock"
    }

    async fn run(
        &self,
        req: AgentRequest,
        _events: mpsc::Sender<AgentEvent>,
        _cancel: CancellationToken,
    ) -> Result<AgentOutcome> {
        self.invocations.fetch_add(1, Ordering::SeqCst);

        // Materialize the transcript so the hook banner inserted before
        // dispatch can sit alongside the agent's own output.
        if let Some(parent) = req.log_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let mut existing = fs::read_to_string(&req.log_path).unwrap_or_default();
        existing.push_str("[mock] agent ran\n");
        fs::write(&req.log_path, existing).ok();

        // Drop a marker file so the session has something to commit.
        let prompt_name = req.env.get("PITBOSS_PROMPT_NAME").cloned().unwrap_or_default();
        let seq = req.env.get("PITBOSS_SESSION_SEQ").cloned().unwrap_or_default();
        let marker = req
            .workdir
            .join(format!("src/hooks_mock_{}_{}.rs", seq, prompt_name.replace('-', "_")));
        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&marker, format!("// session {seq}\n")).ok();

        // Write the agent summary the way a production agent would.
        let summary_file = PathBuf::from(req.env.get("PITBOSS_SUMMARY_FILE").unwrap());
        if let Some(parent) = summary_file.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&summary_file, format!("ran session {seq}")).ok();

        Ok(AgentOutcome {
            exit_code: 0,
            stop_reason: StopReason::Completed,
            tokens: pitboss::state::TokenUsage::default(),
            log_path: req.log_path,
        })
    }
}

fn fake_prompt(name: &str) -> PromptDoc {
    PromptDoc {
        meta: PromptMeta {
            name: name.into(),
            description: format!("desc {name}"),
            weight: 1,
            every: 1,
            max_runs: None,
            verify: false,
            parallel_safe: false,
            tags: vec![],
            max_session_seconds: None,
            max_session_cost_usd: None,
        },
        body: format!("body for {name}\n"),
        source_path: PathBuf::from(format!("/fixture/{name}.md")),
        source_kind: PromptSource::Project,
    }
}

fn lookup(prompts: &[PromptDoc]) -> BTreeMap<String, PromptDoc> {
    prompts
        .iter()
        .map(|p| (p.meta.name.clone(), p.clone()))
        .collect()
}

fn init_git_repo(dir: &Path) {
    let status = Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(dir)
        .status()
        .expect("git init");
    assert!(status.success());
    for (k, v) in [("user.name", "pitboss-test"), ("user.email", "pitboss@test")] {
        Command::new("git")
            .args(["-C"])
            .arg(dir)
            .args(["config", k, v])
            .status()
            .unwrap();
    }
    let status = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["commit", "--allow-empty", "-m", "seed", "-q"])
        .status()
        .expect("seed commit");
    assert!(status.success());
}

struct Built {
    runner: GrindRunner<MockAgent, ShellGit>,
    log: pitboss::grind::SessionLog,
    transcript_dir: PathBuf,
    invocations: Arc<AtomicU32>,
}

async fn build(workspace: &Path, hooks: Hooks, hook_timeout_secs: u64) -> Built {
    init_git_repo(workspace);
    let branch = pitboss::grind::run_branch_name(RUN_ID);
    let git = ShellGit::new(workspace);
    git.create_branch(&branch).await.unwrap();
    git.checkout(&branch).await.unwrap();

    let prompts = vec![fake_prompt("alpha")];
    let mut plan = default_plan_from_dir(&prompts);
    plan.hooks = hooks;

    let run_dir = RunDir::create(workspace, RUN_ID).expect("create run dir");
    let log = run_dir.log().clone();
    let transcript_dir = run_dir.paths().transcripts.clone();

    let mut config = Config::default();
    config.grind.hook_timeout_secs = hook_timeout_secs;

    let runner_git = ShellGit::new(workspace);
    let invocations = Arc::new(AtomicU32::new(0));
    let runner = GrindRunner::new(
        workspace.to_path_buf(),
        config,
        RUN_ID.to_string(),
        branch.clone(),
        plan,
        lookup(&prompts),
        run_dir,
        MockAgent::new(invocations.clone()),
        runner_git,
        PlanBudgets::default(),
        3,
    );
    Built {
        runner,
        log,
        transcript_dir,
        invocations,
    }
}

/// Drain after a fixed number of mock-agent invocations so a single-prompt
/// plan stops cleanly.
async fn run_once(
    runner: &mut GrindRunner<MockAgent, ShellGit>,
    invocations: Arc<AtomicU32>,
    target: u32,
) -> pitboss::grind::GrindRunOutcome {
    let shutdown = GrindShutdown::new();
    let watch_invocations = invocations.clone();
    let watch_shutdown = shutdown.clone();
    let watcher = tokio::spawn(async move {
        loop {
            if watch_invocations.load(Ordering::SeqCst) >= target {
                watch_shutdown.drain();
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let outcome = runner.run(shutdown).await.unwrap();
    let _ = watcher.await;
    outcome
}

#[tokio::test]
async fn pre_and_post_session_hooks_each_fire_once_per_session() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    let pre_marker = dir.path().join("pre.log");
    let post_marker = dir.path().join("post.log");

    let pre_cmd = format!("printf 'pre %s\\n' \"$PITBOSS_SESSION_PROMPT\" >> {}", pre_marker.display());
    let post_cmd = format!(
        "printf 'post %s %s\\n' \"$PITBOSS_SESSION_STATUS\" \"$PITBOSS_SESSION_SUMMARY\" >> {}",
        post_marker.display()
    );
    let hooks = Hooks {
        pre_session: Some(pre_cmd),
        post_session: Some(post_cmd),
        on_failure: None,
    };

    let Built {
        mut runner,
        log,
        transcript_dir,
        invocations,
    } = build(workspace, hooks, 60).await;

    let outcome = run_once(&mut runner, invocations.clone(), 1).await;
    assert!(
        !outcome.sessions.is_empty(),
        "expected at least one session, got {:?}",
        outcome.sessions
    );

    // Each marker file has exactly one line per session.
    let pre_body = fs::read_to_string(&pre_marker).unwrap();
    let post_body = fs::read_to_string(&post_marker).unwrap();
    let session_count = outcome.sessions.len();
    assert_eq!(
        pre_body.lines().count(),
        session_count,
        "pre_session marker lines: {pre_body:?}"
    );
    assert_eq!(
        post_body.lines().count(),
        session_count,
        "post_session marker lines: {post_body:?}"
    );
    assert!(pre_body.lines().all(|l| l == "pre alpha"));
    assert!(post_body.lines().all(|l| l.starts_with("post ok ran session")));

    // Captured banners and prefixed output land in the per-session transcript.
    let transcript = fs::read_to_string(transcript_dir.join("session-0001.log")).unwrap();
    assert!(
        transcript.contains("=== pitboss hook: pre_session"),
        "missing pre_session open banner: {transcript}"
    );
    assert!(
        transcript.contains("=== pitboss hook: pre_session ok"),
        "missing pre_session close banner: {transcript}"
    );
    assert!(
        transcript.contains("=== pitboss hook: post_session"),
        "missing post_session open banner: {transcript}"
    );
    assert!(
        transcript.contains("=== pitboss hook: post_session ok"),
        "missing post_session close banner: {transcript}"
    );

    // Session is `Ok` and was committed.
    let records = log.records().unwrap();
    assert_eq!(records.len(), session_count);
    assert_eq!(records[0].status, SessionStatus::Ok);
    assert!(records[0].commit.is_some());
}

#[tokio::test]
async fn pre_session_failure_skips_dispatch_and_records_error() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    let on_failure_marker = dir.path().join("on_failure.log");

    let hooks = Hooks {
        pre_session: Some("echo refusing-to-dispatch; exit 1".into()),
        post_session: None,
        on_failure: Some(format!(
            "printf 'failed status=%s\\n' \"$PITBOSS_SESSION_STATUS\" >> {}",
            on_failure_marker.display()
        )),
    };

    let Built {
        mut runner,
        log,
        transcript_dir,
        invocations,
        ..
    } = build(workspace, hooks, 60).await;

    // The single-prompt plan would loop forever in the absence of dispatch
    // (the scheduler keeps offering the prompt, but `record_run` is called).
    // Drain after one session record lands.
    let shutdown = GrindShutdown::new();
    let log_for_watch = log.clone();
    let watch_shutdown = shutdown.clone();
    let watcher = tokio::spawn(async move {
        loop {
            if let Ok(records) = log_for_watch.records() {
                if !records.is_empty() {
                    watch_shutdown.drain();
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let _ = runner.run(shutdown).await.unwrap();
    let _ = watcher.await;

    // Agent never dispatched.
    assert_eq!(invocations.load(Ordering::SeqCst), 0);

    // The session record carries `Error` and a summary referencing the hook.
    let records = log.records().unwrap();
    assert!(!records.is_empty(), "expected at least one error record");
    let r = &records[0];
    assert_eq!(r.status, SessionStatus::Error);
    let summary = r.summary.as_deref().unwrap_or("");
    assert!(
        summary.contains("pre_session hook"),
        "summary should explain the skip: {summary:?}"
    );

    // The transcript captured the failing hook's stdout banner.
    let transcript = fs::read_to_string(transcript_dir.join("session-0001.log")).unwrap();
    assert!(transcript.contains("[hook:pre_session] refusing-to-dispatch"));
    assert!(transcript.contains("non-zero exit 1"));

    // on_failure ran exactly once because status is non-Ok.
    let on_failure_body = fs::read_to_string(&on_failure_marker).unwrap();
    assert_eq!(
        on_failure_body.lines().count(),
        records.len(),
        "on_failure should fire once per non-Ok session: {on_failure_body:?}"
    );
    assert!(on_failure_body.contains("failed status=error"));
}

#[tokio::test]
async fn on_failure_does_not_run_for_ok_session() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    let on_failure_marker = dir.path().join("on_failure.log");

    let hooks = Hooks {
        pre_session: None,
        post_session: None,
        on_failure: Some(format!("echo should-not-run >> {}", on_failure_marker.display())),
    };

    let Built {
        mut runner,
        log,
        invocations,
        ..
    } = build(workspace, hooks, 60).await;
    let outcome = run_once(&mut runner, invocations.clone(), 1).await;
    assert!(!outcome.sessions.is_empty());
    let records = log.records().unwrap();
    assert!(records.iter().all(|r| r.status == SessionStatus::Ok));

    // Marker file should not exist (or be empty) — on_failure was not fired.
    let body = fs::read_to_string(&on_failure_marker).unwrap_or_default();
    assert!(
        body.is_empty(),
        "on_failure should not have run on Ok session: {body:?}"
    );
}

#[tokio::test]
async fn hook_timeout_is_killed_and_logged_in_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();

    let hooks = Hooks {
        pre_session: Some("sleep 5".into()),
        post_session: None,
        on_failure: None,
    };

    let Built {
        mut runner,
        log,
        transcript_dir,
        invocations,
        ..
    } = build(workspace, hooks, 1).await;

    // The pre_session timeout should record an Error record without ever
    // dispatching the agent. Drain after the first record lands.
    let shutdown = GrindShutdown::new();
    let log_for_watch = log.clone();
    let watch_shutdown = shutdown.clone();
    let watcher = tokio::spawn(async move {
        loop {
            if let Ok(records) = log_for_watch.records() {
                if !records.is_empty() {
                    watch_shutdown.drain();
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    let start = std::time::Instant::now();
    let _ = runner.run(shutdown).await.unwrap();
    let _ = watcher.await;
    let elapsed = start.elapsed();

    // Hook timeout was 1s; if the kill failed and we waited the full sleep,
    // elapsed would be at least 5s. Allow generous overhead for CI but assert
    // we did not block on the full sleep.
    assert!(
        elapsed < Duration::from_secs(4),
        "hook timeout should kill the child quickly, got elapsed={elapsed:?}"
    );

    assert_eq!(invocations.load(Ordering::SeqCst), 0);

    let transcript = fs::read_to_string(transcript_dir.join("session-0001.log")).unwrap();
    assert!(
        transcript.contains("timed out after 1s"),
        "transcript should contain timeout banner: {transcript}"
    );

    let records = log.records().unwrap();
    assert_eq!(records[0].status, SessionStatus::Error);
}
