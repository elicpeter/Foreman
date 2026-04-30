---
current_phase: "01"
---

This plan adds a new `pitboss grind` execution path: a rotating prompt loop that runs sessions until folded or until a budget is hit, sharing a session log, scratchpad, and per-run directory. It is a fundamentally different path from `pitboss play` (no phased plan, no fixer/auditor cycle by default), so the implementation lives under a new `src/grind/` module rather than retrofitting the runner. Each phase ends with `cargo test` and `cargo clippy --all-targets -- -D warnings` green; clippy and rustfmt match the existing crate.

# Phase 01: Scaffold grind module and prompt frontmatter

**Scope.** Stand up the empty `src/grind/` module hierarchy and the prompt-file data model. Grind has its own concept of "prompt" (a user-authored markdown file with frontmatter), distinct from `src/prompts/` (LLM prompt templates the runner feeds the agent), so we deliberately namespace it under `grind`. This phase produces no behavior change — only types, parsing, and unit tests — so a fresh checkout still builds and the existing `pitboss play` flow is untouched.

**Deliverables.**
- New module `src/grind/mod.rs` with public re-exports, plumbed into `src/lib.rs`.
- `src/grind/prompt.rs` defining `PromptDoc { meta: PromptMeta, body: String, source_path: PathBuf, source_kind: PromptSource }`.
- `PromptMeta` (serde-deserialized YAML frontmatter) with fields: `name`, `description`, `weight: Option<u32>` (default 1), `every: Option<u32>` (run once every N rotations, default 1), `max_runs: Option<u32>`, `verify: bool` (default false), `parallel_safe: bool` (default false), `tags: Vec<String>`, `max_session_seconds: Option<u64>`, `max_session_cost_usd: Option<f64>`.
- `PromptSource` enum (`Project`, `Global`, `Override`).
- `parse_prompt_file(path: &Path) -> Result<PromptDoc>` that handles a leading `---\n…\n---` block followed by markdown body; rejects missing frontmatter, missing `name`, malformed YAML, or duplicate fields with a `thiserror` `PromptParseError` carrying `path` and a one-line diagnostic.
- Validation rules in `PromptMeta::validate`: `name` must match `^[a-z0-9][a-z0-9_-]*$`, `weight >= 1`, `every >= 1`, `max_session_cost_usd >= 0`.
- Unit tests covering: well-formed prompt round-trip, missing frontmatter, malformed YAML, invalid `name`, defaults applied when fields are omitted, `verify`/`parallel_safe` parsing.

**Acceptance.**
- `cargo build` succeeds on a fresh `cargo clean`.
- `cargo test grind::prompt` runs the new tests and they pass.
- `cargo clippy --all-targets -- -D warnings` is clean.
- `pitboss --help` output is unchanged (no new subcommand wired yet).
- `rg "pub mod grind" src/lib.rs` finds the module declaration.

# Phase 02: Prompt discovery and precedence

**Scope.** Implement directory discovery so the rest of the system can ask "what prompts are available?". Sources are ordered: explicit `--prompts-dir <path>` overrides everything; otherwise project `./.pitboss/prompts/` is loaded first, then `~/.pitboss/prompts/` is loaded for any names not already present (project shadows global). Errors in individual files are collected and returned as a list rather than aborting the whole walk, so `pitboss prompts validate` (next phase) can report all bad files at once.

**Deliverables.**
- `src/grind/discovery.rs` with `discover_prompts(opts: DiscoveryOptions) -> DiscoveryResult` where `DiscoveryResult { prompts: Vec<PromptDoc>, errors: Vec<(PathBuf, PromptParseError)> }`.
- `DiscoveryOptions { project_root: PathBuf, home_dir: Option<PathBuf>, override_dir: Option<PathBuf> }`.
- Walks only `*.md` files at the top level of each directory (no recursion), sorted by file name for determinism.
- Project entries shadow global entries with the same `name`; override-dir entries shadow both.
- Helper `resolve_home_prompts_dir() -> Option<PathBuf>` using `std::env::var_os("HOME")`, no extra dep.
- Tests using `tempfile::TempDir` for: project-only, global-only, project-shadows-global, override-replaces-both, mixed valid/invalid files, missing directories returning an empty result, deterministic ordering.

**Acceptance.**
- `cargo test grind::discovery` passes.
- A test asserts that when a file fails to parse, `discover_prompts` still returns the other valid prompts and records the error.
- A test asserts that `--prompts-dir` (override) suppresses both project and global sources entirely.
- `cargo clippy --all-targets -- -D warnings` is clean.

# Phase 03: `pitboss prompts` subcommand (ls / validate / new)

**Scope.** Surface the prompt model on the CLI so users can author and inspect prompts before kicking off a long run. Adds a `Prompts` subcommand with three thin actions backed by phase 02's discovery. `new` writes a templated prompt file with the frontmatter pre-filled and an example body. No grind execution yet.

**Deliverables.**
- `src/cli/prompts.rs` with `Prompts` clap subcommand and `Ls`, `Validate`, `New { name: String, dir: Option<PathBuf>, global: bool }` variants.
- Wire into `src/cli/mod.rs` `Command` enum and `dispatch`.
- `Ls` prints a tabular view: `NAME  SOURCE  WEIGHT  EVERY  VERIFY  PATH`.
- `Validate` re-runs discovery and reports each error as one line, exiting `0` if no errors and `1` otherwise. Prints a final summary `N prompt(s) ok, M error(s)`.
- `New` writes `<dir>/<name>.md` (default: `.pitboss/prompts/`; with `--global`: `~/.pitboss/prompts/`) with a frontmatter template covering every field documented in phase 01, plus an example body. Refuses to overwrite an existing file.
- A `prompts/templates/` (under `src/grind/`, embedded with `include_str!`) for the `New` template body so it ships in the binary.
- `assert_cmd` integration tests for each action against a `tempfile::TempDir` repo.

**Acceptance.**
- `pitboss prompts --help` lists `ls`, `validate`, `new`.
- `pitboss prompts ls` in a fresh repo with no prompts exits `0` and prints "no prompts discovered".
- `pitboss prompts new fp-hunter` creates `.pitboss/prompts/fp-hunter.md` and refuses on a second invocation.
- `pitboss prompts validate` exits `1` when at least one prompt has malformed frontmatter and prints one line per bad file.
- `cargo test --test cli_prompts` (new test file) passes.

# Phase 04: Grind plan files and `[grind]` config section

**Scope.** Add the optional plan file format and the `[grind]` block in `pitboss.toml`. A grind run defaults to "rotate through every discovered prompt by frontmatter rules" with no plan; a plan file at `.pitboss/plans/<name>.toml` lets the user pin which prompts participate, set per-plan defaults, and configure hooks and parallelism caps. This phase only loads and validates the config — it is not yet consumed by an executor.

**Deliverables.**
- `src/grind/plan.rs` defining `GrindPlan { name: String, prompts: Vec<PlanPromptRef>, max_parallel: u32, hooks: Hooks, budgets: PlanBudgets }`.
- `PlanPromptRef { name: String, weight_override: Option<u32>, every_override: Option<u32>, max_runs_override: Option<u32> }`.
- `Hooks { pre_session: Option<String>, post_session: Option<String>, on_failure: Option<String> }` (raw shell strings, executed later).
- `PlanBudgets { max_iterations: Option<u32>, until: Option<chrono::DateTime<Utc>>, max_cost_usd: Option<f64>, max_tokens: Option<u64> }`.
- `load_plan(path: &Path) -> Result<GrindPlan>` and `default_plan_from_dir(prompts: &[PromptDoc]) -> GrindPlan`.
- Extend `src/config/mod.rs` `Config` with `grind: GrindConfig` (serde defaulted) holding: `prompts_dir: Option<PathBuf>`, `default_plan: Option<String>`, `max_parallel: u32` (default 1), `consecutive_failure_limit: u32` (default 3), `transcript_retention: TranscriptRetention` (default `KeepAll`), nested `[grind.budgets]` and `[grind.hooks]` mirroring the plan-level types so an unconfigured plan still inherits them.
- Tests: parse a sample plan TOML, parse a `pitboss.toml` with a full `[grind]` section, default-plan synthesis from discovered prompts, error on a plan referencing an unknown prompt name, error on duplicate `name` entries.

**Acceptance.**
- `cargo test grind::plan` and `cargo test config::grind` pass.
- The existing `Config` tests still pass with no `[grind]` section present (defaults applied).
- A test serializes a plan and re-parses it round-trip without loss.

# Phase 05: Rotation scheduler

**Scope.** Implement the pure scheduling algorithm: given a `GrindPlan`, the prompt set, and a `SchedulerState`, return the next prompt to run (or `None` if exhausted). Handles `weight`, `every`, and `max_runs` from frontmatter and per-plan overrides. The scheduler is fully synchronous and has no IO so it can be exercised by table-driven unit tests covering hundreds of rotations cheaply.

**Deliverables.**
- `src/grind/scheduler.rs` with `Scheduler { plan: GrindPlan, prompts: BTreeMap<String, PromptDoc>, state: SchedulerState }`.
- `SchedulerState { rotation: u64, runs_per_prompt: BTreeMap<String, u32> }` (serde-derived for later persistence).
- `Scheduler::next() -> Option<PromptDoc>`: increments `rotation`; among prompts whose `(rotation % every) == 0` and `runs_per_prompt[name] < max_runs`, picks the one with the highest weighted deficit (weighted round-robin); ties broken alphabetically for determinism.
- `Scheduler::record_run(name: &str)` bumps `runs_per_prompt`.
- Property/table tests asserting: weight `2:1` produces ~2:1 selection ratio over 100 rotations; `every: 3` runs exactly on rotation 3, 6, 9; `max_runs: 5` retires a prompt; empty active set returns `None`.

**Acceptance.**
- `cargo test grind::scheduler` passes.
- A determinism test runs the same scheduler twice with identical inputs and asserts identical sequences.
- No new runtime dependencies (pure-stdlib + existing serde).

# Phase 06: Run directory layout, session log writers, scratchpad

**Scope.** Define and implement the on-disk layout for a grind run: `.pitboss/grind/<run-id>/{state.json, sessions.jsonl, sessions.md, scratchpad.md, transcripts/session-NNNN.log, worktrees/}`. Provide thread-safe writers for the JSONL source-of-truth log, a markdown projection appended after each session, and the scratchpad accessor. No execution path uses these yet — phase 07 will.

**Deliverables.**
- `src/grind/run_dir.rs` with `RunDir::create(repo_root: &Path, run_id: &str) -> Result<RunDir>` and `RunDir::open(...)`.
- `RunDir` owns `paths: RunPaths` and an `append-only` `SessionLog` writer keyed off `sessions.jsonl`.
- `SessionRecord { seq: u32, run_id: String, prompt: String, started_at, ended_at, status: SessionStatus, summary: Option<String>, commit: Option<CommitId>, tokens: TokenUsage, cost_usd: f64, transcript_path: PathBuf }` with `SessionStatus { Ok, Error, Timeout, Aborted }`.
- `SessionLog::append(record)` writes one JSONL line atomically to `sessions.jsonl` (the sole source of truth), then re-renders `sessions.md` from the full JSONL stream so the markdown projection cannot drift from the log. Rendering goes through a small `render_sessions_md(records: &[SessionRecord]) -> String` helper that the tests pin with `insta`. The append + re-render pair runs under a single in-process lock so partial writes are never observable.
- `Scratchpad { path: PathBuf }` with `read() -> Result<String>` and `path_for_agent()` (the agent reads/writes it directly; pitboss never edits its contents).
- `generate_run_id() -> String` (UTC timestamp + 4 hex chars, mirroring existing run-id style if any in `src/state/`).
- Tests: round-trip JSONL append, sessions.md row format pinned via `insta`, run-id uniqueness, scratchpad created empty, `RunDir::open` rejects a missing directory.

**Acceptance.**
- `cargo test grind::run_dir` passes.
- `cargo insta test` snapshots are created and committed for the markdown row format.
- A test writes 50 records concurrently from multiple tasks and asserts the JSONL line count equals 50 with no truncation.

# Phase 07: Sequential `pitboss grind` MVP

**Scope.** Wire phases 01–06 into a working command. Sequential only (no parallelism, no resume, no budgets, no hooks, no PR, no TUI) — those land in later phases. The agent receives the prompt body, the rotation index, the path to `$PITBOSS_SUMMARY_FILE`, the path to the scratchpad, and the prior session log auto-injected as context. Per-prompt `verify: true` reuses the existing test runner and fixer cycle from `src/runner/`; `verify: false` skips it. Two-stage Ctrl-C: first signal drains (finish the current session, then stop); second signal aborts immediately.

**Deliverables.**
- `src/cli/grind.rs` with the `Grind { plan: Option<String>, prompts_dir: Option<PathBuf>, dry_run: bool }` clap subcommand. (Other flags arrive in phases 08–12.)
- `src/grind/run.rs` `GrindRunner` orchestrating: discover → load/synthesize plan → create run dir → loop { schedule → dispatch agent → read summary → record session → optional verify }.
- Auto-injected context: pitboss prepends the last 50 sessions.md rows and the current scratchpad to the agent prompt under stable header markers (`<!-- pitboss:session-log -->`, `<!-- pitboss:scratchpad -->`).
- Agent env: `PITBOSS_RUN_ID`, `PITBOSS_PROMPT_NAME`, `PITBOSS_SUMMARY_FILE`, `PITBOSS_SCRATCHPAD`, `PITBOSS_SESSION_SEQ`.
- Summary fallback: if `$PITBOSS_SUMMARY_FILE` is empty/missing on agent exit, pitboss writes `"(no summary provided)"` and logs a warning.
- One git branch per run, hierarchical: `pitboss/grind/<run-id>`. Use this exact form everywhere a grind branch is named (run branch here, session worktree branches in Phase 11). Each session commits on the run branch sequentially. If the working tree is dirty at session end, pitboss stashes the leftover changes into a named stash `grind/<run-id>/session-NNN-leftover`, records `status: dirty` on the session, and continues — the agent's in-progress work is preserved for morning triage instead of being discarded. A merge-conflict at session end is a hard fail (`SessionStatus::Error`) and the next session continues.
- Standing-instruction injection: pitboss auto-prepends a fixed instruction block to every grind prompt before dispatch, telling the agent to write its session summary to `$PITBOSS_SUMMARY_FILE` (and where the scratchpad and session log live). The block lives in `src/grind/standing_instruction.md` (embedded via `include_str!`) and is wrapped in stable header markers so it can be located, updated, or stripped later without parsing the whole prompt. Users do not need to author this instruction in their own prompt files.
- Two-stage Ctrl-C via `tokio::signal::ctrl_c` and an atomic drain flag.
- Integration test using a `MockAgent` (existing pattern) with three prompts rotating across six sessions, asserting `sessions.jsonl` has six records, scratchpad reads succeed, and the per-run directory has the expected shape.

**Acceptance.**
- `pitboss grind --help` shows the new subcommand.
- `cargo test --test grind_smoke` (new file) runs end-to-end against the mock agent and passes.
- A test asserts that sending a simulated Ctrl-C drain after session 2 finishes session 2 cleanly and exits without starting session 3.
- `pitboss play` end-to-end smoke test (existing) still passes.
- `cargo clippy --all-targets -- -D warnings` is clean.

# Phase 08: Budgets and exit codes

**Scope.** Add the global and per-prompt budgets and the documented exit-code policy. Budgets are checked before each session dispatch and after each session completes; when one trips, the runner finishes any currently-running session, writes a final `BudgetExhausted` log line, and exits with code 3. Per-prompt time and cost limits are enforced inside the dispatch wrapper using existing agent timeout machinery.

**Deliverables.**
- New flags on `Grind`: `--max-iterations <n>`, `--until <rfc3339>`, `--max-cost <usd>`, `--max-tokens <n>`. Flags override `[grind.budgets]` from `pitboss.toml` and the plan's `PlanBudgets`.
- `BudgetTracker` in `src/grind/budget.rs` aggregating tokens and cost as sessions report them; `BudgetTracker::check() -> BudgetCheck { Ok, Exhausted(BudgetReason) }`.
- Per-prompt enforcement: dispatch wraps the agent in `tokio::time::timeout(prompt.max_session_seconds)` and after-the-fact compares cost; over-budget produces `SessionStatus::Timeout` or `SessionStatus::Error` with a clear summary line.
- `ExitCode` enum mapped to: `0` all sessions ok, `1` mixed failures, `2` aborted (Ctrl-C), `3` budget hit, `4` failed to start, `5` consecutive-failure escape valve (uses `consecutive_failure_limit` from phase 04).
- `main.rs` translates `ExitCode` into `std::process::ExitCode`.
- Tests: `--max-iterations 2` stops after exactly 2 sessions; per-prompt `max_session_seconds` is enforced (using a sleeping mock agent); consecutive-failure limit triggers exit code 5; mixed run produces exit code 1.

**Acceptance.**
- `cargo test grind::budget` and `cargo test --test grind_exit_codes` pass.
- An integration test asserts each documented exit code is produced by the corresponding scenario.
- `pitboss grind --help` documents every new flag.

# Phase 09: Resume

**Scope.** Persist enough state for `pitboss grind --resume [<run-id>]` to pick up after a crash, reboot, or Ctrl-C abort. The latest run is the default target. State is the source-of-truth `sessions.jsonl` plus a small `state.json` that caches scheduler state, budget consumption, and the run's branch. Resuming replays scheduler state and continues on the same branch.

**Deliverables.**
- `RunState` struct serialized to `.pitboss/grind/<run-id>/state.json` after every session: `run_id`, `branch`, `plan_name`, `scheduler_state`, `budget_consumed: BudgetSnapshot`, `last_session_seq`, `started_at`, `last_updated_at`, `status: RunStatus { Active, Completed, Aborted, Failed }`.
- `--resume [<run-id>]` flag on `Grind`. When omitted, picks the most-recent `Active` or `Aborted` run under `.pitboss/grind/`.
- Refuses to resume a run whose `pitboss.toml` plan or prompt set has changed in a way that invalidates the scheduler (different prompt name list); prints a clear error pointing the user to start a new run.
- After-resume sanity: re-checks out the run branch; if working tree is dirty, halts with exit code 4.
- Tests: kill-and-resume scenario using mock agent and a manually-injected partial state.json; resume rejects when prompts have been removed; default-most-recent selection.

**Acceptance.**
- `pitboss grind --resume` with no argument picks the latest run in tests.
- `cargo test --test grind_resume` passes.
- A test asserts that after-resume the scheduler emits the same next prompt that the original run would have emitted.

# Phase 10: Hooks

**Scope.** Run plan-level shell hooks (`pre_session`, `post_session`, `on_failure`) loaded from phase 04. Hooks are spawned as `sh -c "<cmd>"` children with the same env vars the agent sees, plus `PITBOSS_SESSION_PROMPT` (the prompt name dispatched for this session, set on all three hook kinds) and — for `post_session`/`on_failure` only — `PITBOSS_SESSION_STATUS` (the resolved `SessionStatus`) and `PITBOSS_SESSION_SUMMARY` (the captured summary text or the `(no summary provided)` fallback from Phase 07). Hook stdout/stderr is captured into the per-session transcript so failures are debuggable. Hook non-zero exit on `pre_session` skips the session as `SessionStatus::Error`; on `post_session`/`on_failure` it is logged but does not affect session status.

**Deliverables.**
- `src/grind/hooks.rs` with `run_hook(kind: HookKind, cmd: &str, env: &HashMap<String, String>) -> HookOutcome` using `tokio::process::Command`.
- Hook timeout (default 60s, configurable via `[grind.hook_timeout_secs]`).
- Hook output appended to the session transcript with a labeled banner.
- `GrindRunner` calls the hooks at the documented points.
- Tests: each hook fires exactly once per session; `pre_session` non-zero skips dispatch and records `Error`; `on_failure` runs only when the session status is non-`Ok`; hook timeout produces a clear log line.

**Acceptance.**
- `cargo test grind::hooks` passes.
- An integration test using `echo`-based hooks asserts the captured banner is present in the transcript.
- A test asserts a 5-second-sleep hook with a 1-second timeout is killed and recorded as a hook timeout.

# Phase 11: Parallel sessions via worktrees

**Scope.** Honor `parallel_safe: true` on prompts and `max_parallel > 1` at the plan/config level. A parallel-eligible session runs in its own git worktree under `.pitboss/grind/<run-id>/worktrees/session-NNNN/`, on an ephemeral branch `pitboss/grind/<run-id>/session-NNNN` off the run branch (same hierarchical convention as the run branch itself). When the session completes, pitboss fast-forwards the run branch to the session's tip and deletes the ephemeral branch; a non-fast-forward result is a hard fail labeling the prompt's `parallel_safe: true` claim as violated. Sequential remains the default.

**Deliverables.**
- `src/grind/worktree.rs` with `SessionWorktree::create(...)`, `SessionWorktree::cleanup(...)`, and `SessionWorktree::merge_into(run_branch)`.
- Concurrency gate in `GrindRunner` using `tokio::sync::Semaphore(max_parallel)`; only `parallel_safe: true` prompts take a non-1 permit, others lock the run branch directly.
- Run-branch fast-forward only (`git merge --ff-only`); non-FF aborts that session as `Error` with a `"parallel_safe contract violated by prompt <name>"` summary.
- Worktree cleanup on session completion (success or failure) so the run dir does not balloon; failed worktrees are kept under `worktrees/failed/session-NNNN/` for forensics.
- Scratchpad coordination across parallel sessions: the agent subprocess writes the scratchpad directly, so an in-process `Mutex` in pitboss cannot serialize those writes — they come from sibling OS processes in separate worktrees. Instead, each parallel session gets its own per-session scratchpad view at `worktrees/session-NNNN/scratchpad.md`, seeded from a snapshot of the run-level scratchpad at session start. On session completion, pitboss merges the per-session view back into the run-level scratchpad: if exactly one parallel session modified it, fast-merge; if multiple modified it, append each session's diff under a labeled `<!-- pitboss:session-NNNN -->` header rather than attempting a 3-way text merge. Sequential sessions continue to read and write the run-level scratchpad in place. The session-log writer (`sessions.jsonl`) is owned by the pitboss parent process (agents never touch it), so its single in-process lock is sufficient.
- Tests: two parallel-safe prompts run concurrently against the mock agent and both commits land; an induced merge conflict produces the labeled error and does not poison the run branch; sequential prompts in the same plan still run one-at-a-time.

**Acceptance.**
- `cargo test --test grind_parallel` passes.
- A test asserts that with `max_parallel: 2` and two `parallel_safe: true` prompts, the wall-clock time is meaningfully less than the sum of session times (using a mock agent that sleeps a known duration).
- A test asserts a non-`parallel_safe` prompt is never scheduled while another session is in flight.

# Phase 12: `--pr` and `--dry-run`

**Scope.** Round out the user-visible flags. `--pr` opts into a single pull request opened at the end of a successful run, reusing the existing `gh pr create` integration that `pitboss play` uses. `--dry-run` prints the resolved rotation plan and a sanity-check dump (discovered prompts with sources, plan name, budgets, hooks, parallelism cap, expected first 10 selections from the scheduler) and exits without invoking the agent.

**Deliverables.**
- `--pr` flag on `Grind`: on successful completion (exit code 0), invokes the same PR-opening helper used by `src/cli/run.rs`; PR title is `grind/<plan-or-default>: <run-id>` and body is the contents of `sessions.md`.
- `--dry-run` flag on `Grind`: bypasses agent dispatch entirely; prints a deterministic, machine-readable section header followed by a human report.
- Refactor any duplicated PR-opening code from `pitboss play` into `src/git/pr.rs` if not already factored, so both subcommands share one path.
- Tests: `--dry-run` against a fixture plan produces a snapshot-pinned (`insta`) report; `--pr` invocation against a `MockGh` (existing test double pattern) issues exactly one `gh pr create` call after a successful run.

**Acceptance.**
- `cargo test --test grind_dry_run` and `cargo test --test grind_pr` pass.
- `pitboss grind --dry-run` exits `0`, makes no commits, and creates no run directory.
- `cargo insta test` snapshots are committed for the dry-run report.

# Phase 13: TUI integration

**Scope.** Extend the existing ratatui dashboard (`src/tui/`) with a grind view. Mirrors the run dashboard's structure: left pane lists recent sessions with status icons; right pane streams the current agent's output; a footer shows budget consumption (sessions, tokens, dollars, time-to-`--until`) and the next scheduled prompt. The grind view subscribes to the same broadcast channel pattern the runner uses, so adding new event types is the only TUI-side change.

**Deliverables.**
- `GrindEvent` broadcast type and corresponding emissions from `GrindRunner` at session start, summary capture, session end, hook fire, budget warn (≥80% consumed), and scheduler-next decisions.
- `src/tui/grind.rs` rendering session list, live transcript, footer.
- `--tui` flag on `Grind` (mirrors `pitboss play --tui`); without it, the existing logger output is used.
- Manual run-it-yourself instructions in module docs (the user has a documented stance that TUI behavior is verified by hand, not in CI).
- Unit tests on rendering helpers (e.g., session-row formatting, budget-percent calculation) — full TUI integration is exercised manually.

**Acceptance.**
- `cargo test tui::grind` passes.
- `pitboss grind --tui` launches the dashboard and renders frames against the mock agent without panics (verified by a smoke test that drives the event stream and asserts no panic over 50 events).
- `cargo clippy --all-targets -- -D warnings` is clean.
- All prior phases' tests still pass under `cargo test --workspace`.