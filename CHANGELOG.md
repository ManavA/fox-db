# Changelog

## Unreleased

- C++ member calls resolve through the receiver's declared or inferred type: a local (`Engine e;`, `Engine e(3);`), a parameter (`Pump* p`, `const Pump& p`), a global, a member of a typed struct (`car.engine.start()`) or of the class the method is in (`engine.start()` in `Car::go`, `this->pump->start()`), `auto e = Engine();`, `auto p = std::make_unique<Pump>();`, `new`, casts and `std::unique_ptr`/`shared_ptr` variables reach that class's method with certainty instead of a guess among every class with a `start`. A smart pointer's or `optional`'s own members (`p.reset()`) and a type in a library namespace with no project class of its name reach no project method; `std::string` beside a project `mylib::string` stays a guess. Anything in doubt stays a guess by name: a receiver whose class cannot be told (`auto x = lookup()`, a template parameter, a call `make()` that may be a method), a class name defined twice or also a typedef, a class that inherits the method instead of defining it, overloads, and virtual or overriding methods. Schema 27; C++ files re-parse once.
- Python calls through `self`, `cls` or `super()` reach the method Python would run, by the calling class's method resolution order (C3): a mixin ahead of a base, the left arm of a diamond, a subclass's own override, and for `super().m()` the next class after the caller's; `callers` and `impact` of an overridden method follow it. Anything in doubt leaves a guess: a base the index cannot follow (a library class, `make()`) unless the calling class defines the method itself, a guessed base link, a class attribute of the name that is no method (`step = _other`), a subclass that would run another definition, `super(Other, self)`, or a name no class of the caller's order defines. Generic bases (`class Repo(Base[T])`) count as bases. A class that appears, goes or whose bases resolve differently (also through a re-targeted import) re-scores the `self.`, `cls.` and `super()` calls of the hierarchy it belongs to. Schema 26; Python files re-parse once.
- C and C++ function-pointer members declared through a typedef (`typedef int (*op_fn)(int); struct ops { op_fn run; };`, also a typedef of a typedef or a function typedef behind a `*`, the typedef in any file) behave like `int (*run)(int)`: calls through them are callers of the member, `.run = impl` stores into it, and impact of `impl` reaches whoever calls `run`. A data member of a typedef type (`myint count;`, `op_fn *table;`) stays a field, and so does a member whose typedef has definitions that disagree. Schema 25; C and C++ files re-parse once.
- `callers` of a C function-pointer member lists the lines storing a function in it (`.irq_mask = foo_mask,`) as their own group after the call sites, `stores a function in it (not calls): N`, no longer among the references that are not calls, and they no longer make the answer read as incomplete. `callers` of a function counts the documentation lines naming it after the verdict (`plus N in docs (not code, not listed)`) and never lists them.
- The SubagentStart brief no longer carries the swarm board protocol line by default: in A/B runs it gave parallel subagents nothing and cost more. `[hooks] swarm_board = true` in `config.toml` brings it back. The parent packet and parent read lines stay.

## 0.2.5 (2026-11-21)

- C and C++ function-pointer struct members are indexed; a call through a member (`a_ops->bmap(...)`) resolves through the receiver's declared type to that member, never to a same-name global, and functions stored in members (`.bmap = ext4_bmap`) are dispatch targets for impact and overrides. On a Linux kernel benchmark, ops-table dispatch recall went from 0 to 1.0. Schema 24; C and C++ files re-parse once.
- The SessionStart brief is fast on very large stores (about 30 ms on the Linux kernel, was about 2 s, which missed its deadline).
- `fox_find` expands a question in words one hop over the call graph, so the files the matching code calls into or is called from rank in the same answer (`FOXDB_EXPAND=0` turns it off).
- The handoff board doubles as a swarm log: fact, failed, summary and question entries with handle attachments, a per-agent `since=me` cursor, and the parent's read files listed as handles in the subagent brief.
- Multi-agent orchestration (`fox dispatch`, `fox wait`) works on macOS.
- Ties between same-score candidates and sorting follow newer clippy; reviewers' fixes for C block scoping, typedef namespaces and incremental re-scoring.

## 0.2.4 (2026-11-19)

The v0.2.3 tag was never released: CI caught that ties between same-score candidates broke differently per filesystem. 0.2.4 is 0.2.3 plus that fix.

- Ties break the same way everywhere: files are indexed in path order. A Python call through a whole-module import (`import core; core.plan()`) resolves to that module's function.

First answers that were empty, wrong or padded in agent benchmark transcripts are fixed.

- `fox_read` also carries `anthropic/alwaysLoad`: with only `fox_find` and `fox_graph` loaded,
  agents read bodies with the native Read instead. The brief's tool line now names
  `fox_read target=<handle>` as the way to read a body.
- `callers`, `refs` and `impact` account for every line that names the target as a whole word
  in the code of its language, so the answer is a labelled superset of `rg -w name` there. The
  first line counts them by group (`16 lines name `f`: 7 calls, 1 definition, 5 imports,
  2 declarations/prototypes, 1 in 1 same-name definition elsewhere, 1 comment; all accounted
  for in indexed Python code`, the code the verdict covers); every occurrence on a line needs its own reference or definition; comments and strings come from the file's tree-sitter parse, and names in strings are
  counted as possible dynamic uses. It reads `all accounted for` only when every line is
  positively classified, the scan was complete and the index is proven fresh (no cited file
  changed, no code file added or edited since indexing that names it, none too big to index,
  nothing pending); otherwise it says what is missing. A complete answer adds nothing else; an
  incomplete one lists at most 20 rows (unresolved lines first, then references that are not
  calls, declarations and imports) and counts the rest. The count is built once per index
  generation on a few threads, capped at 5000 files, 200 MB or 2 s. A name with no indexed
  definition lists its lines instead of answering nothing. On 80 scrapy commits that removed or
  re-signed a function, the share of the lines those commits changed that callers plus refs
  list went from 34% to 37% (only 20 rows are listed; the rest are counted in the first line).
- A call through a module, another type's value or an unrelated receiver no longer resolves
  with certainty to a same-name definition in its own file (`core.plan()` beside a local
  `plan`, Go `s.inner.Plan()` inside `Server.Plan` or `x.plan()` on a local of another type,
  Kotlin `i.plan()`); Swift calls reach Objective-C methods. Every project's references
  re-score once (store schema 22).
- Python `@overload` stubs and property setters/deleters beside their definition are no
  symbols of their own, so the name has one match and calls to it are certain (`must_defer`
  with three stubs said "4 matches" and every call was `guess 1 of 4`); the stub lines count as
  declarations. Python files parse again once (store schema 21).
- `fox_find` no longer pads an identifier's first page with the same attribute repeated in
  every subclass: a field that only contains the name ranks lower, and each further symbol of
  a name already shown counts less (`CrawlerProcess` listed nine `requires_crawler_process`
  fields in its first twenty rows). Long prose queries (an issue's text) keep the identifiers
  in them (`get_proxy`, `RetryMiddleware`, `fetch()`, anything in backticks) among their
  search terms instead of only the longest words.
- A qualifier naming a directory with a dot in it resolves: `yaml.v3.Marshal` and
  `gopkg.in/yaml.v3.Marshal` reach `vendor/gopkg.in/yaml.v3`.
- reStructuredText and AsciiDoc files are split into doc sections like Markdown, so
  `fox_find` answers what the docs say about X in projects documented in `.rst` (scrapy,
  Django); they are read again once. The server instructions and `fox_find` description say
  it answers doc questions.
- `impact` rows at d1 show the line that uses the target (`| :5 raise HttpError(401)`), not
  only the dependent's definition line, and d1 `(top)` rows show their first use line.
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
  name it, its own file included; an answer with no mentions says that does not mean the
  field is unused. A JS/TS class field holding an arrow function is a method, reached with
  certainty only through `this`, its own file or a receiver named like its type (else a guess).
  A qualified target matched equally well by several definitions says so. Files of those grammars
  and PHP parse again once (store schema 20), and when that is most of a project the full-text
  tables are merged at the end, so an upgraded store is no larger than a fresh one.
- When an identifier names a symbol, `fox_find` returns that symbol, names that contain it and
  files or docs that hold it; rows that only share one of its words (docs matching `contains` for
  `IContains`) or only neighbour it in the call graph (the callees of `exec_simple_query`) are
  left out. Doc sections holding a snake_case name (`get_user`) stay. One lookup checks all
  rows at once.
- When the name occurs in lines an answer could not resolve, `callers`, `refs` and `impact` no
  longer ask for `rg`: they are listed at the end as `unresolved mentions` (`path:line | code`).
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
  list and brief line: 5,171) and the list of always-loaded tools.
- `fox_read` with no mode returns a symbol's body, and for a file its declarations with the last
  line of each (`..N`) and how to read one. File outlines count fields instead of listing them;
  a type's own outline lists its fields.
- In a read-only sandbox (`codex sandbox` read-only and workspace profiles) `fox find`, `fox code`,
  `status`, `stats`, `drift`, `pack` and `mem check` no longer fail with "unable to open database
  file". When SQLite cannot open the store's side files, the store's directory is not writable
  and no `-wal` holds unmerged pages, these short CLI reads open the store `immutable=1`. If the
  main file changes during the read (a writer outside the sandbox checkpointed), the command reads
  once more, then says so instead of answering from mixed pages. A store from an older build is
  refused with a message to run fox once outside the sandbox. foxd and the MCP server, which keep
  connections open, never use this mode. A daemon socket the sandbox cannot reach falls back to
  answering directly, as before.
- `fox_read` no longer answers a repeat with `= already sent in this session`. One `fox mcp`
  process serves a Claude Code session and all its subagents (tool calls carry no agent
  identity) and outlives compactions, so the reference pointed subagents and compacted sessions
  at text they never had. The `fresh` argument is gone from the tool list. The CLI keeps its
  per-`FOXDB_SESSION` set for `fox code --rel src|outline`, and the PreCompact hook now clears it.
- The weekly `fox saved ~N tokens` line and `fox stats` compare each answer with a ranged read of
  30 lines around each cited line (never more than the file), not with reading every cited file
  whole, which overstated savings about tenfold.
- `fox hook pre-compact --harness claude` prints four plain lines, which Claude Code hands to the
  compaction summariser as instructions: keep the `path:line` anchors of files being read or
  edited, the symbols being changed, leads already ruled out, and the packet handle `h:…` the
  hook just saved. The text depends only on that handle; Codex gets nothing.
- `fox mem check` (MCP: `fox_memory op=check`) lists memory lines that cite symbols, paths or
  flags that no longer exist, as `path:line | the line | what is missing`, from the project's
  Claude Code auto-memory, `AGENTS.md` / `CLAUDE.md` files and `op=note` notes. Only
  code-shaped tokens that resolve to nothing in the index, the code text or the file system are
  flagged, and each name only when it is anchored to the project on its own (attributed to a
  project path, or qualified by a project symbol); lines recording an absence, planned work or a
  hypothetical are skipped. Memory files are never written. A capped MCP answer counts the rows
  it left out. Each index run records the count (one pass over the code text, about 0.3 s on a
  28 MB project), and while it is above zero the session brief carries one
  `stale memory: N lines … (fox mem check)` line.

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
