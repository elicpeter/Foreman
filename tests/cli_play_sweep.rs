//! Integration tests for phase 06 — `pitboss play --no-sweep` /
//! `--sweep` overrides, the `Runner::run_standalone_sweep` API, and the
//! `pitboss play --no-sweep --sweep` clap mutual-exclusion guard.
//!
//! The programmatic tests reuse the scripted-agent harness from the phase
//! 03 sweep tests so they stay close to that file's idioms; the
//! `assert_cmd` block tests only the bits that have to be exercised through
//! the binary (clap arg parsing).

#![cfg(unix)]

mod common;

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Mutex;

use anyhow::Result;
use assert_cmd::Command as AssertCommand;
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
use pitboss::runner::{self, Event, PhaseResult, RunSummary, Runner};

fn pid(s: &str) -> PhaseId {
    PhaseId::parse(s).expect("valid phase id")
}

#[derive(Default, Clone)]
struct Script {
    writes: Vec<(PathBuf, Vec<u8>)>,
    /// When set, write the dispatched `req.user_prompt` to this path. Lets
    /// the test inspect what the prompt renderer produced (e.g., to verify
    /// `--max-items` truncation).
    capture_prompt: Option<PathBuf>,
    stop_reason: Option<StopReason>,
    exit_code: Option<i32>,
}

impl Script {
    fn write(mut self, rel: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) -> Self {
        self.writes.push((rel.into(), bytes.into()));
        self
    }

    fn capture_prompt_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.capture_prompt = Some(path.into());
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
        if let Some(prompt_path) = &script.capture_prompt {
            if let Some(parent) = prompt_path.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(prompt_path, req.user_prompt.as_bytes())
                .expect("scripted agent: prompt capture write failed");
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
    let status = StdCommand::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(dir)
        .status()
        .expect("git init");
    assert!(status.success(), "git init failed");
    for (k, v) in [
        ("user.name", "pitboss-test"),
        ("user.email", "pitboss@test"),
    ] {
        StdCommand::new("git")
            .args(["-C"])
            .arg(dir)
            .args(["config", k, v])
            .status()
            .unwrap();
    }
    let status = StdCommand::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["commit", "--allow-empty", "-m", "seed", "-q"])
        .status()
        .expect("git seed commit");
    assert!(status.success());
}

fn audit_disabled() -> Config {
    let mut c = Config::default();
    c.audit.enabled = false;
    c.sweep.audit_enabled = false;
    common::disable_final_sweep(&mut c);
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

// ---------- clap-level mutual exclusion ----------

#[test]
fn play_no_sweep_and_sweep_are_mutually_exclusive() {
    let mut cmd = AssertCommand::cargo_bin("pitboss").expect("binary built");
    cmd.args(["play", "--no-sweep", "--sweep"]);
    let assert = cmd.assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("--sweep") && stderr.contains("--no-sweep"),
        "expected clap to mention both flags; stderr:\n{stderr}"
    );
}

// ---------- runner-level overrides ----------

/// `pitboss play --no-sweep` against a 10-item backlog: the trigger
/// threshold is met, but the override suppresses the sweep entirely. No
/// `SweepStarted` event fires and no sweep commit lands.
#[tokio::test]
async fn no_sweep_override_suppresses_threshold_sweep() {
    use tokio::sync::broadcast::error::RecvError;

    let pairs: Vec<(&str, bool)> = (0..10)
        .map(|i| {
            (
                Box::leak(format!("item {i}").into_boxed_str()) as &str,
                false,
            )
        })
        .collect();
    let initial = deferred_items_only(&pairs);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        Script::default().write("src/phase_02.rs", b"// 2\n"),
    ]);

    let mut runner = build_runner(
        dir.path(),
        TWO_PHASE_PLAN,
        &initial,
        audit_disabled(),
        agent,
    )
    .await
    .skip_sweep(true);

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        loop {
            match rx.recv().await {
                Ok(Event::SweepStarted { .. }) => events.push("SweepStarted".to_string()),
                Ok(Event::PhaseStarted { phase_id, .. }) => {
                    events.push(format!("PhaseStarted({phase_id})"))
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

    assert!(
        events.iter().all(|e| e != "SweepStarted"),
        "no sweep should fire under --no-sweep; events: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.starts_with("PhaseStarted("))
            .count(),
        2,
        "both phases must run; events: {events:?}"
    );

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(!state.pending_sweep);
}

/// `pitboss play --sweep` against a 2-item backlog: below the configured
/// threshold of 5, but the override forces a sweep before the next phase.
/// A `SweepStarted` event fires and the sweep commit lands ahead of phase
/// 02.
#[tokio::test]
async fn sweep_override_forces_sweep_below_threshold() {
    use tokio::sync::broadcast::error::RecvError;

    let initial = deferred_items_only(&[("a", false), ("b", false)]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    // Sweep agent flips both items off and writes a marker.
    let post = deferred_items_only(&[("a", true), ("b", true)]);
    let agent = ScriptedAgent::new(vec![
        // Phase 01 implementer.
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        // Forced sweep dispatch.
        Script::default()
            .write(".pitboss/play/deferred.md", post.as_bytes())
            .write("src/sweep_marker.rs", b"// sweep\n"),
        // Phase 02 implementer.
        Script::default().write("src/phase_02.rs", b"// 2\n"),
    ]);

    let mut config = audit_disabled();
    // Belt-and-suspenders: even if the threshold defaults change, the
    // override has to win below the configured trigger, so make sure the
    // threshold is well above the backlog size.
    config.sweep = SweepConfig {
        trigger_min_items: 100,
        trigger_max_items: 100,
        ..config.sweep
    };
    let mut runner = build_runner(dir.path(), TWO_PHASE_PLAN, &initial, config, agent)
        .await
        .force_sweep(true);

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        loop {
            match rx.recv().await {
                Ok(Event::SweepStarted { after, .. }) => {
                    events.push(format!("SweepStarted({after})"))
                }
                Ok(Event::SweepCompleted {
                    after, resolved, ..
                }) => events.push(format!("SweepCompleted({after},{resolved})")),
                Ok(Event::PhaseStarted { phase_id, .. }) => {
                    events.push(format!("PhaseStarted({phase_id})"))
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

    assert!(
        events.iter().any(|e| e.starts_with("SweepStarted")),
        "expected forced sweep to fire; events: {events:?}"
    );
    let sweep_started = events
        .iter()
        .position(|e| e.starts_with("SweepStarted("))
        .expect("sweep started");
    let phase02_started = events
        .iter()
        .position(|e| e == "PhaseStarted(02)")
        .expect("phase 02 started");
    assert!(
        sweep_started < phase02_started,
        "sweep must precede phase 02; events: {events:?}"
    );
}

/// `pitboss play --sweep` on a *single*-phase fresh run: the override
/// arms `pending_sweep`, but the gate sees `state.completed` empty and
/// silently no-ops instead of dispatching a sweep against an empty
/// history. Phase 01 runs unscathed, no `SweepStarted` is emitted before
/// the phase. (The final-sweep loop after phase 01 commits remains
/// owned by phase 08 and is exercised separately.)
#[tokio::test]
async fn sweep_override_on_fresh_run_skips_pre_phase_sweep() {
    use tokio::sync::broadcast::error::RecvError;

    let initial = deferred_items_only(&[("a", false), ("b", false)]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        // Phase 01 implementer — the only dispatch we expect on this run.
        Script::default().write("src/phase_01.rs", b"// 1\n"),
    ]);

    let mut config = audit_disabled();
    config.sweep = SweepConfig {
        trigger_min_items: 100,
        trigger_max_items: 100,
        ..config.sweep
    };
    let mut runner = build_runner(dir.path(), ONE_PHASE_PLAN, &initial, config, agent)
        .await
        .force_sweep(true);

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        loop {
            match rx.recv().await {
                Ok(Event::SweepStarted { .. }) => events.push("SweepStarted".to_string()),
                Ok(Event::PhaseStarted { phase_id, .. }) => {
                    events.push(format!("PhaseStarted({phase_id})"))
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        events
    });

    runner.run_phase().await.unwrap();
    drop(runner);
    let events = collector.await.unwrap();

    let phase_pos = events
        .iter()
        .position(|e| e == "PhaseStarted(01)")
        .expect("phase 01 must start");
    let pre_phase_sweep = events[..phase_pos].iter().any(|e| e == "SweepStarted");
    assert!(
        !pre_phase_sweep,
        "fresh-run --sweep must not dispatch a sweep before phase 01; events: {events:?}",
    );
}

/// On a multi-phase run, `--sweep` is one-shot: it fires exactly one
/// forced sweep at the first inter-phase boundary, then reverts to the
/// configured threshold logic. With a backlog below threshold, the gap
/// between phase 02 and phase 03 must NOT see a second sweep — only the
/// post-phase-01 forced sweep fires.
#[tokio::test]
async fn sweep_override_force_is_consumed_after_first_sweep() {
    use tokio::sync::broadcast::error::RecvError;

    const THREE_PHASE_PLAN: &str = "\
---
current_phase: \"01\"
---

# Pitboss Plan

# Phase 01: First

**Scope.** First.

# Phase 02: Second

**Scope.** Second.

# Phase 03: Third

**Scope.** Third.
";

    let initial = deferred_items_only(&[("a", false), ("b", false)]);
    let dir = make_workspace(THREE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let post = deferred_items_only(&[("a", true), ("b", true)]);
    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/phase_01.rs", b"// 1\n"),
        Script::default()
            .write(".pitboss/play/deferred.md", post.as_bytes())
            .write("src/sweep_marker.rs", b"// sweep\n"),
        Script::default().write("src/phase_02.rs", b"// 2\n"),
        Script::default().write("src/phase_03.rs", b"// 3\n"),
    ]);

    let mut config = audit_disabled();
    config.sweep = SweepConfig {
        trigger_min_items: 100,
        trigger_max_items: 100,
        ..config.sweep
    };
    let mut runner = build_runner(dir.path(), THREE_PHASE_PLAN, &initial, config, agent)
        .await
        .force_sweep(true);

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        loop {
            match rx.recv().await {
                Ok(Event::SweepStarted { after, .. }) => {
                    events.push(format!("SweepStarted({after})"))
                }
                Ok(Event::PhaseStarted { phase_id, .. }) => {
                    events.push(format!("PhaseStarted({phase_id})"))
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        events
    });

    runner.run().await.unwrap();
    drop(runner);
    let events = collector.await.unwrap();

    let sweep_count = events
        .iter()
        .filter(|e| e.starts_with("SweepStarted"))
        .count();
    assert_eq!(
        sweep_count, 1,
        "--sweep must fire exactly once on a multi-phase run; events: {events:?}",
    );
}

// ---------- standalone sweep ----------

/// `pitboss sweep`-style standalone invocation against a 7-item backlog:
/// dispatches the sweep without advancing the plan. With the no-op style
/// scripted agent (no edits), the sweep produces no commit.
#[tokio::test]
async fn standalone_sweep_runs_without_advancing_plan() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
        ("f", false),
        ("g", false),
    ]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    // Scripted agent produces no edits — like a dry-run agent.
    let agent = ScriptedAgent::new(vec![Script::default()]);

    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        audit_disabled(),
        agent,
    )
    .await;

    let result = runner.run_standalone_sweep(None, None, true).await.unwrap();
    assert!(
        matches!(result, PhaseResult::Advanced { commit: None, .. }),
        "standalone sweep without edits must produce no commit; got {result:?}"
    );

    // Plan state machine is untouched.
    let state = runner.state();
    assert!(state.completed.is_empty());
    assert_eq!(runner.plan().current_phase, pid("01"));
    drop(runner);

    // No sweep commit on the branch — only the seed commit from init.
    let log = StdCommand::new("git")
        .args(["-C"])
        .arg(dir.path())
        .args(["log", "--oneline", "--all"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(
        !log.contains("sweep after phase"),
        "no-edit sweep must not produce a commit; log:\n{log}"
    );
}

/// `pitboss sweep --max-items 5` against a 20-item backlog: the
/// implementer's prompt sees only the first 5 pending items in document
/// order. The on-disk file is unchanged, so subsequent sweeps surface the
/// remainder.
#[tokio::test]
async fn standalone_sweep_max_items_clamps_prompt() {
    let texts: Vec<String> = (0..20).map(|i| format!("pending item {i:02}")).collect();
    let pairs: Vec<(&str, bool)> = texts.iter().map(|t| (t.as_str(), false)).collect();
    let initial = deferred_items_only(&pairs);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let captured_prompt = dir.path().join("captured-prompt.txt");
    let agent = ScriptedAgent::new(vec![
        Script::default().capture_prompt_to(captured_prompt.clone())
    ]);

    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        audit_disabled(),
        agent,
    )
    .await;

    runner
        .run_standalone_sweep(None, Some(5), true)
        .await
        .expect("sweep ran");

    let prompt = fs::read_to_string(&captured_prompt).expect("prompt captured");
    let lines: Vec<&str> = prompt
        .lines()
        .filter(|l| l.starts_with("- [ ] pending item "))
        .collect();
    assert_eq!(
        lines.len(),
        5,
        "expected only 5 pending items in the rendered prompt; got {}: {prompt}",
        lines.len(),
    );
    // First 5 items are 00..04 in document order.
    for (i, line) in lines.iter().enumerate() {
        assert!(
            line.contains(&format!("pending item {i:02}")),
            "expected prompt line {i} to be item {i:02}; got: {line}",
        );
    }
    assert!(
        !prompt.contains("pending item 05"),
        "item 05 must be excluded by --max-items 5; prompt:\n{prompt}",
    );

    // The on-disk file is unchanged: 20 items still pending.
    let on_disk = fs::read_to_string(dir.path().join(".pitboss/play/deferred.md")).unwrap();
    let pending_on_disk = on_disk
        .lines()
        .filter(|l| l.starts_with("- [ ] pending item "))
        .count();
    assert_eq!(
        pending_on_disk, 20,
        "on-disk deferred.md must be unclamped; got:\n{on_disk}",
    );
}

/// A halted sweep leaves `state.pending_sweep` armed on disk so a
/// subsequent retry can pick it up. Mirrors the behavior `pitboss play`
/// already exhibits at sweep boundaries.
#[tokio::test]
async fn standalone_sweep_halt_persists_pending_sweep() {
    let initial = deferred_items_only(&[
        ("a", false),
        ("b", false),
        ("c", false),
        ("d", false),
        ("e", false),
    ]);
    let dir = make_workspace(ONE_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    // Scripted agent explodes on the sweep dispatch.
    let agent = ScriptedAgent::new(vec![Script {
        stop_reason: Some(StopReason::Error("synthetic sweep failure".into())),
        ..Script::default()
    }]);

    let mut runner = build_runner(
        dir.path(),
        ONE_PHASE_PLAN,
        &initial,
        audit_disabled(),
        agent,
    )
    .await;
    // Seed `pending_sweep = true` so this test exercises the literal phase
    // 06 spec line: a halted sweep with the obligation already armed must
    // leave the flag true on disk for retry. Without seeding, the halt
    // path only proves the flag isn't *set* — not that it isn't *cleared*.
    runner.state_mut().pending_sweep = true;

    let result = runner.run_standalone_sweep(None, None, true).await.unwrap();
    assert!(matches!(result, PhaseResult::Halted { .. }));

    let state_after = runner.state().clone();
    drop(runner);

    // Mirror the CLI: persist on the halt path so a retry observes the
    // armed obligation.
    pitboss::state::save(dir.path(), Some(&state_after)).unwrap();
    let reloaded = pitboss::state::load(dir.path()).unwrap().expect("state");

    assert!(
        reloaded.pending_sweep,
        "halt path must leave state.pending_sweep = true on disk; got {reloaded:?}",
    );
    assert!(
        !reloaded.deferred_item_attempts.is_empty(),
        "halt path must record sweep-attempt counters for survivors",
    );
}
