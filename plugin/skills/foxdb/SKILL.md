---
name: foxdb
description: Navigate an indexed codebase by name - where X is defined, what calls X, what breaks if X changes, which doc or past session covers X. Use when asked to find, trace, change, rename or delete a function, type, field or file, or to recall a past decision or handoff, even when a grep looks quicker, because the fox_* tools return ranked path:line rows with the code at each call site and a call graph that follows qualified and aliased names.
---

# foxdb

Every session starts with a `foxdb · <project> ...` brief. The index is local and refreshes itself. Four MCP tools; the same verbs exist as the `fox` CLI for a shell.

## The loop

1. **Find.** `fox_find q=<name or words>` for where X is; `kind=doc` for what the docs or README say about X (Markdown, reST and AsciiDoc sections, read with `fox_read target=d:...`). A row ends in `path:line` and starts with a handle (`c:1f3`).
2. **Trace.** `fox_graph target=<handle or name> rel=callers` for who calls X, `rel=impact` for what breaks if X changes, `rel=overrides` for subclass methods. In tree-sitter languages, callers, refs and impact rows carry the line of code.
3. **Read.** `fox_read target=<handle>` returns that symbol's body; `fox_read target=<path>` returns the file's declarations with line ranges. Read a file whole only for lines these do not give you.

Done when: every call site from `rel=callers` (or every `d1` row of `rel=impact`) is accounted for before you change a signature. The first line counts every line naming the target (what `rg -w` finds in code) by group: calls, imports, declarations/prototypes, same-name definitions elsewhere, comments, strings. If it says `all accounted for in indexed <language> code`, no grep of that code is needed (other languages, such as templates, are out of its scope); on `may be incomplete` or `partial`, check the unresolved mentions it lists with their code; if it says the index is stale, reindex first.

For a literal string, error message or config value, use Grep.

For past decisions, handoffs and long logs: `fox_memory op=recall q=<words>`; `op=post` leaves a finding for other agents, `op=read since=me` shows theirs.

## Swarm protocol

When several agents work one task, the board is their shared log. Subagent briefs restate these rules only when `[hooks] swarm_board = true` is set in foxdb's `config.toml`; they hold either way.

1. **Read what is new before each new lead.** `fox_memory op=read since=me` (a subagent adds `agent=<its name>`: subagents share their parent's server), or `since=h:N` from the `parent packet h:N` line of your brief, then the `since=h:N` each read ends with.
2. **Post what you learn, as you learn it.** `op=post kind=fact` for a finding, `kind=failed` for an approach that did not work and why, `kind=question` for an open question. One claim per entry, under 600 characters.
3. **Attach handles, not content.** `evidence=` takes handles (`f:1a`, `c:2b`, `h:3`) or paths; `fox_read` opens them. The files your parent already read are in your brief as handles: read the slice you need.
4. **Return handles.** Your final answer becomes a report and a `summary` entry: findings with `path:line` and handles, not file contents.

## Common rationalizations

| Rationalization | Reality |
|---|---|
| A grep is enough | Grep misses aliased imports and package-qualified calls (`podutil.GetContainerStatus`) and returns same-name functions of other packages; `rel=callers` resolves them. |
| I'll just Read the file | `fox_read` returns the one symbol, or the file's declarations with ranges to read next. |
| The answer was empty, so foxdb cannot help | Try the handle from `fox_find`, or `Type.member` / `pkg.Name`; then check the unresolved mentions it lists. |

## Reference

| Need | MCP tool | CLI |
|---|---|---|
| Where is a symbol, file, doc section, milestone, memory, session | `fox_find(q, kind?)` | `fox find <q> [--kind ...]` |
| Callers / refs / overrides / conformers / impact | `fox_graph(target, rel)` | `fox code <sym> --rel callers\|...\|impact` |
| Body, outline, doc section, handoff, session digest | `fox_read(target, mode?)` | `fox code <sym> --rel src\|outline` |
| Recall, note, board, handoffs, stale memory | `fox_memory(op)` | `fox mem brief\|post\|read\|pickup\|check` |
| What a design doc cites that changed | | `fox drift [doc.md]` |
| Minimal context for a fresh reviewer | | `fox pack <doc.md>` |

- Handles: `s:`/`c:` symbol, `f:` file, `d:` doc, `m:` milestone, `a:` dir, `g:` memory, `x:` session, `h:` handoff.
- Every tool takes `budget` (tokens, default 500). `+N more · cursor=K` means the answer is cut: pass `cursor=K` for the rest. `code left out to fit the budget; budget=N shows it` means call again with that budget for the code.
- `~` = not compiler-resolved (tree-sitter, or Swift changed since the build); `guess 1 of N` marks a call resolved by name among N same-name definitions.
- Secrets are never read or indexed. If a `fox` call errors, continue with your other tools. foxdb is read-only: edit with your editor tools.
