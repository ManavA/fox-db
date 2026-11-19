# Usage: CLI and MCP reference

Everything `fox`, `foxd` and the MCP server do. Setup for a new machine is in [AGENTS-SETUP.md](../AGENTS-SETUP.md); multi-agent work is in [multi-agent.md](multi-agent.md).

## Supported languages

| Language | Extensions | Symbols + containers | Calls | Imports used in resolution |
|---|---|---|---|---|
| Swift | `.swift` | index store (compiler-resolved); tree-sitter for uncovered files | yes | index store |
| Kotlin | `.kt .kts` | class/interface/object/companion, fun, property, typealias, enum entry | yes | yes (package-aware) |
| Java | `.java` | class, interface, method | yes | yes (package-aware) |
| Python | `.py .pyi` | class, function/method, module constant | yes | yes |
| TypeScript / TSX | `.ts .mts .cts .tsx` | class, interface, function, method, module; JSX components | yes | named imports |
| JavaScript / JSX | `.js .jsx .mjs .cjs` | class, function, method, arrow-function bindings; JSX components | yes | named imports |
| Go | `.go` | struct, interface, type, func, method (receiver as container) | yes | yes (package dir) |
| Rust | `.rs` | struct/enum/union/type, trait, fn, impl methods (impl type as container), mod, macro | yes | no |
| C | `.c .h` | struct, union, enum, typedef, function | yes | unique-name fallback |
| C++ | `.cpp .cc .cxx .hpp .hh .hxx` | class, struct, namespace, function, method, `A::f` | yes | unique-name fallback |
| C# | `.cs` | namespace, class, interface, method | yes | unique-name fallback |
| Ruby | `.rb` | class, module, method | yes | unique-name fallback |
| PHP | `.php` | class, interface, trait, function, method | yes | unique-name fallback |
| Bash | `.sh .bash` | function | yes | unique-name fallback |
| Scala | `.scala .sc` | class, trait, object, enum, def, val | yes | no |
| Dart | `.dart` | class, mixin, extension, function, method | yes | unique-name fallback |
| Lua | `.lua` | function, method | yes | unique-name fallback |
| Objective-C | `.m .mm` | `@interface`/`@implementation`, protocol, method, C function | yes | unique-name fallback |
| SQL, HTML, CSS, JSON, YAML, TOML | | file level only | no | |
| Markdown | `.md` | sections and milestone entries | no | |

Each language is a row in `src/lang.rs` plus a tag query in `languages/<lang>.scm` (tree-sitter
`tags.scm` captures), so adding one is data, not code. Grammars are the crates.io
`tree-sitter-*` crates on tree-sitter 0.27, all MIT-licensed. Files are parsed in parallel and
only when their size/mtime moved and their content hash changed.

Resolution is by name with scope awareness: same file, then the caller's own type, then an
imported name (JVM imports must match the candidate's package directory), then the same
directory. `x.f()` does not resolve to a free function in another file, and `f()` does not
resolve to another type's member. Names with no scope signal stay unresolved, except in
languages where imports are not modelled, where a unique project-wide name is accepted.

Index a project once (later refreshes happen in the background, or with `fox watch`):

```sh
cd ~/src/my-app && ~/.foxdb/bin/fox index .
```

## Config

| Setting | Default | Purpose |
|---|---|---|
| `FOXDB_HOME` | `~/.foxdb` | Data dir: `fox.db`, `bin/`, `mirrors/`, `projects/`, `config.toml` |
| `FOXDB_CLAUDE_PROJECTS` | `~/.claude/projects` | Root of the global memory corpus (`*/memory/*.md`) |
| `FOXDB_SWIFT_STORES` | auto | Colon-separated Xcode index-store dirs to read instead of auto-discovery |
| `FOXDB_INDEXSTORE_LIB` | auto | Path to `libIndexStore` (needed on Linux for Swift) |
| `FOXDB_BRIEF_DEADLINE_MS` | `1500` | Time limit for `fox mem brief` |
| `FOXDB_TRACE` | unset | Print timing traces to stderr |
| `FOXDB_NO_DAEMON` | unset | Never use a running `foxd` |
| `FOXDB_SESSION` | unset | Session id for CLI "already sent" references |
| `FOXDB_CODEX_SESSIONS` | `~/.codex/sessions` | Codex rollouts for `fox ingest sessions` |
| `[hooks] skip_agents` (config.toml) | `["egm-*", "*goldfish*", "*reviewer*"]` | Agents that get no SubagentStart brief |
| `[search] backend` (config.toml) | `fts5` | `fts5`: never use tantivy; `auto` (opt-in): a project with at least `tantivy_min_docs` rows gets a tantivy index, smaller ones keep FTS5 only; `tantivy`: every project. `[search.projects]` overrides it per project name. See [Big projects](#big-projects-tantivy-and-the-walk-record) |
| `[search] tantivy_min_docs` (config.toml) | `1000000` | Rows a project needs before `auto` gives it a tantivy index |
| `[search] writer_mb` (config.toml) | `64` | Indexing memory of `foxd`'s tantivy writer (one thread) |
| `FOXDB_SEARCH_BACKEND` | unset | Overrides the global `[search] backend` |
| `vectors` (config.toml) | `false` | The opt-in vector arm (builds with `--features vectors`). See [Vector arm](#vector-arm-opt-in) |
| `FOXDB_VECTORS` | unset | `1` or `0` overrides `vectors` |
| `FOXDB_VECTORS_MODEL` | unset | A directory holding a model2vec model (`tokenizer.json`, `model.safetensors`) to use instead of the one `fox vectors setup` fetches |
| `codanna` (config.toml) | `false` | Use an external [codanna](https://github.com/bartolli/codanna) binary at `~/.foxdb/bin/codanna` instead of the built-in tree-sitter index |

`~/.foxdb/config.toml` (see [`config.example.toml`](../config.example.toml)) maps build
modules to platform tags shown on symbol rows:

```toml
[module_tags]
MyAppTV = "tvOS"
MyAppMobile = "iOS"
```

### codanna fallback

The built-in indexer replaces codanna. To use codanna anyway, put a codanna 0.17.x binary at
`~/.foxdb/bin/codanna` and set `codanna = true` at the top of `config.toml`. `fox index`
then syncs a mirror under `~/.foxdb/mirrors/` and `fox code` answers from codanna, in the
older codanna row format (`~c:` caller definitions, not call sites).

Per-project tier overrides go in `~/.foxdb/projects/<name>.toml` (globs that force a path
to be skipped, name-only, text or code). Projects are not listed anywhere: a project is
registered the first time `fox` or the MCP server runs inside it (keyed by its git
common dir, so worktrees share one project).

## CLI

```
fox --version
fox find <query> [--kind any|symbol|file|doc|milestone|dir|memory] [--all] [--limit N] [--budget TOKENS]
fox code <symbol|handle|path> [--rel callers|callees|refs|conforms|subtypes|extensions|overrides|outline|src|impact] [--depth N] [--budget TOKENS] [--cursor N] [--fallback] [--fresh]
fox watch [PATH]
fox mcp
fox status
fox stats
fox index [PATH] [--quiet] [--no-global]
fox index optimize
fox index rebuild [PATH] [--backend tantivy|fts5]
fox eval retrieval --set bench/eval/SET.json [--repo DIR] [--limit K] [--json] [--gate BASELINE.json]
fox mem brief [--budget TOKENS] [--harness claude|codex]
fox mem post <text> [--kind finding|claim|request|decision|report] [--title T] [--evidence E]
fox mem read [query|h:ID] [--kind K]
fox mem pickup h:ID [--by NAME]
fox hook session-start|subagent-start|subagent-stop|pre-compact [--harness claude|codex]
fox drift [doc.md]
fox pack <doc.md> [--budget TOKENS]
fox scip import <index.scip> [--project P] [--trust-mtime]
fox vectors setup [--yes] | build [--project P] | status
fox ingest sessions [--since-days N] [--claude DIR] [--codex DIR]
fox daemon status|stop
foxd [--version]
fox-bench run --set FILE --repo DIR [--daemon] [--json OUT]
```

Examples, on the foxdb repository itself (Rust, about 100 code files):

```
$ fox index .
foxdb: 166 files seen, 166 changed, 0 removed, 123 sections (0 milestone), 0 summary dirs in 71 ms; global memory notes: 0; tree-sitter: 99 files, 99 parsed, 0 unchanged, 0 removed, 1437 symbols, 16278 refs (16257 scored, 4642 resolved), 1027 ms

$ fox code name_of --rel callers --budget 120
callers of ~c:65e function name_of  src/tiers.rs:143
3 call sites in 1 files
src/
 tiers.rs (3)
  :193 is_build_output c:668 | if name_of(rel) != "target" {
  :227 classify_dir c:66b | let name = name_of(rel);
  :250 classify_file c:66c | let name = name_of(rel);
~ tree-sitter, name-resolved

$ fox code src/tiers.rs --rel outline --budget 70     # a path outlines the whole file
~outline src/tiers.rs (19 symbols, 4 fields not listed: outline a type for them; ..N = last line; fox_read target=<handle> or lines=a-b reads one)
11 enum Tier c:65c ..20
89 function is_secret_name c:65d ..141
143 function name_of c:65e ..145
150 struct Overrides c:65f ..153
+15 more (answer cut) · cursor=5

$ fox code c:345 --rel callees
callees of ~c:345 function classify_file  src/tiers.rs:212
3 callees (project symbols)
src/
 tiers.rs (2)
  :90 function is_secret_name c:33d @214
  :144 function name_of c:33e @213
 tscode.rs (1)
  :41 function map c:330 @220
~ tree-sitter, name-resolved

$ fox code is_secret_name --rel impact   # transitive dependents, grouped by depth and file
impact of ~c:65d function is_secret_name  src/tiers.rs:89: 2 symbols; per depth 2/0
partial at d1: `is_secret_name` is also named in 3 other files, not resolved to it; unresolved mentions at the end
d1
 src/tiers.rs
  :249 classify_file c:66c | pub fn classify_file(rel: &str, size: u64, ov: &Overrides) -> Tier {
  :276 is_denied_path c:66d | pub fn is_denied_path(rel: &str) -> bool {
d2
unresolved mentions:
  src/inventory.rs:141 | if tiers::is_secret_name(&name) {
  src/inventory.rs:197 | return !tiers::is_secret_name(&e.file_name().to_string_lossy());
  src/watch.rs:68 | || tiers::is_secret_name(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
  tests/inventory.rs:105 | assert!(!foxdb::tiers::is_secret_name(name), "opened secret {rel}");
~ tree-sitter, name-resolved

$ fox code classify_file --rel src       # source body with line numbers

$ fox stats
tokens saved (est., vs reading cited files whole)
today: ~35k in 10 answers
7 days: ~35k in 10 answers
all time: ~35k in 10 answers
top: code callers ~15k/5, code outline ~11k/3, code callees ~6397/1, code impact ~2159/1
```

- `index` walks the project (honouring `.gitignore`), re-parses changed code files with tree-sitter, refreshes Swift index stores (and the codanna mirror if enabled), and also indexes global memory unless `--no-global`.
- `stats` reports estimated tokens saved: each answer is compared with reading every file it cites, whole (about 4 characters per token). A coarse weekly line ends the session brief; tool answers never carry a note, so identical answers stay byte-identical.
- `watch` keeps a project fresh with inotify/FSEvents (skip-listed dirs such as `node_modules` are not watched).
- `mem brief` is fail-open: it never prints errors and always exits 0, so it is safe in hooks.
- `code --rel impact` with no target uses the working-tree git diff; in 0.1.0 this covers changed Swift files only (other languages print `no changed Swift files vs HEAD`; name a symbol instead).

## MCP tools

`fox mcp` speaks MCP over stdio.

| Tool | Arguments | Does |
|---|---|---|
| `fox_find` | `q`, `kind?` (any/symbol/file/dir/doc/milestone/memory), `project?` (name or `all`), `budget?`, `cursor?` | Ranked search |
| `fox_graph` | `target?`, `rel` (callers/callees/refs/conforms/subtypes/extensions/overrides/impact), `depth?`, `budget?`, `cursor?` | Code graph of a symbol |
| `fox_read` | `target`, `mode?` (outline/signature/body/section/context), `lines?` (`a-b`), `fresh?`, `budget?`, `cursor?` | Read a handle or path compactly; `h:` handoffs and `x:` session digests too |
| `fox_memory` | `op` (brief/recall/note/post/read/pickup/take/packet/agents/dispatch/wait), `q?`, `text?`, `kind?`, `topic?`, `since?`, `evidence?`, `next?`, `id?`, `harness?`, `writer?`, `reviewer?`, `any?`, `timeout?`, `budget?` | Session brief, recall, notes, the handoff board, packets, the agent tree, dispatch and bounded wait (see [multi-agent.md](multi-agent.md)) |

Each description says when to use the tool, and `initialize` returns a short server
instructions string (six short sentences: one question and tool per line, "call them first, even when a grep looks quicker", and "for a literal string, error message or config value, use Grep"). The definitions plus instructions are about 1,100 tokens,
and a test hashes them: they sit in every request's cached prefix, so they change only on purpose.
Milestone 2 grew them by about 190 tokens (the multi-agent `fox_memory` ops and arguments) and
milestone 3 by about 65 (the `readOnlyHint` annotations and the `any` and `reviewer` arguments);
every session pays that once, in its cached prefix.

- `fox_find`, `fox_graph` and `fox_read` carry `readOnlyHint: true`, so Claude Code may run them in
  parallel; `fox_memory` writes and carries none.
- `fox_find`, `fox_graph` and `fox_read` carry `_meta: {"anthropic/alwaysLoad": true}` in `tools/list`: Claude
  Code defers MCP tools behind tool search by default, and these three load up front. A user's
  server-level `alwaysLoad: false` overrides it. `context-budget.json` pins which tools carry it
  and the bytes every session pays before its first call (instructions, tool list and the
  brief's tool line); a test fails when either changes, so growth is deliberate.
- No tool has an `outputSchema` and no result carries `structuredContent`: Codex shows a result's
  structured JSON instead of its text, which would replace foxdb's budgeted rows.
- `initialize` answers with the client's `protocolVersion` when foxdb implements it (2025-11-25,
  2025-06-18, 2025-03-26, 2024-11-05) and with 2025-11-25 otherwise, never with an echo of a
  version it does not implement.

Example exchange:

```
→ {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"fox_find","arguments":{"q":"config","budget":150}}}
← {"id":2,"jsonrpc":"2.0","result":{"content":[{"text":"~c:1af struct Config  src/config.rs:9  refs:0\n~c:366 module config  src/lib.rs:7  refs:0\nd:4d § Config  docs/usage.md:47\n~c:522 function config  tests/retrieval.rs:70  refs:0\nf:ac config.example.toml:1  [text 572B]\n+27 more · cursor=5\n","type":"text"}],"isError":false}}
```

`fox_memory {"op":"note","text":"Decided to keep the WAL store"}` replies `saved g:… (29 chars)`;
`{"op":"recall","q":"WAL"}` finds it again with a citation.

## Conventions: handles, budgets, cursors

- Every row starts with a handle: `s:` symbol (index store), `c:` tree-sitter symbol, `f:` file, `d:` doc section, `m:` milestone, `a:` summarized dir, `g:` memory, `x:` session, `h:` handoff. Every row ends in an exact `path:line`. Pass a handle (or a `Type.member` name) to `fox_graph` / `fox_read` / `fox code`.
- `budget` is in tokens (default 500, max 4000). Search answers shrink gracefully: a page that fits as full rows is sent whole; otherwise the top five rows are sent in the richest form they fit (full, else skeleton: handle, kind, name, `path:line`, else ids: handle, `path:line`), with a `cursor` for the rest. A shorter form says so in the footer, and `fox_read <handle>` expands a row. A tight budget never trades the best rows' detail for a long tail of ids.
- A large `fox_read` answer this session already received comes back as one line, `= already sent in this session: <handle> ...`; `fresh=true` resends it. The CLI does the same for `fox code --rel src|outline` when `FOXDB_SESSION` is set (`--fresh` resends); through `foxd` the client sends its own session with each request. This covers foxdb's own answers only. Rows are terse: relative paths, each directory and file printed once as a header.
- Graph answers carry code. A `callers` or `refs` row with one site ends in ` | ` and that line's code; a caller with several sites is followed by one `   :line | code` row per site. An `impact` row ends in the dependent's definition line. Code is shown only when the line still names the target (a file changed since indexing shows none), clipped to 140 characters around the name. Code rides along only when the whole answer with it fits the budget; otherwise the rows are locations only and the second line says `code left out to fit the budget; budget=N shows it` (or, past 4000 tokens, that the answer is too long to carry it). Answers with more than 400 sites or dependents carry no code.
- `fox_read` with no `mode`: a path returns its declarations (`line kind name handle ..last line`, a header saying how to read one; fields are counted, and `mode=outline` on a type lists them), a symbol name or handle returns its body.
- A footer `+N more · cursor=K` or `+N more (answer cut) · cursor=K` means the answer is incomplete as shown: call again with `cursor=K` for the next page. Narrow the query before raising the budget.
- `~` on a row means not compiler-resolved: a tree-sitter result, or a Swift symbol whose file changed since the last build (lines may have drifted; `src` relocates by name).

## Claude Code setup

MCP server (user scope):

```sh
claude mcp add --scope user foxdb -- "$HOME/.foxdb/bin/fox" mcp
```

Hooks. foxdb does not install these; add them to `~/.claude/settings.json` yourself
(`scripts/install_hook.py` adds the SessionStart one). Every hook is a command hook, fails open
(prints nothing and exits 0 on any error or after 1.5 s) and uses the `hookSpecificOutput` JSON form.

```json
{"hooks": {
  "SessionStart": [{"hooks": [{"type": "command", "timeout": 2,
    "command": "H=\"$HOME/.foxdb/bin/fox\"; [ -x \"$H\" ] && \"$H\" hook session-start --harness claude; exit 0"}]}],
  "SubagentStart": [{"hooks": [{"type": "command", "timeout": 2,
    "command": "H=\"$HOME/.foxdb/bin/fox\"; [ -x \"$H\" ] && \"$H\" hook subagent-start --harness claude; exit 0"}]}],
  "SubagentStop": [{"hooks": [{"type": "command", "timeout": 2,
    "command": "H=\"$HOME/.foxdb/bin/fox\"; [ -x \"$H\" ] && \"$H\" hook subagent-stop --harness claude; exit 0"}]}],
  "PreCompact": [{"hooks": [{"type": "command", "timeout": 2,
    "command": "H=\"$HOME/.foxdb/bin/fox\"; [ -x \"$H\" ] && \"$H\" hook pre-compact --harness claude; exit 0"}]}]
}}
```

- `session-start`: the project brief, capped at 9,500 characters (Claude spills
  `additionalContext` over 10,000 to a file). The older `fox mem brief --budget 400` form still works.
- `subagent-start`: a ~1,500-character brief for the helper (current gate, open milestones) and a
  pointer to the parent session's board snapshot (`parent packet h:…`), not the board itself. The
  snapshot is taken once per parent session, so siblings started before and after a board post
  still get byte-identical text and share a cached prefix. The helper is registered as an agent.
- `subagent-stop`: saves the helper's final message as a `report` handoff row (redacted), marks
  the agent done and prints nothing.
- `pre-compact`: saves a handoff packet from the transcript and prints nothing; the
  `session-start` that follows a compaction (`source: compact`) adds one `pre-compact packet h:…` line.
- A brief cut at its budget or cap ends with `+N lines (brief cut short; fox_memory op=brief budget=2000)`; that call returns up to 2,000 tokens, so it gives back the cut lines. The hook budgets (400 tokens at SessionStart, 250 at SubagentStart) are in `context-budget.json`.
- Every session-start also registers the session in the agent registry (only when a store exists).

Prompt-cache safety. foxdb adds context only at SessionStart and SubagentStart, never from
PreToolUse, PostToolUse or UserPromptSubmit: Claude Code rewrites the cached history when a tool
hook returns context (anthropics/claude-code#83913), and per-prompt context grows every turn. A
test runs every hook event and fails if any other one injects. `context-budget.json` commits the
size of everything foxdb puts in a cached prefix (brief caps, `tools/list` bytes, server
instructions, the dispatch brief, the report schema); a test fails if any of them changes, so
growth is a reviewed edit to that file.

Reviewers stay zero-context. Fresh-eyes review agents must not get a brief. `subagent-start` skips any agent whose name or type
matches `[hooks] skip_agents` in `config.toml` (default `["egm-*", "*goldfish*", "*reviewer*"]`;
setting it replaces the default) and any agent whose definition file
(`.claude/agents/<type>.md`, `~/.claude/agents/<type>.md`, or the `.codex` equivalents) has a
`foxdb: off` line.

Skill: copy `skill/` to `~/.claude/skills/foxdb/` so agents know when to reach for the tools.

## Codex / AGENTS.md setup

Codex `~/.codex/config.toml`:

```toml
[mcp_servers.foxdb]
command = "/path/to/home/.foxdb/bin/fox"
args = ["mcp"]
```

Codex hooks take the same commands with `--harness codex`, which caps hook output at about
2,200 tokens (Codex spills over 2,500). Keep the command strings stable: Codex asks to trust a
hook again when its command changes.

Or, for any agent that only reads `AGENTS.md`, add:

```md
## Code navigation
To find or trace code, run `~/.foxdb/bin/fox` first, even when a grep looks quicker: `fox find <q>`, `fox code <sym> --rel callers|outline|src|impact`,
`fox mem brief`, `fox mem read` / `fox mem post <finding>`. Rows start with handles and end in path:line;
`~` rows are tree-sitter, not compiler-resolved.
```

## Docs anchored to code

- `fox drift [doc.md]` lists the backticked symbols and paths a Markdown doc cites that changed,
  moved or vanished since the doc's last commit, grouped by section. It uses only git and the
  index (no LLM), so the same inputs give the same bytes. With no doc it summarizes every doc
  that has drift.
- `fox pack <doc.md> [--budget N]` emits the doc plus the signature skeletons of every symbol
  it cites (and the top-level symbols of every file it cites), within the budget: the minimal,
  reproducible context a fresh reviewer gets. Citations that no longer resolve are listed as
  `not found:`.

## Precise tier (SCIP)

`fox scip import <index.scip> [--project P]` reads a [SCIP](https://github.com/sourcegraph/scip)
index that your own build produced and makes the tree-sitter answers it covers precise: a
reference at the same file, line and name as a SCIP occurrence now points at the definition SCIP
names (a `guess 1 of N` label disappears, a call guessed to the wrong same-name method moves to
the right one), and `callers`, `callees`, `refs` and `impact` follow. foxdb never runs your
build or a language server; it only reads the file. Without an import nothing changes.

Produce the index with the indexer for your language, from the project root (foxdb does not run
these):

```
rust-analyzer scip .                                      # Rust: writes index.scip
scip-java index                                           # Java, Kotlin, Scala (Gradle, Maven, sbt)
scip-python index . --project-name my-app                 # Python
scip-typescript index                                     # TypeScript / JavaScript (tsconfig.json)
scip-go                                                   # Go
fox scip import index.scip
```

Freshness is per file and checked by content. A document counts only when the index embedded
its text and that text is the file as it is now; a file edited later keeps its tree-sitter
answers, and so do references into it, until you rebuild the index and import again.

Most indexers (rust-analyzer, scip-java, scip-python, scip-typescript, scip-go) leave the text
out by default, and then nothing proves which version of a file the indexer read: an index
written after an edit it did not see, or copied or touched later, would point references at the
wrong definition and call them precise. So documents without text are ignored by default (the
import reports them as `without text`) and keep their tree-sitter answers. `--trust-mtime` opts
in to trusting them when the file was last modified before the index file was written; use it
only for an index you built yourself and did not edit files during, since that is the case it
cannot tell apart from a wrong one. A re-import replaces the previous one. `fox status` shows the tool, the
import's age and how many of its files are still fresh. `fox drift` follows a cited symbol by its
SCIP id: a symbol moved to another file reads as `moved ... from <old file> (SCIP <id>)`
rather than as an edit. Symbols SCIP defines outside the project (dependencies, the standard
library) leave the tree-sitter answer alone.

## Big projects: tantivy and the walk record

Two things change only for very big projects; below their thresholds foxdb behaves exactly as
before (same answers, same store, no extra files or background work).

**A tantivy index** (opt-in; builds with the `tantivy` Cargo feature, which release builds
include). It is off unless config.toml asks for it, because its ranking trails FTS5's on some
query sets (on the kubernetes eval sets recall@5 dropped from 1.00 to 0.80 on lexical and from
0.67 to 0.31 on retrieval queries; see [benchmarks](benchmarks.md#big-projects-milestone-7)).
With `backend = "auto"`, a project with at least a million rows (a Linux-sized tree; kubernetes
has about a quarter of that) gets one after its next `fox index`, which then writes every full-text row once to a change log
in the store. `foxd` builds the index from that log and from then on applies each save within
about a second. Searches of that project read the index whenever it has applied every logged
change, and read SQLite FTS5 otherwise, so a save is never missing from an answer: without a
running `foxd` a project simply keeps using FTS5 after its next save. SQLite stays the system of
record and FTS5 is kept in full; the index lives in `~/.foxdb/tantivy/<project id>/` and can be
deleted at any time: FTS5 serves meanwhile, and the next `fox index` fills it again. A missing,
truncated or damaged segment file is found by its checksum when `foxd` opens the index or `fox
status` looks at it (searches then use FTS5 until `foxd` has rebuilt it), and a panic inside
tantivy makes that search use FTS5. Settings are in [Config](#config); for example:

```toml
[search]
backend = "auto"            # or "fts5" (the default: never) or "tantivy" (every project)
tantivy_min_docs = 1000000

[search.projects]
linux = "tantivy"           # by project name, overrides backend
```

`fox index rebuild [PATH]` builds (or rebuilds) the index of the project at PATH now;
`fox index rebuild [PATH] --backend fts5` drops it (`foxd` deletes the files; with
`backend = "auto"` set the project to `"fts5"` in `[search.projects]` so it stays off).
`fox status` shows each index: its state, how far it lags the log, segments and documents, and
whether queries read it (or that it is damaged). The first fill of a Linux-sized project takes a
few minutes in `fox index` and more in `foxd`, which writes with one thread and `writer_mb` MB
(64 by default; before this setting it used four threads and peaked at about 1.6 GB), and and leaves free pages in `fox.db` about the size of the project's text,
which SQLite reuses (run `VACUUM` to return them).

**A walk record.** Finding what changed used to walk the whole tree on every save, which on a
Linux-sized tree (about 100k files) took most of the save. A tree of at least 50,000 files now
keeps its last walk in `~/.foxdb/walk/<project id>`, and the next run stats the recorded
directories and files instead, walking again only directories whose time moved (with the same
ignore rules). It falls back to the full walk whenever it cannot be sure: a changed `.gitignore`,
`.ignore`, git exclude or global git config, a changed tier override file, a vanished directory
or file, or directory times within two seconds of the record.

## Vector arm (opt-in)

A question in words such as "where do artwork downloads get cancelled when the user scrolls
away" often shares few words with the code that answers it. The vector arm also looks for
symbols and doc headings whose meaning is close. It is off unless you ask for it three times:

```sh
cargo build --release --features vectors   # 1. a build that has it (release tarballs do not)
fox vectors setup                           # 2. names the model, its size and source, then asks
fox vectors build                           # 3. embed the current project
```

Then switch it on with `vectors = true` near the top of `~/.foxdb/config.toml` (before any
`[section]`), or `FOXDB_VECTORS=1` for one command.

`fox vectors setup` fetches model2vec `potion-code-16M` (MIT, 65 MB) from its Hugging Face
repository at a pinned commit, checks each file's sha256 and puts it in
`~/.foxdb/models/potion-code-16M/`; it downloads nothing without `--yes` or a yes at the prompt,
and nothing else in foxdb uses the network. The model is a table of token vectors (no neural
network runs), so embedding a project is quick on a CPU: about 1 s for a 1,500-file Kotlin app
and about 9 s for kubernetes.

Once on, every `fox index` and `fox watch` run keeps the project's vectors current
(`~/.foxdb/vectors/p<id>.fv`, about 280 bytes per symbol): only files whose content changed are
parsed again, a text seen before is not embedded again, and a run that changed nothing it
covers only rewrites a small stamp file. Only queries of five words or more use it; identifiers,
single tokens and short phrases never do. If the index is behind the store (a save made with the
arm off), damaged, from another model, or the model is missing, the arm is skipped and `find`
answers exactly as without it. `fox vectors status` shows the setting, the model and each
project's index; `fox vectors build` rebuilds a damaged one. To stop using it, set
`vectors = false` (or delete `~/.foxdb/vectors/`).

## Session transcripts

`fox ingest sessions [--since-days N]` reads Claude Code transcripts
(`~/.claude/projects/*/*.jsonl`, including `subagents/`) and Codex rollouts
(`~/.codex/sessions/**/rollout-*.jsonl` and `.jsonl.zst`) read-only. Each transcript becomes one
redacted digest (first real prompt, last answer, up to 15 file anchors, turn count, dates, parent
session for subagents), searchable with `fox find --kind memory` or `fox_memory recall`, and
readable with `fox_read x:<id>`. Raw transcripts are never copied. Unchanged files are skipped by
size and mtime; a first run over a large history is best bounded with `--since-days`.
Roots: `FOXDB_CLAUDE_PROJECTS`, `FOXDB_CODEX_SESSIONS`.

## With headroom

headroom runs as a local proxy that compresses tool output into hash markers and offers
`headroom_retrieve`. foxdb does not depend on it and does not compress output in a competing
way: its answers are already short plain-text rows (no large JSON), and foxdb's "already sent"
references cover only foxdb's own handles. If headroom does replace a foxdb answer with a marker,
`headroom_retrieve` or `fox_read <handle>` both get it back. The benchmark records which mode each
number came from.

Recommended owner setting: exclude foxdb's tools from headroom's lossy compression, since their
answers are already ranked and budgeted (headroom's default exclusions do not cover MCP tools):

```sh
export HEADROOM_EXCLUDE_TOOLS='mcp__foxdb__*'   # add it to any list you already set
```

foxdb output never contains headroom's markers (`hash=`, "compressed", `<<ccr:`, "same as msg");
a test checks the tool list, instructions, briefs, answers and the report schema.

## Limitations

- Swift symbols need an Xcode index store (a build must have run); unbuilt targets only show `~` tree-sitter rows. On Linux Swift needs `FOXDB_INDEXSTORE_LIB`.
- Non-Swift callers are name-resolved by tree-sitter, not type-checked, unless a fresh SCIP import covers the file (see [Precise tier](#precise-tier-scip)); treat `callers` and `impact` as a lower bound. A call through a receiver resolves by its last name: certainly when the receiver is named like the type (`inputs.render()` reaches `Inputs.render`), as a guess when the name has several words and at most three members share it, and not at all for one-word names (`push`, `get`) with no scope signal, so calls on interfaces, generics and library types are often missed. C free functions resolve across the project, but calls through function pointers and macros are not seen. Guessed rows are marked `guess 1 of N` (or `guess by name` for a lone candidate) and counted in the header (`12 call sites in 4 files, 5 guessed by name`).
- `impact` lists, level by level, the definitions that call or name the symbol (a d1 row also shows the line that uses it, as `| :line code`) (a function passed as an argument or named in an initializer table counts), and files that use it at their top level (a C command table, a Python module-level call, a test registered inside an anonymous callback) as `  :line (top)`; such a file is counted but not followed further. Through dispatch it also lists the callers of the interface or base methods the symbol implements or overrides (` via Router.add`) and, for an interface method, its implementations (` implements Router.add`) and their callers. Supertypes are the ones a class names (`implements`, `extends`, base classes). A Go type matches an interface when it has every method of it (embedded interfaces included) and the link has evidence: both in one directory, or a file of the type's package that imports the interface's package and names the interface. A caller of the interface method found only by a name guess is listed but not followed further; in Python and JS it counts only through `self`/`this` or a receiver named like the base type. Lines starting `partial at dN:` say where the answer may be missing something: the name occurs in files the answer does not list, a call that could have meant a dependent the walk expanded (through a receiver named like its type, or with no nearer candidate) was guessed to another same-name definition, the walk stopped (5000 dependents, or past depth 4), or the dispatch candidate list was capped. Rows reached through a name guess are marked `guess`; a guess among more than eight same-name definitions, through a receiver not named like the target's type, is not listed, only counted in the header (`, 3 weak guesses left out`) with a `partial` (or, for `callers`, `may be incomplete`) line, since it may be real. Supertypes are followed only when resolved with certainty.
- When the name occurs in more files than an answer lists, `callers` and `refs` add `may be incomplete: ...` and `impact` adds `partial at d1: ...`; up to 15 such files are listed at the end under `unresolved mentions:` as `path:line | code`, and more are counted. A file naming it only in comments, imports, C-family declarations (`extern`, prototypes) or strings is listed under `also mentioned (not calls):` instead and left out of that count; when every such file is of this kind, the line says so and carries no `may be incomplete` or `partial`. A field's uses are not tracked: `callers`, `refs` and `impact` of a field say so and list the files that name it. The count comes from the index alone (references of that name not resolved elsewhere, and every code file holding the name as a whole word, case as typed, anywhere in the file), so it also counts comments, strings, same-name locals and fields, and other same-name definitions' callers, and leaves out files that define a same-name symbol; it is a prompt to check, not a ground truth.
- Type references (a type used in a signature) are not recorded for most languages, so `refs` of a type mostly lists constructor calls and supertype lists.
- Search is lexical, trigram, name-prefix and graph based; there is no fuzzy or embedding search.
- A project served by tantivy (opt-in) ranks keyword matches by per-field BM25 rather than FTS5's one weighted score, so its order differs from FTS5's and recall can drop: on the kubernetes eval sets recall@5 fell from 1.00 to 0.80 (lexical) and from 0.67 to 0.31 (retrieval, vague queries hardest hit); see [benchmarks](benchmarks.md#big-projects-milestone-7).
- Damage to a tantivy segment file that tantivy reads without panicking is caught by checksum only when `foxd` opens the index or `fox status` runs; until then searches may read it.
- `fox drift` and `fox pack` match citations by name, so an overloaded or renamed symbol can be missed; with a fresh SCIP import, drift follows a moved symbol by its SCIP id, but a renamed one still reads as vanished.
- The SCIP tier only upgrades references tree-sitter already records; occurrences it does not capture (most type references) are not added.
- Single machine, single user; the store is not designed to be shared.
- Milestone parsing assumes Markdown headings with status words; other log formats are indexed as plain doc sections.
