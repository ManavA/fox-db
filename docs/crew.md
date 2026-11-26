# Crew: native sessions in worktrees and panes

`fox dispatch` and `fox swarm` start headless workers (`claude -p`, `codex exec`) that report
back. `fox crew` starts the other kind: interactive `claude` and `codex` sessions that you watch
and talk to, one per git worktree, each in its own terminal pane. It uses
[herdr](https://herdr.dev) when `herdr` is on PATH, and tmux otherwise.

Crew starts each session with the harness's own CLI, the same way you would start it by hand.
It never proxies the session, never replaces or appends to its system prompt
(`--system-prompt`, `--append-system-prompt`) and never uses `--bare`. Your login, prompt
caching, hooks, skills, plugins and compaction work as they do in any session. A plan's prompt
is the CLI's initial argument, so it arrives as the first user message. Crew adds three
environment variables:

- `FOXDB_AGENT`: the session's id in the agent registry (`cl-…` or `cx-…`).
- `FOXDB_PARENT`: the caller's `FOXDB_AGENT`, or `crew:<plan name>` when there is none.
- `FOXDB_HOME`: the store the hooks and the MCP server should use.

The foxdb hooks read these values, so each session joins the agent tree under its parent.

## Commands

```sh
fox crew up plan.toml                  # every session in the plan
fox crew up 3 --harness codex          # three bare sessions: s1, s2, s3
fox crew up 2 --backend tmux --model claude-haiku-5-5
fox crew ls                            # sessions, pane state, then `fox agents --tree`
fox crew attach api                    # herdr: focus its workspace; tmux: attach or switch
fox crew down api                      # close one session's pane
fox crew down --prune                  # close every session of this repo; remove clean worktrees
```

For each session, `up` does four things in order:

1. It creates a worktree at `<repo>/.claude/worktrees/crew-<name>`, on the session's branch
   (default `crew/<name>`). An existing branch is checked out. A new one starts at the plan's
   `base`, or at HEAD. The indexer skips `.claude/worktrees`, so these trees are not indexed
   twice.
2. It runs `fox index` once on the worktree, so the SessionStart brief and the packet hooks find
   the worktree's own project when the session starts. Its index keeps branch content separate
   from the main checkout.
3. It registers the agent with label `crew:<name>` and records the start commit. A
   `writer = true` session takes the repo's writer lease. The lease key is the main checkout,
   so all worktrees of a repo share one lease.
4. It opens the pane and starts the CLI there:
   - **herdr:** `herdr workspace create --cwd <worktree> --label <name> --env FOXDB_…=… --no-focus`,
     then `herdr pane run <root pane> '<cli> [--model M] <prompt>'`.
   - **tmux:** `tmux new-session -d -s fox-crew` for the first window, then `new-window`, with
     `-c <worktree>` and `-e FOXDB_…=…`. An `env -u` prefix clears inherited harness session markers before it starts the CLI.

`down` closes the pane, records the worktree's end commit, marks the agent done and frees its
lease. Without `--prune` the worktree stays. With `--prune` it is removed only when
`git status` shows it clean, and its branch stays. Session records are in
`$FOXDB_HOME/crew/<name>.json`.

## The plan

```toml
name = "auth"            # the crew's name; default: the file name
harness = "claude"       # default for every session: claude | codex
model = "claude-haiku-5-5"   # default model, optional
base = "main"            # where new branches start; default HEAD

[[session]]
name = "api"             # letters, digits, - and _
harness = "codex"
model = "gpt-5.5"
branch = "crew/api"      # default crew/<name>
prompt = "Add rate limiting to the login handler. Tests first."

[[session]]
name = "review"
prompt = "Review the auth module for token handling bugs."
writer = false           # true takes the repo's writer lease (one session per plan)
args = ["--permission-mode", "plan"]   # optional Claude permission mode
```

`args` accepts Claude's `--permission-mode` and its value. Other arguments are rejected so
crew always launches the interactive CLI.

## herdr plugin

`plugin/herdr/herdr-plugin.toml` adds a **New fox session** action and a `worktree.created`
event hook:

```sh
herdr plugin link /path/to/foxdb/plugin/herdr
herdr plugin action invoke foxdb.crew.new-fox-session
```

- The action runs `fox crew herdr new`. It reads the workspace's cwd from
  `HERDR_PLUGIN_CONTEXT_JSON` and starts one bare session there (`fox crew up 1 --backend herdr`).
- The event runs `fox crew herdr worktree-created`. It reads the worktree path from
  `HERDR_PLUGIN_EVENT_JSON` and runs `fox index` on it, so worktrees you make with
  `herdr worktree create` are indexed too.

Both commands need `fox` on the PATH that herdr's server inherits.

## Licences

herdr is Apache-2.0. foxdb is AGPL-3.0-or-later. The two talk only through separate processes:
foxdb runs the `herdr` CLI, and herdr runs the `fox` CLI from the plugin manifest. Neither
links, embeds or ships the other's code. The plugin manifest is part of foxdb and carries
foxdb's licence.

## Caching check

`bench/crew_cache.py` runs the same three-turn task twice with
`claude -p --output-format stream-json`, using `--resume` for turns two and three. The first run
uses a plain environment. The second adds the crew variables. For each run it prints
cache-read ÷ (cache-read + cache-creation + uncached input). Both runs use a scratch
`FOXDB_HOME`, so they never write to your live store.

```sh
python3 bench/crew_cache.py --model claude-haiku-5-5 --max-usd 0.50
```

A claude-haiku-5-5 run measured a 58.7% cache-read share in the plain arm and 69.4% in the
crew arm, at $0.0271 total. Each arm returned `ok`, `42`, and `84`. This benchmark measures
separate `claude -p` processes resumed across turns.
