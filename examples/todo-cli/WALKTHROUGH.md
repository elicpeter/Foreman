# Walkthrough: todo-cli

This is an end-to-end run of the `todo-cli` example. Follow it the first time you use pitboss to get a feel for the loop without surprises.

## 1. Set up an empty workspace

```sh
mkdir scratch-todo && cd scratch-todo
git init
pitboss init
```

`pitboss init` writes `plan.md`, `deferred.md`, `pitboss.toml`, the `.pitboss/` directory, and updates `.gitignore`. Re-running `init` is a no-op.

## 2. Drop in the example plan

Replace the seed `plan.md` and `pitboss.toml` with this example's:

```sh
cp ../pitboss/examples/todo-cli/plan.md plan.md
cp ../pitboss/examples/todo-cli/pitboss.toml pitboss.toml
```

(Adjust the paths to wherever you cloned pitboss.)

## 3. Sanity check with `--dry-run`

```sh
pitboss play --dry-run
```

What this exercises with no token spend:

- Parses `plan.md` and confirms `current_phase: "01"` resolves.
- Parses `pitboss.toml`.
- Creates the per-run branch (`pitboss/run-<utc>`).
- Walks each phase, dispatches the no-op agent, attempts a (no-op) commit,
  emits the same `Event` stream the real run will.
- Skips test execution because the no-op agent doesn't change anything.

If anything is wrong with the plan or config, you'll see it here. The dry run leaves a clean state on the per-run branch so the real run starts fresh.

## 4. Run for real

```sh
pitboss play
```

Watch the streamed output. Each phase will:

1. Print `[pitboss] phase 01 (Cargo skeleton & CLI parsing), attempt 1`.
2. Stream the agent's stdout / tool-use lines as `[agent] ...`.
3. Print `[pitboss] running tests` and the result.
4. (When `audit.enabled`) Print `[pitboss] phase 01 auditor (total dispatch 2)`.
5. Print `[pitboss] phase 01 committed: <short-sha>`.

If a phase halts, pitboss prints a clear reason and exits non-zero. Run `pitboss status` to see where it stopped, fix the underlying issue, then `pitboss rebuy`.

## 5. Inspect the result

```sh
pitboss status              # phase + token + cost summary
git log --oneline           # one commit per phase, all on the per-run branch
cat deferred.md             # anything the auditor marked as follow-up work
ls .pitboss/logs/           # per-attempt agent + test logs for post-mortem
```

## 6. Open a PR (optional)

```sh
pitboss play --pr           # or set git.create_pr = true in pitboss.toml
```

Pitboss shells out to `gh pr create` with a body listing the completed phases plus any unfinished deferred items.

## 7. Clean up

If you want to throw the run away:

```sh
pitboss fold --checkout-original    # back to the branch you were on at run start
git branch -D pitboss/run-<utc>     # delete the per-run branch
rm .pitboss/state.json              # wipe the state breadcrumb
```

`plan.md` and `deferred.md` are preserved. Pitboss never deletes them.
