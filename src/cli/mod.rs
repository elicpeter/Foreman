//! Clap command definitions and dispatch.
//!
//! `init` is implemented (phase 5); the remaining subcommands are filled in
//! across later phases (`run` in phase 12, `status`/`resume` in phase 17, etc.).

use anyhow::Result;
use clap::{Parser, Subcommand};

pub mod init;
pub mod run;

#[derive(Debug, Parser)]
#[command(
    name = "foreman",
    version,
    about = "Orchestrate coding agents through a phased plan"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold a new foreman workspace in the current directory.
    Init,
    /// Generate a `plan.md` for a goal using the planner agent.
    Plan {
        /// Free-form description of what to build.
        goal: String,
    },
    /// Execute the plan, advancing through phases until done or halted.
    Run,
    /// Print a summary of the current run.
    Status,
    /// Resume a halted run from where it left off.
    Resume,
}

/// Dispatch a parsed CLI invocation.
pub async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init => init::run(std::env::current_dir()?),
        Command::Plan { goal: _ } => unimplemented!("`foreman plan` lands in phase 15"),
        Command::Run => run::run(std::env::current_dir()?).await,
        Command::Status => unimplemented!("`foreman status` lands in phase 17"),
        Command::Resume => unimplemented!("`foreman resume` lands in phase 17"),
    }
}
