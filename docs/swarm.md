# Swarms: many workers, one plan

`fox dispatch` starts one worker; `fox wait` waits for it. `fox swarm run plan.toml` runs a whole
plan of them: tasks with dependencies, a concurrency cap, writer leases, one summary at the end,
and optionally a reviewer panel that votes on the result. This page is the reference and a set of
worked patterns.

## The plan

```toml
name = "audit-handlers"   # the swarm's name; default: the file name
concurrency = 3           # most tasks running at once; FOXDB_MAX_CHILDREN (default 8) caps it too

[[task]]
id = "scan-api"           # unique in the plan
harness = "codex"         # codex | claude
task = "List every HTTP handler under api/ that reads the body twice. Report path:line."
repo = "."                # relative to the plan file; default "."
writer = false            # take the repo's writer lease (one writer per repo at a time)
reviewer = false          # a reviewer gets the task only, no brief, and starts nothing
# model = "…"             # optional: passed to the harness as its model flag
depends_on = []           # task ids; their report handles are appended to this task

[[task]]
id = "fix"
harness = "codex"
task = "Fix the handlers listed in the scan report."
writer = true
depends_on = ["scan-api"]

[review]                  # optional: one reviewer per entry, an odd count avoids ties
harnesses = ["claude", "codex", "claude"]
task = "Check that every handler in the scan was fixed and nothing else changed."
```

```
fox swarm run audit.toml [--no-wait] [--timeout SECS]
fox swarm status audit-handlers        # or its id, sw-…
```

`run` starts every task whose dependencies finished well, up to the cap; waits (inotify, no
polling) for any running one; and repeats. It exits 0 when everything finished well, 3 when it
stopped with tasks still running (`--no-wait`, or the timeout, default 6 h), and 4 when a task
failed or was skipped, or the panel rejected. Run it again to continue: it never starts a task
twice.

- **Dependencies.** A dependent starts only when all its dependencies ended `done`, `partial`
  or `flagged`. Its task text gets their report handles:
  `Inputs from earlier tasks of this swarm (read them with fox_read): - scan-api: h:1f`.
  If a dependency failed, went stale or reported `blocked`, the dependent is `skipped`.
- **Caps and leases.** Every task goes through `fox dispatch`, so the depth cap
  (`FOXDB_MAX_DEPTH`, default 3), the per-parent child cap and writer leases apply. A writer
  task waits while another agent holds that repo's lease, and read-only tasks run beside it.
- **Summary.** When every task ended, one `report` handoff (topic `swarm`) lists each task with
  its state, worker and the first line of its report: `- scan-api [done] cx-4 codex report h:1f:
  …`. A second run returns the same summary.
- **Review panel.** Each reviewer gets the panel task plus a pointer to the summary. Report
  status `done` approves; anything else rejects. The majority wins, and the verdict is posted as
  a `decision` under the summary (`swarm audit-handlers review: approved 2/3`).
- **One swarm per plan name and directory.** The swarm's id (`sw-…`) is a hash of the plan
  name and the plan file's directory, so two `plan.toml` files in different directories are two
  swarms with their own summaries. `fox swarm status <name>` refuses a name that several swarms
  share and lists their ids; use an id then.
- **Crash-safe.** A task's key is a hash of the swarm id and the task's own fields (id,
  harness, task text, repo, writer, reviewer, model). The worker carries it as its label
  (`swarm:<key>`), set before its process starts. A re-run looks the key up first and adopts the
  worker it finds, running or finished, so a controller that died after dispatching never
  dispatches again. Editing a task changes its key: only that task runs again. Renaming the plan,
  or moving it to another directory, starts a new swarm.
- **Safe to run twice at once.** Two runs of the same plan (one in the background, one with
  `--no-wait`) take a per-swarm lock around each start and each post, so each task is still
  dispatched once and the summary and verdict are posted once.
- **Status.** `fox swarm status` reads the same state: one line per task with its worker,
  harness, state and report handle, then the summary and verdict.

`run` blocks. An agent whose shell calls have a time limit should use `--no-wait` and run it
again later (or after `fox wait <ids>`), or start it in the background.

## Patterns

### Fan-out workers with one writer

Many read-only workers gather; one writer acts on what they found.

```toml
name = "dead-flags"
concurrency = 4

[[task]]
id = "scan-app"
harness = "codex"
task = "List feature flags under app/ that are never read. Report path:line per flag."
[[task]]
id = "scan-core"
harness = "codex"
task = "List feature flags under core/ that are never read. Report path:line per flag."
[[task]]
id = "scan-tests"
harness = "claude"
task = "List tests that only exercise a flag's off branch. Report path:line."

[[task]]
id = "remove"
harness = "codex"
writer = true
task = "Remove the flags the scans list, with their dead branches and tests. Run the test suite."
depends_on = ["scan-app", "scan-core", "scan-tests"]
```

The three scans run together; `remove` starts after all three, holding the only writer lease.

### Parallel read-only investigators

Several independent questions, no writer, no dependencies. Use a different harness per question
when you want a second opinion.

```toml
name = "why-slow"
concurrency = 3
[[task]]
id = "db"
harness = "codex"
task = "Find the queries on the home screen path that scan whole tables. Report path:line and the query."
[[task]]
id = "net"
harness = "claude"
task = "Find network calls made on the main thread during startup. Report path:line."
[[task]]
id = "img"
harness = "codex"
task = "Find image decodes larger than the view they fill. Report path:line."
```

### Reviewer panel with majority vote

Put a panel over any plan with `[review]`. Reviewers are started as reviewers: the task alone,
no brief, no writer, and they cannot dispatch. Three reviewers on two harnesses catch more than
one reviewer asked three times.

```toml
[review]
harnesses = ["claude", "codex", "claude"]
task = "Did the change keep the public API? Approve only if every exported symbol is unchanged."
```

### Map-reduce over files

Map: one task per shard of the tree. Reduce: one task that depends on all of them.

```toml
name = "license-headers"
concurrency = 4
[[task]]
id = "map-1"
harness = "codex"
task = "For files under src/a* through src/f*, list those without the license header."
[[task]]
id = "map-2"
harness = "codex"
task = "For files under src/g* through src/m*, list those without the license header."
[[task]]
id = "map-3"
harness = "codex"
task = "For files under src/n* through src/z*, list those without the license header."
[[task]]
id = "reduce"
harness = "claude"
task = "Merge the lists into one table by directory, with a count per directory."
depends_on = ["map-1", "map-2", "map-3"]
```

Generate shard tasks with a script when there are many: the plan is plain TOML.

### Loop until dry

Repeat a finder until it finds nothing new. Each round is a new plan name, so a round never
re-adopts the previous round's workers:

```sh
i=1
while :; do
  # next to sweep.toml: a task's repo is relative to its plan file
  sed "s/^name = .*/name = \"todo-sweep-$i\"/" sweep.toml > round.toml
  fox swarm run round.toml || break               # 4: a task failed; stop and look
  h=$(fox swarm status "todo-sweep-$i" | sed -n 's/^summary \(h:[0-9a-f]*\).*/\1/p')
  fox handoff render "$h" --for plain | grep -q 'found nothing' && break
  i=$((i + 1))
done
```

Give the finder's task a fixed phrase for "nothing left" (`say "found nothing" in the summary
when there is nothing to fix`) so the loop can stop on it, and cap the rounds.

### Claude controller, Codex workers (and the reverse)

The controller is whatever session runs `fox swarm run`; the workers are whatever the plan's
`harness` says. A Claude Code session controlling Codex workers:

```toml
name = "port-tests"
concurrency = 3
[[task]]
id = "port-1"
harness = "codex"
task = "Port tests/legacy/a_test.py to pytest style. Run it."
writer = true
[[task]]
id = "port-2"
harness = "codex"
task = "Port tests/legacy/b_test.py to pytest style. Run it."
writer = true
[review]
harnesses = ["claude"]
task = "Check that the ported tests assert the same things as the originals."
```

Writers on one repo run one at a time; give each its own worktree (`repo = "../wt-port-2"`) to run
them side by side. The reverse, a Codex controller with Claude workers, is the same plan with the
harnesses swapped: `fox` is the same binary whichever harness runs it, and the summary is a handle
either harness reads with `fox_read`.

## Cost and tokens

- A worker's prompt starts with one fixed brief that names the parent's packet handle, and the
  same brief goes to every sibling byte for byte, so it can sit in the provider's cached prefix.
  Keep that order: identical brief first, the task after it. Put shared context in the packet
  (`fox handoff create`), not in each task's text.
- Reviewers get the task only. Keep panel tasks short and specific.
- Dependencies pass handles, not report text: a dependent reads only the reports it needs.
- The summary is one line per task. Read the full reports by handle only when a line looks wrong.
- Concurrency is a cost cap as much as a speed knob: each running worker is a full model session.
  Start with 2-4.
- A swarm of read-only scouts on a cheap model, then one writer on a strong one, is usually
  cheaper than several strong writers.

## Limits

- One writer per repo at a time (leases are per repo root). Use worktrees for parallel writers.
- At most `FOXDB_MAX_CHILDREN` (default 8) running children per controller, and trees at most
  `FOXDB_MAX_DEPTH` (default 3) deep. A worker that runs its own swarm counts against both.
- Workers are `codex exec` and `claude -p` only; other harnesses can join through the board and
  `fox agent register` but are not dispatched.
- A worker that dies goes `stale` and its dependents are skipped. Re-running does not retry it;
  edit the task (which changes its key) or rename the plan to try again.
- The summary and verdict are posted once per swarm. A task edited after the summary exists runs
  again, but the summary stays the first one; rename the plan for a fresh summary.
