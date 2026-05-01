---
current_phase: "01"
---

This plan adds a periodic deferred-sweep step that fires between regular phases (and once after the final phase) when `## Deferred items` in `deferred.md` has piled up. Cross-cutting items (test flakes, doc fixes, refactors that don't unblock any current phase) currently sit forever because the implementer prompt only sweeps items relevant to the active phase. The sweep step reuses the per-phase dispatch pipeline (factored out in phase 01) with a sweep-specific prompt and a sweep-specific auditor pass. Phase 05 tracks per-item attempt counts so items that survive multiple sweeps get surfaced as "needs human attention" instead of clogging the backlog forever. `## Deferred phases` (H3 blocks) are out of scope and are protected by an extra byte-snapshot check during sweep dispatches; they behave more like design notes and need a separate promotion path. Each phase ends with `cargo test` and `cargo clippy --all-targets -- -D warnings` green; clippy and rustfmt match the existing crate.

# Phase 01: Extract the dispatch pipeline (no behavior change)

**Scope.** Refactor `Runner::run_phase_inner` (`src/runner/mod.rs:399-544`) to call a new shared helper that owns the dispatch → validate → tests → fixer-loop → optional auditor → stage chain. No behavior changes, no new events, no CLI surface, no test fixture changes. This is a structural prerequisite for phases 03 and 04, which introduce a sweep step and a sweep-specific auditor that share the bulk of this pipeline; duplicating ~140 lines of `run_phase_inner` would lock in drift between paths.

**Deliverables.**
- New private method `Runner::run_dispatch_pipeline(spec: DispatchSpec) -> Result<PipelineOutcome>` in `src/runner/mod.rs`. The pipeline runs: `dispatch_and_validate(spec.request)` → `run_tests` (when a runner is configured and `skip_tests` is false) → `run_fixer_loop` on test failure → `run_auditor_pass` when `spec.audit.is_some()` → `git.stage_changes(spec.exclude_paths)`. It returns `PipelineOutcome::Halted(HaltReason)` on any halt, or `PipelineOutcome::Staged { has_changes: bool }` when the chain ran clean.
- New `DispatchSpec` struct: `request: AgentRequest` (pre-built by the caller, including the per-attempt `log_path`); `phase_id: PhaseId` (used by the fixer loop's attempt tracking and event emission — for sweeps this is the `after_phase`, since `state.attempts` keys on real phase ids); `phase: Option<&Phase>` (carried for the fixer / auditor prompts; `None` when the pipeline is invoked outside a plan phase, see phase 03); `plan_path: &Path`, `deferred_path: &Path`, `exclude_paths: &[&Path]` for the snapshot / stage helpers; `audit: Option<AuditKind>`.
- New `AuditKind` enum scoped to the runner module, with one variant in this phase: `AuditKind::Phase { phase: &Phase }`. Phase 04 extends it with `AuditKind::Sweep { after: PhaseId, resolved: Vec<String>, remaining: Vec<String> }`. Both variants drive `run_auditor_pass` to pick the right prompt (phase 04 splits the prompt path).
- `run_fixer_loop` is generalized to accept `phase: Option<&Phase>` instead of `&Phase`. When `Some`, today's `prompts::fixer_with_deferred(plan, phase, summary, deferred)` is used unchanged. When `None`, a new `prompts::fixer_for_sweep(plan, deferred, summary)` renders a sweep-aware fixer prompt that frames the failure as "the test failure is in code you touched while sweeping the deferred list — fix it without expanding scope." The sweep variant is exercised by phase 03's tests; the regular path is identical to today and covered by existing tests.
- `run_phase_inner` is rewritten to:
  1. Resolve current phase + `check_budget` (today).
  2. `bump_attempts` and emit `PhaseStarted` (today).
  3. Build the implementer `AgentRequest` via a small extracted helper `Runner::implementer_request(&phase, attempt) -> AgentRequest`.
  4. Call `run_dispatch_pipeline` with `phase: Some(&phase)`, `audit: self.config.audit.enabled.then(|| AuditKind::Phase { phase: &phase })`.
  5. On `Halted(reason)` → return `PhaseResult::Halted`.
  6. On `Staged { has_changes }` → commit via `git::commit_message(&phase_id, &phase.title)` if `has_changes`, then today's `deferred.sweep` + `state.completed.push` + `current_phase` advance + `state::save` + emit `PhaseCommitted`.
- `attempt_log_path` (`src/runner/mod.rs:588`) is unchanged; the implementer path passes it as the log path on its own request. Phase 03 adds a `sweep_log_path` helper alongside it.
- All existing tests pass unchanged. No new tests; this is a refactor and the existing coverage is the test.

**Acceptance.**
- `cargo test --workspace` is green with zero diffs to existing test fixtures or insta snapshots.
- `cargo clippy --all-targets -- -D warnings` is clean.
- `dispatch_and_validate`, `run_tests`, `run_fixer_loop`, `run_auditor_pass`, `git.stage_changes`, and `git.commit` are each called from at most one place inside the runner (plus `run_phase_inner`'s small commit call). No second copy of the pipeline.
- `run_phase_inner` is now ~50 lines (down from ~145); the load is on the pipeline helper.

# Phase 02: Sweep prompt, config, and trigger function

**Scope.** Stand up the typed pieces phase 03 will consume — a new prompt template, a `[sweep]` config block, and a pure trigger function — without changing any runtime behavior. A fresh checkout still builds and `pitboss play` runs the existing pipeline unmodified. The trigger range and stale-item escalation threshold (consumed by phase 05) are configurable but defaulted; the agent is *not* told a numeric cap on items to resolve per sweep, because clamping a 7-easy-fixes sweep to 5 just defers the rest.

**Deliverables.**
- New template `src/prompts/templates/sweep.txt` modeled on `implementer.txt`. The prompt tells the agent: its job is `deferred.md`, not a `plan.md` phase; `## Deferred items` only (`## Deferred phases` is off-limits — touching an H3 block halts the run); fix as many items as reasonable in one session; mark each `- [x]` only if the item is actually done; do not add new items; do not modify item text (rewording resets staleness tracking — phase 05); `plan.md` and `.pitboss/` remain off-limits under the same byte-snapshot enforcement as a normal phase. The template includes a "Stale items" section the renderer fills in (phase 05 supplies real values; phase 02 leaves an empty placeholder so the template renders cleanly today).
- New `prompts::sweep(plan: &Plan, deferred: &DeferredDoc, after_phase: Option<&PhaseId>, stale_items: &[StaleItem]) -> String` renderer in `src/prompts/mod.rs`. `after_phase = None` covers a fresh-repo standalone `pitboss sweep` invocation (phase 06). The renderer strips already-checked items before substitution so the agent only sees pending work. New `prompts::StaleItem { text: String, attempts: u32 }` ships alongside; it's empty for now and populated by phase 05.
- New template `src/prompts/templates/sweep_fixer.txt` and `prompts::fixer_for_sweep(plan, deferred, summary)` renderer (phase 01 forward-defined the call site). The prompt frames the test failure as "you broke a test while sweeping deferred items — fix it within the same scope; do not expand to unrelated work."
- `src/config/mod.rs`: new `SweepConfig` struct, serde-defaulted: `enabled: bool` (default `true`), `trigger_min_items: u32` (default `5`), `trigger_max_items: u32` (default `8`, advisory only — see below), `max_consecutive: u32` (default `1`), `escalate_after: u32` (default `3`, consumed by phase 05), `audit_enabled: bool` (default `true`, consumed by phase 04). Add `sweep: SweepConfig` to `Config`. Validation: `trigger_min_items >= 1`, `trigger_min_items <= trigger_max_items`, `max_consecutive >= 1`, `escalate_after >= 1`.
- New module `src/runner/sweep.rs` exposing `should_run_deferred_sweep(deferred: &DeferredDoc, sweep_cfg: &SweepConfig, consecutive_sweeps: u32) -> bool`. Rule: `enabled && unchecked_items >= trigger_min_items && consecutive_sweeps < max_consecutive`. `trigger_max_items` is *advisory*: it documents the expected upper bound rather than gating behavior, since the sweep agent picks how many items to address. The same module exposes `unchecked_count(deferred: &DeferredDoc) -> usize` so callers (status, the trigger, the prompt renderer, phase 05's accounting) share one definition. Tests cover the on/off threshold, the `enabled = false` short-circuit, and the `max_consecutive` clamp.
- `insta` snapshot tests for `prompts::sweep`:
  - Representative `DeferredDoc` (5 unchecked items, 1 already-checked item filtered out, no `## Deferred phases`), empty `stale_items`.
  - Same items plus a non-empty `stale_items` slice (forward coverage for phase 05; the placeholder section renders cleanly today).
  - `after_phase = None` (standalone-sweep wording verified).
- Snapshot test for `prompts::fixer_for_sweep` with a representative test failure summary.
- Unit tests on `prompts::sweep`: placeholder substitution; already-checked items stripped; rendered text below `TEMPLATE_STATIC_BUDGET` for the empty case.

**Acceptance.**
- `cargo build` succeeds on a fresh `cargo clean`.
- `cargo test prompts::sweep`, `cargo test config::sweep`, `cargo test runner::sweep` all pass.
- `cargo clippy --all-targets -- -D warnings` is clean.
- `cargo insta test` snapshots are committed.
- `pitboss play --help` is byte-identical to before this phase.
- A `pitboss.toml` with no `[sweep]` section parses and applies the documented defaults.

# Phase 03: Runner integration — fire sweeps between phases and after the run

**Scope.** Wire phase 02's pieces into the runner so a sweep step fires between regular phases when the trigger is satisfied, and once more after the final phase if items remain. The sweep step calls `run_dispatch_pipeline` from phase 01 with `audit: None` for now; phase 04 turns the auditor back on with a sweep-specific prompt. State that survives a halt+resume goes on `RunState`. No CLI flags yet (phase 06); behavior is governed entirely by `pitboss.toml`. The sweep step does not introduce a synthetic phase id — `state.attempts` keys remain real `PhaseId` values, and sweep log files are distinguished by a `sweep-after-<phase-id>-` filename prefix.

**Deliverables.**
- New fields on `RunState` (`src/state/mod.rs`): `pending_sweep: bool` (default `false`) and `consecutive_sweeps: u32` (default `0`), both serde-defaulted so existing `state.json` files load.
- `Runner::run_phase_inner` end-of-phase change: after `self.deferred.sweep()`, the `state.completed.push`, and the `current_phase` advancement, call `should_run_deferred_sweep` on the post-sweep deferred doc. If `true`, set `state.pending_sweep = true`. `current_phase` still advances normally; the next call to `run_phase` notices `pending_sweep` and dispatches the sweep before the new `current_phase` runs.
- Final-phase case: when `next_phase` is `None`, evaluate `should_run_deferred_sweep` once more. If `true`, set `pending_sweep = true`. Extend `Runner::run`: after observing `Advanced { next_phase: None }`, if `state.pending_sweep` is set, call `run_phase` once more (which dispatches the trailing sweep) before emitting `RunFinished`.
- `Runner::run_phase` dispatch fork: at the top, if `state.pending_sweep` is `true`, *re-evaluate* `should_run_deferred_sweep` against the current on-disk deferred. If the trigger no longer fires (e.g., the user manually cleared items between resumes), set `state.pending_sweep = false`, persist state, and fall through to the regular path. Otherwise call `run_sweep_step(after)` where `after = state.completed.last().cloned()`.
- New private method `Runner::run_sweep_step(after: PhaseId) -> Result<PhaseResult>`:
  - Capture pre-sweep state: `pre_unchecked: usize` and `pre_texts: HashSet<String>` (the latter feeds phase 05's accounting).
  - Snapshot `## Deferred phases` byte-for-byte by serializing the parsed `DeferredDoc::phases` block and hashing it; this is the "sweep can't touch H3 blocks" guard. Verify it's unchanged after dispatch (in addition to the existing plan.md / deferred.md re-parse checks); on mismatch, halt with `HaltReason::DeferredInvalid("sweep modified Deferred phases")` and roll back from the pre-dispatch deferred snapshot.
  - Check budget; if exceeded, halt without dispatching.
  - Bump `state.attempts[after]` (sweep dispatches eat the same attempts budget as the phase they follow, so a stuck sweep doesn't get unlimited tries across resumes). Emit `Event::SweepStarted { after, items_pending: pre_unchecked, attempt }`.
  - Build the sweep `AgentRequest`: prompt from `prompts::sweep(... after_phase = Some(&after), stale_items = &[])` (phase 05 fills `stale_items`); `Role::Implementer`; log path `.pitboss/logs/sweep-after-{after}-implementer-{attempt}.log` via a new `Runner::sweep_log_path(after, role, attempt)` helper; standard system prompt and timeout.
  - Build the `DispatchSpec` with `phase: None`, `phase_id: after.clone()`, `audit: None`. Call `run_dispatch_pipeline`.
  - On `Halted(reason)`: leave `state.pending_sweep = true`, do not increment `state.consecutive_sweeps`, emit `Event::SweepHalted { after, reason }`, return `PhaseResult::Halted { phase_id: after, reason }`.
  - On `Staged { has_changes }`:
    - Re-parse `deferred.md` from disk and replace `self.deferred` so subsequent phases see the post-sweep state.
    - Compute `resolved = pre_unchecked.saturating_sub(unchecked_count(&self.deferred))`. New items added by the agent (the prompt forbids it but agents misbehave) don't pollute the count.
    - If `has_changes`: commit with `git::commit_message_sweep(&after, resolved)` producing `[pitboss] sweep after phase {after}: {n} deferred items resolved` (or `... no items resolved`).
    - If `!has_changes`: log a warning and skip the commit (mirrors today's "phase produced no code changes" branch at `src/runner/mod.rs:512-514`).
    - Clear `state.pending_sweep`, increment `state.consecutive_sweeps`, persist state.
    - Emit `Event::SweepCompleted { after, resolved, commit }`. Return `PhaseResult::Advanced { phase_id: after, next_phase: <plan current_phase>, commit }`. `state.completed` is *not* mutated — it tracks plan progress only.
- A regular phase commit resets `state.consecutive_sweeps = 0` so the `max_consecutive` clamp re-arms after every forward step.
- New events on `runner::Event`: `SweepStarted { after: PhaseId, items_pending: usize, attempt: u32 }`; `SweepCompleted { after: PhaseId, resolved: usize, commit: Option<CommitId> }`; `SweepHalted { after: PhaseId, reason: HaltReason }`. Phase 07 (TUI) consumes these.
- New helpers `git::commit_message_sweep(after: &PhaseId, resolved: usize) -> String` and `Runner::sweep_log_path(after: &PhaseId, role: &str, attempt: u32) -> PathBuf`.
- Integration tests in `tests/sweep_smoke.rs`:
  - **Trigger fires between phases**: two-phase plan; phase 01 leaves 6 unchecked items; sweep agent checks 4 off; assert exactly one `SweepStarted` between `PhaseCommitted { 01 }` and `PhaseStarted { 02 }`; `state.pending_sweep == false` post-sweep; `state.completed == [01]`; post-sweep `deferred.md` has 2 unchecked items; sweep commit message matches.
  - **Disable**: `[sweep] enabled = false` — no sweep fires even with 8 items pending.
  - **Consecutive clamp**: `max_consecutive = 1` (default) — back-to-back sweeps blocked even if items remain above threshold; the next regular phase commit resets the counter.
  - **Resume invariant**: a sweep that halts leaves `state.pending_sweep = true`; a follow-up `Runner::run_phase` retries the sweep before any phase 02 work runs.
  - **Manual cleanup re-evaluation**: `pending_sweep = true`; the user clears the deferred file by hand; the next `run_phase` re-evaluates the trigger, clears `pending_sweep`, and runs the regular phase.
  - **Final-phase trigger**: single-phase plan that leaves 5 unchecked items fires one trailing sweep before `RunFinished`.
  - **Empty sweep** (zero resolved, empty diff): no commit lands; `consecutive_sweeps` increments to 1; `pending_sweep` clears; the next phase runs.
  - **Sweep budget bookkeeping**: a sweep that takes 2 fixer attempts increments `state.attempts[after]` by 3 (implementer + 2 fixer); regression guard against the fixer attempt counter regressing to a local-only counter.
  - **Deferred-phases guard**: a sweep agent that modifies a `## Deferred phases` H3 block halts with `HaltReason::DeferredInvalid("sweep modified Deferred phases")` and the on-disk file is rolled back to the pre-dispatch state.

**Acceptance.**
- `cargo test --test sweep_smoke` passes.
- `cargo test --workspace` is green.
- `cargo clippy --all-targets -- -D warnings` is clean.
- `pitboss play` end-to-end with the dry-run agent (skip-tests + no-op dispatch) produces a sweep commit with the documented message format when a phase leaves ≥5 items.

# Phase 04: Sweep-aware auditor pass

**Scope.** Re-enable the auditor for sweeps with a sweep-specific prompt. The phase auditor's contract is "did the implementer stay within phase scope?", which doesn't apply when there is no phase. The sweep auditor's contract is: "for each item the implementer marked `- [x]`, does the diff actually do that work? if any changes look unrelated to the resolved items, revert them and re-stage." Same dispatch protocol as the existing phase auditor (file edits + post-audit test re-run); only the prompt and inputs differ. Without this, a sweep can bundle drive-by refactors into the same commit and ship.

**Deliverables.**
- New template `src/prompts/templates/sweep_auditor.txt` and `prompts::sweep_auditor(plan: &Plan, deferred: &DeferredDoc, after: &PhaseId, diff: &str, resolved: &[String], remaining: &[String], small_fix_line_limit: usize) -> String` renderer. Inputs: the staged `git diff --cached`; the items the agent claimed to resolve; the remaining unchecked items. The prompt asks the auditor to: (1) verify each resolved item is plausibly addressed by the diff, (2) revert any changes that look unrelated to the resolved items, (3) keep the deferred state consistent (an item the auditor reverts because the implementer didn't actually fix it gets unchecked again), (4) leave a one-line summary in the agent's stdout for log archaeology. `insta` snapshot covers a representative case.
- `AuditKind` (introduced in phase 01) gains a second variant: `AuditKind::Sweep { after: PhaseId, resolved: Vec<String>, remaining: Vec<String> }`.
- `run_auditor_pass` (`src/runner/mod.rs:792`) is generalized to dispatch on `AuditKind`:
  - `AuditKind::Phase { phase }` → today's `prompts::auditor_with_deferred(plan, phase, diff, deferred, small_fix_line_limit)`.
  - `AuditKind::Sweep { after, resolved, remaining }` → the new `prompts::sweep_auditor` renderer.
  - The rest of the helper (stage → diff → dispatch → re-validate → re-run tests on test_runner) is shared. Empty-diff short-circuit (`AuditorSkippedNoChanges`) applies to both kinds.
  - Log path: phase audits keep `attempt_log_path(phase_id, "audit", 1)`; sweep audits use `sweep_log_path(after, "audit", 1)` so logs don't collide.
- `run_sweep_step` (phase 03) is updated to pass `audit: self.config.sweep.audit_enabled.then(|| AuditKind::Sweep { after: after.clone(), resolved: resolved_texts, remaining: remaining_texts })` to the pipeline. The `resolved_texts` and `remaining_texts` lists are computed on the post-dispatch parse of `deferred.md` and threaded through to the audit step.
- New event variant or extension: `Event::AuditorStarted` (today carries `phase_id`) is extended to also carry the audit kind via a small `AuditContext { phase_id: PhaseId, kind: AuditContextKind }` payload, where `AuditContextKind = Phase | Sweep`. The TUI in phase 07 uses this to show the right header text. `AuditorSkippedNoChanges` similarly carries the kind. Existing event consumers (CLI logger) keep working — the phase id is still in the payload.
- The order of operations inside `run_sweep_step` becomes: dispatch sweep implementer → run tests → fixer loop on failure → run sweep auditor (when `[sweep].audit_enabled`) → re-stage → commit. This mirrors `run_phase_inner` exactly, so the pipeline helper from phase 01 needs no further changes.
- Tests in `tests/sweep_auditor.rs`:
  - **Approve path**: sweep agent resolves 3 items with a focused diff; auditor adds no edits; tests pass post-audit; sweep commits.
  - **Auditor reverts off-scope changes**: sweep agent marks 2 items off but the diff also touches an unrelated file; auditor reverts the unrelated file and re-stages; tests pass; sweep commits with only the in-scope changes.
  - **Auditor halts via test failure**: auditor's edits break a test; sweep halts with `HaltReason::TestsFailed`; `pending_sweep` stays true.
  - **Auditor disabled**: `[sweep] audit_enabled = false` skips the sweep auditor entirely (parity with `[audit] enabled = false` for phases).
  - **Empty diff after sweep**: sweep agent makes no code changes; sweep auditor short-circuits via `AuditorSkippedNoChanges`; sweep proceeds to the no-commit branch.

**Acceptance.**
- `cargo test --test sweep_auditor` passes.
- `cargo test --workspace` is green.
- `cargo clippy --all-targets -- -D warnings` is clean.
- `cargo insta test` covers the new sweep auditor prompt snapshot.
- A sweep run with `[audit] enabled = true` and `[sweep] audit_enabled = false` runs the phase auditor for phases but skips it for sweeps (independent toggles confirmed by integration test).

# Phase 05: Stale-item tracking and escalation

**Scope.** Track per-item attempt counts across sweeps so items that survive multiple sweeps get surfaced as "needs human attention" instead of clogging the backlog forever. The data lives on `RunState` (cheap, atomic, survives resume). Stale items get a dedicated section in subsequent sweep prompts and are surfaced via events for `pitboss status` (phase 06) and the TUI (phase 07).

**Deliverables.**
- New field on `RunState`: `deferred_item_attempts: HashMap<String, u32>` (default empty, serde-defaulted). Keyed on raw item text (items are short and unlikely to collide; rephrasing resets the counter, which is acceptable — a rewritten item is effectively new work). The phase-02 sweep prompt already forbids rewording, so the counter resetting on rewrite is a documented consequence rather than a silent gotcha.
- `run_sweep_step` (phase 03) updates the map after every sweep:
  - Capture `pre_texts: HashSet<String>` from pre-sweep unchecked items (already captured in phase 03 for the resolved-count math).
  - After re-parsing post-sweep `deferred.md`, capture `post_unchecked_texts: HashSet<String>`.
  - For each `text in pre_texts ∩ post_unchecked_texts`: `state.deferred_item_attempts.entry(text).and_modify(|n| *n += 1).or_insert(1)`.
  - Prune entries whose key isn't in `post_unchecked_texts` (resolved items shouldn't carry a counter).
  - On a *halted* sweep (no commit), still increment counters for items the agent failed to resolve. A halt is not a free pass on the staleness clock. This is intentionally different from `consecutive_sweeps`, which only increments on success: the two counters track different things — `consecutive_sweeps` is runner backoff, `deferred_item_attempts` is per-item progress.
- The sweep prompt renderer (`prompts::sweep`, defined in phase 02) is now invoked with a real `stale_items` slice: items where `attempts >= sweep_cfg.escalate_after`, ordered by descending `attempts`, capped at 10 entries to keep the prompt bounded. The prompt template's stale-items section explicitly tells the agent: "these items have resisted previous sweeps; consider whether they should be promoted to a `## Deferred phase` H3 block, rewritten for clarity, or removed as obsolete." The phase 02 snapshot test that already covered non-empty stale items now exercises this real path.
- The sweep auditor prompt (phase 04) is similarly extended: stale items are passed to `prompts::sweep_auditor` so the auditor knows which items are "high stakes" and can be more critical about whether they're actually resolved.
- New event `Event::DeferredItemStale { text: String, attempts: u32 }` emitted at the end of `run_sweep_step` for each item whose `attempts` *just crossed* `escalate_after` (transition-only: 2→3 fires once; 3→4 doesn't refire, to avoid spamming the activity log on every subsequent sweep).
- New helper `Runner::stale_items(&self) -> Vec<StaleItem>` returning items where `attempts >= self.config.sweep.escalate_after`, used by the prompt renderer, by `pitboss status` (phase 06), and by the TUI (phase 07).
- Tests in `tests/sweep_staleness.rs`:
  - **Counter increments**: 3 sweeps each leaving the same 2 items unchecked → those 2 items have `attempts == 3` in state.
  - **Counter resets**: an item resolved on the 2nd sweep is removed from the map.
  - **Halt still counts**: a halted sweep increments the counter for surviving items.
  - **Escalation event**: the third sweep emits `DeferredItemStale` for an item that just crossed `escalate_after = 3`; a fourth sweep does not re-emit for the same item (transition-only).
  - **Stale items in prompt**: the rendered sweep prompt for the third sweep includes the stale item under the "Stale items" section.
  - **Stale items cap**: with 15 items each at `attempts = 4`, the rendered prompt shows only 10, ordered by descending `attempts`.
  - **Pruning on text rewrite**: an item whose text is rewritten by the agent (despite the prompt forbidding it) results in the old key being pruned and a new key starting at 1 — verified by an explicit unit test on the bookkeeping helper.
  - **Resume across staleness**: a `state.json` written before this phase loads with an empty `deferred_item_attempts` map; the next sweep starts populating it cleanly.

**Acceptance.**
- `cargo test --test sweep_staleness` passes.
- `cargo test --workspace` is green.
- `cargo clippy --all-targets -- -D warnings` is clean.
- A `state.json` from phase 04 (no `deferred_item_attempts`) loads cleanly with an empty map.

# Phase 06: CLI surface — flags, `pitboss sweep` subcommand, and status

**Scope.** Surface sweep state on the CLI and let an operator drive it manually. Adds two flags on `pitboss play`, a new `pitboss sweep` subcommand for one-shot backlog cleanup outside of a play run, and extends `pitboss status` with a sweep section that includes stale items.

**Deliverables.**
- `pitboss play --no-sweep`: clears `state.pending_sweep` at the top of the run and forces `should_run_deferred_sweep` to return `false` for the duration of the invocation (does not write to `pitboss.toml`). Implemented as a runner-level override flag set via a builder method, mirroring `skip_tests`.
- `pitboss play --sweep`: forces `state.pending_sweep = true` before the next phase even if the trigger threshold isn't met; useful for "I just edited deferred.md by hand, run a sweep now."
- The two flags are mutually exclusive at the clap level.
- New `pitboss sweep` subcommand (`src/cli/sweep.rs`): one-shot sweep without advancing the plan. Flags:
  - `--max-items <N>`: caps the prompt's pending-items list to the first N items (by document order). For pathological 100+ item backlogs that exceed the agent's effective context. The remaining items stay in the file untouched and surface on the next sweep.
  - `--audit / --no-audit`: override `[sweep] audit_enabled` for this invocation only.
  - `--dry-run`: uses the no-op agent like `pitboss play --dry-run`.
  - `--after <phase-id>`: override the `after_phase` label in the prompt (defaults to `state.completed.last()`, falling back to `None` when no run has started).
  Loads workspace state, builds a `Runner`, calls a new `Runner::run_standalone_sweep(after: Option<PhaseId>, max_items: Option<usize>) -> Result<PhaseResult>`. This wraps `run_sweep_step` with no state-machine advancement on either side and supports the `max_items` truncation by passing a clamped `DeferredDoc` view to the prompt renderer (the on-disk file is unchanged). Persists `state.json` on exit. Exits 0 on a successful sweep (committed or no-changes) and 1 on a halt.
- `pitboss status` (`src/cli/status.rs`) gains a "Sweep" block:
  ```
  Sweep:
    pending: <bool>
    consecutive: <n>
    deferred items: <unchecked> unchecked / <total> total
    stale items: <k> (need attention)
      - "<text>" (tried <n> times)
      ...
  ```
  Stale items list shows up to 5 entries with `attempts >= escalate_after`, ordered by descending attempts. A footer line on the stale-items section explicitly suggests: "Promote a stale item to a `## Deferred phase` H3 block, rewrite the text, or check it off if obsolete." Reuses formatting helpers already present.
- Update `pitboss play --help` text to document the new flags and reference the `[sweep]` config block. Update `pitboss --help` to list the new `sweep` subcommand. Update `pitboss sweep --help` with the documented flags.
- Tests:
  - `assert_cmd` integration test for `pitboss play --no-sweep --sweep` exiting non-zero with a clap error.
  - `pitboss status` snapshot test in five states: no pending sweep + no items; pending sweep + 6 items; just-finished sweep with `consecutive=1`; stale items present (3 items at various `attempts`); large backlog (20+ items, no stale yet).
  - `pitboss play --no-sweep` against a repo with 10 deferred items skips the sweep that would otherwise fire (no `SweepStarted` event).
  - `pitboss play --sweep` against a repo with 2 deferred items fires a sweep before the next phase.
  - `pitboss sweep --dry-run` against a repo with 7 deferred items runs through the sweep pipeline with the no-op agent and produces no commit.
  - `pitboss sweep --max-items 5` against a repo with 20 deferred items renders a prompt that includes only the first 5 pending items (verified by reading the agent log).
  - `pitboss sweep` exits 1 when the sweep halts (e.g., the auditor halts via test failure) and `state.pending_sweep` is left true on disk.

**Acceptance.**
- `pitboss --help` shows the `sweep` subcommand.
- `pitboss play --help` shows `--sweep` and `--no-sweep`.
- `pitboss sweep --help` shows the documented flags.
- `pitboss status` in a fresh repo prints the Sweep block with `pending=false consecutive=0`.
- `cargo test --test cli_status` snapshot covers the five states.
- `cargo clippy --all-targets -- -D warnings` is clean.

# Phase 07: TUI audit and sweep rendering

**Scope.** Make sure the sweep step (and its auditor and stale-item events) render cleanly in the existing ratatui dashboard (`src/tui/app.rs`) and that every new event added in phases 03–05 threads through the dashboard. Half audit, half implementation. No changes to runner or CLI behavior.

**Deliverables.**
- Audit pass on `src/tui/app.rs::handle_event` (`src/tui/app.rs:241-323`): every `Event::*` variant has a matching arm; add arms for `SweepStarted`, `SweepCompleted`, `SweepHalted`, `DeferredItemStale`, and the `AuditContext`-extended `AuditorStarted` / `AuditorSkippedNoChanges`. Confirm no variant is silently dropped. The match must remain exhaustive — drop any `_ =>` fallback that was added during phases 03–05. A test exercises the dispatch by sending one of every event variant in sequence and asserting the resulting `App` state for each.
- Header rendering: while a sweep is in flight, the phase header row reads `Sweep after phase 01 — attempt 1` (mirroring the existing `Phase 01: <title> — attempt 1` format). When the sweep auditor runs, the header transitions to `Sweep after phase 01 — auditor`. On `SweepCompleted`, the header transitions back to whatever the next regular `Event::PhaseStarted` reports. The session-stats panel from the deferred phase 04 entry continues to tick during a sweep — verified by hand and by snapshot.
- Output pane: agent output during a sweep flows into the same scrollable pane as a regular phase. The wrap-aware scrolling fix from the deferred phase-08 entry must not regress; the existing `render_keeps_latest_line_visible_when_earlier_lines_wrap` test stays green.
- Sweep summary line on completion: when `SweepCompleted` fires, append a one-line entry to the activity log the TUI uses for `PhaseCommitted` — `sweep after 01: 3 items resolved` (or `0 items resolved`). Match the visual style and color of the existing commit row. When `SweepHalted` fires, append a halt row in the same style as `PhaseHalted`.
- Stale items panel: a new collapsible panel under the existing layout shows up to 5 stale items, populated from `DeferredItemStale` events at runtime and rehydrated on TUI startup from `state.deferred_item_attempts` (filtered through `Runner::stale_items` from phase 05). Stale items render in a distinct color so they catch the eye but don't dominate the dashboard. The panel auto-collapses when the list is empty. On terminals shorter than `STATS_HEIGHT + STALE_HEIGHT + 4`, the stale panel is the first to drop, mirroring the existing height heuristic for the session-stats panel.
- Defensive checks: a `SweepStarted` arriving without a preceding `PhaseCommitted` or run-start, or a `SweepCompleted` without a preceding `SweepStarted`, logs a debug-level message and renders from whatever state we have — no panic. Tests cover both out-of-order cases.
- New `insta` snapshot fixtures under `src/tui/snapshots/`:
  - `pitboss__tui__app__tests__sweep_in_flight.snap` — mid-sweep frame.
  - `pitboss__tui__app__tests__sweep_completed.snap` — post-sweep frame with the resolved-items activity-log line.
  - `pitboss__tui__app__tests__sweep_auditor.snap` — sweep auditor in progress.
  - `pitboss__tui__app__tests__stale_items_panel.snap` — frame with two stale items in the panel.
  - `pitboss__tui__app__tests__sweep_halted.snap` — frame after a sweep halts via auditor test failure.
- An exhaustiveness test (a `match` over `Event` in test code) compiles, proving no variant was added without a TUI handler.
- Manual checklist appended to the TUI module rustdoc: how to drive each new sweep frame by hand for visual verification (the existing user stance is that TUI behavior is verified manually, not in CI).

**Acceptance.**
- `cargo test tui` passes including the five new snapshot tests.
- `cargo insta test` snapshots are reviewed and committed.
- The exhaustiveness test compiles.
- Manual smoke: `pitboss play --tui` against a repo set up to trigger a sweep produces frames matching the snapshots (verified by hand, recorded in the PR description).
- `cargo clippy --all-targets -- -D warnings` is clean.
- `cargo test --workspace` passes — all prior phases' tests still green.
