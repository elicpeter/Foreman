//! `pitboss grind`: rotating prompt loop that runs sessions until folded or a
//! budget is hit.
//!
//! Grind is a separate execution path from `pitboss play`. It has no phased
//! plan, no auditor, and no fixer cycle by default — instead it rotates through
//! a set of user-authored markdown prompts (frontmatter + body) and asks the
//! agent to run one at a time. Each phase listed in `plan.md` (the project's
//! grind implementation plan, not a runtime artifact) wires in another piece:
//! discovery, scheduling, run-dir layout, hooks, parallelism, etc.
//!
//! Phases 01-02 stand up the data model and discovery. The CLI is not yet wired.

pub mod budget;
pub mod discovery;
pub mod dry_run;
pub mod hooks;
pub mod plan;
pub mod prompt;
pub mod run;
pub mod run_dir;
pub mod scheduler;
pub mod state;
pub mod templates;
pub mod worktree;

pub use budget::{
    resolve_budgets, session_cost_usd, BudgetCheck, BudgetReason, BudgetSnapshot, BudgetTracker,
    ExitCode,
};
pub use discovery::{
    discover_prompts, resolve_home_prompts_dir, DiscoveryOptions, DiscoveryResult,
};
pub use dry_run::{
    preview_picks, render_dry_run_report, DryRunInputs, DRY_RUN_HEADER, DRY_RUN_VERSION,
    PREVIEW_PICK_COUNT,
};
pub use hooks::{run_hook, HookKind, HookOutcome};
pub use plan::{
    default_plan_from_dir, load_plan, parse_plan_str, GrindPlan, Hooks, PlanBudgets, PlanLoadError,
    PlanPromptRef, PlanValidationError, DEFAULT_PLAN_NAME,
};
pub use prompt::{
    parse_prompt_file, PromptDoc, PromptMeta, PromptMetaValidationError, PromptParseError,
    PromptSource,
};
pub use run::{
    compose_user_prompt, render_grind_fixer_prompt, run_branch_name, standing_instruction_block,
    BudgetWarningKind, GrindEvent, GrindRunOutcome, GrindRunner, GrindShutdown, GrindStopReason,
    GRIND_EVENT_CHANNEL_CAPACITY,
};
pub use run_dir::{
    generate_run_id, render_sessions_md, RunDir, RunPaths, Scratchpad, SessionLog, SessionRecord,
    SessionStatus,
};
pub use scheduler::{Scheduler, SchedulerState};
pub use state::{
    build_state, diff_prompt_names, list_runs, most_recent_resumable, reconstruct_state_from_log,
    resolve_target, validate_resume, ReconciledState, ResumeError, RunListing, RunState, RunStatus,
};
pub use worktree::{
    merge_scratchpad_into_run, parallel_safe_violation_summary, session_branch_name,
    sweep_stale_session_worktrees, MergeOutcome, ParallelSafeViolationSite, SessionWorktree,
};
