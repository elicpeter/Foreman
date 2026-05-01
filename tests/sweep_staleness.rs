//! Integration tests for the phase 05 sweep staleness tracker.
//!
//! Each test stands a workspace up against a real `git init`'d directory and
//! drives the runner with a [`ScriptedAgent`] (the same shape sweep_smoke.rs
//! and sweep_auditor.rs use). The runner's per-item attempt counter lives on
//! `RunState::deferred_item_attempts` and is updated at every sweep exit —
//! success, halt, and budget-skip alike. These tests exercise the counter
//! transitions, the `Event::DeferredItemStale` emission, the
//! `Runner::stale_items` helper feeding the sweep prompt, and resume across
//! a `state.json` written before this field existed.

#![cfg(unix)]

use std::collections::{HashMap, VecDeque};
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
use pitboss::prompts::{self, StaleItem};
use pitboss::runner::{self, Event, Runner};

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

const FOUR_PHASE_PLAN: &str = "\
---
current_phase: \"01\"
---

# Pitboss Plan

# Phase 01: First

**Scope.** First phase.

# Phase 02: Second

**Scope.** Second phase.

# Phase 03: Third

**Scope.** Third phase.

# Phase 04: Fourth

**Scope.** Fourth phase.
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

/// Tight trigger so 2-3 items already trip a sweep — keeps test fixtures
/// small. All audits off; this file is about counter bookkeeping, not the
/// auditor pass.
fn staleness_config() -> Config {
    let mut c = Config::default();
    c.audit.enabled = false;
    c.sweep.audit_enabled = false;
    c.sweep.trigger_min_items = 1;
    // Phase 08's trailing drain loop fires after the final phase if any
    // unchecked item remains. These tests count sweeps that fire between
    // phases; the drain is covered in `tests/sweep_final_loop.rs`.
    c.sweep.final_sweep_enabled = false;
    // Default escalate_after = 3 stays — the tests reason about that value.
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

/// 4 phases × 3 sweeps with 2 items the implementer never resolves leaves
/// `attempts == 3` for both items in `state.deferred_item_attempts`.
#[tokio::test]
async fn counter_increments_for_surviving_items_across_three_sweeps() {
    let initial = deferred_items_only(&[("alpha", false), ("beta", false)]);
    let dir = make_workspace(FOUR_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    // Each sweep impl is a no-op so both items survive every dispatch. Each
    // phase impl writes a per-phase marker so phase tests / commits land.
    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/p1.rs", b"// 1\n"),
        Script::default(), // sweep 1
        Script::default().write("src/p2.rs", b"// 2\n"),
        Script::default(), // sweep 2
        Script::default().write("src/p3.rs", b"// 3\n"),
        Script::default(), // sweep 3
        Script::default().write("src/p4.rs", b"// 4\n"),
    ]);

    let mut runner = build_runner(
        dir.path(),
        FOUR_PHASE_PLAN,
        &initial,
        staleness_config(),
        agent,
    )
    .await;
    runner.run().await.unwrap();

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(
        state.deferred_item_attempts.get("alpha").copied(),
        Some(3),
        "expected alpha to survive 3 sweeps; map: {:?}",
        state.deferred_item_attempts
    );
    assert_eq!(
        state.deferred_item_attempts.get("beta").copied(),
        Some(3),
        "expected beta to survive 3 sweeps; map: {:?}",
        state.deferred_item_attempts
    );
}

/// Resolving an item drops it from the staleness map. Survivors keep
/// counting; resolved keys do not linger.
#[tokio::test]
async fn counter_resets_when_item_resolved() {
    let initial = deferred_items_only(&[("alpha", false), ("beta", false)]);
    let dir = make_workspace(FOUR_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    // Sweep 1 flips "alpha" → done; "beta" survives.
    let after_sweep_1 = deferred_items_only(&[("alpha", true), ("beta", false)]);

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/p1.rs", b"// 1\n"),
        // Sweep 1: resolves alpha. After-sweep `self.deferred.sweep()` drops
        // it from the cached doc, so subsequent dispatches don't see it.
        Script::default()
            .write(".pitboss/play/deferred.md", after_sweep_1.as_bytes())
            .write("src/sweep_1.rs", b"// sweep 1\n"),
        Script::default().write("src/p2.rs", b"// 2\n"),
        // Sweep 2: no-op; beta survives a second time.
        Script::default(),
        Script::default().write("src/p3.rs", b"// 3\n"),
    ]);

    let mut config = staleness_config();
    // Single-pass sweep boundary plenty for this test; halt the run early
    // by limiting plan to 3 phases? Use the 4-phase plan but expect early
    // termination once scripts run out. We pre-load enough scripts to drive
    // through phase 03; phase 04 would need more, but we don't run that far
    // here — assert on state right after the 2nd sweep instead.
    config.sweep.escalate_after = 5; // keep no events from firing in this test

    let mut runner = build_runner(
        dir.path(),
        FOUR_PHASE_PLAN,
        &initial,
        config,
        agent,
    )
    .await;

    // Drive phase 01 → sweep 1 → phase 02 → sweep 2 → phase 03 by hand so we
    // can stop before the script queue runs out.
    for _ in 0..5 {
        let _ = runner.run_phase().await.unwrap();
    }

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(
        !state.deferred_item_attempts.contains_key("alpha"),
        "alpha was resolved by sweep 1 — must not carry a counter; map: {:?}",
        state.deferred_item_attempts
    );
    assert_eq!(
        state.deferred_item_attempts.get("beta").copied(),
        Some(2),
        "beta survived 2 sweeps; map: {:?}",
        state.deferred_item_attempts
    );
}

/// A halted sweep still advances the per-item attempts counter for any item
/// the agent failed to resolve. The runner halt is not a free pass on the
/// staleness clock — `consecutive_sweeps` and `deferred_item_attempts` track
/// different things.
#[tokio::test]
async fn halted_sweep_increments_counter_for_survivors() {
    let initial = deferred_items_only(&[("alpha", false), ("beta", false)]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/p1.rs", b"// 1\n"),
        // Sweep impl: explode mid-dispatch. Validation halts before any
        // implementer edits land; `self.deferred` stays at the pre-state, so
        // both items count as survivors.
        Script {
            stop_reason: Some(StopReason::Error("synthetic sweep failure".into())),
            exit_code: Some(2),
            ..Script::default()
        },
    ]);

    let mut runner = build_runner(
        dir.path(),
        TWO_PHASE_PLAN,
        &initial,
        staleness_config(),
        agent,
    )
    .await;

    // Phase 01 advances; the second run_phase invokes the sweep, which halts.
    let _ = runner.run_phase().await.unwrap();
    let _ = runner.run_phase().await.unwrap();

    let state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(state.pending_sweep, "pending_sweep persists after sweep halt");
    assert_eq!(
        state.deferred_item_attempts.get("alpha").copied(),
        Some(1),
        "halted sweep must still tick alpha's counter; map: {:?}",
        state.deferred_item_attempts
    );
    assert_eq!(
        state.deferred_item_attempts.get("beta").copied(),
        Some(1),
        "halted sweep must still tick beta's counter; map: {:?}",
        state.deferred_item_attempts
    );
}

/// The 3rd sweep that ticks the counter past `escalate_after = 3` emits
/// `Event::DeferredItemStale` for the crossing item. A subsequent sweep that
/// only pushes the counter higher does *not* re-emit (transition only).
#[tokio::test]
async fn escalation_event_fires_only_once_on_threshold_cross() {
    use tokio::sync::broadcast::error::RecvError;

    let initial = deferred_items_only(&[("alpha", false), ("beta", false)]);
    let dir = make_workspace(FOUR_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/p1.rs", b"// 1\n"),
        Script::default(), // sweep 1
        Script::default().write("src/p2.rs", b"// 2\n"),
        Script::default(), // sweep 2
        Script::default().write("src/p3.rs", b"// 3\n"),
        Script::default(), // sweep 3 → counter crosses 3 → event fires
        Script::default().write("src/p4.rs", b"// 4\n"),
        // Phase 04 has no successor in the plan, so no sweep 4 fires from
        // the natural runner flow. We simulate a 4th sweep manually below
        // to exercise the no-re-emit branch.
    ]);

    let cfg = staleness_config();
    assert_eq!(cfg.sweep.escalate_after, 3);

    let mut runner = build_runner(dir.path(), FOUR_PHASE_PLAN, &initial, cfg, agent).await;

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut stale: Vec<(String, u32)> = Vec::new();
        loop {
            match rx.recv().await {
                Ok(Event::DeferredItemStale { text, attempts }) => stale.push((text, attempts)),
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        stale
    });

    runner.run().await.unwrap();
    drop(runner);
    let stale = collector.await.unwrap();

    // Both items crossed escalate_after on sweep 3 — so we expect exactly two
    // events, both with attempts=3.
    assert_eq!(
        stale.len(),
        2,
        "expected 2 DeferredItemStale events on the 3→3 crossing, got {stale:?}"
    );
    for (_, attempts) in &stale {
        assert_eq!(*attempts, 3, "events must carry the threshold count");
    }
    let mut texts: Vec<&str> = stale.iter().map(|(t, _)| t.as_str()).collect();
    texts.sort();
    assert_eq!(texts, vec!["alpha", "beta"]);
}

/// A 4th sweep that pushes counters from 3 → 4 emits no new event. The
/// runner's natural flow does not chain 4 sweeps under the default config
/// (max_consecutive=1, only 3 inter-phase boundaries in a 4-phase plan), so
/// we synthesize the 4th sweep by re-running `run_phase` after manually
/// re-arming `pending_sweep`.
#[tokio::test]
async fn escalation_event_does_not_refire_on_subsequent_increment() {
    use pitboss::state;
    use tokio::sync::broadcast::error::RecvError;

    let initial = deferred_items_only(&[("alpha", false), ("beta", false)]);
    let dir = make_workspace(FOUR_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    // Need 8 dispatches: 4 phases + 4 sweeps. The 4th sweep is forced by
    // re-arming `pending_sweep` between the natural 3-sweep flow and phase
    // 04. We do that mid-run by pausing after the 3rd sweep.
    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/p1.rs", b"// 1\n"), // 1
        Script::default(),                                // sweep 1 (2)
        Script::default().write("src/p2.rs", b"// 2\n"), // 3
        Script::default(),                                // sweep 2 (4)
        Script::default().write("src/p3.rs", b"// 3\n"), // 5
        Script::default(),                                // sweep 3 (6) — events fire here
        Script::default(),                                // sweep 4 (7) — must NOT re-emit
        Script::default().write("src/p4.rs", b"// 4\n"), // 8
    ]);

    let mut cfg = staleness_config();
    // Allow back-to-back sweeps so we can chain a 4th after the 3rd without
    // a regular phase in between.
    cfg.sweep.max_consecutive = 5;

    let mut runner = build_runner(dir.path(), FOUR_PHASE_PLAN, &initial, cfg, agent).await;

    let mut rx = runner.subscribe();
    let collector = tokio::spawn(async move {
        let mut stale: Vec<(String, u32)> = Vec::new();
        loop {
            match rx.recv().await {
                Ok(Event::DeferredItemStale { text, attempts }) => stale.push((text, attempts)),
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        stale
    });

    // Drive 6 run_phase calls: phase 01 → sweep 1 → phase 02 → sweep 2 → phase 03 → sweep 3.
    for _ in 0..6 {
        let _ = runner.run_phase().await.unwrap();
    }
    // Sanity: counters now at 3 and the events fired.
    let mid_state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(mid_state.deferred_item_attempts.get("alpha").copied(), Some(3));

    // Force a 4th sweep by re-arming pending_sweep without an intervening
    // phase commit. Persist via state::save so run_phase reads it on entry.
    {
        let raw = pitboss::state::load(dir.path()).unwrap().expect("state");
        let mut next = raw;
        next.pending_sweep = true;
        // consecutive_sweeps stays under max_consecutive=5 so the gate fires.
        state::save(dir.path(), Some(&next)).unwrap();
    }
    // Update the runner's in-memory state too — the runner caches it.
    // Easiest: drop and rebuild the runner from disk. But we have no helper
    // for that here. Instead trust that pending_sweep is read live from
    // self.state at the top of run_phase_inner — but it's `self.state`,
    // which is set at construction and only mutated in-process. Since the
    // disk save we just did is invisible to this runner, we instead set
    // `pending_sweep` on the runner via running another phase first then
    // letting the natural flow re-trip it. Simpler: drop the runner and
    // rebuild from the persisted state.
    drop(runner);

    // Rebuild the runner with the freshly-persisted state. The agent
    // continues from where it left off because we reuse the script queue
    // through a fresh agent — but ScriptedAgent owns its queue, so re-using
    // means we need to construct one with the remaining 2 scripts.
    let remaining_agent = ScriptedAgent::new(vec![
        // sweep 4: no-op (counters tick to 4; no fresh threshold cross).
        Script::default(),
        // phase 04 impl.
        Script::default().write("src/p4.rs", b"// 4\n"),
    ]);

    let cfg2 = {
        let mut c = staleness_config();
        c.sweep.max_consecutive = 5;
        c
    };
    let loaded_state = pitboss::state::load(dir.path()).unwrap().expect("state");
    // The cached plan object on disk has current_phase advanced; load it.
    let plan_on_disk = std::fs::read_to_string(dir.path().join(".pitboss/play/plan.md")).unwrap();
    let plan_loaded = plan::parse(&plan_on_disk).unwrap();
    let deferred_on_disk =
        std::fs::read_to_string(dir.path().join(".pitboss/play/deferred.md")).unwrap();
    let deferred_loaded = pitboss::deferred::parse(&deferred_on_disk).unwrap();

    let runner_git = ShellGit::new(dir.path());
    let mut runner2 = Runner::new(
        dir.path().to_path_buf(),
        cfg2,
        plan_loaded,
        deferred_loaded,
        loaded_state,
        remaining_agent,
        runner_git,
    );

    // Subscribe a fresh receiver — older events are still queued on the old
    // collector, so just collect new ones from this runner.
    let mut rx2 = runner2.subscribe();
    let collector2 = tokio::spawn(async move {
        let mut stale: Vec<(String, u32)> = Vec::new();
        loop {
            match rx2.recv().await {
                Ok(Event::DeferredItemStale { text, attempts }) => stale.push((text, attempts)),
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
        stale
    });

    runner2.run().await.unwrap();
    drop(runner2);
    let stale_after_4 = collector2.await.unwrap();
    let _initial = collector.await.unwrap();

    assert!(
        stale_after_4.is_empty(),
        "items already at/above escalate_after must not re-emit — got {stale_after_4:?}"
    );

    let final_state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(
        final_state.deferred_item_attempts.get("alpha").copied(),
        Some(4),
        "alpha counter must keep incrementing past the threshold; map: {:?}",
        final_state.deferred_item_attempts
    );
    assert_eq!(
        final_state.deferred_item_attempts.get("beta").copied(),
        Some(4)
    );
}

/// Real `Runner::stale_items` feeding the sweep prompt: by the 3rd sweep the
/// rendered prompt's "Stale items" section names the items the runner has
/// flagged as crossing `escalate_after`.
#[tokio::test]
async fn stale_items_appear_in_third_sweep_prompt() {
    let initial = deferred_items_only(&[("alpha", false), ("beta", false)]);
    let dir = make_workspace(FOUR_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/p1.rs", b"// 1\n"),
        Script::default(),
        Script::default().write("src/p2.rs", b"// 2\n"),
        Script::default(),
        Script::default().write("src/p3.rs", b"// 3\n"),
        Script::default(),
        Script::default().write("src/p4.rs", b"// 4\n"),
    ]);

    let cfg = staleness_config();
    let mut runner = build_runner(dir.path(), FOUR_PHASE_PLAN, &initial, cfg, agent).await;

    // Drive through phase 03 commit (5 run_phase calls). After that, the
    // counter for both items is 2; on the 3rd sweep call (run_phase #6) the
    // prompt is rendered with stale_items still empty (because crossing
    // happens *during* that sweep, not before). To verify the "stale items
    // in prompt" condition, we instead synthesize the state at the start of
    // sweep 4: counters at 3 (just crossed), prompt rendered, "Stale items"
    // section non-empty.
    for _ in 0..6 {
        let _ = runner.run_phase().await.unwrap();
    }
    let stale = runner.stale_items();
    assert_eq!(
        stale.len(),
        2,
        "after 3 sweeps both items should be at attempts=3 and surface as stale; got {stale:?}"
    );
    let texts: Vec<&str> = stale.iter().map(|s| s.text.as_str()).collect();
    assert!(texts.contains(&"alpha"));
    assert!(texts.contains(&"beta"));

    // Render the sweep prompt with these stale items and the current deferred.
    let plan_now = runner.plan().clone();
    let deferred_now = runner.deferred().clone();
    let after = pid("03");
    let rendered = prompts::sweep(&plan_now, &deferred_now, Some(&after), &stale);
    assert!(
        rendered.contains("# Stale items"),
        "expected Stale items section header in rendered prompt:\n{rendered}"
    );
    assert!(
        rendered.contains("3 sweep attempts"),
        "expected attempt count line in stale items section:\n{rendered}"
    );
    assert!(
        rendered.contains("alpha") && rendered.contains("beta"),
        "expected both stale items in rendered prompt:\n{rendered}"
    );
    assert!(
        !rendered.contains("(none)"),
        "with non-empty stale_items, the (none) marker must not appear:\n{rendered}"
    );
}

/// `Runner::stale_items` caps at [`runner::STALE_ITEMS_PROMPT_CAP`] entries
/// and sorts by descending attempts. Driving 15 distinct items into the map
/// at attempts=4 yields exactly 10 stale items, all at 4.
#[tokio::test]
async fn stale_items_capped_at_ten_and_sorted_by_attempts() {
    let initial = deferred_items_only(&[("placeholder", false)]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let agent = ScriptedAgent::new(vec![Script::default().write("src/p1.rs", b"// 1\n")]);
    let mut cfg = staleness_config();
    // Disable sweeps entirely so we only build the runner; we manipulate
    // state directly afterward.
    cfg.sweep.enabled = false;
    let runner = build_runner(dir.path(), TWO_PHASE_PLAN, &initial, cfg, agent).await;

    // Hand-populate `state.deferred_item_attempts`. We can't mutate the
    // private state directly, so write to disk and rebuild the runner.
    let mut state = runner.state().clone();
    drop(runner);

    let mut attempts_map: HashMap<String, u32> = HashMap::new();
    // 15 items, all at the same attempts=4 (above escalate_after=3).
    for i in 0..15 {
        attempts_map.insert(format!("item-{i:02}"), 4);
    }
    // One additional item at attempts=10 — must be the first entry returned.
    attempts_map.insert("topdog".to_string(), 10);
    state.deferred_item_attempts = attempts_map;
    pitboss::state::save(dir.path(), Some(&state)).unwrap();

    // Rebuild the runner so it picks up the persisted state.
    let agent = ScriptedAgent::new(vec![]);
    let plan_obj = plan::parse(TWO_PHASE_PLAN).unwrap();
    let deferred_obj = pitboss::deferred::parse(&initial).unwrap();
    let loaded = pitboss::state::load(dir.path()).unwrap().expect("state");
    let runner2 = Runner::new(
        dir.path().to_path_buf(),
        {
            let mut c = Config::default();
            c.sweep.enabled = true;
            c.sweep.escalate_after = 3;
            c
        },
        plan_obj,
        deferred_obj,
        loaded,
        agent,
        ShellGit::new(dir.path()),
    );

    let stale = runner2.stale_items();
    assert_eq!(
        stale.len(),
        runner::STALE_ITEMS_PROMPT_CAP,
        "stale items must cap at {}; got {} entries",
        runner::STALE_ITEMS_PROMPT_CAP,
        stale.len()
    );
    // The first entry is the highest-attempts item.
    assert_eq!(stale[0].text, "topdog");
    assert_eq!(stale[0].attempts, 10);
    // Remaining 9 are all at attempts=4 (the highest among the rest), in
    // text-ascending order as the deterministic tiebreaker.
    for entry in &stale[1..] {
        assert_eq!(entry.attempts, 4);
    }
    let tail_texts: Vec<&str> = stale[1..].iter().map(|s| s.text.as_str()).collect();
    let mut sorted = tail_texts.clone();
    sorted.sort();
    assert_eq!(
        tail_texts, sorted,
        "ties broken by ascending text for determinism"
    );
}

/// A `state.json` written before phase 05 (no `deferred_item_attempts` field)
/// must load with an empty map and the next sweep must populate it cleanly.
#[tokio::test]
async fn legacy_state_json_loads_with_empty_map_and_populates_on_first_sweep() {
    let initial = deferred_items_only(&[("alpha", false), ("beta", false)]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    // Write a phase-04 shape state.json — no `deferred_item_attempts`.
    let phase_04_state_json = r#"{
  "run_id": "20260430T120000Z",
  "branch": "pitboss/play/20260430T120000Z",
  "original_branch": "main",
  "started_at": "2026-04-30T12:00:00Z",
  "started_phase": "01",
  "completed": [],
  "attempts": {},
  "token_usage": {"input": 0, "output": 0, "by_role": {}},
  "aborted": false,
  "pending_sweep": false,
  "consecutive_sweeps": 0
}
"#;
    let state_path = dir.path().join(".pitboss/play/state.json");
    fs::write(&state_path, phase_04_state_json).unwrap();

    let loaded = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert!(
        loaded.deferred_item_attempts.is_empty(),
        "legacy state.json must default deferred_item_attempts to empty map"
    );

    // Drive a real run from the legacy state. Branch already exists in the
    // state, so create + checkout must use that name verbatim.
    let agent = ScriptedAgent::new(vec![
        Script::default().write("src/p1.rs", b"// 1\n"),
        Script::default(), // sweep 1 — no-op
        Script::default().write("src/p2.rs", b"// 2\n"),
    ]);
    let plan_obj = plan::parse(TWO_PHASE_PLAN).expect("parse plan");
    let deferred_obj = pitboss::deferred::parse(&initial).unwrap();
    let cfg = staleness_config();
    let git = ShellGit::new(dir.path());
    git.create_branch(&loaded.branch).await.unwrap();
    git.checkout(&loaded.branch).await.unwrap();
    let runner_git = ShellGit::new(dir.path());
    let mut runner = Runner::new(
        dir.path().to_path_buf(),
        cfg,
        plan_obj,
        deferred_obj,
        loaded,
        agent,
        runner_git,
    );

    runner.run().await.unwrap();

    let final_state = pitboss::state::load(dir.path()).unwrap().expect("state");
    assert_eq!(
        final_state.deferred_item_attempts.get("alpha").copied(),
        Some(1),
        "first sweep after a legacy resume must start populating the map; got {:?}",
        final_state.deferred_item_attempts
    );
    assert_eq!(
        final_state.deferred_item_attempts.get("beta").copied(),
        Some(1),
    );
}

/// `Runner::stale_items` returns nothing when nothing has crossed
/// `escalate_after`. Sanity check the empty-map → empty-vec path used by
/// the prompt renderer's `(none)` marker.
#[test]
fn stale_items_empty_when_below_threshold() {
    let initial = deferred_items_only(&[("alpha", false)]);
    let dir = make_workspace(TWO_PHASE_PLAN, &initial);
    init_git_repo(dir.path());

    let plan_obj = plan::parse(TWO_PHASE_PLAN).unwrap();
    let deferred_obj = pitboss::deferred::parse(&initial).unwrap();
    let cfg = staleness_config();
    let mut state = runner::fresh_run_state(&plan_obj, &cfg, Utc::now());
    // Two items at attempts=2, below the default escalate_after=3.
    state
        .deferred_item_attempts
        .insert("alpha".to_string(), 2);
    state.deferred_item_attempts.insert("beta".to_string(), 1);

    let runner = Runner::new(
        dir.path().to_path_buf(),
        cfg,
        plan_obj,
        deferred_obj,
        state,
        ScriptedAgent::new(vec![]),
        ShellGit::new(dir.path()),
    );

    let stale: Vec<StaleItem> = runner.stale_items();
    assert!(
        stale.is_empty(),
        "items below escalate_after must not be reported as stale; got {stale:?}"
    );
}
