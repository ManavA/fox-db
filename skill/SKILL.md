---
name: foxdb
description: Navigate an indexed codebase by name - where X is defined, what calls X, what breaks if X changes, which doc or past session covers X. Use when asked to find, trace, change, rename or delete a function, type, field or file, or to recall a past decision or handoff, even when a grep looks quicker, because the fox_* tools return ranked path:line rows with the code at each call site and a call graph that follows qualified and aliased names.
---

# foxdb

Every session starts with a `foxdb · <project> ...` brief. The index is local and refreshes itself. Four MCP tools; the same verbs exist as the `fox` CLI for a shell.

## The loop

1. **Find.** `fox_find q=<name or words>` for where X is. A row ends in `path:line` and starts with a handle (`c:1f3`).
2. **Trace.** `fox_graph target=<handle or name> rel=callers` for who calls X, `rel=impact` for what breaks if X changes, `rel=overrides` for subclass methods. Rows carry the code at each call site.
3. **Read.** `fox_read target=<handle>` returns that symbol's body; `fox_read target=<path>` returns the file's declarations with line ranges. Read a file whole only for lines these do not give you.

Done when: every call site from `rel=callers` (or every `d1` row of `rel=impact`) is accounted for before you change a signature, and every `may be incomplete` or `partial` line is checked with the `rg` command it names.

For past decisions, handoffs and long logs: `fox_memory op=recall q=<words>`; `op=post` leaves a finding for other agents, `op=read since=auto` shows theirs.

## Common rationalizations

| Rationalization | Reality |
|---|---|
| A grep is enough | Grep misses aliased imports and package-qualified calls (`podutil.GetContainerStatus`) and returns same-name functions of other packages; `rel=callers` resolves them. |
| I'll just Read the file | `fox_read` returns the one symbol, or the file's declarations with ranges to read next. |
| The answer was empty, so foxdb cannot help | Try the handle from `fox_find`, or `Type.member` / `pkg.Name`; then follow the `rg` line it gives. |

## Reference

| Need | MCP tool | CLI |
|---|---|---|
| Where is a symbol, file, doc section, milestone, memory, session | `fox_find(q, kind?)` | `fox find <q> [--kind ...]` |
| Callers / refs / overrides / conformers / impact | `fox_graph(target, rel)` | `fox code <sym> --rel callers\|...\|impact` |
| Body, outline, doc section, handoff, session digest | `fox_read(target, mode?)` | `fox code <sym> --rel src\|outline` |
| Recall, note, board, handoffs | `fox_memory(op)` | `fox mem brief\|post\|read\|pickup` |
| What a design doc cites that changed | | `fox drift [doc.md]` |
| Minimal context for a fresh reviewer | | `fox pack <doc.md>` |

- Handles: `s:`/`c:` symbol, `f:` file, `d:` doc, `m:` milestone, `a:` dir, `g:` memory, `x:` session, `h:` handoff.
- Every tool takes `budget` (tokens, default 500). `+N more · cursor=K` means the answer is cut: pass `cursor=K` for the rest. `code left out to fit the budget; budget=N shows it` means call again with that budget for the code.
- `~` = name-resolved (tree-sitter), not compiler-resolved; `guess 1 of N` marks a call resolved by name among N same-name definitions.
- A repeated `fox_read` returns `= already sent ...`; pass `fresh=true` if that text is no longer in your context.
- Secrets are never read or indexed. If a `fox` call errors, continue with your other tools. foxdb is read-only: edit with your editor tools.
