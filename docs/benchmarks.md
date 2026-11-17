# Benchmarks

Read these numbers with their conditions. Every run below was taken on a shared development host, not a quiet machine, and the conditions are given with each table. Targets that are not yet met are listed as open, not as passed.

## Status of the targets

| Target | Status |
|---|---|
| `find` on one mid-sized project | Measured in milestone 1: p95 47 ms cold CLI, 9.2 ms via `foxd` (busy host, load 7-11); see below. Milestone 5's body arm made `find` about 1.4x slower (kubernetes p95 216 to 299 ms, [search quality](#search-quality-milestone-5)), so the milestone 1 figure is out of date |
| A single save re-indexed in about 1 s on a large tree | Met on kubernetes (about 1.0 s median); **open** on a Linux-sized tree, where a single save takes about 1.5-2 s, most of it the file walk that detects changes |
| Shared-store `find` p95 under 150 ms (kubernetes beside linux) | **Open, likely missed.** Measured 153 ms p95 on a busy host (load 11-24) before the milestone 4 follow-up fixes and milestone 5; since milestone 5 single-project kubernetes `find` alone is p95 299 ms (busy host, load about 10), above this target. Not re-measured on a quiet host |
| Retrieval quality never regresses | Gated by `fox eval retrieval --gate` on checked-in sets |

## Query bench


`fox-bench run --set bench/queries/<set>.toml --repo <dir> [--daemon] [--json out.json]` runs
fixed query sets (symbol lookup, callers, doc section, memory recall, natural language) through
the real `fox` binary, so latency includes process spawn. It reports p50/p95 latency, tokens per
answer (4 bytes per token), tokens saved against the naive alternative, hit@5 and MRR against each query's expected
row, the MCP tool-definition size and the store size. `fox-bench tools` prints only the tool
size. Set `FOXDB_HOME` to a scratch store that already indexes the repo, and
`FOXDB_CLAUDE_PROJECTS=bench/fixtures/claude-projects` for the memory queries.

The naive alternative is what an agent without foxdb reads for the same query: `rg -n` for the
query's rarest term (first 250 lines, as agent grep tools truncate) plus one file (first 2,000
lines): the expected file if grep found it, else grep's first file. It depends only on the query,
never on foxdb's answer, so a noisier answer cannot raise its own baseline. It is a rough
yardstick; the A/B runner below is the real measure.

Each run says whether it ran with headroom on or off, read from the process environment
(`ANTHROPIC_BASE_URL` pointing at `127.0.0.1:8787`, or `FOXBENCH_HEADROOM=1`). The CLI bench
does not go through any model, so the mode is recorded rather than effective; it matters for the
A/B runner.

`bench/ab.py tasks.toml` is the agent A/B runner: the same tasks through `claude -p` and
`codex exec`, with foxdb as the only MCP server and with none, each in a throwaway clone. It only
prints the commands unless given `--run`. `--headroom on|off` sets or removes
`ANTHROPIC_BASE_URL` for the child processes only; nothing in your settings changes. Results are
JSON lines with cost, turns, cache reads and writes, pass/fail and the headroom mode.

Milestone 1 results. Both builds were measured on the same day on the same corpus (a snapshot of
the Kotlin app's tracked files and of this repo, one store holding both plus the memory fixture),
5 runs per query, on a shared machine at load 7-11, with headroom off in the bench's environment.
The JSON for the foxdb rows is in `bench/results/m1-*.json`; the Kotlin app is a private project,
so its query set and results stay out of the repository (keep yours in the ignored `bench/private/`). "Before" is `3e226d7` (the cascade search
and full-content FTS5); `foxd` latency is for repeated queries, which its generation-keyed cache
answers.

| Set | Build | p50 | p95 | hit@5 | MRR | Tokens/answer | Store |
|---|---|---|---|---|---|---|---|
| Kotlin app, 26 queries | before | 57 ms | 106 ms | 23/26 | 0.81 | 310 | 86.2 MB |
| | after, cold CLI | 27 ms | 47 ms | 25/26 | 0.88 | 246 | 72.7 MB |
| | after, via `foxd` | 7.5 ms | 9.2 ms | 25/26 | 0.88 | 246 | |
| foxdb itself, 19 queries | before | 38 ms | 63 ms | 15/19 | 0.79 | 24 | |
| | after, cold CLI | 27 ms | 38 ms | 19/19 | 1.00 | 118 | |
| | after, via `foxd` | 7.6 ms | 9.1 ms | 19/19 | 1.00 | 118 | |

On the Kotlin set the naive alternative averages about 50,000 tokens a query. Tokens per answer
fell overall but rose for doc sections (249 to 359), which now show the section's own rows instead
of a shorter list. On the foxdb set the old build answered four natural-language queries with
"no matches"; the new one finds all four, which is most of the rise from 24 to 118 tokens.

The `bench/results/m0-baseline-*.json` files are the earlier baseline of the old build, recorded
from an environment with headroom on (`ANTHROPIC_BASE_URL` at the proxy). The CLI bench does not
pass through a model, so the mode does not change those numbers, but it is labelled as recorded.

## Scale (milestone 4)

`bench/results/m4-scale-foxdb.json`: kubernetes (shallow clone, about 32k files) alone, then linux (about 97k files) added to the same store; 32 cores, store on tmpfs, shared host at load 9-24, so treat latencies as plus or minus 2x.

| Measure | Before (`419dcf2`) | After |
|---|---|---|
| kubernetes cold index | 501 s | 77 s |
| kubernetes save, one leaf file | 6.9 s | 0.83 s |
| kubernetes save, 100 files | 88 s | 3.7 s |
| linux save, one leaf file | 27 s | 1.8 s |
| linux save, 100 files | 376 s | 9.5 s |
| shared-store `find` p50 / p95, CLI | 129 / 6,540 ms | 73 / 153 ms |
| `foxd` first `find` after a save | 4.9 s | 0.23 s |
| `foxd` peak RSS | 1,011 MB | 258 MB |

The "after" latencies were measured before the follow-up fixes (scope checked per match before ranking, prefix matches kept in project slices, fragmented ranges bridged into at most 8 slices, background graph rebuilds). Reviewer re-measurements on the same build: a one-file save is about 1.0 s median on kubernetes (2-3 s for files that define very common names) and 1.64 s with no change on linux, most of it the file walk.

## Search quality (milestone 5)

Recall@5 and MRR on checked-in query sets over kubernetes, before and after the body arm and routing, with the cost in store size and latency: see [search-quality.md](search-quality.md#results). Vague recall@5 went from 0.11 to 0.43; `find` p50 rose from 92 to 142 ms on a host at load about 10.

## Orchestration (milestones 2 and 3)

Packet size, dispatch and wait latency: see [multi-agent.md](multi-agent.md#worked-examples).
