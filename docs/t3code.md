# foxdb under T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) (MIT) is a GUI and control plane over coding
harnesses: Codex, Claude Code, OpenCode, Cursor, and agents that speak ACP. Every T3 thread runs
one of those harnesses. Give each of them the foxdb MCP server and every thread, whatever its
provider, shares one foxdb index and one board, because they all open the same store
(`$FOXDB_HOME/fox.db`, default `~/.foxdb`).

foxdb does not need T3 installed, and T3 does not need foxdb. What follows was checked against
T3's source at commit `2b2d65d` (November 2026); file names are relative to that repository.

## How T3 gives its providers MCP servers

T3 adds its own server, `t3-code` (HTTP, with a per-session bearer token), to each provider
session. It does not replace the servers the provider already has:

| Provider | What T3 does | Where foxdb goes |
|---|---|---|
| Codex | Starts `codex app-server` with `CODEX_HOME` from the provider settings and, per thread, config overrides for `mcp_servers.t3-code` only. The source says it overrides "only T3's servers so launch-argument MCP servers survive" (`apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts`). | `config.toml` in that `CODEX_HOME`, or the provider's launch arguments |
| Claude | Runs Claude Code through the Agent SDK with `mcpServers: {"t3-code": …}` added to the query (`ClaudeAdapterV2.ts`). It uses Claude Code's own configuration directory (`CLAUDE_CONFIG_DIR` when set per provider instance, see `docs/user/providers-claude.md`). | Claude Code's MCP config for that directory, or the project's `.mcp.json` |
| OpenCode | Adds `t3-code` at runtime with OpenCode's `mcp.add` for the thread's directory (`packages/provider-opencode/src/server/v2/adapter.ts`); OpenCode's own configuration stays. | `opencode.json` (`mcp` section) |
| ACP agents | Sends `mcpServers` in the ACP `session/new` (or load/resume) request, and that list holds T3's servers (`packages/provider-acp/src/server/AcpSessionRuntime.ts`). | The agent's own MCP configuration, if it has one |

## Register foxdb

foxdb's server is `fox mcp` over stdio. Use the absolute path of `fox` if T3 runs with a
different `PATH` than your shell (a desktop app often does).

**Codex** (in the `CODEX_HOME` T3's Codex instance uses; `~/.codex` by default):

```sh
codex mcp add foxdb -- fox mcp
```

or in `config.toml`:

```toml
[mcp_servers.foxdb]
command = "fox"
args = ["mcp"]
env_vars = ["FOXDB_AGENT", "FOXDB_PARENT", "FOXDB_HOME", "FOXDB_SESSION"]
```

A Codex instance with a shadow home shares the main home's configuration, so one entry covers
both accounts. Launch arguments work too:
`-c mcp_servers.foxdb.command="fox" -c 'mcp_servers.foxdb.args=["mcp"]'`.

**Claude Code** (user scope, for the default configuration directory):

```sh
claude mcp add --scope user foxdb -- fox mcp
```

For a Claude instance with its own `CLAUDE_CONFIG_DIR`, run the same command with that variable
set, or commit a project `.mcp.json` so every Claude thread in the project gets it:

```json
{ "mcpServers": { "foxdb": { "command": "fox", "args": ["mcp"] } } }
```

**OpenCode** (`opencode.json`, in the project or the user config):

```json
{ "mcp": { "foxdb": { "type": "local", "command": ["fox", "mcp"], "enabled": true } } }
```

Then use **Refresh provider status** in T3's provider settings (`docs/user/providers-opencode.md`).

**ACP agents:** register `fox mcp` in the agent's own configuration if it has one. T3's ACP
settings have no field for extra servers today.

After registering, start a new thread for each provider and ask it to run
`fox_find query=<a symbol you know>`. The answers come from the same store, and a post one
thread makes with `fox_memory op=post` is what another thread reads with `op=read since=me`.
For the hooks (session brief, subagent briefs), follow the Claude Code and Codex setup in
[usage.md](usage.md#claude-code-setup) and [AGENTS-SETUP.md](../AGENTS-SETUP.md): they work the same under T3, because T3 runs those harnesses with their own settings.

## Identity

Each provider session starts its own `fox mcp`, and without `FOXDB_AGENT` foxdb names the agent
after the MCP client and the parent process (`<client>-<pid>`; see
[multi-agent.md](multi-agent.md)). Whether two T3 threads are two foxdb agents depends on how the provider runs them:

- **Codex: one identity for all threads.** T3 runs one `codex app-server` for many threads
  ("One app-server serves many threads", `CodexAdapterV2.ts`). Its per-thread id goes only into
  the shell environment of commands the thread runs (`T3CODE_THREAD_ID`), not into MCP servers.
  So every Codex thread under one app-server shows up as the same agent (for example
  `codex-mcp-client-41582`) and shares one `since=me` cursor. One thread's
  `op=read since=me` then consumes posts another thread has not seen. Under T3, Codex threads
  should read the board with an explicit `since=h:N` cursor, or name themselves with `agent=<name>`
  on each read and post (as Claude Code subagents do); setting
  `FOXDB_AGENT` in the provider environment does not help, because it is still one value for all
  threads.
- **Claude, OpenCode and ACP agents:** separate threads are separate agents only when the
  provider starts a separate process tree per thread. Check `fox agents` after starting two
  threads; if both show the same id, treat them as the Codex case above.

Do not set one `FOXDB_AGENT` in a provider's environment for all its threads: it gives every
thread that provider runs one cursor, even where they would otherwise be separate.

## T3's thread relationships and foxdb's handoffs

T3 links threads with one primitive, a context transfer, of five kinds
(`packages/contracts/src/orchestrationV2.ts`). foxdb has a counterpart for each, built from its
handoffs (`h:` handles), packets with lineage, and the agent registry:

| T3 kind | What it is in T3 | foxdb counterpart |
|---|---|---|
| `fork` | A new thread from a stable point of another (a completed run or checkpoint). | `fox handoff create --parent h:N`: a packet whose `parent:` line points at the source packet; the source lists it under `children:`. |
| `provider_handoff` | The same work continues on another provider; T3 resumes that provider's old thread with a delta, or starts fresh with a portable summary. | Outside T3, `fox switch --to codex\|claude`: a packet, plus a new agent whose `FOXDB_PARENT` is the old one. Inside T3, `fox switch --for t3` makes the packet and prints the message to paste, but no agent and no `FOXDB_PARENT`: T3 starts the provider, so foxdb cannot set its environment. The link is the packet itself (it names the agent it came from), and the thread that takes it is recorded as its taker. |
| `merge_back` | A forked thread's result returns to its source. | The worker's report, posted as a `report` handoff; `fox handoff take` on the original task then answers `already_done: report h:M`. A swarm's summary is a merge-back of many. |
| `subagent_spawn` | `delegate_task` starts a child thread with the task only. | `fox dispatch` (or a swarm task): a registered child with `parent` set, the fixed brief plus the task. |
| `subagent_result` | The child's published result, finalized once. | The child's schema-checked report (`report` handoff, `agent.report`); `fox wait` returns it. |

Both systems are deterministic here: T3's portable handoffs are a budgeted selection with no
model-written summary, and foxdb packets are rendered the same way every time.

## Avoid paying twice

- **One handoff, not two.** When you switch providers inside T3, T3 already carries the
  conversation (up to 16,000 tokens by default, `T3CODE_CONTEXT_HANDOFF_TOKEN_CAP`). Do not
  paste a full foxdb packet on top. `fox switch --for t3` prints a one-line message that points
  at the packet, and `fox handoff render h:N --for t3` caps the packet at 2,000 characters; the
  thread reads the rest with `fox_read` when it needs it.
- **One orchestrator per task.** T3's `delegate_task` and `fox dispatch` (or `fox swarm`) each
  keep their own registry and wait. Pick one for a given task tree. foxdb's writer leases only
  cover workers foxdb started; T3's answer to parallel writers is a worktree per thread.
- **Tool list size.** T3 measures and budgets its own MCP tools; foxdb's sit beside them in the
  same prefix. foxdb keeps its four tool definitions frozen and small (`context-budget.json`), so
  adding it changes the prefix once, not per turn.
- **Compaction.** T3 can compact a Claude thread just before its prompt cache expires. The MCP
  tools keep no "already sent" set, so a read after a compaction returns the full text.
