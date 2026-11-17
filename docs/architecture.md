# Architecture

foxdb is one Rust crate with two binaries: `fox` (CLI, MCP server over stdio, hooks, dispatch supervisor) and `foxd` (an optional per-user daemon). Both read and write one SQLite store, `$FOXDB_HOME/fox.db`.

## Indexing

`fox index` walks a project (honouring `.gitignore` and the per-project tiers), stores files, directories, Markdown sections and milestone entries as rows of `node`, parses changed code files with tree-sitter in parallel (or reads Xcode index stores for Swift) and resolves call references by name with scope awareness. Unchanged files are skipped by size, mtime and content hash. Secrets are never read. A project is registered the first time `fox` or the MCP server runs inside it, keyed by its git common dir, so worktrees share one project.

## Storage

One SQLite file (WAL) is the system of record. The FTS5 tables are contentless
(`content=''`, `contentless_delete=1`): the text lives in the files and in `node`, not a second
time inside the index, which cut the benchmark store by about 16% (86.2 MB to 72.7 MB). Older stores migrate once on
open, under a file lock, so several processes opening an old store at once wait for one migration
instead of racing it. The call graph is the `edge` and `ts_ref` tables; long-lived processes load it into a CSR
in memory and rebuild it when the index generation moves, and cold CLI calls use SQL.

Why not something bigger:
- **Postgres** (pgvector, ParadeDB, AGE) adds a server, a network hop and operations work for one
  user on one machine, at tens of thousands of symbols. It would only pay off for a team-shared
  index across machines.
- **turbopuffer** is hosted and object-storage backed: its warm p50 is already above foxdb's,
  cold queries take most of a second, and it would send private source off the machine.
- **Meilisearch** (or another search server) is a second store to keep consistent with a second
  process to run, for ranking that BM25 plus code-aware heuristics already gives at this scale.
- Graph databases (Kuzu is archived, Cozo unmaintained) are not needed for a graph that fits in a
  few MB of RAM.
- **Qdrant** (or any vector database) is not used yet because no eval on our own agent tasks has
  shown that embeddings help: lexical, trigram, prefix and graph arms already answer identifier
  queries, and the body arm closed most of the gap on questions. If an eval shows a gain, the
  first step is usearch with a code embedding model in native Rust behind the `VectorBackend`
  seam; Qdrant comes later, only if that outgrows one process.

## Search

`find` routes the query, runs its arms in parallel and fuses their ranked lists with weighted
reciprocal rank fusion (k = 60). A bare identifier (`loadUserName`, `Type.member`) reads names;
a question in words reads body text instead; mixing the two hurt both in the retrieval eval.

1. exact name (and `Type.member`), every query;
2. FTS5 keyword, BM25 with name and signature weighted over path and body. Queries are split
   the code way (`loadUserName` -> `load user name`, plus the whole identifier; snake_case is
   one squashed term too), stopwords and one-letter terms dropped, rare-looking terms first.
   An identifier reads names, qualified names and paths only (at most 8 terms). Words read the
   body arm: each symbol's doc comment and the first 1,600 characters of its source,
   identifier-split, plus doc sections and notes (at most 12 terms, words found in over a tenth
   of the store left out);
3. trigram substring over paths and names, for a single token;
4. name prefix (an index range scan), for an identifier;
5. call-graph neighbours of the exact matches, for an identifier.

The fused list is then re-ranked: the definition of the queried name (or a heading whose title
is the query) x3, a file whose stem is the query x1.5, test paths x0.3, vendored and generated
code x0.3, and each further hit from a file already shown counts half as much again (a twentieth
less for questions, whose answer often sits beside a sibling). Keyword, substring and prefix
retrieval sit behind the `SearchBackend` trait (with `IndexSink`, `VectorBackend` and `Embedder`
beside it), so a tantivy index or a vector arm can plug in later; see
[search-quality.md](search-quality.md) for the seam, the eval and the numbers.

FTS5 never merges segments inside a write (`automerge` and `deletemerge` are off): an index run
ends with one bounded merge step, `foxd` merges while idle, and `fox index optimize` merges
everything now.

In a store shared by several projects, the project scope is checked for every full-text match
before anything is ranked or cut, so a neighbour with many matches cannot push a project's hits
out of the ranked window. A project that holds under half of the index is also read only in its
own slice: every node id belongs to a per-project id range (appended after each index write),
and saves that alternate between projects are bridged into at most 8 slices, so the keyword and
substring arms read only those. There, keyword matches (prefixes included) are weighted by their
frequency inside the project (whole words: name 10, signature 4, path 2, body 1; a prefix-only
match 1), and substring hits rank names before paths, shortest first. The order can differ from
the single-project BM25 order; the hits are the same.

## Daemon (`foxd`)

`foxd` is a long-lived per-user process. It keeps one read connection with its prepared
statements, the call graph in memory as a CSR adjacency list, and a result cache for `find` and
the graph verbs of `fox code` (not `src`, which reads files live), all keyed by the index generation (a counter every index or memory
write bumps), so a re-index is seen on the next query. `fox find` and `fox code` try its unix
socket first (`$FOXDB_HOME/foxd.sock`, mode 0600; in `$XDG_RUNTIME_DIR` if that path is too
long) and fall back to opening the store directly when nothing answers. `FOXDB_NO_DAEMON=1`
forces the direct path; `fox daemon status|stop` checks or stops it. The MCP server is already
long-lived and keeps the same warm state in-process. Warm and cold paths run the same search
arms, so `foxd` returns the same answer as the direct path, only faster. Start `foxd` from your login session or a
user service; foxdb never starts it for you.

## Caching

- **Index generation.** Every index or memory write bumps a counter; `foxd` and the MCP server key
  their result caches and in-memory graphs by it, so a re-index is seen on the next query.
- **Already sent.** Within one session a large `fox_read` answer is sent once; later calls get a
  one-line reference (`fresh=true` resends).
- **Prompt-cache safety.** foxdb adds context only at SessionStart and SubagentStart, and every
  byte it puts in a cached prefix (tool definitions, server instructions, brief caps, the dispatch
  brief, the report schema) is committed in `context-budget.json`, which a test enforces. Briefs
  are deterministic, so siblings share a cached prefix. See
  [usage.md](usage.md#claude-code-setup) for the hook rules.

## Decisions

- [ADR 0001: backend](adr/0001-backend.md)
- [ADR 0002: code graph](adr/0002-code-graph.md)
