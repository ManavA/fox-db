# ADR 0001: Backend and core language (D1, D2)

Status: accepted for M1 (2026-10-20). Owner rulings folded in: Rust core, "use existing OSS first", 20 ms cold target.

## Context

Machine: M2, 16 GB, macOS 27.2, Xcode 27.1. Test case: example-ios-app (870 tracked .swift). All measurements below
ran against a **mirror** of example-ios-app (see "Ignore behaviour"), never the live checkout.

## M0 measurements

### Ignore behaviour (D7)

| Tool | How it ignores paths | Consequence |
|---|---|---|
| codebase-memory-mcp 0.12.1 (CBM) | `.gitignore`, `.git/info/exclude`, git global excludes, `.cbmignore` in the repo root, built-in skip-lists. `index_repository` has **no exclude argument**. **Symlinks are skipped** (`reason: symlink`). | `.agent-review` is *not* gitignored in example-ios-app, so pointing CBM at the live repo would parse the 31G tree. We may not write `.cbmignore` / `.git/info/exclude` there, and a global excludesFile would change git for every repo. |
| codanna 0.17.1 | `codanna init` writes `.codanna/`, `.codannaignore`, `.fastembed_cache` symlink into the cwd. | Must not run inside the repo. |
| Graphicle 0.1.2 | `graphicle-graph extract --exclude PATTERN`, `--out DIR`. `graphicle setup` edits AGENTS.md and installs git hooks. | Only `graphicle-graph extract` is safe. |

**Resolution:** fox-db builds a filtered mirror at `~/.foxdb/mirrors/<project>-src`: `git ls-files` minus the fox-db
deny list (secrets, `.agent-review`, `.DerivedData*`, `.build`, `.claude`, `exports`, `.server-work`, ...), code
extensions only, copied with APFS clones (`cp -c`, copy-on-write, no extra disk). example-ios-app: 1,211 files / 870 swift,
2.8 s, 33 MB logical. Prototype: `spike/mirror.py`. The 31G directory is never touched by any backend. Long-term (M2)
the Rust core does this with the `ignore` crate and refreshes changed files only.

### Code-graph backends on the mirror (tree-sitter, semantic/embedding features off)

| | CBM 0.12.1 (C) | codanna 0.17.1 (Rust) | Graphicle 0.1.2 (Rust) |
|---|---|---|---|
| Cold index | 7.3 s (`moderate`) | 19.8 s | 4.7 s (`--code-only --no-cluster`) |
| Peak RSS | 450 MB (`CBM_MEM_BUDGET_MB=3000`) | 566 MB | 1.36 GB |
| On-disk | 204 MB SQLite | 70 MB | 174 MB graph.json |
| Graph | 51,070 nodes / 322,322 edges | n/a | 42,223 nodes / 141,476 edges |
| One query, CLI, warm | **710 ms** (`cli search_graph`, every call) | **10-20 ms** (`retrieve symbol`) | 80 ms warm, 610 ms first (re-reads graph.json) |
| Swift inbound callers, 20 methods of two core session classes vs grep call sites | non-zero on 13/20, matches grep on 11 | non-zero on 10/20, matches grep on 10 (misses all cross-file callers tried, e.g. one method with 17 grep call sites found 0) | same method: 0 inbound |

None resolve generic names (`isCurrent`, `disposition`) - tree-sitter limit, as expected.

### Xcode index store (prototype `spike/indexstore/isread.c`)

`libIndexStore.dylib` from the active toolchain, C API declared by hand (Xcode ships no header).
`.DerivedDataPhysicalRelease/Index.noindex/DataStore`:

| units | unique records | symbols | occurrences | call occurrences | definitions | full read | RSS |
|---|---|---|---|---|---|---|---|
| 1,490 | 2,837 | 513,455 | 840,363 | 130,546 | 388,564 | 0.36 s warm / 0.96 s cold | 11 MB |

This is compiler-resolved Swift (USRs, overrides, extensions, protocol witnesses) for the whole app in under a second.

### Prior-art refresh (gh search, Rust, by stars and recent pushes, 2026-10-20)

- **Graphicle-AI/graphicle** (Apache-2.0, 333 stars, created 2026-10-14): code graph + agent memory + MCP in one Rust
  binary, SQLite. Closest thing to fox-db. Not adopted as base now: setup writes into repos (AGENTS.md, git hooks),
  per-query JSON graph load, 1.36 GB RSS, weak Swift caller recall in the sample, 6 days old. Re-evaluate its memory
  engine in M4 (possible import/borrow) - watch item.
- sdsrss/code-graph-mcp (84), LordCasser/atlas (39), greysquirr3l/coraline (15), telagod/abyss (10): tree-sitter graph
  MCP servers, small; nothing with Swift semantic resolution.
- styler-ai/ProjectAtlas (453, MIT): file/folder purpose metadata for agents; overlaps the inventory tier only.
- Rust agent-memory MCP servers found: all < 5 stars.
- Nothing found that reads the Swift index store.

## Decisions

**D2 = Rust** (owner ruling). `fox` CLI, indexer and daemon in Rust: `rusqlite` (bundled SQLite + FTS5), `ignore`
(ripgrep's walker), `notify` (FSEvents), tree-sitter crates only where needed. Target: `fox mem brief` and `fox find`
< 20 ms cold. Python is dropped.

**D1 = Rust fox-db core over off-the-shelf parts, wrap don't fork:**
1. **Swift:** read the index store directly from Rust via `libIndexStore.dylib` (dlopen from the active toolchain;
   the M0 C prototype is the reference). indexstore-db is not needed for bulk import. Tree-sitter Swift only for files
   edited since their unit was written.
2. **Kotlin / Python / unbuilt files:** **codanna** is the base (Rust, Apache-2.0, 10-20 ms queries; CBM is 35-70x slower
   per CLI call and fails the plan's 300 ms gate, let alone 20 ms). Wrapped by CLI against the mirror first; linking
   the `codanna` crate in-process is the M2 option if the CLI hop costs too much. Its weak cross-file Swift recall does
   not matter because Swift comes from the index store.
3. **CBM**: not on the hot path. Kept installed (pinned 0.12.1, binary only, its own config set to
   `ui_enabled=false auto_watch=false watcher_enabled=false`, `CBM_CACHE_DIR=~/.foxdb/cbm`) as the M2 cross-check for
   Kotlin call-graph accuracy on example-android, where its `kotlin_lsp.c` may beat codanna. Drop it if it does not.
4. **No fork** of anything at this point.
5. fox-db's own SQLite (`~/.foxdb/fox.db`) is the single serving store; backends feed it, queries never shell out on
   the `find`/`brief` path.

## Installed (M0)

- `~/.foxdb/bin/codebase-memory-mcp-0.12.1` (+ symlink), sha256 `9bfcbe38...60d2` verified against release checksums.
  Its `install.sh` and `install` subcommand were **not** run: no hooks, skills or MCP entries were written.
- `~/.foxdb/bin/codanna-0.17.1` (+ symlink), sha256 `158bea69...3713` verified. `codanna plugin` not run. It created
  `~/.codanna/providers.json` on first `init`.
- Graphicle was run from the scratchpad only; not installed.

## Deferred

- A/B agent baseline runs (arms A grep/Read, B a language-server tool on 10 tasks): deferred to M5 by the coordinator.
- Index-store vs tree-sitter caller comparison against Xcode call hierarchy on 20 gold symbols: M2 (needs the USR
  lookup in the importer; M0 compared the tree-sitter tools against grep only).
- Second DataStore (`.DerivedDataInstalledAppHarness`) and USR merge: M2.

## M1 results (Rust core, 2026-10-20)

example-ios-app through `fox index` (live checkout, read-only):

| metric | value |
|---|---|
| cold inventory | 3.5 s, 29 MB RSS: 1,649 files, 4,540 doc sections, 254 milestone sections (227 AGENT_MILESTONE_LOG + 27 IOS_AGENT_LOG), 21 summary dirs |
| warm refresh (no changes) | 0.36-1.2 s (dominated by lstat-only summary of `.agent-review`: 90,476 files / 32.7 GB) |
| store | 23 MB |
| `fox mem brief` | p50 6.0 ms, p95 6.4 ms (30 runs, process spawn included) - target < 20 ms |
| `fox find` | p50 5.6-6.9 ms, p95 6.0-7.2 ms - target < 20 ms |
| milestone parse | IDs on 218/227 sections, states on 196/227 (sections without a state word have none to find); 30-section random spot check: IDs right on 28/28 that have one |

Not in M1 (moved to M2 with the daemon): the `notify` FSEvents watcher and `foxd`. Freshness until then: the
SessionStart brief spawns a detached incremental `fox index` when the project is unindexed or older than 10 min.
