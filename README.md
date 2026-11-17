# foxdb

foxdb is a local code graph and index for coding agents. One SQLite file
(`~/.foxdb/fox.db`) holds every project you work in: files, directories, Markdown doc
sections, milestone logs, Claude Code memory notes, code symbols and their call graph
(Swift from Xcode's index store, 18 more languages through built-in tree-sitter). Agents
reach it through a small CLI (`fox`), an optional daemon (`foxd`) and an MCP server with four
tools, and get short, budgeted answers that end in `path:line` instead of reading whole files.
It also keeps a handoff board, so several agents in Claude Code, Codex, opencode or T3 Code
can share findings, hand work over and run as a supervised swarm. Everything stays on your
machine; nothing is written into your repositories.

## What it can do

- **Search**: ranked search over symbols, files, doc sections, milestones and memory in milliseconds, with the project scope respected in a shared store.
- **Call graph**: callers, callees, references, overrides, conformances and transitive impact of a change, plus outlines and source of one symbol.
- **Docs, milestones and memory**: Markdown sections and milestone logs as searchable rows, Claude Code memory notes, read-only digests of past Claude Code and Codex sessions, and `fox drift` / `fox pack` for docs that cite code.
- **Multi-agent handoff and orchestration**: a shared board, handoff packets, leased takes, writer leases, `fox dispatch` / `fox wait` for Codex and Claude workers, `fox switch` across harnesses, and `fox swarm` for a whole plan with dependencies and a review panel ([docs/swarm.md](docs/swarm.md)). Works under [T3 Code](docs/t3code.md) too.
- **Token sparing**: answers are budgeted rows with handles instead of file dumps, repeated reads come back as one-line references, and every hook output is capped and stable so it stays in the prompt cache.

## Quickstart (5 minutes)

You need Linux or macOS, git, and access to the repository, which is private: the GitHub CLI
(`gh`, logged in) is simplest, or git credentials for github.com. On Linux x86_64 a logged-in
`gh` also fetches the prebuilt binary; otherwise you need a Rust toolchain (`cargo`) to build
from source, which takes several minutes.

1. Get the code and install `fox` and `foxd` into `~/.foxdb/bin` (no root):

```sh
gh repo clone ManavA/foxdb && cd foxdb     # or: git clone https://github.com/ManavA/foxdb.git (needs credentials)
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

**If you are an AI agent setting this up, read [AGENTS-SETUP.md](AGENTS-SETUP.md).**

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

0.1.0 is the first release, used daily by one developer on large Kotlin, Swift and Rust
codebases. It is single-machine and single-user, the call graph outside Swift is
name-resolved (treat `callers` as a lower bound), and some performance targets are still open;
see [docs/benchmarks.md](docs/benchmarks.md) and the limitations in
[docs/usage.md](docs/usage.md#limitations).

## License

AGPL-3.0-or-later, see [LICENSE](LICENSE). Copyright (c) 2026 ManavA.
