# Search quality: the body arm, routing, the backend seam and the eval

Milestone 5 changed what `fox find` reads for a question in words, moved FTS5 merge work out of
the write path, put the search engine behind a typed seam, and checked in query sets that every
later change is measured against.

## What changed

**Body arm.** Before, a symbol's full-text row held its split name, qualified name and path,
nothing else. A question such as "make an HTTP request body readable again so it can be retried"
shares almost no words with `rewindRequest`, so it was rarely found. Each tree-sitter symbol now
also has a row in `fts_body`: the comment block right above it (`//`, `#`, `/*`, `--`, `;`,
attributes), then its own source, cut at 1,600 characters, with every identifier followed by its
parts (`rewindRequest rewind request`). Swift index-store symbols, which come without source,
get names only. Doc sections, notes and sessions already kept their text in `fts_node`, and the
body arm reads them from there, merged with the symbols by BM25 score.

**Routing.** A bare identifier (`dedupDeltas`, `Queue.dedupDeltas`, `omap_dma_cont`) reads names
only: exact, keyword over names, prefix, trigram and graph arms. Another single token, such as a
path fragment (`cpumanager/policy_st`), reads names and trigrams. Words read the body arm in
place of the names arm, plus the exact arm. Two things that looked harmless hurt questions in the
eval and are gone from their route:

- The trigram arm. It matched one word of the question inside paths (`aggregator` in
  `kube-aggregator/`) and then agreed with the body arm on the wrong rows.
- Strong same-file damping. Fused scores are flat (1/61, 1/62, ...), so halving a repeat from one
  file moved it some sixty places. The answer to a question often sits beside a better-scoring
  sibling in its file, so for questions a repeat now counts 0.95 times as much (a few places).

Two smaller ranking rules came with it: a heading whose title is the query gets the x3 a
definition gets, and a question keeps up to 12 terms but leaves out words found in more than a
tenth of the store (BM25 gives them little weight, and scoring their matches was most of the
time). The eval found no loss from either cut; 0.05 instead of 0.1 already lost questions.

**Snake_case names.** unicode61 splits on `_`, so `omap_dma_context_notifier` was only ever its
parts and `omap_dma_cont*` a phrase query. Names (and body text) now also carry the squashed
form (`omapdmacontextnotifier`), the way a camelCase name is already one token, and queries
squash too. A prefix lookup on a snake_case name is one term.

**FTS5 merges.** FTS5 merges segments inline on writes by default (`automerge`), and with
`contentless_delete` it also rewrites segments whose rows were deleted (`deletemerge`). On a
Linux-sized tree that made a 100-file update spend about 30 s merging. Both are off for
`fts_node`, `fts_trigram` and `fts_body`. Each index run ends with one bounded merge step (64
pages per table), `foxd` merges 256 pages at a time while no request has arrived for 20 s
(`FOXDB_IDLE_SECS`), and `fox index optimize` merges every table into one segment.

**Upgrading.** Schema 13 creates `fts_body`, fills it with every existing symbol's names at once
(so the body arm never misses a symbol), and marks each project for a body backfill. The next
index run of a project reads its files once and writes only their symbols' body rows: node ids,
and so `~c:` handles, stay the same.

## The seam

`src/search.rs` defines the interfaces a later engine implements:

```rust
pub struct Scope { pub projects: Vec<i64>, pub kinds: Vec<String>, pub path_prefix: Option<String> }
pub struct Terms { pub words: Vec<String>, pub ident: Option<String>, pub qualified: bool }
pub enum Field { Names, Body }

pub trait SearchBackend {
    fn keyword(&self, t: &Terms, field: Field, s: &Scope, k: usize) -> Result<Vec<i64>>;
    fn substring(&self, needle: &str, s: &Scope, k: usize) -> Result<Vec<i64>>;
    fn prefix(&self, ident: &str, s: &Scope, k: usize) -> Result<Vec<i64>>;
}
pub trait IndexSink {
    fn apply(&mut self, changes: &[Change], seq: u64) -> Result<()>;
    fn generation(&self, project: i64) -> u64;
    fn rebuild(&mut self, project: i64) -> Result<()>;
}
pub trait VectorBackend { fn nearest(&self, v: &[f32], s: &Scope, k: usize) -> Result<Vec<(i64, f32)>>; }
pub trait Embedder { fn dims(&self) -> usize; fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>; }
```

A backend gets normalized terms and a logical scope, never FTS5 syntax or SQL. The SQLite
backend (`Sqlite`) renders the scope itself (`Filter::render`: kinds, projects, path prefix, and
the id ranges of a minority project in a shared store). `NoVectors` is the empty vector arm;
with the `vectors` feature, `vectors::Index` and `vectors::Model` implement the two vector
traits (milestone 8, opt-in: see [architecture.md](architecture.md#vector-arm-opt-in)). The exact and graph arms, fusion and re-ranking stay outside the seam, so a tantivy
backend changes none of them. Results are node ids; display always comes from SQLite, which stays
the system of record.

## The eval

`fox eval retrieval --set FILE [--repo DIR] [--limit K] [--json] [--gate BASELINE]` runs a
query set against the indexed repo, the way the cold CLI searches, and reports per query type:

- recall@5: the share of queries with a target in the top 5 (almost every query has one target);
- hit@10;
- MRR over the top 50.

With `--gate`, it exits non-zero when any type's recall@5 or MRR is below the baseline's. A
target is a `path`, optionally with a symbol `name` (bare, or `Parent.name`), or matches by
containment (`path_contains`, `name_contains`).

| File | Repo | Queries |
|---|---|---|
| `bench/eval/kubernetes-retrieval.json` | kubernetes at the commit in the file | 28 vague, 10 keyword, 10 exact |
| `bench/eval/kubernetes-lexical.json` | same | 15: symbol, keyword, phrase, prefix, substring, regex |
| `bench/eval/linux-lexical.json` | linux at the commit in the file | 15, same types |
| `bench/eval/fixture-retrieval.json` | `bench/eval/fixture/` (in this repo) | 10 vague, 4 keyword, 5 exact |

The fixture set runs in `cargo test` on every change against `bench/eval/fixture-baseline.json`.
The public sets need a clone; their baselines are `bench/results/m5-eval-*.json`:

```
git clone https://github.com/kubernetes/kubernetes && cd kubernetes
git checkout <commit from the set>
FOXDB_HOME=/tmp/fx fox index . --no-global
FOXDB_HOME=/tmp/fx fox eval retrieval --set ../foxdb/bench/eval/kubernetes-retrieval.json \
  --gate ../foxdb/bench/results/m5-eval-kubernetes-retrieval.json
```

Raise a baseline (rerun with `--json` and commit it) when quality improves; never lower one to
make a change pass. Private projects' sets go in the ignored `bench/private/`.

## Results

Kubernetes, shallow clone at the commit in the set (31,914 files, 227,413 symbols). "Before" is
`592d0fa` (milestone 4), "after" this milestone. Each cell is recall@5 / MRR.

| Set, type | n | Before | After |
|---|---|---|---|
| retrieval, vague | 28 | 0.11 / 0.08 | 0.43 / 0.26 |
| retrieval, keyword | 10 | 0.70 / 0.45 | 1.00 / 0.61 |
| retrieval, exact | 10 | 1.00 / 1.00 | 1.00 / 1.00 |
| retrieval, all | 48 | 0.42 / 0.35 | 0.67 / 0.49 |
| lexical, symbol | 4 | 1.00 / 0.81 | 1.00 / 0.81 |
| lexical, keyword | 3 | 1.00 / 0.75 | 1.00 / 0.83 |
| lexical, phrase | 2 | 1.00 / 0.42 | 1.00 / 1.00 |
| lexical, prefix | 2 | 1.00 / 0.75 | 1.00 / 0.75 |
| lexical, substring | 2 | 1.00 / 1.00 | 1.00 / 1.00 |
| lexical, regex | 2 | 1.00 / 1.00 | 1.00 / 1.00 |
| lexical, all | 15 | 1.00 / 0.79 | 1.00 / 0.88 |

On a private Kotlin app with the same three query types (36 queries, kept in `bench/private/`),
vague recall@5 went from 0.28 to 0.56 and overall from 0.44 to 0.69. Across both repos (84
queries) that is vague 0.19 to 0.49 and overall 0.43 to 0.68, close to what the research
measured for the body arm with routing (0.21 to 0.55 and 0.44 to 0.71). "Regex" queries are not
run as regular expressions; they hit through the name and trigram arms.

Cost on kubernetes:

- Store 644 MB to 720 MB (+12%).
- Cold index 79 s to 86 s.
- `find` latency over the 63 kubernetes queries, alternating the two builds on a host at load
  about 10: p50 92 to 142 ms, p95 216 to 299 ms (1.4x). Questions pay for the body arm; an
  identifier lookup costs what it did.

The queries were written by the agents that ran the research, after reading the code they
describe, and they cover two repositories. They are a regression gate and a rough guide, not a
measure of agent success; the agent A/B runner (`bench/ab.py`) is that.

## Later

- **Tantivy (done in milestone 7, opt-in):** with `backend = "auto"` a project of about 1M rows
  or more gets a tantivy index (off by default: recall trails FTS5 on the kubernetes sets)
  kept by `foxd` from a change log written in the same transaction; it falls back to FTS5
  whenever the index lags. See [architecture.md](architecture.md#derived-tantivy-index-for-big-projects)
  and [benchmarks.md](benchmarks.md#big-projects-milestone-7).
- **Vectors:** a trial with usearch and a code embedding model in native Rust, for questions only
  (an identifier never reaches it), and only if an agent-task eval shows fewer turns or more
  first-try finds. If it does not, the feature is removed, not tuned.
