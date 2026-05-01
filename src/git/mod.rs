//! Git integration via a trait + shell-out implementation.
//!
//! The runner uses [`Git`] for every git operation it performs: checking the
//! working tree, creating and switching branches, staging the per-phase set of
//! changes, committing, and reading diff stats. Two implementations ship:
//!
//! - [`ShellGit`] shells out to the local `git` CLI. Production default.
//! - [`MockGit`] is an in-memory stub used by runner tests; it records every
//!   call and lets tests assert that the runner is passing the right exclusion
//!   set on `stage_changes`.
//!
//! The trait surface is intentionally narrow: pitboss never resolves merges,
//! talks to remotes, or rewrites history — it only lands per-phase commits
//! onto a fresh per-run branch. Adding scope here later means adding methods,
//! never reshaping the existing ones.

pub mod mock;
pub mod pr;
pub mod shell;

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::plan::PhaseId;

pub use mock::{MockGit, MockOp};
pub use pr::{grind_pr_title, open_grind_pr, pr_body, pr_title, PrSummary};
pub use shell::ShellGit;

/// A git commit hash (full SHA-1 hex from `git rev-parse HEAD`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommitId(String);

impl CommitId {
    /// Wrap a hash string. The value is not validated; trust the source.
    pub fn new(hash: impl Into<String>) -> Self {
        Self(hash.into())
    }

    /// Borrow the underlying hash string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Summary of changes between two refs as reported by `git diff --shortstat`.
/// Zero-valued when the range is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffStat {
    /// Number of files changed in the range.
    pub files_changed: u64,
    /// Total lines inserted across the range.
    pub insertions: u64,
    /// Total lines deleted across the range.
    pub deletions: u64,
}

/// Errors that originate inside the git layer. Most callers carry an
/// `anyhow::Result`; the typed enum exists so the runner can distinguish, for
/// example, "command failed" from "output was unparsable".
#[derive(Debug, Error)]
pub enum GitError {
    /// A `git` invocation exited with a non-success status.
    #[error("git {operation} failed (exit {exit:?}): {stderr}")]
    Command {
        /// Short identifier of the operation (e.g., `"status"`, `"commit"`).
        operation: String,
        /// Process exit code, when known.
        exit: Option<i32>,
        /// Captured stderr — short enough to surface verbatim in error chains.
        stderr: String,
    },
    /// A `git` invocation succeeded but produced output we couldn't parse.
    #[error("git {operation}: unexpected output: {output}")]
    UnexpectedOutput {
        /// Operation name, mirroring [`GitError::Command::operation`].
        operation: String,
        /// The unparsable output, trimmed.
        output: String,
    },
}

/// Narrow git surface used by the runner.
///
/// All methods take `&self`; impls handle their own interior synchronization
/// so the runner can hand the same `Git` to multiple roles in sequence.
#[async_trait]
pub trait Git: Send + Sync {
    /// `true` when the working tree has no untracked or modified paths.
    async fn is_clean(&self) -> Result<bool>;

    /// Currently checked-out branch. Errors if HEAD is detached or unborn.
    async fn current_branch(&self) -> Result<String>;

    /// Create `name` pointing at the current HEAD. Does not switch to it.
    async fn create_branch(&self, name: &str) -> Result<()>;

    /// Switch HEAD to `name`. Fails if the working tree has incompatible
    /// changes.
    async fn checkout(&self, name: &str) -> Result<()>;

    /// Stage every untracked or modified path **except** those in `exclude`.
    ///
    /// Each excluded path becomes a `:!<path>` git pathspec, applied by git
    /// itself rather than by pitboss walking the tree. The runner always
    /// passes `plan.md`, `deferred.md`, and `.pitboss` so per-phase commits
    /// stay scoped to code; the trait keeps the parameter generic so tests
    /// can verify that contract.
    async fn stage_changes(&self, exclude: &[&Path]) -> Result<()>;

    /// `true` if the index differs from HEAD — i.e., a `commit` would produce
    /// a non-empty commit. The runner consults this before committing so a
    /// phase that only modified excluded paths produces no commit.
    async fn has_staged_changes(&self) -> Result<bool>;

    /// Commit the current index with `message` and return the resulting
    /// commit's id. Fails if the index has no staged changes.
    async fn commit(&self, message: &str) -> Result<CommitId>;

    /// Summary of `git diff --shortstat <from>..<to>`. An empty range
    /// resolves to [`DiffStat::default`].
    async fn diff_stat(&self, from: &str, to: &str) -> Result<DiffStat>;

    /// Unified diff of the index against `HEAD`, as produced by
    /// `git diff --cached`. The runner uses this to feed the auditor agent the
    /// changes the implementer (and any fixer attempts) just produced before
    /// they're committed; staging excluded paths via [`Git::stage_changes`]
    /// keeps planning artifacts out of the diff. An empty index produces an
    /// empty string.
    async fn staged_diff(&self) -> Result<String>;

    /// Stash any uncommitted changes (including untracked files) into a stash
    /// labeled `message`, optionally restricting the stash to paths *outside*
    /// `exclude` via `:!<path>` pathspecs. Returns `true` when something was
    /// actually stashed, `false` when the working tree had nothing to capture.
    /// Used by the grind runner to preserve an agent's leftover edits at
    /// session end so they survive into a morning triage rather than being
    /// discarded — while keeping pitboss's own bookkeeping under `.pitboss/`
    /// out of the stash.
    async fn stash_push(&self, message: &str, exclude: &[&Path]) -> Result<bool>;

    /// Open a pull request via `gh pr create` for the current branch.
    ///
    /// Returns the URL `gh` prints on stdout (e.g.,
    /// `https://github.com/owner/repo/pull/42`). The branch must already be
    /// pushed to a remote with `gh` configured for the repo; pitboss does not
    /// push or create remotes itself. Implementations are expected to invoke
    /// `gh pr create` with `--fill-first`-equivalent metadata via `--title`
    /// and `--body` so the call is non-interactive.
    async fn open_pr(&self, title: &str, body: &str) -> Result<String>;

    /// Create a new worktree at `path` and check out a fresh `branch` off
    /// `base_branch`. Used by the grind parallel runner so each parallel
    /// session can commit in isolation without contending for the main
    /// workspace's index.
    async fn add_worktree(&self, path: &Path, branch: &str, base_branch: &str) -> Result<()>;

    /// Remove the worktree at `path` and its administrative bookkeeping.
    /// Best-effort — succeeds when the worktree is already gone so callers
    /// can use this both as the happy-path teardown and as a forensics
    /// quarantine cleanup.
    async fn remove_worktree(&self, path: &Path) -> Result<()>;

    /// Force-delete a branch, even when it is not merged anywhere. Used by
    /// the parallel runner to drop ephemeral session branches once their
    /// commits have been folded back onto the run branch.
    async fn delete_branch(&self, branch: &str) -> Result<()>;

    /// `git merge --ff-only <source>` against the currently checked-out
    /// branch. Errors when a true merge would be required so the caller can
    /// surface a clear "parallel_safe contract violated" message instead of
    /// silently producing a merge commit.
    async fn merge_ff_only(&self, source_branch: &str) -> Result<()>;
}

/// Build a per-run branch name from a prefix and a UTC timestamp.
///
/// The output is `<prefix>YYYYMMDDTHHMMSSZ`, so a prefix of `pitboss/run-`
/// yields `pitboss/run-20260429T143022Z`. Format intentionally has no
/// separators inside the timestamp so the resulting branch is git-safe (no
/// colons, no slashes beyond the prefix the user chose).
pub fn branch_name(prefix: &str, at: DateTime<Utc>) -> String {
    format!("{}{}", prefix, at.format("%Y%m%dT%H%M%SZ"))
}

/// Build the per-phase commit subject. Format: `[pitboss] phase <id>: <title>`.
pub fn commit_message(phase_id: &PhaseId, title: &str) -> String {
    format!("[pitboss] phase {}: {}", phase_id, title)
}

/// Build the per-sweep commit subject. Format:
/// `[pitboss] sweep after phase <id>: <n> deferred items resolved`, with the
/// `n=0` case rendered as `[pitboss] sweep after phase <id>: no items resolved`.
/// Distinct from [`commit_message`] so log-greppers can tell sweep commits
/// apart from regular phase commits at a glance.
pub fn commit_message_sweep(after: &PhaseId, resolved: usize) -> String {
    if resolved == 0 {
        format!("[pitboss] sweep after phase {}: no items resolved", after)
    } else {
        format!(
            "[pitboss] sweep after phase {}: {} deferred items resolved",
            after, resolved
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    #[test]
    fn branch_name_formats_timestamp_compactly() {
        let at = DateTime::parse_from_rfc3339("2026-04-29T14:30:22Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            branch_name("pitboss/play/", at),
            "pitboss/play/20260429T143022Z"
        );
        assert_eq!(branch_name("", at), "20260429T143022Z");
    }

    #[test]
    fn commit_message_uses_canonical_format() {
        assert_eq!(
            commit_message(&pid("02"), "Domain types"),
            "[pitboss] phase 02: Domain types"
        );
        assert_eq!(
            commit_message(&pid("10b"), "Followup"),
            "[pitboss] phase 10b: Followup"
        );
    }

    #[test]
    fn commit_message_sweep_uses_canonical_format() {
        assert_eq!(
            commit_message_sweep(&pid("02"), 4),
            "[pitboss] sweep after phase 02: 4 deferred items resolved"
        );
        assert_eq!(
            commit_message_sweep(&pid("10b"), 0),
            "[pitboss] sweep after phase 10b: no items resolved"
        );
    }

    #[test]
    fn commit_id_round_trips_through_display() {
        let id = CommitId::new("abc123");
        assert_eq!(id.as_str(), "abc123");
        assert_eq!(format!("{}", id), "abc123");
    }
}
