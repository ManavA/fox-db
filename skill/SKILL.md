---
name: foxdb
description: Local code graph + index of this machine's projects (Swift symbols/callers from Xcode's index store, Kotlin/Java/Python/TS/JS/Go/Rust/C/C++/C#/Ruby/PHP/Scala/Dart/Lua/Bash/ObjC via built-in tree-sitter, files, docs, milestone logs, Claude auto-memory, past Claude/Codex sessions, an agent handoff board) behind the `fox_*` MCP tools and the `fox` CLI. Use before grepping for a symbol, caller, file, doc section, milestone state or past decision, and instead of reading multi-MB handoff logs end to end.
---

# foxdb

Every session starts with a `foxdb · <project> ...` brief (SessionStart hook). The index is local, refreshes itself, and answers in a few ms through the `foxd` daemon (tens of ms without it). Four MCP tools; the same verbs exist as the `fox` CLI for Bash/Codex.

## When to use

- **Use** `fox_find` instead of grep/glob to locate a symbol, file, doc section or topic in a large repo.
- **Use** `fox_graph callers|impact` before changing a signature, instead of grepping for call sites.
- **Use** `fox_read` to read one symbol, outline or doc section instead of a whole file.
- **Use** `fox_memory recall` before re-reading long logs or old sessions; `post` to leave findings for other agents, `read` to see theirs.
- **Skip** it when one grep, a file you already know, or something already in context answers.

| Need | MCP tool | CLI |
|---|---|---|
| Find a symbol, file, doc section, milestone, memory, session | `fox_find(q, kind?)` | `fox find <q> [--kind ...]` |
| Callers / refs / conformers / overrides / blast radius | `fox_graph(target, rel)` | `fox code <sym> --rel callers\|...\|impact` |
| Outline, body, doc section, handoff, session digest | `fox_read(target, mode)` | `fox code <sym> --rel outline\|src` |
| Brief, recall, note, board | `fox_memory(op)` | `fox mem brief\|post\|read\|pickup` |
| What a design doc cites that changed since it was written | | `fox drift [doc.md]` |
| Minimal context for a fresh reviewer | | `fox pack <doc.md>` |

Conventions
- Rows start with a handle (`s:`/`c:` symbol, `f:` file, `d:` doc, `m:` milestone, `a:` dir, `g:` memory, `x:` session, `h:` handoff) and end in `path:line`.
- Every tool takes `budget` (tokens, default 500). Under a tight budget rows shrink (full, then skeleton, then ids) before any are dropped; `fox_read <handle>` expands one. `+N more (answer cut) · cursor=K` means the answer is cut; pass `cursor=K` for the rest.
- `~` = not compiler-resolved (tree-sitter, or a Swift file changed since the last build). Treat `callers` as a lower bound for non-Swift code.
- A repeated `fox_read` returns `= already sent ...`; pass `fresh=true` if that text is no longer in your context.
- Secrets are never read or indexed; notes, handoffs and session digests are redacted. If a `fox` call errors or is missing, continue without it.

foxdb is read-only: use your editor tools for edits; foxdb is for finding things, the call graph, memory and handoffs.
