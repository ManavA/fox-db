# Changelog

## 0.1.0 (2026-11-16)

First release. Linux x86_64 binary tarball (`fox`, `foxd`, skill, README, AGENTS-SETUP, CHANGELOG,
docs, `config.example.toml` and `scripts/install.sh`) with a
sha256; other platforms build from source.

Index and search
- One SQLite store (WAL, contentless FTS5) for every project: files, directories, Markdown
  sections, milestone logs, Claude Code memory notes and code symbols.
- Swift symbols and callers from Xcode's index store; 18 more languages through built-in
  tree-sitter, with scope-aware name resolution and `guess 1 of N` labels for ambiguous calls.
- `find` runs exact, keyword, body, substring, prefix and graph arms in parallel, fused with
  weighted reciprocal rank fusion and code-aware re-ranking; questions read doc comments and
  bodies, identifiers read names.
- Per-project id ranges keep search scoped inside a store shared by several large projects;
  change-sized incremental resolution keeps saves on large trees in seconds.
- `fox eval retrieval` with checked-in query sets as a quality gate; the `SearchBackend` seam
  for a later tantivy or vector backend.
- `fox drift` and `fox pack` for docs that cite code; read-only digests of past Claude Code and
  Codex sessions.

Agents
- CLI `fox`, daemon `foxd` (warm graph and result cache) and an MCP server with four tools:
  `fox_find`, `fox_graph`, `fox_read`, `fox_memory`.
- Budgeted answers with handles and `path:line`, cursors, "already sent" references.
- Fail-open Claude Code and Codex hooks (session brief, subagent brief, subagent report,
  pre-compact packet), all prompt-cache safe and size-committed in `context-budget.json`.
- Multi-agent: a shared board with topics and cursors, handoff packets with lineage, leased
  takes, writer leases, an agent registry with process liveness, `fox dispatch` and `fox wait`
  for Codex and Claude workers with verifiable reports (base and head commits, schema-checked
  answers, rule checks), `fox switch` across harnesses, `fox swarm` for whole plans with a review
  panel, and T3 Code support.

Release
- `fox --version`.
- `scripts/install.sh` installs into `~/.foxdb/bin` without root, from the release asset when
  `gh` is logged in, else from source.
- Docs split out of the README into `docs/`, plus `AGENTS-SETUP.md` for agents doing the setup
  and `scripts/docs-test.sh`, which runs every runnable block of both in a scratch home.

Known open items: a single save on a Linux-sized tree takes about 1.5-2 s; shared-store search
p95 under 150 ms is open and likely missed, since the milestone 5 body arm made `find` about
1.4x slower (not re-measured on a quiet host). See `docs/benchmarks.md`.
