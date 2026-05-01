//! On-disk layout for a single grind run, plus thread-safe writers for the
//! session log and a read-only handle for the agent-owned scratchpad.
//!
//! A run lives at `.pitboss/grind/<run-id>/` and owns:
//!
//! - `state.json` — scheduler/budget snapshot written by phase 09.
//! - `sessions.jsonl` — append-only source of truth for session records.
//! - `sessions.md` — markdown projection re-rendered after every append.
//! - `scratchpad.md` — agent-owned freeform notes; pitboss only reads it.
//! - `transcripts/session-NNNN.log` — per-session agent output.
//! - `worktrees/` — per-session worktrees for parallel sessions (phase 11).
//!
//! `sessions.jsonl` is the single source of truth. `sessions.md` is rebuilt
//! from the full JSONL stream on every append, so the markdown projection
//! cannot drift from the log. Both writes happen under a single in-process
//! [`std::sync::Mutex`] so partial state is never observable to other tasks
//! within the same pitboss process.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::git::CommitId;
use crate::state::TokenUsage;
use crate::util::write_atomic;

/// File and directory names that make up a run directory. Centralized so the
/// layout is described in exactly one place.
pub const STATE_FILENAME: &str = "state.json";
pub const SESSIONS_JSONL: &str = "sessions.jsonl";
pub const SESSIONS_MD: &str = "sessions.md";
pub const SCRATCHPAD_MD: &str = "scratchpad.md";
pub const TRANSCRIPTS_DIR: &str = "transcripts";
pub const WORKTREES_DIR: &str = "worktrees";

/// Resolved on-disk paths for a run. Cloning is cheap and intentional — the
/// session log, scratchpad, and (later) state writer all carry their own copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPaths {
    /// `<repo>/.pitboss/grind/<run-id>/`.
    pub root: PathBuf,
    /// `<root>/state.json`.
    pub state: PathBuf,
    /// `<root>/sessions.jsonl`.
    pub sessions_jsonl: PathBuf,
    /// `<root>/sessions.md`.
    pub sessions_md: PathBuf,
    /// `<root>/scratchpad.md`.
    pub scratchpad: PathBuf,
    /// `<root>/transcripts/`.
    pub transcripts: PathBuf,
    /// `<root>/worktrees/`.
    pub worktrees: PathBuf,
}

impl RunPaths {
    /// Build the path set for a run id under `repo_root`. Performs no IO.
    pub fn for_run(repo_root: &Path, run_id: &str) -> Self {
        let root = repo_root.join(".pitboss").join("grind").join(run_id);
        Self {
            state: root.join(STATE_FILENAME),
            sessions_jsonl: root.join(SESSIONS_JSONL),
            sessions_md: root.join(SESSIONS_MD),
            scratchpad: root.join(SCRATCHPAD_MD),
            transcripts: root.join(TRANSCRIPTS_DIR),
            worktrees: root.join(WORKTREES_DIR),
            root,
        }
    }

    /// Conventional transcript path for a session sequence number, e.g.
    /// `transcripts/session-0001.log` for `seq = 1`.
    pub fn transcript_for(&self, seq: u32) -> PathBuf {
        self.transcripts.join(format!("session-{seq:04}.log"))
    }
}

/// Resolved status of a finished session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// Agent exited cleanly and any per-prompt verify step (phase 07) passed.
    Ok,
    /// Agent reported an error or post-hoc verification failed.
    Error,
    /// The session was killed by a timeout (per-prompt or budget).
    Timeout,
    /// User-driven abort (e.g., second Ctrl-C from phase 07).
    Aborted,
    /// Agent left uncommitted leftovers in the working tree which pitboss
    /// stashed into a labeled stash for morning triage. The work itself was
    /// otherwise valid; the dirty status is a hint that triage is needed.
    Dirty,
}

impl SessionStatus {
    /// Lower-case label used by `sessions.md` and exposed to plan-level hooks
    /// via `PITBOSS_SESSION_STATUS`. Stable string — scripts read it.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Ok => "ok",
            SessionStatus::Error => "error",
            SessionStatus::Timeout => "timeout",
            SessionStatus::Aborted => "aborted",
            SessionStatus::Dirty => "dirty",
        }
    }
}

/// Source-of-truth record for a single session. Each call to
/// [`SessionLog::append`] writes one of these as a JSONL line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// 1-based sequence number within the run.
    pub seq: u32,
    /// The owning run's id.
    pub run_id: String,
    /// Name of the prompt dispatched in this session.
    pub prompt: String,
    /// Wall-clock start time.
    pub started_at: DateTime<Utc>,
    /// Wall-clock end time.
    pub ended_at: DateTime<Utc>,
    /// Resolved outcome.
    pub status: SessionStatus,
    /// Captured agent summary (or [`None`] if the agent produced none).
    pub summary: Option<String>,
    /// Commit landed on the run branch by this session, when one was created.
    pub commit: Option<CommitId>,
    /// Token usage attributed to this session.
    pub tokens: TokenUsage,
    /// Cost in USD attributed to this session.
    pub cost_usd: f64,
    /// Path to the session's transcript file. Stored as written by the caller
    /// — typically a path relative to the run root.
    pub transcript_path: PathBuf,
}

/// Append-only session log. The [`SessionLog::append`] entry point holds an
/// in-process mutex so the JSONL append and markdown re-render run as one
/// indivisible step. Cloning is cheap and shares the underlying lock.
#[derive(Debug, Clone)]
pub struct SessionLog {
    paths: RunPaths,
    lock: Arc<Mutex<()>>,
}

impl SessionLog {
    fn new(paths: RunPaths) -> Self {
        Self {
            paths,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Resolved paths for the run this log writes to.
    pub fn paths(&self) -> &RunPaths {
        &self.paths
    }

    /// Append a session record.
    ///
    /// Steps, all under a single in-process mutex:
    /// 1. Append one JSON line to `sessions.jsonl` and `fsync`.
    /// 2. Read the full JSONL stream back, parse each line.
    /// 3. Render markdown via [`render_sessions_md`].
    /// 4. Atomically replace `sessions.md`.
    pub fn append(&self, record: &SessionRecord) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("session log mutex poisoned"))?;

        let mut line = serde_json::to_string(record)
            .with_context(|| "session log: serializing record".to_string())?;
        line.push('\n');

        {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.paths.sessions_jsonl)
                .with_context(|| {
                    format!(
                        "session log: opening {:?} for append",
                        self.paths.sessions_jsonl
                    )
                })?;
            f.write_all(line.as_bytes())
                .with_context(|| format!("session log: writing {:?}", self.paths.sessions_jsonl))?;
            f.sync_data()
                .with_context(|| format!("session log: fsync {:?}", self.paths.sessions_jsonl))?;
        }

        let records = self.read_records_locked()?;
        let md = render_sessions_md(&records);
        write_atomic(&self.paths.sessions_md, md.as_bytes())?;
        Ok(())
    }

    /// Read all records currently persisted. Acquires the same mutex as
    /// [`SessionLog::append`] so a concurrent append cannot interleave a
    /// half-written line.
    pub fn records(&self) -> Result<Vec<SessionRecord>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("session log mutex poisoned"))?;
        self.read_records_locked()
    }

    fn read_records_locked(&self) -> Result<Vec<SessionRecord>> {
        let raw = match fs::read_to_string(&self.paths.sessions_jsonl) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "session log: reading {:?}",
                    self.paths.sessions_jsonl
                )))
            }
        };
        let mut out = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let rec: SessionRecord = serde_json::from_str(line).with_context(|| {
                format!(
                    "session log: parsing {:?} line {}",
                    self.paths.sessions_jsonl,
                    i + 1
                )
            })?;
            out.push(rec);
        }
        Ok(out)
    }
}

/// Render the markdown projection of `sessions.jsonl`. Pure function so tests
/// can pin the format with `insta` independently of any IO.
pub fn render_sessions_md(records: &[SessionRecord]) -> String {
    let mut out = String::new();
    out.push_str("# Sessions\n\n");
    if records.is_empty() {
        out.push_str("_No sessions recorded yet._\n");
        return out;
    }
    for r in records {
        out.push_str(&format!(
            "## session-{:04} — {} ({})\n\n",
            r.seq,
            r.prompt,
            r.status.as_str()
        ));
        out.push_str(&format!(
            "- started: {}\n",
            r.started_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ));
        out.push_str(&format!(
            "- ended: {}\n",
            r.ended_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ));
        let secs = (r.ended_at - r.started_at).num_seconds().max(0);
        out.push_str(&format!("- duration: {secs}s\n"));
        match &r.commit {
            Some(c) => {
                let s = c.as_str();
                let short = if s.len() >= 7 { &s[..7] } else { s };
                out.push_str(&format!("- commit: {short}\n"));
            }
            None => out.push_str("- commit: (none)\n"),
        }
        let total = r.tokens.input + r.tokens.output;
        out.push_str(&format!(
            "- tokens: {} (in {} / out {})\n",
            total, r.tokens.input, r.tokens.output
        ));
        out.push_str(&format!("- cost: ${:.4}\n", r.cost_usd));
        out.push_str(&format!("- transcript: {}\n", r.transcript_path.display()));
        match &r.summary {
            Some(s) if !s.is_empty() => {
                out.push_str("- summary:\n\n");
                for line in s.lines() {
                    out.push_str("    ");
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }
            _ => out.push_str("- summary: (none)\n\n"),
        }
    }
    out
}

/// Read-only handle to the per-run scratchpad. The agent subprocess reads
/// and writes the file directly; pitboss never edits it from this side.
#[derive(Debug, Clone)]
pub struct Scratchpad {
    path: PathBuf,
}

impl Scratchpad {
    /// Build a handle pointing at `<run-root>/scratchpad.md`.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read the current scratchpad contents.
    pub fn read(&self) -> Result<String> {
        fs::read_to_string(&self.path)
            .with_context(|| format!("scratchpad: reading {:?}", self.path))
    }

    /// Path the agent should be told to read or write. Pitboss exposes this
    /// to the agent via env var (`PITBOSS_SCRATCHPAD`, phase 07).
    pub fn path_for_agent(&self) -> &Path {
        &self.path
    }
}

/// Owning handle for an open run directory. Combines the resolved
/// [`RunPaths`], the [`SessionLog`] writer, and the [`Scratchpad`] reader.
#[derive(Debug, Clone)]
pub struct RunDir {
    paths: RunPaths,
    log: SessionLog,
    scratchpad: Scratchpad,
}

impl RunDir {
    /// Create the run directory tree on disk. Returns an error if the run
    /// directory already exists — runs are immutable once created, and a
    /// resume path goes through [`RunDir::open`].
    pub fn create(repo_root: &Path, run_id: &str) -> Result<Self> {
        let paths = RunPaths::for_run(repo_root, run_id);
        if paths.root.exists() {
            return Err(anyhow!("grind run dir {:?} already exists", paths.root));
        }
        fs::create_dir_all(&paths.root)
            .with_context(|| format!("create_dir_all {:?}", paths.root))?;
        fs::create_dir_all(&paths.transcripts)
            .with_context(|| format!("create_dir_all {:?}", paths.transcripts))?;
        fs::create_dir_all(&paths.worktrees)
            .with_context(|| format!("create_dir_all {:?}", paths.worktrees))?;
        // Seed sessions.jsonl as an empty file so callers can rely on its
        // existence; sessions.md mirrors the empty stream.
        write_atomic(&paths.sessions_jsonl, b"")?;
        write_atomic(&paths.sessions_md, render_sessions_md(&[]).as_bytes())?;
        // Scratchpad is created empty; the agent owns its content from here.
        write_atomic(&paths.scratchpad, b"")?;
        Ok(Self::from_paths(paths))
    }

    /// Open an existing run directory. Errors if the directory or any of the
    /// expected files are missing.
    pub fn open(repo_root: &Path, run_id: &str) -> Result<Self> {
        let paths = RunPaths::for_run(repo_root, run_id);
        if !paths.root.is_dir() {
            return Err(anyhow!("grind run dir {:?} does not exist", paths.root));
        }
        for required in [&paths.sessions_jsonl, &paths.sessions_md, &paths.scratchpad] {
            if !required.exists() {
                return Err(anyhow!(
                    "grind run dir {:?} is missing required file {:?}",
                    paths.root,
                    required
                ));
            }
        }
        if !paths.transcripts.is_dir() {
            return Err(anyhow!(
                "grind run dir {:?} is missing transcripts/",
                paths.root
            ));
        }
        Ok(Self::from_paths(paths))
    }

    fn from_paths(paths: RunPaths) -> Self {
        let log = SessionLog::new(paths.clone());
        let scratchpad = Scratchpad::new(paths.scratchpad.clone());
        Self {
            paths,
            log,
            scratchpad,
        }
    }

    /// Resolved on-disk paths for this run.
    pub fn paths(&self) -> &RunPaths {
        &self.paths
    }

    /// The append-only session log. Cheap to clone across tasks.
    pub fn log(&self) -> &SessionLog {
        &self.log
    }

    /// The agent-facing scratchpad handle.
    pub fn scratchpad(&self) -> &Scratchpad {
        &self.scratchpad
    }
}

/// Generate a fresh run id of the form `<utc-timestamp>-<4-hex>`, e.g.
/// `20260430T180522Z-1a2b`. Mirrors the existing `%Y%m%dT%H%M%SZ` style used
/// by `pitboss play`'s [`crate::runner::fresh_run_state`] and adds four hex
/// characters of suffix so two ids generated in the same second don't collide
/// (the `pitboss play` runner only ever produces one id per process; grind
/// will produce many in tight succession during tests and possibly from
/// multiple worktrees).
pub fn generate_run_id() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    // Seed the counter once with the low 16 bits of the current nanosecond
    // clock so the suffix is unpredictable across processes; subsequent calls
    // within the same process simply increment, guaranteeing in-process
    // uniqueness up to 2^16 ids.
    let raw = COUNTER.fetch_add(1, Ordering::Relaxed);
    let suffix = if raw == 0 {
        let nanos = Utc::now().timestamp_subsec_nanos();
        let seed = nanos & 0xFFFF;
        // Fold the seed into the counter so that the next caller continues
        // from there, not from 1.
        COUNTER.store(seed.wrapping_add(1), Ordering::Relaxed);
        seed
    } else {
        raw & 0xFFFF
    };
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ");
    format!("{ts}-{suffix:04x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn fixture_record(seq: u32, run_id: &str, status: SessionStatus) -> SessionRecord {
        let started: DateTime<Utc> = "2026-04-30T18:05:00Z".parse().unwrap();
        let ended: DateTime<Utc> = "2026-04-30T18:06:30Z".parse().unwrap();
        let mut by_role = std::collections::HashMap::new();
        by_role.insert(
            "implementer".to_string(),
            crate::state::RoleUsage {
                input: 1000,
                output: 500,
            },
        );
        let tokens = TokenUsage {
            input: 1000,
            output: 500,
            by_role,
        };
        SessionRecord {
            seq,
            run_id: run_id.to_string(),
            prompt: "fp-hunter".to_string(),
            started_at: started,
            ended_at: ended,
            status,
            summary: Some("fixed the foo bug in bar.rs".to_string()),
            commit: Some(CommitId::new(format!("abc1234{seq:033}"))),
            tokens,
            cost_usd: 0.0123,
            transcript_path: PathBuf::from(format!("transcripts/session-{seq:04}.log")),
        }
    }

    #[test]
    fn create_lays_out_expected_files_and_dirs() {
        let repo = tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "20260430T180000Z-aaaa").unwrap();
        let p = dir.paths();
        assert!(p.root.is_dir());
        assert!(p.sessions_jsonl.is_file());
        assert!(p.sessions_md.is_file());
        assert!(p.scratchpad.is_file());
        assert!(p.transcripts.is_dir());
        assert!(p.worktrees.is_dir());
        // sessions.jsonl seeded empty.
        assert_eq!(fs::read(&p.sessions_jsonl).unwrap(), Vec::<u8>::new());
        // scratchpad starts empty.
        assert_eq!(dir.scratchpad().read().unwrap(), "");
    }

    #[test]
    fn create_refuses_when_directory_already_exists() {
        let repo = tempdir().unwrap();
        RunDir::create(repo.path(), "rid").unwrap();
        let err = RunDir::create(repo.path(), "rid").unwrap_err();
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }

    #[test]
    fn open_rejects_missing_directory() {
        let repo = tempdir().unwrap();
        let err = RunDir::open(repo.path(), "no-such-run").unwrap_err();
        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    #[test]
    fn open_finds_an_existing_run() {
        let repo = tempdir().unwrap();
        RunDir::create(repo.path(), "rid").unwrap();
        let opened = RunDir::open(repo.path(), "rid").unwrap();
        assert_eq!(opened.paths().root, repo.path().join(".pitboss/grind/rid"));
    }

    #[test]
    fn append_round_trips_through_jsonl() {
        let repo = tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "rid").unwrap();
        let log = dir.log();

        let r1 = fixture_record(1, "rid", SessionStatus::Ok);
        let r2 = fixture_record(2, "rid", SessionStatus::Error);
        log.append(&r1).unwrap();
        log.append(&r2).unwrap();

        let back = log.records().unwrap();
        assert_eq!(back, vec![r1, r2]);

        // Each line in sessions.jsonl is a single record.
        let raw = fs::read_to_string(&dir.paths().sessions_jsonl).unwrap();
        assert_eq!(raw.lines().count(), 2);
        for line in raw.lines() {
            let _: SessionRecord = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn append_rerenders_sessions_md_from_full_stream() {
        let repo = tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "rid").unwrap();
        let log = dir.log();

        let r1 = fixture_record(1, "rid", SessionStatus::Ok);
        let r2 = fixture_record(2, "rid", SessionStatus::Aborted);
        log.append(&r1).unwrap();
        let after_one = fs::read_to_string(&dir.paths().sessions_md).unwrap();
        log.append(&r2).unwrap();
        let after_two = fs::read_to_string(&dir.paths().sessions_md).unwrap();

        // The second render must include both sessions, not just the latest.
        assert!(after_one.contains("session-0001"));
        assert!(!after_one.contains("session-0002"));
        assert!(after_two.contains("session-0001"));
        assert!(after_two.contains("session-0002"));

        // And it must equal the pure render over the full record list.
        let expected = render_sessions_md(&[r1, r2]);
        assert_eq!(after_two, expected);
    }

    #[test]
    fn render_sessions_md_empty_snapshot() {
        let s = render_sessions_md(&[]);
        insta::assert_snapshot!("sessions_md_empty", s);
    }

    #[test]
    fn render_sessions_md_two_rows_snapshot() {
        let r1 = fixture_record(1, "rid", SessionStatus::Ok);
        let mut r2 = fixture_record(2, "rid", SessionStatus::Error);
        r2.summary = None;
        r2.commit = None;
        let s = render_sessions_md(&[r1, r2]);
        insta::assert_snapshot!("sessions_md_two_rows", s);
    }

    #[test]
    fn generate_run_id_is_unique_within_process() {
        let mut seen: HashSet<String> = HashSet::new();
        for _ in 0..200 {
            let id = generate_run_id();
            assert!(id.contains('-'), "run id should have a hex suffix: {id}");
            let suffix = id.rsplit('-').next().unwrap();
            assert_eq!(suffix.len(), 4, "suffix should be 4 hex chars: {id}");
            assert!(
                suffix.chars().all(|c| c.is_ascii_hexdigit()),
                "suffix should be hex: {id}"
            );
            assert!(seen.insert(id), "run id collision");
        }
    }

    #[test]
    fn transcript_for_uses_zero_padded_seq() {
        let p = RunPaths::for_run(Path::new("/tmp/repo"), "rid");
        assert_eq!(
            p.transcript_for(7),
            Path::new("/tmp/repo/.pitboss/grind/rid/transcripts/session-0007.log")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fifty_concurrent_appends_produce_fifty_lines() {
        let repo = tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "rid").unwrap();
        let log = Arc::new(dir.log().clone());

        let mut joins = Vec::new();
        for seq in 1..=50u32 {
            let log = log.clone();
            joins.push(tokio::task::spawn_blocking(move || {
                let rec = SessionRecord {
                    seq,
                    run_id: "rid".to_string(),
                    prompt: format!("p{seq}"),
                    started_at: "2026-04-30T18:05:00Z".parse().unwrap(),
                    ended_at: "2026-04-30T18:05:01Z".parse().unwrap(),
                    status: SessionStatus::Ok,
                    summary: Some(format!("session {seq} summary")),
                    commit: None,
                    tokens: TokenUsage::default(),
                    cost_usd: 0.0,
                    transcript_path: PathBuf::from(format!("transcripts/session-{seq:04}.log")),
                };
                log.append(&rec)
            }));
        }
        for j in joins {
            j.await.unwrap().unwrap();
        }

        let raw = fs::read_to_string(&dir.paths().sessions_jsonl).unwrap();
        assert_eq!(raw.lines().count(), 50);
        // Every line must be valid JSON — proves no two appends interleaved.
        let mut seqs: Vec<u32> = Vec::new();
        for line in raw.lines() {
            let rec: SessionRecord = serde_json::from_str(line).unwrap();
            seqs.push(rec.seq);
        }
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=50u32).collect::<Vec<_>>());
    }

    #[test]
    fn scratchpad_reads_what_the_agent_wrote() {
        let repo = tempdir().unwrap();
        let dir = RunDir::create(repo.path(), "rid").unwrap();
        let pad = dir.scratchpad();
        // Agent writes directly; pitboss reads.
        fs::write(pad.path_for_agent(), "agent jotted this down").unwrap();
        assert_eq!(pad.read().unwrap(), "agent jotted this down");
    }
}
