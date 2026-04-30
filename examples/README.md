# Examples

Each subdirectory is a complete `plan.md` + `pitboss.toml` you can drop into a fresh workspace.

| Example | What it builds | Notes |
| ------- | -------------- | ----- |
| [`todo-cli/`](todo-cli) | A small Rust CLI todo app, phased into foundation → CRUD → persistence → tests. | Good first dogfood run. Sized so a real Claude Code agent can finish it end-to-end without burning a budget. |

## How to use an example

```sh
# Pick a fresh empty directory
mkdir scratch && cd scratch
git init

# Scaffold the pitboss workspace
pitboss init

# Replace the placeholder plan with the example's plan
cp /path/to/pitboss/examples/todo-cli/plan.md plan.md
cp /path/to/pitboss/examples/todo-cli/pitboss.toml pitboss.toml

# Sanity check: exercises the runner end-to-end with no token spend
pitboss play --dry-run

# When ready, run for real
pitboss play
```

`pitboss play --dry-run` is always the recommended first step on an example: it confirms the plan parses, the per-run branch creates cleanly, and the event stream renders the way you expect, all without invoking the agent.
