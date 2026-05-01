<div align="center">

  <img src="assets/pitboss-wordmark.svg" alt="nyx" height="110"/>


**A coding-agent pitboss.** Hand it a phased plan, walk away, come back to a branch full of green commits.

[![crates.io](https://img.shields.io/crates/v/pitboss.svg)](https://crates.io/crates/pitboss)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-CE422B?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-007EC6)](#license)
[![Agents](https://img.shields.io/badge/agents-Claude%20%C2%B7%20Codex%20%C2%B7%20Aider%20%C2%B7%20Gemini-7C3AED)](#agent-backends)
[![CI](https://github.com/elicpeter/pitboss/actions/workflows/ci.yml/badge.svg)](https://github.com/elicpeter/pitboss/actions/workflows/ci.yml)

</div>

Pitboss is a Rust CLI that drives a coding agent through a multi-phase implementation plan. Claude Code is the default; OpenAI's Codex CLI, Aider, and Gemini CLI are also wired in and selectable from `.pitboss/config.toml` (see [Agent backends](#agent-backends)). It runs your test suite after every phase, retries failures with a fixer agent, audits the diff, lands a commit, then moves on. Bounded retries everywhere. Token and dollar budgets. A live TUI if you want to watch.

<div align="center">
  <img src="assets/pitboss-tui.png" alt="pitboss play --tui dashboard" width="900"/>
</div>
<div align="center">
  <sub align="center"><i>`pitboss play --tui`. The dashboard. Phases on the left, live agent output on the right.</i></sub>
</div>

## Contents

- [How it works](#how-it-works)
- [Install](#install)
- [Quickstart](#quickstart)
- [Generating a plan](#generating-a-plan)
- [The run loop](#the-run-loop)
- [Sweeps: draining the deferred backlog](#sweeps-draining-the-deferred-backlog)
- [Grind: rotating prompt runner](#grind-rotating-prompt-runner)
- [Configuration](#configuration)
- [Agent backends](#agent-backends)
- [Test runner detection](#test-runner-detection)
- [Dry runs and verbose output](#dry-runs-and-verbose-output)
- [Workspace layout](#workspace-layout)
- [Troubleshooting](#troubleshooting)
- [Contributing](#contributing)
- [License](#license)

## How it works

Three files do the work.

All three live under `.pitboss/`, which is gitignored. Pitboss never writes to your tracked files.

| File | Owner | Contents |
|------|-------|----------|
| `.pitboss/play/plan.md` | you | The phases. Read-only to agents. |
| `.pitboss/play/deferred.md` | the agent | Anything the agent couldn't finish in a phase. Swept between phases. |
| `.pitboss/play/state.json` | pitboss | Run id, branch, attempts, token usage. |
| `.pitboss/config.toml` | you | Agent backend, models, budgets, retries. |

Each phase becomes its own commit on a per-run branch, optionally rolled into a pull request when the run finishes.

## Install

### From crates.io

```sh
cargo install pitboss
```

This pulls the latest published release and drops the `pitboss` binary in `~/.cargo/bin`. Requires a stable Rust toolchain (1.88 or newer).

### Prebuilt binaries

Each tagged release ships static builds on the [Releases page](https://github.com/elicpeter/pitboss/releases) for:

- `x86_64-unknown-linux-gnu`
- `x86_64-apple-darwin` (Intel macOS)
- `aarch64-apple-darwin` (Apple Silicon macOS)

Pick the archive that matches your platform, verify the checksum, and drop the binary on your `PATH`:

```sh
TAG=v0.1.0                                # whichever release you want
TARGET=aarch64-apple-darwin               # or x86_64-apple-darwin / x86_64-unknown-linux-gnu
ARCHIVE="pitboss-${TAG}-${TARGET}.tar.gz"

curl -fsSLO "https://github.com/elicpeter/pitboss/releases/download/${TAG}/${ARCHIVE}"
curl -fsSLO "https://github.com/elicpeter/pitboss/releases/download/${TAG}/${ARCHIVE}.sha256"
shasum -a 256 -c "${ARCHIVE}.sha256"

tar -xzf "${ARCHIVE}"
install "pitboss-${TAG}-${TARGET}/pitboss" /usr/local/bin/pitboss
```

### From source

```sh
git clone https://github.com/elicpeter/pitboss
cd pitboss
cargo install --path .
```

### Runtime dependencies

To actually drive the agent you also need:

- **`claude`**, the Claude Code CLI from Anthropic. Required for the default backend; optional if you select a different one in `.pitboss/config.toml`.
- **`git`**, any reasonably recent version.
- **`gh`** (optional), only if you want `--pr` to open pull requests.

If you plan to swap backends, install whichever CLI you intend to use instead of (or in addition to) `claude`:

- **`codex`**, only if `[agent] backend = "codex"`. See [Agent backends](#agent-backends).
- **`aider`**, only if `[agent] backend = "aider"`.
- **`gemini`**, only if `[agent] backend = "gemini"`.

## Quickstart

```sh
mkdir my-project && cd my-project
git init
pitboss init                # scaffold .pitboss/{config.toml, play/{plan.md, deferred.md, ...}}
$EDITOR .pitboss/play/plan.md   # describe the work, phase by phase
pitboss play --dry-run      # exercise the runner without spending tokens
pitboss play                # let the agent loop drive the plan
pitboss status              # check progress at any time
```

`pitboss status` looks like this:

<div align="center">
  <img src="assets/pitboss-status.png" alt="pitboss status output" width="700"/>
</div>

A few entry points worth knowing:

- `pitboss plan "build a CLI todo app in Rust"` has the planner agent draft `plan.md` for you. Add `--interview` to answer design questions first and get a more targeted plan (see [Generating a plan](#generating-a-plan)).
- `pitboss play --tui` swaps the stderr logger for the dashboard above.
- `pitboss play --pr` (or `git.create_pr = true`) opens a pull request with `gh pr create` after the run finishes.
- `pitboss rebuy` picks up where a halted run left off (buying back into the table).
- `pitboss fold --checkout-original` marks the run folded and switches HEAD back to the branch you were on before `pitboss play`.
- The pre-rename verbs (`pitboss run`, `pitboss resume`, `pitboss abort`) are kept as aliases, so existing scripts and muscle memory keep working.

## Generating a plan

`pitboss plan "my goal"` calls the planner agent to draft `plan.md`. Give it a plain description of the feature or change; the planner also reads the repo layout, manifests, and README for context.

```sh
pitboss plan "add JSON export to the audit report"
pitboss plan "add JSON export to the audit report" --force   # overwrite an existing plan.md
```

### Interview mode

Pass `--interview` and pitboss runs a design session before calling the planner. The agent generates targeted questions about your goal, asks them one by one in the terminal, and uses your answers to write a more concrete `plan.md`.

```sh
pitboss plan --interview "add a --watch mode to the build CLI"
```

<div align="center">
  <img src="assets/pitboss-interview.png" alt="pitboss plan --interview session" width="900"/>
</div>
<div align="center">
  <sub align="center"><i>`pitboss plan --interview`. The agent asks design questions, you answer, then the planner runs with the full context.</i></sub>
</div>

Questions cover things like interface design, data structures, edge cases, and test approach. Press Enter to skip any question you don't want to answer. The Q&A is compiled into a design spec and handed to the planner alongside the goal, so the resulting plan reflects decisions you made up front rather than ones the agent guessed at.

The number of questions varies with the goal; the agent caps at 50.

## The run loop

For each phase in `plan.md`:

1. Snapshot `plan.md` and `deferred.md` (SHA-256).
2. Dispatch the **implementer** agent with the active phase, the unfinished deferred work, and the user prompt template.
3. If the agent modified `plan.md`, restore the snapshot and halt.
4. Re-parse `deferred.md`. On parse failure, restore the snapshot and halt.
5. Run the project test suite. If it fails, dispatch the **fixer** agent up to `retries.fixer_max_attempts` times.
6. Stage the diff and dispatch the **auditor** agent (when `audit.enabled = true`). The auditor inlines small fixes and records anything larger in `deferred.md`. Tests run again post-audit.
7. Commit the staged diff to the per-run branch as `[pitboss] phase <id>: <title>`. The entire `.pitboss/` directory is gitignored, so nothing pitboss writes lands in your commits.
8. Prune checked-off deferred items, advance `current_phase` in `plan.md`, persist `state.json`, move on. If the unchecked-item count is over `[sweep] trigger_min_items`, the next dispatch is a sweep instead of the next phase. See [Sweeps](#sweeps-draining-the-deferred-backlog).

Every retry is bounded. When a budget is exhausted the runner halts with a clear reason and `pitboss rebuy` picks up from the same phase.

<div align="center">
  <img src="assets/pitboss-halt.png" alt="pitboss TUI halted on budget exceeded" width="900"/>
</div>
<div align="center">
  <sub align="center"><i>USD budget tripped mid-phase. Pitboss halts, no commit lands, `pitboss rebuy` picks up from phase 02.</i></sub>
</div>

## Sweeps: draining the deferred backlog

When the implementer or auditor cannot finish something inside a phase, it lands in `deferred.md` as an unchecked checkbox. Without a way to drain that file the backlog grows forever, the agent re-reads the same items every phase, and small papercuts pile up until they are no longer small. A sweep is a side dispatch between phases that reads the pending list, makes its edits, runs tests, and commits. The plan does not advance.

After every commit the runner counts unchecked items. Once the count reaches `trigger_min_items` (default 5), the next phase boundary becomes a sweep instead of a regular phase. The sweep agent gets a curated prompt with the pending items and a soft cap of `trigger_max_items` (default 8) on how many to address in one pass. The cap is advisory: pitboss sends the full pending list and the agent decides what fits. The sweep commit lands as `[pitboss] sweep after phase <id>`.

After a sweep, the runner re-evaluates the gate. If the count is still over the threshold and `max_consecutive` (default 1) sweeps have not run back-to-back yet, another sweep dispatches. Otherwise the loop returns to the next regular phase. The cap exists so a backlog the agent cannot drain does not livelock the run.

Items that survive a sweep carry a per-item attempt counter. When a counter crosses `escalate_after` (default 3), the auditor flags the item as stale on its next pass so a human can look at it. The counter resets the moment the item gets checked off.

When `audit_enabled` is on (the default), the auditor pass runs after a sweep the same way it runs after a phase. Small fixes get inlined; larger findings are recorded back into `deferred.md`.

After the last regular phase commits, the runner enters a bounded final-sweep loop to drain whatever is left. The loop runs at most `final_sweep_max_iterations` (default 3) iterations and exits early when the unchecked count hits zero or an iteration resolves no items. `final_sweep_enabled` is independent of `enabled`, so you can keep the trailing drain on while disabling between-phase sweeps, or vice versa.

```toml
[sweep]
enabled                    = true   # master switch for between-phase sweeps
trigger_min_items          = 5      # unchecked count that arms the gate
trigger_max_items          = 8      # advisory cap surfaced to the agent
max_consecutive            = 1      # back-to-back sweeps before a real phase must run
escalate_after             = 3      # sweep attempts an item survives before staleness escalation
audit_enabled              = true   # run the auditor after a sweep, same as after a phase
final_sweep_enabled        = true   # drain loop after the final regular phase
final_sweep_max_iterations = 3      # cap on drain-loop iterations
```

### Manual sweeps

Three ways to override the gate:

```sh
pitboss sweep                  # one-shot sweep, no plan advancement
pitboss play --sweep           # next phase boundary is a sweep, threshold ignored
pitboss play --no-sweep        # suppress sweeps entirely for this run
```

`pitboss sweep` runs the same pipeline the inter-phase gate dispatches, without touching `current_phase`. Useful after editing `deferred.md` by hand or to drain a backlog ahead of the next `pitboss play`. Flags:

- `--max-items <N>` clamps the prompt's pending list to the first N items in document order. For pathological 100+ item backlogs that would otherwise blow past the agent's effective context. The on-disk file is unchanged; remaining items surface on the next sweep.
- `--audit` / `--no-audit` overrides `[sweep] audit_enabled` for this invocation only.
- `--after <phase-id>` overrides the prompt's `after_phase` label. Defaults to the most recently completed phase, or none when no run has started.
- `--dry-run` swaps the agent for the deterministic no-op, mirroring `pitboss play --dry-run`.

Exit code is 0 on a clean sweep (committed or no changes) and 1 on a halt. State is persisted on the way out so a halted sweep can be retried.

`pitboss play --no-sweep` and `pitboss rebuy --no-sweep` clear any inherited `pending_sweep` flag at startup and refuse to arm the gate from any subsequent commit. The override is in-memory; the `[sweep]` block in `.pitboss/config.toml` is untouched. `--sweep` and `--no-sweep` are mutually exclusive.

## Grind: rotating prompt runner

`pitboss grind` is the second execution mode. Where `play` walks one phased plan and stops, `grind` rotates through a set of user-authored markdown prompts and keeps going until a budget trips or you Ctrl-C it. No auditor, no fixer cycle, no `plan.md`. One session at a time by default; prompts marked `parallel_safe` can fan out into worktrees.

Reach for it when the work is naturally a queue rather than a sequence: recurring lint sweeps, doc passes, snapshot refreshes, dependency bumps across a monorepo, anything you'd otherwise run by hand on a loop.

```sh
pitboss prompts new fix-lints     # scaffold a prompt under .pitboss/grind/prompts/
$EDITOR .pitboss/grind/prompts/fix-lints.md
pitboss grind --tui               # rotate through every discovered prompt
```

<div align="center">
  <img src="assets/pitboss-grind.png" alt="pitboss grind --tui dashboard" width="900"/>
</div>
<div align="center">
  <sub align="center"><i>`pitboss grind --tui`. Sessions on the left with status glyphs (`+` ok, `~` dirty, `x` error, `>` in flight), live agent output on the right, run-level budgets and the next scheduler pick along the bottom.</i></sub>
</div>

A few flags worth knowing:

- `--rotation <name>` loads a specific rotation file from `.pitboss/grind/rotations/<name>.toml`. Without it, grind builds a default rotation over every discovered prompt.
- `--max-iterations N`, `--until <RFC3339>`, `--max-cost <USD>`, `--max-tokens N` cap the run. The first one to trip halts the loop with exit code 3.
- `--resume [<run-id>]` picks up a halted run on the same branch. With no argument, grind picks the most recent active or aborted run.
- `--pr` opens a pull request when the run finishes cleanly; `--require-pr` upgrades a failed `gh pr create` to a non-zero exit.
- `--dry-run` resolves the rotation, prints the discovered prompts, the budget plan, and the first few scheduler picks, then exits without dispatching anything.

Each grind run gets its own branch (`pitboss/grind/<run-id>`) and a per-run directory under `.pitboss/grind/runs/<run-id>/` containing `state.json`, the source-of-truth `sessions.jsonl` log, a rendered `sessions.md`, per-session transcripts, and (for parallel sessions) a `worktrees/` subtree.

## Configuration

Pitboss reads `.pitboss/config.toml`. Every section is optional, missing keys fall back to defaults. Unknown keys load with a warning so a config written by a newer pitboss still works.

```toml
# Per-role model selection. Strings pass verbatim to the agent (e.g.
# `claude --model <id>`), so they must be valid model identifiers.
[models]
planner     = "claude-opus-4-7"
implementer = "claude-opus-4-7"
auditor     = "claude-opus-4-7"
fixer       = "claude-opus-4-7"

# Bounded retries. No infinite loops.
[retries]
fixer_max_attempts = 2   # 0 disables the fixer entirely
max_phase_attempts = 3

# Auditor pass. ON by default. Disable to commit straight after tests pass.
[audit]
enabled              = true
small_fix_line_limit = 30   # line threshold separating "inline" from "defer"

# Deferred-item sweeps. See "Sweeps" above for the full picture.
[sweep]
enabled                    = true
trigger_min_items          = 5
trigger_max_items          = 8
max_consecutive            = 1
escalate_after             = 3
audit_enabled              = true
final_sweep_enabled        = true
final_sweep_max_iterations = 3

# Per-run branch and optional PR.
[git]
branch_prefix = "pitboss/run-"   # full branch is <prefix><utc_timestamp>
create_pr     = false            # equivalent to `pitboss play --pr`

# Test runner override. Leave commented to auto-detect.
# [tests]
# command = "cargo test --workspace"

# Cost guard. Either limit being set activates budget enforcement: the
# runner halts before the next dispatch that would exceed the cap.
[budgets]
# max_total_tokens = 1_000_000
# max_total_usd    = 5.00

# Override or extend the default per-model price points. Defaults cover
# claude-opus-4-7, claude-sonnet-4-6, and claude-haiku-4-5.
# [budgets.pricing.claude-opus-4-7]
# input_per_million_usd  = 15.0
# output_per_million_usd = 75.0

# Caveman mode. Off by default. See "Caveman mode" below.
[caveman]
enabled   = false
intensity = "full"   # one of: lite, full, ultra
```

### Per-role model recommendations

The defaults set every role to the latest Opus, which is fine if you don't want to think about it. For a cheaper run, split it like this:

| Role          | Model                | Rationale                                            |
| ------------- | -------------------- | ---------------------------------------------------- |
| `planner`     | `claude-opus-4-7`    | One careful plan up front saves dozens of bad phases. |
| `implementer` | `claude-opus-4-7`    | Most of the spend, most sensitive to capability.     |
| `auditor`     | `claude-sonnet-4-6`  | Diff review and short-form notes. Sonnet handles it. |
| `fixer`       | `claude-sonnet-4-6`  | Test fix-ups are usually small and local.            |

Configure pricing for any model you reference in `[models]` so `pitboss status` and the USD budget check produce accurate numbers.

### Caveman mode

Pitboss can prepend a "talk like caveman" directive to every agent system prompt to cut output tokens. The idea comes from the [caveman skill](https://github.com/JuliusBrussee/caveman): drop articles, filler words, and pleasantries while keeping technical content exact. Output drops by roughly 65 to 75 percent on prose. Code blocks, commit messages, PR descriptions, and the structured `plan.md` and `deferred.md` artifacts stay in their normal format so downstream parsing is not affected.

Off by default. Flip `enabled = true` in the `[caveman]` block above to turn it on. Three intensity levels:

| Level   | What it does                                                                       |
| ------- | ---------------------------------------------------------------------------------- |
| `lite`  | Drops filler and hedging only. Keeps articles and full sentences. Lowest risk.     |
| `full`  | Drops articles too, allows fragments, prefers short synonyms. The skill's default. |
| `ultra` | Heavy abbreviation (DB, auth, fn, impl). Arrows for causality. Most compression.   |

Works on every backend. Claude Code receives the directive via `--append-system-prompt`; Codex and Aider get it concatenated ahead of the user prompt.

One tradeoff worth knowing. The planner, fixer, and auditor each produce output that the next role reads as input. Terser plan and audit prose can lose detail the next role would have used. A reasonable approach is to start with `lite`, watch a run or two, then move up to `full` or `ultra` if the plans and audits still hold up.

## Agent backends

Pitboss dispatches every implementer / fixer / auditor / planner role through a pluggable backend that wraps an external coding-agent CLI. Pick one in `.pitboss/config.toml`:

```toml
[agent]
backend = "claude_code"   # one of: claude_code (default), codex, aider, gemini
```

Each backend has its own optional sub-table for binary path, extra arguments, and a model override that wins over the role-level `[models]` table when set:

```toml
[agent.<backend>]
binary     = "/usr/local/bin/<cli>"   # default: resolve on PATH
extra_args = ["--flag", "value"]      # appended to every invocation
model      = "<model-id>"             # optional, beats [models].<role>
```

Omit `[agent]` entirely and pitboss runs with `claude_code` and PATH-resolved `claude`, same as it always has.

### Claude Code (default)

The reference backend, built on Anthropic's Claude Code CLI. Streams structured JSON events, populates `AgentOutcome` directly from them, and is the only backend that exercises every code path the runner relies on.

- **Binary:** `claude` ([install](https://docs.anthropic.com))
- **Config:**
  ```toml
  [agent]
  backend = "claude_code"

  [agent.claude_code]
  # binary, extra_args, model are all optional
  ```
- **Limitations:** none known.

### OpenAI Codex CLI

Wraps OpenAI's `codex` CLI. The agent concatenates the system and user prompts, pipes them on stdin, and parses the newline-delimited JSON event stream into `AgentOutcome`.

- **Binary:** `codex`
- **Config:**
  ```toml
  [agent]
  backend = "codex"

  [agent.codex]
  model = "gpt-5-codex"
  ```
- **Limitations:** none known.

### Aider

Wraps the `aider` CLI. The phase prompt is delivered via inline `--message <body>`; output parsing keys off Aider's plain-text edit/commit prefixes (`Applied edit to ...`, `Commit ...`).

- **Binary:** `aider`
- **Config:**
  ```toml
  [agent]
  backend = "aider"

  [agent.aider]
  model      = "sonnet"
  extra_args = ["--yes-always", "--map-tokens", "0"]
  ```
- **Limitations:**
  - **No per-phase file-scope auto-discovery.** Aider only edits files added to its chat. Until pitboss grows a per-phase scope mechanism, enumerate the relevant paths yourself via `extra_args = ["--file", "src/foo.rs", "--file", "src/bar.rs"]`.
  - **Prompt size capped by `ARG_MAX`.** The current `--message <body>` argv path is bounded by the OS argument limit (~256 KB on macOS, ~2 MB on Linux). Comfortable today; a future change will switch to `--message-file` for large payloads.

### Gemini CLI

Wraps Google's `gemini` CLI in single-shot JSON-output mode. The phase prompt is passed as `--prompt <body>`; the terminal JSON document is parsed for the response and tool-call summary.

- **Binary:** `gemini`
- **Config:**
  ```toml
  [agent]
  backend = "gemini"

  [agent.gemini]
  model = "gemini-2.5-pro"
  ```
- **Limitations:**
  - **Prompt size capped by `ARG_MAX`.** Same inline-argv exposure as Aider; will be resolved alongside it via a shared inline-vs-stdin helper.
  - **Tool-call ordering is approximate.** Gemini's JSON stats report tool usage as a name → count map, so the dashboard's tool-call list reflects map-iteration order, not the model's actual call sequence. Cosmetic; the run itself is unaffected.

## Test runner detection

The runner probes the workspace in this order and uses the first match:

1. `Cargo.toml` → `cargo test`
2. `package.json` (with a non-empty `scripts.test`) → `pnpm test` / `yarn test` / `npm test` (chosen by lock file)
3. `pyproject.toml` or `setup.py` → `pytest`
4. `go.mod` → `go test ./...`

Unrecognized layouts skip the test step. The runner then advances on a passing implementer dispatch alone. Override detection by setting `[tests] command = "..."`. The value is whitespace-split into program and args, so shell features (pipes, env-var assignments) need an explicit `sh -c "..."` wrapper.

## Dry runs and verbose output

`pitboss play --dry-run` swaps the configured agent for a deterministic no-op and skips test execution. Use it to sanity-check that:

- `.pitboss/play/plan.md` parses and `current_phase` resolves to a real heading.
- `.pitboss/config.toml` parses cleanly with the keys you expect.
- The per-run branch is created and checked out without touching `main`.
- The event stream and TUI / logger render correctly.

Dry-run advances through every phase, attempts the per-phase commit (which no-ops because nothing was staged), and finishes without any model spend. The post-run PR step is suppressed in dry-run mode regardless of `--pr` / `git.create_pr` so a no-op branch never accidentally opens a PR.

`pitboss -v <command>` lowers the log filter to `debug`. `-vv` lowers it to `trace`. `PITBOSS_LOG` and `RUST_LOG` still take precedence when set, so per-process tuning works without touching the flag.

## Workspace layout

After `pitboss init`:

```
your-project/
├── .gitignore           # pitboss appends `.pitboss/` if missing
└── .pitboss/            # entirely gitignored — per-user, per-session state
    ├── config.toml      # backend, models, budgets, retries
    ├── play/            # multi-phase runner (`pitboss play`) artifacts
    │   ├── plan.md          # source of truth for the work
    │   ├── deferred.md      # agent-writable, swept between phases
    │   ├── state.json       # runner-managed
    │   ├── snapshots/       # pre-agent snapshots of plan.md / deferred.md
    │   └── logs/            # per-phase, per-attempt agent and test logs
    └── grind/           # `pitboss grind` session state (one subdir per run)
```

Everything pitboss writes lives under `.pitboss/`, so a single gitignore line keeps your project tree clean. `init` is idempotent — re-running it on a populated workspace skips every existing file and prints a per-file summary.

## Troubleshooting

<details>
<summary><code>run halted at phase NN: plan.md was modified by the agent</code></summary>

The agent wrote to `plan.md`. Pitboss restored the file from snapshot, your plan is intact. Re-read the phase prompt: it likely needs sharper guard rails about not editing planning artifacts. `pitboss rebuy` retries the same phase.
</details>

<details>
<summary><code>run halted at phase NN: deferred.md is invalid: ...</code></summary>

The agent wrote a malformed `deferred.md`. Pitboss restored from snapshot. The error message includes a 1-based line number. Check the agent's log under `.pitboss/play/logs/phase-<id>-implementer-<n>.log` to see what it tried to write.
</details>

<details>
<summary><code>run halted at phase NN: tests failed: ...</code></summary>

The implementer plus fixer dispatches together couldn't get the suite green within the configured budget. The summary includes the trailing lines of the test log; the full transcript is at `.pitboss/play/logs/phase-<id>-tests-<n>.log`. Either bump `retries.fixer_max_attempts`, fix the failing test by hand, or rework the phase.
</details>

<details>
<summary><code>run halted at phase NN: budget exceeded: ...</code></summary>

`max_total_tokens` or `max_total_usd` was hit before the next dispatch. `pitboss status` shows the running totals and per-role breakdown. Raise the cap (or clear it) and `pitboss rebuy`.
</details>

<details>
<summary><code>run X was folded; remove .pitboss/play/state.json to start over</code></summary>

A previous run was folded with `pitboss fold` (or its `pitboss abort` alias). Pitboss keeps the state file as a breadcrumb. Delete `.pitboss/play/state.json` to start fresh. Everything else (plan, deferred, branch, commits) is preserved.
</details>

<details>
<summary><code>no run to rebuy: .pitboss/play/state.json is empty</code></summary>

You called `pitboss rebuy` on a workspace where no run has started. Use `pitboss play` instead.
</details>

<details>
<summary><code>creating per-run branch ... (workspace must already be a git repo)</code></summary>

The workspace isn't a git repo. `git init` it first. Pitboss won't, on purpose.
</details>

`pitboss --version` prints the pitboss crate version. Useful when filing issues.

## Examples

The [`examples/`](examples) directory contains a walkthrough plan you can copy into a fresh workspace and run end-to-end.

## Contributing

PRs and issues welcome. A few ground rules to keep CI green and the diff reviewable.

### Local checks

CI runs `fmt`, `clippy`, `test`, `doc`, and an MSRV build. Run the same locally before pushing:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo test --workspace --doc
cargo doc --workspace --no-deps --all-features
```

MSRV is **Rust 1.88**. New code must build under that toolchain. CI also runs `cargo deny check` and CodeQL on every push.

### Adding a new agent backend

Implement the `Agent` trait in a new module under `src/agent/`, register it in the backend dispatch, and add an integration test under `tests/backends.rs` that exercises a happy-path phase. Document the binary, config block, and known limitations in the [Agent backends](#agent-backends) section above. The Claude Code backend is the reference implementation worth reading first.

### Source layout

```
src/
├── main.rs          CLI entry, wires the tracing subscriber
├── cli/             clap commands (init, plan, run, status, resume, abort, interview)
├── plan/            Plan/Phase types, parser, snapshot
├── deferred/        DeferredDoc/items/phases, parser
├── state/           RunState, atomic IO
├── config/          config.toml schema and loader
├── util/paths.rs    workspace-relative paths under `.pitboss/`
├── agent/           Agent trait, request/outcome, subprocess utils
│   ├── claude_code.rs
│   └── dry_run.rs
├── git/             Git trait, ShellGit, MockGit, PR helpers
├── tests/           project test runner detection (NOT the integration tests)
├── prompts/         system prompt templates
├── runner/          orchestration loop and events
└── tui/             ratatui dashboard
tests/               integration tests
```

### Filing issues

Include `pitboss --version`, the relevant snippet from `.pitboss/config.toml`, and the failing log under `.pitboss/play/logs/`. For runner halts, the phase id and the halt reason from `pitboss status` are usually enough to start.

### Commits and PRs

Keep commits focused. Reference an issue in the PR body when one exists. The CI matrix runs on Ubuntu and macOS, so platform-specific code needs to work on both.

## License

MIT OR Apache-2.0.
