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
next index run). A single save on a Linux-sized tree stayed over 1 s, most of it the file walk
that detects changes, until milestone 7's walk record.

Milestone 5, search quality, the backend seam, swarms and T3 Code (done): FTS5 merges moved
out of the write path; snake_case names as whole terms; the body arm (doc comments and bodies)
with routing; the typed `SearchBackend` seam; `fox eval retrieval` with checked-in query sets as
a quality gate (vague recall@5 on the kubernetes set 0.11 to 0.43); `fox swarm`; T3 Code setup
and `--for t3`. See [search-quality.md](search-quality.md), [swarm.md](swarm.md)
and [docs/t3code.md](t3code.md).

Milestone 6, a precise tier from SCIP (done): `fox scip import` reads SCIP indexes your own
builds produce (scip-java, rust-analyzer `scip`, scip-python, scip-typescript, scip-go) with a
small built-in protobuf reader (no new dependency) and upgrades matching tree-sitter references,
and the call edges they carry, to the definitions SCIP names. Freshness is per file by content:
a document counts only when the index embedded its text and that text is the current file, so a
file changed since the index was built keeps its tree-sitter answers until the next import, and a
re-import replaces the previous one. Documents without text (most indexers' default) are opt-in
with `--trust-mtime`, since a modification time cannot prove what the indexer read. `fox status` shows each SCIP tier and how
fresh it is; `fox drift` reports a symbol moved to another file by its SCIP id. A store that
never imports gains no tables and no work. See [usage.md](usage.md#precise-tier-scip).

Milestone 7, big projects (done): an opt-in tantivy index for a project of about a million rows
or more (with `[search] backend = "auto"` past `tantivy_min_docs`, or by `[search.projects]` in
config.toml; off by default because its recall trails FTS5 on the kubernetes eval sets), kept by `foxd` from a change log written in the same SQLite transaction as the
full-text rows, read only when it has applied every logged change and otherwise replaced by
FTS5, which stays in full; and a walk record for trees of 50,000 files or more, so a save stats
the recorded directories and files instead of walking the tree. On Linux, `find` p95 fell from
about 450 to about 155 ms and a one-file save from 1.8 to 0.67 s; below the thresholds nothing
changes. See
[architecture.md](architecture.md#derived-tantivy-index-for-big-projects) and
[benchmarks.md](benchmarks.md#big-projects-milestone-7). Not done: the `find` p95 trigger is not
measured automatically (set `backend = "tantivy"` for such a project); tantivy's ranking does
not yet match FTS5's recall on keyword and vague queries, which is what keeps it opt-in; `fox index check`,
`repair` and `fox bench search` from the design were left out (`fox status` shows the lag,
`fox index rebuild` rebuilds).

Milestone 8, an optional vector arm (done, opt-in and not in the default build): the
`vectors` Cargo feature, `vectors = true` in config.toml, and `fox vectors setup`, which names
model2vec `potion-code-16M` (MIT, 65 MB, Hugging Face, pinned commit, sha256-checked) before it
downloads it. Static embeddings computed in Rust over symbol body text and doc headings, one flat
int8 file per project with an exact scan, refreshed after each index run (changed files only;
otherwise only a stamp moves), read only when current and intact. Sentences of five or more
words only, fused after the other arms with weight 0.5. On the retrieval eval, in a store
holding only the project under test, it lifts vague queries on a private Kotlin app (recall@5
0.56 to 0.68, MRR 0.38 to 0.48) and slightly on kubernetes (MRR 0.26 to 0.28, recall@5
unchanged), with no loss on exact, keyword or lexical sets; in one store holding several
projects, as a real `~/.foxdb` does, that gain is small or none; a warm `find` costs no more, a cold CLI `find` about 50 ms more. See
[architecture.md](architecture.md#vector-arm-opt-in) and
[benchmarks.md](benchmarks.md#vector-arm-milestone-8). Not done: the agent-task eval the design
asked for before vectors ship (fewer turns or more first-try finds on vague tasks), so the arm
stays opt-in; the gain is measured only in single-project stores and mostly goes away in a
shared one, whose arm-off baseline is itself lower (a shared-store ranking issue that predates
milestone 8 and is still open); the kubernetes gain is below the design's 0.08 MRR bar; the cold-CLI cost is mostly
loading the tokenizer's vocabulary; a code model of higher quality (jina-v2-base-code, Qwen3)
would need ONNX or a GPU.

Scale, each only on its trigger (the `SearchBackend` seam stays clean so a vector arm can plug
in without touching the arms, fusion or ranking):
- One SQLite file per project plus a catalog, when a store holds more than about 5 projects or
  200k files.
- A Postgres team mode, when an index must be shared or written across machines or people.
- Vectors beyond milestone 8's static arm, only if an eval on our own agent tasks shows a gain:
  a transformer code model (ONNX) and usearch HNSW past about a million rows; Qdrant later.
- Not planned: graph databases, Apache AGE, Milvus, Weaviate, Vespa, turbopuffer.

Later, each only if the A/B runs show a gap:
- Run the agent A/B at scale and tune the RRF weights from what agents open next.
- Bi-temporal memory (valid from/to, supersedes) anchored to symbols, flagged stale when the code changes.
- Verified handoffs: check a report's `path:line` evidence against the current index when it is read.
- Personalized PageRank over the graph and an Aider-style budgeted repo map.
- Content-hash, worktree-tagged artifacts so new worktrees cost nothing to index.
- Notes, memory and session text in the vector arm (it covers symbols and doc headings).
- Indexed regex over the trigram table with a dirty-file overlay.
