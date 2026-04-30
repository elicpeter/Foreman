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
//! Phase 01 only stands up the data model. The CLI is not yet wired.

pub mod discovery;
pub mod prompt;

pub use discovery::{discover_prompts, resolve_home_prompts_dir, DiscoveryOptions, DiscoveryResult};
pub use prompt::{
    parse_prompt_file, PromptDoc, PromptMeta, PromptMetaValidationError, PromptParseError,
    PromptSource,
};
