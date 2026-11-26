# Multi-agent

Many agents, in any mix of harnesses, sharing one fox-db store. Whole plans with dependencies and review panels are in [swarm.md](swarm.md); running fox-db under T3 Code is in [t3code.md](t3code.md).

One store, many agents. Every harness session, subagent and dispatched worker shares
`$FOXDB_HOME/fox.db` (WAL, writer busy timeout 10 s, readers 500 ms), so a board post, a report or a
packet written by one agent is a handle (`h:…`) any other agent can read, in any harness. Each
harness spawns its own `fox mcp` over stdio, so identity is per process: `FOXDB_AGENT` (this agent),
`FOXDB_PARENT` (who started it) and `FOXDB_HOME` (the store). Without `FOXDB_AGENT`, an agent is
the session it runs in, never the bare client name every session of a harness shares: Claude
Code's `CLAUDE_CODE_SESSION_ID`, else the session the SessionStart hook registered for an
ancestor process (the harness its MCP server and shell commands descend from), else
`<client>-<harness pid>` for MCP and `cli-<shell pid>` for the CLI. Takes, `since=me` cursors
and post attribution all use this id, unless a call names itself with `agent=` (a subagent does: see
[Swarm protocol](#swarm-protocol)). Set `FOXDB_AGENT` for an id that survives restarts.

```
fox dispatch --harness codex|claude --task <text|h:ID> [--repo DIR] [--writer|--reviewer] [--label L] [--model M]
fox wait <agent|h:ID>... [--any|--all] [--timeout SECS]     # exit 3 still running, 4 stale/failed/no report
fox agents [--tree]        fox status --agents        fox lease list|take|release
fox handoff create [--goal G] [--next N] [--question Q] [--transcript PATH]   # -> packet h:ID
fox handoff render h:ID [--for claude|codex|t3] [--budget N]     fox handoff take h:ID
fox switch --to codex|claude [--handoff h:ID | --transcript PATH] [--inline] [--exec]
fox switch --for t3 [--handoff h:ID] [--inline]       # a message to paste into a T3 Code thread
fox swarm run <plan.toml> [--no-wait]     fox swarm status <swarm>     # see docs/swarm.md
fox mem post <text> --topic T [--kind fact|failed|question]     fox mem read --since me [--topic T]
```

- **Registry.** An agent has an id, harness, model, role, parent, repo, status
  (running/done/failed/stale), start time, native resume id (Claude session id, Codex thread id)
  and last report handle. Liveness is the process, not a heartbeat the agent must remember: a
  pid plus its `/proc` start time (so a reused pid does not count). A dispatched worker lives
  while its supervisor or its harness child runs, so a killed supervisor does not free a lease
  the child still writes under. Hook-registered sessions use their harness process (from the
  SessionStart hook); agents registered by hand with no pid use `fox agent heartbeat`.
  `FOXDB_STALE_SECS` (default 1800) sets the heartbeat timeout.
- **Packets** (`kind=packet`) hold goal, state, decisions with reasons, touched files
  (`git status`), the `git diff --stat` line, symbols overlapping the changed hunks, open
  questions, next steps and evidence handles (reports and findings of this agent's tree, session
  or parent handoff, not the project's latest). For a registered agent with no transcript (a
  dispatched worker), the goal is its task and the state its status and last output. Creating
  the same open packet again returns the stored one. No diff text is inlined; everything is
  redacted (long base64-like runs too, but not paths).
  `render` is deterministic and capped: `--for` only picks the cap (Claude 9,500 chars, Codex
  ~2,200 tokens). Packets keep lineage (`parent:` / `children:` lines).
- **Take** claims a handoff once, with a lease (default 30 min; the taker renews by taking
  again). An expired lease is retaken by exactly one agent: a compare-and-set on the old claim
  inside `BEGIN IMMEDIATE`. `pickup` is a take without expiry. A refusal says why, starting
  `refused h:N` and a fixed reason: `not_found`, `already_taken by <holder> until <time>` (or
  `with no expiry`), or `already_done: report h:M` when a `done` worker (or a hand-posted report) already reported on it; a failed, blocked, partial or flagged report leaves it open for a retry.
- **Writer leases** are advisory, one per repo root. A second `--writer` is refused with the
  holder's id. A lease is held while its holder's process lives and ends on release or death,
  never on a timer, so a long build does not let a second writer in.
- **Board.** Posts carry a topic and the poster's agent id. `read --since me` (`auto` is the same)
  returns only rows newer than this agent's cursor, minus its own posts, and advances the cursor.
  An agent's first read starts at the newest rows that fit (with a `+N older` line), not the
  oldest. `--since h:N` is the stateless form: rows after `h:N`, for whoever asks; every read
  ends with the `since=h:N` to pass next.
- **Swarm entries.** `fact`, `failed` (an approach that did not work, and why), `summary` and
  `question` are kinds of their own: at most 600 characters each, shown whole on their board
  row, so reading what is new costs one call. Their `evidence` holds handles: a path of an
  indexed file becomes its `f:` handle, other paths stay, other words are dropped (at most 8
  items, 400 bytes), a handle that names nothing is refused, and
  `fox_read target=h:N` lists what each handle points at. Open questions count with decisions
  and requests in subagent briefs.
- **Dedup** ("already sent") applies only to the CLI, per `FOXDB_SESSION` (a dispatched
  worker's id), and the PreCompact hook clears it. The MCP tools keep none: one server process
  serves a session and its subagents, which never saw each other's answers. What the tree
  shares is compute (one store and `foxd`'s warm cache) and handles: briefs point at packets
  instead of inlining them.

## Swarm protocol

Agents working one task share what they learn through the board, as an append-only log each
reads incrementally (the design DeLM measured against native subagents). The rules (the skill
states them; subagent briefs repeat them only with `[hooks] swarm_board = true`, below):

1. **Read what is new before each new lead**: `fox_memory op=read since=me`, or the `since=h:N`
   the brief gives and then the one each read ends with. Repeated reads return only new rows.
2. **Post as you learn**: `kind=fact` for a finding, `kind=failed` for an approach that did not
   work and why (so no sibling repeats it), `kind=question` for an open question.
3. **Attach handles, not content**: `evidence=f:1a c:2b` or paths; `fox_read` opens them.
4. **Return handles**: a final answer with `path:line` and handles, not file contents.

**Identity.** A dispatched worker runs its own `fox mcp` with `FOXDB_AGENT` set, so `since=me`
and attribution need nothing more. A Claude Code subagent calls through its parent's MCP server,
with the parent's environment, so the server cannot tell siblings apart: a subagent passes
`agent=<its name>` on `op=post` and `op=read since=me`, or carries the stateless `since=h:N`. An
explicit `agent` argument wins over `FOXDB_AGENT`. While a session (or worker) has running
subagents, a `since=me` read that names no `agent` is refused with that hint, since its cursor
would be every sibling's, and a post that names none is attributed to `<session>/unnamed`, so
the session's own reads still show it. A read whose first row is longer than its budget shows
that row clipped and moves past it.

**SubagentStart** (Claude Code) adds to the brief:

```
parent packet h:30 - open decisions/requests (fox_read target=h:30)
parent read (fox_read a handle, lines=a-b for a slice): f:1c src/search.rs, f:9 src/query.rs
Final answer: findings and handles (path:line, s:/f:), not file contents; it is saved for the parent.
```

With `swarm_board = true` under `[hooks]` in `config.toml` (`$FOXDB_HOME`, default `~/.foxdb`),
a board line follows the parent packet line:

```
board: before each new lead, fox_memory op=read since=h:30, then the since=h:N it ends with. Post fact|failed (approach, why)|question with agent=<your name>, evidence=handles, not content.
```

It is off by default: in two A/B runs it gave parallel subagents no benefit and cost about a
tenth more. The board itself works either way; agents that follow the skill's protocol still
read and post.

`h:30` is the parent's board snapshot, shared by siblings started together, so their briefs stay
byte-identical. The `parent read` line lists indexed files the parent read (Claude Code `Read`
calls and `fox_read` of a path, newest first), from the last 16 MB of the parent's transcript,
as many as fit the 1,500-character brief cap; it names files, never their content. Codex
subagents get no such line: Codex reads files through the shell, which the transcript does not
record as reads.

**SubagentStop** saves the final answer as a `report`, as before, and posts a `summary` entry
that holds its opening (up to 600 characters), the report's handle, and the paths and handles it
cites, as handles. Siblings reading the board get the gist without the report.

## Dispatch

`fox dispatch` registers the worker, takes the writer lease if asked, writes the prompt and
starts a detached supervisor (`fox _supervise`, own session via `setsid`). The supervisor owns
the child, feeds the prompt on stdin, posts the child's final message as a `report` handoff (a
child of the task handoff when the task was `h:…`), marks the agent done or failed, frees the
lease and touches `$FOXDB_HOME/agents/<id>/done`. Logs and events stay in that directory.

The prompt is the user message, never a system prompt: one fixed brief that names the parent's
packet, then the task. The packet is `--packet h:…` if given, else the newer of the parent's own
latest packet (`fox handoff create` / `op=packet`) and its board snapshot (frozen open decisions
and requests, with a pointer to that packet). Siblings started together get the same handle, so
the brief is byte-identical; the snapshot is rebuilt only once the board has new decisions or
requests and it is older than `FOXDB_SNAPSHOT_SECS` (default 600). Check and insert run in one
transaction, so parallel spawns (SubagentStart included) share one. Reviewers (`--reviewer`, `reviewer: true` over MCP, or a label matching
`[hooks] skip_agents`) get the task alone, and their SessionStart and SubagentStart hooks inject
nothing. A reviewer cannot hold the writer lease.

The fox-db MCP server runs outside a caller's sandbox, so dispatch checks the caller instead: a
registered reviewer starts nothing, a read-only worker cannot start a writer, a tree is at most
`FOXDB_MAX_DEPTH` (default 3) deep, and a parent has at most `FOXDB_MAX_CHILDREN` (default 8)
running children.

Flags used, checked against `codex exec --help` (codex-cli 0.169.0) and `claude --help` (2.1.322):

| Harness | Command |
|---|---|
| Codex | `codex exec --json -o <agents/ID/last.txt> --output-schema <agents/ID/report.schema.json> -C <repo> -s read-only\|workspace-write [--skip-git-repo-check outside git] -c mcp_servers.foxdb.command="<fox>" -c 'mcp_servers.foxdb.args=["mcp"]' -c 'mcp_servers.foxdb.env_vars=["FOXDB_AGENT","FOXDB_PARENT","FOXDB_HOME","FOXDB_SESSION"]' [-m M] -` |
| Claude | `claude -p --output-format json --json-schema <schema> --session-id <uuid> --mcp-config '<json>' --allowedTools 'mcp__foxdb__*' --exclude-dynamic-system-prompt-sections [--permission-mode acceptEdits for writers] [--model M]` |

The Codex thread id comes from the `thread.started` event, so `codex exec resume <id>` works
later; the Claude session id is assigned before launch, so `claude -p --resume <uuid>` does.
Neither `--ephemeral` nor `--no-session-persistence` is ever passed. `FOXDB_CODEX_BIN` and
`FOXDB_CLAUDE_BIN` override the executables.

Codex sandbox: in `read-only` and `workspace-write` the child's own shell may be unable to write
`$FOXDB_HOME` or reach the `foxd` socket, so the brief tells it to use the fox-db MCP tools, whose
server runs outside the sandbox. Nothing relies on Codex hooks, which need trust.

Whether `codex exec` auto-approves those MCP calls is checked only by one opt-in test that makes
a real model call; the stub tests cannot. It is off by default and costs one short Codex turn:

```sh
FOXDB_REAL_SMOKE=1 cargo test --release --test orchestration real_codex_smoke_opt_in -- --nocapture
```

It dispatches one read-only Codex worker in a scratch repo and store (your real `codex` and its
login, nothing else of yours) that must call `fox_memory op=post` and answer in the report
format. It passes when the worker is `done` with a well-formed report and its post is on the
board, which shows that `codex exec` ran the fox-db MCP call without an approval prompt.

## Verifiable reports

Every worker's handoff points at an exact change and has one format.
- **Base and head commits.** At dispatch the worktree is recorded as a commit id (HEAD when
  clean, else a commit of the whole worktree on top of HEAD, untracked files included); again
  when the worker is done or found stale. The commits are built with a private copy of the
  index (`GIT_INDEX_FILE`, `write-tree`, `commit-tree`), so the user's index, HEAD, refs and files
  are untouched; only objects are added, no ref points at them, and `git gc` may prune them after
  its expiry. Identity and dates are fixed, so the same tree is the same id. The report, the
  agent's packet and `fox agents` (`base..head`) cite `git diff <base> <head>`; the controller's
  own uncommitted edits sit in the base, not in the worker's change.
- **Fixed format.** The final answer must be one JSON object: `status` (done/partial/blocked),
  `summary`, `files` (path or path:line), `tests` (`ran`: exact commands, `passed`), `decisions`,
  `next`. Codex gets the schema as `--output-schema`, Claude as `--json-schema` (its validated
  object arrives as `structured_output`), and the supervisor parses it again. A malformed answer
  marks the worker `failed` with `malformed report: <why>` and the raw text; it is never guessed
  at. The stored report is plain text: `<status>: <summary>`, then `files:`, `tests:`,
  `decision:`, `next:`, `change:` and `check:` lines.
- **Checks**, rules only, no model calls, each a `check:` line (and `(N checks)` in `fox wait`):
  a writer that changed nothing (base = head); tests claimed to pass with none run; a test command
  the Codex event log never shows (Claude's JSON output has no command log, so that rule is
  Codex only); a writer claiming a file its diff does not touch.
- **End status.** A worker ends `done` only when it exited cleanly with a `done` report and no
  check fired. Otherwise it ends `failed` (crash, no final message or malformed report), `blocked`
  or `partial` (its own report says so) or `flagged` (a `done` report with checks). Every finished
  worker records its head commit, failed ones too, so partial work stays addressable. Only a
  `done` worker's report closes its handoff: after any other ending, `fox handoff take` lets a
  retry worker claim it, and `fox wait h:N` waits for the retry.

## Wait

`fox wait` runs in the caller, never in `foxd` (the daemon serializes requests). It watches
`$FOXDB_HOME/agents/*/done` with inotify, falls back to a 250 ms poll of SQLite's
`data_version`, and checks liveness about once a second, so a dead worker turns `stale` instead
of hanging. A handoff ends at its first report from a `done` worker (or one posted by hand), or
when every worker dispatched for it has ended without one. Exit codes: 0 all done, 3 still
running at the timeout, 4 ended any other way (stale, failed, blocked, partial, flagged, or a
handoff without a done report), so `fox wait $W && merge` stops on a crash, a blocked or partial
report and a report that failed its checks; read the report before overriding it. Through MCP, `op=wait` returns within 50 s (Codex's
default tool timeout is 60 s) with "still running" and you call it again; `any: true` returns as
soon as one target finishes. For long waits use the harness's own background waits:
Claude's Bash `run_in_background` or Monitor on `fox wait …`; Codex runs `fox wait …` in its shell.

Waking an idle Claude controller when a worker finishes: Claude Code's `asyncRewake` command
hooks run in the background, and exit code 2 wakes the session even when it is idle, with the
hook's stderr as a reminder (the hook `timeout` still applies; in `-p` mode the hook is killed
at teardown). A PostToolUse hook on `fox dispatch` can wait for the new worker. fox-db installs
nothing; add it to `~/.claude/settings.json` yourself (needs `jq`; checked against the Claude
Code hooks documentation, not exercised by fox-db's tests):

```json
{"hooks": {"PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command",
  "asyncRewake": true, "timeout": 3600,
  "command": "ids=$(jq -r '.tool_response.stdout // empty' | grep -oE '^[A-Za-z0-9._-]+ dispatched' | cut -d' ' -f1 | xargs); [ -n \"$ids\" ] || exit 0; fox wait $ids --timeout 3550 | head -c 600 >&2; exit 2"}]}]}}
```

The hook reads every `<id> dispatched` line the Bash call printed and waits on all of them, so
several dispatches in one call wake the controller once, when the last one ends. It runs after
every Bash call (no `if` filter: a call that assigns `W=$(fox dispatch …)` does not start with
`fox dispatch`) and exits at once when the call dispatched nothing. It sees only
the call's stdout: a capture such as `W=$(fox dispatch … | cut -d' ' -f1)` hides the line, so
print the ids again (`printf '%s dispatched\n' "$W"`), as the worked example below does.
The worker's one-line status (`cx-3 done report h:2f  done: …`) then arrives as the next turn.

## Switching

`fox switch --to <harness>` prints a command; `--exec` runs it.
- Same harness: the native resume, no packet: `claude --resume <session>` (add `--fork-session`
  to branch) or `codex resume <thread>`.
  A known agent with no resumable id (a worker that died before reporting) gets a packet
  instead; `--last` is only for when no agent is named.
- Across harnesses: a packet (given, or built from `--transcript`, which reads Claude `.jsonl` and
  Codex `.jsonl` / `.jsonl.zst`), then the other harness with the fox-db MCP wired per process and a
  prompt that references the packet (`--inline` pastes it instead). The prompt comes right after
  the harness name, since Claude's `--allowedTools <tools...>` is variadic. The line carries
  `FOXDB_AGENT` (a new id), `FOXDB_PARENT` (the agent or transcript session it continues) and
  `FOXDB_HOME`; with `--exec` the new session is registered under this process's pid:

```
# packet h:2a (~240 tokens)
FOXDB_AGENT=cx-2b FOXDB_PARENT=5e55-1 FOXDB_HOME=~/.foxdb codex -C /repo -c 'mcp_servers.foxdb.command="/path/to/home/.foxdb/bin/fox"' -c 'mcp_servers.foxdb.args=["mcp"]' -c 'mcp_servers.foxdb.env_vars=[…]' 'Continue the task in foxdb handoff h:2a: take it with fox_memory op=take id=h:2a, read it with fox_read target=h:2a, then carry on.'
```

## Native mechanisms first

fox-db adds the shared store; it does not replace what a harness already does well.
- Claude in Claude: the Agent tool with the SubagentStart/SubagentStop hooks above. Background
  sessions: `claude --bg`, then `claude agents`, `logs`, `attach`, `stop`.
- Codex: `codex queue --thread <id> --message <text> [--resume]` to message a running thread,
  `codex exec resume <id>`, `codex fork`, `codex agents`. Its `multi_agent_v2` feature is stable but off by default.
- Same-harness continuation is the native resume; packets are for crossing harnesses or agents.

## Harness setup

Claude Code: the MCP entry and hooks in [Claude Code setup](usage.md#claude-code-setup). A dispatched
Claude child gets its MCP server through `--mcp-config` and permission through `--allowedTools`
(print mode denies unapproved MCP calls); add `--strict-mcp-config` yourself only if user servers
should be dropped.

Codex `~/.codex/config.toml` (forward the identity variables; Codex gives a server only these):

```toml
[mcp_servers.foxdb]
command = "/path/to/home/.foxdb/bin/fox"
args = ["mcp"]
env_vars = ["FOXDB_AGENT", "FOXDB_PARENT", "FOXDB_HOME", "FOXDB_SESSION"]
tool_timeout_sec = 60
```

Codex hooks (`~/.codex/hooks.json`; the `hooks` feature is stable and on in 0.169.0, and Codex asks
you to trust each hook). The file shape below mirrors Claude's and is not verified against Codex;
check `/hooks` after adding it. Replace `/path/to/home` with your home directory (absolute paths, so
the hooks work whatever `PATH` Codex sees):

```json
{"hooks": {
  "SessionStart": [{"hooks": [{"type": "command", "command": "/path/to/home/.foxdb/bin/fox hook session-start --harness codex"}]}],
  "SubagentStart": [{"hooks": [{"type": "command", "command": "/path/to/home/.foxdb/bin/fox hook subagent-start --harness codex"}]}],
  "SubagentStop": [{"hooks": [{"type": "command", "timeout": 30, "command": "/path/to/home/.foxdb/bin/fox hook subagent-stop --harness codex"}]}],
  "PreCompact": [{"hooks": [{"type": "command", "command": "/path/to/home/.foxdb/bin/fox hook pre-compact --harness codex"}]}]
}}
```

opencode (`opencode.json`; MCP tools sit behind its Code Mode by default):

```json
{"mcp": {"foxdb": {"type": "local", "command": ["/path/to/home/.foxdb/bin/fox", "mcp"]}}}
```

DeepSeek harness (DSH): add fox-db as a stdio MCP overlay (`$DSH_HOME/cordis.patch.yml`, same shape
as its memory-server overlays); its bridge strips credential-like and `DSH_*` variables, so set
`FOXDB_HOME` explicitly if you use a non-default store.

`AGENTS.md` pointer line, for any of them:

```md
- Shared agent context lives in foxdb: `fox_memory op=read since=me` for new board posts, `op=take id=h:…` before working a handoff, `op=post` for findings; packets via `fox handoff render h:…`.
```

## Worked examples

Claude controller, three Codex workers, one writer:

```sh
export FOXDB_AGENT=ctl                     # the controller's identity; children get it as FOXDB_PARENT
fox handoff create --goal "Split search.rs" --next "writer edits; readers review"   # packet h:1
# every worker's brief points at h:1 (ctl's latest packet); --packet h:… picks another
W=$(fox dispatch --harness codex --task "Split src/search.rs into arms" --writer --label impl | cut -d' ' -f1)
R1=$(fox dispatch --harness codex --task "List callers of find_warm" --label scan | cut -d' ' -f1)
R2=$(fox dispatch --harness codex --task "Check tests touching search" --label scan | cut -d' ' -f1)
printf '%s dispatched\n' $W $R1 $R2 # the lines the wake hook reads
fox dispatch --harness codex --task "also edit" --writer   # refused: writer lease … held by cx-1
fox wait $W $R1 $R2 --timeout 1800        # in Claude: Bash run_in_background, then read on notify
fox mem read --kind report                # the three reports, as h: handles; fox_read expands one
```

Codex handing to Claude (through MCP, so it works inside the Codex sandbox):
`fox_memory {"op":"packet","text":"Finish the lease docs","next":"proofread README"}` → `packet h:31`;
`fox_memory {"op":"dispatch","harness":"claude","text":"h:31"}` → `cl-4 dispatched …` (its brief
also points at h:31, the caller's latest packet);
`fox_memory {"op":"wait","id":"cl-4"}` until it reports.

Switching mid-task, Claude to Codex: `fox switch --to codex --transcript ~/.claude/projects/<p>/<session>.jsonl`
prints the packet handle and the `codex` command; paste it, or add `--exec`.

Resuming a stale agent's work: `fox agents --tree` shows `cx-7 … stale`; `fox handoff create --agent cx-7`
builds a packet from its task, last output and status (or its registered transcript), touched
files and its own reports (`fox switch --to codex --agent cx-7` resumes its Codex thread instead,
read from its event log); `fox dispatch --harness
claude --task h:<that packet>` (or `fox switch --to claude --handoff h:…`) carries on, and the new
report becomes a child of the packet.

Measured (`fox-bench orch --repo ~/foxdb --n 10`, stub worker, `bench/results/m3-orchestration-foxdb.json`;
milestone 2 in `m2-orchestration-foxdb.json`): a packet for this repo with a clean tree is about
135 tokens. A realistic mid-task packet (a scratch clone with 12 edited files, 3 new ones, 6
decisions with reasons, 4 open requests and 8 reports and findings) is about 610 tokens, under both
harness caps. `fox dispatch` takes about 48 ms, most of it the base-commit snapshot (about 12 ms
in milestone 2, before snapshots), and `fox wait` returns about 57 ms after dispatch for a worker
that exits at once (it also snapshots the head and checks the report).
