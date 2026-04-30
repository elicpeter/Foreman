//! Runner-owned state stored at `.pitboss/state.json`.
//!
//! Phase 2 introduces the type vocabulary; phase 5 wires the atomic load/save
//! helpers that read and write this struct from disk.
//!
//! The on-disk form is `Option<RunState>` so a freshly-initialized workspace —
//! no run started yet — round-trips as JSON `null`. [`load`] returns `Ok(None)`
//! for a missing file or a `null` payload; [`save`] writes either `null` or the
//! pretty-printed [`RunState`] via [`crate::util::write_atomic`].

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::plan::PhaseId;
use crate::util::write_atomic;

/// Path of the runner-owned state file inside a workspace
/// (`<workspace>/.pitboss/state.json`).
pub fn state_path(workspace: impl AsRef<Path>) -> PathBuf {
    workspace.as_ref().join(".pitboss").join("state.json")
}

/// Read the state file from a workspace.
///
/// Returns `Ok(None)` when the file is missing or holds the empty payload
/// (`null` / whitespace) — i.e., no run has started. Returns `Ok(Some(_))` when
/// a serialized [`RunState`] is present, or an error when the file is present
/// but malformed.
pub fn load(workspace: impl AsRef<Path>) -> Result<Option<RunState>> {
    let path = state_path(&workspace);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("state::load: reading {:?}", path)))
        }
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let parsed: Option<RunState> = serde_json::from_slice(&bytes)
        .with_context(|| format!("state::load: parsing {:?}", path))?;
    Ok(parsed)
}

/// Atomically write the state file. Creates the `.pitboss/` directory if it
/// does not already exist. Pass `None` to mark the workspace as having no
/// active run; the file is written as JSON `null`.
pub fn save(workspace: impl AsRef<Path>, state: Option<&RunState>) -> Result<()> {
    let path = state_path(&workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("state::save: creating {:?}", parent))?;
    }
    let mut bytes = serde_json::to_vec_pretty(&state)
        .with_context(|| format!("state::save: serializing {:?}", path))?;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(())
}

/// Per-role token counters. Aggregated into [`TokenUsage::by_role`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleUsage {
    /// Total input tokens billed for this role.
    pub input: u64,
    /// Total output tokens billed for this role.
    pub output: u64,
}

/// Aggregated token usage for a run, broken down by agent role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Total input tokens across all roles.
    pub input: u64,
    /// Total output tokens across all roles.
    pub output: u64,
    /// Per-role breakdown keyed by the role name (e.g., `"implementer"`).
    pub by_role: HashMap<String, RoleUsage>,
}

/// Persistent runner state stored at `.pitboss/state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    /// Stable identifier for this run (typically a UTC timestamp slug).
    pub run_id: String,
    /// Git branch the runner is committing to.
    pub branch: String,
    /// Branch that was checked out before the run started, captured by
    /// `pitboss play` on the fresh-run path so `pitboss fold
    /// --checkout-original` can restore it. `None` when the run was started
    /// before this field existed (older state files) or when the original
    /// branch could not be resolved (detached HEAD).
    #[serde(default)]
    pub original_branch: Option<String>,
    /// When the run was first started.
    pub started_at: DateTime<Utc>,
    /// The `current_phase` at the moment the run began.
    pub started_phase: PhaseId,
    /// Phases the runner has finished and committed.
    pub completed: Vec<PhaseId>,
    /// Number of attempts made per phase, summed across roles.
    pub attempts: HashMap<PhaseId, u32>,
    /// Aggregated token usage so far.
    pub token_usage: TokenUsage,
    /// `true` once the run has been explicitly folded via `pitboss fold`
    /// (originally named `pitboss abort`, hence the field name preserved here
    /// for backwards compatibility with state files written before the
    /// rename). `pitboss play` and `pitboss rebuy` refuse to act on a folded
    /// run; the user must clear `.pitboss/state.json` (e.g., re-run
    /// `pitboss init` after removing it) to start over.
    #[serde(default)]
    pub aborted: bool,
}

impl RunState {
    /// Build a fresh `RunState` with no completed phases and zero usage.
    pub fn new(
        run_id: impl Into<String>,
        branch: impl Into<String>,
        started_phase: PhaseId,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            branch: branch.into(),
            original_branch: None,
            started_at: Utc::now(),
            started_phase,
            completed: Vec::new(),
            attempts: HashMap::new(),
            token_usage: TokenUsage::default(),
            aborted: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    #[test]
    fn round_trips_through_json() {
        let mut by_role = HashMap::new();
        by_role.insert(
            "implementer".to_string(),
            RoleUsage {
                input: 1234,
                output: 567,
            },
        );
        by_role.insert(
            "auditor".to_string(),
            RoleUsage {
                input: 200,
                output: 50,
            },
        );

        let mut attempts = HashMap::new();
        attempts.insert(pid("02"), 1);
        attempts.insert(pid("10b"), 3);

        let state = RunState {
            run_id: "20260429T143022Z".into(),
            branch: "pitboss/run-20260429T143022Z".into(),
            original_branch: Some("main".into()),
            started_at: DateTime::parse_from_rfc3339("2026-04-29T14:30:22Z")
                .unwrap()
                .with_timezone(&Utc),
            started_phase: pid("02"),
            completed: vec![pid("01")],
            attempts,
            token_usage: TokenUsage {
                input: 1434,
                output: 617,
                by_role,
            },
            aborted: false,
        };

        let json = serde_json::to_string(&state).unwrap();
        let back: RunState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn deserializes_legacy_state_without_new_fields() {
        // Older state.json files predate `original_branch` and `aborted`.
        // Both must default cleanly so a workspace started under an earlier
        // pitboss build resumes under this one without manual surgery.
        let legacy = serde_json::json!({
            "run_id": "rid",
            "branch": "br",
            "started_at": "2026-04-29T14:30:22Z",
            "started_phase": "01",
            "completed": [],
            "attempts": {},
            "token_usage": {"input": 0, "output": 0, "by_role": {}}
        });
        let state: RunState = serde_json::from_value(legacy).unwrap();
        assert_eq!(state.original_branch, None);
        assert!(!state.aborted);
    }

    #[test]
    fn new_initializes_empty_aggregates() {
        let s = RunState::new("rid", "branch", pid("01"));
        assert_eq!(s.run_id, "rid");
        assert_eq!(s.branch, "branch");
        assert!(s.completed.is_empty());
        assert!(s.attempts.is_empty());
        assert_eq!(s.token_usage.input, 0);
        assert_eq!(s.token_usage.output, 0);
        assert!(s.token_usage.by_role.is_empty());
    }

    #[test]
    fn phase_id_is_usable_as_map_key_through_serde() {
        // Regression guard: HashMap<PhaseId, _> must round-trip through JSON.
        let mut attempts = HashMap::new();
        attempts.insert(pid("01"), 2);
        let json = serde_json::to_string(&attempts).unwrap();
        let back: HashMap<PhaseId, u32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get(&pid("01")), Some(&2));
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        // No `.pitboss/` at all.
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn save_none_then_load_round_trips_to_none() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), None).unwrap();
        let path = state_path(dir.path());
        assert!(path.exists(), "state.json should be created by save()");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.trim_end() == "null",
            "expected JSON null, got {:?}",
            contents
        );
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn save_some_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let state = RunState::new("rid", "branch", pid("02"));
        save(dir.path(), Some(&state)).unwrap();
        let loaded = load(dir.path()).unwrap().expect("expected Some(RunState)");
        assert_eq!(loaded, state);
    }

    #[test]
    fn load_returns_none_for_whitespace_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "  \n\t\n").unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn load_surfaces_parse_error_for_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not valid json").unwrap();
        let err = load(dir.path()).unwrap_err();
        assert!(err.to_string().contains("state::load: parsing"));
    }
}
