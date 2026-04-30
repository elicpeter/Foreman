//! Clap command definitions and dispatch.
//!
//! `init` is implemented (phase 5); `play` (phase 12), `plan` (phase 15), and
//! the lifecycle trio `status` / `rebuy` / `fold` (phase 17) round out the
//! current surface. Later phases plug into the same dispatch table.
//!
//! The verbs follow the casino theme baked into the binary name: a `play`
//! executes one phased plan to completion (one hand), `rebuy` picks the run
//! back up after a halt or fold (buying back into the table), and `fold`
//! marks the run aborted (this hand is over). For backwards compatibility
//! every renamed subcommand keeps its original name as a clap alias, so
//! `pitboss run`, `pitboss resume`, and `pitboss abort` continue to work.

use anyhow::Result;
use clap::{Parser, Subcommand};

pub mod fold;
pub mod init;
pub mod interview;
pub mod plan;
pub mod play;
pub mod rebuy;
pub mod status;

#[derive(Debug, Parser)]
#[command(
    name = "pitboss",
    version,
    about = "Orchestrate coding agents through a phased plan"
)]
pub struct Cli {
    /// Lower the log level for this invocation. `-v` enables debug output;
    /// `-vv` enables trace. Equivalent to `PITBOSS_LOG=debug` /
    /// `PITBOSS_LOG=trace`. The env var still wins when set.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Pick the `tracing-subscriber` filter directive implied by `--verbose`.
    /// Returns `None` when no `-v` was passed (caller falls back to
    /// `PITBOSS_LOG` / `RUST_LOG` / `info`).
    pub fn verbose_filter(&self) -> Option<&'static str> {
        match self.verbose {
            0 => None,
            1 => Some("debug"),
            _ => Some("trace"),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold a new pitboss workspace in the current directory.
    Init,
    /// Generate a `plan.md` for a goal using the planner agent.
    Plan {
        /// Free-form description of what to build.
        goal: String,
        /// Overwrite an existing `plan.md`. Without this flag the command
        /// refuses to clobber a hand-written file. An untouched `pitboss init`
        /// seed is replaced silently and does not require `--force`.
        #[arg(long)]
        force: bool,
        /// Conduct a design interview before generating the plan. The agent
        /// generates targeted questions about the feature; your answers are
        /// compiled into a design spec and woven into the planner prompt,
        /// producing a more precise and complete `plan.md`.
        #[arg(long)]
        interview: bool,
    },
    /// Execute the plan, advancing through phases until done or halted.
    /// Aliased as `run` for backwards compatibility.
    #[command(alias = "run")]
    Play {
        /// Render a live `ratatui` dashboard instead of the plain logger.
        #[arg(long)]
        tui: bool,
        /// After the run finishes successfully, open a pull request via
        /// `gh pr create`. Equivalent to setting `git.create_pr = true` in
        /// `pitboss.toml`; either source enables the post-run PR step.
        #[arg(long)]
        pr: bool,
        /// Swap the configured agent for the deterministic `DryRunAgent`.
        /// Lets users sanity-check the plan, config, branch, and event flow
        /// end-to-end without any model spend; tests are skipped because the
        /// agent makes no edits.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Print a summary of the current run.
    Status,
    /// Rebuy into a halted run and pick up where it left off. Aliased as
    /// `resume` for backwards compatibility.
    #[command(alias = "resume")]
    Rebuy {
        /// Render a live `ratatui` dashboard instead of the plain logger.
        #[arg(long)]
        tui: bool,
        /// After the resumed run finishes successfully, open a pull request
        /// via `gh pr create`. Mirrors `pitboss play --pr`.
        #[arg(long)]
        pr: bool,
        /// Swap the configured agent for the deterministic `DryRunAgent`.
        /// Mirrors `pitboss play --dry-run`.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Fold the active run (mark it aborted). `pitboss play` and `pitboss
    /// rebuy` refuse to operate on a folded state. Aliased as `abort` for
    /// backwards compatibility.
    #[command(alias = "abort")]
    Fold {
        /// After marking the run folded, switch HEAD back to the branch that
        /// was checked out when the run began (when known).
        #[arg(long)]
        checkout_original: bool,
    },
}

/// Dispatch a parsed CLI invocation.
pub async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init => init::run(std::env::current_dir()?),
        Command::Plan {
            goal,
            force,
            interview,
        } => plan::run(std::env::current_dir()?, goal, force, interview).await,
        Command::Play { tui, pr, dry_run } => {
            play::run(std::env::current_dir()?, tui, pr, dry_run).await
        }
        Command::Status => status::run(std::env::current_dir()?),
        Command::Rebuy { tui, pr, dry_run } => {
            rebuy::run(std::env::current_dir()?, tui, pr, dry_run).await
        }
        Command::Fold { checkout_original } => {
            fold::run(std::env::current_dir()?, checkout_original).await
        }
    }
}
