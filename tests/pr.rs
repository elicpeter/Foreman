//! Integration tests for phase 19 — `gh pr create` after a successful run.
//!
//! Drives a one-phase plan through the runner with a [`ScriptedAgent`] (kept
//! in-file rather than shared with `tests/runner.rs` so each test file stays
//! self-contained), then exercises [`cli::play::open_post_run_pr`] against a
//! [`ShellGit`] pointed at a fake `gh` fixture script. The fake records its
//! invocation into a sidecar file in the workspace so we can assert exactly
//! what title/body the binary received.

#![cfg(unix)]

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use pitboss::agent::{Agent, AgentEvent, AgentOutcome, AgentRequest, StopReason};
use pitboss::cli::play::open_post_run_pr;
use pitboss::config::Config;
use pitboss::deferred::DeferredDoc;
use pitboss::git::{self, Git, MockGit, MockOp, PrSummary, ShellGit};
use pitboss::plan::{self, PhaseId};
use pitboss::runner::{self, RunSummary, Runner};
use pitboss::state::TokenUsage;

fn pid(s: &str) -> PhaseId {
    PhaseId::parse(s).unwrap()
}

/// Pruned scripted agent (the runner test file has the full version; this
/// trim keeps the dependency graph local).
#[derive(Default, Clone)]
struct Script {
    writes: Vec<(PathBuf, Vec<u8>)>,
    stop_reason: Option<StopReason>,
    tokens: Option<TokenUsage>,
}

impl Script {
    fn write(mut self, rel: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) -> Self {
        self.writes.push((rel.into(), bytes.into()));
        self
    }
}

struct ScriptedAgent {
    name: String,
    scripts: Mutex<VecDeque<Script>>,
}

impl ScriptedAgent {
    fn new(scripts: Vec<Script>) -> Self {
        Self {
            name: "scripted".into(),
            scripts: Mutex::new(scripts.into()),
        }
    }
}

#[async_trait]
impl Agent for ScriptedAgent {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(
        &self,
        req: AgentRequest,
        events: mpsc::Sender<AgentEvent>,
        _cancel: CancellationToken,
    ) -> Result<AgentOutcome> {
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        for (rel, bytes) in &script.writes {
            let path = req.workdir.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(&path, bytes).expect("scripted agent write");
        }
        if let Some(parent) = req.log_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&req.log_path, b"scripted log\n").ok();
        let _ = events.send(AgentEvent::Stdout("scripted ran".into())).await;
        Ok(AgentOutcome {
            exit_code: 0,
            stop_reason: script.stop_reason.unwrap_or(StopReason::Completed),
            tokens: script.tokens.unwrap_or_default(),
            log_path: req.log_path,
        })
    }
}

const ONE_PHASE_PLAN: &str = "\
---
current_phase: \"01\"
---

# Pitboss Plan

# Phase 01: Foundation

**Scope.** Single phase fixture.
";

const EMPTY_DEFERRED: &str = "## Deferred items\n\n## Deferred phases\n";

fn make_workspace(plan_text: &str, deferred_text: &str) -> tempfile::TempDir {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("plan.md"), plan_text).unwrap();
    fs::write(dir.path().join("deferred.md"), deferred_text).unwrap();
    fs::create_dir_all(dir.path().join(".pitboss/snapshots")).unwrap();
    fs::create_dir_all(dir.path().join(".pitboss/logs")).unwrap();
    dir
}

fn init_git_repo(dir: &Path) {
    let status = Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(dir)
        .status()
        .expect("git init");
    assert!(status.success());
    for (k, v) in [
        ("user.name", "pitboss-test"),
        ("user.email", "pitboss@test"),
    ] {
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
        .expect("git seed");
    assert!(status.success());
}

fn audit_disabled() -> Config {
    let mut c = Config::default();
    c.audit.enabled = false;
    c
}

fn fixture_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push(name);
    p
}

#[tokio::test]
async fn open_post_run_pr_with_shell_git_invokes_fake_gh_with_generated_title_and_body() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"//! phase 1\n")
    ]);

    let plan = plan::parse(ONE_PHASE_PLAN).unwrap();
    let deferred = pitboss::deferred::parse(EMPTY_DEFERRED).unwrap();
    let config = audit_disabled();
    let state = runner::fresh_run_state(&plan, &config, Utc::now());

    // Branch setup mirrors what `cli::play::execute` does on a fresh run.
    let setup = ShellGit::new(dir.path());
    setup.create_branch(&state.branch).await.unwrap();
    setup.checkout(&state.branch).await.unwrap();

    // The runner's git handle is the one we'll poke with `open_pr` after the
    // run, so it gets the fake gh binary.
    let runner_git = ShellGit::new(dir.path()).with_gh_binary(fixture_path("fake-gh-success.sh"));
    let mut runner = Runner::new(
        dir.path().to_path_buf(),
        config,
        plan,
        deferred,
        state,
        agent,
        runner_git,
    );

    let summary = runner.run().await.unwrap();
    assert!(
        matches!(summary, RunSummary::Finished),
        "summary: {summary:?}"
    );
    // Sanity: the runner committed the phase, so completed has one entry.
    assert_eq!(runner.state().completed.len(), 1);

    let url = open_post_run_pr(&runner).await.unwrap();
    assert_eq!(url, "https://github.com/example/repo/pull/42");

    // The fake script logs its argv into `.gh-fake-log` in cwd. The cwd is
    // the workspace because `ShellGit::open_pr` sets `current_dir` itself.
    let log = fs::read_to_string(dir.path().join(".gh-fake-log")).unwrap();
    assert!(log.contains("--title"), "fake log: {log}");
    assert!(
        log.contains("pitboss: phase 01 — Foundation"),
        "fake log: {log}"
    );
    assert!(log.contains("--body"), "fake log: {log}");
    assert!(log.contains("## Run"), "fake log: {log}");
    assert!(log.contains("## Completed phases"), "fake log: {log}");
    assert!(log.contains("- phase 01: Foundation"), "fake log: {log}");
}

#[tokio::test]
async fn open_post_run_pr_with_mock_git_records_title_and_body() {
    // Mock-based variant: avoids spawning gh entirely so we can pin the exact
    // title/body the runner produced for a finished one-phase run.
    let plan = plan::parse(ONE_PHASE_PLAN).unwrap();
    let mut state = runner::fresh_run_state(&plan, &Config::default(), Utc::now());
    state.completed.push(pid("01"));

    let summary = PrSummary {
        plan: &plan,
        state: &state,
        deferred: &DeferredDoc::empty(),
    };
    let expected_title = git::pr_title(&summary);
    let expected_body = git::pr_body(&summary);

    let git = MockGit::new();
    git.set_open_pr_response("https://github.com/example/repo/pull/9001");
    let url = git.open_pr(&expected_title, &expected_body).await.unwrap();
    assert_eq!(url, "https://github.com/example/repo/pull/9001");

    let ops = git.ops();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        MockOp::OpenPr { title, body } => {
            assert_eq!(title, &expected_title);
            assert_eq!(body, &expected_body);
            // Title pulls in the (single) completed phase's title.
            assert!(title.contains("Foundation"), "title: {title}");
            // Body has the standard sections.
            assert!(body.contains("## Run"), "body: {body}");
            assert!(body.contains("## Completed phases"), "body: {body}");
            assert!(body.contains("- phase 01: Foundation"), "body: {body}");
            assert!(body.contains("## Token usage"), "body: {body}");
        }
        other => panic!("expected OpenPr, got {other:?}"),
    }
}
