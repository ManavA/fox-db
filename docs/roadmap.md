# Roadmap

Milestone 1, measure then stabilize (done): the bench and dry-run A/B runner; cache-stable
output (deterministic briefs, frozen tool definitions, per-harness caps); contentless FTS5;
parallel search arms fused with RRF and code-aware ranking; the `foxd` daemon with an in-memory
graph and result cache; tiered results and already-sent references; the handoff board,
subagent hooks and transcript ingest; `fox drift` and `fox pack`.

Milestone 2, multi-agent (done): the agent registry with process liveness, handoff packets with
lineage and leased takes, `fox switch`, `fox dispatch` with a supervisor and `fox wait`, writer
leases, board topics and since-cursors, all through the existing four MCP tools.

Milestone 3, verifiable handoffs and cache safety (done): base and head commits per worker,
schema-checked reports with rule checks, typed take refusals, a marked brief cut, `readOnlyHint`
and protocol-version negotiation over MCP, no per-answer savings note, and tests for the hook
set, the committed context budget, headroom markers and home paths in the tree.

Milestone 4, scale to very large codebases (done): planner statistics after every index run
(`ANALYZE` on a cold index, `PRAGMA optimize` after updates); change-sized incremental
resolution (a `ts_ref(project_id, name)` index; a save re-scores only the references in the
changed files or named by them); parallel scoring with set-based import matching; per-project
id ranges that confine full-text search to one project's slice of a shared store, with the
project checked per match before ranking, so a busy neighbour does not hide its hits; one CSR
graph per project in `foxd`, rebuilt in the background only when that project's generation
moves (searches walk SQL meanwhile), loaded from the edge table in parallel, with sorted id
arrays instead of a hash map; no file cap on the tree walk; and receiver calls resolved by
name among several same-name definitions labelled `guess 1 of N` in `callers`, `refs`,
`callees` and `impact` (stores indexed before this re-score every reference once, on their
next index run). Open: a single save on a Linux-sized tree stays over 1 s, most of it the
file walk that detects changes; a cheaper change detector would be needed.

Milestone 5, search quality, the backend seam, swarms and T3 Code (done): FTS5 merges moved
out of the write path; snake_case names as whole terms; the body arm (doc comments and bodies)
with routing; the typed `SearchBackend` seam; `fox eval retrieval` with checked-in query sets as
a quality gate (vague recall@5 on the kubernetes set 0.11 to 0.43); `fox swarm`; T3 Code setup
and `--for t3`. See [search-quality.md](search-quality.md), [swarm.md](swarm.md)
and [docs/t3code.md](t3code.md).

Milestone 6, a precise tier from SCIP:
- Import SCIP indexes (scip-java for Java and Kotlin, rust-analyzer `scip`, scip-python,
  scip-typescript) produced by your own builds, and upgrade `~` tree-sitter rows to resolved ones.
  No live language servers.
- Drift checks that follow a moved symbol by its SCIP id instead of its name.

Scale, each only on its trigger (the `SearchBackend` seam stays clean so a tantivy keyword
backend or a vector arm can plug in without touching the arms, fusion or ranking):
- One SQLite file per project plus a catalog, when a store holds more than about 5 projects or
  200k files.
- A tantivy backend, opt-in per project, when a project holds about 1M rows or its `find` p95
  is over 300 ms on a quiet host: SQLite stays the master copy, a change log in the same
  transaction is replayed into tantivy, and FTS5 serves whenever tantivy lags.
- A Postgres team mode, when an index must be shared or written across machines or people.
- Vectors, only if an eval on our own agent tasks shows a gain: usearch with a code embedding
  model in native Rust first (sentence queries only, never bare identifiers), Qdrant later.
- Not planned: graph databases, Apache AGE, Milvus, Weaviate, Vespa, turbopuffer.

Later, each only if the A/B runs show a gap:
- Run the agent A/B at scale and tune the RRF weights from what agents open next.
- Bi-temporal memory (valid from/to, supersedes) anchored to symbols, flagged stale when the code changes.
- Verified handoffs: check a report's `path:line` evidence against the current index when it is read.
- Personalized PageRank over the graph and an Aider-style budgeted repo map.
- Content-hash, worktree-tagged artifacts so new worktrees cost nothing to index.
- A static-embedding arm (model2vec) for docs, memory and natural-language queries.
- Indexed regex over the trigram table with a dirty-file overlay.
- A tantivy keyword backend behind `SearchBackend`, if FTS5 stays the bottleneck.
