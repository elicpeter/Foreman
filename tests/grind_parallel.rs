//! Phase 11 grind acceptance: drive a [`GrindRunner`] with parallel-safe
//! prompts, real worktrees, and a scripted [`MockAgent`] and assert the
//! semantics the spec calls out: both commits land when sessions don't
//! conflict, a non-FF merge labels the prompt's `parallel_safe: true` claim
//! as violated, sequential prompts continue to serialize, parallel sessions
//! actually overlap on the wall clock, and a non-parallel-safe prompt locks
//! every permit so no sibling session is ever running alongside it.

#![cfg(unix)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use pitboss::agent::{Agent, AgentEvent, AgentOutcome, AgentRequest, StopReason};
use pitboss::config::Config;
use pitboss::git::{Git, ShellGit};
use pitboss::grind::{
    parallel_safe_violation_summary, GrindPlan, GrindRunner, GrindShutdown, GrindStopReason, Hooks,
    ParallelSafeViolationSite, PlanBudgets, PlanPromptRef, PromptDoc, PromptMeta, PromptSource,
    RunDir, SessionStatus,
};

/// Pre-canned per-prompt behavior for the [`MockAgent`]. Each prompt declares
/// which file the session writes, what content it writes, and how long the
/// agent should pretend to think before returning.
#[derive(Debug, Clone)]
struct PromptBehavior {
    /// Relative path inside `req.workdir` the agent writes.
    file_path: PathBuf,
    /// Bytes written to that path. Distinct per session so commits stay
    /// distinct; overlapping content across two sessions is what tickles the
    /// non-FF case.
    content_template: String,
    /// Wall-clock the agent sleeps before returning.
    sleep: Duration,
}

/// Records the number of in-flight sessions at the moment each agent was
/// invoked, plus the wall-clock interval of every session that ran. Drives
/// the parallel-vs-sequential assertions below — including the overlap-based
/// proof of parallelism that replaces the previous flaky wall-clock fraction
/// assertion (see `deferred.md`).
#[derive(Debug, Default)]
struct ConcurrencyJournal {
    /// Currently-in-flight session count.
    in_flight: AtomicUsize,
    /// Maximum in-flight count observed at any point during the run.
    max_in_flight: AtomicUsize,
    /// Per-prompt list of (in_flight at start) snapshots so the test can ask
    /// "what siblings were running when this prompt started?"
    by_prompt: Mutex<HashMap<String, Vec<usize>>>,
    /// Per-session-key (prompt:seq) start instants captured at enter; popped
    /// at leave to land in `intervals` paired with the leave instant.
    pending_starts: Mutex<HashMap<String, Instant>>,
    /// Closed intervals (entered, left). Drives `max_overlap`, which proves
    /// parallelism without depending on summed-sleep wall-clock arithmetic.
    intervals: Mutex<Vec<(Instant, Instant)>>,
}

impl ConcurrencyJournal {
    fn enter(&self, prompt: &str, seq: &str) -> usize {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        loop {
            let prev = self.max_in_flight.load(Ordering::SeqCst);
            if now <= prev {
                break;
            }
            if self
                .max_in_flight
                .compare_exchange(prev, now, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
        self.by_prompt
            .lock()
            .unwrap()
            .entry(prompt.to_string())
            .or_default()
            .push(now);
        self.pending_starts
            .lock()
            .unwrap()
            .insert(session_key(prompt, seq), Instant::now());
        now
    }

    fn leave(&self, prompt: &str, seq: &str) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        let key = session_key(prompt, seq);
        if let Some(start) = self.pending_starts.lock().unwrap().remove(&key) {
            self.intervals.lock().unwrap().push((start, Instant::now()));
        }
    }

    fn max(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }

    fn entries_for(&self, prompt: &str) -> Vec<usize> {
        self.by_prompt
            .lock()
            .unwrap()
            .get(prompt)
            .cloned()
            .unwrap_or_default()
    }

    /// Largest pairwise overlap across every closed session interval.
    ///
    /// Returns `Duration::ZERO` when fewer than two sessions ran or when no
    /// pair overlapped on the wall clock. Used to prove parallelism directly
    /// — `max_overlap > sleep / 2` is far more robust than `elapsed < 0.75 *
    /// sum(sleeps)`, which falls apart when per-session git overhead climbs
    /// on a slow CI host.
    fn max_overlap(&self) -> Duration {
        let intervals = self.intervals.lock().unwrap().clone();
        let mut best = Duration::ZERO;
        for (i, a) in intervals.iter().enumerate() {
            for b in intervals.iter().skip(i + 1) {
                let lo = a.0.max(b.0);
                let hi = a.1.min(b.1);
                if hi > lo {
                    let d = hi - lo;
                    if d > best {
                        best = d;
                    }
                }
            }
        }
        best
    }
}

fn session_key(prompt: &str, seq: &str) -> String {
    format!("{prompt}#{seq}")
}

struct MockAgent {
    name: String,
    behaviors: HashMap<String, PromptBehavior>,
    invocations: Arc<AtomicU32>,
    journal: Arc<ConcurrencyJournal>,
}

impl MockAgent {
    fn new(behaviors: HashMap<String, PromptBehavior>, journal: Arc<ConcurrencyJournal>) -> Self {
        Self {
            name: "grind-parallel-mock".into(),
            behaviors,
            invocations: Arc::new(AtomicU32::new(0)),
            journal,
        }
    }
}

#[async_trait]
impl Agent for MockAgent {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(
        &self,
        req: AgentRequest,
        events: mpsc::Sender<AgentEvent>,
        _cancel: CancellationToken,
    ) -> Result<AgentOutcome> {
        let n = self.invocations.fetch_add(1, Ordering::SeqCst) + 1;
        let prompt_name = req
            .env
            .get("PITBOSS_PROMPT_NAME")
            .cloned()
            .unwrap_or_default();
        let seq = req
            .env
            .get("PITBOSS_SESSION_SEQ")
            .cloned()
            .unwrap_or_default();

        let _ = self.journal.enter(&prompt_name, &seq);

        let behavior = self
            .behaviors
            .get(&prompt_name)
            .unwrap_or_else(|| panic!("no behavior configured for prompt {prompt_name:?}"))
            .clone();

        // Write the marker file in the agent's workdir. For parallel
        // sessions this is the worktree path; for sequential sessions it is
        // the main workspace.
        let target = req.workdir.join(&behavior.file_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let body = behavior
            .content_template
            .replace("{prompt}", &prompt_name)
            .replace("{seq}", &seq);
        std::fs::write(&target, body.as_bytes()).expect("write marker file");

        // Touch the per-session scratchpad so the merge logic has something
        // observable to fold back into the run-level scratchpad.
        if let Some(pad) = req.env.get("PITBOSS_SCRATCHPAD") {
            let pad_path = PathBuf::from(pad);
            let mut existing = std::fs::read_to_string(&pad_path).unwrap_or_default();
            existing.push_str(&format!("- session {seq} ({prompt_name})\n"));
            std::fs::write(&pad_path, existing).ok();
        }

        // Sleep last so the in-flight counter is held for the entire
        // duration the test wants to observe parallelism over.
        if !behavior.sleep.is_zero() {
            tokio::time::sleep(behavior.sleep).await;
        }

        if let Some(parent) = req.log_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(
            &req.log_path,
            format!("[mock] session {seq} for {prompt_name}\n").as_bytes(),
        )
        .ok();

        let summary_file = PathBuf::from(req.env.get("PITBOSS_SUMMARY_FILE").unwrap());
        if let Some(parent) = summary_file.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(
            &summary_file,
            format!("session {seq} ran prompt {prompt_name}\n").as_bytes(),
        )
        .ok();

        let _ = events
            .send(AgentEvent::Stdout(format!(
                "[mock] {prompt_name}#{seq} (n={n})"
            )))
            .await;

        self.journal.leave(&prompt_name, &seq);

        Ok(AgentOutcome {
            exit_code: 0,
            stop_reason: StopReason::Completed,
            tokens: pitboss::state::TokenUsage::default(),
            log_path: req.log_path,
        })
    }
}

fn parallel_prompt(name: &str) -> PromptDoc {
    PromptDoc {
        meta: PromptMeta {
            name: name.into(),
            description: format!("desc for {name}"),
            weight: 1,
            every: 1,
            max_runs: Some(1),
            verify: false,
            parallel_safe: true,
            tags: vec![],
            max_session_seconds: None,
            max_session_cost_usd: None,
        },
        body: format!("body of {name}"),
        source_path: PathBuf::from(format!("/fixture/{name}.md")),
        source_kind: PromptSource::Project,
    }
}

fn sequential_prompt(name: &str) -> PromptDoc {
    PromptDoc {
        meta: PromptMeta {
            name: name.into(),
            description: format!("desc for {name}"),
            weight: 1,
            every: 1,
            max_runs: Some(1),
            verify: false,
            parallel_safe: false,
            tags: vec![],
            max_session_seconds: None,
            max_session_cost_usd: None,
        },
        body: format!("body of {name}"),
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

fn plan_with(prompts: &[PromptDoc], max_parallel: u32) -> GrindPlan {
    GrindPlan {
        name: "test-parallel".to_string(),
        prompts: prompts
            .iter()
            .map(|p| PlanPromptRef {
                name: p.meta.name.clone(),
                weight_override: None,
                every_override: None,
                max_runs_override: None,
            })
            .collect(),
        max_parallel,
        hooks: Hooks::default(),
        budgets: PlanBudgets::default(),
    }
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
        .expect("seed commit");
    assert!(status.success());
}

/// Seed a tracked file with `seed_content` so the agent's modifications
/// produce a real diff (some tests need a tracked-file conflict, not an
/// untracked-file conflict).
fn seed_tracked_file(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
    let status = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["add", "--", rel])
        .status()
        .expect("git add seed file");
    assert!(status.success());
    let status = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args([
            "-c",
            "user.name=pitboss-test",
            "-c",
            "user.email=pitboss@test",
            "commit",
            "-m",
            "seed-tracked",
            "-q",
        ])
        .status()
        .expect("commit seed file");
    assert!(status.success());
}

fn run_id() -> String {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    format!("20260430T180000Z-test{n:04}")
}

async fn make_runner(
    workspace: &Path,
    branch: &str,
    run_id_str: &str,
    plan: GrindPlan,
    prompts: &[PromptDoc],
    behaviors: HashMap<String, PromptBehavior>,
    journal: Arc<ConcurrencyJournal>,
) -> GrindRunner<MockAgent, ShellGit> {
    let git = ShellGit::new(workspace);
    git.create_branch(branch).await.unwrap();
    git.checkout(branch).await.unwrap();

    let run_dir = RunDir::create(workspace, run_id_str).expect("create run dir");
    let runner_git = ShellGit::new(workspace);
    GrindRunner::new(
        workspace.to_path_buf(),
        Config::default(),
        run_id_str.to_string(),
        branch.to_string(),
        plan,
        lookup(prompts),
        run_dir,
        MockAgent::new(behaviors, journal),
        runner_git,
        PlanBudgets::default(),
        3,
    )
}

fn count_run_branch_commits(workspace: &Path, branch: &str) -> u32 {
    let out = Command::new("git")
        .args(["-C"])
        .arg(workspace)
        .args(["rev-list", "--count", branch])
        .output()
        .expect("git rev-list");
    assert!(out.status.success(), "rev-list failed: {:?}", out);
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<u32>().expect("parse rev-list count")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_parallel_safe_sessions_commit_concurrently_and_both_land() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let rid = run_id();
    let branch = pitboss::grind::run_branch_name(&rid);

    let prompts = vec![parallel_prompt("alpha"), parallel_prompt("bravo")];
    let plan = plan_with(&prompts, 2);

    let mut behaviors: HashMap<String, PromptBehavior> = HashMap::new();
    behaviors.insert(
        "alpha".to_string(),
        PromptBehavior {
            file_path: PathBuf::from("src/alpha_marker.rs"),
            content_template: "// alpha session {seq}\n".into(),
            sleep: Duration::from_millis(0),
        },
    );
    behaviors.insert(
        "bravo".to_string(),
        PromptBehavior {
            file_path: PathBuf::from("src/bravo_marker.rs"),
            content_template: "// bravo session {seq}\n".into(),
            sleep: Duration::from_millis(0),
        },
    );

    let journal = Arc::new(ConcurrencyJournal::default());
    let mut runner = make_runner(
        dir.path(),
        &branch,
        &rid,
        plan,
        &prompts,
        behaviors,
        journal.clone(),
    )
    .await;

    let outcome = runner.run(GrindShutdown::new()).await.unwrap();

    assert_eq!(outcome.stop_reason, GrindStopReason::Completed);
    assert_eq!(outcome.sessions.len(), 2, "expected exactly two sessions");
    for r in &outcome.sessions {
        assert_eq!(r.status, SessionStatus::Ok, "record: {r:?}");
        assert!(r.commit.is_some(), "session {} produced no commit", r.seq);
    }

    // Both marker files land on the run branch.
    let alpha_marker = dir.path().join("src/alpha_marker.rs");
    let bravo_marker = dir.path().join("src/bravo_marker.rs");
    assert!(alpha_marker.is_file(), "alpha marker missing");
    assert!(bravo_marker.is_file(), "bravo marker missing");

    // The run branch advanced by exactly two commits beyond the seed.
    let total = count_run_branch_commits(dir.path(), &branch);
    assert_eq!(total, 3, "seed + 2 sessions, got {total}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_safe_violation_when_two_sessions_modify_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    seed_tracked_file(dir.path(), "src/conflict.rs", "// seed value\n");
    let rid = run_id();
    let branch = pitboss::grind::run_branch_name(&rid);

    // Both prompts write the SAME tracked file with prompt-specific content.
    // One will merge first; the other's worktree-sync ff-merge will refuse
    // because the agent's local edit conflicts with the run-branch tip
    // brought in by the sync.
    let prompts = vec![parallel_prompt("alpha"), parallel_prompt("bravo")];
    let plan = plan_with(&prompts, 2);

    let mut behaviors: HashMap<String, PromptBehavior> = HashMap::new();
    behaviors.insert(
        "alpha".to_string(),
        PromptBehavior {
            file_path: PathBuf::from("src/conflict.rs"),
            content_template: "// alpha wrote this {seq}\n".into(),
            sleep: Duration::from_millis(120),
        },
    );
    behaviors.insert(
        "bravo".to_string(),
        PromptBehavior {
            file_path: PathBuf::from("src/conflict.rs"),
            content_template: "// bravo wrote this {seq}\n".into(),
            sleep: Duration::from_millis(120),
        },
    );

    let journal = Arc::new(ConcurrencyJournal::default());
    let mut runner = make_runner(
        dir.path(),
        &branch,
        &rid,
        plan,
        &prompts,
        behaviors,
        journal.clone(),
    )
    .await;

    let outcome = runner.run(GrindShutdown::new()).await.unwrap();

    assert_eq!(outcome.stop_reason, GrindStopReason::Completed);
    assert_eq!(outcome.sessions.len(), 2, "expected exactly two sessions");
    let oks: Vec<_> = outcome
        .sessions
        .iter()
        .filter(|r| r.status == SessionStatus::Ok)
        .collect();
    let errors: Vec<_> = outcome
        .sessions
        .iter()
        .filter(|r| r.status == SessionStatus::Error)
        .collect();
    assert_eq!(oks.len(), 1, "expected one Ok: {:?}", outcome.sessions);
    assert_eq!(
        errors.len(),
        1,
        "expected one Error: {:?}",
        outcome.sessions
    );
    let err_summary = errors[0].summary.as_deref().unwrap_or("");
    // The conflict scenario (both prompts mutating the same tracked file)
    // trips the worktree-sync ff-merge: when the loser tries to bring the
    // winner's run-branch tip into its worktree, the agent's local edit
    // would be overwritten. The run-branch ff-merge variant is held off by
    // the run-branch lock and only fires under external interference.
    assert!(
        err_summary
            == parallel_safe_violation_summary(
                &errors[0].prompt,
                ParallelSafeViolationSite::WorktreeSync
            ),
        "summary should label the worktree-sync violation: got {err_summary:?}"
    );
    assert!(
        errors[0].commit.is_none(),
        "violating session must not produce a commit"
    );

    // Run branch advanced by exactly one commit (the winner).
    let total = count_run_branch_commits(dir.path(), &branch);
    assert_eq!(total, 3, "seed + tracked-seed + 1 winner, got {total}");

    // The failed worktree should be quarantined under worktrees/failed/.
    let failed_dir = dir
        .path()
        .join(".pitboss/grind/runs")
        .join(&rid)
        .join("worktrees/failed");
    assert!(
        failed_dir.is_dir(),
        "expected forensics dir at {:?}",
        failed_dir
    );
    let entries: Vec<_> = std::fs::read_dir(&failed_dir).unwrap().flatten().collect();
    assert!(
        !entries.is_empty(),
        "violating worktree should be preserved under {:?}",
        failed_dir
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_prompts_in_same_plan_serialize_one_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let rid = run_id();
    let branch = pitboss::grind::run_branch_name(&rid);

    // All non-parallel-safe — even with max_parallel=2 they should never
    // overlap because each takes the full permit count.
    let prompts = vec![
        sequential_prompt("alpha"),
        sequential_prompt("bravo"),
        sequential_prompt("charlie"),
    ];
    let plan = plan_with(&prompts, 2);

    let mut behaviors: HashMap<String, PromptBehavior> = HashMap::new();
    for name in ["alpha", "bravo", "charlie"] {
        behaviors.insert(
            name.to_string(),
            PromptBehavior {
                file_path: PathBuf::from(format!("src/{name}.rs")),
                content_template: format!("// {name} session {{seq}}\n"),
                sleep: Duration::from_millis(80),
            },
        );
    }

    let journal = Arc::new(ConcurrencyJournal::default());
    let mut runner = make_runner(
        dir.path(),
        &branch,
        &rid,
        plan,
        &prompts,
        behaviors,
        journal.clone(),
    )
    .await;

    let outcome = runner.run(GrindShutdown::new()).await.unwrap();

    assert_eq!(outcome.stop_reason, GrindStopReason::Completed);
    assert_eq!(outcome.sessions.len(), 3);
    for r in &outcome.sessions {
        assert_eq!(r.status, SessionStatus::Ok, "record: {r:?}");
    }
    assert_eq!(
        journal.max(),
        1,
        "non-parallel-safe sessions must never overlap: max_in_flight = {}",
        journal.max()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_wall_clock_is_meaningfully_less_than_sum_of_session_times() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let rid = run_id();
    let branch = pitboss::grind::run_branch_name(&rid);

    let prompts = vec![parallel_prompt("alpha"), parallel_prompt("bravo")];
    let plan = plan_with(&prompts, 2);

    // Each session sleeps 600ms inside the agent. The previous version of
    // this test compared elapsed wall-clock against `0.75 * sum(sleeps)`,
    // which flaked on slow CI hosts because per-session git overhead
    // (worktree create + ff-merge + teardown) ate into the safety margin.
    // The new assertion is overlap-driven: the journal records each
    // session's enter/leave instants, and we require their intervals to
    // overlap by at least half the per-session sleep — direct evidence of
    // concurrent execution that does not depend on summed-time arithmetic
    // or guess at the overhead budget.
    let session_sleep = Duration::from_millis(600);
    let mut behaviors: HashMap<String, PromptBehavior> = HashMap::new();
    behaviors.insert(
        "alpha".to_string(),
        PromptBehavior {
            file_path: PathBuf::from("src/alpha.rs"),
            content_template: "// alpha {seq}\n".into(),
            sleep: session_sleep,
        },
    );
    behaviors.insert(
        "bravo".to_string(),
        PromptBehavior {
            file_path: PathBuf::from("src/bravo.rs"),
            content_template: "// bravo {seq}\n".into(),
            sleep: session_sleep,
        },
    );

    let journal = Arc::new(ConcurrencyJournal::default());
    let mut runner = make_runner(
        dir.path(),
        &branch,
        &rid,
        plan,
        &prompts,
        behaviors,
        journal.clone(),
    )
    .await;

    let start = Instant::now();
    let outcome = runner.run(GrindShutdown::new()).await.unwrap();
    let elapsed = start.elapsed();

    assert_eq!(outcome.stop_reason, GrindStopReason::Completed);
    assert_eq!(outcome.sessions.len(), 2);

    // Strong instrumentation assertion: in-flight count peaked at 2.
    assert_eq!(
        journal.max(),
        2,
        "expected two concurrent sessions: max_in_flight = {}",
        journal.max()
    );

    // Direct overlap proof: the two session intervals must overlap by at
    // least half the per-session sleep. On a host where the sessions
    // genuinely interleave, overlap lands close to `session_sleep` itself;
    // on a serialized run it would be zero.
    let overlap = journal.max_overlap();
    let min_overlap = session_sleep / 2;
    assert!(
        overlap >= min_overlap,
        "expected overlap ≥ {min_overlap:?}, got {overlap:?} (sleep = {session_sleep:?}, elapsed = {elapsed:?})"
    );

    // Cheap sanity bound: even with generous overhead, two parallel
    // sessions cannot take longer than serial execution. Loose enough that
    // a slow CI host won't flake; tight enough to catch a regression that
    // accidentally serializes the dispatch.
    let serial_upper_bound = session_sleep * 2 + Duration::from_millis(800);
    assert!(
        elapsed < serial_upper_bound,
        "elapsed {elapsed:?} ≥ serial bound {serial_upper_bound:?} — parallelism likely broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_parallel_safe_prompt_locks_every_permit_in_a_mixed_plan() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let rid = run_id();
    let branch = pitboss::grind::run_branch_name(&rid);

    // Mixed plan. The non-parallel-safe `bravo` must never run alongside
    // any other session even though the plan's max_parallel is 2.
    let prompts = vec![
        parallel_prompt("alpha"),
        sequential_prompt("bravo"),
        parallel_prompt("charlie"),
    ];
    let plan = plan_with(&prompts, 2);

    let mut behaviors: HashMap<String, PromptBehavior> = HashMap::new();
    behaviors.insert(
        "alpha".to_string(),
        PromptBehavior {
            file_path: PathBuf::from("src/alpha.rs"),
            content_template: "// alpha {seq}\n".into(),
            sleep: Duration::from_millis(60),
        },
    );
    behaviors.insert(
        "bravo".to_string(),
        PromptBehavior {
            file_path: PathBuf::from("src/bravo.rs"),
            content_template: "// bravo {seq}\n".into(),
            sleep: Duration::from_millis(60),
        },
    );
    behaviors.insert(
        "charlie".to_string(),
        PromptBehavior {
            file_path: PathBuf::from("src/charlie.rs"),
            content_template: "// charlie {seq}\n".into(),
            sleep: Duration::from_millis(60),
        },
    );

    let journal = Arc::new(ConcurrencyJournal::default());
    let mut runner = make_runner(
        dir.path(),
        &branch,
        &rid,
        plan,
        &prompts,
        behaviors,
        journal.clone(),
    )
    .await;

    let outcome = runner.run(GrindShutdown::new()).await.unwrap();

    assert_eq!(outcome.stop_reason, GrindStopReason::Completed);
    assert_eq!(outcome.sessions.len(), 3);
    for r in &outcome.sessions {
        assert_eq!(r.status, SessionStatus::Ok, "record: {r:?}");
    }
    // bravo was non-parallel-safe; its in-flight snapshot at start must
    // always be 1 (just itself).
    let bravo_snaps = journal.entries_for("bravo");
    assert!(!bravo_snaps.is_empty(), "bravo never started");
    for snap in &bravo_snaps {
        assert_eq!(
            *snap, 1,
            "bravo started with {snap} concurrent sessions in flight"
        );
    }
}
