# Changelog

## 0.2.3 (unreleased)

First answers that were empty, wrong or padded in agent benchmark transcripts are fixed.

- A Go target qualified by its package resolves: `pod.GetContainerStatus`, the repo path
  `pkg/api/v1/pod.GetContainerStatus` or the full import path. `fox_find` takes the same names.
  A qualifier may also be a dotted Python or JS module (`pkg.fields.json.JSONField`). Every
  segment must match (`pkga.Server.Start` is pkga's, Java `p.A.run` is p/A's), an import path
  prefers the longest directory match (`vendor/github.com/acme/pod` over a lone `pod/`), and a
  Go file stem is never a package.
- `overrides` of a method lists the same-name methods of its subtypes at any depth, and the
  methods it overrides, instead of the types that reference it (`Field.get_db_prep_value` said 0;
  it now lists 16).
- Struct and class fields are symbols of kind `field` in Go, C, C++, Rust, Java, C#, TypeScript,
  JavaScript and Python, so `fox_find ContainerLogMaxWorkers` finds the struct field first.
  Fields are never call or reference targets, so callers of a same-name function stay certain;
  `callers`, `refs` and `impact` of a field say its uses are not tracked and list the files that
  name it. A JS/TS class field holding an arrow function is a method. Files of those grammars
  and PHP parse again once (store schema 20), and when that is most of a project the full-text
  tables are merged at the end, so an upgraded store is no larger than a fresh one.
- When an identifier names a symbol, `fox_find` returns that symbol, names that contain it and
  files or docs that hold it; rows that only share one of its words (docs matching `contains` for
  `IContains`) or only neighbour it in the call graph (the callees of `exec_simple_query`) are
  left out. Doc sections holding a snake_case name (`get_user`) stay. One lookup checks all
  rows at once.
- When the name occurs in files an answer could not resolve, `callers`, `refs` and `impact` no
  longer ask for `rg`: up to 15 such files are listed at the end as `unresolved mentions`
  (`path:line | code`); more are counted.
- Answers carry code. `callers` and `refs` rows end in the call site's line (`  :338 Caller c:1f | return f(x)`),
  or list one `   :line | code` row per site for a caller with several; `impact` rows end in the
  dependent's definition line. The code is shown only when the line still names the target
  (a file changed since indexing shows none), clipped around the name. Code rides along only
  when the whole answer with it fits the budget; otherwise the rows are locations only, as
  before, and the second line names the budget that shows the code (when one up to 4000 does).
- The server instructions, the four tool descriptions, the skill and the brief's tool line lead
  with the question each tool answers ("Where is X?", "Who calls X, and what breaks if X
  changes?"), say to call foxdb first even when a grep looks quicker, and no longer say to skip
  it when one grep answers. The skill is a find, trace, read loop with a completion check. A
  literal string, error message or config value goes to Grep (a kind of content, not a call on
  effort); past decisions go to `fox_memory op=recall`, not `fox_find`. The descriptions say
  that only tree-sitter graph rows carry code and that `~` also marks a Swift file changed since
  the build. Empty-answer hints give the MCP call, not the CLI.
- An empty answer while a project's first index is still running says that foxdb is indexing
  it and to use Grep and Read until it finishes, instead of `no matches`.
- `fox_find` and `fox_graph` carry `_meta: {"anthropic/alwaysLoad": true}`, so Claude Code loads
  them up front instead of deferring them behind tool search. `context-budget.json` now also pins
  the brief's tool line, the bytes every session pays before its first call (instructions, tool
  list and brief line: 4,935) and the list of always-loaded tools.
- `fox_read` with no mode returns a symbol's body, and for a file its declarations with the last
  line of each (`..N`) and how to read one. File outlines count fields instead of listing them;
  a type's own outline lists its fields.

## 0.2.2 (2026-11-18)

`impact` finds much more of what a change breaks and says when it may have missed something; the
session brief names the MCP tools.

- The SessionStart and SubagentStart brief names the `fox_find`, `fox_graph`, `fox_read` and
  `fox_memory` MCP tools on its second line, with the `fox` CLI as the fallback, so an agent with
  no shell can still use foxdb. The line stays when a long brief is cut for its budget.
- `impact` keeps uses at a file's top level (a C command table, a Python module-level call, a
  test registered inside an anonymous callback). The file is listed as `  :line (top)` at the
  depth it first appears, counted in the totals and not followed further.
- A function passed as a value is a reference: C and C++ initializer list elements and call
  arguments, Go identifier arguments, Python arguments and keyword values, and JS/TS arguments.
  A parameter or local of the same name is not, in any declaration form (Python comprehension,
  `with`/`except ... as`, `*args`, walrus and module-level loop variables; JS destructuring, rest
  and `catch` parameters; Go `select` and type-switch variables), nor is an all-caps C constant
  or macro (`elog(ERROR, ...)`). A parameter hides the name only in its own function, lambda or
  comprehension; a name taken from `require(...)` or a module-level rebinding of a definition
  (`main = click.command(main)`) hides nothing. A call of a parameter or local
  (`def load(parse): parse(s)`, a Go closure) calls no definition. A definition inside a function
  is visible only inside that function, where it shadows same-name definitions outside (a nested
  `to_str` no longer claims the rest of its file's `to_str()` calls). A value read through a receiver (`f(request.id)`) resolves to a
  member only through `self`/`this`, an import or the exact type name, in its own file too.
- Python and JS: a bare name never resolves to a method, and a call through another object's
  receiver (`self.connection.close()`) is no longer certain to reach a same-file method. A name
  imported from a module (`import { X } from "./m"`, `from pkg.core import X`, including a
  package that re-exports it, and `from m import x as y`) resolves to that module's definition;
  one imported from a module the project does not hold (`from json import loads as parse`, a JS
  package) resolves to none of the project's. A Python name imported from a project module that
  only re-exports it under another name is left unresolved.
- A call guessed among more than eight same-name definitions through a receiver named unlike the
  target's type (`response.close()` with dozens of `close` methods) is no longer listed as a
  caller or dependent; the header counts it (`, 3 weak guesses left out`) and a `partial` or
  `may be incomplete` line says it may be real.
- Dispatch: a method's `impact` also lists callers of the interface or base methods it
  implements or overrides (` via Router.add`), and an interface method's `impact` lists its
  implementations (` implements Router.add`) and their callers. Supertypes come from TS
  `implements`/`extends` (on any line of the class header), plain JS `extends`, Python base
  classes, Kotlin and other grammars' supertype lists, stored as their own reference kind and
  followed only when resolved with certainty. A Go
  type implements an interface when it has every method of it, embedded interfaces included (an
  interface embedding one outside the project is skipped), and the link has evidence: both in one
  directory, or a file of the type's package that imports the interface's package (by its full
  path: a standard library `log` is not the project's `internal/log`) and names the interface.
  A Go qualifier is the name the file uses for an import (its alias, else its last segment). A caller of the interface method found only by a name guess is listed but not
  followed further, and in Python and JS only counts through `self`/`this` or a receiver named
  like the base type. A capped candidate list adds a `partial at d1:` line.
- Go package-qualified calls (`stats.Register()`) prefer the package named by the full import
  path, and reach only packages of that name (`io.Reader` is not the local `Reader`); method
  calls on values (`req.Header.Get()`), including a local named like a package
  (`user := ...; user.Save()`), get no such preference.
- Finding a function's locals and a call's receiver no longer walks up the tree from every
  name: a deeply nested function (psql's `tab-complete.c`, one function with hundreds of
  `else if`) indexes several times faster than in 0.2.1.
- `impact` adds lines starting `partial at dN:` when the walk stopped (5000 dependents, or past
  depth 4) or when a call named like a dependent was guessed to another same-name definition our
  dependent was a real alternative to (a receiver named like its type, or no nearer the call), in
  a file the answer does not list. The existing "also appears in N other files" check now covers
  every file the answer lists at any depth.
- A paged `fox code` or board answer ends `+N more (answer cut) · cursor=K`. Search footers and
  the MCP tool definitions are unchanged.
- Known gaps: a C++ method call (`s.compute()`) can resolve to a same-name free function, a
  TypeScript default import used under another name is not followed, and a call through a Go
  parameter typed as an embedded interface does not dispatch.
- The store moves to schema 19: on the next index run every code file parses again once, and
  every project re-scores its references.

## 0.2.1 (2026-11-17)

Caller and impact answers no longer look complete when they are not, and more calls resolve.

- C and C++: a free function call resolves by name across the whole project (one global
  namespace), not only near the definition. A header prototype stands for its definition
  instead of competing with it, `static` functions outside headers stay in their own file, and
  among several same-name definitions the one nearest in the tree that is not under a test,
  sample or tools directory is taken (labelled `guess 1 of N`). A function named in a designated
  initializer (`.open = foo_open`) is a reference; a parameter or local of the same name
  (`.size = size` in `f(int size)`) is not.
- Calls through a receiver chain or a property (`theme.current.palette.fill()`,
  `inputs.render(plan)`) resolve by their last name. The receiver's last name breaks a tie
  between candidates already in scope (Go embedded fields such as `pl.Evaluator.Preempt()` reach
  `Evaluator.Preempt`). When it is the only signal (`palette` and an unimported `Palette`), the
  call is a guess for a name of several words and stays unresolved for a one-word name, since
  the receiver's real type is unknown (`state.update()` on a library `MutableStateFlow`). With no
  signal at all, a receiver call whose name has several words is a guess among at most three
  same-name members (`guess by name`, `guess 1 of N`); one-word names (`push`, `get`) and
  standard-library method names (`toString`, `hashCode`, `is_empty`, `to_vec`) stay unresolved.
- Function references are references: Kotlin `::fn` and `Type::fn`, and in Go a function named
  by a plain identifier in a keyed composite literal (`RunE: runCmd`), unless a parameter or local
  of the enclosing function has that name. A package-qualified value (`RunE: cmd.Run`) is not
  captured; the completeness line counts its file.
- A symbol query prefers the name as typed over a same-name symbol in another case
  (`render` the method before `Render` the class), and a definition over its C prototype.
- Completeness signal: `callers` and `refs` (CLI and MCP `fox_graph`) add one line when the
  name occurs in more files than the answer lists, counted from the index only (unresolved or
  guessed references of that name, and every code file holding the name as a whole word, case
  as typed, anywhere in the file: file-scope initializers, macro bodies, comments): `may be
  incomplete: `name` also appears in N other files; confirm with: rg -nw name`. `impact` adds
  the same line as `partial at d1: ...`. Nothing is added when the counts agree. Files that
  define a same-name symbol of their own are not counted.
- Source directories named `target` (Linux `drivers/target`, `drivers/nvme/target`) are indexed.
  A `target` directory is skipped as build output only beside a build manifest (`Cargo.toml`,
  `pom.xml`, `build.sbt`, `build.gradle`, `project.clj`) or when it holds a `CACHEDIR.TAG`.
- The store moves to schema 15: on the next index run, C, C++, Objective-C, Kotlin and Go files
  parse again once (handles of their symbols change), every project re-scores its references,
  and every code file is read once more for its words (a new `fts_word` table).

## 0.2.0 (2026-11-17)

Highlights: an opt-in precise tier from SCIP indexes, an opt-in tantivy backend for Linux-sized
projects, a faster change detector for trees of 50,000+ files, and an opt-in vector arm. With
none of them switched on, answers, speed and store size match 0.1.0 (checked side by side on
small and medium repos). The README now leads with agent-driven install.

- An opt-in vector arm (milestone 8), not in the default build. Built with
  `--features vectors`, switched on with `vectors = true` in config.toml, and fed by
  `fox vectors setup`, which names the model (model2vec `potion-code-16M`, MIT, 65 MB from its
  Hugging Face repository at a pinned commit, sha256-checked) before downloading anything. Static
  embeddings computed in Rust (no ONNX, no GPU, no server) over every tree-sitter symbol's body
  text and every doc heading, kept in one flat int8 file per project under
  `~/.foxdb/vectors/`, refreshed after each index run (only changed files are parsed again;
  a run that changes nothing only moves a small stamp file). Only sentences of five words or
  more read it, never identifiers, tokens or short phrases; it is fused after the other arms. A
  stale, damaged or missing index, a missing model or any error leaves the arm empty, so the
  answer is the one without it. In a store holding only that project, a private Kotlin app's
  vague recall@5 rose from 0.56 to 0.68 (MRR 0.38 to 0.48) and kubernetes gained a little (vague
  MRR 0.26 to 0.28, recall@5 unchanged); exact and lexical sets are unchanged. In one store
  holding several projects, as a real `~/.foxdb` does, the gain is small or none (kubernetes
  vague MRR 0.278 to 0.277, the Kotlin app's vague recall@5 unchanged). `fox vectors build` and `fox vectors status`
  manage it. Without the feature or the setting nothing changes: same answers, no files, no
  work.
- Big projects (milestone 7), opt-in. With `[search] backend = "auto"` a project of about a
  million rows or more (a Linux-sized tree; `[search] tantivy_min_docs`) gets a tantivy index,
  kept by `foxd` from a change log written in the same SQLite transaction as the full-text rows;
  `backend = "tantivy"` or `[search.projects]` gives one to a named project. The default is
  `fts5` (no tantivy anywhere) because tantivy's recall trails FTS5 on the kubernetes eval sets.
  Searches read an index only when it has applied every logged change and use FTS5 otherwise, so
  a save is never missing from an answer; FTS5 stays in full. Crash recovery replays the log; an
  index ahead of SQLite, unreadable, or with a missing or damaged segment file is rebuilt; a
  deleted index directory is refilled; a panic in tantivy falls back to FTS5 (release builds now
  unwind instead of aborting). `foxd` writes with one thread and `writer_mb` MB, fills included.
  `fox index rebuild [--backend tantivy|fts5]` builds or drops one, and `fox status` shows its
  lag and whether it is damaged. On Linux `find` p95 fell from about 450 to about 155 ms. The
  `tantivy` Cargo feature is in the default build (about 2-3 MB per binary);
  `--no-default-features` leaves it out.
- A cheaper change detector for trees of 50,000 files or more: the last walk is kept in
  `~/.foxdb/walk/` and a save stats the recorded directories and files instead of walking the
  tree, re-walking only directories that changed and falling back to the full walk whenever it
  cannot be sure. A one-file save on Linux went from 1.8 to 0.67 s. `.git` counts by its presence
  only, so git commands (which touch it) do not force the full walk.
- Smaller projects (anything below both thresholds, kubernetes included) are unchanged: same
  answers, same store, no new files or background work.
- Precise tier from SCIP (milestone 6): `fox scip import <index.scip> [--project P]` reads an
  index your own build produced (scip-java, rust-analyzer `scip`, scip-python, scip-typescript,
  scip-go) and makes the tree-sitter references it covers precise, `guess 1 of N` labels
  included. Per-file freshness by content: a document is trusted only when the index embedded
  its text and that text is the current file, with fallback to tree-sitter for changed files.
  Documents without text (the default for most indexers) are ignored unless the import is given
  `--trust-mtime`, which is opt-in because a modification time cannot prove what the indexer read;
  a re-import replaces the previous one. `fox status` shows the SCIP tier and its freshness;
  `fox drift` reports a symbol moved to another file by its SCIP id. No new dependency, and
  nothing changes for a project without an import.
- The repository is public. Cloning and install need no GitHub login: `scripts/install.sh`
  downloads the release asset with curl or wget and checks its sha256 (`gh` only as a fallback).

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
