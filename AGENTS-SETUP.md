# Setting up foxdb (for AI agents)

This file is for an agent asked to "set up foxdb from <repo>". Follow it top to bottom. Every
command block marked `sh` below can be run as written; replace only what a comment tells you to
replace. `scripts/docs-test.sh` runs these same blocks in a scratch home, so they are known to work.

## Rules (read first)

- **Ask before changing any harness setting.** Registering the MCP server, adding hooks, copying
  the skill and editing `AGENTS.md` all change the user's environment. Show the user what you
  will run and get a yes for each harness. Installing the binaries into `~/.foxdb` needs no
  harness change.
- **Back up a settings file before you edit it** (the commands below do), and edit it only with
  the commands shown or the harness's own CLI (`claude mcp add`, `codex mcp add`).
- **Never edit harness memory files**: `CLAUDE.md`, `~/.claude/projects/*/memory/`, Codex
  memories or any other file an agent writes for itself. foxdb only reads them.
- Never use `sudo`; everything installs under the user's home.
- Never delete `~/.foxdb` unless the user asked to uninstall: it holds their index and handoff board.
- Do not start `foxd` as a system service without asking; it is optional.
- If a step fails, stop and report the exact output instead of improvising around it.

## 1. Prerequisites

```sh runnable
uname -sm                                   # Linux x86_64 gets a prebuilt binary; anything else builds from source
git --version
command -v curl >/dev/null || command -v wget >/dev/null && echo "download: ok" || echo "no curl or wget (source build)"
command -v cargo >/dev/null && cargo --version || echo "cargo: missing (needed only for a source build)"
for h in claude codex opencode jq; do command -v "$h" >/dev/null && echo "found $h" || echo "no $h"; done
[ -z "${FOXDB_HOME:-}" ] || [ "$FOXDB_HOME" = "$HOME/.foxdb" ] || echo "FOXDB_HOME=$FOXDB_HOME is not ~/.foxdb: read the note below before going on"
```

The repository is public; no GitHub login is needed. You need either curl or wget on Linux
x86_64 (for the prebuilt release asset) or `cargo` 1.88 or newer for a source build (install
from https://rustup.rs if missing, after asking). `jq` is needed only for the Claude Code hooks
step. Note which harnesses were found: you will set up only those.

This guide uses the default data directory `~/.foxdb`. If `FOXDB_HOME` is set to anything else,
ask the user to unset it for foxdb (simplest). Otherwise replace `~/.foxdb` and `$HOME/.foxdb`
with that directory in every command and snippet below, and make sure each harness passes the same
`FOXDB_HOME` to `fox`; the installer and `fox` follow `FOXDB_HOME`, the snippets below do not.

## 2. Build or install

If the user named a local checkout (for example "set up foxdb from ~/foxdb"), `cd` into it and
run `scripts/install.sh --source` instead of the block below, so that checkout is what gets built
(without `--source` the installer prefers the v0.2.2 release asset on Linux x86_64).
Otherwise clone the repository and run the installer from inside the clone:

```sh
git clone https://github.com/ManavA/foxdb.git && cd foxdb
```

```sh runnable
scripts/install.sh
export PATH="$HOME/.foxdb/bin:$PATH"
fox --version
```

On Linux x86_64 `scripts/install.sh` downloads the v0.2.2 release tarball and its `.sha256` from
the GitHub release with curl or wget (no login) and checks the checksum; otherwise it builds `fox`
and `foxd` from source with cargo (cloning tag v0.2.2 when run outside a checkout).
`FOXDB_RELEASE_URL` overrides where the asset is downloaded from.
It copies them to `~/.foxdb/bin` (or `$FOXDB_HOME/bin`) and the skill to `~/.foxdb/skill/`, and
touches nothing else. `--source` forces a source build; `--tarball FILE` installs a tarball you
already downloaded. The prebuilt binary needs glibc 2.39 or newer; on an older system the
installer falls back to a source build. Expected last line: `fox 0.2.2`.

A source build takes several minutes (over 5 on a busy machine). If your tool calls have a time
limit, run `scripts/install.sh` in the background with its output going to a log file, and poll
the log until it prints `installed fox 0.2.2`.

Ask the user whether to add `export PATH="$HOME/.foxdb/bin:$PATH"` to their shell profile.
The harness entries below use absolute paths, so they work without it.

## 3. config.toml

All settings are optional. Create the file only if it does not exist:

```sh runnable
mkdir -p ~/.foxdb
[ -f ~/.foxdb/config.toml ] || cat > ~/.foxdb/config.toml <<'EOF'
# foxdb settings; every key is optional. Reference: docs/usage.md#config
# Agents whose name or type matches get no SubagentStart brief (zero-context reviewers).
[hooks]
skip_agents = ["egm-*", "*goldfish*", "*reviewer*"]
EOF
cat ~/.foxdb/config.toml
```

For Swift projects, `[module_tags]` maps Xcode module names to platform tags; see
`config.example.toml` in the repository.

## 4. Index the user's projects

Ask the user which projects to index (any git repositories). Index each one once; after that
foxdb refreshes in the background whenever an agent uses it there:

```sh runnable
cd ~/src/my-app            # replace with each of the user's project directories
fox index .
fox status
```

Expected: `fox index` prints one line ending in the file, section and symbol counts and a time
in ms; `fox status` lists the project with `*` in front of it and `indexed ... ago`. A very large
project (tens of thousands of files) can take a few minutes the first time; run it in the
background and poll.

## 5. MCP registration

The server is `<home>/.foxdb/bin/fox mcp` over stdio. Use the absolute path; the commands
below fill it in from `$HOME`.

### Claude Code

```sh runnable needs=claude
claude mcp add --scope user foxdb -- "$HOME/.foxdb/bin/fox" mcp
```

### Codex

This backs up `~/.codex/config.toml` (once, before the first change), registers the server and
adds two settings to its entry:

```sh runnable needs=codex
mkdir -p ~/.codex
[ ! -f ~/.codex/config.toml ] || [ -f ~/.codex/config.toml.foxdb-backup ] || cp ~/.codex/config.toml ~/.codex/config.toml.foxdb-backup
codex mcp add foxdb -- "$HOME/.foxdb/bin/fox" mcp
grep -q '"FOXDB_SESSION"' ~/.codex/config.toml || sed -i '/^\[mcp_servers\.foxdb\]$/a env_vars = ["FOXDB_AGENT", "FOXDB_PARENT", "FOXDB_HOME", "FOXDB_SESSION"]\ntool_timeout_sec = 60' ~/.codex/config.toml
codex mcp get foxdb
```

The `env_vars` line forwards the agent identity variables (Codex passes a server only the
variables it names); `tool_timeout_sec = 60` covers `fox_memory op=wait`, which returns within
50 s. On macOS use `sed -i ''` instead of `sed -i`.

### opencode

Add to the user's `opencode.json` (global: `~/.config/opencode/opencode.json`), merging with any
existing `mcp` key, after backing the file up. Replace `/path/to/home` with the user's home:

```json
{"mcp": {"foxdb": {"type": "local", "command": ["/path/to/home/.foxdb/bin/fox", "mcp"]}}}
```

opencode puts MCP tools behind its Code Mode by default.

### T3 Code

T3 Code runs Claude Code, Codex and opencode with their own settings, so registering foxdb with
each harness as above is enough; each T3 thread then shares the same index and board. Read
[docs/t3code.md](docs/t3code.md) for the identity caveat (all Codex threads under one T3
app-server look like one foxdb agent unless `FOXDB_AGENT` is set per thread) and for
`fox switch --for t3`.

### Other harnesses

Any MCP client that can launch a stdio server: command `/path/to/home/.foxdb/bin/fox`, argument
`mcp`. Forward `FOXDB_AGENT`, `FOXDB_PARENT`, `FOXDB_HOME` and `FOXDB_SESSION` if the harness
filters the environment.

## 6. Optional hooks

Hooks give every session a short project brief and let subagents report back on the board.
Each is a command hook that prints nothing and exits 0 on any error or after 1.5 s. Ask first.

| Hook | What it does | Cache safety |
|---|---|---|
| `SessionStart` | Project brief (current gate, open milestones, board), capped at 9,500 characters | Runs once per session, before the first prompt, so it is part of the cached prefix |
| `SubagentStart` | A ~1,500-character brief for the helper and a pointer to the parent's board snapshot; skipped for agents matching `skip_agents` | The snapshot is frozen per parent session, so sibling subagents get byte-identical text |
| `SubagentStop` | Saves the helper's final message as a report on the board; prints nothing | Injects nothing |
| `PreCompact` | Saves a handoff packet before compaction; prints nothing | Injects nothing; the following session start adds one line |

foxdb never injects from `PreToolUse`, `PostToolUse` or `UserPromptSubmit`, which would rewrite
the cached history or grow every turn; a test enforces this.

### Claude Code hooks

This backs up `~/.claude/settings.json` (once: a second run keeps the first, pre-foxdb backup),
then adds the four hooks, replacing any earlier foxdb entries and keeping everything else:

```sh runnable needs=jq
mkdir -p ~/.claude && f=~/.claude/settings.json && { [ -f "$f" ] || echo '{}' > "$f"; }
[ -f "$f.foxdb-backup" ] || cp "$f" "$f.foxdb-backup"
jq '
  def foxhook(ev): {hooks: [{type: "command", timeout: 2,
    command: ("H=\"$HOME/.foxdb/bin/fox\"; [ -x \"$H\" ] && \"$H\" hook " + ev + " --harness claude; exit 0")}]};
  def notfox: all(.hooks[]?; (.command // "") | contains(".foxdb/bin/fox") | not);
  .hooks //= {}
  | reduce (["SessionStart","session-start"], ["SubagentStart","subagent-start"],
            ["SubagentStop","subagent-stop"], ["PreCompact","pre-compact"]) as [$e, $v]
      (.; .hooks[$e] = ([(.hooks[$e] // [])[] | select(notfox)] + [foxhook($v)]))
' "$f" > "$f.foxdb-new" && mv "$f.foxdb-new" "$f"
jq -r '.hooks | keys[]' "$f"
```

Expected output: `PreCompact`, `SessionStart`, `SubagentStart`, `SubagentStop` (plus any events
the user already had).

### Codex hooks

Codex hooks take the same commands with `--harness codex` (output capped at about 2,200 tokens).
The file shape below mirrors Claude's and is not verified against Codex; Codex asks the user to
trust each hook, and `/hooks` in Codex shows whether they loaded. Write it to
`~/.codex/hooks.json` only if that file does not exist yet; otherwise show it to the user. Replace
`/path/to/home` with the user's home (absolute paths, so the hooks work whatever `PATH` Codex sees):

```json
{"hooks": {
  "SessionStart": [{"hooks": [{"type": "command", "command": "/path/to/home/.foxdb/bin/fox hook session-start --harness codex"}]}],
  "SubagentStart": [{"hooks": [{"type": "command", "command": "/path/to/home/.foxdb/bin/fox hook subagent-start --harness codex"}]}],
  "SubagentStop": [{"hooks": [{"type": "command", "command": "/path/to/home/.foxdb/bin/fox hook subagent-stop --harness codex"}]}],
  "PreCompact": [{"hooks": [{"type": "command", "command": "/path/to/home/.foxdb/bin/fox hook pre-compact --harness codex"}]}]
}}
```

Optional, for controllers that dispatch workers: the `asyncRewake` wake hook in
[docs/multi-agent.md](docs/multi-agent.md#wait) wakes an idle Claude session when a worker ends.

## 7. Skill and AGENTS.md lines

Claude Code skill (tells agents when to use the tools):

```sh runnable needs=claude
mkdir -p ~/.claude/skills/foxdb && cp ~/.foxdb/skill/SKILL.md ~/.claude/skills/foxdb/
```

For Codex, opencode and any agent that reads `AGENTS.md`, offer to add these lines to the
project's `AGENTS.md` (show them to the user; do not edit `CLAUDE.md` or memory files):

```md
## Code navigation
Use foxdb before grepping: `fox_find` / `fox find <q>`, `fox_graph` / `fox code <sym> --rel callers|outline|src|impact`,
`fox_memory op=brief`. Rows start with handles and end in path:line; `~` rows are tree-sitter, not compiler-resolved.
- Shared agent context lives in foxdb: `fox_memory op=read since=auto` for new board posts, `op=take id=h:…` before working a handoff, `op=post` for findings; packets via `fox handoff render h:…`.
```

## 8. Verify

```sh runnable
fox --version
cd ~/src/my-app            # a project you indexed in step 4
fox status | head -3
fox find "load config" --budget 100
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"check","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | fox mcp | grep -o '"name":"fox_[a-z]*"' | sort -u
echo '{"hook_event_name":"SessionStart","source":"startup","session_id":"setup-check"}' | fox hook session-start --harness claude | cut -c1-60
```

Expected: `fox 0.2.2`; the project with `*` and `indexed ... ago`; rows that start with a
handle (`c:`, `f:`, `d:` …) and end in `path:line`; exactly four tool names (`fox_find`,
`fox_graph`, `fox_memory`, `fox_read`); and a JSON line starting
`{"hookSpecificOutput":{"additionalContext":"foxdb · `.

```sh runnable needs=claude
claude mcp list | grep foxdb
```

```sh runnable needs=codex
codex mcp list | grep foxdb
```

Expected: `foxdb: … fox mcp - ✔ Connected` for Claude Code and an `enabled` foxdb row for Codex.
Finally ask the user to start a new session and run `fox_find` on a symbol they know.

## 9. Uninstall and rollback

Each block undoes one setup step; run only those for harnesses you set up. The blocks remove
only foxdb's entries and then delete the `*.foxdb-backup` files made during setup. Those backups
hold the files as they were before setup; if the user would rather restore one (which also undoes
every later change), copy it back instead of running that harness's block.

Claude Code: removes the server, the skill and the foxdb hooks (any hook whose command contains
`.foxdb/bin/fox`), keeps every other setting, then deletes the setup backup:

```sh runnable needs=claude
claude mcp remove --scope user foxdb
rm -rf ~/.claude/skills/foxdb
f=~/.claude/settings.json
if [ -f "$f" ] && command -v jq >/dev/null; then
  jq 'if .hooks then .hooks |= (with_entries(.value |= map(select(all(.hooks[]?; (.command // "") | contains(".foxdb/bin/fox") | not))))
        | with_entries(select(.value | length > 0))) | if .hooks == {} then del(.hooks) else . end else . end' "$f" > "$f.foxdb-new"
  mv "$f.foxdb-new" "$f"
fi
rm -f "$f.foxdb-backup"
```

Codex: `codex mcp remove` deletes the whole `[mcp_servers.foxdb]` table, including the
`env_vars` and `tool_timeout_sec` lines added in step 5:

```sh runnable needs=codex
codex mcp remove foxdb
rm -f ~/.codex/config.toml.foxdb-backup
```

If you wrote `~/.codex/hooks.json` in step 6, delete it, or, if the user has added other hooks
since, remove only the entries whose command contains `.foxdb/bin/fox`. For opencode, remove the
`foxdb` key under `mcp` in `opencode.json` (or restore the backup you made, then delete it).

Finally the daemon and the data directory:

```sh runnable
fox daemon stop            # prints "foxd stopped", or "foxd not running"
rm -rf ~/.foxdb            # deletes the index and the handoff board; ask first
```

Remove the `PATH` line from the shell profile and any lines added to `AGENTS.md` by hand.

## Cheat-sheet for agents using foxdb

**When to use which tool**

| Need | MCP | CLI |
|---|---|---|
| Find a symbol, file, doc section, milestone, memory note or past session | `fox_find q=… kind=…` | `fox find <q> [--kind …]` |
| Callers, callees, refs, overrides, conformers, blast radius of a change | `fox_graph target=… rel=…` | `fox code <sym> --rel callers\|impact\|…` |
| Outline of a file or type, one symbol's source, a doc section, a handoff | `fox_read target=… mode=…` | `fox code <sym> --rel outline\|src` |
| Session brief, recall, notes, board, packets, dispatch, wait | `fox_memory op=…` | `fox mem …`, `fox handoff …`, `fox dispatch`, `fox wait` |
| What a doc cites that changed; minimal context for a reviewer | | `fox drift doc.md`, `fox pack doc.md` |

Skip foxdb when one grep, a file already in context or a path you know answers faster.

**Handles.** Every row starts with a handle and ends in `path:line`: `s:` Swift symbol, `c:`
tree-sitter symbol, `f:` file, `d:` doc section, `m:` milestone, `a:` directory, `g:` memory
note, `x:` session digest, `h:` handoff. Pass a handle to `fox_read` or `fox_graph` instead of
re-searching. `~` marks a result that is not compiler-resolved; treat `callers` as a lower bound.

**Budgets.** `budget` is in tokens (default 500, max 4000). Under a tight budget rows shrink
(full, then skeleton, then ids) before any are dropped. `+N more · cursor=K` or `+N more (answer cut) · cursor=K` means the answer is cut: call again
with `cursor=K`; narrow the query before raising the budget. A repeated `fox_read` returns
`= already sent …`; pass `fresh=true` only if that text has left your context.

**Multi-agent recipes** (details in [docs/multi-agent.md](docs/multi-agent.md) and
[docs/swarm.md](docs/swarm.md)):

- Leave a finding for others: `fox_memory op=post kind=finding text=… evidence=path:line`; read theirs with `op=read since=auto`.
- Hand work over: `fox_memory op=packet text=<goal> next=<steps>` returns `packet h:N`; the next agent runs `op=take id=h:N`, then `fox_read target=h:N`.
- One writer, several readers: `fox dispatch --harness codex --task "…" --writer`, then `fox dispatch … --label scan` for readers (a second `--writer` is refused), then `fox wait <ids> --timeout 1800` (exit 0 done, 3 still running, 4 failed or stale) and `fox mem read --kind report`.
- A whole plan: write a `plan.toml` with tasks, dependencies and an optional review panel, then `fox swarm run plan.toml`.
- Switch harness mid-task: `fox switch --to codex --transcript <session.jsonl>` prints the command that continues in Codex from a packet; `fox switch --for t3` prints a message to paste into a T3 Code thread.
- Reviewers stay zero-context: dispatch with `--reviewer`, or name them to match `skip_agents`.
