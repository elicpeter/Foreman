//! `pitboss rebuy` — buy back into a halted run from `.pitboss/state.json`.
//!
//! Rebuy reuses the runner driver from [`crate::cli::play::execute`] with
//! [`StartMode::Resume`], so the only behavioral delta from `pitboss play`
//! is that a missing or empty `state.json` is an error: there is nothing to
//! rebuy into. The per-run branch is checked out, [`crate::runner::Runner`]
//! is constructed against the loaded plan / deferred / state, and execution
//! continues from `plan.current_phase`, which the runner advanced to the
//! next phase the last time it persisted state.
//!
//! `pitboss resume` is kept as a clap alias of `pitboss rebuy` so existing
//! scripts and muscle memory continue to work.

use std::path::PathBuf;

use anyhow::Result;

use super::play::{execute, StartMode};

/// Top-level entry point for the `rebuy` subcommand. `tui` toggles the
/// `ratatui` dashboard the same way [`crate::cli::play::run`] does. `pr` opts
/// the resumed run into the post-run `gh pr create` step (see
/// [`crate::cli::play::run`]). `dry_run` mirrors `pitboss play --dry-run`:
/// the configured agent is swapped for a no-op so the resumed run can be
/// exercised end-to-end without spending tokens.
pub async fn run(workspace: PathBuf, tui: bool, pr: bool, dry_run: bool) -> Result<()> {
    execute(workspace, tui, pr, dry_run, StartMode::Resume).await
}
