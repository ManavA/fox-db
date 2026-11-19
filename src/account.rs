//! Line accounting for `callers`, `refs` and `impact`: every line of the indexed code files
//! that names the target as a whole word (what `rg -w name` finds there), sorted into groups,
//! so an answer is a labelled superset of that grep. The first line of the answer counts the
//! groups and says whether every line is accounted for; the groups the answer does not already
//! list follow at the end, as `path:line | code` rows, trimmed to the budget.

use crate::project::Project;
use crate::tscode::{TSym, clip_around, word_at};
use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, HashMap, HashSet};

/// What the resolved lines are: the answer's own rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// `callers` of a function: resolved calls.
    Calls,
    /// `refs`, or `callers` of a type: resolved references.
    Refs,
    /// `impact`: the uses its d1 rows stand for.
    Impact,
    /// No definition of the name is indexed: nothing is resolved.
    Name,
}

/// A line's group. Ordered as the groups are listed at the end of an answer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Cat {
    /// May use the target: not resolved, guessed to another definition, or a weak guess.
    Unresolved,
    /// A reference to the target that is not a call (`callers` only).
    NotCall,
    /// A C/C++ prototype, `extern`, a forward declaration, a TypeScript `declare`.
    Decl,
    /// The definition line of another symbol of the same name, a line inside one, or a use
    /// resolved with certainty to one.
    Same,
    Import,
    /// Only in a comment or a string literal (a docstring included).
    Comment,
    /// The target's own definition line.
    Definition,
}

const LISTED: [Cat; 6] = [Cat::Unresolved, Cat::NotCall, Cat::Decl, Cat::Same, Cat::Import, Cat::Comment];

/// More files naming it than this: too common to read line by line.
const FILES_MAX: usize = 3000;
/// Comment and string rows listed at most, whatever the budget: they are counted, a few shown.
const COMMENT_ROWS: usize = 5;
/// Rows a group keeps when the budget is short, before the rest of the room is shared out.
fn quota(c: Cat) -> usize {
    match c {
        Cat::Unresolved => 8,
        Cat::NotCall => 5,
        Cat::Comment => 2,
        _ => 3,
    }
}

/// A line of a file: (path, line).
pub type Line = (String, i64);
/// A reference on a line: (kind, target, ambiguity).
type RefAt = (String, i64, i64);

/// The accounting of one answer.
pub struct Ledger {
    name: String,
    mode: Mode,
    resolved: usize,
    guessed: usize,
    counts: BTreeMap<Cat, usize>,
    rows: BTreeMap<Cat, Vec<String>>,
    /// Same-name definitions elsewhere (their definition lines are the `Same` rows).
    same_defs: usize,
    stale: Vec<String>,
    /// A rebuild is pending: files naming it only in text may be missed.
    reindexing: bool,
    /// Files naming it, when too many to read.
    too_many: Option<usize>,
}

/// Same-name definition: (id, line, end line, prototype, kind, qualified name).
struct Def {
    id: i64,
    line: i64,
    end: i64,
    decl: bool,
    kind: String,
    qname: String,
}

/// Account for every line of the indexed code naming `name`. `resolved` holds the lines the
/// answer already lists (its calls, refs or d1 uses), `top` the target (None: no definition),
/// `linked` same-name definitions the answer lists too (implementations at d1).
pub fn account(
    c: &Connection,
    p: &Project,
    name: &str,
    top: Option<&TSym>,
    resolved: &HashSet<(String, i64)>,
    linked: &[i64],
    mode: Mode,
) -> Result<Ledger> {
    let mut l = Ledger {
        name: name.to_string(),
        mode,
        resolved: resolved.len(),
        guessed: 0,
        counts: BTreeMap::new(),
        rows: BTreeMap::new(),
        same_defs: 0,
        stale: vec![],
        reindexing: false,
        too_many: None,
    };
    if name.is_empty() || !name.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$') {
        return Ok(l);
    }
    let pid = p.id;
    let top_id = top.map(|t| t.id).unwrap_or(-1);
    // Every definition of that exact name, by file.
    let mut defs: HashMap<String, Vec<Def>> = HashMap::new();
    let mut q = c.prepare_cached(
        "SELECT n.id, n.path, n.line, n.end_line, json_extract(n.meta,'$.k'), json_extract(n.meta,'$.parent'), json_extract(n.meta,'$.d'), n.name
         FROM node n INDEXED BY node_sym_name
         WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.source = 'treesitter' AND n.name = ?2 COLLATE NOCASE LIMIT 20000",
    )?;
    for (id, path, line, end, kind, parent, decl, nm) in q
        .query_map(params![pid, name], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                r.get::<_, Option<i64>>(6)?.is_some(),
                r.get::<_, String>(7)?,
            ))
        })?
        .flatten()
    {
        if nm != name {
            continue;
        }
        let qname = if parent.is_empty() { nm } else { format!("{parent}.{nm}") };
        defs.entry(path).or_default().push(Def { id, line, end: end.max(line), decl, kind, qname });
    }
    // Every reference row of that name, by line: (kind, target, ambiguity).
    let mut refs: HashMap<Line, Vec<RefAt>> = HashMap::new();
    let mut q = c.prepare_cached("SELECT path, line, kind, dst, amb FROM ts_ref WHERE project_id = ?1 AND name = ?2")?;
    for (path, line, kind, dst, amb) in q
        .query_map(params![pid, name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
        })?
        .flatten()
    {
        refs.entry((path, line)).or_default().push((kind, dst, amb));
    }
    // Files naming it as a whole word (comments and strings included).
    let mut files: HashSet<String> = if crate::db::reindex_pending(c, pid) {
        crate::tscode::body_paths(c, pid, name)?.into_iter().collect()
    } else {
        let mut f = c.prepare_cached("SELECT path FROM fts_word WHERE fts_word MATCH ?1 AND project_id = ?2 LIMIT 50000")?;
        f.query_map(params![format!("words:\"{}\"", crate::tsindex::case_word(name)), pid], |r| r.get::<_, String>(0))?.flatten().collect()
    };
    files.extend(defs.keys().cloned());
    files.extend(refs.keys().map(|k| k.0.clone()));
    files.extend(resolved.iter().map(|k| k.0.clone()));
    if files.len() > FILES_MAX {
        l.too_many = Some(files.len());
        return Ok(l);
    }
    let mut files: Vec<String> = files.into_iter().collect();
    files.sort();
    // The spans of the target and of what contains it: a line inside one of these is no line
    // of another definition.
    let top_span = top.map(|t| (t.path.as_str(), t.line, t.end_line.max(t.line)));
    let mut cited: Vec<String> = vec![];
    for path in &files {
        let Some(text) = crate::guard::read_text(&p.root.join(path), path, 4 << 20, false) else {
            if !p.root.join(path).exists() {
                l.stale.push(path.clone());
            }
            continue;
        };
        if !text.contains(name) {
            continue;
        }
        let raw: Vec<&str> = text.lines().collect();
        let ext = path.rsplit_once('.').map(|x| x.1).unwrap_or("");
        let masked = mask(&text, ext);
        let mine = defs.get(path).map(Vec::as_slice).unwrap_or(&[]);
        // Definition line of each same-name symbol: the first line of its first lines naming it.
        let mut def_at: HashMap<i64, &Def> = HashMap::new();
        for d in mine {
            let last = d.end.min(d.line + 5);
            let at = (d.line..=last)
                .find(|n| masked.get((*n as usize).saturating_sub(1)).is_some_and(|m| word_at(m, name).is_some()))
                .unwrap_or(d.line);
            def_at.entry(at).or_insert(d);
        }
        // Spans of other same-name definitions, less those holding the target.
        let spans: Vec<(i64, i64)> = mine
            .iter()
            .filter(|d| d.id != top_id && !matches!(d.kind.as_str(), crate::tsindex::FIELD | "impl" | "extension" | "namespace" | "module"))
            .filter(|d| top_span.is_none_or(|(tp, tl, te)| tp != path || !(d.line <= tl && te <= d.end)))
            .map(|d| (d.line, d.end))
            .collect();
        let inside = |n: i64| spans.iter().any(|(a, b)| *a <= n && n <= *b);
        let mut any = false;
        for (i, line) in raw.iter().enumerate() {
            let n = i as i64 + 1;
            if word_at(line, name).is_none() {
                continue;
            }
            if resolved.contains(&(path.clone(), n)) {
                any = true;
                continue;
            }
            let code = || clip_around(line.trim(), name).unwrap_or_default();
            let m = masked.get(i).map(String::as_str).unwrap_or("");
            let (cat, row) = if let Some(d) = def_at.get(&n) {
                if linked.contains(&d.id) {
                    l.resolved += 1;
                    any = true;
                    continue;
                }
                if d.id == top_id {
                    (Cat::Definition, String::new())
                } else {
                    let cat = if d.decl || !matches!(ext, "py" | "pyi") && prototype(m, name, ext) { Cat::Decl } else { Cat::Same };
                    (cat, format!("  {path}:{n} {} {} c:{:x} | {}", d.kind, d.qname, d.id, code()))
                }
            } else if let Some(rs) = refs.get(&(path.clone(), n)) {
                let mine: Vec<&(String, i64, i64)> = rs.iter().filter(|r| r.1 == top_id).collect();
                if !mine.is_empty() {
                    if mode == Mode::Calls && mine.iter().all(|r| r.0 != "call") {
                        (Cat::NotCall, format!("  {path}:{n} | {}", code()))
                    } else {
                        (Cat::Unresolved, format!("  {path}:{n} weak guess | {}", code()))
                    }
                } else if rs.iter().any(|r| r.0 == "import") {
                    (Cat::Import, format!("  {path}:{n} | {}", code()))
                } else if rs.iter().all(|r| r.1 != 0 && r.2 == 0) {
                    (Cat::Same, String::new())
                } else if rs.iter().any(|r| r.1 != 0) {
                    (Cat::Unresolved, format!("  {path}:{n} guessed elsewhere | {}", code()))
                } else {
                    (Cat::Unresolved, format!("  {path}:{n} | {}", code()))
                }
            } else if import_line(line.trim()) {
                (Cat::Import, format!("  {path}:{n} | {}", code()))
            } else if word_at(m, name).is_none() {
                (Cat::Comment, format!("  {path}:{n} | {}", code()))
            } else if prototype(m, name, ext) {
                (Cat::Decl, format!("  {path}:{n} | {}", code()))
            } else if inside(n) {
                (Cat::Same, String::new())
            } else {
                (Cat::Unresolved, format!("  {path}:{n} | {}", code()))
            };
            any = true;
            *l.counts.entry(cat).or_default() += 1;
            if !row.is_empty() {
                if cat == Cat::Same {
                    l.same_defs += 1;
                }
                l.rows.entry(cat).or_default().push(row);
            }
        }
        if any {
            cited.push(path.clone());
        }
    }
    cited.extend(resolved.iter().map(|k| k.0.clone()));
    cited.sort();
    cited.dedup();
    l.stale.extend(stale(c, p, &cited)?);
    l.reindexing = crate::db::reindex_pending(c, pid);
    Ok(l)
}

/// Cited files whose size or modification time differs from what was indexed (or that are gone).
fn stale(c: &Connection, p: &Project, files: &[String]) -> Result<Vec<String>> {
    let mut q = c.prepare_cached("SELECT size, mtime FROM ts_file WHERE project_id = ?1 AND path = ?2")?;
    let mut out = vec![];
    for f in files {
        let Ok((size, mtime)) = q.query_row(params![p.id, f], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))) else { continue };
        let same = std::fs::metadata(p.root.join(f)).is_ok_and(|md| md.len() as i64 == size && crate::inventory::mtime_ms(&md) == mtime);
        if !same {
            out.push(f.clone());
        }
    }
    Ok(out)
}

/// An import line: `import`, `from ... import`, `use`, `using`, `#include`, `require`, a
/// re-export, a Go `package` clause.
fn import_line(l: &str) -> bool {
    ["import ", "from ", "use ", "pub use ", "pub(crate) use ", "using ", "package ", "#include", "#import", "require ", "require("]
        .iter()
        .any(|p| l.starts_with(p))
        || l.starts_with("export {") && l.contains(" from ")
        || l.starts_with("export * from")
}

/// A declaration naming it with no body: `extern`, a C-family prototype (only type words
/// before the name, a parameter list after, a `;` at the end), a forward declaration
/// (`struct name;`), a TypeScript `declare`. `m` is the line with comments and strings blanked.
fn prototype(m: &str, name: &str, ext: &str) -> bool {
    let t = m.trim();
    if t.starts_with("extern ") || t.starts_with("declare ") || t.starts_with("export declare ") {
        return true;
    }
    // A Python `@overload` stub or property setter (no symbol of its own).
    if matches!(ext, "py" | "pyi") {
        let rest = t.strip_prefix("async ").unwrap_or(t);
        return rest
            .strip_prefix("def ")
            .is_some_and(|r| r.trim_start().strip_prefix(name).is_some_and(|x| x.trim_start().starts_with('(')));
    }
    let cfam = matches!(ext, "h" | "hh" | "hpp" | "hxx" | "c" | "cc" | "cpp" | "cxx" | "m" | "mm");
    if !cfam {
        return false;
    }
    let Some(at) = word_at(t, name) else { return false };
    let before = t[..at].trim();
    let after = t[at + name.len()..].trim_start();
    if matches!(before, "struct" | "class" | "union" | "enum") && after == ";" {
        return true;
    }
    const NOT_TYPES: &[&str] = &["return", "else", "case", "do", "new", "delete", "throw", "goto", "sizeof", "co_return", "co_await"];
    !before.is_empty()
        && after.starts_with('(')
        && !t.contains('{')
        && (t.ends_with(';') || t.ends_with(','))
        && before.chars().all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | ' ' | '\t' | '*' | '&' | ':'))
        && !before.split(|ch: char| !(ch.is_alphanumeric() || ch == '_')).any(|w| NOT_TYPES.contains(&w))
}

/// Each line of `text` with its comments and string literals blanked, for a file of extension
/// `ext`: `//` and `/* */` in C-family languages, `#` in scripting languages, `--` in Lua, and
/// Python's triple-quoted strings (docstrings) across lines. A JavaScript template literal stays:
/// `${f()}` may call. Conservative where unsure: what stays may be code.
pub(crate) fn mask(text: &str, ext: &str) -> Vec<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        Code,
        Block,
        Str(char),
        Triple(char),
    }
    let hash = matches!(ext, "py" | "pyi" | "rb" | "sh" | "bash" | "zsh" | "pl" | "r" | "R" | "php" | "ex" | "exs");
    let slash = !matches!(ext, "py" | "pyi" | "rb" | "sh" | "bash" | "zsh" | "pl" | "r" | "R" | "lua" | "ex" | "exs");
    let dash = ext == "lua";
    let py = matches!(ext, "py" | "pyi");
    let squote = !matches!(ext, "rs" | "ml" | "clj");
    let raw_tick = ext == "go";
    let js_tick = matches!(ext, "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts");
    let sh = matches!(ext, "sh" | "bash" | "zsh" | "pl");
    let mut st = St::Code;
    let mut out = Vec::new();
    for line in text.lines() {
        let ch: Vec<char> = line.chars().collect();
        let mut m = String::with_capacity(line.len());
        let mut esc = false;
        let mut i = 0;
        // Where the open string began: (byte in `m`, char in `ch`, the char before its quote).
        let mut open = (0usize, 0usize, ' ');
        let at = |i: usize, s: &str| s.chars().enumerate().all(|(k, c)| ch.get(i + k) == Some(&c));
        while i < ch.len() {
            let c = ch[i];
            match st {
                St::Code => {
                    let hash_here = hash && c == '#' && (!sh || i == 0 || ch[i - 1].is_whitespace());
                    if slash && at(i, "//") || hash_here || dash && at(i, "--") {
                        if dash && at(i, "--[[") {
                            st = St::Block;
                        }
                        break;
                    }
                    if slash && at(i, "/*") {
                        st = St::Block;
                        m.push_str("  ");
                        i += 2;
                        continue;
                    }
                    if py && (at(i, "\"\"\"") || at(i, "'''")) {
                        st = St::Triple(c);
                        m.push_str("   ");
                        i += 3;
                        continue;
                    }
                    // A Rust char literal (`'"'`, `'\''`): blanked, it opens no string.
                    if !squote && c == '\'' && (ch.get(i + 2) == Some(&'\'') || ch.get(i + 1) == Some(&'\\')) {
                        let len = if ch.get(i + 1) == Some(&'\\') {
                            ch[i + 2..].iter().skip(1).position(|x| *x == '\'').map_or(1, |k| k + 4)
                        } else {
                            3
                        };
                        let len = len.min(ch.len() - i);
                        m.extend(std::iter::repeat_n(' ', len));
                        i += len;
                        continue;
                    }
                    if c == '"' || squote && c == '\'' || (raw_tick || js_tick) && c == '`' {
                        st = St::Str(c);
                        open = (m.len(), i, if i > 0 { ch[i - 1] } else { ' ' });
                        m.push(' ');
                    } else {
                        m.push(c);
                    }
                }
                St::Block => {
                    let end = if dash { "]]" } else { "*/" };
                    if at(i, end) {
                        st = St::Code;
                        m.push_str("  ");
                        i += 2;
                        continue;
                    }
                    m.push(' ');
                }
                St::Str(q) => {
                    if esc {
                        esc = false;
                    } else if c == '\\' && q != '`' {
                        esc = true;
                    } else if c == q {
                        st = St::Code;
                        // A string that interpolates code stays: `f"{plan()}"`, `"${plan()}"`.
                        let body: String = ch[open.1..=i].iter().collect();
                        if interpolates(ext, open.2, q, &body) {
                            m.truncate(open.0);
                            m.push_str(&body);
                            i += 1;
                            continue;
                        }
                    }
                    m.push(' ');
                }
                St::Triple(q) => {
                    if c == q && ch.get(i + 1) == Some(&q) && ch.get(i + 2) == Some(&q) {
                        st = St::Code;
                        m.push_str("   ");
                        i += 3;
                        continue;
                    }
                    m.push(' ');
                }
            }
            i += 1;
        }
        // A string ends with its line unless continued (`\`) or a Go raw string.
        if matches!(st, St::Str(q) if q != '`') && !esc {
            st = St::Code;
        }
        out.push(m);
    }
    out
}

/// Whether a string literal (`body`, quotes included, opened by `q` after `before`) can run
/// code in a file of extension `ext`.
fn interpolates(ext: &str, before: char, q: char, body: &str) -> bool {
    match ext {
        "py" | "pyi" => matches!(before, 'f' | 'F') && body.contains('{'),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => q == '`' && body.contains("${"),
        "kt" | "kts" | "dart" | "scala" | "groovy" | "php" => q == '"' && body.contains('$'),
        "swift" => body.contains("\\("),
        "cs" => before == '$' && body.contains('{'),
        "rb" => q == '"' && body.contains("#{"),
        "sh" | "bash" | "zsh" | "pl" => q == '"' && (body.contains("$(") || body.contains('`') || body.contains("${")),
        _ => false,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl Ledger {
    pub fn set_guessed(&mut self, n: usize) {
        self.guessed = n;
    }

    fn n(&self, c: Cat) -> usize {
        self.counts.get(&c).copied().unwrap_or(0)
    }

    /// Lines naming it that may use the target and are listed only at the end.
    pub fn unresolved(&self) -> usize {
        self.n(Cat::Unresolved)
    }

    /// The accounting line that leads the answer; None when nothing was read.
    pub fn head(&self) -> Option<String> {
        let name = &self.name;
        if let Some(files) = self.too_many {
            return Some(format!(
                "`{name}` is named in {files} indexed files, too many to account for line by line; the rows below are what resolves to it"
            ));
        }
        let total = self.resolved + self.counts.values().sum::<usize>();
        if total == 0 {
            return None;
        }
        let mut parts = vec![];
        let g = if self.guessed > 0 { format!(" ({} guessed)", self.guessed) } else { String::new() };
        match self.mode {
            Mode::Calls => parts.push(plural(self.resolved, "call", "calls") + &g),
            Mode::Refs => parts.push(plural(self.resolved, "ref", "refs") + &g),
            Mode::Impact => parts.push(plural(self.resolved, "use", "uses") + " (d1)"),
            Mode::Name => {}
        }
        if self.n(Cat::Definition) > 0 {
            parts.push(plural(self.n(Cat::Definition), "definition", "definition lines"));
        }
        if self.n(Cat::NotCall) > 0 {
            parts.push(plural(self.n(Cat::NotCall), "reference, not a call", "references, not calls"));
        }
        if self.n(Cat::Import) > 0 {
            parts.push(plural(self.n(Cat::Import), "import", "imports"));
        }
        if self.n(Cat::Decl) > 0 {
            parts.push(plural(self.n(Cat::Decl), "declaration/prototype", "declarations/prototypes"));
        }
        if self.n(Cat::Same) > 0 {
            parts.push(format!(
                "{} in {} elsewhere",
                self.n(Cat::Same),
                plural(self.same_defs, "same-name definition", "same-name definitions")
            ));
        }
        if self.n(Cat::Comment) > 0 {
            parts.push(plural(self.n(Cat::Comment), "comment/string", "comments/strings"));
        }
        if self.unresolved() > 0 {
            parts.push(format!("{} unresolved", self.unresolved()));
        }
        let status = if !self.stale.is_empty() {
            let k = self.stale.len();
            let some: Vec<&str> = self.stale.iter().take(3).map(String::as_str).collect();
            format!(
                "the index is stale for {} ({}{}): counts may be off; reindex (fox index) first",
                plural(k, "cited file", "cited files"),
                some.join(", "),
                if k > 3 { ", ..." } else { "" }
            )
        } else if self.reindexing {
            "the index is being rebuilt: lines in files not yet re-read may be missing".to_string()
        } else if self.unresolved() == 0 && self.n(Cat::NotCall) > 0 {
            // A function passed as a value may be called through it: not followed.
            let k = self.n(Cat::NotCall);
            format!(
                "may be incomplete: {} it as a value, and calls through {} are not followed (rel=impact lists the code passing it)",
                plural(k, "line passes", "lines pass"),
                if k == 1 { "it" } else { "them" }
            )
        } else if self.unresolved() == 0 {
            "all accounted for".to_string()
        } else {
            let u = plural(self.unresolved(), "unresolved line", "unresolved lines");
            match self.mode {
                Mode::Impact => format!("partial at d1: {u} may use it, listed under unresolved mentions"),
                Mode::Name => format!("no definition of `{name}` is indexed; its lines are listed below"),
                _ => format!("may be incomplete: {u} may use it, listed under unresolved mentions"),
            }
        };
        let total = plural(total, "line names", "lines name");
        Some(format!("{total} `{name}`: {}; {status}", parts.join(", ")))
    }

    fn header(&self, c: Cat) -> String {
        match c {
            Cat::Unresolved => "unresolved mentions:".into(),
            Cat::NotCall => "references, not calls (what is called through them is not followed):".into(),
            Cat::Decl => "declarations and prototypes:".into(),
            Cat::Same => {
                let inner = self.n(Cat::Same) - self.same_defs;
                if inner > 0 {
                    format!(
                        "same-name definitions elsewhere (different symbols; {} inside or using them not listed):",
                        plural(inner, "line", "lines")
                    )
                } else {
                    "same-name definitions elsewhere (different symbols):".into()
                }
            }
            Cat::Import => "import lines:".into(),
            Cat::Comment => "comments and strings:".into(),
            Cat::Definition => String::new(),
        }
    }

    /// The groups' rows for the end of the answer. With `room` (bytes left on a budget below
    /// the largest), groups past it keep a few rows each and say which budget lists them all;
    /// `main` is the size of the rest of the answer. Comment rows are a few at most.
    pub fn tail(&self, room: Option<usize>, main: usize) -> Vec<String> {
        let size = |v: &[String]| v.iter().map(|l| l.len() + 1).sum::<usize>();
        let groups: Vec<(Cat, &Vec<String>)> = LISTED.iter().filter_map(|c| self.rows.get(c).map(|v| (*c, v))).collect();
        let cap = |c: Cat, n: usize| if c == Cat::Comment { n.min(COMMENT_ROWS) } else { n };
        let full: usize = groups.iter().map(|(c, v)| self.header(*c).len() + 1 + size(&v[..cap(*c, v.len())])).sum();
        let mut keep: Vec<usize> = groups.iter().map(|(c, v)| cap(*c, v.len())).collect();
        let trimmed = room.is_some_and(|r| full > r);
        if let Some(room) = room.filter(|_| trimmed) {
            // Headers and the hint first; then each group up to its quota, in order; then the rest.
            keep = vec![0; groups.len()];
            let mut used: usize = groups.iter().map(|(c, _)| self.header(*c).len() + 16).sum::<usize>() + 80;
            for pass in 0..2 {
                for (i, (c, v)) in groups.iter().enumerate() {
                    let upto = if pass == 0 { quota(*c).min(cap(*c, v.len())) } else { cap(*c, v.len()) };
                    while keep[i] < upto && used + v[keep[i]].len() < room {
                        used += v[keep[i]].len() + 1;
                        keep[i] += 1;
                    }
                }
            }
        }
        let mut out = vec![];
        let left_out = groups.iter().zip(&keep).any(|((c, v), k)| *k < cap(*c, v.len()));
        // First, so a page cut does not hide it.
        if left_out {
            let need = (main + full + 64).div_ceil(4);
            out.push(if need <= 4000 {
                format!("rows left out to fit the budget; budget={need} lists them all")
            } else {
                "rows left out to fit the budget; budget=4000 lists them all, over several pages (cursor)".to_string()
            });
        }
        for ((c, v), k) in groups.iter().zip(&keep) {
            let h = self.header(*c);
            if *k < v.len() {
                out.push(format!("{} (first {k} of {})", h.trim_end_matches(':'), v.len()) + ":");
            } else {
                out.push(h);
            }
            out.extend(v[..*k].iter().cloned());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{mask, prototype};

    #[test]
    fn mask_blanks_comments_and_strings() {
        let m = mask("x = 1  # plan it\ny = \"plan\"\n\"\"\"\nplan here\n\"\"\"\nplan()\n", "py");
        assert!(!m[0].contains("plan") && !m[1].contains("plan") && !m[3].contains("plan") && m[5].contains("plan"), "{m:?}");
        let m = mask("/* plan\n * plan\n */ plan(1); // plan\nchar *s = \"plan\";\n", "c");
        assert!(!m[0].contains("plan") && !m[1].contains("plan") && m[2].matches("plan").count() == 1 && !m[3].contains("plan"), "{m:?}");
        let m = mask("fn go<'a>(x: &'a str) { plan(x) }\n", "rs");
        assert!(m[0].contains("plan"), "{m:?}");
        let m = mask("const s = `${plan(1)}`\n", "ts");
        assert!(m[0].contains("plan"), "{m:?}");
        let m = mask("const s = `it's plan`; plan()\n", "ts");
        assert!(m[0].matches("plan").count() == 1, "{m:?}");
        let m = mask("if c == '\"' { plan(x) }\nlet q = '\\''; plan(y)\n", "rs");
        assert!(m[0].contains("plan") && m[1].contains("plan"), "{m:?}");
        for (l, ext) in [
            ("x = f\"{plan()}\"", "py"),
            ("val s = \"${plan()}\"", "kt"),
            ("let s = \"\\(plan())\"", "swift"),
            ("var s = $\"{plan()}\";", "cs"),
            ("s = \"#{plan}\"", "rb"),
            ("x=\"$(plan)\"", "sh"),
            ("n=${#plan} plan", "sh"),
        ] {
            assert!(mask(l, ext)[0].contains("plan"), "{l}");
        }
        for (l, ext) in [("x = \"{plan}\"", "py"), ("s = 'plan'", "rb"), ("x='$(plan)'", "sh")] {
            assert!(!mask(l, ext)[0].contains("plan"), "{l}");
        }
    }

    #[test]
    fn prototypes_and_forward_declarations() {
        for (l, ext) in [
            ("int plan(int q);", "h"),
            ("PlannedStmt *plan(Query *q,", "h"),
            ("static int plan(void);", "c"),
            ("extern int plan;", "c"),
            ("struct plan;", "h"),
            ("declare function plan(x: number): void;", "ts"),
        ] {
            assert!(prototype(l, "plan", ext), "{l}");
        }
        for (l, ext) in [("return plan(q);", "c"), ("int x = plan(q);", "c"), ("int plan(int q) { return q; }", "c"), ("plan(1);", "c")] {
            assert!(!prototype(l, "plan", ext), "{l}");
        }
    }
}
