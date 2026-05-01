//! Integration tests for the phase 03 deferred-sweep dispatch.
//!
//! Each test stands a workspace up against a real `git init`'d directory and
//! drives the runner with a [`ScriptedAgent`] (cloned from the existing runner
//! integration tests). The scripted agent writes whatever bytes it's told to
//! when it's dispatched; the sweep flow's "agent flips items off" is modeled
//! by handing it a fresh `deferred.md` body to overwrite the on-disk file
//! with. The runner re-parses the file via `dispatch_and_validate` so the
//! cached `DeferredDoc` and the on-disk bytes stay in lockstep.

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
use pitboss::config::{Config, SweepConfig};
use pitboss::deferred::DeferredDoc;
use pitboss::git::{Git, ShellGit};
use pitboss::plan::{self, PhaseId};
use pitboss::runner::{self, Event, HaltReason, PhaseResult, RunSummary, Runner};

fn pid(s: &str) -> PhaseId {
    PhaseId::parse(s).expect("valid phase id")
}

#[derive(Default, Clone)]
struct Script {
    writes: Vec<(PathBuf, Vec<u8>)>,
    stop_reason: Option<StopReason>,
    exit_code: Option<i32>,
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

const ONE_PHASE_PLAN: &str = "\
---
current_phase: \"01\"
---

# Pitboss Plan

# Phase 01: Single

**Scope.** Only phase.
";

/// `## Deferred items` body with the supplied (text, done) pairs and an empty
/// `## Deferred phases` section. Counts of unchecked items are used to drive
/// the sweep trigger across the various tests.
fn deferred_items_only(items: &[(&str, bool)]) -> String {
    let mut s = String::from("## Deferred items\n\n");
    for (text, done) in items {
        let mark = if *done { 'x' } else { ' ' };
        s.push_str(&format!("- [{mark}] {text}\n"));
    }
    s.push_str("\n## Deferred phases\n");
    s
}

/// `## Deferred items` body plus a `## Deferred phases` block carrying one
/// `### From phase X: title` entry. Used by the H3-guard test.
fn deferred_with_phases(items: &[(&str, bool)], phases_body: &str) -> String {
    let mut s = String::from("## Deferred items\n\n");
    for (text, done) in items {
        let mark = if *done { 'x' } else { ' ' };
        s.push_str(&format!("- [{mark}] {text}\n"));
    }
    s.push_str("\n## Deferred phases\n\n");
    s.push_str(phases_body);
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

fn audit_disabled() -> Config {
    let mut c = Config::default();
    c.audit.enabled = false;
    c
}

async fn build_runner_with_state(
    workspace: &Path,
    plan_text: &str,
    deferred_text: &str,
    config: Config,
    agent: ScriptedAgent,
    state_override: Option<pitboss::state::RunState>,
) -> Runner<ScriptedAgent, ShellGit> {
    let plan_obj = plan::parse(plan_text).expect("parse plan");
    let deferred = if deferred_text.trim().is_empty() {
        DeferredDoc::empty()
    } else {
        pitboss::deferred::parse(deferred_text).expect("parse deferred")
    };
    let state = state_override
        .unwrap_or_else(|| runner::fresh_run_state(&plan_obj, &config, Utc::now()));

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

async fn build_runner(
    workspace: &Path,
    plan_text: &str,
    deferred_text: &str,
    config: Config,
    agent: ScriptedAgent,
) -> Runner<ScriptedAgent, ShellGit> {
    build_runner_with_state(workspace, plan_text, deferred_text, config, agent, None).await
}

/// Phase 01 leaves 6 unchecked items; the sweep trip-wire fires; the sweep
/// agent flips 4 of them; phase 02 runs after. The full event sequence and
/// the resulting deferred.md, state.json, and commit log are checked.
#[tokio::test]
async fn sweep_fires_between_phases_when_trigger_trips() {
    use tokio::sync::broadcast::error::RecvError;

    let initial = deferred_items_only(&[
        ("polish error message", false),
        ("drop unused stub", false),
        ("rename flag to enabled", false),
        ("tighten test for empty deferred", false),
        ("document sweep section in README", false),
        ("audit small_fix_line_limit default", false),
    ]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    // Sweep agent flips 4 items off, leaving 2 unchecked.
    let post_sweep_deferred = deferred_items_only(&[
        ("polish error message", true),
        ("drop unused stub", true),
        ("rename flag to enabled", true),
        ("tighten test for empty deferred", true),
        ("document sweep section in README", false),
        ("audit small_fix_line_limit default", false),
    ]);

    let agent = ScriptedAgent::new(vec![
        // Phase 01 implementer.
        Script::default().write("src/phase_01.rs", b"// phase 1\n"),
        // Sweep implementer: rewrite deferred.md AND drop a marker file so
        // there's something to commit.
        Script::default()
            .write(".pitboss/play/deferred.md", post_sweep_deferred.as_bytes())
            .write("src/sweep_marker.rs", b"// sweep\n"),
        // Phase 02 implementer.
        Script::default().write("src/phase_02.rs", b"// phase 2\n"),
    ]);

    let mut runner =
        build_runner(dir.path(), TWO_PHASE_PLAN, &initial, audit_disabled(), agent).await;

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        loop {
            match rx.recv().await {
                Ok(Event::PhaseStarted { phase_id, .. }) => {
                    events.push(format!("PhaseStarted({phase_id})"))
                }
                Ok(Event::PhaseCommitted { phase_id, .. }) => {
                    events.push(format!("PhaseCommitted({phase_id})"))
                }
                Ok(Event::SweepStarted {
                    after,
                    items_pending,
                    ..
                }) => events.push(format!("SweepStarted({after},{items_pending})")),
                Ok(Event::SweepCompleted {
                    after, resolved, ..
                }) => events.push(format!("SweepCompleted({after},{resolved})")),
                Ok(Event::SweepHalted { after, .. }) => {
                    events.push(format!("SweepHalted({after})"))
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

    // Sweep fired exactly once, between phase 01 commit and phase 02 start.
    let phase01_committed = events
        .iter()
        .position(|e| e == "PhaseCommitted(01)")
        .expect("phase 01 must commit");
    let sweep_started = events
        .iter()
        .position(|e| e.starts_with("SweepStarted("))
        .expect("sweep must fire");
    let phase02_started = events
        .iter()
        .position(|e| e == "PhaseStarted(02)")
        .expect("phase 02 must start");
    assert!(phase01_committed < sweep_started, "events: {events:?}");
    assert!(sweep_started < phase02_started, "events: {events:?}");
    assert_eq!(
        events
            .iter()
            .filter(|e| e.starts_with("SweepStarted("))
            .count(),
        1,
        "exactly one SweepStarted expected: {events:?}"
    );
    assert!(events.contains(&"SweepStarted(01,6)".to_string()));
    assert!(events.contains(&"SweepCompleted(01,4)".to_string()));

    // State: pending_sweep cleared; completed contains only real phases.
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(!state.pending_sweep);
    assert_eq!(
        state
            .completed
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
        vec!["01", "02"]
    );
    // After phase 02 commits, consecutive_sweeps re-arms back to zero.
    assert_eq!(state.consecutive_sweeps, 0);

    // Deferred file has the 2 surviving unchecked items (the post-sweep
    // `sweep()` call drops the four ticked items).
    let deferred_after = fs::read_to_string(dir.path().join(".pitboss/play/deferred.md")).unwrap();
    let parsed = pitboss::deferred::parse(&deferred_after).unwrap();
    assert_eq!(
        parsed.items.iter().filter(|i| !i.done).count(),
        2,
        "deferred:\n{deferred_after}"
    );
    assert_eq!(parsed.items.iter().filter(|i| i.done).count(), 0);

    // Sweep commit message uses the canonical format.
    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().any(|l| l.contains(
            "[pitboss] sweep after phase 01: 4 deferred items resolved"
        )),
        "sweep commit not found in log:\n{log:?}"
    );

    // Sweep log file lands at the expected per-attempt path.
    assert!(
        dir.path()
            .join(".pitboss/play/logs/sweep-after-01-implementer-2.log")
            .exists(),
        "sweep dispatch log must be written under the sweep- prefix"
    );
}

/// `[sweep] enabled = false` short-circuits the trip-wire even with 8 items
/// pending. No sweep dispatch fires; the runner advances straight to phase 02.
#[tokio::test]
async fn disabled_sweep_never_fires() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
        ("f", false),
        ("g", false),
        ("h", false),
    ]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let mut config = audit_disabled();
    config.sweep = SweepConfig {
        enabled: false,
        ..SweepConfig::default()
    };

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        Script::default().write("src/phase_02.rs", b"// 2\n"),
    ]);

    let mut runner = build_runner(dir.path(), TWO_PHASE_PLAN, &initial, config, agent).await;
    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    let log = git_log_oneline(dir.path());
    let phase_commits: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("[pitboss] phase"))
        .collect();
    assert_eq!(phase_commits.len(), 2);
    assert!(
        log.iter().all(|l| !l.contains("sweep after phase")),
        "no sweep commit expected when disabled; log:\n{log:?}"
    );

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(!state.pending_sweep);
    assert_eq!(state.consecutive_sweeps, 0);
}

/// `max_consecutive = 1` (the default) blocks the sweep gate when the runner
/// has already chained one back-to-back. The gate clears `pending_sweep` and
/// the regular phase runs; the next phase commit re-arms the counter.
#[tokio::test]
async fn consecutive_clamp_blocks_back_to_back_sweep() {
    use chrono::Utc;
    use pitboss::state;

    let initial = deferred_items_only(&[
        ("x1", false),
        ("x2", false),
        ("x3", false),
        ("x4", false),
        ("x5", false),
        ("x6", false),
    ]);
    // Single-phase plan would land on the no-final-trigger path; use the
    // two-phase plan so we can synthesize a "between phase 01 and 02" boundary
    // already past phase 01.
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let config = audit_disabled();
    assert_eq!(config.sweep.max_consecutive, 1);

    // Synthesize: phase 01 already committed; pending_sweep set; one
    // consecutive sweep already chained. The gate should refuse to fire a
    // second sweep and fall through to phase 02.
    let plan_obj = plan::parse(TWO_PHASE_PLAN).unwrap();
    let mut state_obj = runner::fresh_run_state(&plan_obj, &config, Utc::now());
    state_obj.completed = vec![pid("01")];
    state_obj.pending_sweep = true;
    state_obj.consecutive_sweeps = 1;
    state_obj.attempts.insert(pid("01"), 1);
    // current_phase is already advanced to 02 (the post-phase-01 boundary).
    let mut plan_advanced = plan_obj.clone();
    plan_advanced.set_current_phase(pid("02"));
    fs::write(
        dir.path().join(".pitboss/play/plan.md"),
        plan::serialize(&plan_advanced),
    )
    .unwrap();
    state::save(dir.path(), Some(&state_obj)).unwrap();

    // Only phase 02's implementer should dispatch — no sweep.
    let agent =
        ScriptedAgent::new(vec![Script::default().write("src/phase_02.rs", b"// 2\n")]);

    let git = ShellGit::new(dir.path());
    git.create_branch(&state_obj.branch).await.unwrap();
    git.checkout(&state_obj.branch).await.unwrap();
    let runner_git = ShellGit::new(dir.path());
    let mut runner = Runner::new(
        dir.path().to_path_buf(),
        config,
        plan_advanced,
        pitboss::deferred::parse(&initial).unwrap(),
        state_obj,
        agent,
        runner_git,
    );

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("sweep after phase")),
        "sweep commit must not land when the clamp is hit; log:\n{log:?}"
    );

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    // Counter re-armed to 0 after phase 02 advanced.
    assert_eq!(state.consecutive_sweeps, 0);
    assert!(!state.pending_sweep);
}

/// A sweep that halts mid-dispatch leaves `pending_sweep = true`. Calling
/// `run_phase` again retries the sweep before any phase 02 work runs.
#[tokio::test]
async fn sweep_halt_persists_pending_sweep_for_resume() {
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
        // Phase 01 implementer.
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        // Sweep impl: explode.
        Script {
            stop_reason: Some(StopReason::Error("synthetic sweep failure".into())),
            exit_code: Some(2),
            ..Script::default()
        },
        // Sweep impl retry: succeed.
        Script::default()
            .write(".pitboss/play/deferred.md", post_sweep.as_bytes())
            .write("src/sweep_done.rs", b"// retry success\n"),
        // Phase 02 implementer.
        Script::default().write("src/phase_02.rs", b"// 2\n"),
    ]);

    let mut runner =
        build_runner(dir.path(), TWO_PHASE_PLAN, &initial, audit_disabled(), agent).await;

    // Phase 01 → sweep halts. Drive run_phase manually so we can inspect
    // intermediate state.
    let _r1 = runner.run_phase().await.unwrap(); // phase 01 advances
    let r2 = runner.run_phase().await.unwrap(); // sweep halts
    match r2 {
        PhaseResult::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert!(
                matches!(reason, HaltReason::AgentFailure(_)),
                "got {reason:?}"
            );
        }
        other => panic!("expected sweep halt, got {other:?}"),
    }
    let mid_state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(
        mid_state.pending_sweep,
        "pending_sweep must survive a sweep halt"
    );
    // No sweep commit landed yet.
    let mid_log = git_log_oneline(dir.path());
    assert!(mid_log.iter().all(|l| !l.contains("sweep after phase")));

    // Resume: next run_phase retries the sweep.
    let r3 = runner.run_phase().await.unwrap();
    assert!(
        matches!(r3, PhaseResult::Advanced { .. }),
        "sweep retry should advance, got {r3:?}"
    );
    // Then phase 02 runs.
    let r4 = runner.run_phase().await.unwrap();
    match r4 {
        PhaseResult::Advanced { phase_id, .. } => assert_eq!(phase_id.as_str(), "02"),
        other => panic!("expected phase 02 advance, got {other:?}"),
    }

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(!state.pending_sweep);
    let log = git_log_oneline(dir.path());
    assert!(log
        .iter()
        .any(|l| l.contains("sweep after phase 01: 2 deferred items resolved")));
}

/// A user clearing `deferred.md` between resumes — pending_sweep is still set
/// in `state.json`, but the on-disk file no longer satisfies the trigger.
/// The gate re-evaluates against the on-disk state, clears `pending_sweep`,
/// and runs the regular phase.
#[tokio::test]
async fn manual_deferred_cleanup_clears_pending_sweep() {
    let dir = make_workspace(TWO_PHASE_PLAN, &deferred_items_only(&[]));
    init_git_repo(dir.path());

    let config = audit_disabled();

    // Synthesize state mid-stream: phase 01 completed; pending_sweep=true.
    let plan_obj = plan::parse(TWO_PHASE_PLAN).unwrap();
    let mut state_obj = runner::fresh_run_state(&plan_obj, &config, Utc::now());
    state_obj.completed = vec![pid("01")];
    state_obj.pending_sweep = true;
    state_obj.consecutive_sweeps = 0;
    state_obj.attempts.insert(pid("01"), 1);
    let mut plan_advanced = plan_obj.clone();
    plan_advanced.set_current_phase(pid("02"));
    fs::write(
        dir.path().join(".pitboss/play/plan.md"),
        plan::serialize(&plan_advanced),
    )
    .unwrap();
    pitboss::state::save(dir.path(), Some(&state_obj)).unwrap();
    // The on-disk deferred has no unchecked items — the user cleared them by
    // hand between sessions.
    fs::write(
        dir.path().join(".pitboss/play/deferred.md"),
        deferred_items_only(&[("note", true)]).as_bytes(),
    )
    .unwrap();

    let agent =
        ScriptedAgent::new(vec![Script::default().write("src/phase_02.rs", b"// 2\n")]);

    let git = ShellGit::new(dir.path());
    git.create_branch(&state_obj.branch).await.unwrap();
    git.checkout(&state_obj.branch).await.unwrap();
    let runner_git = ShellGit::new(dir.path());
    let mut runner = Runner::new(
        dir.path().to_path_buf(),
        config,
        plan_advanced,
        DeferredDoc::empty(),
        state_obj,
        agent,
        runner_git,
    );

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("sweep after phase")),
        "no sweep commit when the trigger no longer fires; log:\n{log:?}"
    );
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(!state.pending_sweep);
}

/// A single-phase plan that leaves 5 unchecked items finishes with
/// `next_phase = None` and `pending_sweep = false` — the inter-phase gate
/// must be guarded against the no-next-phase branch (phase 08 owns the
/// end-of-run drain).
#[tokio::test]
async fn final_phase_does_not_set_pending_sweep() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent =
        ScriptedAgent::new(vec![Script::default().write("src/phase_01.rs", b"// 1\n")]);
    let mut runner =
        build_runner(dir.path(), ONE_PHASE_PLAN, &initial, audit_disabled(), agent).await;

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(
        !state.pending_sweep,
        "no pending_sweep without a next phase; state: {state:?}"
    );
    assert_eq!(state.consecutive_sweeps, 0);

    let log = git_log_oneline(dir.path());
    assert!(log.iter().all(|l| !l.contains("sweep after phase")));
}

/// A sweep that produced no code changes still records a `consecutive_sweeps`
/// increment and clears `pending_sweep`, but lands no commit. The next phase
/// runs normally.
#[tokio::test]
async fn empty_sweep_skips_commit_but_clears_pending() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        // Sweep impl: write nothing. Deferred file untouched, no code changes.
        Script::default(),
        Script::default().write("src/phase_02.rs", b"// 2\n"),
    ]);

    let mut runner =
        build_runner(dir.path(), TWO_PHASE_PLAN, &initial, audit_disabled(), agent).await;
    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));

    let log = git_log_oneline(dir.path());
    let sweep_commits: Vec<&String> =
        log.iter().filter(|l| l.contains("sweep after phase")).collect();
    assert!(
        sweep_commits.is_empty(),
        "empty sweep must not land a commit; log:\n{log:?}"
    );
    let phase_commits: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("[pitboss] phase"))
        .collect();
    assert_eq!(
        phase_commits.len(),
        2,
        "both phase commits expected; log:\n{log:?}"
    );

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(!state.pending_sweep);
    // Re-armed to zero after phase 02 commit.
    assert_eq!(state.consecutive_sweeps, 0);
}

/// A sweep that drives 2 fixer attempts increments `state.attempts[after]` by
/// 3 (1 implementer + 2 fixer), proving the fixer loop's bookkeeping flows
/// through the shared `state.attempts` counter rather than a local-only one.
#[tokio::test]
async fn sweep_fixer_attempts_share_state_attempts_counter() {
    const INITIAL_TEST_SCRIPT: &str = "#!/bin/sh\ntest -f .pass-marker\n";

    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());
    fs::write(dir.path().join(".test.sh"), INITIAL_TEST_SCRIPT).unwrap();
    // Pre-seed the .pass-marker so phase 01 tests pass.
    fs::write(dir.path().join(".pass-marker"), b"").unwrap();

    let mut config = audit_disabled();
    config.tests.command = Some("/bin/sh ./.test.sh".to_string());
    config.retries.fixer_max_attempts = 2;

    let post_sweep = deferred_items_only(&[
        ("a", true),
        ("b", true),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);

    let agent = ScriptedAgent::new(vec![
        // Phase 01 impl: writes code; marker already on disk so tests pass.
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        // Sweep impl: ticks 2 items off and swaps the test script to one that
        // looks for `.fixer2-marker` (which does not yet exist) → tests fail.
        Script::default()
            .write(".pitboss/play/deferred.md", post_sweep.as_bytes())
            .write("src/sweep_marker.rs", b"// sweep\n")
            .write(".test.sh", "#!/bin/sh\ntest -f .fixer2-marker\n"),
        // Fixer 1: still no marker → tests still fail.
        Script::default().write("src/sweep_fixer_1.rs", b"// fixer 1\n"),
        // Fixer 2: drops the marker → tests pass.
        Script::default().write(".fixer2-marker", b""),
        // Phase 02 impl: tests still pass (marker present).
        Script::default().write("src/phase_02.rs", b"// 2\n"),
    ]);

    let mut runner =
        build_runner(dir.path(), TWO_PHASE_PLAN, &initial, config, agent).await;
    let summary = runner.run().await.unwrap();
    assert!(
        matches!(summary, RunSummary::Finished),
        "expected finish, got {summary:?}"
    );

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    // Phase 01 dispatched once; sweep dispatched 1 impl + 2 fixer; phase 02
    // dispatched once. So state.attempts[01] = 1 + 3 = 4; state.attempts[02] = 1.
    assert_eq!(
        state.attempts.get(&pid("01")).copied(),
        Some(4),
        "state.attempts: {:?}",
        state.attempts
    );
    assert_eq!(state.attempts.get(&pid("02")).copied(), Some(1));

    // The sweep's per-fixer log files land under the sweep prefix.
    let logs = dir.path().join(".pitboss/play/logs");
    assert!(
        logs.join("sweep-after-01-implementer-2.log").exists(),
        "sweep impl log missing"
    );
    assert!(
        logs.join("phase-01-fix-1.log").exists(),
        "fixer1 log missing"
    );
    assert!(
        logs.join("phase-01-fix-2.log").exists(),
        "fixer2 log missing"
    );
}

/// A sweep that edits the `## Deferred phases` block trips the
/// "sweep modified Deferred phases" guard, halts with `DeferredInvalid`, and
/// rolls deferred.md back to its pre-dispatch bytes.
#[tokio::test]
async fn sweep_modifying_deferred_phases_halts_with_rollback() {
    let phases_body = "### From phase 07: rework agent trait\n\noriginal body line\n";
    let initial = deferred_with_phases(
        &[
            ("a", false),
            ("b", false),
            ("c", false),
            ("d", false),
            ("e", false),
        ],
        phases_body,
    );
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let pre_bytes = fs::read(dir.path().join(".pitboss/play/deferred.md")).unwrap();

    // Sweep agent edits the phases block, which is forbidden.
    let tampered_phases = "### From phase 07: rework agent trait\n\nTAMPERED BODY LINE\n";
    let tampered = deferred_with_phases(
        &[
            ("a", true),
            ("b", true),
            ("c", false),
            ("d", false),
            ("e", false),
        ],
        tampered_phases,
    );

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        // Sweep impl: writes tampered deferred.md.
        Script::default()
            .write(".pitboss/play/deferred.md", tampered.as_bytes())
            .write("src/sweep.rs", b"// sweep\n"),
    ]);

    let mut runner =
        build_runner(dir.path(), TWO_PHASE_PLAN, &initial, audit_disabled(), agent).await;
    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            match reason {
                HaltReason::DeferredInvalid(msg) => {
                    assert!(
                        msg.contains("sweep modified Deferred phases"),
                        "unexpected reason text: {msg}"
                    );
                }
                other => panic!("expected DeferredInvalid, got {other:?}"),
            }
        }
        other => panic!("expected halt, got {other:?}"),
    }

    // deferred.md restored byte-for-byte from the pre-dispatch snapshot.
    let after = fs::read(dir.path().join(".pitboss/play/deferred.md")).unwrap();
    assert_eq!(
        after, pre_bytes,
        "deferred.md must roll back to pre-sweep bytes when H3 invariant violated"
    );

    // Sweep commit did NOT land.
    let log = git_log_oneline(dir.path());
    assert!(
        log.iter().all(|l| !l.contains("sweep after phase")),
        "no sweep commit expected on H3 violation; log:\n{log:?}"
    );

    // Pending_sweep stays true so a resume can retry.
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(state.pending_sweep);
}
