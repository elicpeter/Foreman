//! Integration tests for the phase-12 runner.
//!
//! Exercises the runner end-to-end against a real `git init`'d workspace, with
//! a [`ScriptedAgent`] (defined below) standing in for the production agent.
//! `ScriptedAgent` is a per-call queue of `Script`s, each describing a set of
//! file mutations and a stop reason; the runner dispatches it once per phase.
//! Every test covers one of the acceptance criteria spelled out in plan.md
//! phase 12.

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

use foreman::agent::{Agent, AgentEvent, AgentOutcome, AgentRequest, StopReason};
use foreman::config::Config;
use foreman::deferred::DeferredDoc;
use foreman::git::{Git, ShellGit};
use foreman::plan::{self, PhaseId};
use foreman::runner::{self, HaltReason, RunSummary, Runner};
use foreman::state::TokenUsage;

fn pid(s: &str) -> PhaseId {
    PhaseId::parse(s).expect("valid phase id")
}

/// One scripted phase. Empty by default — the agent does nothing.
#[derive(Default, Clone)]
struct Script {
    /// Files to write or overwrite, relative to the workspace.
    writes: Vec<(PathBuf, Vec<u8>)>,
    /// Override the stop reason. Defaults to `Completed`.
    stop_reason: Option<StopReason>,
    /// Override the exit code. Defaults to 0.
    exit_code: Option<i32>,
}

impl Script {
    fn write(mut self, rel: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) -> Self {
        self.writes.push((rel.into(), bytes.into()));
        self
    }
}

/// Per-call scripted agent. Each `agent.run` pops the next [`Script`] off the
/// queue, applies its writes, and reports the configured outcome.
struct ScriptedAgent {
    name: String,
    scripts: Mutex<VecDeque<Script>>,
}

impl ScriptedAgent {
    fn new(scripts: Vec<Script>) -> Self {
        Self {
            name: "scripted".to_string(),
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
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        for (rel, bytes) in &script.writes {
            let path = req.workdir.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(&path, bytes).expect("scripted agent: file write failed");
        }
        // Always materialize the log file so the runner's expected per-attempt
        // log path exists on disk.
        if let Some(parent) = req.log_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&req.log_path, b"scripted log\n").ok();
        let _ = events.send(AgentEvent::Stdout("scripted ran".into())).await;
        Ok(AgentOutcome {
            exit_code: script.exit_code.unwrap_or(0),
            stop_reason: script.stop_reason.unwrap_or(StopReason::Completed),
            tokens: TokenUsage::default(),
            log_path: req.log_path,
        })
    }
}

const THREE_PHASE_PLAN: &str = "\
---
current_phase: \"01\"
---

# Foreman Plan

Three-phase test fixture.

# Phase 01: First

**Scope.** First phase.

# Phase 02: Second

**Scope.** Second phase.

# Phase 03: Third

**Scope.** Third phase.
";

const ONE_PHASE_PLAN: &str = "\
---
current_phase: \"01\"
---

# Foreman Plan

# Phase 01: Single

**Scope.** Only phase.
";

const EMPTY_DEFERRED: &str = "## Deferred items\n\n## Deferred phases\n";

fn make_workspace(plan_text: &str, deferred_text: &str) -> tempfile::TempDir {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("plan.md"), plan_text).unwrap();
    fs::write(dir.path().join("deferred.md"), deferred_text).unwrap();
    fs::create_dir_all(dir.path().join(".foreman/snapshots")).unwrap();
    fs::create_dir_all(dir.path().join(".foreman/logs")).unwrap();
    dir
}

fn init_git_repo(dir: &Path) {
    let status = Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(dir)
        .status()
        .expect("git init");
    assert!(status.success(), "git init failed");
    for (k, v) in [
        ("user.name", "foreman-test"),
        ("user.email", "foreman@test"),
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
        .expect("git seed commit");
    assert!(status.success(), "git seed commit failed");
}

fn git_log_oneline(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["log", "--oneline", "--all"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

async fn build_runner(
    workspace: &Path,
    plan_text: &str,
    deferred_text: &str,
    config: Config,
    agent: ScriptedAgent,
) -> (Runner<ScriptedAgent, ShellGit>, ShellGit) {
    let plan = plan::parse(plan_text).expect("parse plan");
    let deferred = if deferred_text.trim().is_empty() {
        DeferredDoc::empty()
    } else {
        foreman::deferred::parse(deferred_text).expect("parse deferred")
    };
    let state = runner::fresh_run_state(&plan, &config, Utc::now());

    let git = ShellGit::new(workspace);
    git.create_branch(&state.branch).await.unwrap();
    git.checkout(&state.branch).await.unwrap();

    let runner_git = ShellGit::new(workspace);
    let runner = Runner::new(
        workspace.to_path_buf(),
        config,
        plan,
        deferred,
        state,
        agent,
        runner_git,
    );
    (runner, git)
}

#[tokio::test]
async fn run_advances_through_three_phase_plan() {
    let dir = make_workspace(THREE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"//! phase 1\n"),
        Script::default().write("src/phase_02.rs", b"//! phase 2\n"),
        Script::default().write("src/phase_03.rs", b"//! phase 3\n"),
    ]);

    let (mut runner, _branch_git) =
        build_runner(dir.path(), THREE_PHASE_PLAN, EMPTY_DEFERRED, Config::default(), agent).await;

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished), "summary: {summary:?}");

    let plan_after = fs::read_to_string(dir.path().join("plan.md")).unwrap();
    let plan = plan::parse(&plan_after).expect("plan still parses");
    assert_eq!(
        plan.current_phase.as_str(),
        "03",
        "current_phase should sit at the final phase after the last phase completes"
    );

    let state = foreman::state::load(dir.path()).unwrap().expect("state");
    let completed: Vec<&str> = state.completed.iter().map(|p| p.as_str()).collect();
    assert_eq!(completed, vec!["01", "02", "03"]);

    let log = git_log_oneline(dir.path());
    let phase_commits: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("[foreman] phase"))
        .collect();
    assert_eq!(
        phase_commits.len(),
        3,
        "expected 3 phase commits, got log:\n{log:?}"
    );

    for phase in ["01", "02", "03"] {
        assert!(
            dir.path().join(format!("src/phase_{}.rs", phase)).exists(),
            "src/phase_{}.rs must be on disk",
            phase
        );
    }
}

#[tokio::test]
async fn halts_on_plan_tamper_and_restores_snapshot() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let bogus_plan = "---\ncurrent_phase: \"99\"\n---\n\n# Phase 99: bogus\n";
    let agent = ScriptedAgent::new(vec![Script::default().write("plan.md", bogus_plan.as_bytes())]);
    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, Config::default(), agent).await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert_eq!(reason, HaltReason::PlanTampered);
        }
        other => panic!("expected halt, got {other:?}"),
    }

    let plan_after = fs::read_to_string(dir.path().join("plan.md")).unwrap();
    assert_eq!(
        plan_after, ONE_PHASE_PLAN,
        "plan.md must be byte-for-byte restored after tamper"
    );

    let state = foreman::state::load(dir.path()).unwrap().expect("state");
    assert!(
        state.completed.is_empty(),
        "no phase should be marked completed after a tamper halt"
    );
}

#[tokio::test]
async fn halts_on_invalid_deferred_and_restores() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let bad_deferred = "## Garbage\n\n- not valid\n";
    let agent = ScriptedAgent::new(vec![Script::default().write("deferred.md", bad_deferred.as_bytes())]);
    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, Config::default(), agent).await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert!(
                matches!(reason, HaltReason::DeferredInvalid(_)),
                "got {reason:?}"
            );
        }
        other => panic!("expected halt, got {other:?}"),
    }

    let deferred_after = fs::read_to_string(dir.path().join("deferred.md")).unwrap();
    assert_eq!(
        deferred_after, EMPTY_DEFERRED,
        "deferred.md must be restored after parse failure"
    );
}

#[tokio::test]
async fn halts_on_test_failure_with_no_fixer() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let mut config = Config::default();
    config.tests.command = Some("/bin/sh -c false".to_string());
    // Explicitly disable the fixer for this test — we want the bare phase-12
    // "implementer fails the suite" path, not the fixer loop.
    config.retries.fixer_max_attempts = 0;

    let agent = ScriptedAgent::new(vec![Script::default().write("src/lib.rs", b"// placeholder\n")]);
    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, config, agent).await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert!(matches!(reason, HaltReason::TestsFailed(_)), "got {reason:?}");
        }
        other => panic!("expected halt, got {other:?}"),
    }

    // No commit should have landed on the per-run branch — only the seed is there.
    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("[foreman] phase")),
        "no phase commits expected on test failure; got log:\n{log:?}"
    );

    // attempts = 1 because no fixer dispatches were issued.
    let state = foreman::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(state.attempts.get(&pid("01")).copied(), Some(1));
}

#[tokio::test]
async fn advances_with_no_commit_when_only_deferred_changed() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let new_deferred = "## Deferred items\n\n- [ ] open item from agent\n\n## Deferred phases\n";
    let agent = ScriptedAgent::new(vec![Script::default().write("deferred.md", new_deferred.as_bytes())]);
    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, Config::default(), agent).await;

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("[foreman] phase")),
        "deferred-only changes must not produce a commit; log:\n{log:?}"
    );

    // Deferred sweep keeps the unchecked item in place.
    let deferred = fs::read_to_string(dir.path().join("deferred.md")).unwrap();
    assert!(
        deferred.contains("open item from agent"),
        "open item must survive sweep; got: {deferred:?}"
    );

    // State still records phase as completed.
    let state = foreman::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(
        state.completed.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        vec!["01"]
    );
}

#[tokio::test]
async fn mixed_changes_with_plan_tamper_halts_before_commit() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let bogus_plan = "---\ncurrent_phase: \"99\"\n---\n\n# Phase 99: bogus\n";
    let agent = ScriptedAgent::new(vec![Script::default()
        .write("src/foo.rs", b"// real change\n")
        .write("plan.md", bogus_plan.as_bytes())]);
    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, Config::default(), agent).await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert_eq!(reason, HaltReason::PlanTampered);
        }
        other => panic!("expected halt, got {other:?}"),
    }

    // plan.md restored despite mixed changes.
    let plan_after = fs::read_to_string(dir.path().join("plan.md")).unwrap();
    assert_eq!(plan_after, ONE_PHASE_PLAN);
    // src/foo.rs remains in the working tree (we only revert the planning artifacts).
    assert!(dir.path().join("src/foo.rs").exists());
    // No commit landed.
    let log = git_log_oneline(dir.path());
    assert!(log.iter().all(|l| !l.contains("[foreman] phase")));
}

#[tokio::test]
async fn agent_failure_halts_with_agent_failure_reason() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![Script {
        stop_reason: Some(StopReason::Error("boom".into())),
        exit_code: Some(2),
        ..Script::default()
    }]);
    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, Config::default(), agent).await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            match reason {
                HaltReason::AgentFailure(msg) => assert!(msg.contains("boom"), "msg: {msg}"),
                other => panic!("expected AgentFailure, got {other:?}"),
            }
        }
        other => panic!("expected halt, got {other:?}"),
    }
}

/// Test runner script that exits 0 only when `.pass-marker` is present in the
/// workspace. Used by the fixer integration tests to model "tests start
/// failing, pass once the fixer creates the marker."
const PASS_MARKER_TEST_SCRIPT: &str = "#!/bin/sh\ntest -f .pass-marker\n";

#[tokio::test]
async fn fixer_succeeds_on_attempt_2_and_phase_commits() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());
    fs::write(dir.path().join(".test.sh"), PASS_MARKER_TEST_SCRIPT).unwrap();

    let mut config = Config::default();
    config.tests.command = Some("/bin/sh ./.test.sh".to_string());
    config.retries.fixer_max_attempts = 2;

    let agent = ScriptedAgent::new(vec![
        // Implementer: writes code but no marker → tests fail.
        Script::default().write("src/lib.rs", b"// implementer\n"),
        // Fixer attempt 1: still no marker → tests fail.
        Script::default().write("src/extra.rs", b"// fixer attempt 1\n"),
        // Fixer attempt 2: writes the marker → tests pass.
        Script::default().write(".pass-marker", b""),
    ]);

    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        config,
        agent,
    )
    .await;

    let summary = runner.run().await.unwrap();
    assert!(
        matches!(summary, RunSummary::Finished),
        "expected finish after fixer succeeds, got {summary:?}"
    );

    // Per-attempt fixer logs land at the spec'd path.
    let logs_dir = dir.path().join(".foreman/logs");
    assert!(
        logs_dir.join("phase-01-fix-1.log").exists(),
        "phase-01-fix-1.log must exist after first fixer attempt"
    );
    assert!(
        logs_dir.join("phase-01-fix-2.log").exists(),
        "phase-01-fix-2.log must exist after second fixer attempt"
    );

    // attempts counter == 3: implementer + 2 fixer dispatches.
    let state = foreman::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(
        state.attempts.get(&pid("01")).copied(),
        Some(3),
        "attempts should reflect 1 implementer + 2 fixer dispatches"
    );
    assert_eq!(
        state.completed.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        vec!["01"]
    );

    // One phase commit landed (the implementer + fixer changes are bundled).
    let log = git_log_oneline(dir.path());
    let phase_commits: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("[foreman] phase"))
        .collect();
    assert_eq!(
        phase_commits.len(),
        1,
        "expected single phase commit; got log:\n{log:?}"
    );

    // Files written by every dispatch are on disk.
    assert!(dir.path().join("src/lib.rs").exists());
    assert!(dir.path().join("src/extra.rs").exists());
    assert!(dir.path().join(".pass-marker").exists());
}

#[tokio::test]
async fn fixer_exhausts_retries_then_halts_with_tests_failed() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());
    fs::write(dir.path().join(".test.sh"), PASS_MARKER_TEST_SCRIPT).unwrap();

    let mut config = Config::default();
    config.tests.command = Some("/bin/sh ./.test.sh".to_string());
    config.retries.fixer_max_attempts = 2;

    // No script in the queue ever writes the marker, so every test run fails.
    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/lib.rs", b"// implementer\n"),
        Script::default().write("src/fix1.rs", b"// fixer attempt 1\n"),
        Script::default().write("src/fix2.rs", b"// fixer attempt 2\n"),
    ]);

    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        config,
        agent,
    )
    .await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert!(
                matches!(reason, HaltReason::TestsFailed(_)),
                "expected TestsFailed after fixer exhaustion, got {reason:?}"
            );
        }
        other => panic!("expected halt, got {other:?}"),
    }

    // Both fixer attempt logs exist even though the loop exhausted.
    let logs_dir = dir.path().join(".foreman/logs");
    assert!(logs_dir.join("phase-01-fix-1.log").exists());
    assert!(logs_dir.join("phase-01-fix-2.log").exists());

    // No phase commit landed.
    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("[foreman] phase")),
        "no phase commit expected on fixer exhaustion; got log:\n{log:?}"
    );

    // attempts counter == 3 (1 implementer + 2 fixer); phase NOT in completed.
    let state = foreman::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(state.attempts.get(&pid("01")).copied(), Some(3));
    assert!(
        state.completed.is_empty(),
        "no phase should be marked complete after a halt"
    );
}

#[tokio::test]
async fn fixer_emits_fixer_started_events_with_increasing_attempt() {
    use foreman::runner::Event;
    use tokio::sync::broadcast::error::RecvError;

    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());
    fs::write(dir.path().join(".test.sh"), PASS_MARKER_TEST_SCRIPT).unwrap();

    let mut config = Config::default();
    config.tests.command = Some("/bin/sh ./.test.sh".to_string());
    config.retries.fixer_max_attempts = 2;

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/lib.rs", b"// implementer\n"),
        Script::default().write("src/extra.rs", b"// fixer 1\n"),
        Script::default().write(".pass-marker", b""),
    ]);

    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, config, agent).await;
    let mut rx = runner.subscribe();

    let collector = tokio::spawn(async move {
        let mut fixer_events = Vec::new();
        loop {
            match rx.recv().await {
                Ok(Event::FixerStarted {
                    phase_id,
                    fixer_attempt,
                    attempt,
                }) => fixer_events.push((phase_id, fixer_attempt, attempt)),
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        fixer_events
    });

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    drop(runner);
    let events = collector.await.unwrap();
    let fixer_attempts: Vec<u32> = events.iter().map(|(_, fa, _)| *fa).collect();
    assert_eq!(fixer_attempts, vec![1, 2], "got fixer events: {events:?}");
    let totals: Vec<u32> = events.iter().map(|(_, _, a)| *a).collect();
    assert_eq!(
        totals,
        vec![2, 3],
        "total attempt counter should be 2 then 3 (after impl=1)"
    );
}
