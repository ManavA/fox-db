# ADR 0002: Code graph (M2) and MCP surface (M3) - results

Status: implemented 2026-10-20/21. Measured on the shared M2/16GB machine; load average was 5-8 during the latency
loop but reached ~250 earlier (other agents' xcodebuild runs), which made cold-index timings unreliable.

## What was built
- Swift: `libIndexStore.dylib` loaded with dlopen from the active toolchain (`src/indexstore.rs`), read-only, never builds.
  Import (`src/swiftimport.rs`) is incremental by unit mtime; records are content-addressed, so unchanged records are
  never re-read. Symbols keyed by USR; `occ` holds raw occurrences; CALLS / REFERENCES / CONFORMS_TO / INHERITS /
  EXTENDS / OVERRIDES edges are derived in SQL. `extension T: P` also yields T CONFORMS_TO P. Accessors fold into their
  property; locals and parameters are not symbols. Stale = source mtime > newest covering unit mtime + 1 s.
- Kotlin/Python and stale/uncovered Swift: pinned codanna in a mirror (`src/codanna.rs`), results labelled `~`.
- `fox watch` (notify/FSEvents): unit changes -> import; source edits -> immediate stale marking.
- `fox code`, `fox mcp` (fox_find, fox_graph, fox_read, fox_memory brief/recall/note). Tool definitions are
  ~2.4 KB JSON (~600 tokens; test asserts <= 1500 at 3 and 4 bytes/token).

## Coverage finding (important)
The two named stores (.DerivedDataPhysicalRelease, .DerivedDataInstalledAppHarness) hold only AetherEngine, ~30
AppShared files and one harness test file; no AppTV/AppMobile code. The importer therefore also reads,
read-only, `.DerivedDataMobile` and Xcode's `~/Library/Developer/Xcode/DerivedData/App-*` (built Sep 28, so many
files are stale). Full-app Swift precision needs a recent Xcode build of those targets.
Scratch import of all four stores: 40,267 symbols, ~516k occurrences, ~163k edges; cold import ~12-20 s, warm no-change 0.15-0.3 s.

## Acceptance: 20 sample methods vs grep
Method: 20 callable symbols with callers in >=2 files, spread by name, in non-stale files (`/tmp` bench.py in session
scratchpad). grep = `\bname\(` over Sources/Tests/Vendor/PhysicalTests *.swift. Each grep line is classified against the index.

| | lines |
|---|---|
| grep hits (non-stale files) | 285 |
| = true call sites also found by the index | 110 |
| grep false positives: call of a different same-named symbol (index resolves it elsewhere) | 79 |
| grep false positives: declarations / comments | 57 / 5 |
| grep hits in files the stores do not cover (index blind; falls back to `~`) | 34 |
| covered-file grep hits the index lacks and that are not declarations/comments/homonyms | 0 |
| index sites grep's pattern misses (non-paren refs, multi-line calls) | 29 |

Index: 139 sites in 44 files, precision 100% by construction (compiler-resolved). grep precision on covered files:
110/(110+79) = 58% (plus decl/comment noise). Within covered files the index found every grep hit that is a real
call and 29 more, so it is better than grep including cross-file; where a caller's file is not in any store the index
is worse than grep and the `~` tree-sitter merge is the only coverage (recall there is name-level only).
Not measured: recall against Xcode's call hierarchy (the plan's >=95% / >=98% gold) - no gold set was built.
Kotlin callers on 10 example-android symbols were not checked.

## Latency: `fox code`
336 process invocations (8 symbols x 7 rels x 6), release binary, warm cache, cwd in example-ios-app:
p50 19.9 ms, **p95 26.5 ms**, p99 36 ms, max 257 ms (one outlier). Found and fixed by this loop: stale-path scan
(80 ms), name lookup using the wrong index (8 ms), `callees`/outline scans (up to 300 ms) -> indexes in schema v3.
The codanna fallback adds ~50 ms per call, so it is off the default path (stale symbols, unknown names, Kotlin/Python).
codanna cold index of the example-ios-app mirror took ~104 s under heavy load (19.8 s in M0 on an idle machine).

## Deferred
Gold-set recall/precision vs Xcode call hierarchy; Kotlin/Python accuracy check; Xcode/SPM/Gradle module nodes;
`decide`/`sessions` memory ops; codanna removal handling is a forced full reindex; launchd daemon for `fox watch`.
