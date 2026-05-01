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

pub mod exit_code;
pub mod fold;
pub mod grind;
pub mod init;
pub mod interview;
pub mod plan;
pub mod play;
pub mod prompts;
pub mod rebuy;
pub mod status;
pub mod sweep;

pub use exit_code::ExitCode;

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

    /// `true` when the parsed subcommand will take over the terminal with the
    /// `ratatui` dashboard. The tracing `fmt` layer must be suppressed in that
    /// mode: any `info!` / `warn!` write into the alternate screen corrupts
    /// ratatui's diff buffer (cells it thinks are unchanged stop getting
    /// repainted), so stale log chars survive across redraws and bleed
    /// through the dashboard. Drop `--tui` for log-based debugging.
    pub fn is_tui_mode(&self) -> bool {
        match &self.command {
            Command::Play { tui, .. } | Command::Rebuy { tui, .. } => *tui,
            Command::Grind(args) => args.tui,
            Command::Init
            | Command::Plan { .. }
            | Command::Status
            | Command::Fold { .. }
            | Command::Sweep(_)
            | Command::Prompts(_) => false,
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
        /// `config.toml`; either source enables the post-run PR step.
        #[arg(long)]
        pr: bool,
        /// Swap the configured agent for the deterministic `DryRunAgent`.
        /// Lets users sanity-check the plan, config, branch, and event flow
        /// end-to-end without any model spend; tests are skipped because the
        /// agent makes no edits.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Suppress deferred sweeps for this run. Clears any inherited
        /// `pending_sweep` flag at startup and refuses to arm the sweep
        /// gate from any subsequent phase commit. The override is
        /// in-memory — `pitboss.toml`'s `[sweep]` block is untouched.
        #[arg(long = "no-sweep", conflicts_with = "sweep")]
        no_sweep: bool,
        /// Force a sweep before the next phase even if the configured
        /// trigger threshold isn't met. Useful after editing
        /// `deferred.md` by hand. See `[sweep]` in `pitboss.toml` for the
        /// thresholds the override bypasses.
        #[arg(long = "sweep")]
        sweep: bool,
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
        /// Suppress deferred sweeps for this resumed run. Mirrors
        /// `pitboss play --no-sweep`.
        #[arg(long = "no-sweep", conflicts_with = "sweep")]
        no_sweep: bool,
        /// Force a sweep before the next phase. Mirrors
        /// `pitboss play --sweep`.
        #[arg(long = "sweep")]
        sweep: bool,
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
    /// Run a one-shot deferred sweep without advancing the plan. Useful
    /// after editing `deferred.md` by hand or to drain a backlog ahead of
    /// the next `pitboss play`.
    Sweep(sweep::SweepArgs),
    /// Author and inspect grind prompt files (`ls`, `validate`, `new`).
    Prompts(prompts::PromptsArgs),
    /// Rotate through grind prompts, dispatching one session per rotation
    /// onto a per-run branch (sequential MVP; parallelism arrives in
    /// phase 11).
    Grind(grind::GrindArgs),
}

/// Dispatch a parsed CLI invocation.
///
/// Most subcommands return [`ExitCode::Success`] on success and surface
/// failures through the `Err` channel; `pitboss grind` returns a richer
/// [`ExitCode`] that maps to the documented `pitboss grind` exit-code policy
/// (0 success, 1 mixed failures, 2 aborted, 3 budget hit, 4 failed to start,
/// 5 consecutive-failure escape valve).
pub async fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Init => {
            init::run(std::env::current_dir()?)?;
            Ok(ExitCode::Success)
        }
        Command::Plan {
            goal,
            force,
            interview,
        } => {
            plan::run(std::env::current_dir()?, goal, force, interview).await?;
            Ok(ExitCode::Success)
        }
        Command::Play {
            tui,
            pr,
            dry_run,
            no_sweep,
            sweep,
        } => {
            play::run(std::env::current_dir()?, tui, pr, dry_run, no_sweep, sweep).await?;
            Ok(ExitCode::Success)
        }
        Command::Status => {
            status::run(std::env::current_dir()?)?;
            Ok(ExitCode::Success)
        }
        Command::Rebuy {
            tui,
            pr,
            dry_run,
            no_sweep,
            sweep,
        } => {
            rebuy::run(std::env::current_dir()?, tui, pr, dry_run, no_sweep, sweep).await?;
            Ok(ExitCode::Success)
        }
        Command::Sweep(args) => sweep::run(std::env::current_dir()?, args).await,
        Command::Fold { checkout_original } => {
            fold::run(std::env::current_dir()?, checkout_original).await?;
            Ok(ExitCode::Success)
        }
        Command::Prompts(args) => {
            prompts::run(std::env::current_dir()?, args)?;
            Ok(ExitCode::Success)
        }
        Command::Grind(args) => grind::run(std::env::current_dir()?, args).await,
    }
}
