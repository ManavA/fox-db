# Architecture

foxdb is one Rust crate with two binaries: `fox` (CLI, MCP server over stdio, hooks, dispatch supervisor) and `foxd` (an optional per-user daemon). Both read and write one SQLite store, `$FOXDB_HOME/fox.db`.

## Indexing

`fox index` walks a project (honouring `.gitignore` and the per-project tiers), stores files, directories, Markdown sections and milestone entries as rows of `node`, parses changed code files with tree-sitter in parallel (or reads Xcode index stores for Swift) and resolves call references by name with scope awareness. Unchanged files are skipped by size, mtime and content hash. Secrets are never read. A project is registered the first time `fox` or the MCP server runs inside it, keyed by its git common dir, so worktrees share one project.

**C member calls.** A C function-pointer member (`void (*irq_mask)(struct irq_data *)`) is a
method of its struct. So is a member declared through a typedef of a function pointer (`op_fn
run;`, also a typedef of one or a function typedef behind a `*`): its typedef may live in any
file, so each resolve re-reads the stored typedefs for the members of the files it scores (every
member when a typedef changed) and turns them into methods or back into fields; a typedef whose
definitions disagree, or that shares its name with a C++ class, leaves its members fields. Fields, functions and typedefs keep their type in `meta.t`, with struct
tags and typedef names in separate namespaces (`S:inode`, `T:irq_chip_t`). A call through a
member (`inode->i_mapping->a_ops->bmap(...)`) stores the receiver as a type path
(`ts_ref.rtype`, `S:inode|i_mapping|a_ops|bmap`). Its base is the declared type of a parameter,
local or file-level variable (locals are scoped to their block; parameter names of a nested prototype or cast declare nothing), a cast, or a
function's return type. Each field on the way gives the next type, through typedefs; a typedef or field defined twice with different types gives none. Resolution
follows the path to the member, which then is the call's target with certainty. When the
receiver's type is unknown, the call is a guess among at most three same-name function-pointer
members, else unresolved. It never reaches a free function or another struct's member, and a
member is no definition that hides a free function's prototype. A function stored in a member
(`.bmap = ext4_bmap`, `chip->irq_mask = foo_mask;`) (only a function, never a type of the same name) records that member as its `ts_ref.slot`
(negative when the member is a guess). `impact` treats it like interface dispatch: calls through
the member reach the function, and the functions stored in a member are its first-level
dependents. A re-parse of a C file re-scores the references that store a function in its
members, and those whose type path passes through a field or function whose type changed (every
type path when a typedef changed). A run that re-parses most of a big project drops the old rows
in one pass per table and resolves everything again.

**C++ receivers.** A C++ member call keeps its receiver as a type path too, with one namespace
for type names (`S:Engine`): the declared type of a local, parameter or global (a smart
pointer's pointee), the class `auto` gets from `Engine()`, `new`, `make_unique<T>()` or a cast,
and for a name that is no local, or `this`, a field of the class whose method the call is in.
Each step keeps its operator: a smart pointer or `optional` (`W:Pump`) reaches its pointee only
through `->` or `*`, and its own members (`p.reset()`) are no project method, nor are those of a
qualified type in a library namespace (`std::`, `boost::`, ...) when the project has no class of
its last name. A qualifier matches a class by the name it is written with from outside, past `inline`
namespaces (`meta.cq`), looked up from the namespaces around the use outward (a qualified
type path carries them: `S:detail::Engine@a.b`); a match found only at global scope is a guess
when another class's name ends in the same qualifier (a using-directive, an out-of-line method's
class, a class-scope type or a namespace alias may name it); skipping any other namespace is no
match; an unmatched qualifier leaves the name pick, as a guess when classes of that
name exist elsewhere (`std::string` beside `mylib::string`). Template parameters are no classes.
Resolution follows the fields to the receiver's class, which must be the only class (or typedef)
of its name, or for a qualified name the only class whose whole name ends in it; a method that
class itself defines once is the target with certainty (its out-of-line definition over the
in-class declaration). Any unknown step, a class that does not define the method (it may inherit
it), overloads, a `virtual`/`override` method of that name anywhere, or a call `f()` where `f`
is also a method leave the name-based guess. Adding or removing a class re-scores the type paths
naming it and every C++ path with a field step (a field's type may now name another class).

**Python method resolution.** After name scoring, a Python call through `self`, `cls` or
`super()` is re-targeted by the calling class's C3 linearization, built from the stored and
freshly scored base-class references (`impl` rows), memoized per class in one resolve: the first
class in that order (past the caller's own for `super()`) defining the name is the target, with
certainty. A base the index cannot follow (a library class, a guessed link) is an unknown entry:
with one anywhere in the order only a definition in the calling class itself is sure (the unknown
base's ancestors may come earlier). A guessed base link, a hierarchy C3 rejects, a class
attribute of the name that is no method (`step = _other`), or a project subclass whose own order
would reach another definition leave the call a guess. `super(X, self)` counts only when `X` is
the enclosing class. Walks are memoized per class, name and start. An incremental run records the classes of
re-parsed or removed Python files before they go; after scoring, a class that appeared, went or
whose bases resolve differently (an edit, a re-targeted import, a base class added elsewhere)
re-scores, in a second pass, the `self`/`cls`/`super()` calls of every class connected to it
through base links, of the names the changed classes and their old and new ancestors define,
and the calls in the changed classes and their ancestors of names their descendants define
(only those can resolve differently). A call no class of the caller's order defines stays a
guess. A class named by a guessed base link anywhere may have a
subclass that cannot be told, so its calls stay guesses. Same-name candidates tie by path, then
id, so a re-parse that renumbers a file breaks ties as a full run does. `tests/differential.rs` checks incremental runs against full ones over
sequences of edits.

**Change detection on big trees** (`walkcache`). A tree of at least 50,000 files keeps its last
walk in `$FOXDB_HOME/walk/<project id>`: the directories it entered with their modification
times (nanoseconds), the files in each with size and mtime, the notes on directories it left out,
the ignore files it obeyed with their stamps, and the project's file generation (`meta
files:<id>`, moved in the same transaction as every change to the project's file rows). The next
run stats every recorded directory and ignore file in parallel; directories whose time moved
are walked again, with their subtrees, by the same walker restricted to them (so their ignore
rules are the same), and every other recorded file is stat'ed. Anything it cannot vouch for falls
back to the full walk: a missing or unreadable record, a changed root, ignore file, git config or
tier override file, a vanished directory or file, a dirty root, or a directory time within two
seconds of the record (coarse timestamps). When the record's file generation is the store's, the
store's file rows are exactly the record's, so only files whose size or mtime moved are looked up,
and the tree-sitter pass of the same run reads only those files when it is known to have taken
every earlier change (`meta tsfiles:<id>`); otherwise both read everything as before. Trees
below the threshold never write a record, and their runs are unchanged.

## Precise tier (SCIP)

`fox scip import` decodes a SCIP index (only the fields it needs: metadata, documents,
occurrences, roles) and stores, per project, `scip_doc` (path and the file's content hash when
the text the index embedded is the file's current content, empty otherwise; a document without
text is empty too unless the import was given `--trust-mtime`, which trusts a file last modified
before the index file was written), `scip_sym`
(definitions: SCIP symbol, name, container, path, line, and the previous path when a re-import
finds the symbol in another file) and `scip_occ` (references: path, line, name, symbol). The
tables are created by the first import. Resolution (`tsindex::resolve_scoped`, cold, incremental
or after an import) scores references by name as before, then, for a project with an import,
replaces the target of every reference at the same path, line and name as a SCIP occurrence
whose own file and definition file both still match `ts_file.hash`, provided the definition maps
to exactly one tree-sitter symbol at its name line. Those rows carry `ts_ref.amb = -1` (precise);
edges follow from the final targets. A changed file fails the hash check, so its references, and
references into it, fall back to the name resolution on the same save. Projects without an import
pay one `sqlite_master` lookup per resolution run.

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
less for questions, whose answer often sits beside a sibling).

A question in words is then expanded over the call graph (lexically anchored, after LARGER):
its five best symbol hits that are not tests or vendored code, each scoring at least 0.3 of the
best, anchor a one-hop walk over certain references (tree-sitter references that are not
guesses, and compiler-resolved edges), callers and callees alike, eight neighbours per anchor,
most connected first. Each neighbour earns its anchor's score, less a quarter for each place
further down the anchor's list, summed over anchors, but never more than 0.97 of the best
anchor that links it (so the definition a question describes stays above the helpers it
calls), then re-ranked like any hit; a neighbour the walk lifted still follows its anchor
after same-file damping, which can lower the anchor. So "where is
the code that does X" also returns the files the matching code calls into or is called from,
in the same call, even when they share no words with the question. Identifiers are not
expanded: an exact name is already answered precisely, and expanding names with no definition
cost recall in the localization runs. `FOXDB_EXPAND=0` turns it off (for comparisons).

Keyword, substring and prefix
retrieval sit behind the `SearchBackend` trait (with `IndexSink`, `VectorBackend` and `Embedder`
beside it): SQLite FTS5 implements it, and so does a big project's tantivy index (see
[below](#derived-tantivy-index-for-big-projects)); see
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

## Derived tantivy index for big projects

Builds with the `tantivy` feature can keep a [tantivy](https://github.com/quickwit-oss/tantivy)
index beside SQLite. It is opt-in (the default `backend` is `fts5`, because tantivy's recall
trails FTS5's on the kubernetes eval sets): with `backend = "auto"` a project of at least
`tantivy_min_docs` rows gets one (one million by default; a Linux-sized tree has about 1.3
million, kubernetes about 270,000), and `backend = "tantivy"` gives one to any project. SQLite stays the only system of record; the index is derived and can
be thrown away at any time.

1. **One index per project**, at `$FOXDB_HOME/tantivy/<project id>/`. Each full-text row (of
   `fts_node`, `fts_trigram` and `fts_body`) is one document keyed by node id and table, with the
   node id as a fast field. It stores no content: hits go back to SQLite for display. Each arm
   reads its own fields, so no query needs a filter.
2. **One writer, owned by `foxd`.** The CLI and the MCP server only open readers (about a
   millisecond); if `foxd` is not running, a project's index stops moving and its searches use
   FTS5 after its next save.
3. **Outbox in the same SQLite transaction** (`search_outbox`, `outbox.rs`). Every write to a
   full-text row of a project with an index also appends `(seq, project, rowid, table, op,
   text)` in the same transaction, so SQLite's commit is the single point of truth. The tables
   exist only once a project has an index; for any other project every logging call is a no-op.
4. **Apply after SQLite commits.** Once a second `foxd` applies a project's logged rows in `seq`
   order (deletes, then upserts), commits with `prepare_commit().set_payload(last seq)`, and only
   then deletes the applied rows. Readers reload on the next search.
5. **Crash recovery on open.** A payload below the log replays from payload + 1 (the rows are
   still there; rows at or below it are deleted); a payload above SQLite's highest `seq` means
   SQLite lost a committed transaction, and an index that cannot be opened, has another layout,
   or has a segment file missing or failing its checksum (`Index::validate_checksum`, read once
   per open) is deleted: all start a fill, and FTS5 serves meanwhile. A missing or empty
   directory gets a new index, and a fill unless the log still starts with the fill's reset; `fox
   index` also asks for the fill when the directory is gone (`foxd` drains only while
   `$FOXDB_HOME/tantivy` exists).
   Every call into tantivy runs under `catch_unwind` (release builds unwind): a search that
   panics falls back to FTS5 and leaves `<project id>.broken`, which stops searches reading that
   index until `foxd` has opened (and so checked) it again; `fox status` leaves the same mark
   when it finds a damaged file, and a panic in a tantivy thread makes `foxd` re-open every
   index.
6. **Staleness is visible and never served.** A search reads a project's index only when the
   index has applied the project's whole fill and every logged row (`fox status` shows the lag);
   otherwise that project falls back to FTS5. A save is therefore never missing from an answer;
   it is served by FTS5 for the second or so before `foxd` applies it.
7. **FTS5 is kept in full**, trigram included, so the fallback always works.
8. **Writer settings:** one thread and `writer_mb` (64) MB inside `foxd`, fills included,
   `del_docs_ratio_before_merge = 0.3` (each save deletes and re-adds a file's rows), and
   readers on `ReloadPolicy::Manual`.

A **fill** writes every full-text row of the project to the log once: it is how an index is
created and rebuilt. `fox index` starts it when a project crosses the threshold (and `fox index
rebuild` on request): the inventory pass logs a reset and re-stores every file as if changed,
and the tree-sitter pass reads every file once and logs every symbol's rows without rewriting
them in SQLite. Symbols, sessions and notes that no pass re-reads are logged from `node` the way
their importers write them.

Queries mirror the FTS5 ones. Keyword search over names and over body text weights the fields
10, 4, 2, 1 as `bm25(...)` does; a prefix is expanded through the term dictionary (at most 128
terms per field) and each match scores the prefix's own IDF, as FTS5 scores a prefix as one term.
Substring search is a phrase of trigrams over paths and names, which is exactly FTS5's trigram
semantics. Name prefixes stay on SQLite's name index, which is already a range scan. Hits are
checked against SQLite (scope, kind, existence) before they are ranked or cut; the global notes
in a project's scope are searched in FTS5 and interleaved.

## Vector arm (opt-in)

Builds with the `vectors` feature (not in the default build) add a sixth arm for sentences
(`src/vectors.rs`). It runs only when `vectors = true`, the model is in place and the query has
five or more words; the routing above never sends it an identifier, a token or a short phrase.

- **Embeddings:** model2vec `potion-code-16M`, a static table of 256-d token vectors. A text's
  vector is the mean of its WordPiece tokens' rows (times the model's per-token weights),
  L2-normalized. The tokenizer is a small BERT WordPiece implementation that reads the model's
  `tokenizer.json` (it matched the reference tokenizer on 2,305 of 2,306 sampled code and text
  chunks; the one difference is in a non-ASCII text). A query reads only its own tokens' rows
  from the safetensors file.
- **What is embedded:** each tree-sitter symbol as its qualified name, path and the body arm's
  text (comment block above it, then its source, 1,600 characters), and each doc heading and
  milestone as its title and path. Headings are in because symbols alone pushed a heading that
  answers a phrase below the code that agrees with the keyword arm.
- **Store:** one file per project, `$FOXDB_HOME/vectors/p<id>.fv`: a JSON header (model commit,
  dimensions, row count, a fingerprint of what the rows were built from, a checksum of the body),
  then per row the node id, a hash of the embedded text, a hash of the file version it came
  from, a scale and an int8 vector. Search is an exact scan (integer dot products, threads only
  past 65,536 rows); no approximate index is needed at these sizes.
- **Freshness:** a separate `p<id>.stamp` names the project generation the file is current for
  and the file's checksum. The arm reads the index only when the stamp matches the project's
  present generation and that file, and the file's own checksum holds; otherwise it returns
  nothing and the other arms answer alone. After each index run (when on), the refresh reads the
  store in one snapshot: if the fingerprint (tree-sitter file hashes, symbol ids, doc headings)
  is unchanged, it only rewrites the stamp; otherwise it keeps rows whose node id and file
  version are unchanged, parses changed files again, reuses any vector whose text hash it has
  seen, embeds the rest, and writes the file and then the stamp (each atomically).
- **Fusion:** the arm's ranked ids join the RRF with weight 0.5 (keyword 1.0) after the other
  arms; re-ranking and damping are unchanged. Its hits are checked against SQLite like any other.
- **Failure:** any error or panic inside the arm leaves it empty. Nothing in the arm writes to
  SQLite.

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
- **Already sent.** Within one CLI session (`FOXDB_SESSION`) a large `fox code --rel src|outline`
  answer is sent once; later calls get a one-line reference (`--fresh` resends), and the
  PreCompact hook clears the set. The MCP server keeps no such set: it serves a session and all
  its subagents and outlives compactions.
- **Prompt-cache safety.** foxdb adds context only at SessionStart and SubagentStart, and every
  byte it puts in a cached prefix (tool definitions, server instructions, brief caps, the dispatch
  brief, the report schema) is committed in `context-budget.json`, which a test enforces. Briefs
  are deterministic, so siblings share a cached prefix. See
  [usage.md](usage.md#claude-code-setup) for the hook rules.

## Decisions

- [ADR 0001: backend](adr/0001-backend.md)
- [ADR 0002: code graph](adr/0002-code-graph.md)
