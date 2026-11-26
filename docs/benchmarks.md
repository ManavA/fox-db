# Benchmarks

Read these numbers with their conditions. Every run below was taken on a shared development host, not a quiet machine, and the conditions are given with each table. Targets that are not yet met are listed as open, not as passed.

## Status of the targets

| Target | Status |
|---|---|
| `find` on one mid-sized project | Measured in milestone 1: p95 47 ms cold CLI, 9.2 ms via `foxd` (busy host, load 7-11); see below. Milestone 5's body arm made `find` about 1.4x slower (kubernetes p95 216 to 299 ms, [search quality](#search-quality-milestone-5)), so the milestone 1 figure is out of date |
| A single save re-indexed in about 1 s on a large tree | Met: kubernetes about 0.9 s median; Linux 0.67 s median since milestone 7's walk record (1.8 s before), see [big projects](#big-projects-milestone-7) |
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

The naive alternative is what an agent without fox-db reads for the same query: `rg -n` for the
query's rarest term (first 250 lines, as agent grep tools truncate) plus one file (first 2,000
lines): the expected file if grep found it, else grep's first file. It depends only on the query,
never on fox-db's answer, so a noisier answer cannot raise its own baseline. It is a rough
yardstick; the A/B runner below is the real measure.

Each run says whether it ran with headroom on or off, read from the process environment
(`ANTHROPIC_BASE_URL` pointing at `127.0.0.1:8787`, or `FOXBENCH_HEADROOM=1`). The CLI bench
does not go through any model, so the mode is recorded rather than effective; it matters for the
A/B runner.

`bench/ab.py tasks.toml` is the agent A/B runner: the same tasks on a real repository, in the
style of SWE-bench [4], through `claude -p` and `codex exec`, with fox-db as the only MCP server
and with none. Each run, and the grader, starts from a fresh clone, and the agent's clone has the
task file, `bench/tasks` and `bench/private` removed so it cannot read the answers [2] [8]. A
claude run stops at `--max-usd` (1 USD by default), so arms are compared at a fixed budget [3]. It only
prints the commands unless given `--run`. `--headroom on|off` sets or removes
`ANTHROPIC_BASE_URL` for the child processes only; nothing in your settings changes. By default
(`--store scratch`) each "on" run gets its own `FOXDB_HOME` holding only that clone's index, with no
memory notes or past sessions; `--store live` uses your store. Each repeat (3 by default) shuffles
the task order and the arm order within a task from a recorded `--seed`. A task can list
ground-truth items (`expect`, each optionally labelled `call`, `store`, `doc` or `file`); the runner
scores precision, recall and F1 of the items the final answer names, counting as wrong only other
names that exist in a scratch index of the repo, and `--grader-selftest` checks the grader against
each task's prose oracles (must score 1), an empty answer (the do-nothing agent, must score 0) and
an oracle padded with decoys (listing every name must not pass) [2]. Results are JSON lines with cost,
turns, cache reads and writes, scores, the headroom and store modes, seed, model, harness version,
fox binary sha256 and git commits. `bench/ab.py report RESULTS.jsonl` prints paired on-off
differences per task and overall with bootstrap 95% CIs [7], pass^k (the chance that all k
repeats pass, estimated as in τ-bench [1]) and p90 cost [3], one block per run.
Compiler-oracle tasks (`bench/oracle/`) record a hop `depth` (1 for direct callers, 2 for callers
of callers) and fox's own label for each expected item (certain, guess or missing). On those tasks
the report adds F1 and pass differences per depth and a wrong-certain count: answers naming an item
fox labelled certain that the compiler says is wrong. Each extra hop halves the odds of
localization [9], so depth is the axis to stratify on; RepoReason [10] and CGRizz [11] are related
work. Details in `bench/oracle/README.md`.

`--harness local` asks whether fox-db helps a small local model, where frontier models already find
every caller with grep. It drives `bench/local_agent.py`, a one-command-per-turn bash loop after
SWE-agent [5] and mini-swe-agent [6], against
an OpenAI-compatible server such as llama.cpp's `llama-server` (`--local-url`, temperature 0), with
three arms: `grep` (bash only), `fox-cli` (bash plus the `fox` CLI on the scratch store) and
`packet` (bash plus a fox callers/impact packet for the names in the question, about 800 tokens).
Command output is cut at 2000 characters; a run ends on a `FINAL:` answer or a step, token or
context cap, with one last turn to answer after a cap. Rows add steps, tokens in and out, wall
time, the cap hit and repeated-command loops; `report` compares `fox-cli` and `packet` with `grep`.

Milestone 1 results. Both builds were measured on the same day on the same corpus (a snapshot of
the Kotlin app's tracked files and of this repo, one store holding both plus the memory fixture),
5 runs per query, on a shared machine at load 7-11, with headroom off in the bench's environment.
The JSON for the fox-db rows is in `bench/results/m1-*.json`; the Kotlin app is a private project,
so its query set and results stay out of the repository (keep yours in the ignored `bench/private/`). "Before" is `3e226d7` (the cascade search
and full-content FTS5); `foxd` latency is for repeated queries, which its generation-keyed cache
answers.

| Set | Build | p50 | p95 | hit@5 | MRR | Tokens/answer | Store |
|---|---|---|---|---|---|---|---|
| Kotlin app, 26 queries | before | 57 ms | 106 ms | 23/26 | 0.81 | 310 | 86.2 MB |
| | after, cold CLI | 27 ms | 47 ms | 25/26 | 0.88 | 246 | 72.7 MB |
| | after, via `foxd` | 7.5 ms | 9.2 ms | 25/26 | 0.88 | 246 | |
| fox-db itself, 19 queries | before | 38 ms | 63 ms | 15/19 | 0.79 | 24 | |
| | after, cold CLI | 27 ms | 38 ms | 19/19 | 1.00 | 118 | |
| | after, via `foxd` | 7.6 ms | 9.1 ms | 19/19 | 1.00 | 118 | |

On the Kotlin set the naive alternative averages about 50,000 tokens a query. Tokens per answer
fell overall but rose for doc sections (249 to 359), which now show the section's own rows instead
of a shorter list. On the fox-db set the old build answered four natural-language queries with
"no matches"; the new one finds all four, which is most of the rise from 24 to 118 tokens.

The `bench/results/m0-baseline-*.json` files are the earlier baseline of the old build, recorded
from an environment with headroom on (`ANTHROPIC_BASE_URL` at the proxy). The CLI bench does not
pass through a model, so the mode does not change those numbers, but it is labelled as recorded.

## Versus grep+read, and caller recall (0.2.1)

**A simulated estimate on a busy host, not a measured agent run.** A script played two agents
over the same questions on a private Kotlin app, kubernetes and Linux (shallow clones): one
answering with `fox` only, one with `rg` plus reading whole matching files (first 2,000 lines),
counting answer bytes / 4 as tokens. Host load was 10-24 throughout.

- Tokens per lookup: about 97-99% fewer with fox-db on medium to very large repos. Naive cost is
  mostly whole-file reads; a frugal grep-first agent that reads about 120 lines around each hit
  narrows the gap to roughly 2.5x-30x depending on how much it reads.
- Tool calls: one `fox` call per lookup against several greps and reads.
- Vague (natural-language) search: fox-db ranked the right file first or second on most
  questions; grep listings held it, but usually far down a count-sorted list, so the naive agent
  needed more reads to land on it.
- Caller recall: before 0.2.1, C callers and impact covered only callers near the definition
  (a header prototype tied with its definition), Kotlin calls through receiver chains and
  properties and function references were missed, and Go calls through embedded fields went to
  the wrong type. Measured against `rg -w` ground truth (call-shaped uses outside comments and
  strings, in indexed files; callers of other same-name definitions count against fox-db):

| Corpus | Queries | Mean file recall before | after | Pooled recall before | after |
|---|---|---|---|---|---|
| Linux, `callers` | 12 | 0.40 | 0.90 | 0.09 | 0.99 (one query, `mutex_lock`, paged past the bench budget and is left out of the pooled figure) |
| Linux, `impact` | 2 | 0.52 | 0.96 | 0.09 | 0.93 |
| kubernetes, `callers` | 15 | 0.84 | 0.86 | 0.88 | 0.89 |
| Kotlin app, `callers` | 14 | 0.63 | 0.74 | 0.75 | 0.81 |
| Kotlin app, `impact` | 3 | 0.50 | 0.72 | 0.55 | 0.73 |

  Linux `wake_up_process` went from 3 caller files to 167 of 168, `fd_install` from 3 to 47 of
  47. Of the 27 answers still short of ground truth, 23 now carry the `may be incomplete` (or
  `partial at d1`) line; it also appears on 10 answers that were complete, mostly where a
  same-name local variable or another same-name definition's callers share the word.
- Cost of the change: complete answers are longer (the Linux `callers` set totals about 14,600
  tokens against 1,800, because it now lists the callers), and the completeness line adds about
  20-25 tokens when present. On a small repo, answers whose lists were complete are unchanged
  except where they gained correct callers or now pick the symbol in the case typed. A `callers`
  call on Linux took about 20-35 ms against 14-17 ms (median of 3, cold CLI). A first index run
  after the upgrade parses C, C++, Objective-C, Kotlin and Go files again; store size grew about
  3-4% (Linux 2.6 to 2.7 GB, kubernetes 689 to 713 MB).

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

## Precise tier (milestone 6): no cost without an import

Old (v0.1.0, `a59d4f3`) against new, same host, back to back, alternating order, host load about
10-14 on 32 cores. Without a SCIP import the store schema is identical (the SCIP tables are only
created by the first import) and every `find`, `code` and `drift` answer compared was
byte-identical.

| Repo | Measure | Old | New |
|---|---|---|---|
| fox-db (this repo) | cold index, median | 1.07 s | 1.09 s |
| fox-db | one-file save, median | 0.076 s | 0.070 s |
| fox-db | `find`/`code` p50 / p95 (CLI, 10 queries x 15) | 21.9 / 29.9 ms | 22.1 / 32.0 ms |
| fox-db | store | 3,497,984 B | 3,497,984 B |
| private Kotlin project (2,214 files) | cold index, median | 6.88 s | 7.01 s |
| private Kotlin project | one-file save, median | 0.196 s | 0.184 s |
| private Kotlin project | `find`/`code` p50 / p95 (9 queries x 15) | 36.4 / 52.6 ms | 36.3 / 53.5 ms |
| private Kotlin project | store | 80.60 MB | 80.58 MB |

The cold-index gap is noise: `FOXDB_TRACE` phase times (parse, store, resolve, optimize) match
run for run. The fixture eval and both kubernetes sets give identical scores and pass their
gates (kubernetes retrieval: exact 1.00, keyword 1.00, vague 0.43 recall@5). `fox` grows by
75 KB (45.83 to 45.90 MB) and `foxd` by 128 bytes; the SCIP reader is built in and needs no
new dependency.

## Big projects (milestone 7)

`bench/results/m7-big-projects.json`. Old is `5b735b0` (milestone 6), new is the milestone 7
default build (tantivy feature on). Same host, back to back, alternating order where a measure
repeats; 32 cores, data on tmpfs, a shared host at load 8-13 throughout, so treat latencies as
plus or minus 30%.

**Linux** (shallow clone, 97,132 files, 1.2M tree-sitter symbols: about 1.3M rows, past the
1M trigger, so it gets a tantivy index):

| Measure | Old (FTS5) | New (tantivy) |
|---|---|---|
| `find` p50 / p95, cold CLI, the 15 linux-lexical queries x 5, two runs | 69.5 / 442 and 76.2 / 451 ms | 52.1 / 166 and 52.6 / 145 ms |
| linux-lexical recall@5 / MRR, all 15 | 0.733 / 0.671 | 0.867 / 0.792 |
| one-file save, median of 7 | 1.80 s | 0.67 s |
| no-change save, median of 7 | 1.71 s | 0.35 s |
| SQLite store / of it free pages | 2.92 GB / 0.20 GB | 4.28 GB / 1.52 GB |
| tantivy index | | 0.47-0.56 GB (by segment count) |
| a save visible to tantivy after `fox index` returns | | 0.3-0.6 s (FTS5 serves meanwhile) |

Recall@5 did not drop for any query type (symbol, keyword, phrase, prefix, substring, regex);
one keyword query (`dock station present`) moved from rank 1 to 3, so keyword MRR is 0.78 against
1.00. The free pages are the fill's change log, applied and deleted; SQLite reuses them (`VACUUM`
returns them). A fill took 2.4 minutes in `fox index` and under 2 in `foxd`, whose memory peaked at
1.6 GB while it built and merged the index (four writer threads, since cut to one: see below) and settled at about 100 MB
(11 MB idle after a restart, 44 MB after searches).

**Below the trigger nothing changes.** Kubernetes (31,915 files, 272,618 rows: below both the
1M-row tantivy trigger and the 50,000-file walk record) and this repository, old against new:

| Repo | Measure | Old | New |
|---|---|---|---|
| kubernetes | `find` p50 / p95 (lexical set, 17 queries x 3) | 53.5 / 131.3 ms | 53.0 / 135.4 ms |
| kubernetes | cold index | 81.9 s | 83.7 s |
| kubernetes | one-file save, median of 10 | 0.89 s | 0.95 s |
| kubernetes | store | 728,014,848 B | 728,018,944 B |
| fox-db | `find`/`code` p50 / p95 (21 queries x 15) | 22.2 / 30.0 ms | 22.2 / 29.3 ms |
| fox-db | cold index, median of 3 | 1.04 s | 1.08 s |
| fox-db | one-file save, median of 5 | 0.070 s | 0.064 s |
| fox-db | store | 3,694,592 B | 3,690,496 B |

Every `find` and `code` answer compared was byte-identical (17 on kubernetes, 21 on fox-db, 28 on
a private 2,214-file Kotlin project, whose numbers match the same way), the store schema and the
files under `FOXDB_HOME` were identical, and the kubernetes retrieval and lexical eval reports
were identical (the retrieval gate passes). Store sizes differ by one 4 KB page either way. A
`foxd` on these stores had the same threads (2 idle, 34 after 20 searches), the same memory
(within 1 MB) and created no new files.

The kubernetes one-file save is within the stated noise (old 0.892 s, new 0.949 s in the
results file; an earlier version of this table had the two swapped).

**Why tantivy is opt-in.** A review forced `backend = "tantivy"` on the same kubernetes store
(272,618 rows, below the trigger) and compared it with FTS5 on the committed eval sets:
kubernetes-lexical recall@5 / MRR 0.80 / 0.666 against 1.00 / 0.883 (keyword 0.667 against 1.0,
regex 0.5 against 1.0, symbol 0.75 against 1.0), and kubernetes-retrieval 0.313 against 0.667
(keyword 0.5 against 1.0, vague 0.0 against 0.429: fourteen vague queries FTS5 answers at ranks
1-22 found nothing in the top 5). The Linux lexical set has no vague and only three keyword
queries, so it did not show this. Recall must not drop, so the default `backend` is now `fts5`
and `auto` (the trigger) or `tantivy` is opt-in. The fill also now writes with one thread and
`writer_mb` MB inside `foxd` (the design's budget) instead of four threads; the 1.6 GB peak above
was with four, and the one-thread fill time and peak have not been measured again.

**Binaries and build.** `fox` grows from 45.90 MB to 46.05 MB without tantivy and to 48.02 MB
with it; `foxd` from 43.76 to 43.78 and 46.91 MB. A clean release build of both took 317 s
without tantivy (80 crates) and 349 s with it (164 crates). Tantivy is in the default build,
since it costs nothing at run time below the trigger; `--no-default-features` leaves it out.
Release builds now unwind on panic instead of aborting (so a panic in tantivy falls back to
FTS5): `fox` is 49.29 MB and `foxd` 47.47 MB in the default build (48.54 and 46.93 MB with
abort), 46.50 and 43.92 MB without tantivy. On this repository old (abort) against new, back to
back at host load 16-23: the same `find` answers byte for byte (8 queries), `find` 28 against
25-26 ms, the same files under `FOXDB_HOME` and store sizes within one 4 KB page.

## Vector arm (milestone 8)

`bench/results/m8-vector-arm.json`. Old is `0b0ab9d` (milestone 7). Same host, back to back,
alternating order; 32 cores, data on tmpfs, a shared host at load 8-13 throughout, so treat
latencies as plus or minus 30%.

**Off, nothing changes.** Old against the new default build, and against a build with the
`vectors` feature but without `vectors = true` (fresh homes for each build):

| Repo | Measure | Old | New default | New `vectors` build, arm off |
|---|---|---|---|---|
| fox-db | `find`/`code` p50 / p95 (21 queries x 5) | 21.6 / 29.8 ms | 22.2 / 30.8 ms | 22.7 / 31.5 ms (old 21.7 / 31.3) |
| fox-db | cold index, median of 3 | 1.06 s | 1.12 s | 1.06 s (old 1.01) |
| fox-db | one-file save, median of 5 | 0.071 s | 0.069 s | 0.066 s (old 0.071) |
| private Kotlin project (2,214 files) | `find`/`code` p50 / p95 | 28.6 / 51.0 ms | 29.3 / 51.2 ms | 28.3 / 53.7 ms (old 29.0 / 53.6) |
| private Kotlin project | cold index, median of 3 | 6.63 s | 6.69 s | 6.63 s (old 6.52) |
| private Kotlin project | one-file save, median of 5 | 0.26 s | 0.25 s | 0.23 s (old 0.26) |
| kubernetes | `find` p50 / p95 (retrieval set, 48 x 3, two runs) | 161.7 / 287.1, 157.3 / 270.7 ms | | 161.2 / 274.1, 158.9 / 268.2 ms |
| kubernetes | cold index | 82.1 s | | 83.6 s |
| kubernetes | one-file save, median of 5 | 0.99 s | | 1.00 s |

Every answer compared was byte-identical (21 on fox-db, 28 on the Kotlin project, 48 on
kubernetes), and so were the store schemas and the files under `FOXDB_HOME`; store sizes are
equal or within a page (kubernetes ten pages, 0.006%). A `foxd` on the Kotlin store had the same
threads and memory (46 MB). The eval gates pass unchanged.

**Binaries.** The default build is unchanged (`fox` 49.28 MB, `foxd` 47.47 MB; the feature is
not compiled in). With `--features vectors`, `fox` is 51.64 MB (+2.4 MB) and `foxd` 47.73 MB
(+0.3 MB), from 27 more crates (`ureq` with rustls for `fox vectors setup`, `sha2`,
`unicode-normalization`). It stays out of the default build and the release tarball: the arm is
opt-in and its gain is uneven (below).

**On.** `potion-code-16M`, weight 0.5, sentences of five words or more:

| Set | Type | Off: recall@5 / hit@10 / MRR | On |
|---|---|---|---|
| kubernetes-retrieval | vague (28) | 0.43 / 0.43 / 0.26 | 0.43 / 0.46 / 0.28 |
| kubernetes-retrieval | keyword (10) | 1.00 / 1.00 / 0.61 | 1.00 / 1.00 / 0.67 |
| kubernetes-retrieval | exact (10) | 1.00 / 1.00 / 1.00 | unchanged |
| kubernetes-lexical | all (15) | 1.00 / 1.00 / 0.88 | unchanged (two to four words: the arm never runs) |
| private Kotlin project | vague (25) | 0.56 / 0.72 / 0.38 | 0.68 / 0.76 / 0.48 |
| private Kotlin project | keyword (5), exact (6) | 1.00 / 1.00 / 0.61, 1.00 | unchanged |

On kubernetes ten queries rose and five fell (one from rank 1 to 7); on the Kotlin project
fifteen rose and two fell. The gate in the design (vague MRR up by 0.08, exact recall@5 still
1.00) is met on the Kotlin project and not on kubernetes. Before the five-word rule, the arm
moved a heading that answers the phrase `version skew policy` from first to second on the
lexical set (and to eleventh before headings were embedded).

**Only in a store holding one project.** Every number in the table above came from a fresh
`FOXDB_HOME` holding just the project under test. A review re-run reproduced them exactly in such
homes, then measured one store holding fox-db, kubernetes and the Kotlin project together, as a
real `~/.foxdb` does. There the gain mostly goes away: kubernetes vague MRR 0.278 off against
0.277 on (only hit@10 rose, 0.43 to 0.46, and one query fell from rank 1 to 2), and the Kotlin
project's vague recall@5 stayed at 0.24 (MRR 0.13 to 0.15). The arm-off baseline itself is lower
in the shared store (the Kotlin project's vague recall@5 0.56 to 0.24, keyword 1.00 to 0.40, also
with `FOXDB_VECTORS=0` and so not caused by milestone 8). So the vague-set gain above holds for a
store with one project; in a shared store it is small or none. This is a further reason the arm
stays opt-in and out of the default build.

| Cost | kubernetes (240,674 rows) | Kotlin project (30,131 rows) |
|---|---|---|
| first `fox vectors build` | 9.0-9.6 s | 1.1 s |
| index file | 68 MB | 8.6 MB |
| one-file save with the arm on (off) | 2.0 s (0.97 s) | |
| save that changed nothing, on (off) | 1.18 s (0.83 s) | |
| cold CLI `find` p50 / p95, on (off), two runs | 231 / 330, 219 / 302 (178 / 286, 172 / 283) ms | 101 / 130, 105 / 129 (44 / 62, 49 / 73) ms |
| warm `find` through `foxd` p50 / p95, on (off) | | 35.5 / 53.6 (34.1 / 56.4) ms |
| `foxd` memory, on (off) | 142 MB (53 MB) | 53 MB (27 MB) |

A cold CLI process pays about 50 ms to read the tokenizer's vocabulary, the needed model rows
and the project's index; `foxd` and `fox mcp` pay it once. A save with the arm on re-reads the
store's symbol list and rewrites the index file; a save that changes nothing the index covers
rewrites only its stamp.

## Orchestration (milestones 2 and 3)

Packet size, dispatch and wait latency: see [multi-agent.md](multi-agent.md#worked-examples).

## References

The A/B runner's evaluation methods follow Stanford CS329Z, Engineering AI Agents (Fall 2026,
https://cs329z.stanford.edu/): Lecture 4, Tool Use & Function Calling, and Lecture 15,
LLM-as-Judge & Evaluation Infrastructure (pass^k); Lecture 13, Evaluation Fundamentals &
Benchmark Design (task validity, isolation, budgets, error bars and costs in every report);
Lecture 1, Introduction: What Are Agentic Systems? (cost-controlled evaluation); and Lecture 16,
the SWE-bench guest lecture (real-repository tasks).

1. Yao, S., Shinn, N., Razavi, P., Narasimhan, K. (2024). τ-bench: A Benchmark for Tool-Agent-User Interaction in Real-World Domains. arXiv:2406.12045. https://arxiv.org/abs/2406.12045
2. Zhu, Y., Jin, T., Pruksachatkun, Y., et al. (2025). Establishing Best Practices for Building Rigorous Agentic Benchmarks. arXiv:2507.02825. https://arxiv.org/abs/2507.02825
3. Kapoor, S., Stroebl, B., Siegel, Z. S., Nadgir, N., Narayanan, A. (2024). AI Agents That Matter. arXiv:2407.01502. https://arxiv.org/abs/2407.01502
4. Jimenez, C. E., Yang, J., Wettig, A., Yao, S., Pei, K., Press, O., Narasimhan, K. (2023). SWE-bench: Can Language Models Resolve Real-World GitHub Issues? arXiv:2310.06770. https://arxiv.org/abs/2310.06770
5. Yang, J., Jimenez, C. E., Wettig, A., Lieret, K., Yao, S., Narasimhan, K., Press, O. (2024). SWE-agent: Agent-Computer Interfaces Enable Automated Software Engineering. arXiv:2405.15793. https://arxiv.org/abs/2405.15793
6. mini-swe-agent. https://github.com/SWE-agent/mini-swe-agent
7. Efron, B. (1979). Bootstrap Methods: Another Look at the Jackknife. The Annals of Statistics 7(1). https://doi.org/10.1214/aos/1176344552
8. Grace, M., Hadfield, J., Olivares, R., De Jonghe, J. (2026). Demystifying evals for AI agents. Anthropic. https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents
9. arXiv:2610.33958. https://arxiv.org/abs/2610.33958
10. RepoReason. arXiv:2601.03731. https://arxiv.org/abs/2601.03731
11. CGRizz. arXiv:2611.29745. https://arxiv.org/abs/2611.29745
