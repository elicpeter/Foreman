//! Integration tests for phase 08 — the bounded final-sweep drain loop.
//!
//! After the final regular phase commits, [`Runner::run`] keeps invoking the
//! phase 03 sweep step until the backlog drains, the agent stops making
//! progress, the iteration cap is hit, or the dispatch halts. Each test
//! stands a workspace up against a real `git init`'d directory and drives the
//! runner with a [`ScriptedAgent`] (the same shape sweep_smoke.rs and
//! sweep_staleness.rs use).

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
use pitboss::runner::{self, Event, HaltReason, RunSummary, Runner};

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

const ONE_PHASE_PLAN: &str = "\
---
current_phase: \"01\"
---

# Pitboss Plan

# Phase 01: Single

**Scope.** Only phase.
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

/// Default config tweaks for this file: phase / sweep auditors off, regular
/// between-phase trigger raised so it cannot fire on the single-phase plans
/// used here. The final-sweep loop still defaults to enabled with
/// `final_sweep_max_iterations = 3`.
fn final_loop_config() -> Config {
    let mut c = Config::default();
    c.audit.enabled = false;
    c.sweep.audit_enabled = false;
    // Single-phase plans never see a between-phase trigger anyway, but keep
    // the threshold high so future test additions can't trip it accidentally.
    c.sweep.trigger_min_items = 100;
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

/// Stamp tag for events: the same compact `Started/Completed/Halted/RunFinished`
/// format used in `sweep_smoke.rs`, plus `PhaseHalted` for halt-path tests.
async fn collect_events(mut rx: tokio::sync::broadcast::Receiver<Event>) -> Vec<String> {
    use tokio::sync::broadcast::error::RecvError;
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
            Ok(Event::SweepHalted { after, .. }) => events.push(format!("SweepHalted({after})")),
            Ok(Event::PhaseHalted { phase_id, .. }) => {
                events.push(format!("PhaseHalted({phase_id})"));
                break;
            }
            Ok(Event::RunFinished) => {
                events.push("RunFinished".to_string());
                break;
            }
            Ok(Event::DeferredItemStale { text, .. }) => {
                events.push(format!("DeferredItemStale({text})"))
            }
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
    events
}

/// Single-phase plan, 4 unchecked items; iteration 1 marks all four off and
/// the loop exits at the top of iteration 2 on the `pre_unchecked == 0`
/// short-circuit. Exactly one `SweepStarted` / `SweepCompleted` pair lands
/// after `PhaseCommitted(01)` and before `RunFinished`.
#[tokio::test]
async fn drain_to_zero() {
    let initial = deferred_items_only(&[
        ("polish error message", false),
        ("drop unused stub", false),
        ("rename flag", false),
        ("tighten test fixture", false),
    ]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let drained = deferred_items_only(&[
        ("polish error message", true),
        ("drop unused stub", true),
        ("rename flag", true),
        ("tighten test fixture", true),
    ]);

    let agent = ScriptedAgent::new(vec![
        // Phase 01.
        Script::default().write("src/phase_01.rs", b"// phase 1\n"),
        // Final-sweep iter 1: ticks all four off + drops a marker file.
        Script::default()
            .write(".pitboss/play/deferred.md", drained.as_bytes())
            .write("src/sweep_marker.rs", b"// sweep\n"),
        // No iter 2 — pre_unchecked == 0 short-circuits.
    ]);

    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        final_loop_config(),
        agent,
    )
    .await;
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let events = collector.await.unwrap();
    let phase01 = events
        .iter()
        .position(|e| e == "PhaseCommitted(01)")
        .expect("phase 01 must commit");
    let sweep_started = events
        .iter()
        .position(|e| e.starts_with("SweepStarted("))
        .expect("final-sweep must fire");
    let sweep_completed = events
        .iter()
        .position(|e| e.starts_with("SweepCompleted("))
        .expect("final-sweep must complete");
    let finished = events
        .iter()
        .position(|e| e == "RunFinished")
        .expect("RunFinished must fire");
    assert!(phase01 < sweep_started);
    assert!(sweep_started < sweep_completed);
    assert!(sweep_completed < finished);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.starts_with("SweepStarted("))
            .count(),
        1,
        "expected exactly one final-sweep iter; events: {events:?}"
    );
    assert!(events.contains(&"SweepStarted(01,4)".to_string()));
    assert!(events.contains(&"SweepCompleted(01,4)".to_string()));

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(!state.pending_sweep);
    assert!(
        state.deferred_item_attempts.is_empty(),
        "drained items must not carry stale-counter entries; map: {:?}",
        state.deferred_item_attempts
    );
    let log = git_log_oneline(dir.path());
    assert!(log
        .iter()
        .any(|l| l.contains("sweep after phase 01: 4 deferred items resolved")));
}

/// 9 items remaining after phase 01; iter 1 resolves 4, iter 2 resolves 3,
/// iter 3 resolves 0 → loop exits via the no-progress guard. `RunFinished`
/// fires with 2 unchecked items remaining.
#[tokio::test]
async fn multi_iteration_progress() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
        ("f", false),
        ("g", false),
        ("h", false),
        ("i", false),
    ]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    // Iter 1: agent marks a, b, c, d done. After post-dispatch sweep, 5
    // unchecked remain on disk: e, f, g, h, i.
    let after_iter_1 = deferred_items_only(&[
        ("a", true),
        ("b", true),
        ("c", true),
        ("d", true),
        ("e", false),
        ("f", false),
        ("g", false),
        ("h", false),
        ("i", false),
    ]);
    // Iter 2: starts with e..i (5 unchecked). Agent ticks e, f, g done →
    // post-sweep on disk has h, i unchecked.
    let after_iter_2 = deferred_items_only(&[
        ("e", true),
        ("f", true),
        ("g", true),
        ("h", false),
        ("i", false),
    ]);

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        Script::default()
            .write(".pitboss/play/deferred.md", after_iter_1.as_bytes())
            .write("src/sweep_1.rs", b"// 1\n"),
        Script::default()
            .write(".pitboss/play/deferred.md", after_iter_2.as_bytes())
            .write("src/sweep_2.rs", b"// 2\n"),
        // Iter 3: empty script — agent makes no edits; resolved == 0; loop
        // exits via no-progress guard.
        Script::default(),
    ]);

    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        final_loop_config(),
        agent,
    )
    .await;
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let events = collector.await.unwrap();
    let starts: Vec<&String> = events
        .iter()
        .filter(|e| e.starts_with("SweepStarted("))
        .collect();
    assert_eq!(
        starts.len(),
        3,
        "expected 3 sweep iters; events: {events:?}"
    );
    assert_eq!(starts[0], "SweepStarted(01,9)");
    assert_eq!(starts[1], "SweepStarted(01,5)");
    assert_eq!(starts[2], "SweepStarted(01,2)");

    let completes: Vec<&String> = events
        .iter()
        .filter(|e| e.starts_with("SweepCompleted("))
        .collect();
    assert_eq!(completes.len(), 3);
    assert_eq!(completes[0], "SweepCompleted(01,4)");
    assert_eq!(completes[1], "SweepCompleted(01,3)");
    assert_eq!(completes[2], "SweepCompleted(01,0)");
    assert!(events.contains(&"RunFinished".to_string()));

    // Two items survive on disk.
    let deferred_after = fs::read_to_string(dir.path().join(".pitboss/play/deferred.md")).unwrap();
    let parsed = pitboss::deferred::parse(&deferred_after).unwrap();
    let unchecked: Vec<&str> = parsed
        .items
        .iter()
        .filter(|i| !i.done)
        .map(|i| i.text.as_str())
        .collect();
    assert_eq!(unchecked, vec!["h", "i"]);

    // Both survivors carry phase-05 attempts (they were in pre_texts of
    // every iter that observed them).
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(state.deferred_item_attempts.get("h").copied(), Some(3));
    assert_eq!(state.deferred_item_attempts.get("i").copied(), Some(3));
    assert!(!state.pending_sweep);
}

/// `final_sweep_max_iterations = 2`; iter 1 resolves 2, iter 2 resolves 2,
/// loop exits because the cap was hit (each iter made progress). 6 items
/// survive.
#[tokio::test]
async fn cap_hit() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
        ("f", false),
        ("g", false),
        ("h", false),
        ("i", false),
        ("j", false),
    ]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let after_iter_1 = deferred_items_only(&[
        ("a", true),
        ("b", true),
        ("c", false),
        ("d", false),
        ("e", false),
        ("f", false),
        ("g", false),
        ("h", false),
        ("i", false),
        ("j", false),
    ]);
    let after_iter_2 = deferred_items_only(&[
        ("c", true),
        ("d", true),
        ("e", false),
        ("f", false),
        ("g", false),
        ("h", false),
        ("i", false),
        ("j", false),
    ]);

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        Script::default()
            .write(".pitboss/play/deferred.md", after_iter_1.as_bytes())
            .write("src/sweep_1.rs", b"// 1\n"),
        Script::default()
            .write(".pitboss/play/deferred.md", after_iter_2.as_bytes())
            .write("src/sweep_2.rs", b"// 2\n"),
        // No third iter: cap = 2.
    ]);

    let mut config = final_loop_config();
    config.sweep.final_sweep_max_iterations = 2;
    let mut runner = build_runner(dir.path(), ONE_PHASE_PLAN, &initial, config, agent).await;
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let events = collector.await.unwrap();
    let starts: Vec<&String> = events
        .iter()
        .filter(|e| e.starts_with("SweepStarted("))
        .collect();
    assert_eq!(starts.len(), 2, "cap = 2 must clamp; events: {events:?}");
    assert_eq!(starts[0], "SweepStarted(01,10)");
    assert_eq!(starts[1], "SweepStarted(01,8)");

    let deferred_after = fs::read_to_string(dir.path().join(".pitboss/play/deferred.md")).unwrap();
    let parsed = pitboss::deferred::parse(&deferred_after).unwrap();
    let unchecked: Vec<&str> = parsed
        .items
        .iter()
        .filter(|i| !i.done)
        .map(|i| i.text.as_str())
        .collect();
    assert_eq!(unchecked, vec!["e", "f", "g", "h", "i", "j"]);
    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(!state.pending_sweep);
}

/// Iter 2's implementer fails. The loop bails via `RunSummary::Halted`,
/// `state.pending_sweep` stays true, and a follow-up `Runner::run` resumes
/// the loop and drives to `RunFinished`.
#[tokio::test]
async fn halt_mid_loop_then_resume_drains() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
        ("f", false),
    ]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let after_iter_1 = deferred_items_only(&[
        ("a", true),
        ("b", true),
        ("c", false),
        ("d", false),
        ("e", false),
        ("f", false),
    ]);

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        // Iter 1 succeeds.
        Script::default()
            .write(".pitboss/play/deferred.md", after_iter_1.as_bytes())
            .write("src/sweep_1.rs", b"// 1\n"),
        // Iter 2 explodes.
        Script {
            stop_reason: Some(StopReason::Error("synthetic sweep failure".into())),
            exit_code: Some(2),
            ..Script::default()
        },
    ]);

    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        final_loop_config(),
        agent,
    )
    .await;
    let summary = runner.run().await.unwrap();
    match summary {
        RunSummary::Halted { phase_id, reason } => {
            assert_eq!(phase_id.as_str(), "01");
            assert!(
                matches!(reason, HaltReason::AgentFailure(_)),
                "got {reason:?}"
            );
        }
        other => panic!("expected halt, got {other:?}"),
    }
    drop(runner);

    let mid_state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(
        mid_state.pending_sweep,
        "pending_sweep must survive a final-sweep halt"
    );
    // Iter 1's commit landed; iter 2 didn't.
    let mid_log = git_log_oneline(dir.path());
    assert_eq!(
        mid_log
            .iter()
            .filter(|l| l.contains("sweep after phase 01"))
            .count(),
        1,
        "exactly one sweep commit before halt; log:\n{mid_log:?}"
    );

    // Resume: rebuild runner from on-disk state with a fresh script queue
    // that drains the remaining 4 items in one go.
    let resume_state = pitboss::state::load(dir.path()).unwrap().expect("state");
    let plan_obj = plan::parse(ONE_PHASE_PLAN).expect("parse plan");
    let deferred_text = fs::read_to_string(dir.path().join(".pitboss/play/deferred.md")).unwrap();
    let deferred = pitboss::deferred::parse(&deferred_text).unwrap();
    let drained = deferred_items_only(&[("c", true), ("d", true), ("e", true), ("f", true)]);
    let agent = ScriptedAgent::new(vec![Script::default()
        .write(".pitboss/play/deferred.md", drained.as_bytes())
        .write("src/sweep_resume.rs", b"// resume\n")]);
    let runner_git = ShellGit::new(dir.path());
    let mut runner = Runner::new(
        dir.path().to_path_buf(),
        final_loop_config(),
        plan_obj,
        deferred,
        resume_state,
        agent,
        runner_git,
    );
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));
    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let events = collector.await.unwrap();
    assert!(events.contains(&"SweepStarted(01,4)".to_string()));
    assert!(events.contains(&"SweepCompleted(01,4)".to_string()));
    assert!(events.contains(&"RunFinished".to_string()));
    let final_state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(!final_state.pending_sweep);
}

/// `[sweep] final_sweep_enabled = false` with a 6-item backlog: `RunFinished`
/// fires immediately, no sweep events between phase 01 and the terminal
/// event.
#[tokio::test]
async fn disabled_config_skips_loop() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
        ("f", false),
    ]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![Script::default().write("src/phase_01.rs", b"// 1\n")]);
    let mut config = final_loop_config();
    config.sweep.final_sweep_enabled = false;
    let mut runner = build_runner(dir.path(), ONE_PHASE_PLAN, &initial, config, agent).await;
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let events = collector.await.unwrap();
    assert!(
        !events.iter().any(|e| e.starts_with("SweepStarted(")),
        "no sweep should fire when final_sweep_enabled = false; events: {events:?}"
    );
}

/// `[sweep] enabled = false` (master switch) dominates `final_sweep_enabled =
/// true`; the loop also doesn't run.
#[tokio::test]
async fn master_switch_off_dominates_final_sweep() {
    let initial = deferred_items_only(&[("a", false), ("b", false), ("c", false)]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![Script::default().write("src/phase_01.rs", b"// 1\n")]);
    let mut config = final_loop_config();
    config.sweep.enabled = false;
    assert!(config.sweep.final_sweep_enabled, "default stays on");
    let mut runner = build_runner(dir.path(), ONE_PHASE_PLAN, &initial, config, agent).await;
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let events = collector.await.unwrap();
    assert!(
        !events.iter().any(|e| e.starts_with("SweepStarted(")),
        "master switch off must dominate; events: {events:?}"
    );
}

/// Phase 01 leaves 0 unchecked items; the loop method short-circuits at the
/// top of iter 1 (or rather, the gate skips it entirely). `RunFinished` fires
/// without a sweep event.
#[tokio::test]
async fn already_empty_skips_loop() {
    let initial = deferred_items_only(&[("only", true)]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![Script::default().write("src/phase_01.rs", b"// 1\n")]);
    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        final_loop_config(),
        agent,
    )
    .await;
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let events = collector.await.unwrap();
    assert!(
        !events.iter().any(|e| e.starts_with("SweepStarted(")),
        "no sweep needed for an already-drained backlog; events: {events:?}"
    );
}

/// A single iteration that resolves no items: the no-progress exit fires
/// after iter 1, but `apply_sweep_staleness` still tracks the survivors so
/// the staleness clock continues across runs.
#[tokio::test]
async fn no_progress_increments_staleness_then_exits() {
    let initial = deferred_items_only(&[("alpha", false), ("beta", false)]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        // Iter 1: no edits. resolved = 0 → loop exits.
        Script::default(),
    ]);
    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        final_loop_config(),
        agent,
    )
    .await;
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let events = collector.await.unwrap();
    let starts = events
        .iter()
        .filter(|e| e.starts_with("SweepStarted("))
        .count();
    assert_eq!(
        starts, 1,
        "no-progress exit fires after one iter; events: {events:?}"
    );

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(state.deferred_item_attempts.get("alpha").copied(), Some(1));
    assert_eq!(state.deferred_item_attempts.get("beta").copied(), Some(1));
    assert!(!state.pending_sweep);
}

/// `Runner::skip_sweep(true)` (the runtime mirror of `pitboss play
/// --no-sweep`) suppresses the final-sweep loop even with a 6-item backlog.
#[tokio::test]
async fn no_sweep_override_suppresses_final_loop() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
        ("f", false),
    ]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![Script::default().write("src/phase_01.rs", b"// 1\n")]);
    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        final_loop_config(),
        agent,
    )
    .await
    .skip_sweep(true);
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let events = collector.await.unwrap();
    assert!(
        !events.iter().any(|e| e.starts_with("SweepStarted(")),
        "--no-sweep must suppress the trailing drain; events: {events:?}"
    );
}

/// Phase 01's implementer halts (sweep-style failure mid-implementer): the
/// run halts before reaching `Advanced { next_phase: None }`, so the
/// final-sweep loop never runs. Regression guard for the dispatch ordering.
#[tokio::test]
async fn halted_final_phase_skips_final_loop() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        // Phase 01's implementer explodes.
        Script {
            stop_reason: Some(StopReason::Error("synthetic phase failure".into())),
            exit_code: Some(2),
            ..Script::default()
        },
    ]);
    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        final_loop_config(),
        agent,
    )
    .await;
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));

    let summary = runner.run().await.unwrap();
    assert!(
        matches!(summary, RunSummary::Halted { ref phase_id, .. } if phase_id == &pid("01")),
        "expected halt at phase 01, got {summary:?}"
    );
    drop(runner);

    let events = collector.await.unwrap();
    assert!(
        !events.iter().any(|e| e.starts_with("SweepStarted(")),
        "final-sweep loop must not fire when the final phase halts; events: {events:?}"
    );
    assert!(events.iter().any(|e| e == "PhaseHalted(01)"));
}

/// Hardening regression: the final-phase resume guard must fire on the
/// explicit `state.post_final_phase` flag, not just on the inferred
/// invariant `completed.last() == current_phase &&
/// next_phase_id_after(...).is_none()`. We simulate a future runner
/// change that advances `current_phase` past the final phase (today the
/// runner doesn't, but the inference would silently break if it ever
/// did) and assert resume still routes into the final-sweep loop and
/// doesn't replay the final phase.
#[tokio::test]
async fn final_phase_commit_sets_post_final_phase_flag_and_resume_uses_it() {
    let initial = deferred_items_only(&[("a", false)]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let drained = deferred_items_only(&[("a", true)]);
    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        Script::default()
            .write(".pitboss/play/deferred.md", drained.as_bytes())
            .write("src/sweep_marker.rs", b"// sweep\n"),
    ]);
    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        final_loop_config(),
        agent,
    )
    .await;
    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(
        state.post_final_phase,
        "final-phase commit must persist post_final_phase = true; state: {state:?}"
    );

    // Corrupt the inferred invariant: drop the final phase from completed.
    // The explicit flag should still drive the resume guard. Without the
    // flag, a future change to that invariant would silently re-run the
    // final phase here.
    let mut tampered = state.clone();
    tampered.completed.clear();
    pitboss::state::save(dir.path(), Some(&tampered)).unwrap();

    let plan_obj = plan::parse(ONE_PHASE_PLAN).expect("parse plan");
    let deferred_text = fs::read_to_string(dir.path().join(".pitboss/play/deferred.md")).unwrap();
    let deferred = pitboss::deferred::parse(&deferred_text).unwrap();
    let resume_state = pitboss::state::load(dir.path()).unwrap().expect("state");
    // Resume must NOT re-dispatch the final phase. With the deferred file
    // already drained (zero unchecked items), the final-sweep loop's
    // `pre_unchecked == 0` short-circuit exits immediately and emits
    // `RunFinished`.
    let resume_agent = ScriptedAgent::new(vec![]);
    let runner_git = ShellGit::new(dir.path());
    let mut runner = Runner::new(
        dir.path().to_path_buf(),
        final_loop_config(),
        plan_obj,
        deferred,
        resume_state,
        resume_agent,
        runner_git,
    );
    let rx = runner.subscribe();
    let collector = tokio::spawn(collect_events(rx));
    let summary = runner.run().await.unwrap();
    assert!(matches!(summary, RunSummary::Finished));
    drop(runner);

    let events = collector.await.unwrap();
    assert!(
        !events.iter().any(|e| e.starts_with("PhaseStarted(")),
        "resume with post_final_phase = true must not re-dispatch any phase; events: {events:?}",
    );
    assert!(
        events.contains(&"RunFinished".to_string()),
        "resume must finish cleanly; events: {events:?}",
    );
}
