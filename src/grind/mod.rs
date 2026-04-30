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

pub mod discovery;
pub mod plan;
pub mod prompt;
pub mod scheduler;
pub mod templates;

pub use discovery::{
    discover_prompts, resolve_home_prompts_dir, DiscoveryOptions, DiscoveryResult,
};
pub use plan::{
    default_plan_from_dir, load_plan, parse_plan_str, GrindPlan, Hooks, PlanBudgets, PlanLoadError,
    PlanPromptRef, PlanValidationError, DEFAULT_PLAN_NAME,
};
pub use prompt::{
    parse_prompt_file, PromptDoc, PromptMeta, PromptMetaValidationError, PromptParseError,
    PromptSource,
};
pub use scheduler::{Scheduler, SchedulerState};
