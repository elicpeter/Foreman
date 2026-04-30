---
current_phase: "12"
project: foreman
---

# Foreman — Implementation Plan

A Rust CLI that orchestrates coding agents (Claude Code first; pluggable) through a phased implementation plan, with deferred-work tracking, automatic test/commit/audit loops, and a ratatui dashboard.

## Guiding Principles

These decisions are locked in early to minimize later refactoring. Every phase must respect them.

- **Single crate, well-modularized.** `src/{cli,plan,deferred,state,config,agent,git,tests,prompts,runner,tui}/`. Splitting to a workspace later is mechanical if needed; up-front workspacing adds ceremony we don't need.
- **Async everywhere.** `tokio` is the runtime. Agents are subprocesses with streaming output; nothing blocks.
- **Errors:** `thiserror` for typed errors in parsers/state (we need to recognize specific failure modes — e.g., "agent corrupted plan.md" must trigger a revert). `anyhow` at command boundaries.
- **Logging:** `tracing` + `tracing-subscriber`. No `println!` in library code. The CLI configures the subscriber.
- **Atomic file writes.** Every write to `plan.md`, `deferred.md`, or `state.json` goes through a `write_atomic(path, bytes)` helper that writes to `path.tmp` and renames. No half-written state files, ever.
- **Event-driven runner.** From phase 12 onward, the runner emits structured events to a `tokio::sync::broadcast` channel. The plain CLI logger and (later) the TUI are both *subscribers*. This means adding the TUI is purely additive — no runner refactor.
- **Agent trait first, impls second.** Phase 7 nails the `Agent` trait shape including a `DryRunAgent` (no-op, deterministic) used in tests. Phase 8 adds `ClaudeCodeAgent`. Future agents (Codex, etc.) just implement the trait.
- **plan.md is read-only to agents.** Only the runner mutates `current_phase` in frontmatter when advancing. Any agent-side modification is rejected via SHA-256 snapshot comparison and the run halts.
- **deferred.md is the only agent-writable artifact.** Items + replanned phase blocks. Validated by parser after every agent run; if it doesn't parse, revert from snapshot and halt.
- **Per-run git branch.** `foreman/run-<timestamp>`. One commit per phase. Optional PR via `gh` CLI behind a flag.
- **Commit scope: code only, never planning artifacts.** Each per-phase commit includes files the agent created or modified, but **never** `plan.md`, `deferred.md`, or anything under `.foreman/`. These three paths are excluded from `git add` regardless of what the agent touched. They remain in the working tree — foreman never deletes them. `.foreman/` is gitignored on `init`; `plan.md` and `deferred.md` are not, so the user can choose to commit them themselves outside of foreman runs.
- **Per-role models.** `foreman.toml` configures models for: `planner`, `implementer`, `auditor`, `fixer`. All default to a sensible Sonnet/Opus split.
- **Bounded retries everywhere.** No infinite loops. Test failures → fixer up to N times → halt and ask user.

## Layout (target)

```
foreman/
├── Cargo.toml
├── src/
│   ├── main.rs          — CLI entry, wires subscribers
│   ├── cli/             — clap commands
│   ├── plan/            — Plan, Phase, parser, snapshot
│   ├── deferred/        — DeferredDoc, items, phases, parser
│   ├── state/           — RunState, atomic IO
│   ├── config/          — foreman.toml schema + loader
│   ├── agent/           — trait, request/outcome, subprocess utils
│   │   ├── claude_code.rs
│   │   └── dry_run.rs
│   ├── git/             — branch/commit/status (Git trait)
│   ├── tests/           — project test runner detection
│   ├── prompts/         — system prompt templates
│   ├── runner/          — orchestration loop + events
│   └── tui/             — ratatui dashboard
└── tests/               — integration tests
```

In a working directory under foreman's control:
```
project/
├── plan.md              — source of truth for the work
├── deferred.md          — agent-writable, sweep between phases
├── foreman.toml         — config
└── .foreman/
    ├── state.json       — runner-owned
    ├── snapshots/       — pre-agent snapshots of plan.md & deferred.md
    └── logs/            — per-phase, per-attempt agent stdout/stderr
```

---

# Phase 01: Project foundation

**Scope.** Stand up the crate with all dependencies pinned, module skeleton in place, logging wired, and a working CLI shell that does nothing useful yet but parses commands cleanly.

**Deliverables.**
- `Cargo.toml` with locked deps: `tokio` (full), `clap` (derive), `anyhow`, `thiserror`, `tracing`, `tracing-subscriber` (env-filter, fmt), `serde`, `serde_json`, `serde_yaml`, `toml`, `sha2`, `time` (or `chrono`), `tempfile` (dev), `assert_cmd` + `predicates` (dev).
- Module files (`mod.rs` stubs) for every directory in the target layout. Each declares its public surface even if empty.
- `main.rs` initializes `tracing-subscriber` (env-driven via `RUST_LOG` / `FOREMAN_LOG`) and dispatches to clap.
- CLI commands stubbed (return `unimplemented!()` is fine): `init`, `plan`, `run`, `status`, `resume`.
- `rustfmt.toml`, `clippy.toml` (deny warnings in CI), `.gitignore`.
- A `util::write_atomic(path, &[u8]) -> Result<()>` helper, fully tested. This is reused everywhere.

**Acceptance.**
- `cargo build` passes clean.
- `cargo clippy -- -D warnings` passes.
- `foreman --help` lists all top-level commands.
- `cargo test` runs (one passing test for `write_atomic`, including a crash-resistance test using a tempfile).

**Notes.** Lock dependency versions now. Adding deps later is fine; changing them mid-project is costly.

---

# Phase 02: Domain types

**Scope.** Define every core type the rest of the system manipulates. Get this right and most later phases just pattern-match on these types.

**Deliverables.**
- `plan::PhaseId(String)` — newtype wrapping a string ID. Derives `Ord` via a custom impl that splits leading digits and trailing suffix (so `"02" < "10" < "10b"`). Tested with a wide table of cases.
- `plan::Phase { id: PhaseId, title: String, body: String }` — `body` is the raw markdown after the `# Phase NN: Title` line, preserved verbatim.
- `plan::Plan { current_phase: PhaseId, phases: Vec<Phase> }` — phases stored sorted by `PhaseId`.
- `deferred::DeferredItem { text: String, done: bool }`.
- `deferred::DeferredPhase { source_phase: PhaseId, title: String, body: String }`.
- `deferred::DeferredDoc { items: Vec<DeferredItem>, phases: Vec<DeferredPhase> }`.
- `state::RunState { run_id: String, branch: String, started_at: DateTime, started_phase: PhaseId, completed: Vec<PhaseId>, attempts: HashMap<PhaseId, u32>, token_usage: TokenUsage }`.
- `state::TokenUsage { input: u64, output: u64, by_role: HashMap<String, RoleUsage> }`.
- All types `serde::Serialize` + `Deserialize`. Round-trip tested.

**Acceptance.**
- Unit tests for `PhaseId` ordering covering: pure numeric, suffixed, mixed, malformed (rejected with typed error).
- JSON round-trip tests for `RunState`.
- 100% of public types have rustdoc comments.

**Notes.** Even though we removed the `12b` mechanism, `PhaseId` keeps the suffixed sort logic — costs nothing now, opens the door later, and is needed anyway for any non-zero-padded user input.

---

# Phase 03: Plan parser

**Scope.** Read and write `plan.md` losslessly. Provide snapshot/hash utilities for tamper detection.

**Deliverables.**
- `plan::parse(input: &str) -> Result<Plan, PlanParseError>` — strict YAML frontmatter parse, then phase block extraction by `# Phase <id>:` heading regex. Anything before the first phase heading is preserved as `Plan::preamble`.
- `plan::serialize(plan: &Plan) -> String` — exact round-trip for any plan that `parse` accepts.
- `plan::PlanParseError` — typed (MissingFrontmatter, BadFrontmatter, NoPhases, DuplicatePhaseId, etc.).
- `plan::snapshot(path) -> Result<Snapshot>` and `plan::verify_unchanged(path, snapshot) -> Result<()>` using SHA-256.
- `plan::Plan::set_current_phase(&mut self, id)` — the only mutator the runner uses.

**Acceptance.**
- Round-trip property test: `serialize(parse(s))? == s` for a corpus of fixture plans.
- Hostile inputs rejected: empty file, frontmatter only, duplicate IDs, unknown frontmatter keys (warn but accept), Windows line endings (normalize to LF on write).
- Hash mismatch detected even for whitespace-only changes.

---

# Phase 04: Deferred doc parser

**Scope.** Same idea as plan parser but for `deferred.md`. The agent writes this file, so the parser must be strict and produce useful errors.

**Deliverables.**
- `deferred::parse(input: &str) -> Result<DeferredDoc, DeferredParseError>` — recognizes two H2 sections: `## Deferred items` (a checklist) and `## Deferred phases` (H3 blocks `### From phase <id>: <title>`).
- `deferred::serialize(doc: &DeferredDoc) -> String` — round-trip stable.
- An empty / missing file is parsed as `DeferredDoc::empty()`.
- `DeferredDoc::sweep(&mut self)` — removes `done: true` items.
- `deferred::snapshot` / `verify` parallel to plan.

**Acceptance.**
- Round-trip property tests.
- Strict mode: bad section headers, malformed checkbox lines, H3 in wrong section → typed errors with line numbers.
- Tolerant of trailing whitespace, empty sections, blank lines between items.

---

# Phase 05: Workspace layout & `foreman init`

**Scope.** Implement `foreman init` end to end. After this phase, you can scaffold a real foreman workspace from any empty directory.

**Deliverables.**
- `cli init` subcommand. Creates (only if not already present):
  - `plan.md` — one-phase template. **If already exists, skip and emit a warning** (`plan.md already exists, leaving it alone`). Never overwrite or delete.
  - `deferred.md` — empty scaffold. Same rule: skip + warn if present.
  - `foreman.toml` — defaults. Same rule.
  - `.foreman/` directory with `snapshots/` and `logs/` subdirs.
  - `.foreman/state.json` — initialized empty state. Same rule: skip + warn if present (so we don't blow away an in-progress run).
- Updates `.gitignore`:
  - Adds `.foreman/` (entire directory ignored — state, snapshots, and logs are all local).
  - Does **not** add `plan.md` or `deferred.md` — leaves that to the user.
  - If `.gitignore` already contains the entry, no change.
- `state::load(workspace) -> Result<RunState>` and `state::save(...)` using `write_atomic`.
- `init` exit behavior: prints a per-file summary (`created`, `skipped (exists)`) and exits 0 even if everything was skipped. Never destructive.

**Acceptance.**
- Integration test (`assert_cmd` + `tempfile`): fresh init creates all files.
- Re-running `init` on a populated workspace exits 0, modifies nothing, and prints "skipped" for each existing file.
- Specific test: pre-existing `plan.md` with custom content survives `init` byte-for-byte and produces a warning on stderr.
- `.gitignore` is correctly updated and idempotent across multiple `init` runs.

---

# Phase 06: Configuration

**Scope.** Load `foreman.toml`. Provide typed access throughout the codebase.

**Deliverables.**
- `config::Config { models: ModelRoles, retries: RetryBudgets, audit: AuditConfig, git: GitConfig }`.
- `ModelRoles { planner, implementer, auditor, fixer }` — each `String`.
- `RetryBudgets { fixer_max_attempts: u32 (default 2), max_phase_attempts: u32 (default 3) }`.
- `AuditConfig { enabled: bool (default true), small_fix_line_limit: u32 (default 30) }`.
- `GitConfig { branch_prefix: String (default "foreman/run-"), create_pr: bool (default false) }`.
- Defaults populated for missing keys; unknown keys → warning, not error.
- `config::load(workspace) -> Result<Config>`.

**Acceptance.**
- Tests cover: full file, partial file (defaults filled), empty file, malformed file.
- Doc comments on every config field that explain effect on runner behavior.

---

# Phase 07: Agent trait & subprocess utilities

**Scope.** The single most important abstraction. Everything downstream depends on it being right.

**Deliverables.**
- `agent::Agent` async trait:
  ```rust
  #[async_trait]
  pub trait Agent: Send + Sync {
      fn name(&self) -> &str;
      async fn run(
          &self,
          req: AgentRequest,
          events: mpsc::Sender<AgentEvent>,
          cancel: CancellationToken,
      ) -> Result<AgentOutcome>;
  }
  ```
- `AgentRequest { role: Role, model: String, system_prompt: String, user_prompt: String, workdir: PathBuf, log_path: PathBuf, timeout: Duration }`.
- `AgentEvent { Stdout(line), Stderr(line), TokenDelta(usage), ToolUse(name) }` — streamed during run.
- `AgentOutcome { exit_code: i32, stop_reason: StopReason, tokens: TokenUsage, log_path: PathBuf }`.
- `StopReason::{Completed, Timeout, Cancelled, Error(String)}`.
- `agent::dry_run::DryRunAgent` — deterministic stub for tests that writes a configurable script of events and returns a configurable outcome. **Critical**: every later phase tests against this; we never touch a real agent in unit tests.
- `agent::subprocess` — helpers for spawning a child process, teeing stdout/stderr to both a log file and an `mpsc::Sender`, and honoring cancellation.

**Acceptance.**
- `DryRunAgent` exercises: success path, failure path, timeout path, cancellation path. All four covered by tests.
- Subprocess helper tested against `/bin/sh -c "echo hi"` style cases on unix.

**Notes.** The trait's shape is intentionally generic enough to support codex-style and future agents without modification. Adding a new agent later means: implement the trait, register it in a small factory.

---

# Phase 08: Claude Code agent implementation

**Scope.** Concrete `ClaudeCodeAgent` implementing `Agent`. Shells out to the `claude` binary.

**Deliverables.**
- `agent::claude_code::ClaudeCodeAgent { binary: PathBuf }` — defaults to `which claude`.
- Builds command: `claude -p <prompt> --model <model> --output-format stream-json --cwd <workdir>` (verify exact flags against current `claude` CLI before implementing — if any have changed, this phase updates them).
- Parses streaming JSON events; emits `AgentEvent`s. Aggregates token usage.
- Honors `cancel` (kills child with SIGTERM, then SIGKILL after a grace period).
- Handles non-zero exit: maps known error patterns to `StopReason::Error(...)` with a useful message.

**Acceptance.**
- Mock-based unit tests using a fake `claude` binary (a small shell script in `tests/fixtures/`).
- One real end-to-end test gated behind an env var (`FOREMAN_REAL_AGENT_TESTS=1`) so CI can skip it.
- Documentation in module-level rustdoc explaining how to install and configure `claude`.

**Notes.** Keep all CLI flag knowledge isolated to this file. If Anthropic changes the CLI, only this file changes.

---

# Phase 09: Git integration

**Scope.** Branch, commit, status. Behind a trait so we can swap implementations or mock in tests.

**Deliverables.**
- `git::Git` trait: `is_clean()`, `current_branch()`, `create_branch(name)`, `checkout(name)`, `stage_changes(exclude: &[&Path])`, `commit(message) -> CommitId`, `diff_stat(from, to)`, `has_staged_changes()`.
- `stage_changes` adds everything modified/created in the working tree **except** the paths in `exclude`. Implementation: `git add -A -- . ':!plan.md' ':!deferred.md' ':!.foreman'` (git pathspec exclusion). The runner always passes these three exclusions; the trait stays generic so it's testable.
- `has_staged_changes()` is checked before commit — if the agent only modified excluded paths (e.g., only edited `deferred.md`), there's nothing to commit. Runner logs a warning ("phase produced no code changes") and continues; the phase still counts as complete.
- `git::ShellGit` — shells out to `git`. Preferred over `git2` for portability and predictability.
- `git::MockGit` — in-memory mock used by runner tests; tracks an exclusion set and verifies the runner is passing it correctly.
- Branch naming: `<config.git.branch_prefix><utc_timestamp>` (e.g., `foreman/run-20260429T143022Z`).
- Commit message format: `[foreman] phase <id>: <title>`.

**Acceptance.**
- `ShellGit` tested against a real `git init` in a tempdir, including the exclusion behavior: create `plan.md`, `deferred.md`, `.foreman/state.json`, and `src/foo.rs`; call `stage_changes` with the standard exclusions; verify only `src/foo.rs` is staged.
- "Empty commit" path tested: only excluded files changed → `has_staged_changes()` returns false, no commit attempted.
- `MockGit` round-trip tested.

---

# Phase 10: Test runner detection

**Scope.** Detect the project's test framework and run it. Report pass/fail and a brief summary.

**Deliverables.**
- `tests::detect(workdir) -> Option<TestRunner>` — probes for `Cargo.toml` (cargo), `package.json` (npm/pnpm/yarn — read scripts), `pyproject.toml` / `setup.py` (pytest), `go.mod` (go test).
- `tests::TestRunner::run() -> TestOutcome { passed: bool, summary: String, log_path: PathBuf }`.
- Streams output to a log file; returns a short summary (last N lines on failure, count on success).
- Detection is best-effort and configurable: `foreman.toml` `[tests] command = "..."` overrides detection.

**Acceptance.**
- Detection tests against fixture project layouts.
- Override path tested.
- Failure path returns useful summary.

---

# Phase 11: Prompt templates

**Scope.** Codify the system prompts we agreed on. Centralize so they're easy to iterate.

**Deliverables.**
- `prompts::implementer(plan: &Plan, deferred: &DeferredDoc, current: &Phase) -> String` — produces the full system + user prompt enforcing:
  1. Read deferred.md first; finish unchecked items, then deferred phases.
  2. Then implement `current_phase` from plan.md.
  3. **Never edit plan.md.** **Never touch `.foreman/`.**
  4. Anything unfinished or replanned → append to deferred.md (items for small things, `### From phase X:` blocks for replans).
- `prompts::auditor(plan, current_phase, diff) -> String` — small fixes (≤ `small_fix_line_limit` lines) inline; larger → deferred items.
- `prompts::fixer(plan, current_phase, test_output) -> String`.
- `prompts::planner(goal, repo_summary) -> String`.
- All prompts in `.txt` files under `src/prompts/templates/`, embedded via `include_str!` and parameterized with simple `{placeholder}` substitution (no full template engine — keep it boring).

**Acceptance.**
- Snapshot tests (`insta` crate, dev-only) for each prompt against fixture inputs. Snapshot tests catch unintended prompt changes.
- Each prompt fits within a sensible character budget; documented.

---

# Phase 12: Runner core (orchestration, event-driven)

**Scope.** The heart of foreman. Implements the per-phase loop using only the implementer role — no fixer, no auditor yet. Emits events. CLI logger subscribes.

**Deliverables.**
- `runner::Runner { config, plan, deferred, state, agent, git, tests, events_tx }`.
- `runner::Event { PhaseStarted, AgentStdout(line), AgentEvent(...), TestStarted, TestFinished, PhaseCommitted, PhaseHalted(reason) }`.
- `runner::run_phase(&mut self) -> Result<PhaseResult>`:
  1. Snapshot `plan.md` and `deferred.md`.
  2. Build implementer prompt.
  3. Dispatch agent; stream events.
  4. Verify `plan.md` unchanged → if changed, restore from snapshot and halt with a clear error.
  5. Re-parse `deferred.md` → if invalid, restore and halt.
  6. Run tests.
  7. On pass: `git.stage_changes(&[plan.md, deferred.md, .foreman])`. If `has_staged_changes()`, commit with the phase message. Either way, sweep deferred items, persist `RunState` (writes to `.foreman/state.json` — but state is *runner*-managed and lives outside git anyway), and advance `current_phase` in `plan.md`.
  8. On fail (this phase): halt — fixer comes in phase 13.
- `cli run` subcommand: validates workspace, creates branch, loops phases until done or halted.
- Logger subscriber prints structured progress to stdout (this is the "no TUI" CLI experience).

**Acceptance.**
- End-to-end integration test using `DryRunAgent` and `MockGit`: a 3-phase fixture plan runs to completion.
- Tamper test: `DryRunAgent` configured to corrupt `plan.md` → run halts and restores.
- Bad deferred test: agent writes malformed `deferred.md` → run halts and restores.
- Test-failure test: tests fail → run halts (no fixer yet).
- Exclusion test: `DryRunAgent` "modifies" only `deferred.md` → run advances with no commit, warning logged.
- Mixed-changes test: agent modifies both `src/foo.rs` and `plan.md` → run halts (plan tampering wins over the commit).

**Notes.** This phase spends real care on the event channel. Adding the TUI in phase 16 must require zero changes to runner code.

---

# Phase 13: Fixer role

**Scope.** When tests fail, spawn the fixer agent up to `retries.fixer_max_attempts` times before halting.

**Deliverables.**
- `runner::run_phase` now: on test failure, spawn fixer agent with the implementer prompt's fix variant + test output. Re-run tests. Repeat up to budget.
- Each fixer attempt logged to `.foreman/logs/phase-<id>-fix-<n>.log`.
- `RunState::attempts` updated.

**Acceptance.**
- Test where fixer succeeds on attempt 2.
- Test where fixer exhausts retries → halt.

---

# Phase 14: Auditor role

**Scope.** Post-phase, pre-commit audit pass.

**Deliverables.**
- After tests pass and before commit: dispatch auditor agent with diff vs branch base.
- Auditor protocol from prompts: small fixes inline; larger → append to `deferred.md`.
- After auditor returns: re-validate plan/deferred snapshots, re-run tests (auditor may have edited code), then commit.
- Toggleable via `audit.enabled` config.

**Acceptance.**
- Audit-disabled path unchanged from phase 13.
- Audit-enabled path tested with `DryRunAgent` simulating both small-fix and large-defer cases.

---

# Phase 15: Planner role

**Scope.** `foreman plan "<goal>"` generates `plan.md` from a goal string + repo overview.

**Deliverables.**
- `cli plan <goal>` — collects a brief repo summary (file tree, top-level READMEs, package manifests), invokes planner agent, writes the result to `plan.md` (refusing to overwrite without `--force`).
- Validates the generated plan parses cleanly; if not, re-prompts once with the parse error in the prompt; otherwise fails.

**Acceptance.**
- Tested with `DryRunAgent` returning canned plan.md content.
- Force flag tested.
- Validation-retry loop tested.

---

# Phase 16: TUI dashboard

**Scope.** Pretty live dashboard subscribed to runner events. Pure addition — no runner changes.

**Deliverables.**
- `tui::App` using `ratatui` + `crossterm`.
- Layout: header (run id, branch, current phase), middle (phase list with status icons), right pane (streaming agent output, scrollable), footer (key bindings: `q` quit, `p` pause, `a` abort).
- Subscribes to the runner's `broadcast::Receiver<Event>`.
- `cli run --tui` flag enables it; default remains plain logger.

**Acceptance.**
- Manual smoke test using `DryRunAgent` against a fixture plan.
- Snapshot tests for layout rendering at fixed terminal sizes (ratatui supports this).
- Quitting cleanly cancels the run.

---

# Phase 17: Status, resume, abort

**Scope.** Re-entrant lifecycle.

**Deliverables.**
- `cli status` — reads `state.json` + plan + deferred, prints a nice summary (current phase, completed, deferred work, branch, last commit).
- `cli resume` — picks up where a halted run left off, validates clean state, continues.
- `cli abort` — marks state as aborted, optionally checks out the original branch.

**Acceptance.**
- Tests for each command's happy and edge-case paths (e.g., resume after a halt, status with no run started).

---

# Phase 18: Cost tracking & budgets

**Scope.** Track token usage; enforce a max-cost halt.

**Deliverables.**
- `RunState::token_usage` aggregated from all `AgentOutcome`s.
- `config::Budgets { max_total_tokens, max_total_usd }` (USD computed via a config'd per-model rate table).
- Runner checks budget before each agent dispatch; halts with `PhaseHalted(BudgetExceeded)` if exceeded.
- `status` displays usage + budget remaining.

**Acceptance.**
- Tests forcing budget overflow.
- Per-role usage breakdown verified.

---

# Phase 19: PR creation

**Scope.** Optional `gh pr create` after a successful run.

**Deliverables.**
- `git::open_pr(title, body)` — shells out to `gh`.
- `cli run --pr` flag (or `git.create_pr = true` in config).
- PR body auto-generated from completed phases + remaining deferred items.

**Acceptance.**
- Tests with a mock `gh` script.
- Real `gh` not required in CI.

---

# Phase 20: Polish & end-to-end validation

**Scope.** Round out UX, write docs, validate against a real toy plan run by real Claude Code.

**Deliverables.**
- `cli run --dry-run` — uses `DryRunAgent` end-to-end so users can sanity-check a plan and config without spending tokens.
- Error messages reviewed for clarity (every `PlanParseError`, `DeferredParseError`, agent failure).
- `--version` and `--verbose` flags wired.
- `README.md` with: install, quickstart, configuration, model recommendations, troubleshooting.
- `examples/` with at least one example plan and walkthrough.
- A real run: hand-write a tiny plan ("build a CLI todo app in Rust"), run foreman against it with real Claude Code, document what happens, fix anything that breaks. This phase doesn't complete until the dogfood run completes.

**Acceptance.**
- Dogfood run produces a working todo app and passes its own tests.
- README covers every command and config key.
- `cargo doc --no-deps` produces clean docs with no broken links.