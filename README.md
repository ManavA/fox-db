# foxdb

foxdb is a local code graph and index for coding agents. One SQLite db 
holds every project you work in: files, directories, Markdown doc
sections, milestone logs, Claude Code memory notes, code symbols and their call graph
(Swift from Xcode's index store, 18 more languages through built-in tree-sitter). Agents
reach it through a small CLI (`fox`), an optional daemon (`foxd`) and an MCP server with four
tools, and get short, budgeted answers that end in `path:line` instead of reading whole files.
It also keeps a handoff board, so several agents in Claude Code, Codex, opencode or T3 Code
can share findings, hand work over and run as a supervised swarm. Everything stays on your
machine; nothing is written into your repositories.

**AI agents:** to set foxdb up, read [AGENTS-SETUP.md](AGENTS-SETUP.md).

## Install with your agent (recommended)

Paste this into Claude Code, Codex, opencode or any coding agent:

> Set up foxdb from https://github.com/ManavA/foxdb by following its
> [AGENTS-SETUP.md](https://github.com/ManavA/foxdb/blob/main/AGENTS-SETUP.md)
> from top to bottom. Ask me before changing any harness setting.

The agent checks prerequisites, installs `fox` and `foxd` into `~/.foxdb/bin` (no root), indexes
your projects, registers the MCP server and skill for each harness it finds, backs up every
settings file first, verifies the result and tells you how to undo it. No GitHub login is needed.

## Install as a plugin (Claude Code or Codex)

Install `fox` and `foxd` into `~/.foxdb/bin` (no root; a prebuilt binary where the release has
one for your machine, else a source build with cargo), then add the plugin. It brings the four
MCP tools, the session and subagent hooks and the skill; in Claude Code it also puts `fox` on the
agent's PATH. Your settings files are not edited.

```sh
curl -fsSL https://raw.githubusercontent.com/ManavA/foxdb/main/scripts/install.sh | sh
claude plugin marketplace add ManavA/foxdb && claude plugin install foxdb@foxdb   # Claude Code
codex plugin marketplace add ManavA/foxdb && codex plugin add foxdb@foxdb         # Codex
```

Index a project with `~/.foxdb/bin/fox index .` (step 2 below), start a new session there and
ask the agent to "use fox_find to find <a function>". Codex asks you to review the plugin's hooks
(`/hooks`) before they run. Use either the plugin or the manual setup for a harness, not both, or
the tools and hooks run twice.

## What it can do

- **Search**: ranked search over symbols, files, doc sections, milestones and memory in milliseconds; plain questions also search doc comments and function bodies.
- **Call graph**: callers, callees, references, overrides, conformances and transitive impact of a change, plus outlines and source of one symbol. Name-guessed links are labelled `guess 1 of N`.
- **Docs, milestones and memory**: Markdown sections and milestone logs as searchable rows, Claude Code memory notes, read-only digests of past Claude Code and Codex sessions, and `fox drift` / `fox pack` for docs that cite code.
- **Multi-agent handoff and orchestration**: a shared board, handoff packets, leased takes, writer leases, `fox dispatch` / `fox wait` for Codex and Claude workers, `fox switch` across harnesses, and `fox swarm` for a whole plan with dependencies and a review panel ([docs/swarm.md](docs/swarm.md)). Works under [T3 Code](docs/t3code.md) too.
- **Token sparing**: answers are budgeted rows with handles instead of file dumps, repeated reads come back as one-line references, and every hook output is capped and stable so it stays in the prompt cache.
- **Scales up when you need it**: shared stores of many projects, trees the size of Kubernetes and Linux, and three opt-in extras that change nothing until you turn them on (see Scaling).

## Manual install (5 minutes)

You need Linux or macOS and git. Where the release has a prebuilt binary for your machine (from v0.2.4:
Linux and macOS, x86_64 and arm64; v0.2.2: Linux x86_64 only), the installer downloads it with
curl (or wget) and checks its sha256; elsewhere you need a Rust toolchain (`cargo`) to build from
source, which takes several minutes. No GitHub login is needed. Release tarballs from v0.2.4
also carry a GitHub build provenance attestation; to check a downloaded one, run
`gh attestation verify foxdb-<version>-<target>.tar.gz --repo ManavA/foxdb`.

1. Get the code and install `fox` and `foxd` into `~/.foxdb/bin` (no root):

```sh
git clone https://github.com/ManavA/foxdb.git && cd foxdb
```

```sh runnable
scripts/install.sh
export PATH="$HOME/.foxdb/bin:$PATH"   # put this line in your shell profile too
fox --version
```

2. Index a project (any git repository of yours; later refreshes happen in the background):

```sh runnable
cd ~/src/my-app
fox index .
fox status
```

3. Ask it something. Replace `load_config` with a function from your own project:

```sh runnable
fox find "load config"
fox code load_config --rel callers
fox code load_config --rel src
```

4. Hook it into Claude Code (the MCP server for every project, plus the skill that tells agents when to use it):

```sh runnable needs=claude
claude mcp add --scope user foxdb -- "$HOME/.foxdb/bin/fox" mcp
mkdir -p ~/.claude/skills/foxdb && cp ~/.foxdb/skill/SKILL.md ~/.claude/skills/foxdb/
```

5. Hook it into Codex:

```sh runnable needs=codex
codex mcp add foxdb -- "$HOME/.foxdb/bin/fox" mcp
```

Start a new session and ask the agent to "use fox_find to find load_config". The session-start
brief, subagent hooks, opencode, T3 Code and multi-agent settings are in
[AGENTS-SETUP.md](AGENTS-SETUP.md).

## Usage

Agents call the four MCP tools (`fox_find`, `fox_graph`, `fox_read`, `fox_memory`); you and your
scripts use the same features through the `fox` CLI. Every answer is a short list of rows ending
in `path:line`, each with a handle (`s:1a2`, `d:4`, `h:7`) that `fox code` or `fox_read` expands.
Full reference: [docs/usage.md](docs/usage.md).

### Search

```
fox find "retry failed upload"            # plain question: names, paths, doc sections, comments and bodies
fox find UploadWorker                     # bare identifier: exact and prefix name match first
fox find "release checklist" --kind doc   # only doc sections (also: symbol, file, milestone, dir, memory)
fox find config --all --limit 20          # every indexed project, not just the current one
```

Results from several search methods (exact name, keyword, substring, fuzzy name, graph
neighbours, and body text for questions) are fused and ranked; definitions rank above tests and
generated code. `fox eval retrieval` measures search quality on a fixed query set
([docs/search-quality.md](docs/search-quality.md)).

### Call graph and code

```
fox code UploadWorker --rel outline       # members of a type or file
fox code upload --rel callers             # who calls it (also callees, refs, overrides, subtypes, impact)
fox code upload --rel impact --depth 3    # everything that transitively depends on it
fox code s:1a2 --rel src                  # the source of one symbol, budgeted
```

Swift links come from Xcode's index and are exact. Other languages are linked by name; a link
picked among several same-name definitions says `guess 1 of N`. Import a SCIP index to make them
exact (see Scaling).

### Docs, milestones and memory

```
fox find "current gate" --kind milestone  # status rows from milestone logs
fox drift docs/design.md                  # code the doc cites that changed, moved or vanished
fox pack docs/design.md                   # the doc plus the code it cites, for a fresh reviewer
fox ingest sessions --since-days 7        # read-only digests of past Claude Code and Codex sessions
fox stats                                 # tokens saved versus reading files directly
```

### Multi-agent: share, hand off, switch

All agents on the machine share one board. Findings, decisions and reports are posted once and
read by handle, so siblings don't each pay for the same large read.

```
fox mem post "Upload retries ignore 429" --kind finding --evidence src/upload.rs:88
fox mem read --since me                   # only what is new to this agent
fox handoff create --goal "Fix 429 retries" --next "add backoff in upload.rs"   # -> h:12
fox handoff take h:12                     # claimed exactly once, even with several takers
fox switch --to codex --handoff h:12      # prints the command to continue the task in Codex
```

Claude can run Codex workers and the other way round. Each worker is tracked, gets the foxdb
MCP and the parent's packet pointer, and posts its final output as a report:

```
fox dispatch --harness codex --task "Audit upload.rs for retry bugs" --label audit   # -> cx-3
fox dispatch --harness codex --task h:12 --writer                                    # one writer per repo
fox wait cx-3 cx-4 --all --timeout 1800
fox agents --tree                          # who is running, under whom, and their reports
```

Details: [docs/multi-agent.md](docs/multi-agent.md).

### Swarms

`fox swarm run plan.toml` runs a whole plan: tasks with dependencies, a concurrency cap, one
writer per repo, a single summary at the end, and an optional reviewer panel that votes. Running
it again resumes and never starts a task twice.

```toml
name = "audit-handlers"
concurrency = 3

[[task]]
id = "scan"
harness = "codex"
task = "List every HTTP handler under api/ that reads the body twice. Report path:line."

[[task]]
id = "fix"
harness = "codex"
task = "Fix the handlers listed in the scan report."
writer = true
depends_on = ["scan"]

[review]
harnesses = ["claude", "codex", "claude"]
task = "Check every handler in the scan was fixed and nothing else changed."
```

```
fox swarm run audit.toml
fox swarm status audit-handlers
```

Patterns (parallel investigators, one writer, review panels, Claude directing Codex and back):
[docs/swarm.md](docs/swarm.md).

### T3 Code

Register foxdb once for the providers T3 Code runs (Codex, Claude, OpenCode) and every T3 thread
shares the same index and board. T3's fork, handoff and merge-back map onto foxdb packets, and
`--for t3` formats a packet to paste into a thread:

```
fox handoff render h:12 --for t3          # packet capped at 2,000 characters
fox switch --for t3 --handoff h:12        # a one-line message pointing at the packet
```

Setup and caveats (Codex threads under one T3 app-server share one foxdb identity):
[docs/t3code.md](docs/t3code.md).

### Scaling

One store holds every project; search stays inside the current project unless you pass `--all`.
Kubernetes-sized trees index in about a minute and a one-file save re-checks only what changed;
trees of 50,000 files or more skip the full file walk on save. Run `foxd` to keep the call graph
in memory and answer from a warm cache (the CLI falls back to direct reads without it).

Three opt-in extras change nothing until you turn them on:

| Extra | Turn on | What it gives |
|---|---|---|
| Precise tier (SCIP) | `fox scip import index.scip` | exact definitions and call edges from an index your own build produced, replacing name guesses |
| Tantivy backend | `[search] backend = "auto"` in config.toml, then `fox index rebuild` | about 3x faster keyword search on Linux-sized projects (about 1M rows and up) |
| Vector arm | build with `--features vectors`, then `fox vectors setup` and `vectors = true` | meaning-based matches for long, vague questions |

Measured trade-offs and open targets: [docs/benchmarks.md](docs/benchmarks.md),
[docs/architecture.md](docs/architecture.md).

## Docs

- [AGENTS-SETUP.md](AGENTS-SETUP.md): full setup for every harness, verification, uninstall, and an agent cheat-sheet
- [docs/usage.md](docs/usage.md): CLI and MCP reference, configuration, supported languages, hooks
- [docs/multi-agent.md](docs/multi-agent.md): board, packets, dispatch, wait, switching
- [docs/swarm.md](docs/swarm.md): many workers from one plan, with a review panel
- [docs/t3code.md](docs/t3code.md): foxdb under T3 Code
- [docs/architecture.md](docs/architecture.md): storage, search pipeline, daemon, caching, and why not Postgres, a graph database or Qdrant yet
- [docs/search-quality.md](docs/search-quality.md): the search backend seam and the retrieval eval
- [docs/benchmarks.md](docs/benchmarks.md): measured numbers and open targets
- [docs/roadmap.md](docs/roadmap.md) and [docs/adr/](docs/adr/): where it is going and why
- [CHANGELOG.md](CHANGELOG.md)

## Status

0.2.4, used daily by one developer on large Kotlin, Swift and Rust codebases. It is
single-machine and single-user, the call graph outside Swift is name-resolved unless you import
a SCIP index (treat `callers` as a lower bound), and some performance targets are still open;
see [docs/benchmarks.md](docs/benchmarks.md) and the limitations in
[docs/usage.md](docs/usage.md#limitations).

## License

AGPL-3.0-or-later, see [LICENSE](LICENSE). Copyright (c) 2026 ManavA.
