# Compiler-oracle tasks: "Certain or Guessing", by hop depth

`oracle.py` builds ground truth for "who calls X" and "what stores X as a function value" from
the compiler, for C (gcc over a recorded compile database), Rust (`cargo check`) and Go
(`go build`, `go test -c`). It adds a parameter to the target, compiles, and classifies each new
error as a call, a store or something else. The module docstring has the exact commands.

## Depth

Each task records `depth`, the number of hops of direct calls from the target:

- depth 1: `oracle-callers-X` (functions that call X) and `oracle-stores-X` (places that use X as a
  value).
- depth 2: `oracle-impact2-X`, the functions that call X plus the functions that call one of
  those callers. The oracle changes each direct caller in turn, so both hops come from the
  compiler. Each expected item records `hop`, its nearest hop.

A depth-2 task is skipped when one direct caller cannot be changed: it is variadic, it has no
single definition in its file, a Rust trait fixes its signature, or the target has more than
`--depth2-max-callers` callers. It is also skipped when nothing lies two hops out, or when the
answer would hold more than `--depth2-max-gold` functions. `log.txt` gives each reason.

Hops follow direct calls only. Calls through function pointers, callbacks, tables and interface
or trait dispatch are invisible to this oracle, and the prompts say to leave them out.

Hop depth is the axis to stratify on: arXiv 2610.33958 finds that each extra hop halves the
odds of localization (https://arxiv.org/abs/2610.33958).

## Fox's own label for each item

At task-build time `oracle.py` asks fox, on the scratch `--fox-home`, the same question:
`fox code X --rel callers` for depth-1 callers, `--rel refs` minus calls for stores, and
`--rel impact --depth 2` for depth 2. Every expected item gets `fox`:

- `certain`: fox lists it, with at least one site it does not mark as a guess;
- `guess`: fox lists it only with sites it marks as guesses (name matches it could not resolve);
- `missing`: fox does not list it.

`fox_extra` lists what fox answers that the oracle says is wrong, with fox's label. `via = true`
marks an impact row fox reached through a function value (a vtable field, say). Fox does not mark
those rows as guesses, but they fall outside the direct-call question. Fox rows in code the build
leaves out (files it does not compile, inactive `#if` branches, cfg'd-off Rust items) are dropped
from both lists.

## Report

`bench/ab.py` rows carry `depth` and the model's `answer`. `ab.py report RESULTS.jsonl` adds per
comparison:

- `depth N`: F1 and pass differences with bootstrap CIs over that depth's tasks;
- `gold found, by fox label`: of the expected items fox labels certain, guess or missing, how many
  each arm's answers name;
- `wrong answers fox also gave`: of the `fox_extra` rows labelled certain or guess, how many each
  arm's answers name. Compare the two rates to see whether wrong answers come from guess rows;
- `wrong-certain`: answers that name a wrong item fox labelled certain.

Rows written before depth and answers were recorded can take them from the task files:
`ab.py report RESULTS.jsonl --tasks c.toml --tasks go.toml`. Without answers the label lines read
`no answers recorded`. Grading matches names only, so a `fox_extra` row that shares its name
with an expected item is left out of the counts.

## Edit tasks graded by the compiler (not built yet)

The tasks above grade an answer. The next kind grades an edit: "add a parameter to X and fix
every caller", passing when `cargo check` or `go build` succeeds in the run's clone afterwards,
with the `fox check` stop gate (`[hooks] stop_check`) as a third arm. `ab.py` cannot run these
yet: Claude Code runs without edit permission (`claude -p` with no `--permission-mode`), Codex
runs `-s read-only`, and rows are graded from the answer alone. Adding it needs a task field
naming the build command, an edit-permitted mode for both harnesses, and the build's exit code
in the row.

## Related work

- arXiv 2610.33958, each extra hop halves the odds of localization: https://arxiv.org/abs/2610.33958
- RepoReason: https://arxiv.org/abs/2601.03731
- CGRizz: https://arxiv.org/abs/2611.29745
