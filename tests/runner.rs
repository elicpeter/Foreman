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

use pitboss::agent::{Agent, AgentEvent, AgentOutcome, AgentRequest, StopReason};
use pitboss::config::Config;
use pitboss::deferred::DeferredDoc;
use pitboss::git::{Git, ShellGit};
use pitboss::plan::{self, PhaseId};
use pitboss::runner::{self, HaltReason, RunSummary, Runner};
use pitboss::state::TokenUsage;

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
    /// Token usage reported back in the [`AgentOutcome`]. Defaults to zero.
    /// Used by the budget-overflow tests; the existing tests rely on the
    /// default zero so the runner's budget check is a no-op.
    tokens: Option<TokenUsage>,
}

impl Script {
    fn write(mut self, rel: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) -> Self {
        self.writes.push((rel.into(), bytes.into()));
        self
    }

    fn tokens(mut self, tokens: TokenUsage) -> Self {
        self.tokens = Some(tokens);
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
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
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
            tokens: script.tokens.unwrap_or_default(),
            log_path: req.log_path,
        })
    }
}

const THREE_PHASE_PLAN: &str = "\
---
current_phase: \"01\"
---

# Pitboss Plan

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

# Pitboss Plan

# Phase 01: Single

**Scope.** Only phase.
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
    assert!(status.success(), "git init failed");
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

/// `Config::default()` with the auditor pass disabled. Most tests below
/// exercise the phase 12 / 13 flow (implementer → tests → fixer → commit) and
/// don't want the phase 14 auditor consuming scripts off the agent queue.
fn audit_disabled() -> Config {
    let mut c = Config::default();
    c.audit.enabled = false;
    c
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
        pitboss::deferred::parse(deferred_text).expect("parse deferred")
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

    let (mut runner, _branch_git) = build_runner(
        dir.path(),
        THREE_PHASE_PLAN,
        EMPTY_DEFERRED,
        audit_disabled(),
        agent,
    )
    .await;

    let summary = runner.run().await.unwrap();
    assert!(
        matches!(summary, RunSummary::Finished),
        "summary: {summary:?}"
    );

    let plan_after = fs::read_to_string(dir.path().join("plan.md")).unwrap();
    let plan = plan::parse(&plan_after).expect("plan still parses");
    assert_eq!(
        plan.current_phase.as_str(),
        "03",
        "current_phase should sit at the final phase after the last phase completes"
    );

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    let completed: Vec<&str> = state.completed.iter().map(|p| p.as_str()).collect();
    assert_eq!(completed, vec!["01", "02", "03"]);

    let log = git_log_oneline(dir.path());
    let phase_commits: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("[pitboss] phase"))
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
    let agent = ScriptedAgent::new(vec![
        Script::default().write("plan.md", bogus_plan.as_bytes())
    ]);
    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        audit_disabled(),
        agent,
    )
    .await;

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

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
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
    let agent = ScriptedAgent::new(vec![
        Script::default().write("deferred.md", bad_deferred.as_bytes())
    ]);
    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        audit_disabled(),
        agent,
    )
    .await;

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

    let mut config = audit_disabled();
    config.tests.command = Some("/bin/sh -c false".to_string());
    // Explicitly disable the fixer for this test — we want the bare phase-12
    // "implementer fails the suite" path, not the fixer loop.
    config.retries.fixer_max_attempts = 0;

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/lib.rs", b"// placeholder\n")
    ]);
    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, config, agent).await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert!(
                matches!(reason, HaltReason::TestsFailed(_)),
                "got {reason:?}"
            );
        }
        other => panic!("expected halt, got {other:?}"),
    }

    // No commit should have landed on the per-run branch — only the seed is there.
    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("[pitboss] phase")),
        "no phase commits expected on test failure; got log:\n{log:?}"
    );

    // attempts = 1 because no fixer dispatches were issued.
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(state.attempts.get(&pid("01")).copied(), Some(1));
}

#[tokio::test]
async fn advances_with_no_commit_when_only_deferred_changed() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let new_deferred = "## Deferred items\n\n- [ ] open item from agent\n\n## Deferred phases\n";
    let agent = ScriptedAgent::new(vec![
        Script::default().write("deferred.md", new_deferred.as_bytes())
    ]);
    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        audit_disabled(),
        agent,
    )
    .await;

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("[pitboss] phase")),
        "deferred-only changes must not produce a commit; log:\n{log:?}"
    );

    // Deferred sweep keeps the unchecked item in place.
    let deferred = fs::read_to_string(dir.path().join("deferred.md")).unwrap();
    assert!(
        deferred.contains("open item from agent"),
        "open item must survive sweep; got: {deferred:?}"
    );

    // State still records phase as completed.
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(
        state
            .completed
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
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
    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        audit_disabled(),
        agent,
    )
    .await;

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
    assert!(log.iter().all(|l| !l.contains("[pitboss] phase")));
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
    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        audit_disabled(),
        agent,
    )
    .await;

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

    let mut config = audit_disabled();
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

    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, config, agent).await;

    let summary = runner.run().await.unwrap();
    assert!(
        matches!(summary, RunSummary::Finished),
        "expected finish after fixer succeeds, got {summary:?}"
    );

    // Per-attempt fixer logs land at the spec'd path.
    let logs_dir = dir.path().join(".pitboss/logs");
    assert!(
        logs_dir.join("phase-01-fix-1.log").exists(),
        "phase-01-fix-1.log must exist after first fixer attempt"
    );
    assert!(
        logs_dir.join("phase-01-fix-2.log").exists(),
        "phase-01-fix-2.log must exist after second fixer attempt"
    );

    // attempts counter == 3: implementer + 2 fixer dispatches.
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(
        state.attempts.get(&pid("01")).copied(),
        Some(3),
        "attempts should reflect 1 implementer + 2 fixer dispatches"
    );
    assert_eq!(
        state
            .completed
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
        vec!["01"]
    );

    // One phase commit landed (the implementer + fixer changes are bundled).
    let log = git_log_oneline(dir.path());
    let phase_commits: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("[pitboss] phase"))
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

    let mut config = audit_disabled();
    config.tests.command = Some("/bin/sh ./.test.sh".to_string());
    config.retries.fixer_max_attempts = 2;

    // No script in the queue ever writes the marker, so every test run fails.
    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/lib.rs", b"// implementer\n"),
        Script::default().write("src/fix1.rs", b"// fixer attempt 1\n"),
        Script::default().write("src/fix2.rs", b"// fixer attempt 2\n"),
    ]);

    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, config, agent).await;

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
    let logs_dir = dir.path().join(".pitboss/logs");
    assert!(logs_dir.join("phase-01-fix-1.log").exists());
    assert!(logs_dir.join("phase-01-fix-2.log").exists());

    // No phase commit landed.
    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("[pitboss] phase")),
        "no phase commit expected on fixer exhaustion; got log:\n{log:?}"
    );

    // attempts counter == 3 (1 implementer + 2 fixer); phase NOT in completed.
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(state.attempts.get(&pid("01")).copied(), Some(3));
    assert!(
        state.completed.is_empty(),
        "no phase should be marked complete after a halt"
    );
}

#[tokio::test]
async fn fixer_emits_fixer_started_events_with_increasing_attempt() {
    use pitboss::runner::Event;
    use tokio::sync::broadcast::error::RecvError;

    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());
    fs::write(dir.path().join(".test.sh"), PASS_MARKER_TEST_SCRIPT).unwrap();

    let mut config = audit_disabled();
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

/// Audit-enabled small-fix path: implementer writes code, auditor inlines a
/// small extra source file, both land in a single per-phase commit.
#[tokio::test]
async fn auditor_inlines_small_fix_and_commits_combined_diff() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    // audit defaults (enabled = true) but no test runner override → tests skipped.
    let config = Config::default();

    let agent = ScriptedAgent::new(vec![
        // Implementer.
        Script::default().write("src/lib.rs", b"// implementer\n"),
        // Auditor: small inline fix.
        Script::default().write("src/audit_extra.rs", b"// audit fix\n"),
    ]);

    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, config, agent).await;

    let summary = runner.run().await.unwrap();
    assert!(
        matches!(summary, RunSummary::Finished),
        "expected finish, got {summary:?}"
    );

    // Both files exist on disk.
    assert!(dir.path().join("src/lib.rs").exists());
    assert!(dir.path().join("src/audit_extra.rs").exists());

    // Single phase commit landed (implementer + auditor edits combined).
    let log = git_log_oneline(dir.path());
    let phase_commits: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("[pitboss] phase"))
        .collect();
    assert_eq!(
        phase_commits.len(),
        1,
        "expected one combined commit; got log:\n{log:?}"
    );

    // Audit log written under the conventional path.
    assert!(
        dir.path()
            .join(".pitboss/logs/phase-01-audit-1.log")
            .exists(),
        "phase-01-audit-1.log must exist after the auditor pass"
    );

    // attempts counter == 2 (1 implementer + 1 auditor).
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(
        state.attempts.get(&pid("01")).copied(),
        Some(2),
        "attempts should reflect 1 implementer + 1 auditor dispatch"
    );

    // The committed tree contains both files.
    let staged = std::process::Command::new("git")
        .args(["-C"])
        .arg(dir.path())
        .args(["show", "--name-only", "--format=", "HEAD"])
        .output()
        .unwrap();
    let staged = String::from_utf8(staged.stdout).unwrap();
    assert!(staged.contains("src/lib.rs"), "show: {staged}");
    assert!(staged.contains("src/audit_extra.rs"), "show: {staged}");
}

/// Audit-enabled defer path: implementer writes code, auditor only appends to
/// `deferred.md`. The runner commits the implementer's code and the deferred
/// item survives the post-commit sweep (it's unchecked).
#[tokio::test]
async fn auditor_defers_large_finding_to_deferred_md() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let config = Config::default();

    let auditor_deferred =
        "## Deferred items\n\n- [ ] auditor: refactor the foo module\n\n## Deferred phases\n";
    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/lib.rs", b"// implementer\n"),
        // Auditor only appends to deferred.md; no code changes.
        Script::default().write("deferred.md", auditor_deferred.as_bytes()),
    ]);

    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, config, agent).await;

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    // One commit lands — the implementer's src/lib.rs.
    let log = git_log_oneline(dir.path());
    let phase_commits: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("[pitboss] phase"))
        .collect();
    assert_eq!(phase_commits.len(), 1, "log:\n{log:?}");

    // Auditor's deferred item survived the sweep (it's unchecked).
    let deferred_after = fs::read_to_string(dir.path().join("deferred.md")).unwrap();
    assert!(
        deferred_after.contains("auditor: refactor the foo module"),
        "deferred.md after run:\n{deferred_after}"
    );
}

/// Audit-enabled path on a phase that produced no code changes (only
/// `deferred.md` was touched by the implementer): the auditor must be skipped
/// because there's no diff to audit, and the run advances without a commit.
#[tokio::test]
async fn auditor_skipped_when_implementer_only_touched_planning_artifacts() {
    use pitboss::runner::Event;
    use tokio::sync::broadcast::error::RecvError;

    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let new_deferred = "## Deferred items\n\n- [ ] open item\n\n## Deferred phases\n";
    let agent = ScriptedAgent::new(vec![
        Script::default().write("deferred.md", new_deferred.as_bytes())
    ]);

    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        Config::default(),
        agent,
    )
    .await;

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut saw_skipped = false;
        let mut saw_started = false;
        loop {
            match rx.recv().await {
                Ok(Event::AuditorSkippedNoChanges { .. }) => saw_skipped = true,
                Ok(Event::AuditorStarted { .. }) => saw_started = true,
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        (saw_skipped, saw_started)
    });

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    drop(runner);
    let (saw_skipped, saw_started) = collector.await.unwrap();
    assert!(saw_skipped, "expected AuditorSkippedNoChanges event");
    assert!(
        !saw_started,
        "auditor must not dispatch when there is no staged diff"
    );

    // No commit (only excluded paths changed).
    let log = git_log_oneline(dir.path());
    assert!(log.iter().all(|l| !l.contains("[pitboss] phase")));

    // attempts counter stays at 1 (just the implementer).
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(state.attempts.get(&pid("01")).copied(), Some(1));
}

/// Auditor breaks the test suite → halt with TestsFailed. The phase must NOT
/// be marked completed and no commit lands.
#[tokio::test]
async fn auditor_test_failure_halts_phase() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());
    fs::write(dir.path().join(".test.sh"), PASS_MARKER_TEST_SCRIPT).unwrap();

    let mut config = Config::default();
    config.tests.command = Some("/bin/sh ./.test.sh".to_string());
    // Disable the fixer so the implementer must produce a passing suite on its
    // own; this keeps the test focused on the post-audit re-run.
    config.retries.fixer_max_attempts = 0;

    let agent = ScriptedAgent::new(vec![
        // Implementer: writes the marker → tests pass.
        Script::default()
            .write("src/lib.rs", b"// implementer\n")
            .write(".pass-marker", b""),
        // Auditor edit that breaks the suite: rewrite the test script so it
        // always exits non-zero. Models the case where an audit-time fix has
        // an unintended side effect.
        Script::default().write(".test.sh", "#!/bin/sh\nfalse\n"),
    ]);

    let (mut runner, _g) =
        build_runner(dir.path(), ONE_PHASE_PLAN, EMPTY_DEFERRED, config, agent).await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert!(
                matches!(reason, HaltReason::TestsFailed(_)),
                "expected TestsFailed after audit broke tests, got {reason:?}"
            );
        }
        other => panic!("expected halt, got {other:?}"),
    }

    // Phase not marked completed.
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(state.completed.is_empty());
    // attempts counter == 2 (implementer + auditor).
    assert_eq!(state.attempts.get(&pid("01")).copied(), Some(2));

    // No phase commit landed.
    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("[pitboss] phase")),
        "no phase commit expected after auditor broke tests; log:\n{log:?}"
    );
}

/// Audit-disabled path is unchanged from phase 13 behavior: implementer-only
/// flow with no auditor dispatch and no AuditorStarted event.
#[tokio::test]
async fn audit_disabled_path_skips_auditor_entirely() {
    use pitboss::runner::Event;
    use tokio::sync::broadcast::error::RecvError;

    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/lib.rs", b"// implementer\n")
    ]);

    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        audit_disabled(),
        agent,
    )
    .await;

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut saw_audit_event = false;
        loop {
            match rx.recv().await {
                Ok(Event::AuditorStarted { .. }) | Ok(Event::AuditorSkippedNoChanges { .. }) => {
                    saw_audit_event = true
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        saw_audit_event
    });

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    drop(runner);
    let saw_audit_event = collector.await.unwrap();
    assert!(
        !saw_audit_event,
        "no AuditorStarted / AuditorSkippedNoChanges events when audit is disabled"
    );

    // attempts counter == 1 (implementer only).
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(state.attempts.get(&pid("01")).copied(), Some(1));
}

/// Regression test: `runner::log_events` must return after the runner emits
/// its terminal event, even though the runner keeps holding the broadcast
/// `Sender` (it's needed for post-run lookups like PR creation). Before this
/// was wired up, `pitboss play --dry-run` would advance through every phase,
/// print "[pitboss] run finished", and then hang the process forever waiting
/// on the logger task.
#[tokio::test]
async fn log_events_returns_after_run_finished_even_with_runner_alive() {
    let dir = make_workspace(THREE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/p1.rs", b"// 1\n"),
        Script::default().write("src/p2.rs", b"// 2\n"),
        Script::default().write("src/p3.rs", b"// 3\n"),
    ]);

    let (mut runner, _g) = build_runner(
        dir.path(),
        THREE_PHASE_PLAN,
        EMPTY_DEFERRED,
        audit_disabled(),
        agent,
    )
    .await;

    let rx = runner.subscribe();
    let logger = tokio::spawn(pitboss::runner::log_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    // The runner is still in scope (so the broadcast Sender is alive); the
    // logger must still return because RunFinished was broadcast.
    tokio::time::timeout(std::time::Duration::from_secs(2), logger)
        .await
        .expect("log_events must return within 2s of RunFinished")
        .expect("logger task panicked");
}

/// Halt-side counterpart: `log_events` must also exit when the runner halts,
/// because `Runner::run()` broadcasts `Event::PhaseHalted` parallel to the
/// success-side `RunFinished`.
#[tokio::test]
async fn log_events_returns_after_phase_halted_even_with_runner_alive() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![Script {
        stop_reason: Some(StopReason::Error("synthetic halt".into())),
        exit_code: Some(2),
        ..Script::default()
    }]);

    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        audit_disabled(),
        agent,
    )
    .await;

    let rx = runner.subscribe();
    let logger = tokio::spawn(pitboss::runner::log_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Halted { .. }));

    tokio::time::timeout(std::time::Duration::from_secs(2), logger)
        .await
        .expect("log_events must return within 2s of PhaseHalted")
        .expect("logger task panicked");
}

/// `Runner::skip_tests(true)` short-circuits test detection: even when the
/// workspace contains a recognized layout (a `Cargo.toml` here), the runner
/// emits `TestsSkipped` and never spawns the suite. Phases still advance and
/// per-phase commits happen when the agent staged code.
#[tokio::test]
async fn skip_tests_bypasses_test_detection_and_still_advances() {
    use pitboss::runner::Event;
    use tokio::sync::broadcast::error::RecvError;

    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    // A real Cargo.toml would normally trigger `cargo test`; with skip_tests
    // on, the runner never invokes it. The body is intentionally not a valid
    // crate (no `[package]` section) — if the runner accidentally invoked
    // cargo here the test would fail loudly.
    fs::write(
        dir.path().join("Cargo.toml"),
        b"# placeholder, no package\n",
    )
    .unwrap();

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/lib.rs", b"// implementer\n")
    ]);

    let (mut runner, _g) = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        EMPTY_DEFERRED,
        audit_disabled(),
        agent,
    )
    .await;
    runner = runner.skip_tests(true);

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut saw_skipped = false;
        let mut saw_started = false;
        loop {
            match rx.recv().await {
                Ok(Event::TestsSkipped) => saw_skipped = true,
                Ok(Event::TestStarted) => saw_started = true,
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        (saw_skipped, saw_started)
    });

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    drop(runner);
    let (saw_skipped, saw_started) = collector.await.unwrap();
    assert!(saw_skipped, "TestsSkipped must fire when skip_tests is on");
    assert!(
        !saw_started,
        "TestStarted must not fire when skip_tests is on"
    );

    // The agent's code change still landed.
    let log = git_log_oneline(dir.path());
    let phase_commits: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("[pitboss] phase"))
        .collect();
    assert_eq!(
        phase_commits.len(),
        1,
        "expected a phase commit; log:\n{log:?}"
    );
}

/// Helper for the budget tests: builds a [`TokenUsage`] with the supplied
/// top-level totals and an empty `by_role` map.
///
/// The runner re-keys the outcome under the dispatch's role when folding into
/// [`RunState::token_usage`] (see `Runner::fold_token_usage`), so leaving
/// `by_role` empty here avoids double-counting against the per-role bucket.
fn tokens_total(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input,
        output,
        by_role: Default::default(),
    }
}

/// Phase 1 implementer reports tokens that exceed `max_total_tokens`. The
/// next phase's budget check fires before any further dispatch and halts the
/// run with `BudgetExceeded`.
#[tokio::test]
async fn token_budget_halts_run_before_next_phase_dispatch() {
    let dir = make_workspace(THREE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let mut config = audit_disabled();
    // Cap intentionally tiny: phase 1 will report 1500 tokens (1000 in + 500
    // out) and trip the budget check at the start of phase 2.
    config.budgets.max_total_tokens = Some(1000);

    let agent = ScriptedAgent::new(vec![
        Script::default()
            .write("src/phase_01.rs", b"// phase 1\n")
            .tokens(tokens_total(1000, 500)),
        Script::default().write("src/phase_02.rs", b"// phase 2\n"),
        Script::default().write("src/phase_03.rs", b"// phase 3\n"),
    ]);

    let (mut runner, _g) =
        build_runner(dir.path(), THREE_PHASE_PLAN, EMPTY_DEFERRED, config, agent).await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "02");
            match reason {
                HaltReason::BudgetExceeded(msg) => {
                    assert!(msg.contains("token"), "msg: {msg}");
                    assert!(
                        msg.contains("1500"),
                        "msg should report current usage: {msg}"
                    );
                }
                other => panic!("expected BudgetExceeded, got {other:?}"),
            }
        }
        other => panic!("expected halt, got {other:?}"),
    }

    // Phase 1 commit landed, phases 2 and 3 didn't.
    let log = git_log_oneline(dir.path());
    let phase_commits: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("[pitboss] phase"))
        .collect();
    assert_eq!(
        phase_commits.len(),
        1,
        "exactly one phase commit expected, got log:\n{log:?}"
    );

    // State reflects phase 1 completed and the implementer's per-role usage.
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(
        state
            .completed
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
        vec!["01"]
    );
    let impl_usage = state.token_usage.by_role.get("implementer").unwrap();
    assert_eq!(impl_usage.input, 1000);
    assert_eq!(impl_usage.output, 500);
    // Top-level totals match — implementer dispatched once.
    assert_eq!(state.token_usage.input, 1000);
    assert_eq!(state.token_usage.output, 500);
    // Phase 2 was never dispatched, so its attempts entry stays empty.
    assert!(!state.attempts.contains_key(&pid("02")));
}

/// USD budget enforcement: implementer reports tokens that, priced under the
/// default opus rate, blow past `max_total_usd`. Halt fires before phase 2.
#[tokio::test]
async fn usd_budget_halts_when_priced_usage_exceeds_cap() {
    let dir = make_workspace(THREE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let mut config = audit_disabled();
    // Default opus pricing: 1M input × $15 + 1M output × $75 = $90.
    // Cap of $50 is comfortably under that → halt fires before phase 2.
    config.budgets.max_total_usd = Some(50.0);

    let agent = ScriptedAgent::new(vec![
        Script::default()
            .write("src/phase_01.rs", b"// phase 1\n")
            .tokens(tokens_total(1_000_000, 1_000_000)),
        Script::default().write("src/phase_02.rs", b"// phase 2\n"),
        Script::default().write("src/phase_03.rs", b"// phase 3\n"),
    ]);

    let (mut runner, _g) =
        build_runner(dir.path(), THREE_PHASE_PLAN, EMPTY_DEFERRED, config, agent).await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "02");
            match reason {
                HaltReason::BudgetExceeded(msg) => {
                    assert!(msg.contains("USD"), "msg: {msg}");
                }
                other => panic!("expected BudgetExceeded, got {other:?}"),
            }
        }
        other => panic!("expected halt, got {other:?}"),
    }

    // Per-role breakdown was preserved through the halt.
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    let impl_usage = state.token_usage.by_role.get("implementer").unwrap();
    assert_eq!(impl_usage.input, 1_000_000);
    assert_eq!(impl_usage.output, 1_000_000);
}

/// Token budget set on a fresh run is checked before the very first dispatch
/// when prior usage already meets the cap (e.g., resumed from an earlier run
/// that had recorded usage). Verifies the budget check guards every dispatch
/// site, not just inter-phase transitions.
#[tokio::test]
async fn budget_check_fires_for_first_dispatch_when_usage_already_at_cap() {
    let dir = make_workspace(ONE_PHASE_PLAN, EMPTY_DEFERRED);
    init_git_repo(dir.path());

    let mut config = audit_disabled();
    config.budgets.max_total_tokens = Some(100);

    // Pre-seed RunState with usage that already meets the cap. The runner
    // exposes `fresh_run_state` for the standard fresh-start case; we build a
    // mutated state and persist it before constructing the Runner so the
    // first dispatch has a tripped budget to react to.
    use chrono::Utc;
    use pitboss::config::Config;
    use pitboss::deferred::DeferredDoc;
    use pitboss::plan;
    use pitboss::runner;
    use pitboss::state;

    let plan_obj = plan::parse(ONE_PHASE_PLAN).unwrap();
    let _ = DeferredDoc::empty();
    let mut state_obj = runner::fresh_run_state(&plan_obj, &Config::default(), Utc::now());
    // Pre-seed totals — by_role is irrelevant for the token-budget check,
    // which compares the top-level `input + output` sum against the cap.
    state_obj.token_usage = tokens_total(60, 50); // 110 ≥ 100
    state::save(dir.path(), Some(&state_obj)).unwrap();

    // Build runner with this state. We cannot reuse `build_runner` because it
    // calls `fresh_run_state` itself; do it manually.
    use pitboss::git::{Git, ShellGit};
    let git = ShellGit::new(dir.path());
    git.create_branch(&state_obj.branch).await.unwrap();
    git.checkout(&state_obj.branch).await.unwrap();

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/never.rs", b"// should not appear\n")
    ]);
    let runner_git = ShellGit::new(dir.path());
    let mut runner = pitboss::runner::Runner::new(
        dir.path().to_path_buf(),
        config,
        plan_obj,
        DeferredDoc::empty(),
        state_obj,
        agent,
        runner_git,
    );

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert!(
                matches!(reason, HaltReason::BudgetExceeded(_)),
                "got {reason:?}"
            );
        }
        other => panic!("expected halt, got {other:?}"),
    }

    // Implementer never dispatched, so the script's file isn't on disk and
    // attempts is empty.
    assert!(!dir.path().join("src/never.rs").exists());
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(state.attempts.is_empty());
}
