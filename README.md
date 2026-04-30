<div align="center">

  <img src="assets/foreman-wordmark.svg" alt="nyx" height="110"/>


**A coding-agent foreman.** Hand it a phased plan, walk away, come back to a branch full of green commits.

[![Rust](https://img.shields.io/badge/rust-stable-CE422B?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-007EC6)](#license)
[![Agent](https://img.shields.io/badge/agent-Claude%20Code-D97757)](https://docs.anthropic.com)
[![Status](https://img.shields.io/badge/status-alpha-yellow)]()

</div>

Foreman is a Rust CLI that drives a coding agent (Claude Code today, others pluggable) through a multi-phase implementation plan. It runs your test suite after every phase, retries failures with a fixer agent, audits the diff, lands a commit, then moves on. Bounded retries everywhere. Token and dollar budgets. A live TUI if you want to watch.

<div align="center">
  <img src="assets/foreman-tui.png" alt="foreman run --tui dashboard" width="900"/>
</div>
<sub align="center"><i>`foreman run --tui`. The dashboard. Phases on the left, live agent output on the right.</i></sub>

## Contents

- [How it works](#how-it-works)
- [Install](#install)
- [Quickstart](#quickstart)
- [The run loop](#the-run-loop)
- [Configuration](#configuration)
- [Test runner detection](#test-runner-detection)
- [Dry runs and verbose output](#dry-runs-and-verbose-output)
- [Workspace layout](#workspace-layout)
- [Troubleshooting](#troubleshooting)
- [Contributing](#contributing)
- [License](#license)

## How it works

Three files do the work.

| File | Owner | Contents |
|------|-------|----------|
| `plan.md` | you | The phases. Read-only to agents. |
| `deferred.md` | the agent | Anything the agent couldn't finish in a phase. Swept between phases. |
| `.foreman/state.json` | foreman | Run id, branch, attempts, token usage. |

Each phase becomes its own commit on a per-run branch, optionally rolled into a pull request when the run finishes.

## Install

A recent stable Rust toolchain is the only build requirement.

```sh
git clone <this repo>
cd foreman
cargo install --path .
```

To actually drive the agent you also need:

- **`claude`**, the Claude Code CLI from Anthropic.
- **`git`**, any reasonably recent version.
- **`gh`** (optional), only if you want `--pr` to open pull requests.

## Quickstart

```sh
mkdir my-project && cd my-project
git init
foreman init                # scaffold plan.md, deferred.md, foreman.toml, .foreman/
$EDITOR plan.md             # describe the work, phase by phase
foreman run --dry-run       # exercise the runner without spending tokens
foreman run                 # let the agent loop drive the plan
foreman status              # check progress at any time
```

`foreman status` looks like this:

```text
$ foreman status
run: 20260429T143022Z (started 2026-04-29T14:30:22+00:00)
branch: foreman/run-20260429T143022Z
original branch: main
plan: phase 02 of 3 — Domain types (2)
completed: 01
deferred items: 2 (1 unchecked, 1 checked)
deferred phases: 1
tokens: input=12850 output=4210
  auditor:     input=2100  output=480
  fixer:       input=1750  output=820
  implementer: input=9000  output=2910
cost: $0.5210
  token budget: 17060/1000000 used, 982940 remaining
  USD budget:   $0.5210/$5.0000 used, $4.4790 remaining
last commit: abc1234 [foreman] phase 01: Project foundation
```

A few entry points worth knowing:

- `foreman plan "build a CLI todo app in Rust"` invokes the planner agent to draft `plan.md` for you.
- `foreman run --tui` swaps the stderr logger for the dashboard above.
- `foreman run --pr` (or `git.create_pr = true`) opens a pull request with `gh pr create` after the run finishes.
- `foreman resume` picks up where a halted run left off.
- `foreman abort --checkout-original` marks the run aborted and switches HEAD back to the branch you were on before `foreman run`.

## The run loop

For each phase in `plan.md`:

1. Snapshot `plan.md` and `deferred.md` (SHA-256).
2. Dispatch the **implementer** agent with the active phase, the unfinished deferred work, and the user prompt template.
3. If the agent modified `plan.md`, restore the snapshot and halt.
4. Re-parse `deferred.md`. On parse failure, restore the snapshot and halt.
5. Run the project test suite. If it fails, dispatch the **fixer** agent up to `retries.fixer_max_attempts` times.
6. Stage the diff and dispatch the **auditor** agent (when `audit.enabled = true`). The auditor inlines small fixes and records anything larger in `deferred.md`. Tests run again post-audit.
7. Commit the staged diff to the per-run branch as `[foreman] phase <id>: <title>`. `plan.md`, `deferred.md`, and `.foreman/` are excluded from the commit.
8. Sweep checked-off deferred items, advance `current_phase` in `plan.md`, persist `state.json`, move on.

Every retry is bounded. When a budget is exhausted the runner halts with a clear reason and `foreman resume` picks up from the same phase.

```text
phase 02 — Domain types   [halted: plan tampered]
┌ phases (0/3) ────────────────┐┌ agent output ────────────────────────────────┐
│· 01 Project foundation       ││[halt] phase 02: plan.md was modified by the  │
│x 02 Domain types  (1x)       ││agent                                         │
│· 03 Plan parser              ││                                              │
│                              ││                                              │
└──────────────────────────────┘└──────────────────────────────────────────────┘
q quit   p pause   a abort
```
<sub align="center"><i>An agent edited a guarded file. Foreman halts, restores from snapshot, no commit lands.</i></sub>

## Configuration

Foreman reads `foreman.toml` from the workspace root. Every section is optional, missing keys fall back to defaults. Unknown keys load with a warning so a config written by a newer foreman still works.

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

# Per-run branch and optional PR.
[git]
branch_prefix = "foreman/run-"   # full branch is <prefix><utc_timestamp>
create_pr     = false            # equivalent to `foreman run --pr`

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
```

### Per-role model recommendations

The defaults set every role to the latest Opus, which is fine if you don't want to think about it. For a cheaper run, split it like this:

| Role          | Model                | Rationale                                            |
| ------------- | -------------------- | ---------------------------------------------------- |
| `planner`     | `claude-opus-4-7`    | One careful plan up front saves dozens of bad phases. |
| `implementer` | `claude-opus-4-7`    | Most of the spend, most sensitive to capability.     |
| `auditor`     | `claude-sonnet-4-6`  | Diff review and short-form notes. Sonnet handles it. |
| `fixer`       | `claude-sonnet-4-6`  | Test fix-ups are usually small and local.            |

Configure pricing for any model you reference in `[models]` so `foreman status` and the USD budget check produce accurate numbers.

## Test runner detection

The runner probes the workspace in this order and uses the first match:

1. `Cargo.toml` → `cargo test`
2. `package.json` (with a non-empty `scripts.test`) → `pnpm test` / `yarn test` / `npm test` (chosen by lock file)
3. `pyproject.toml` or `setup.py` → `pytest`
4. `go.mod` → `go test ./...`

Unrecognized layouts skip the test step. The runner then advances on a passing implementer dispatch alone. Override detection by setting `[tests] command = "..."`. The value is whitespace-split into program and args, so shell features (pipes, env-var assignments) need an explicit `sh -c "..."` wrapper.

## Dry runs and verbose output

`foreman run --dry-run` swaps the configured agent for a deterministic no-op and skips test execution. Use it to sanity-check that:

- `plan.md` parses and `current_phase` resolves to a real heading.
- `foreman.toml` parses cleanly with the keys you expect.
- The per-run branch is created and checked out without touching `main`.
- The event stream and TUI / logger render correctly.

Dry-run advances through every phase, attempts the per-phase commit (which no-ops because nothing was staged), and finishes without any model spend. The post-run PR step is suppressed in dry-run mode regardless of `--pr` / `git.create_pr` so a no-op branch never accidentally opens a PR.

`foreman -v <command>` lowers the log filter to `debug`. `-vv` lowers it to `trace`. `FOREMAN_LOG` and `RUST_LOG` still take precedence when set, so per-process tuning works without touching the flag.

## Workspace layout

After `foreman init`:

```
your-project/
├── plan.md              # source of truth for the work
├── deferred.md          # agent-writable, swept between phases
├── foreman.toml         # config
├── .gitignore           # foreman appends `.foreman/` if missing
└── .foreman/
    ├── state.json       # runner-managed, ignored by git
    ├── snapshots/       # pre-agent snapshots of plan.md and deferred.md
    └── logs/            # per-phase, per-attempt agent and test logs
```

`init` is idempotent. Re-running it on a populated workspace skips every existing file and prints a per-file summary.

## Troubleshooting

<details>
<summary><code>run halted at phase NN: plan.md was modified by the agent</code></summary>

The agent wrote to `plan.md`. Foreman restored the file from snapshot, your plan is intact. Re-read the phase prompt: it likely needs sharper guard rails about not editing planning artifacts. `foreman resume` retries the same phase.
</details>

<details>
<summary><code>run halted at phase NN: deferred.md is invalid: ...</code></summary>

The agent wrote a malformed `deferred.md`. Foreman restored from snapshot. The error message includes a 1-based line number. Check the agent's log under `.foreman/logs/phase-<id>-implementer-<n>.log` to see what it tried to write.
</details>

<details>
<summary><code>run halted at phase NN: tests failed: ...</code></summary>

The implementer plus fixer dispatches together couldn't get the suite green within the configured budget. The summary includes the trailing lines of the test log; the full transcript is at `.foreman/logs/phase-<id>-tests-<n>.log`. Either bump `retries.fixer_max_attempts`, fix the failing test by hand, or rework the phase.
</details>

<details>
<summary><code>run halted at phase NN: budget exceeded: ...</code></summary>

`max_total_tokens` or `max_total_usd` was hit before the next dispatch. `foreman status` shows the running totals and per-role breakdown. Raise the cap (or clear it) and `foreman resume`.
</details>

<details>
<summary><code>state.json marks run X as aborted; remove .foreman/state.json to start over</code></summary>

A previous run was aborted with `foreman abort`. Foreman keeps the state file as a breadcrumb. Delete `.foreman/state.json` to start fresh. Everything else (plan, deferred, branch, commits) is preserved.
</details>

<details>
<summary><code>no run to resume: .foreman/state.json is empty</code></summary>

You called `foreman resume` on a workspace where no run has started. Use `foreman run` instead.
</details>

<details>
<summary><code>creating per-run branch ... (workspace must already be a git repo)</code></summary>

The workspace isn't a git repo. `git init` it first. Foreman won't, on purpose.
</details>

`foreman --version` prints the foreman crate version. Useful when filing issues.

## Examples

The [`examples/`](examples) directory contains a walkthrough plan you can copy into a fresh workspace and run end-to-end.

## Contributing

```
src/
├── main.rs          CLI entry, wires the tracing subscriber
├── cli/             clap commands (init, plan, run, status, resume, abort)
├── plan/            Plan/Phase types, parser, snapshot
├── deferred/        DeferredDoc/items/phases, parser
├── state/           RunState, atomic IO
├── config/          foreman.toml schema and loader
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

See `plan.md` for the phase-by-phase design log.

## License

MIT OR Apache-2.0.
