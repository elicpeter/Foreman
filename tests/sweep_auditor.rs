//! Integration tests for the phase 04 sweep-auditor pass.
//!
//! Each test stands a workspace up against a real `git init`'d directory and
//! drives the runner with a [`ScriptedAgent`]. Sweep dispatches are followed
//! by an auditor pass when `[sweep] audit_enabled = true`; these tests cover
//! the approve path, the off-scope revert path, the post-audit test-failure
//! halt, the audit-disabled passthrough, and the empty-diff short-circuit.

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
use pitboss::plan;
use pitboss::runner::{self, AuditContextKind, Event, HaltReason, RunSummary, Runner};

#[derive(Default, Clone)]
struct Script {
    writes: Vec<(PathBuf, Vec<u8>)>,
    deletes: Vec<PathBuf>,
    stop_reason: Option<StopReason>,
    exit_code: Option<i32>,
}

impl Script {
    fn write(mut self, rel: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) -> Self {
        self.writes.push((rel.into(), bytes.into()));
        self
    }

    fn delete(mut self, rel: impl Into<PathBuf>) -> Self {
        self.deletes.push(rel.into());
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
        for rel in &script.deletes {
            let path = req.workdir.join(rel);
            if path.exists() {
                fs::remove_file(&path).ok();
            }
        }
        if let Some(parent) = req.log_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&req.log_path, b"scripted log\n").ok();
        let _ = events.send(AgentEvent::Stdout("scripted ran".into())).await;
        Ok(AgentOutcome {
            exit_code: script.exit_code.unwrap_or(0),
            stop_reason: script.stop_reason.unwrap_or(StopReason::Completed),
            tokens: Default::default(),
            log_path: req.log_path,
        })
    }
}

const TWO_PHASE_PLAN: &str = "\
---
current_phase: \"01\"
---

# Pitboss Plan

# Phase 01: First

**Scope.** First phase.

# Phase 02: Second

**Scope.** Second phase.
";

fn deferred_items_only(items: &[(&str, bool)]) -> String {
    let mut s = String::from("## Deferred items\n\n");
    for (text, done) in items {
        let mark = if *done { 'x' } else { ' ' };
        s.push_str(&format!("- [{mark}] {text}\n"));
    }
    s.push_str("\n## Deferred phases\n");
    s
}

fn make_workspace(plan_text: &str, deferred_text: &str) -> tempfile::TempDir {
    let dir = tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join(".pitboss/play/snapshots")).unwrap();
    fs::create_dir_all(dir.path().join(".pitboss/play/logs")).unwrap();
    fs::write(dir.path().join(".pitboss/play/plan.md"), plan_text).unwrap();
    fs::write(dir.path().join(".pitboss/play/deferred.md"), deferred_text).unwrap();
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
    assert!(status.success());
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

fn git_show_files(dir: &Path, commit: &str) -> Vec<String> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["show", "--name-only", "--pretty=format:", commit])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Config with both `[audit] enabled` and `[sweep] audit_enabled` on (the
/// default), with the trigger set so 5 unchecked items trip a sweep.
fn audit_full_enabled() -> Config {
    let mut c = Config::default();
    // Phase 08's trailing drain loop is covered in `sweep_final_loop.rs`;
    // these tests assert the audit-on between-phase flow with exact sweep
    // counts, so opt out of the drain.
    c.sweep.final_sweep_enabled = false;
    c
}

async fn build_runner(
    workspace: &Path,
    plan_text: &str,
    deferred_text: &str,
    config: Config,
    agent: ScriptedAgent,
) -> Runner<ScriptedAgent, ShellGit> {
    let plan_obj = plan::parse(plan_text).expect("parse plan");
    let deferred = if deferred_text.trim().is_empty() {
        DeferredDoc::empty()
    } else {
        pitboss::deferred::parse(deferred_text).expect("parse deferred")
    };
    let state = runner::fresh_run_state(&plan_obj, &config, Utc::now());

    let git = ShellGit::new(workspace);
    git.create_branch(&state.branch).await.unwrap();
    git.checkout(&state.branch).await.unwrap();

    let runner_git = ShellGit::new(workspace);
    Runner::new(
        workspace.to_path_buf(),
        config,
        plan_obj,
        deferred,
        state,
        agent,
        runner_git,
    )
}

/// Approve path: sweep agent resolves 3 items with a focused diff; the
/// auditor's review adds no edits; the post-audit test re-run passes; the
/// sweep commit lands.
#[tokio::test]
async fn sweep_auditor_approves_focused_diff() {
    use tokio::sync::broadcast::error::RecvError;

    let initial = deferred_items_only(&[
        ("polish error message", false),
        ("drop unused stub", false),
        ("rename flag to enabled", false),
        ("tighten test for empty deferred", false),
        ("document sweep section in README", false),
    ]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let post_sweep = deferred_items_only(&[
        ("polish error message", true),
        ("drop unused stub", true),
        ("rename flag to enabled", true),
        ("tighten test for empty deferred", false),
        ("document sweep section in README", false),
    ]);

    let agent = ScriptedAgent::new(vec![
        // Phase 01 implementer.
        Script::default().write("src/phase_01.rs", b"// phase 1\n"),
        // Phase 01 auditor: no edits, just a no-op.
        Script::default(),
        // Sweep implementer: ticks 3 items off; touches one in-scope file.
        Script::default()
            .write(".pitboss/play/deferred.md", post_sweep.as_bytes())
            .write("src/sweep_inscope.rs", b"// sweep in-scope\n"),
        // Sweep auditor: no edits — approves the diff as-is.
        Script::default(),
        // Phase 02 implementer.
        Script::default().write("src/phase_02.rs", b"// phase 2\n"),
        // Phase 02 auditor.
        Script::default(),
    ]);

    let mut runner = build_runner(
        dir.path(),
        TWO_PHASE_PLAN,
        &initial,
        audit_full_enabled(),
        agent,
    )
    .await;

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut events: Vec<String> = Vec::new();
        loop {
            match rx.recv().await {
                Ok(Event::AuditorStarted { context, .. }) => {
                    let kind = match context.kind {
                        AuditContextKind::Phase => "phase",
                        AuditContextKind::Sweep => "sweep",
                    };
                    events.push(format!("AuditorStarted({kind},{})", context.phase_id));
                }
                Ok(Event::AuditorSkippedNoChanges { context }) => {
                    let kind = match context.kind {
                        AuditContextKind::Phase => "phase",
                        AuditContextKind::Sweep => "sweep",
                    };
                    events.push(format!("AuditorSkipped({kind},{})", context.phase_id));
                }
                Ok(Event::SweepCompleted { resolved, .. }) => {
                    events.push(format!("SweepCompleted(resolved={resolved})"));
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        events
    });

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    drop(runner);
    let events = collector.await.unwrap();

    // Sweep auditor fired with the Sweep kind under phase id 01.
    assert!(
        events.contains(&"AuditorStarted(sweep,01)".to_string()),
        "expected sweep auditor dispatch, events: {events:?}"
    );
    assert!(
        events.contains(&"SweepCompleted(resolved=3)".to_string()),
        "expected 3 resolved items, events: {events:?}"
    );

    // Sweep commit landed.
    let log = git_log_oneline(dir.path());
    let sweep_commit = log
        .iter()
        .find(|l| l.contains("sweep after phase 01: 3 deferred items resolved"))
        .unwrap_or_else(|| panic!("sweep commit must land; log:\n{log:?}"));
    let hash = sweep_commit.split_whitespace().next().unwrap();
    let files = git_show_files(dir.path(), hash);
    assert!(
        files.iter().any(|f| f == "src/sweep_inscope.rs"),
        "sweep commit must include the in-scope file; files: {files:?}"
    );

    // Sweep audit log file landed under the sweep prefix.
    assert!(
        dir.path()
            .join(".pitboss/play/logs/sweep-after-01-audit-1.log")
            .exists(),
        "sweep audit log must land under sweep- prefix"
    );
}

/// Auditor reverts off-scope changes: implementer touches an unrelated file in
/// addition to the in-scope work; the auditor deletes the unrelated file and
/// re-stages; the post-audit test re-run passes; the sweep commits with only
/// the in-scope changes.
#[tokio::test]
async fn sweep_auditor_reverts_off_scope_changes() {
    let initial = deferred_items_only(&[
        ("polish error message", false),
        ("drop unused stub", false),
        ("rename flag to enabled", false),
        ("tighten test for empty deferred", false),
        ("document sweep section in README", false),
    ]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let post_sweep = deferred_items_only(&[
        ("polish error message", true),
        ("drop unused stub", true),
        ("rename flag to enabled", false),
        ("tighten test for empty deferred", false),
        ("document sweep section in README", false),
    ]);

    let agent = ScriptedAgent::new(vec![
        // Phase 01 implementer.
        Script::default().write("src/phase_01.rs", b"// phase 1\n"),
        // Phase 01 auditor.
        Script::default(),
        // Sweep implementer: ticks 2 items off; writes an in-scope file AND
        // an unrelated drive-by refactor file.
        Script::default()
            .write(".pitboss/play/deferred.md", post_sweep.as_bytes())
            .write("src/sweep_inscope.rs", b"// sweep in-scope\n")
            .write("src/unrelated_refactor.rs", b"// drive-by\n"),
        // Sweep auditor: deletes the unrelated file, leaves the in-scope work.
        Script::default().delete("src/unrelated_refactor.rs"),
        // Phase 02 implementer + auditor.
        Script::default().write("src/phase_02.rs", b"// phase 2\n"),
        Script::default(),
    ]);

    let mut runner = build_runner(
        dir.path(),
        TWO_PHASE_PLAN,
        &initial,
        audit_full_enabled(),
        agent,
    )
    .await;

    let summary = runner.run().await.unwrap();
    assert!(
        matches!(summary, RunSummary::Finished),
        "expected finish, got {summary:?}"
    );

    let log = git_log_oneline(dir.path());
    let sweep_commit = log
        .iter()
        .find(|l| l.contains("sweep after phase 01:"))
        .unwrap_or_else(|| panic!("sweep commit must land; log:\n{log:?}"));
    let hash = sweep_commit.split_whitespace().next().unwrap();
    let files = git_show_files(dir.path(), hash);
    assert!(
        files.iter().any(|f| f == "src/sweep_inscope.rs"),
        "in-scope file must commit; files: {files:?}"
    );
    assert!(
        files.iter().all(|f| f != "src/unrelated_refactor.rs"),
        "unrelated file must be reverted before commit; files: {files:?}"
    );
    assert!(
        !dir.path().join("src/unrelated_refactor.rs").exists(),
        "unrelated file must not exist on disk after the sweep"
    );
}

/// Auditor halts via test failure: auditor's edits break the test suite; the
/// sweep halts with `HaltReason::TestsFailed`; `pending_sweep` stays true so a
/// resume can retry.
#[tokio::test]
async fn sweep_auditor_test_failure_halts_sweep() {
    const PASS_MARKER_TEST: &str = "#!/bin/sh\ntest -f .pass-marker\n";

    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());
    fs::write(dir.path().join(".test.sh"), PASS_MARKER_TEST).unwrap();
    // Pre-seed the marker so phase 01 + sweep impl pass tests.
    fs::write(dir.path().join(".pass-marker"), b"").unwrap();

    let mut config = audit_full_enabled();
    config.tests.command = Some("/bin/sh ./.test.sh".to_string());
    // Disable the fixer so a post-audit test failure halts directly.
    config.retries.fixer_max_attempts = 0;

    let post_sweep = deferred_items_only(&[
        ("a", true),
        ("b", true),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);

    let agent = ScriptedAgent::new(vec![
        // Phase 01 impl: tests pass.
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        // Phase 01 auditor.
        Script::default(),
        // Sweep impl: ticks 2 items off; writes in-scope code; tests pass.
        Script::default()
            .write(".pitboss/play/deferred.md", post_sweep.as_bytes())
            .write("src/sweep_inscope.rs", b"// sweep\n"),
        // Sweep auditor: rewrites .test.sh to always fail, breaking tests.
        Script::default().write(".test.sh", "#!/bin/sh\nfalse\n"),
    ]);

    let mut runner = build_runner(dir.path(), TWO_PHASE_PLAN, &initial, config, agent).await;

    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert!(
                matches!(reason, HaltReason::TestsFailed(_)),
                "expected TestsFailed, got {reason:?}"
            );
        }
        other => panic!("expected halt, got {other:?}"),
    }

    // Sweep commit must NOT have landed.
    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("sweep after phase")),
        "no sweep commit expected on auditor-induced halt; log:\n{log:?}"
    );

    // pending_sweep stays true so a resume can retry the sweep.
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(
        state.pending_sweep,
        "pending_sweep must persist after a sweep auditor halt"
    );
}

/// `[sweep] audit_enabled = false` skips the sweep auditor entirely. The phase
/// auditor still runs for phases — independent toggles.
#[tokio::test]
async fn sweep_auditor_disabled_skips_only_sweep_audit() {
    use tokio::sync::broadcast::error::RecvError;

    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let mut config = audit_full_enabled();
    // Phase audit ON; sweep audit OFF.
    assert!(config.audit.enabled);
    config.sweep.audit_enabled = false;

    let post_sweep = deferred_items_only(&[
        ("a", true),
        ("b", true),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);

    let agent = ScriptedAgent::new(vec![
        // Phase 01 impl.
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        // Phase 01 auditor (still fires — phase audit enabled).
        Script::default(),
        // Sweep impl. NO sweep auditor script — auditor must not dispatch.
        Script::default()
            .write(".pitboss/play/deferred.md", post_sweep.as_bytes())
            .write("src/sweep.rs", b"// sweep\n"),
        // Phase 02 impl.
        Script::default().write("src/phase_02.rs", b"// 2\n"),
        // Phase 02 auditor.
        Script::default(),
    ]);

    let mut runner = build_runner(dir.path(), TWO_PHASE_PLAN, &initial, config, agent).await;

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut phase_audits = 0;
        let mut sweep_audits = 0;
        loop {
            match rx.recv().await {
                Ok(Event::AuditorStarted { context, .. }) => match context.kind {
                    AuditContextKind::Phase => phase_audits += 1,
                    AuditContextKind::Sweep => sweep_audits += 1,
                },
                Ok(Event::AuditorSkippedNoChanges { context }) => {
                    if let AuditContextKind::Sweep = context.kind {
                        sweep_audits += 1;
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        (phase_audits, sweep_audits)
    });

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    drop(runner);
    let (phase_audits, sweep_audits) = collector.await.unwrap();
    assert_eq!(phase_audits, 2, "phase auditor must run for both phases");
    assert_eq!(
        sweep_audits, 0,
        "sweep auditor must not dispatch when [sweep] audit_enabled = false"
    );

    // Sweep commit still lands.
    let log = git_log_oneline(dir.path());
    assert!(log
        .iter()
        .any(|l| l.contains("sweep after phase 01: 2 deferred items resolved")));
}

/// Empty diff after sweep: sweep agent makes no code changes (only flips items
/// in deferred.md, which is excluded from the commit). The sweep auditor
/// short-circuits via `AuditorSkippedNoChanges`; the sweep proceeds to the
/// no-commit branch.
#[tokio::test]
async fn sweep_auditor_short_circuits_on_empty_diff() {
    use tokio::sync::broadcast::error::RecvError;

    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let post_sweep = deferred_items_only(&[
        ("a", true),
        ("b", true),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        // Phase 01 auditor.
        Script::default(),
        // Sweep impl: only edits deferred.md (excluded from commit), so the
        // staged diff is empty and the sweep auditor must short-circuit.
        Script::default().write(".pitboss/play/deferred.md", post_sweep.as_bytes()),
        // No sweep auditor script — runner must skip the dispatch.
        Script::default().write("src/phase_02.rs", b"// 2\n"),
        Script::default(),
    ]);

    let mut runner = build_runner(
        dir.path(),
        TWO_PHASE_PLAN,
        &initial,
        audit_full_enabled(),
        agent,
    )
    .await;

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut sweep_started = false;
        let mut sweep_skipped = false;
        loop {
            match rx.recv().await {
                Ok(Event::AuditorStarted { context, .. }) => {
                    if let AuditContextKind::Sweep = context.kind {
                        sweep_started = true;
                    }
                }
                Ok(Event::AuditorSkippedNoChanges { context }) => {
                    if let AuditContextKind::Sweep = context.kind {
                        sweep_skipped = true;
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        (sweep_started, sweep_skipped)
    });

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    drop(runner);
    let (sweep_started, sweep_skipped) = collector.await.unwrap();
    assert!(
        sweep_skipped,
        "sweep auditor must emit AuditorSkippedNoChanges on empty diff"
    );
    assert!(
        !sweep_started,
        "sweep auditor must not dispatch when there is no staged diff"
    );

    // No sweep commit landed (only excluded paths changed).
    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("sweep after phase")),
        "no sweep commit when diff is empty; log:\n{log:?}"
    );
}
