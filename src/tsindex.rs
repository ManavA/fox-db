//! Native tree-sitter code index (replaces the external codanna binary).
//!
//! Each code file is parsed with its grammar and the language's tag query (`languages/*.scm`):
//! definitions become `symbol` nodes (`source = 'treesitter'`, handle `c:`), references land in
//! `ts_ref` and are resolved by name with scope and import awareness into `dst` plus `CALLS` /
//! `REFERENCES` edges. Files are re-parsed only when their size/mtime moved *and* their content
//! hash changed; parsing runs in parallel (rayon), writes in one transaction.

use crate::db;
use crate::lang::{self, Lang};
use crate::outbox::{self, Sink, TBL_BODY, TBL_NODE, TBL_TRI};
use crate::project::Project;
use anyhow::Result;
use rayon::prelude::*;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, QueryCursor};

pub const MAX_FILE: u64 = 2 << 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub kind: String,
    pub name: String,
    pub line: u32,
    pub end_line: u32,
    start: usize,
    end: usize,
    /// Index of the enclosing definition in `Parsed::defs`.
    pub container: Option<usize>,
    /// Explicit container name from a `@container` capture (Go receivers, C++ `A::f`).
    pub container_name: Option<String>,
    /// A declaration without a body (a C/C++ prototype): it stands for its definition.
    pub decl: bool,
    /// Declared `static` (C/C++): visible only in its own file, unless that file is a header.
    pub stat: bool,
    /// A class field holding a function (`dispatch = () => {}`): a method reached only through
    /// `this`, its own file or a receiver named like its type; any other call to it is a guess.
    pub field_fn: bool,
    /// C/C++: the struct or typedef name of a field's, function's (its return value) or
    /// typedef's type (`struct inode *i_inode` -> `inode`), for typing member calls.
    pub ty: Option<String>,
    /// C/C++: a typedef of a function (0) or of a function pointer (1, 2 for a pointer to one):
    /// its pointer depth over a function.
    td: Option<u8>,
    /// C/C++: the pointers a typedef or field adds to its typedef'd type (`op_fn *x` -> 1).
    tk: Option<u8>,
    /// C++: a method declared in its class body without a body (`void start();`); its
    /// out-of-line definition is the one a call reaches.
    pub proto: bool,
    /// C++: a method declared `virtual`, `override` or `final`: a call through a receiver may run
    /// another class's.
    pub virt: bool,
    /// Python: a class whose bases include an expression the index cannot follow (`make()`).
    pub dyn_bases: bool,
    /// C++: an `inline namespace`: its members are named from outside as if it were not there.
    pub inline_ns: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ref {
    /// call | ref | import
    pub kind: &'static str,
    pub name: String,
    pub line: u32,
    /// Innermost definition containing the reference.
    pub caller: Option<usize>,
    /// Called through a receiver (`x.f()`), not bare (`f()`).
    pub recv: bool,
    /// The receiver's last name (`a.inputs.f()` -> `inputs`), when it is a plain name; for an
    /// import, the module it names (`./errors`, `pkg.core`).
    pub via: Option<String>,
    /// The receiver is a parameter or local (`user := ...; user.Save()`), not a package.
    pub recv_local: bool,
    /// C: for a call through a member, the receiver's type path ending in the member
    /// (`T:inode|i_mapping|a_ops|bmap`); for a function stored in a member (`.bmap = ext4_bmap`),
    /// that member's path (`T:address_space_operations|bmap`). See [`c_chain`].
    pub rtype: Option<String>,
    pos: usize,
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
}

impl Parsed {
    /// Display name of a definition's container (`Outer.Inner`).
    /// C++: the name a class is written with from outside (`absl.Status` for `Status` in
    /// `namespace absl { inline namespace lts {` ...), when inline namespaces make it differ from
    /// its container path.
    pub fn outside_name(&self, i: usize) -> Option<String> {
        let d = &self.defs[i];
        if d.container_name.is_some() {
            return None;
        }
        let mut names = vec![d.name.replace("::", ".")];
        let mut skipped = false;
        let mut cur = d.container;
        while let Some(c) = cur {
            if self.defs[c].inline_ns {
                skipped = true;
            } else {
                names.push(self.defs[c].name.replace("::", "."));
            }
            cur = self.defs[c].container;
        }
        names.reverse();
        skipped.then(|| names.join("."))
    }

    pub fn container_of(&self, i: usize) -> Option<String> {
        let d = &self.defs[i];
        if let Some(n) = &d.container_name {
            return Some(n.clone());
        }
        let mut names = vec![];
        let mut cur = d.container;
        while let Some(c) = cur {
            names.push(self.defs[c].name.clone());
            cur = self.defs[c].container;
        }
        if names.is_empty() {
            return None;
        }
        names.reverse();
        Some(names.join("."))
    }
}

pub fn is_type_kind(k: &str) -> bool {
    matches!(k, "class" | "interface" | "object" | "struct" | "enum" | "module" | "trait" | "type" | "protocol")
}

/// A struct or class field: a symbol found by name and listed in outlines, never the target of
/// a call or reference (a field and a same-name method would make every call a guess).
pub const FIELD: &str = "field";

pub fn is_callable_kind(k: &str) -> bool {
    matches!(k, "function" | "method" | "macro" | "constructor")
}

fn ref_kind(suffix: &str) -> &'static str {
    match suffix {
        "call" | "send" => "call",
        "import" => "import",
        // A named supertype (`implements`, `extends`, a base class): kept apart from other type
        // references so dispatch can trust it.
        "implementation" => "impl",
        _ => "ref",
    }
}

/// Is this call name reached through a receiver expression (`x.f`, `x->f`, `[x f]`, `x::f`)?
fn has_receiver(n: tree_sitter::Node, p: Option<tree_sitter::Node>) -> bool {
    let Some(p) = p else { return false };
    let k = p.kind();
    if ["navigation", "member", "field", "selector", "attribute", "scoped", "qualified"].iter().any(|w| k.contains(w)) {
        return p.named_child(0).is_some_and(|c| c.id() != n.id());
    }
    ["object", "receiver", "scope"].iter().any(|f| p.child_by_field_name(f).is_some_and(|c| c.id() != n.id()))
}

/// The last name of the receiver a call goes through: `x` in `x.f()`, `Evaluator` in
/// `pl.Evaluator.Preempt()`. None for calls, indexing or literals as receivers.
fn receiver_name(n: tree_sitter::Node, p: Option<tree_sitter::Node>, src: &[u8]) -> Option<(String, usize)> {
    let p = p?;
    let mut r = p.child_by_field_name("object").or_else(|| p.child_by_field_name("operand")).or_else(|| p.child_by_field_name("argument"));
    if r.is_none() {
        r = p.named_child(0).filter(|c| c.id() != n.id());
    }
    let mut r = r?;
    // Python `super().f()` and `super(Child, self).f()` in `Child`: the next class of its method
    // resolution order. `super(Other, self)` starts elsewhere: a call as receiver, as any other.
    if r.kind() == "call" && r.child_by_field_name("function").and_then(|f| f.utf8_text(src).ok()) == Some("super") {
        let first = r.child_by_field_name("arguments").and_then(|a| a.named_child(0));
        let own = match first {
            None => true,
            Some(a) => {
                let mut up = r.parent();
                while let Some(u) = up.filter(|u| u.kind() != "class_definition") {
                    up = u.parent();
                }
                let cls = up.and_then(|c| c.child_by_field_name("name")).and_then(|n| n.utf8_text(src).ok());
                a.kind() == "identifier" && cls.is_some() && a.utf8_text(src).ok() == cls
            }
        };
        return own.then(|| ("super".to_string(), r.start_byte()));
    }
    // A chain: its last name (`a.b` -> `b`).
    while r.named_child_count() > 0 && ["navigation", "member", "field", "selector", "attribute"].iter().any(|w| r.kind().contains(w)) {
        let last = r.named_child(r.named_child_count() as u32 - 1)?;
        if last.named_child_count() > 0 {
            // `navigation_suffix` and the like wrap the name.
            r = last.named_child(last.named_child_count() as u32 - 1)?;
        } else {
            r = last;
        }
    }
    let t = r.utf8_text(src).ok()?;
    (r.named_child_count() == 0 && !t.is_empty() && t.len() <= 80 && t.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .then(|| (t.to_string(), r.start_byte()))
}

/// C/C++: is field name `nn` (inside `decl`) declared as a function pointer (`(*name)(...)`)?
fn c_fn_pointer(nn: tree_sitter::Node, decl: tree_sitter::Node) -> bool {
    let (mut paren, mut func) = (false, false);
    let mut cur = nn;
    for _ in 0..8 {
        let Some(p) = cur.parent() else { return false };
        if p.id() == decl.id() {
            return paren && func;
        }
        paren |= p.kind() == "parenthesized_declarator";
        func |= p.kind() == "function_declarator" && paren;
        cur = p;
    }
    false
}

/// C: the name a function definition or prototype declares: the identifier a function declarator
/// names directly, under any pointers and parentheses (`**f(void)`, `(*f(int))(int)`). None for
/// a variable (`int **p;`, `int (*fp)(int);`).
fn c_fn_name(decl: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut d = decl.child_by_field_name("declarator")?;
    for _ in 0..16 {
        let next = match d.kind() {
            "pointer_declarator" | "function_declarator" => d.child_by_field_name("declarator"),
            "parenthesized_declarator" | "attributed_declarator" => {
                let mut w = d.walk();
                d.named_children(&mut w).find(|c| c.kind() == "identifier" || c.kind().ends_with("declarator"))
            }
            _ => None,
        }?;
        if next.kind() == "identifier" {
            return (d.kind() == "function_declarator").then_some(next);
        }
        d = next;
    }
    None
}

/// C/C++: what a typedef or a field declares besides its type name, from its declarators:
/// `(td, tk)` (see [`Def`]). A typedef whose declarator is a function (`(*op_fn)(int)`, `op_t(int)`)
/// has `td`, its pointers below the function; a typedef or a field of a typedef'd type
/// (`ty` is `T:..`, in C++ any named type, `S:..`) has `tk`, its pointers. Arrays and functions
/// leave both unset.
fn c_decl_shape(kind: &str, nn: tree_sitter::Node, decl: tree_sitter::Node, ty: Option<&str>, cpp: bool) -> (Option<u8>, Option<u8>) {
    if !(kind == "type" && decl.kind() == "type_definition" || kind == FIELD && decl.kind() == "field_declaration") {
        return (None, None);
    }
    let (mut ptrs, mut func, mut bad) = (0u8, false, false);
    let mut cur = nn;
    for _ in 0..8 {
        let Some(p) = cur.parent() else { break };
        if p.id() == decl.id() {
            break;
        }
        match p.kind() {
            "pointer_declarator" if !func => ptrs = ptrs.saturating_add(1),
            "function_declarator" => func = true,
            "array_declarator" | "reference_declarator" => bad = true,
            _ => {}
        }
        cur = p;
    }
    match (kind == "type" && func, bad) {
        (_, true) => (None, None),
        (true, _) => (Some(ptrs), None),
        _ if func => (None, None),
        _ => (None, ty.is_some_and(|t| t.starts_with(if cpp { "S:" } else { "T:" })).then_some(ptrs)),
    }
}

/// C: the type a declaration (or a cast's type descriptor) names, in its namespace: a struct or
/// union tag as `S:inode` (`const struct inode *`), a typedef name as `T:irq_chip_t`
/// (`irq_chip_t *`). Tags and typedef names are separate namespaces in C. None for a primitive
/// or an anonymous struct.
fn c_type_name(decl: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let t = decl.child_by_field_name("type")?;
    let (ns, n) = match t.kind() {
        "struct_specifier" | "union_specifier" => ("S", t.child_by_field_name("name")?),
        "type_identifier" => ("T", t),
        _ => return None,
    };
    n.utf8_text(src).ok().filter(|s| !s.is_empty() && s.len() <= 120).map(|s| format!("{ns}:{s}"))
}

/// Longest receiver chain kept for a C member call (`a->b.c->d`).
const C_CHAIN_MAX: usize = 8;

/// C: a receiver expression as a type path for resolution: its base (the type, `S:tag` or
/// `T:typedef`, when the base's type is declared or cast, `F:fn` for a call's return value,
/// `V:name` for a name of unknown type) and the fields read on the way, joined by `|`:
/// `inode->i_mapping->a_ops` with a parameter `struct inode *inode` is `S:inode|i_mapping|a_ops`.
fn c_chain(e: tree_sitter::Node, walk: &Walk, src: &[u8], depth: usize) -> Option<String> {
    if depth > C_CHAIN_MAX {
        return None;
    }
    let text = |n: tree_sitter::Node| n.utf8_text(src).ok().filter(|t| !t.is_empty() && t.len() <= 120).map(str::to_string);
    match e.kind() {
        "identifier" => Some(walk.recv_types.get(&e.start_byte()).cloned().unwrap_or_else(|| format!("V:{}", text(e).unwrap_or_default()))),
        "field_expression" => {
            let base = c_chain(e.child_by_field_name("argument")?, walk, src, depth + 1)?;
            Some(format!("{base}|{}", text(e.child_by_field_name("field")?)?))
        }
        "parenthesized_expression" => c_chain(e.named_child(0)?, walk, src, depth + 1),
        "pointer_expression" | "subscript_expression" => c_chain(e.child_by_field_name("argument")?, walk, src, depth + 1),
        "cast_expression" => c_type_name(e.child_by_field_name("type")?, src),
        "call_expression" => {
            let f = e.child_by_field_name("function")?;
            (f.kind() == "identifier").then(|| text(f).map(|t| format!("F:{t}"))).flatten()
        }
        _ => None,
    }
}

/// C++: a receiver expression as a type path, like [`c_chain`] but with the class of the base
/// as `S:Name` (`S:ns::Name` when qualified), `W:Name` for a smart pointer or `optional` holding
/// one, `F:fn` for a call's value, and each step after it led by its operator (`.` or `>` for
/// `->`): `engine.start()` in `Car::go` is `S:Car|.engine|.start`, `p->start()` with
/// `std::unique_ptr<Pump> p` is `W:Pump|>start`. None when the base's type cannot be told.
fn cpp_chain(e: tree_sitter::Node, walk: &Walk, src: &[u8], depth: usize) -> Option<String> {
    if depth > C_CHAIN_MAX {
        return None;
    }
    let text = |n: tree_sitter::Node| n.utf8_text(src).ok().filter(|t| !t.is_empty() && t.len() <= 120).map(str::to_string);
    let pos = e.start_byte();
    match e.kind() {
        "identifier" => {
            if let Some(t) = walk.recv_types.get(&pos) {
                return Some(t.clone());
            }
            // A local or global of a type that cannot be told.
            if walk.locals.contains(&pos) || walk.file_locals.contains_key(&pos) {
                return None;
            }
            Some(format!("S:{}|.{}", walk.recv_class.get(&pos)?, text(e)?))
        }
        "this" => walk.recv_class.get(&pos).map(|c| format!("S:{c}")),
        "field_expression" => {
            let base = cpp_chain(e.child_by_field_name("argument")?, walk, src, depth + 1)?;
            let mut w = e.walk();
            let op = if e.children(&mut w).any(|c| c.kind() == "->") { '>' } else { '.' };
            Some(format!("{base}|{op}{}", text(e.child_by_field_name("field")?)?))
        }
        "parenthesized_expression" => cpp_chain(e.named_child(0)?, walk, src, depth + 1),
        "pointer_expression" => {
            let inner = cpp_chain(e.child_by_field_name("argument")?, walk, src, depth + 1)?;
            let deref = e.child_by_field_name("operator").is_some_and(|o| o.kind() == "*");
            // `*p` of a smart pointer is its pointee; a dereference after a step (`*a.b`) or the
            // address of a smart pointer cannot be told here.
            match inner.strip_prefix("W:") {
                _ if inner.contains('|') => None,
                Some(c) if deref => Some(format!("S:{c}")),
                Some(_) => None,
                None => Some(inner),
            }
        }
        "call_expression" | "cast_expression" | "new_expression" => cpp_value(e, src).filter(|t| !cpp_tparam(t, &walk.tparams)),
        _ => None,
    }
}

/// Python: record file `path`'s classes as the store holds them (their ids, resolved bases and
/// expression-base flag) in `c`, before the file's rows go: the resolution after the re-parse
/// compares them with the fresh ones (see [`py_rescope`]).
fn py_old_classes(tx: &Connection, pid: i64, path: &str, c: &mut CChange) -> Result<()> {
    let mut q = tx.prepare_cached(
        "SELECT id, name, json_extract(meta,'$.db') IS NOT NULL FROM node
         WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND json_extract(meta,'$.k') = 'class' ORDER BY id",
    )?;
    let mut b = tx.prepare_cached("SELECT dst, amb FROM ts_ref WHERE src = ?1 AND src != 0 AND kind = 'impl' ORDER BY rowid")?;
    for (id, name, db) in
        q.query_map(params![pid, path], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))?.flatten()
    {
        let bases: Vec<(i64, i64)> = b.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
        c.py_old.entry((path.to_string(), name)).or_default().push((id, bases, db));
        // Base links from elsewhere to it lose their target when the file's rows go.
        let mut inc = tx.prepare_cached("SELECT rowid FROM ts_ref WHERE dst = ?1 AND dst != 0 AND kind = 'impl'")?;
        for rowid in inc.query_map([id], |r| r.get::<_, i64>(0))?.flatten() {
            c.py_in.insert(rowid, id);
        }
    }
    c.py_paths.insert(path.to_string());
    Ok(())
}

/// Python, after a scoped resolution wrote its results: the `self.`, `cls.` and `super()` calls
/// outside this run whose answer may have changed. A class changed when it appeared, went, or
/// its bases resolve differently (an edit, an import re-targeted, a base class added or
/// removed elsewhere); every class connected to a changed one through base links (its
/// subclasses, its bases, theirs) may now read another method resolution order, so their calls
/// are re-scored.
fn py_rescope(tx: &Connection, pid: i64, refs: &[RefRow], scored: &[(i64, i64, i64)], c: &CChange) -> Result<Vec<i64>> {
    let is_py = |p: &str| lang::for_path(p).is_some_and(|l| l.family == "python");
    // Old class ids -> the re-parsed class of the same file and name (0: gone or ambiguous).
    let mut new: HashMap<(String, String), Vec<PyClass>> = HashMap::new();
    {
        let mut q = tx.prepare_cached(
            "SELECT id, name, json_extract(meta,'$.db') IS NOT NULL FROM node
             WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND json_extract(meta,'$.k') = 'class' ORDER BY id",
        )?;
        let mut paths: Vec<&String> = c.py_paths.iter().collect();
        paths.sort();
        for path in paths {
            for (id, name, db) in
                q.query_map(params![pid, path], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))?.flatten()
            {
                new.entry((path.clone(), name)).or_default().push((id, vec![], db));
            }
        }
    }
    let mut fresh: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    for (r, s) in refs.iter().zip(scored) {
        if r.kind == "impl" && is_py(&r.path) {
            fresh.entry(r.src).or_default().push((s.0, s.1));
        }
    }
    for v in new.values_mut() {
        for k in v.iter_mut() {
            k.1 = fresh.get(&k.0).cloned().unwrap_or_default();
        }
    }
    let mut renum: HashMap<i64, i64> = HashMap::new();
    for (key, olds) in &c.py_old {
        let to = match (olds.as_slice(), new.get(key).map(Vec::as_slice)) {
            ([_], Some([n])) => n.0,
            _ => 0,
        };
        for o in olds {
            renum.insert(o.0, to);
        }
    }
    let norm = |id: i64| renum.get(&id).copied().unwrap_or(id);
    let mut seeds: Vec<i64> = vec![];
    // The changed classes and their bases, old and new: the names they and their ancestors
    // define are the only ones whose resolution order may have changed.
    let mut names_of: Vec<i64> = vec![];
    // The classes whose own bases changed.
    let mut changed: Vec<i64> = vec![];
    let keys: HashSet<&(String, String)> = c.py_old.keys().chain(new.keys()).collect();
    for key in keys {
        let olds = c.py_old.get(key).map(Vec::as_slice).unwrap_or(&[]);
        let news = new.get(key).map(Vec::as_slice).unwrap_or(&[]);
        let same = match (olds, news) {
            ([o], [n]) => o.2 == n.2 && o.1.iter().map(|b| (norm(b.0), b.1)).eq(n.1.iter().copied()),
            _ => false,
        };
        if !same {
            for o in olds {
                seeds.extend(o.1.iter().map(|b| norm(b.0)));
                names_of.extend(o.1.iter().map(|b| norm(b.0)));
            }
            for n in news {
                seeds.push(n.0);
                seeds.extend(n.1.iter().map(|b| b.0));
                changed.push(n.0);
                names_of.push(n.0);
                names_of.extend(n.1.iter().map(|b| b.0));
            }
        }
    }
    // Base links re-scored in other files (a base class added, removed or re-targeted).
    for (r, s) in refs.iter().zip(scored) {
        let old = if r.dst == 0 { c.py_in.get(&r.rowid).copied().unwrap_or(0) } else { r.dst };
        if r.kind == "impl" && !c.py_paths.contains(&r.path) && is_py(&r.path) && (norm(old), r.amb) != (s.0, s.1) {
            seeds.extend([r.src, norm(old), s.0]);
            names_of.extend([r.src, norm(old), s.0]);
            changed.push(r.src);
        }
    }
    seeds.retain(|&x| x > 0);
    if seeds.is_empty() {
        return Ok(vec![]);
    }
    // Every class connected through base links, as stored now.
    let mut up = tx.prepare_cached("SELECT dst FROM ts_ref WHERE src = ?1 AND src != 0 AND kind = 'impl'")?;
    let mut down = tx.prepare_cached("SELECT src FROM ts_ref WHERE dst = ?1 AND dst != 0 AND kind = 'impl'")?;
    let mut seen: HashSet<i64> = seeds.iter().copied().collect();
    let mut todo = seeds;
    let mut classes: Vec<i64> = vec![];
    // The names: those defined in the changed classes, their old and new bases and the bases'
    // ancestors (as stored now; a removed class's names were re-scored by name already).
    let mut anc: HashSet<i64> = names_of.iter().copied().filter(|&x| x > 0).collect();
    let mut up_todo: Vec<i64> = anc.iter().copied().collect();
    while let Some(k) = up_todo.pop() {
        for b in up.query_map([k], |r| r.get::<_, i64>(0))?.flatten() {
            if b > 0 && anc.insert(b) {
                up_todo.push(b);
            }
        }
    }
    // Names defined by the symbols directly in `ids`, and those symbols plus theirs (methods
    // and the functions nested in them), in batches.
    let names_in = |ids: &[i64]| -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        for chunk in ids.chunks(500) {
            let list = chunk.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            let mut q = tx.prepare(&format!("SELECT name FROM node WHERE json_extract(meta,'$.pid') IN ({list}) AND kind = 'symbol'"))?;
            out.extend(q.query_map([], |r| r.get::<_, String>(0))?.flatten());
        }
        Ok(out)
    };
    let members_of = |ids: &[i64]| -> Result<Vec<i64>> {
        let mut srcs: Vec<i64> = vec![];
        let mut level: Vec<i64> = ids.to_vec();
        for _ in 0..2 {
            let mut next: Vec<i64> = vec![];
            for chunk in level.chunks(500) {
                let list = chunk.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
                let mut q = tx.prepare(&format!("SELECT id FROM node WHERE json_extract(meta,'$.pid') IN ({list}) AND kind = 'symbol'"))?;
                next.extend(q.query_map([], |r| r.get::<_, i64>(0))?.flatten());
            }
            srcs.extend(&next);
            level = next;
        }
        Ok(srcs)
    };
    let done: HashSet<i64> = refs.iter().map(|r| r.rowid).collect();
    // The `self.`/`cls.`/`super()` calls of `srcs` whose name is in `names`.
    let calls_of = |srcs: &[i64], names: &HashSet<String>, out: &mut Vec<i64>| -> Result<()> {
        for chunk in srcs.chunks(500) {
            let list = chunk.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            let mut q = tx.prepare(&format!(
                "SELECT rowid, name FROM ts_ref WHERE src IN ({list}) AND src != 0 AND kind = 'call' AND via IN ('self', 'cls', 'super')"
            ))?;
            out.extend(
                q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                    .flatten()
                    .filter(|(x, n)| !done.contains(x) && names.contains(n))
                    .map(|x| x.0),
            );
        }
        Ok(())
    };
    let anc: Vec<i64> = anc.into_iter().collect();
    let names = names_in(&anc)?;
    let mut out: Vec<i64> = vec![];
    // The changed classes and their ancestors call names their subclasses define: a subclass
    // reached or lost may make such a call's answer differ.
    {
        let mut desc: HashSet<i64> = changed.iter().copied().filter(|&x| x > 0).collect();
        let mut dtodo: Vec<i64> = desc.iter().copied().collect();
        while let Some(k) = dtodo.pop() {
            for d in down.query_map([k], |r| r.get::<_, i64>(0))?.flatten() {
                if d > 0 && desc.insert(d) {
                    dtodo.push(d);
                }
            }
        }
        let desc: Vec<i64> = desc.into_iter().collect();
        let dnames = names_in(&desc)?;
        if !dnames.is_empty() {
            calls_of(&members_of(&anc)?, &dnames, &mut out)?;
        }
    }
    if names.is_empty() {
        out.sort_unstable();
        out.dedup();
        return Ok(out);
    }
    while let Some(k) = todo.pop() {
        classes.push(k);
        let near: Vec<i64> = up.query_map([k], |r| r.get(0))?.flatten().chain(down.query_map([k], |r| r.get(0))?.flatten()).collect();
        for n in near {
            if n > 0 && seen.insert(n) {
                todo.push(n);
            }
        }
    }
    // Their calls through `self`, `cls` or `super()` (in methods, and functions nested in them).
    classes.sort_unstable();
    calls_of(&members_of(&classes)?, &names, &mut out)?;
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Python: does class definition `n` name a base the index records no reference for (a call, a
/// `*bases` splat, any other expression)?
fn py_dyn_bases(n: tree_sitter::Node) -> bool {
    let Some(args) = n.child_by_field_name("superclasses") else { return false };
    let mut w = args.walk();
    let named = |x: tree_sitter::Node| matches!(x.kind(), "identifier" | "attribute");
    args.named_children(&mut w).any(|a| match a.kind() {
        "keyword_argument" | "comment" => false,
        "subscript" => !a.child_by_field_name("value").is_some_and(named),
        _ => !named(a),
    })
}

/// C++: is method definition or declaration `n` marked `virtual`, `override` or `final`?
fn cpp_virtual(n: tree_sitter::Node) -> bool {
    let mut w = n.walk();
    let kids: Vec<tree_sitter::Node> = n.children(&mut w).collect();
    kids.iter().any(|c| c.kind() == "virtual" || c.kind() == "virtual_function_specifier")
        || kids.iter().filter(|c| c.kind() == "function_declarator").any(|d| {
            let mut w = d.walk();
            d.children(&mut w).any(|c| c.kind() == "virtual_specifier")
        })
}

/// C++: a type path whose base is a qualified type (`S:detail::Engine`) gets the namespaces
/// around `at` appended (`S:detail::Engine@a.b`, inline namespaces left out): a partly qualified
/// name is looked up from there.
fn cpp_with_ns(t: String, at: tree_sitter::Node, src: &[u8]) -> String {
    let (base, rest) = t.split_once('|').map_or((t.as_str(), None), |(b, r)| (b, Some(r)));
    if !(base.starts_with("S:") || base.starts_with("W:")) || !base.contains("::") || base[2..].starts_with("::") {
        return t;
    }
    let mut names: Vec<String> = vec![];
    let mut up = at.parent();
    while let Some(n) = up {
        if n.kind() == "namespace_definition" {
            let mut w = n.walk();
            let inline = n.children(&mut w).any(|c| c.kind() == "inline");
            if !inline && let Some(nm) = n.child_by_field_name("name").and_then(|x| x.utf8_text(src).ok()) {
                names.push(nm.replace("::", "."));
            }
        }
        up = n.parent();
    }
    if names.is_empty() {
        return t;
    }
    names.reverse();
    let base = format!("{base}@{}", names.join("."));
    match rest {
        Some(r) => format!("{base}|{r}"),
        None => base,
    }
}

/// C++: does type path base `t` (`S:T`, `W:T`, `S:T::inner`) name a template parameter?
fn cpp_tparam(t: &str, tparams: &HashSet<String>) -> bool {
    let b = t.get(2..).unwrap_or("").trim_start_matches("::");
    tparams.contains(b.split("::").next().unwrap_or(""))
}

/// C: the member a function named as an initializer value or assigned to a field is stored in,
/// as a type path ending in that member (see [`c_chain`]): `.bmap = ext4_bmap` in
/// `struct address_space_operations ext4_aops = {...}` is `S:address_space_operations|bmap`,
/// `chip->irq_mask = foo_mask` with `struct irq_chip *chip` is `S:irq_chip|irq_mask`.
fn c_slot(value: tree_sitter::Node, parent: tree_sitter::Node, walk: &Walk, src: &[u8]) -> Option<String> {
    match parent.kind() {
        "assignment_expression" => {
            let left = parent.child_by_field_name("left").filter(|l| l.kind() == "field_expression")?;
            if parent.child_by_field_name("right")?.id() != value.id() {
                return None;
            }
            c_chain(left, walk, src, 0)
        }
        "initializer_pair" => {
            let mut pair = parent;
            let mut path: Vec<String> = vec![];
            for _ in 0..C_CHAIN_MAX {
                // `.a.b = f`: its field designators, in order (an index designator adds none).
                let mut w = pair.walk();
                let names: Vec<String> = pair
                    .children_by_field_name("designator", &mut w)
                    .filter(|d| d.kind() == "field_designator")
                    .filter_map(|d| d.named_child(0)?.utf8_text(src).ok().map(str::to_string))
                    .collect();
                path.splice(0..0, names);
                // Up through arrays of structs (`{ { .f = x }, ... }`) to what the list initializes.
                let mut up = pair.parent()?.parent()?;
                while up.kind() == "initializer_list" {
                    up = up.parent()?;
                }
                let base = match up.kind() {
                    "initializer_pair" => {
                        pair = up;
                        continue;
                    }
                    "init_declarator" => c_type_name(up.parent()?, src)?,
                    "compound_literal_expression" => c_type_name(up.child_by_field_name("type")?, src)?,
                    _ => return None,
                };
                return (!path.is_empty()).then(|| format!("{base}|{}", path.join("|")));
            }
            None
        }
        _ => None,
    }
}

/// Namespaces of libraries: a type in one no project class matches is no project class.
const CPP_LIB_NAMESPACES: &[&str] = &["std", "boost", "absl", "folly", "llvm", "Qt", "cv", "Eigen", "fmt", "grpc", "google"];

/// Wrappers whose pointee is the receiver of `p->f()` and `(*p).f()`; `p.f()` is the wrapper's
/// own member.
const CPP_SMART: &[&str] = &["unique_ptr", "shared_ptr", "weak_ptr", "auto_ptr", "scoped_ptr", "intrusive_ptr", "optional"];

/// Functions whose first template argument is the type they return a wrapper of.
const CPP_WRAP_MAKERS: &[&str] =
    &["make_unique", "make_shared", "make_optional", "static_pointer_cast", "dynamic_pointer_cast", "const_pointer_cast"];

/// Casts whose first template argument is the type of what they return.
const CPP_CASTS: &[&str] = &["static_cast", "dynamic_cast", "reinterpret_cast", "const_cast"];

/// C++: the class a type node names, as `S:Name` (`S:ns::Name` when qualified), or `W:Name` for
/// a smart pointer or `optional` of one. None for a primitive, `auto` or any other template.
fn cpp_type(t: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let text = |n: tree_sitter::Node| n.utf8_text(src).ok().filter(|s| !s.is_empty() && s.len() <= 120);
    match t.kind() {
        "type_identifier" | "namespace_identifier" => text(t).map(|s| format!("S:{s}")),
        "qualified_identifier" => {
            let name = t.child_by_field_name("name")?;
            if name.kind() == "template_type" {
                // `std::unique_ptr<T>`: the wrapper, whatever its namespace.
                return cpp_type(name, src);
            }
            let full: String = text(t)?.chars().filter(|c| !c.is_whitespace()).collect();
            let full = full.trim_start_matches("::");
            (!full.contains('<') && !full.is_empty()).then(|| format!("S:{full}"))
        }
        "struct_specifier" | "class_specifier" | "union_specifier" => cpp_type(t.child_by_field_name("name")?, src),
        "type_descriptor" => cpp_type(t.child_by_field_name("type")?, src),
        "template_type" => {
            if !CPP_SMART.contains(&text(t.child_by_field_name("name")?)?) {
                return None;
            }
            let args = t.child_by_field_name("arguments")?;
            let arg = args.named_child(0).filter(|a| args.named_child_count() == 1 && a.kind() == "type_descriptor")?;
            // `unique_ptr<T[]>`, `shared_ptr<T*>`: not a wrapper of a `T`.
            let inner = arg.child_by_field_name("declarator").is_none().then(|| cpp_type(arg, src)).flatten()?;
            inner.strip_prefix("S:").map(|c| format!("W:{c}"))
        }
        _ => None,
    }
}

/// C++: the type a value expression has when its form says so: `Engine()` and `f()` (the path
/// `F:name`: a function's return type, else the class of that name), `new Engine`,
/// `make_unique<Engine>()` (a wrapper), `static_cast<Engine*>(x)`.
fn cpp_value(v: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let text = |n: tree_sitter::Node| n.utf8_text(src).ok().filter(|s| !s.is_empty() && s.len() <= 120);
    match v.kind() {
        "parenthesized_expression" => cpp_value(v.named_child(0)?, src),
        "new_expression" | "compound_literal_expression" | "cast_expression" => cpp_type(v.child_by_field_name("type")?, src),
        "call_expression" => {
            let mut f = v.child_by_field_name("function")?;
            if f.kind() == "qualified_identifier" {
                f = f.child_by_field_name("name")?;
            }
            match f.kind() {
                "identifier" => text(f).map(|t| format!("F:{t}")),
                "template_function" => {
                    let maker = text(f.child_by_field_name("name")?)?;
                    let arg = f.child_by_field_name("arguments")?.named_child(0)?;
                    let t = cpp_type(arg, src)?;
                    if CPP_WRAP_MAKERS.contains(&maker) {
                        t.strip_prefix("S:").map(|c| format!("W:{c}"))
                    } else {
                        CPP_CASTS.contains(&maker).then_some(t)
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// C++: the type a declaration gives the name `n` it declares (`anc`: its ancestors): the
/// declared type, or for `auto` the type of the initializer when its form says so.
fn cpp_declared_type(d: tree_sitter::Node, n: tree_sitter::Node, anc: &[tree_sitter::Node], src: &[u8]) -> Option<String> {
    let t = d.child_by_field_name("type")?;
    if t.kind() != "placeholder_type_specifier" {
        return cpp_type(t, src);
    }
    // `auto x = value;` (also `auto* x`, `auto& x`): the initializer of the declarator holding `n`.
    let init = anc.iter().rev().take_while(|a| a.id() != d.id()).find(|a| a.kind() == "init_declarator")?;
    let decl = init.child_by_field_name("declarator")?;
    (decl.start_byte() <= n.start_byte() && n.end_byte() <= decl.end_byte()).then_some(())?;
    cpp_value(init.child_by_field_name("value")?, src)
}

/// C++: the class a node opens as the scope of `this` and unqualified members: a class or struct
/// with a body, or a method defined outside its class (`void Car::go() {}`).
fn cpp_class_context(n: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let name = |x: tree_sitter::Node| cpp_type(x, src).filter(|t| t.starts_with("S:")).map(|t| t[2..].to_string());
    match n.kind() {
        "class_specifier" | "struct_specifier" => {
            n.child_by_field_name("body")?;
            name(n.child_by_field_name("name")?)
        }
        "function_definition" => {
            let mut d = n.child_by_field_name("declarator")?;
            while matches!(d.kind(), "pointer_declarator" | "reference_declarator" | "parenthesized_declarator") {
                d = d.child_by_field_name("declarator").or_else(|| d.named_child(0))?;
            }
            let q = d.child_by_field_name("declarator").filter(|_| d.kind() == "function_declarator")?;
            if q.kind() != "qualified_identifier" {
                return None;
            }
            let scope = q.child_by_field_name("scope")?;
            match scope.kind() {
                "namespace_identifier" => scope.utf8_text(src).ok().map(str::to_string),
                "template_type" => name(scope.child_by_field_name("name")?),
                _ => name(scope),
            }
        }
        _ => None,
    }
}

/// Function-like nodes whose parameters and locals can shadow a function name.
const FN_KINDS: &[&str] = &[
    "function_declaration",
    "method_declaration",
    "func_literal",
    "function_definition",
    "lambda_expression",
    "lambda",
    "arrow_function",
    "method_definition",
    "function_expression",
    "function",
    "generator_function_declaration",
];

/// Parents of a value slot whose identifier may name a function (an argument, an initializer
/// element, a keyed value), or just a parameter or local of the enclosing function.
const VALUE_SLOTS: &[&str] = &[
    "literal_element",
    "initializer_pair",
    "initializer_list",
    "argument_list",
    "arguments",
    "keyword_argument",
    "assignment_expression",
    "init_declarator",
];

/// Parameter lists: a name declared in one outside any function body (a C prototype) is no
/// file-level local.
const PARAM_LISTS: &[&str] = &["parameter_list", "parameters", "formal_parameters"];

/// Node kinds that name a declared variable.
fn is_ident(k: &str) -> bool {
    matches!(k, "identifier" | "shorthand_property_identifier_pattern")
}

/// Nodes that open a scope for parameters and locals: functions (any depth, so a lambda's
/// parameter hides a name only inside that lambda), Python comprehensions, and C blocks and `for`
/// statements (a local declared there is visible only there; an unbraced `case` arm is part of
/// its `switch` block).
fn is_scope(k: &str, c_lang: bool) -> bool {
    FN_KINDS.contains(&k)
        || matches!(k, "list_comprehension" | "set_comprehension" | "dictionary_comprehension" | "generator_expression")
        || c_lang && matches!(k, "compound_statement" | "for_statement" | "for_range_loop")
}

/// C: is the innermost parameter list among `anc` (ancestors, parent last) another declarator's
/// (a prototype, a function-pointer variable or a cast inside a body), not the parameters of the
/// function definition it belongs to? Its names are declared nowhere in the body.
fn c_foreign_param(anc: &[tree_sitter::Node]) -> bool {
    // Only a few levels up (a parameter's own declarators), so a deep tree stays linear.
    let mut i = None;
    for (k, a) in anc.iter().enumerate().rev().take(12) {
        match a.kind() {
            "parameter_list" => {
                i = Some(k);
                break;
            }
            "compound_statement" | "declaration" | "field_declaration" | "function_definition" => return false,
            _ => {}
        }
    }
    let Some(i) = i else { return false };
    for a in anc[..i].iter().rev() {
        match a.kind() {
            "function_definition" => return false,
            "function_declarator" | "pointer_declarator" | "parenthesized_declarator" | "attributed_declarator" => {}
            // C++: the parameters of a lambda are its own.
            "abstract_function_declarator" | "lambda_declarator" => {}
            "lambda_expression" => return false,
            _ => return true,
        }
    }
    true
}

/// A name declared twice in one C scope with different types (`#ifdef` arms): no type.
const C_TYPE_CONFLICT: &str = "?";

/// Parents of an identifier used as a receiver (`user.Save()`, `obj.f`).
const RECEIVER_PARENTS: &[&str] = &["selector_expression", "member_expression", "attribute", "field_expression"];

/// What one top-down pass over a file's tree finds.
struct Walk<'t> {
    /// Start bytes of identifiers in value slots or receiver positions that name a parameter or
    /// local of an enclosing function or comprehension.
    locals: HashSet<usize>,
    /// The same for names declared only at file level (a Python module-level loop variable, a
    /// JS `const`, a C global), with the name: one that is also a definition of the file is no
    /// local.
    file_locals: HashMap<usize, String>,
    /// The parent of every named leaf (by node id): a reference's name node looks up its
    /// receiver here.
    parents: HashMap<usize, tree_sitter::Node<'t>>,
    /// C: the declared struct or typedef name of each receiver identifier that names a parameter,
    /// local or file-level variable (`struct inode *inode; inode->i_mapping`), by start byte.
    recv_types: HashMap<usize, String>,
    /// C++: the class whose members an identifier or `this` at a receiver position may name (the
    /// class of the method it is in), by start byte.
    recv_class: HashMap<usize, String>,
    /// C++: the names template parameter lists of the file declare (`template <class T>`): no
    /// class a receiver's type may name.
    tparams: HashSet<String>,
}

/// One top-down pass over the tree (see [`Walk`]). Linear in the tree size: ancestors are kept
/// on a stack, never looked up with `parent()`, which walks down from the root on every call
/// and made deeply nested functions (a long `else if` chain) quadratic.
fn walk_tree<'t>(root: tree_sitter::Node<'t>, src: &[u8], c_lang: bool, cpp: bool) -> Walk<'t> {
    let cfam = c_lang || cpp;
    let mut parents: HashMap<usize, tree_sitter::Node<'t>> = HashMap::new();
    // Scope 0 is the file. (parent scope, declared names)
    let mut scopes: Vec<(usize, HashSet<String>)> = vec![(0, HashSet::new())];
    // C: per scope, the declared type of each name that has a struct or typedef type.
    let mut types: Vec<HashMap<String, String>> = vec![HashMap::new()];
    // Open scopes: (depth of the scope node, scope index).
    let mut open: Vec<(usize, usize)> = vec![];
    // C++: open classes and out-of-line methods: (depth of the node, class name).
    let mut classes: Vec<(usize, String)> = vec![];
    let mut recv_class: HashMap<usize, String> = HashMap::new();
    let mut tparams: HashSet<String> = HashSet::new();
    // (start byte, innermost scope, name) of identifiers in value slots and receiver positions.
    let mut uses: Vec<(usize, usize, String)> = vec![];
    let mut anc: Vec<tree_sitter::Node> = vec![];
    let mut cur = root.walk();
    'walk: loop {
        let n = cur.node();
        if cpp && let Some(c) = cpp_class_context(n, src) {
            classes.push((anc.len(), c));
        }
        if cpp
            && let Some(c) = classes.last()
            && (n.kind() == "this"
                || is_ident(n.kind())
                    && anc.last().is_some_and(|p| p.kind() == "field_expression" && p.named_child(0).is_some_and(|f| f.id() == n.id())))
        {
            recv_class.insert(n.start_byte(), c.1.clone());
        }
        if cpp
            && n.kind() == "type_identifier"
            && anc.last().is_some_and(|p| {
                matches!(
                    p.kind(),
                    "type_parameter_declaration" | "optional_type_parameter_declaration" | "variadic_type_parameter_declaration"
                )
            })
            && let Ok(t) = n.utf8_text(src)
        {
            tparams.insert(t.to_string());
        }
        if is_scope(n.kind(), cfam) {
            let parent = open.last().map_or(0, |o| o.1);
            scopes.push((parent, HashSet::new()));
            types.push(HashMap::new());
            open.push((anc.len(), scopes.len() - 1));
        }
        let scope = open.last().map_or(0, |o| o.1);
        if is_ident(n.kind())
            && let Ok(name) = n.utf8_text(src)
        {
            // At file level: not a prototype's parameter, nor a Python module-level assignment
            // (that is a definition).
            let file_ok = scope != 0
                || !anc.iter().any(|a| PARAM_LISTS.contains(&a.kind()))
                    && !anc.last().is_some_and(|p| p.kind() == "assignment" && anc.iter().any(|a| a.kind() == "module"));
            if file_ok && !(cfam && c_foreign_param(&anc)) && declares(n, &anc, src) {
                scopes[scope].1.insert(name.to_string());
                if cfam
                    && let Some(d) =
                        anc.iter().rev().find(|a| matches!(a.kind(), "parameter_declaration" | "declaration" | "for_range_loop"))
                    && let Some(t) = if c_lang { c_type_name(*d, src) } else { cpp_declared_type(*d, n, &anc, src) }
                {
                    let e = types[scope].entry(name.to_string()).or_insert_with(|| t.clone());
                    if *e != t {
                        *e = C_TYPE_CONFLICT.to_string();
                    }
                }
            }
            if let Some(p) = anc.last()
                && (VALUE_SLOTS.contains(&p.kind())
                    || RECEIVER_PARENTS.contains(&p.kind()) && p.named_child(0).is_some_and(|f| f.id() == n.id())
                    // C: the base of `(*pp)->f()` and `arr[i]->f()`, for its declared type.
                    || cfam
                        && matches!(p.kind(), "pointer_expression" | "subscript_expression")
                        && p.child_by_field_name("argument").is_some_and(|f| f.id() == n.id())
                    || matches!(p.kind(), "call" | "call_expression")
                        && p.child_by_field_name("function").is_some_and(|f| f.id() == n.id()))
            {
                uses.push((n.start_byte(), scope, name.to_string()));
            }
        }
        if cur.goto_first_child() {
            anc.push(n);
            continue;
        }
        if n.is_named()
            && let Some(p) = anc.last()
        {
            parents.insert(n.id(), *p);
        }
        loop {
            // Leaving the node at depth `anc.len()`.
            if open.last().is_some_and(|o| o.0 == anc.len()) {
                open.pop();
            }
            if classes.last().is_some_and(|o| o.0 == anc.len()) {
                classes.pop();
            }
            if cur.goto_next_sibling() {
                continue 'walk;
            }
            if !cur.goto_parent() {
                break 'walk;
            }
            anc.pop();
        }
    }
    let (mut locals, mut file_locals) = (HashSet::new(), HashMap::new());
    let mut recv_types = HashMap::new();
    for (pos, mut sc, name) in uses {
        loop {
            if scopes[sc].1.contains(&name) {
                if let Some(t) = types[sc].get(&name).filter(|t| *t != C_TYPE_CONFLICT) {
                    recv_types.insert(pos, t.clone());
                }
                if sc == 0 {
                    file_locals.insert(pos, name);
                } else {
                    locals.insert(pos);
                }
                break;
            }
            if sc == 0 {
                break;
            }
            sc = scopes[sc].0;
        }
    }
    if cpp {
        recv_types.retain(|_, t| !cpp_tparam(t, &tparams));
    }
    Walk { locals, file_locals, parents, recv_types, recv_class, tparams }
}

/// Is identifier `x` in a declaring position (a parameter or variable name, not a use)?
/// `anc` holds its ancestors, the parent last.
fn declares(x: tree_sitter::Node, anc: &[tree_sitter::Node], src: &[u8]) -> bool {
    let is_field = |p: tree_sitter::Node, field: &str, c: tree_sitter::Node| p.child_by_field_name(field).is_some_and(|y| y.id() == c.id());
    let mut c = x;
    // Passed through a C function declarator: a function pointer parameter, or a prototype.
    let mut via_fn_declarator = false;
    for i in (0..anc.len()).rev() {
        let p = anc[i];
        match p.kind() {
            // Python and JS parameters (`*args`, `...rest` pass through their patterns).
            "parameters" | "lambda_parameters" | "formal_parameters" => return true,
            "default_parameter" | "typed_default_parameter" => return is_field(p, "name", c),
            "typed_parameter" => return !is_field(p, "type", c),
            "assignment" | "for_statement" | "for_in_statement" | "for_in_clause" | "assignment_pattern" => {
                return is_field(p, "left", c);
            }
            // Python `with ... as x`, `except E as x`, walrus `(x := ...)`.
            "as_pattern_target" => return true,
            "except_clause" => return p.named_child(0).is_some_and(|f| f.id() != c.id()),
            "named_expression" => return is_field(p, "name", c),
            // JS declarators, except a name bound to a function (that is a definition).
            // JS declarators, except a name bound to a function (a definition) or one taken from
            // `require(...)` (an import).
            "variable_declarator" => {
                let value = p.child_by_field_name("value");
                let fn_value = value.is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression" | "function" | "class"));
                let required = value.is_some_and(|v| {
                    v.kind() == "call_expression"
                        && v.child_by_field_name("function").is_some_and(|f| f.utf8_text(src).is_ok_and(|t| t == "require"))
                });
                return is_field(p, "name", c) && !fn_value && !required;
            }
            "required_parameter" | "optional_parameter" => return is_field(p, "pattern", c),
            "arrow_function" => return is_field(p, "parameter", c),
            "catch_clause" => return is_field(p, "parameter", c),
            "pair_pattern" => {
                if !is_field(p, "value", c) {
                    return false;
                }
                c = p;
            }
            "pattern_list"
            | "tuple_pattern"
            | "list_pattern"
            | "array_pattern"
            | "object_pattern"
            | "rest_pattern"
            | "list_splat_pattern"
            | "dictionary_splat_pattern" => c = p,
            // Go: `name := ...`, `for name := range ...`, `case name := <-ch`,
            // `switch name := x.(type)` (the left-hand list).
            "expression_list" => {
                return i > 0 && {
                    let g = anc[i - 1];
                    match g.kind() {
                        "short_var_declaration" | "range_clause" | "receive_statement" => is_field(g, "left", p),
                        "type_switch_statement" => is_field(g, "alias", p),
                        _ => false,
                    }
                };
            }
            "parameter_declaration" | "variadic_parameter_declaration" | "optional_parameter_declaration" => {
                return !is_field(p, "type", c) && !is_field(p, "value", c);
            }
            "var_spec" | "const_spec" | "declaration" => {
                return !via_fn_declarator && !is_field(p, "type", c) && !is_field(p, "value", c);
            }
            // C/C++: a declarator wraps the name (`*p`, `a[4]`, `x = 1`, `(*fn)(int)`).
            "for_range_loop" => return is_field(p, "declarator", c),
            "pointer_declarator"
            | "array_declarator"
            | "parenthesized_declarator"
            | "reference_declarator"
            | "init_declarator"
            | "function_declarator" => {
                // A C++ reference declarator names its declarator by position, not by field.
                if !is_field(p, "declarator", c) && !matches!(p.kind(), "parenthesized_declarator" | "reference_declarator") {
                    return false;
                }
                via_fn_declarator |= p.kind() == "function_declarator";
                c = p;
            }
            _ => return false,
        }
    }
    false
}

/// Parse one file and extract definitions and references.
pub fn extract(l: &Lang, src: &[u8]) -> Parsed {
    let mut out = Parsed::default();
    let Some(q) = l.query() else { return out };
    let mut parser = Parser::new();
    if parser.set_language(&l.language()).is_err() {
        return out;
    }
    let Some(tree) = parser.parse(src, None) else { return out };
    let names = q.capture_names();
    let mut cur = QueryCursor::new();
    let mut it = cur.matches(q, tree.root_node(), src);
    let text = |n: tree_sitter::Node| n.utf8_text(src).unwrap_or("").to_string();
    let c_lang = l.name == "c";
    let cpp = l.name == "cpp";
    let walk = walk_tree(tree.root_node(), src, c_lang, cpp);
    let c_family = l.family == "c";
    // Python `@overload` stubs and property setters/deleters, by start byte: (name, scope node).
    let mut stubs: HashMap<usize, (String, usize)> = HashMap::new();
    while let Some(m) = it.next() {
        let mut name_node = None;
        let mut outer: Option<(tree_sitter::Node, &str)> = None;
        let mut container = None;
        for cap in m.captures() {
            let cn = names[cap.index as usize];
            if cn == "name" {
                name_node = Some(cap.node);
            } else if cn == "container" || cn == "local.scope" {
                container = Some(text(cap.node));
            } else if cn.starts_with("definition.") || cn.starts_with("reference.") {
                outer = Some((cap.node, cn));
            }
        }
        let Some((node, cn)) = outer else { continue };
        if name_node.is_none() && c_lang && matches!(node.kind(), "function_definition" | "declaration") {
            name_node = c_fn_name(node);
        }
        let name = match name_node {
            Some(n) => text(n),
            None if l.name == "kotlin" && node.kind() == "companion_object" => "Companion".to_string(),
            None => continue,
        };
        let name = name.trim().trim_matches('"').to_string();
        if name.is_empty() || name.len() > 200 {
            continue;
        }
        let nn = name_node.unwrap_or(node);
        if let Some(k) = cn.strip_prefix("definition.") {
            let mut kind = k.to_string();
            if kind == "class" {
                // Grammars that share one node for class/interface/enum (Kotlin, Swift).
                let mut w = node.walk();
                for ch in node.children(&mut w) {
                    if !ch.is_named() && matches!(ch.kind(), "interface" | "enum" | "struct" | "protocol" | "extension") {
                        kind = ch.kind().to_string();
                        break;
                    }
                }
            }
            // Grammars that keep a body beside its signature (Dart): extend over the body.
            let mut last = node;
            if let Some(b) = node.next_named_sibling()
                && matches!(b.kind(), "function_body" | "block")
                && node.parent().is_some_and(|p| p.kind() != "function_definition")
            {
                last = b;
            }
            // A class field holding a function (`handleClick = () => {}`) is a method, and so is
            // a C/C++ function-pointer member (`void (*irq_mask)(struct irq_data *)`).
            let value_fn = kind == FIELD
                && node
                    .child_by_field_name("value")
                    .or_else(|| node.child_by_field_name("right"))
                    .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression" | "function" | "lambda"));
            let field_fn = value_fn || kind == FIELD && c_family && c_fn_pointer(nn, node);
            // A field spans its name: one declaration may name several (`A, B int`). One holding
            // a function spans its body, so the calls inside belong to it.
            let (node, last) = if kind == FIELD && !value_fn { (nn, nn) } else { (node, last) };
            let mut ty = if c_family && matches!(kind.as_str(), "field" | "function" | "type") {
                let d = outer.map_or(node, |o| o.0);
                if cpp {
                    d.child_by_field_name("type").and_then(|t| cpp_type(t, src)).map(|t| cpp_with_ns(t, d, src))
                } else {
                    c_type_name(d, src)
                }
            } else {
                None
            };
            let (td, tk) =
                if c_family && !field_fn { c_decl_shape(&kind, nn, outer.map_or(node, |o| o.0), ty.as_deref(), cpp) } else { (None, None) };
            // A function typedef's declared type is its return type, no alias.
            if td.is_some() {
                ty = None;
            }
            let proto = cpp && !field_fn && kind == "method" && node.kind() == "field_declaration";
            let virt = cpp && !field_fn && kind == "method" && cpp_virtual(node);
            let dyn_bases = l.family == "python" && kind == "class" && py_dyn_bases(node);
            let inline_ns = cpp && node.kind() == "namespace_definition" && {
                let mut w = node.walk();
                node.children(&mut w).any(|c| c.kind() == "inline")
            };
            if field_fn {
                kind = "method".into();
            }
            if l.family == "python"
                && let Some(scope) = python_stub(node, src)
            {
                stubs.insert(node.start_byte(), (name.clone(), scope));
            }
            let mut w = node.walk();
            let stat = node.children(&mut w).any(|ch| ch.kind() == "storage_class_specifier" && text(ch) == "static");
            out.defs.push(Def {
                kind,
                name,
                line: nn.start_position().row as u32 + 1,
                end_line: last.end_position().row as u32 + 1,
                start: node.start_byte(),
                end: last.end_byte(),
                container: None,
                container_name: container,
                decl: node.kind() == "declaration",
                stat,
                field_fn,
                ty,
                td,
                tk,
                proto,
                virt,
                dyn_bases,
                inline_ns,
            });
        } else if let Some(k) = cn.strip_prefix("reference.") {
            // A value in an argument list, keyed literal or initializer (`f(name)`, `Name: name`,
            // `.size = size`) that names a parameter or local is no function reference; nor is a
            // C constant or macro (`elog(ERROR, ...)`).
            // A call of a parameter or local (`def load(parse): parse(s)`) calls no definition.
            if matches!(ref_kind(k), "ref" | "call") && walk.locals.contains(&nn.start_byte())
                || ref_kind(k) == "ref"
                    && c_family
                    && name.chars().any(|ch| ch.is_ascii_uppercase())
                    && !name.chars().any(|ch| ch.is_lowercase())
            {
                continue;
            }
            let parent = walk.parents.get(&nn.id()).copied().or_else(|| nn.parent());
            let kind = ref_kind(k);
            let recv = kind != "import" && has_receiver(nn, parent);
            let rv = if recv { receiver_name(nn, parent, src) } else { None };
            let recv_local = rv.as_ref().is_some_and(|(_, at)| walk.locals.contains(at));
            let via = if kind == "import" { import_source(nn, &walk.parents, src) } else { rv.map(|x| x.0) };
            let rtype = match (c_lang, kind, parent) {
                (true, "call", Some(p)) if recv && p.kind() == "field_expression" => c_chain(p, &walk, src, 0),
                (true, "ref", Some(p)) => c_slot(nn, p, &walk, src),
                (false, "call", Some(p)) if cpp && recv && p.kind() == "field_expression" => {
                    cpp_chain(p, &walk, src, 0).map(|t| cpp_with_ns(t, p, src))
                }
                _ => None,
            };
            out.refs.push(Ref {
                kind,
                name,
                line: nn.start_position().row as u32 + 1,
                caller: None,
                recv,
                via,
                recv_local,
                rtype,
                pos: nn.start_byte(),
            });
        }
    }
    // A file-level local hides an argument only when the file defines nothing of that name
    // (`main = click.command(main)` rebinds a definition).
    if !walk.file_locals.is_empty() {
        let defined: HashSet<&str> = out.defs.iter().map(|d| d.name.as_str()).collect();
        let hidden: HashSet<usize> = walk.file_locals.iter().filter(|(_, n)| !defined.contains(n.as_str())).map(|(pos, _)| *pos).collect();
        out.refs.retain(|r| !matches!(r.kind, "ref" | "call") || !hidden.contains(&r.pos));
    }
    // A stub beside a definition of the same name in the same scope is not a symbol of its
    // own: calls reach the one definition with certainty.
    if !stubs.is_empty() {
        let real: HashSet<(String, usize)> = out
            .defs
            .iter()
            .filter(|d| !stubs.contains_key(&d.start))
            .filter_map(|d| {
                let n = tree.root_node().descendant_for_byte_range(d.start, d.start)?;
                Some((d.name.clone(), python_scope(n)?))
            })
            .collect();
        out.defs.retain(|d| stubs.get(&d.start).is_none_or(|k| !real.contains(k)));
    }
    nest(&mut out);
    out
}

/// The scope a Python definition sits in: the block (or module) holding it, past its
/// decorators.
fn python_scope(n: tree_sitter::Node) -> Option<usize> {
    let mut cur = n;
    for _ in 0..4 {
        if cur.kind() == "function_definition" {
            let up = cur.parent()?;
            let up = if up.kind() == "decorated_definition" { up.parent()? } else { up };
            return Some(up.id());
        }
        cur = cur.parent()?;
    }
    None
}

/// For a Python function decorated `@overload` (`typing.overload`) or `@x.setter` /
/// `@x.deleter`: the scope it sits in. None for any other definition.
fn python_stub(def: tree_sitter::Node, src: &[u8]) -> Option<usize> {
    let dd = def.parent().filter(|p| p.kind() == "decorated_definition")?;
    let mut w = dd.walk();
    let stub = dd.children(&mut w).filter(|c| c.kind() == "decorator").any(|c| {
        let t = c.utf8_text(src).unwrap_or("").trim_start_matches('@').trim();
        t == "overload" || t.ends_with(".overload") || t.ends_with(".setter") || t.ends_with(".deleter")
    });
    if stub { python_scope(def) } else { None }
}

/// The module an imported name comes from: JS `import { X } from "./m"` -> `./m`, Python
/// `from pkg.core import X` -> `pkg.core`. None for other import forms.
fn import_source(n: tree_sitter::Node, parents: &HashMap<usize, tree_sitter::Node>, src: &[u8]) -> Option<String> {
    let mut cur = n;
    for _ in 0..5 {
        let p = parents.get(&cur.id()).copied().or_else(|| cur.parent())?;
        match p.kind() {
            "import_statement" => {
                let s = p.child_by_field_name("source")?.utf8_text(src).ok()?;
                return Some(s.trim_matches(['"', '\'', '`']).to_string());
            }
            "import_from_statement" => return p.child_by_field_name("module_name")?.utf8_text(src).ok().map(str::to_string),
            // Go: the alias (`ilog "example.com/m/internal/log"`).
            "import_spec" => return p.child_by_field_name("name")?.utf8_text(src).ok().map(str::to_string),
            _ => cur = p,
        }
    }
    None
}

/// Dedupe definitions, link each to its innermost enclosing definition, and refs to theirs.
fn nest(p: &mut Parsed) {
    p.defs.sort_by_key(|d| (d.start, std::cmp::Reverse(d.end)));
    // Same name on the same line from two patterns (declaration vs definition): keep the widest,
    // and an explicit container name from either.
    let mut first: HashMap<(String, u32), usize> = HashMap::new();
    let mut keep = vec![true; p.defs.len()];
    for (i, k) in keep.iter_mut().enumerate() {
        match first.get(&(p.defs[i].name.clone(), p.defs[i].line)) {
            Some(&j) => {
                *k = false;
                if p.defs[j].container_name.is_none() {
                    p.defs[j].container_name = p.defs[i].container_name.take();
                }
            }
            None => {
                first.insert((p.defs[i].name.clone(), p.defs[i].line), i);
            }
        }
    }
    let mut k = keep.iter();
    p.defs.retain(|_| *k.next().unwrap_or(&true));
    link(&mut p.defs);
    // Locals (variables inside a function body) are not symbols.
    let local: Vec<bool> = p
        .defs
        .iter()
        .map(|d| {
            matches!(d.kind.as_str(), "property" | "variable" | "constant")
                && d.container.is_some_and(|c| is_callable_kind(&p.defs[c].kind))
                // A field of a type declared inside a function is local too.
                || d.kind == FIELD && {
                    let mut up = d.container;
                    let mut inside = false;
                    for _ in 0..64 {
                        let Some(c) = up else { break };
                        if is_callable_kind(&p.defs[c].kind) {
                            inside = true;
                            break;
                        }
                        up = p.defs[c].container;
                    }
                    inside
                }
        })
        .collect();
    if local.iter().any(|x| *x) {
        let mut l = local.iter();
        p.defs.retain(|_| !*l.next().unwrap_or(&false));
        link(&mut p.defs);
    }
    for i in 0..p.defs.len() {
        let in_type = p.defs[i].container.is_some_and(|c| is_type_kind(&p.defs[c].kind)) || p.defs[i].container_name.is_some();
        if p.defs[i].kind == "function" && in_type {
            p.defs[i].kind = "method".into();
        }
    }
    // Refs: innermost definition whose byte range contains the reference (definitions are sorted).
    // One reference per name and position: a supertype (`class A(Base)`) also matches the
    // argument pattern, and keeps its own kind.
    let rank = |k: &str| match k {
        "impl" => 0,
        "call" => 1,
        "ref" => 2,
        _ => 3,
    };
    p.refs.sort_by_key(|r| (r.pos, rank(r.kind)));
    p.refs.dedup_by(|a, b| a.pos == b.pos && a.name == b.name);
    let mut stack: Vec<usize> = vec![];
    let mut di = 0;
    for r in p.refs.iter_mut() {
        while di < p.defs.len() && p.defs[di].start <= r.pos {
            while stack.last().is_some_and(|&t| p.defs[t].end <= p.defs[di].start) {
                stack.pop();
            }
            stack.push(di);
            di += 1;
        }
        while stack.last().is_some_and(|&t| p.defs[t].end <= r.pos) {
            stack.pop();
        }
        r.caller = stack.last().copied();
    }
}

fn link(defs: &mut [Def]) {
    let mut stack: Vec<usize> = vec![];
    for i in 0..defs.len() {
        while stack.last().is_some_and(|&t| defs[t].end <= defs[i].start) {
            stack.pop();
        }
        defs[i].container = stack.last().copied();
        stack.push(i);
    }
}

pub fn content_hash(b: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub files: usize,
    pub parsed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub symbols: usize,
    pub refs: usize,
    /// References scored this run: all of them on a cold index, the change's on an update.
    pub rescored: usize,
    /// References with a target, among those scored.
    pub resolved: usize,
    pub ms: u128,
}

enum Work {
    Touch(String, i64, i64),
    Parse {
        path: String,
        lang: &'static str,
        hash: String,
        size: i64,
        mtime: i64,
        parsed: Parsed,
        texts: Vec<String>,
        words: String,
    },
    /// An unchanged file whose symbols' body text is written again (a backfill or a rebuild).
    Body {
        path: String,
        size: i64,
        mtime: i64,
        texts: Vec<String>,
        words: String,
    },
}

/// Most source characters (doc comment first, then the definition) the body arm reads per symbol.
pub const BODY_CHARS: usize = 1600;

/// Does a trimmed line belong to a comment block or an attribute above a definition?
fn is_doc_line(t: &str) -> bool {
    ["//", "#", "/*", "*", "--", ";", "@"].iter().any(|p| t.starts_with(p))
}

/// Body-arm text for definition `d` of `src`: its leading comment block, then its own source,
/// at most [`BODY_CHARS`] characters, every identifier followed by its parts (see [`split_words`]).
pub fn body_text(src: &[u8], d: &Def) -> String {
    let (start, end) = (d.start.min(src.len()), d.end.min(src.len()));
    let before = String::from_utf8_lossy(&src[..start]);
    let mut doc: Vec<&str> = vec![];
    // Skip the partial line the definition starts on (indentation, `pub `), then walk upwards.
    let mut lines = before.lines().rev();
    if !before.is_empty() && !before.ends_with('\n') {
        lines.next();
    }
    for l in lines.take(60) {
        let t = l.trim();
        if t.is_empty() || !is_doc_line(t) {
            break;
        }
        doc.push(t);
    }
    doc.reverse();
    let mut raw = doc.join("\n");
    raw.push('\n');
    raw.push_str(&String::from_utf8_lossy(&src[start..end.max(start)]));
    let cut: String = raw.chars().take(BODY_CHARS).collect();
    split_words(&cut)
}

/// Words of `text` for the full-text index: each identifier as written, then its parts when it
/// has several (`fooBar foo bar`), and snake_case also as one squashed word (`foo_bar foobar`),
/// so a whole snake_case name is one term, as a camelCase one already is.
pub fn split_words(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for tok in text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|t| t.chars().any(char::is_alphanumeric)) {
        out.push_str(tok);
        out.push(' ');
        let parts = crate::search::ident_parts(tok);
        if parts.len() > 1 {
            if tok.contains('_') {
                out.push_str(&parts.concat());
                out.push(' ');
            }
            if tok.chars().any(|c| c.is_uppercase() || c.is_ascii_digit()) {
                out.push_str(&parts.join(" "));
                out.push(' ');
            }
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// Incrementally (re)index the project's code files from the inventory.
pub fn index_project(c: &mut Connection, p: &Project) -> Result<Stats> {
    index_project_with(c, p, None)
}

/// [`index_project`], told which files the inventory pass of this run changed (a big tree with a
/// walk record). It then reads only those, when it is known to have taken every earlier change
/// (see `walkcache::filegen`) and owes no backfill, re-score or fill; otherwise it reads all.
pub fn index_project_with(c: &mut Connection, p: &Project, delta: Option<&crate::inventory::Delta>) -> Result<Stats> {
    let t0 = std::time::Instant::now();
    let mut st = Stats::default();
    // The file generation this run takes (trees with a walk record only).
    let filegen: Option<i64> = c.query_row("SELECT v FROM meta WHERE k = 'files:' || ?1", [p.id], |r| r.get(0)).ok();
    let only: Option<&Vec<String>> = delta
        .filter(|d| {
            let taken: Option<i64> = c.query_row("SELECT v FROM meta WHERE k = 'tsfiles:' || ?1", [p.id], |r| r.get(0)).ok();
            taken == Some(d.filegen)
                && !db::reindex_pending(c, p.id)
                && !db::rescore_pending(c, p.id)
                && outbox::state(c, p.id).is_none_or(|s| s.1 != "filling")
                && c.query_row("SELECT 1 FROM swift_unit WHERE project_id = ?1 LIMIT 1", [p.id], |_| Ok(())).is_err()
        })
        .map(|d| &d.paths);
    // Inventory code files (tier 3) with a known grammar. Swift files the index store covers are skipped.
    let files: Vec<(String, i64, i64)> = if let Some(paths) = only {
        let mut q = c.prepare_cached(
            "SELECT f.path, COALESCE(f.size,0), COALESCE(f.mtime,0) FROM node f WHERE f.project_id = ?1 AND f.path = ?2 AND f.kind = 'file' AND f.tier >= 2",
        )?;
        let mut v = vec![];
        for path in paths {
            v.extend(q.query_map(params![p.id, path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.flatten());
        }
        v.into_iter()
            .filter(|(path, size, _): &(String, i64, i64)| {
                lang::for_path(path).is_some() && (*size as u64) <= MAX_FILE && !crate::tiers::is_denied_path(path)
            })
            .collect()
    } else {
        let mut q = c.prepare(
            "SELECT f.path, COALESCE(f.size,0), COALESCE(f.mtime,0) FROM node f WHERE f.project_id = ?1 AND f.kind = 'file' AND f.tier >= 2
             AND NOT (f.lang = 'swift' AND EXISTS (SELECT 1 FROM swift_rec r WHERE r.path = f.path))",
        )?;
        q.query_map([p.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .flatten()
            .filter(|(path, size, _): &(String, i64, i64)| {
                lang::for_path(path).is_some() && (*size as u64) <= MAX_FILE && !crate::tiers::is_denied_path(path)
            })
            .collect()
    };
    // Path order, not the directory listing's: new definitions get ids in that order, and ids
    // break ties between same-score candidates, so the answer is the same on every filesystem.
    let mut files = files;
    files.sort();
    let old: HashMap<String, (String, i64, i64)> = if let Some(paths) = only {
        let mut q = c.prepare_cached("SELECT path, hash, size, mtime FROM ts_file WHERE project_id = ?1 AND path = ?2")?;
        let mut m = HashMap::new();
        for path in paths {
            m.extend(q.query_map(params![p.id, path], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?.flatten());
        }
        m
    } else {
        let mut q = c.prepare("SELECT path, hash, size, mtime FROM ts_file WHERE project_id = ?1")?;
        q.query_map([p.id], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?.flatten().collect()
    };
    st.files = files.len();
    let now: HashSet<&str> = files.iter().map(|f| f.0.as_str()).collect();
    let removed: Vec<String> = old.keys().filter(|k| !now.contains(k.as_str())).cloned().collect();
    // A body backfill (or a requested rebuild) reads every file once; unchanged ones only have
    // their symbols' body text written again, so their ids and handles stay.
    // A derived search index's fill (see `outbox`) reads every file once too, and writes every
    // symbol's rows to its log. Only a fill the inventory pass of this run started counts.
    let fill = outbox::state(c, p.id).is_some_and(|s| s.1 == "filling");
    let reindex = db::reindex_pending(c, p.id) || fill;
    // Files an upgrade asked to parse again (empty hash): when they are most of the project, the
    // rewrite leaves many full-text segments behind, merged once at the end.
    let forced = old.values().filter(|o| o.0.is_empty()).count();
    let todo: Vec<&(String, i64, i64)> =
        files.iter().filter(|(path, size, mtime)| reindex || old.get(path).is_none_or(|o| o.1 != *size || o.2 != *mtime)).collect();
    let root = p.root.clone();
    let work: Vec<Work> = todo
        .par_iter()
        .filter_map(|(path, size, mtime)| {
            let l = lang::for_path(path)?;
            let bytes = std::fs::read(root.join(path)).ok()?;
            let hash = content_hash(&bytes);
            let same = old.get(path).is_some_and(|o| o.0 == hash);
            if same && !reindex {
                return Some(Work::Touch(path.clone(), *size, *mtime));
            }
            let parsed = extract(l, &bytes);
            let texts = parsed.defs.iter().map(|d| body_text(&bytes, d)).collect();
            let words = file_words(&bytes);
            if same {
                return Some(Work::Body { path: path.clone(), size: *size, mtime: *mtime, texts, words });
            }
            Some(Work::Parse { path: path.clone(), lang: l.name, hash, size: *size, mtime: *mtime, parsed, texts, words })
        })
        .collect();
    trace("ts parse", t0);
    let tx = c.transaction()?;
    let sink = Sink::open(&tx);
    let mut emitted: HashSet<i64> = HashSet::new();
    // A big cold run builds the name index once at the end: a sort, not millions of random inserts.
    // So does a run that re-parses most of a big project (an upgrade that re-parses a language).
    let reparsed = work.iter().filter(|w| matches!(w, Work::Parse { path, .. } if old.contains_key(path))).count();
    let bulk = work.len() > 5000 && (old.is_empty() || reparsed * 2 > old.len());
    if bulk {
        tx.execute_batch("DROP INDEX IF EXISTS ts_ref_name; DROP INDEX IF EXISTS ts_ref_rtype;")?;
    }
    // Re-parsing most of a big project: drop the old rows of every re-parsed file in one pass
    // per table, not file by file (the resolution after it is a full one).
    let pre_dropped = bulk && !old.is_empty();
    if pre_dropped {
        let paths: Vec<&str> =
            work.iter().filter_map(|w| if let Work::Parse { path, .. } = w { Some(path.as_str()) } else { None }).collect();
        drop_files_bulk(&tx, &sink, p.id, &paths)?;
    }
    // Scope for incremental resolution: changed paths, and names defined there before or after.
    let mut sc_paths: HashSet<String> = HashSet::new();
    let mut sc_names: HashSet<String> = HashSet::new();
    let full = old.is_empty() && only.is_none();
    let old_names = |path: &str, out: &mut HashSet<String>| -> Result<()> {
        let mut q =
            tx.prepare_cached("SELECT name FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'")?;
        out.extend(q.query_map(params![p.id, path], |r| r.get::<_, String>(0))?.flatten());
        Ok(())
    };
    let mut cchange = CChange::default();
    let is_c = |path: &str| lang::for_path(path).is_some_and(|l| l.family == "c") || is_header(path);
    let is_py = |path: &str| lang::for_path(path).is_some_and(|l| l.family == "python");
    for path in &removed {
        old_names(path, &mut sc_names)?;
        sc_paths.insert(path.clone());
        if !full && !bulk && is_py(path) {
            py_old_classes(&tx, p.id, path, &mut cchange)?;
        }
        if !full && !bulk && is_c(path) {
            cchange.add(&c_type_rows_stored(&tx, p.id, path)?, &HashSet::new());
        }
        sc_names.extend(drop_file(&tx, &sink, p.id, path)?);
        tx.execute("DELETE FROM ts_file WHERE project_id = ?1 AND path = ?2", params![p.id, path])?;
        st.removed += 1;
    }
    // Each file's word row, written after the loop in rowid order (see `store_words`).
    let mut words_rows: Vec<(String, String)> = Vec::new();
    for w in work {
        match w {
            Work::Touch(path, size, mtime) => {
                tx.execute(
                    "UPDATE ts_file SET size = ?3, mtime = ?4 WHERE project_id = ?1 AND path = ?2",
                    params![p.id, path, size, mtime],
                )?;
                st.unchanged += 1;
            }
            Work::Body { path, size, mtime, texts, words } => {
                tx.execute(
                    "UPDATE ts_file SET size = ?3, mtime = ?4 WHERE project_id = ?1 AND path = ?2",
                    params![p.id, path, size, mtime],
                )?;
                store_bodies(&tx, &sink, p.id, &path, &texts, fill.then_some(&mut emitted))?;
                words_rows.push((path, words));
                st.unchanged += 1;
            }
            Work::Parse { path, lang, hash, size, mtime, parsed, texts, words } => {
                if !full && !bulk && is_py(&path) {
                    py_old_classes(&tx, p.id, &path, &mut cchange)?;
                }
                if old.contains_key(&path) && !pre_dropped {
                    old_names(&path, &mut sc_names)?;
                    if !full && !bulk && is_c(&path) {
                        cchange.add(&c_type_rows_stored(&tx, p.id, &path)?, &c_type_rows(&parsed, is_cpp_path(&path)));
                    }
                    sc_names.extend(drop_file(&tx, &sink, p.id, &path)?);
                } else if !full && !bulk && is_c(&path) {
                    cchange.add(&HashSet::new(), &c_type_rows(&parsed, is_cpp_path(&path)));
                }
                sc_names.extend(parsed.defs.iter().map(|d| d.name.clone()));
                sc_paths.insert(path.clone());
                store_file(&tx, &sink, p.id, &path, lang, &parsed, &texts, fill.then_some(&mut emitted))?;
                tx.execute(
                    "INSERT OR REPLACE INTO ts_file(project_id, path, lang, hash, size, mtime) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![p.id, path, lang, hash, size, mtime],
                )?;
                words_rows.push((path, words));
                st.parsed += 1;
                st.symbols += parsed.defs.len();
                st.refs += parsed.refs.len();
            }
        }
    }
    words_rows.sort_by_cached_key(|(path, _)| word_rowid(p.id, path));
    for (path, words) in &words_rows {
        store_words(&tx, p.id, path, words)?;
    }
    drop(words_rows);
    if bulk {
        tx.execute_batch(
            "CREATE INDEX IF NOT EXISTS ts_ref_name ON ts_ref(project_id, name);
             CREATE INDEX IF NOT EXISTS ts_ref_rtype ON ts_ref(project_id, rtype) WHERE rtype IS NOT NULL;",
        )?;
    }
    trace("ts parse+store", t0);
    // A store upgraded past the ambiguity column re-scores every reference once.
    let rescore = db::rescore_pending(&tx, p.id);
    if st.parsed > 0 || st.removed > 0 || rescore {
        let r = resolve_scoped_c(&tx, p.id, (!full && !rescore && !bulk).then_some((&sc_paths, &sc_names)), &cchange)?;
        (st.rescored, st.resolved) = (r.rescored, r.resolved);
    }
    if rescore {
        db::rescore_done(&tx, p.id)?;
    }
    if fill {
        fill_rest(&tx, &sink, p.id, &emitted)?;
        outbox::fill_done(&tx, p.id)?;
    }
    if reindex {
        db::reindex_done(&tx, p.id)?;
    }
    if let Some(fg) = filegen {
        // Every file change up to this generation is taken (see `index_project_with`).
        tx.execute("INSERT OR REPLACE INTO meta(k, v) VALUES ('tsfiles:' || ?1, ?2)", params![p.id, fg])?;
    }
    if only.is_some() {
        st.files = tx.query_row("SELECT COUNT(*) FROM ts_file WHERE project_id = ?1", [p.id], |r| r.get::<_, i64>(0))? as usize;
    }
    trace("ts resolve", t0);
    let changed = st.parsed > 0 || st.removed > 0 || rescore || reindex;
    if changed {
        db::bump_project(&tx, p.id)?;
    }
    tx.commit()?;
    if changed {
        db::optimize(c, full);
        trace("ts optimize", t0);
    }
    if forced >= 200 && forced * 2 >= st.files {
        let _ = db::fts_optimize(c);
        trace("ts fts optimize after an upgrade", t0);
    }
    st.ms = t0.elapsed().as_millis();
    Ok(st)
}

pub(crate) fn trace(what: &str, t0: std::time::Instant) {
    if std::env::var_os("FOXDB_TRACE").is_some() {
        eprintln!("{what} {:?}", t0.elapsed());
    }
}

/// Drop a file's symbols and references. Returns the names of references elsewhere that store a
/// function in one of its members (`ts_ref.slot`): they must be scored again.
fn drop_file(tx: &Connection, sink: &Sink, pid: i64, path: &str) -> Result<Vec<String>> {
    let sel = "SELECT id FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'";
    let mut q = tx.prepare_cached(&format!(
        "SELECT name FROM ts_ref INDEXED BY ts_ref_slot WHERE slot != 0 AND slot IN ({sel}) AND path != ?2
         UNION SELECT name FROM ts_ref INDEXED BY ts_ref_slot WHERE slot != 0 AND slot IN ({}) AND path != ?2",
        sel.replacen("SELECT id", "SELECT -id", 1)
    ))?;
    let slotted: Vec<String> = q.query_map(params![pid, path], |r| r.get(0))?.flatten().collect();
    for t in [TBL_NODE, TBL_TRI, TBL_BODY] {
        sink.del_where(tx, pid, t, sel, &[&pid, &path])?;
    }
    // Prepared once per run: a re-parse of most of a big tree calls this for every file.
    for sql in [
        format!("DELETE FROM edge WHERE src IN ({sel})"),
        format!("DELETE FROM edge WHERE dst IN ({sel})"),
        format!("DELETE FROM fts_node WHERE rowid IN ({sel})"),
        format!("DELETE FROM fts_trigram WHERE rowid IN ({sel})"),
        format!("DELETE FROM fts_body WHERE rowid IN ({sel})"),
        format!("UPDATE ts_ref SET dst = 0 WHERE dst != 0 AND dst IN ({sel})"),
        "DELETE FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'".to_string(),
        "DELETE FROM ts_ref WHERE project_id = ?1 AND path = ?2".to_string(),
    ] {
        tx.prepare_cached(&sql)?.execute(params![pid, path])?;
    }
    tx.prepare_cached("DELETE FROM fts_word WHERE rowid = ?1")?.execute([word_rowid(pid, path)])?;
    Ok(slotted)
}

/// [`drop_file`] for many files at once, one statement per table over a temporary list of their
/// symbols. References into them are not looked up: the caller resolves everything again.
fn drop_files_bulk(tx: &Connection, sink: &Sink, pid: i64, paths: &[&str]) -> Result<()> {
    tx.execute_batch(
        "DROP TABLE IF EXISTS temp.gone_path; DROP TABLE IF EXISTS temp.gone;
         CREATE TEMP TABLE gone_path(path TEXT PRIMARY KEY) WITHOUT ROWID;
         CREATE TEMP TABLE gone(id INTEGER PRIMARY KEY);",
    )?;
    {
        let mut ins = tx.prepare("INSERT OR IGNORE INTO temp.gone_path(path) VALUES (?1)")?;
        for path in paths {
            ins.execute([path])?;
        }
    }
    tx.execute(
        "INSERT INTO temp.gone(id) SELECT id FROM node WHERE project_id = ?1 AND kind = 'symbol' AND source = 'treesitter'
           AND path IN temp.gone_path",
        [pid],
    )?;
    let sel = "SELECT id FROM temp.gone WHERE ?1 = ?1";
    for t in [TBL_NODE, TBL_TRI, TBL_BODY] {
        sink.del_where(tx, pid, t, sel, &[&pid])?;
    }
    tx.execute_batch(
        "DELETE FROM edge WHERE src IN temp.gone;
         DELETE FROM edge WHERE dst IN temp.gone;
         DELETE FROM fts_node WHERE rowid IN temp.gone;
         DELETE FROM fts_trigram WHERE rowid IN temp.gone;
         DELETE FROM fts_body WHERE rowid IN temp.gone;
         UPDATE ts_ref SET dst = 0 WHERE dst != 0 AND dst IN temp.gone;
         DELETE FROM node WHERE id IN temp.gone;",
    )?;
    tx.execute("DELETE FROM ts_ref WHERE project_id = ?1 AND path IN temp.gone_path", [pid])?;
    let mut w = tx.prepare_cached("DELETE FROM fts_word WHERE rowid = ?1")?;
    for path in paths {
        w.execute([word_rowid(pid, path)])?;
    }
    tx.execute_batch("DROP TABLE temp.gone_path; DROP TABLE temp.gone;")?;
    Ok(())
}

/// The `fts_word` rowid of a file: a fixed 63-bit FNV-1a hash of project and path, so a row is
/// found again without a lookup table (and never depends on the Rust version).
pub fn word_rowid(pid: i64, path: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in pid.to_le_bytes().iter().chain(path.as_bytes()) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    ((h >> 1) as i64).max(1)
}

/// A word as `fts_word` holds it: each capital becomes `~` and its lower case, so the
/// case-folding tokenizer still tells `State` from `state` (as `rg -w` does).
pub fn case_word(w: &str) -> String {
    let mut out = String::with_capacity(w.len() + 2);
    for ch in w.chars() {
        if ch.is_uppercase() {
            out.push('~');
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Every identifier of a file, once each, in first-seen order (comments and strings included,
/// as `rg -w` sees them), each through [`case_word`].
pub fn file_words(src: &[u8]) -> String {
    let text = String::from_utf8_lossy(src);
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = String::new();
    for w in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if w.is_empty() || w.len() > 100 || w.starts_with(|c: char| c.is_ascii_digit()) || !seen.insert(w) {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&case_word(w));
    }
    out
}

/// Write a file's `fts_word` row. A run writes its rows in ascending rowid order: FTS5 flushes
/// its pending terms to a new segment whenever a rowid is below the previous one, and these
/// rowids are path hashes, so file order flushed about every other row and a cold index of a
/// large tree went quadratic.
fn store_words(tx: &Connection, pid: i64, path: &str, words: &str) -> Result<()> {
    let id = word_rowid(pid, path);
    tx.execute("DELETE FROM fts_word WHERE rowid = ?1", [id])?;
    tx.execute("INSERT INTO fts_word(rowid, project_id, path, words) VALUES (?1,?2,?3,?4)", params![id, pid, path, words])?;
    Ok(())
}

/// Write the body-arm rows of an unchanged file's symbols again, found by their stable keys.
/// In a fill (`emitted` given) the rows only go to the derived index's log, all three of them:
/// SQLite's own full-text rows are already current.
fn store_bodies(
    tx: &Connection,
    sink: &Sink,
    pid: i64,
    path: &str,
    texts: &[String],
    mut emitted: Option<&mut HashSet<i64>>,
) -> Result<()> {
    let mut get =
        tx.prepare_cached("SELECT id, name, COALESCE(json_extract(meta, '$.parent') || '.' || name, name) FROM node WHERE key = ?1")?;
    let mut del = tx.prepare_cached("DELETE FROM fts_body WHERE rowid = ?1")?;
    let mut ins = tx.prepare_cached("INSERT INTO fts_body(rowid, name, sig, path, body) VALUES (?1,?2,?3,?4,?5)")?;
    for (i, text) in texts.iter().enumerate() {
        let row: Option<(i64, String, String)> =
            get.query_row([format!("t:{pid}:{path}#{i}")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok();
        let Some((id, name, q)) = row else { continue };
        let split = crate::inventory::split_ident(&name);
        if let Some(e) = emitted.as_deref_mut() {
            sink.put(tx, pid, TBL_NODE, id, &[&split, &q, path, ""])?;
            sink.put(tx, pid, TBL_TRI, id, &[path, &name])?;
            sink.put(tx, pid, TBL_BODY, id, &[&split, &q, path, text])?;
            e.insert(id);
            continue;
        }
        del.execute([id])?;
        ins.execute(params![id, split, q, path, text])?;
        sink.put(tx, pid, TBL_BODY, id, &[&split, &q, path, text])?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn store_file(
    tx: &Connection,
    sink: &Sink,
    pid: i64,
    path: &str,
    lang: &str,
    p: &Parsed,
    texts: &[String],
    mut emitted: Option<&mut HashSet<i64>>,
) -> Result<()> {
    let mut ins = tx.prepare_cached(
        "INSERT INTO node(project_id, kind, key, name, path, line, end_line, sig, lang, tier, source, meta, updated_at)
         VALUES (?1,'symbol',?2,?3,?4,?5,?6,?3,?7,3,'treesitter',?8,?9)",
    )?;
    let mut fts = tx.prepare_cached("INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1,?2,?3,?4,'')")?;
    let mut tri = tx.prepare_cached("INSERT INTO fts_trigram(rowid, path, name) VALUES (?1,?2,?3)")?;
    let mut body = tx.prepare_cached("INSERT INTO fts_body(rowid, name, sig, path, body) VALUES (?1,?2,?3,?4,?5)")?;
    let now = crate::db::now();
    let mut ids: Vec<i64> = Vec::with_capacity(p.defs.len());
    for (i, d) in p.defs.iter().enumerate() {
        let parent = p.container_of(i);
        let pid_ref = d.container.map(|c| ids[c]);
        let mut meta = serde_json::json!({"k": d.kind, "parent": parent, "pid": pid_ref, "ts": 1});
        if d.decl {
            meta["d"] = 1.into();
        }
        if d.stat {
            meta["s"] = 1.into();
        }
        if d.field_fn {
            meta["f"] = 1.into();
        }
        if let Some(t) = &d.ty {
            meta["t"] = t.as_str().into();
        }
        if let Some(n) = d.td {
            meta["td"] = n.into();
        }
        if let Some(n) = d.tk {
            meta["tk"] = n.into();
        }
        if d.proto {
            meta["b"] = 1.into();
        }
        if d.virt {
            meta["v"] = 1.into();
        }
        if d.dyn_bases {
            meta["db"] = 1.into();
        }
        if matches!(d.kind.as_str(), "class" | "struct" | "union")
            && let Some(q) = p.outside_name(i)
        {
            meta["cq"] = q.into();
        }
        // C: a member of a struct or union tag (not of an anonymous struct named by a typedef).
        if (d.kind == FIELD || d.field_fn)
            && let Some(c) = d.container
            && matches!(p.defs[c].kind.as_str(), "struct" | "class")
        {
            meta["ps"] = 1.into();
        }
        ins.execute(params![pid, format!("t:{pid}:{path}#{i}"), d.name, path, d.line, d.end_line, lang, meta.to_string(), now])?;
        let id = tx.last_insert_rowid();
        ids.push(id);
        let q = match &parent {
            Some(pn) => format!("{pn}.{}", d.name),
            None => d.name.clone(),
        };
        let split = crate::inventory::split_ident(&d.name);
        let text = texts.get(i).map(String::as_str).unwrap_or("");
        fts.execute(params![id, split, q, path])?;
        tri.execute(params![id, path, d.name])?;
        body.execute(params![id, split, q, path, text])?;
        if sink.on(pid) {
            sink.put(tx, pid, TBL_NODE, id, &[&split, &q, path, ""])?;
            sink.put(tx, pid, TBL_TRI, id, &[path, &d.name])?;
            sink.put(tx, pid, TBL_BODY, id, &[&split, &q, path, text])?;
            if let Some(e) = emitted.as_deref_mut() {
                e.insert(id);
            }
        }
    }
    let mut r = tx.prepare_cached(
        "INSERT INTO ts_ref(project_id, path, src, name, line, kind, recv, dst, via, rtype) VALUES (?1,?2,?3,?4,?5,?6,?7,0,?8,?9)",
    )?;
    for x in &p.refs {
        let src = x.caller.map(|i| ids[i]).unwrap_or(0);
        // `recv` 2: through a receiver that is a parameter or local.
        let recv = if x.recv_local { 2 } else { x.recv as i64 };
        r.execute(params![pid, path, src, x.name, x.line, x.kind, recv, x.via, x.rtype])?;
    }
    Ok(())
}

/// The rest of a fill: every symbol, session and note of project `pid` whose rows this run did
/// not write to the log, rebuilt from `node` the way their importers write them. A tree-sitter
/// symbol whose file could not be read again gets its names without body text.
fn fill_rest(tx: &Connection, sink: &Sink, pid: i64, emitted: &HashSet<i64>) -> Result<()> {
    let mut q = tx.prepare(
        "SELECT id, kind, source, name, COALESCE(path, ''), COALESCE(sig, ''), json_extract(meta, '$.parent'), meta
         FROM node WHERE project_id = ?1 AND kind IN ('symbol', 'session', 'memory')",
    )?;
    type Row = (i64, String, String, String, String, String, Option<String>, Option<String>);
    let rows: Vec<Row> = q
        .query_map([pid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
        .flatten()
        .collect();
    for (id, kind, source, name, path, sig, parent, meta) in rows {
        if emitted.contains(&id) {
            continue;
        }
        let split = crate::inventory::split_ident(&name);
        let meta: serde_json::Value = meta.and_then(|m| serde_json::from_str(&m).ok()).unwrap_or_default();
        match (kind.as_str(), source.as_str()) {
            ("symbol", "indexstore") => {
                // As `swiftimport` writes them: the declaration as signature, no body text.
                let ident = parent.as_ref().map(|pn| format!("{pn}.{name}")).unwrap_or_else(|| name.clone());
                sink.put(tx, pid, TBL_NODE, id, &[&split, &sig, &path, ""])?;
                sink.put(tx, pid, TBL_BODY, id, &[&split, &sig, &path, ""])?;
                sink.put(tx, pid, TBL_TRI, id, &[&path, &ident])?;
            }
            ("symbol", _) => {
                let q = parent.as_ref().map(|pn| format!("{pn}.{name}")).unwrap_or_else(|| name.clone());
                sink.put(tx, pid, TBL_NODE, id, &[&split, &q, &path, ""])?;
                sink.put(tx, pid, TBL_TRI, id, &[&path, &name])?;
                sink.put(tx, pid, TBL_BODY, id, &[&split, &q, &path, ""])?;
            }
            ("session", _) => {
                let s = |k: &str| meta[k].as_str().unwrap_or("").to_string();
                let anchors = meta["anchors"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                let title = if name == "(no prompt)" { String::new() } else { name.clone() };
                let body = format!("{title}\n{}\n{anchors}", s("last"));
                sink.put(tx, pid, TBL_NODE, id, &[&name, &s("harness"), &path, &body])?;
                sink.put(tx, pid, TBL_TRI, id, &[&path, &name])?;
            }
            ("memory", "note") => {
                let text = meta["text"].as_str().unwrap_or("");
                sink.put(tx, pid, TBL_NODE, id, &[&split, "note", &path, text])?;
                sink.put(tx, pid, TBL_TRI, id, &[&path, &name])?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// A candidate definition, with what scoring needs precomputed.
#[derive(Clone)]
struct Cand {
    id: i64,
    path: String,
    kind: String,
    /// Enclosing definition's id (0 at top level).
    pid: i64,
    /// Last segment of the container name.
    parent: String,
    family: &'static str,
    /// Declared inside a function body: visible only in its own file.
    local: bool,
    /// A C/C++ prototype (no body).
    decl: bool,
    /// C/C++ `static` outside a header: visible only in its own file.
    file_local: bool,
    /// See [`Def::field_fn`].
    field_fn: bool,
    /// See [`Def::proto`].
    proto: bool,
    /// The whole container name (`ns.Engine`).
    parent_full: String,
    /// See [`Def::virt`].
    virt: bool,
    /// C++: a class's name from outside, past inline namespaces (`meta.cq`).
    outside: Option<String>,
}

/// One reference row to score.
struct RefRow {
    rowid: i64,
    path: String,
    src: i64,
    name: String,
    kind: String,
    dst: i64,
    recv: bool,
    /// Through a receiver that is a parameter or local.
    recv_local: bool,
    amb: i64,
    /// Container of the calling definition (-1 when unknown).
    caller_parent: i64,
    /// Last segment of the calling definition's container name (Go: its receiver's type).
    caller_type: String,
    /// The calling definition and the definitions enclosing it, innermost first.
    enclosing: Vec<i64>,
    /// Last name of the receiver (`Evaluator` in `pl.Evaluator.Preempt()`).
    via: Option<String>,
    line: i64,
    /// See [`Ref::rtype`].
    rtype: Option<String>,
    /// The member a function is stored in (C), as last written.
    slot: i64,
    /// C++: the class a call's receiver is of, when its declared or inferred type says so.
    recv_ty: Option<String>,
    /// C++: `recv_ty` is the only class of its last name in the project.
    recv_unique: bool,
    /// C++: the receiver is known to be outside the project (see [`CppRecv::External`]).
    recv_ext: bool,
    /// C++: the receiver's type is in doubt (see [`CppRecv::Doubt`]).
    recv_doubt: bool,
}

/// A C symbol a type path walks through: a struct member (field or function pointer), a
/// typedef, or a function with its return type.
#[derive(Clone)]
struct CSym {
    id: i64,
    kind: String,
    /// The struct a member belongs to, in its namespace (`S:irq_chip`, or `T:name` for an
    /// anonymous struct named by a typedef).
    parent: String,
    ty: Option<String>,
    fnptr: bool,
    /// The whole container name (`ns.Engine` for a member of `ns::Engine`).
    full_parent: String,
    /// See [`Cand::outside`].
    outside: Option<String>,
}

/// C++: what a call's receiver is, from its type path.
#[derive(Clone)]
enum CppRecv {
    /// A project class, by its whole name, and whether it is the only class of its last name.
    Class(String, bool),
    /// Known to be outside the project: no project method is called.
    External,
    /// A qualified type naming no project class while classes of its last name exist (in other
    /// namespaces): the name-based pick stands, as a guess.
    Doubt,
    /// Cannot be told: the name-based guess stands.
    Unknown,
}

/// What a C type path names: the member's candidates and whether the receiver's type was known
/// (then the candidates are that type's members and no other).
struct CRes {
    ids: Vec<i64>,
    typed: bool,
}

/// Most typedef steps followed from a type name (`irq_chip_t` -> `irq_chip`).
const C_ALIAS_STEPS: usize = 4;

/// C symbols by name, for typing member calls. A full run holds every C symbol up front; a scoped
/// run reads a name's symbols from the store the first time it is asked for.
#[derive(Default)]
struct CTypes {
    /// Struct members by (struct, member name).
    member: HashMap<(String, String), Vec<CSym>>,
    /// Typedefs and functions by name: (kind, type).
    named: HashMap<String, Vec<(String, Option<String>)>>,
    /// Per member name: the types fields of that name have (at most two kept: one is an answer).
    field_tys: HashMap<String, Vec<Option<String>>>,
    /// Per member name: the function-pointer members of that name.
    fnptrs: HashMap<String, Vec<i64>>,
    /// C++: the classes, structs and unions of a name: their whole names (`ns.Engine`) and the
    /// names they are written with from outside (past inline namespaces).
    classes: HashMap<String, Vec<(String, String)>>,
    /// C++: names of methods (a call `f()` in a class may be one).
    methods: HashSet<String>,
    /// Names read from the store (scoped runs).
    loaded: HashSet<String>,
    all: bool,
}

impl CTypes {
    fn add(&mut self, name: &str, s: CSym) {
        if (s.kind == FIELD || s.fnptr) && !s.parent.is_empty() {
            if s.fnptr {
                self.fnptrs.entry(name.to_string()).or_default().push(s.id);
            } else {
                let v = self.field_tys.entry(name.to_string()).or_default();
                if v.len() < 2 && !v.contains(&s.ty) {
                    v.push(s.ty.clone());
                }
            }
            self.member.entry((s.parent.clone(), name.to_string())).or_default().push(s);
        } else if matches!(s.kind.as_str(), "type" | "function") {
            self.named.entry(name.to_string()).or_default().push((s.kind, s.ty));
        } else if matches!(s.kind.as_str(), "class" | "struct" | "union") {
            let full = if s.full_parent.is_empty() { name.to_string() } else { format!("{}.{name}", s.full_parent.replace("::", ".")) };
            let outside = s.outside.clone().unwrap_or_else(|| full.clone());
            self.classes.entry(name.to_string()).or_default().push((full, outside));
        } else if s.kind == "method" {
            self.methods.insert(name.to_string());
        }
    }

    fn load(&mut self, name: &str, fetch: &mut dyn FnMut(&str) -> Vec<CSym>) {
        if !self.all && self.loaded.insert(name.to_string()) {
            for s in fetch(name) {
                self.add(name, s);
            }
        }
    }

    /// `t` and the types it is a typedef of, nearest first: `T:irq_chip_t` -> `S:irq_chip`. A
    /// struct tag (`S:`) is no alias of anything. None when a typedef on the way names two
    /// different types (two definitions of one name): the type is unknown.
    fn aliases(&mut self, t: &str, fetch: &mut dyn FnMut(&str) -> Vec<CSym>) -> Option<Vec<String>> {
        let mut out = vec![t.to_string()];
        for _ in 0..C_ALIAS_STEPS {
            let cur = out.last().cloned().unwrap_or_default();
            let Some(name) = cur.strip_prefix("T:") else { break };
            self.load(name, fetch);
            let targets: HashSet<&String> =
                self.named.get(name).into_iter().flatten().filter(|(k, _)| k == "type").filter_map(|(_, t)| t.as_ref()).collect();
            if targets.len() > 1 {
                return None;
            }
            match targets.into_iter().next().cloned() {
                Some(n) if !out.contains(&n) => out.push(n),
                _ => break,
            }
        }
        Some(out)
    }

    /// Members named `name` of type `t` (through its typedefs). None when the type is unknown.
    fn members(&mut self, t: &str, name: &str, fetch: &mut dyn FnMut(&str) -> Vec<CSym>) -> Option<Vec<CSym>> {
        self.load(name, fetch);
        for t in self.aliases(t, fetch)? {
            if let Some(m) = self.member.get(&(t, name.to_string())) {
                return Some(m.clone());
            }
        }
        Some(vec![])
    }

    /// The type of field `name` read from a value of type `t`; with `t` unknown (or not found),
    /// the one type every field of that name has. A struct defined twice whose field of that
    /// name has two types gives none.
    fn field_type(&mut self, t: Option<&str>, name: &str, fetch: &mut dyn FnMut(&str) -> Vec<CSym>) -> Option<String> {
        if let Some(t) = t {
            let m = self.members(t, name, fetch)?;
            if !m.is_empty() {
                let tys: HashSet<&Option<String>> = m.iter().map(|f| &f.ty).collect();
                return if tys.len() == 1 { m[0].ty.clone() } else { None };
            }
        }
        self.load(name, fetch);
        match self.field_tys.get(name).map(Vec::as_slice) {
            Some([one]) => one.clone(),
            _ => None,
        }
    }

    /// C++: the class type `t` (`S:Engine`, `S:ns::Engine`) names. An unqualified name must be
    /// the only class, struct or typedef of that name in the project; a qualified one the only
    /// class whose whole name ends in it, and one in a library namespace no project class
    /// matches is outside the project (`std::string`).
    fn cpp_class(&mut self, t: &str, fetch: &mut dyn FnMut(&str) -> Vec<CSym>) -> CppRecv {
        let Some(q) = t.strip_prefix("S:") else { return CppRecv::Unknown };
        // The namespaces around the use (see [`cpp_with_ns`]).
        let (q, around) = q.split_once('@').unwrap_or((q, ""));
        let q = q.trim_start_matches("::").replace("::", ".");
        if q.is_empty() || q.contains('<') {
            return CppRecv::Unknown;
        }
        let last = q.rsplit('.').next().unwrap_or("").to_string();
        self.load(&last, fetch);
        let fulls: Vec<(String, String)> = self.classes.get(&last).cloned().unwrap_or_default();
        let types = self.named.get(&last).into_iter().flatten().filter(|(k, _)| k == "type").count();
        let unique = fulls.len() == 1 && types == 0;
        if q.contains('.') {
            // The qualifier names the class as written from outside (past inline namespaces,
            // see [`Parsed::outside_name`]), from one of the namespaces around the use.
            // Skipping any other namespace is no match: `a::Engine` is not `a::detail::Engine`.
            // Looked up from the innermost namespace around the use outward, as C++ does: the
            // first scope where a class matches decides.
            let around: Vec<&str> = if around.is_empty() { vec![] } else { around.split('.').collect() };
            let mut hits: Vec<&(String, String)> = vec![];
            let mut at = 0;
            for k in (0..=around.len()).rev() {
                let want = if k == 0 { q.clone() } else { format!("{}.{q}", around[..k].join(".")) };
                hits = fulls.iter().filter(|(_, o)| *o == want).collect();
                if !hits.is_empty() {
                    at = k;
                    break;
                }
            }
            // A match only at global scope is sure only when no other class ends in the same
            // qualifier: a using-directive, the class of an out-of-line method, a class-scope
            // type or a namespace alias the index does not follow may name that one instead.
            let shadowable = at == 0 && fulls.iter().any(|(_, o)| o.ends_with(&format!(".{q}")));
            return match hits.as_slice() {
                [_] if shadowable => CppRecv::Doubt,
                [one] => CppRecv::Class(one.0.clone(), unique),
                [] if !fulls.is_empty() => CppRecv::Doubt,
                // A library namespace; any other may be an alias or a macro's the index does not
                // follow (`namespace c = b;`): the name-based pick stands.
                [] if types == 0 && CPP_LIB_NAMESPACES.contains(&q.split('.').next().unwrap_or("")) => CppRecv::External,
                _ => CppRecv::Unknown,
            };
        }
        if unique { CppRecv::Class(fulls[0].0.clone(), true) } else { CppRecv::Unknown }
    }

    /// C++: the class a call's receiver is of, from its type path (see [`cpp_chain`]): the base's
    /// class and the fields read on the way, each found in the class before it. Unknown when any
    /// step is in doubt: a field of a base class, an ambiguous type, two types for one field, a
    /// call `f()` where `f` may be a method. External for a wrapper's own member (`p.reset()` on
    /// a smart pointer) or a qualified type outside the project.
    fn cpp_receiver(&mut self, path: &str, fetch: &mut dyn FnMut(&str) -> Vec<CSym>) -> CppRecv {
        self.cpp_receiver_in(path, fetch).unwrap_or(CppRecv::Unknown)
    }

    fn cpp_receiver_in(&mut self, path: &str, fetch: &mut dyn FnMut(&str) -> Vec<CSym>) -> Option<CppRecv> {
        let segs: Vec<&str> = path.split('|').collect();
        let (base, steps) = segs.split_first()?;
        let steps: Vec<(char, &str)> = steps.iter().map(|s| s.chars().next().map(|op| (op, &s[1..]))).collect::<Option<_>>()?;
        let ((last_op, _), mids) = steps.split_last()?;
        let mut ty = if let Some(f) = base.strip_prefix("F:") {
            // A function's return type, else the class a constructor call `Engine()` makes; a
            // method of that name (called from inside its class) may be the one called.
            self.load(f, fetch);
            if self.methods.contains(f) {
                return None;
            }
            let rets: HashSet<&String> =
                self.named.get(f).into_iter().flatten().filter(|(k, _)| k == "function").filter_map(|(_, t)| t.as_ref()).collect();
            match rets.len() {
                0 => format!("S:{f}"),
                1 => rets.into_iter().next()?.clone(),
                _ => return None,
            }
        } else {
            (base.starts_with("S:") || base.starts_with("W:")).then(|| base.to_string())?
        };
        // Through `op`: a wrapper reaches its pointee only by `->`; by `.` its own members.
        let step = |ty: String, op: char| -> Result<String, CppRecv> {
            match ty.strip_prefix("W:") {
                Some(c) if op == '>' => Ok(format!("S:{c}")),
                Some(_) => Err(CppRecv::External),
                None => Ok(ty),
            }
        };
        for &(op, m) in mids {
            let t = match step(ty, op) {
                Ok(t) => t,
                Err(e) => return Some(e),
            };
            let CppRecv::Class(full, _) = self.cpp_class(&t, fetch) else { return None };
            let last = full.rsplit('.').next()?.to_string();
            let found = self.members(&format!("S:{last}"), m, fetch)?;
            let tys: HashSet<&Option<String>> = found.iter().filter(|f| !f.fnptr).map(|f| &f.ty).collect();
            ty = match tys.into_iter().collect::<Vec<_>>().as_slice() {
                [Some(t)] => t.clone(),
                _ => return None,
            };
        }
        Some(match step(ty, *last_op) {
            Ok(t) => self.cpp_class(&t, fetch),
            Err(e) => e,
        })
    }

    /// Resolve a type path (see [`c_chain`]) to the member it ends in.
    fn resolve(&mut self, path: &str, fetch: &mut dyn FnMut(&str) -> Vec<CSym>) -> CRes {
        let segs: Vec<&str> = path.split('|').collect();
        let Some((&last, rest)) = segs.split_last() else { return CRes { ids: vec![], typed: false } };
        let (base, mids) = rest.split_first().map_or(("", &[][..]), |(b, m)| (*b, m));
        let mut ty: Option<String> = if base.starts_with("T:") || base.starts_with("S:") {
            Some(base.to_string())
        } else if let Some(f) = base.strip_prefix("F:") {
            self.load(f, fetch);
            let rets: HashSet<&String> =
                self.named.get(f).into_iter().flatten().filter(|(k, _)| k == "function").filter_map(|(_, t)| t.as_ref()).collect();
            if rets.len() == 1 { rets.into_iter().next().cloned() } else { None }
        } else {
            None
        };
        for m in mids {
            ty = self.field_type(ty.as_deref(), m, fetch);
        }
        if let Some(t) = &ty {
            let m = self.members(t, last, fetch).unwrap_or_default();
            if !m.is_empty() {
                let mut ids: Vec<i64> = m.iter().filter(|s| s.fnptr).map(|s| s.id).collect();
                ids.sort_unstable();
                return CRes { ids, typed: true };
            }
        }
        self.load(last, fetch);
        let mut ids = self.fnptrs.get(last).cloned().unwrap_or_default();
        ids.sort_unstable();
        CRes { ids, typed: false }
    }
}

/// A C call through a member: its target among the member's candidates (lowest id first) and
/// the ambiguity, as [`score`] returns them. A receiver of known type names its struct's member;
/// an unknown one leaves a guess among a few same-name function-pointer members.
fn c_member_score(c: &CRes) -> (i64, i64) {
    match (c.ids.len(), c.typed) {
        (0, _) => (0, 0),
        (1, true) => (c.ids[0], 0),
        (n, true) => (c.ids[0], n as i64),
        (1, false) => (c.ids[0], 1),
        (n, false) if n <= GUESS_MAX => (c.ids[0], n as i64),
        _ => (0, 0),
    }
}

/// The member a function stored in it is linked to (`ts_ref.slot`): its id when the target's type
/// names one member, minus its id when that is a guess (two structs share the tag, or the
/// receiver's type is unknown and a few same-name members exist), 0 when nothing fits.
fn c_slot_id(c: &CRes) -> i64 {
    match c_member_score(c) {
        (0, _) => 0,
        (id, 0) => id,
        (id, _) => -id,
    }
}

/// Is `path` a C++ file?
fn is_cpp_path(path: &str) -> bool {
    lang::for_path(path).is_some_and(|l| l.name == "cpp")
}

/// Is `path` a C file (not C++ or Objective-C, where `x->f()` may call a method)?
fn is_c_path(path: &str) -> bool {
    lang::for_path(path).is_some_and(|l| l.name == "c")
}

/// Go: does import path `imp` name project directory `dir`? The import path ends with the
/// directory (`example.com/m/internal/log` and `internal/log`), or a domain-qualified import
/// path is a suffix of a deeper directory (`k8s.io/api/core/v1` under `staging/src/`). A
/// standard library path (`log`, `encoding/json`) never names a project directory.
pub(crate) fn go_import_names_dir(imp: &str, dir: &str) -> bool {
    let ends = |a: &str, b: &str| a == b || a.ends_with(b) && a[..a.len() - b.len()].ends_with('/');
    let domain = imp.split('/').next().is_some_and(|f| f.contains('.'));
    imp.contains('/') && ends(imp, dir) || domain && ends(dir, imp)
}

/// Go: is import `m` the package in `dir`? Its full path names the directory, or a
/// domain-qualified path (`example.com/m/models`) ends in the directory's last name. A standard
/// library path (`log`) is never a project package (`internal/log`).
fn go_import_is_dir(m: &Import, dir: &str) -> bool {
    let domain = m.raw.contains('/') && m.raw.split('/').next().is_some_and(|f| f.contains('.'));
    go_import_names_dir(&m.raw, dir) || domain && m.last == dir.rsplit('/').next().unwrap_or("")
}

/// `a/./b/../c` -> `a/c`.
fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = vec![];
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            x => out.push(x),
        }
    }
    out.join("/")
}

/// One import: its last segment (imported name, module or `*`) and, for dotted JVM imports, the
/// package as a directory path (`a.b.C` -> `a/b`), which must match the candidate's directory.
struct Import {
    last: String,
    pkg: String,
    /// The import as written (`example.com/m/internal/log`).
    raw: String,
    /// Go: the name the package is used by in the file (its alias, else its last segment).
    qual: String,
}

impl Import {
    fn parse(name: &str) -> Import {
        let n = name.trim().trim_end_matches(';');
        let (pkg, last) = n.rsplit_once(['.', '/', ':', '\\']).unwrap_or(("", n));
        Import {
            last: last.to_string(),
            pkg: pkg.trim_end_matches(':').replace(['.', ':'], "/"),
            raw: n.to_string(),
            qual: last.to_string(),
        }
    }
}

/// A file's imports, with their last segments in a set: a candidate is imported when one of its
/// names (own, container, file stem, Go package directory) is among them.
#[derive(Default)]
struct FileImports {
    all: Vec<Import>,
    lasts: HashSet<String>,
    /// JS and Python: the module each imported name comes from (`./errors`, `pkg.core`).
    sources: HashMap<String, String>,
    /// Python: modules imported whole (`import core`), so `core.plan()` names that module's `plan`.
    modules: HashSet<String>,
}

/// Where an explicitly imported name's module puts a candidate.
#[derive(PartialEq)]
enum Source {
    /// The module is the candidate's file or package.
    Match,
    /// Another project module.
    Other,
    /// A module outside the project (`some-lib`): no project candidate.
    External,
}

impl FileImports {
    fn push(&mut self, name: &str, source: Option<&str>, go: bool) {
        let mut i = Import::parse(name);
        if go {
            // Go: `source` is the import's alias (`ilog "example.com/m/internal/log"`).
            if let Some(alias) = source.filter(|a| !a.is_empty() && *a != "_" && *a != ".") {
                i.qual = alias.to_string();
                self.lasts.insert(alias.to_string());
                self.all.push(i);
                return;
            }
        } else if let Some(src) = source.filter(|x| !x.is_empty() && *x != name) {
            self.sources.insert(name.to_string(), src.to_string());
        } else {
            self.modules.insert(name.to_string());
        }
        self.lasts.insert(i.last.clone());
        self.all.push(i);
    }
    /// JS and Python: where the module `name` was imported from puts candidate `s`. None when the
    /// name was not imported by name with a known module.
    fn source(&self, fam: &str, name: &str, from: &str, s: &Cand) -> Option<Source> {
        let stem = s.path.rsplit_once('.').map_or(s.path.as_str(), |x| x.0);
        let src = match self.sources.get(name) {
            Some(src) => src,
            // `import core` then `core.plan()`: the module is the name itself.
            None if fam == "python" && self.modules.contains(name) => name,
            None => return None,
        };
        let module = match fam {
            "js" => {
                if !src.starts_with('.') {
                    return Some(Source::External);
                }
                normalize(&format!("{}/{src}", dir_of(from)))
            }
            "python" => {
                let dots = src.chars().take_while(|c| *c == '.').count();
                let rest = src[dots..].replace('.', "/");
                if dots == 0 {
                    rest
                } else {
                    let mut base = dir_of(from).to_string();
                    for _ in 1..dots {
                        base = dir_of(&base).to_string();
                    }
                    normalize(&format!("{base}/{rest}"))
                }
            }
            _ => return None,
        };
        // The module's file, its package (`pkg/core/__init__`, `dir/index`), or a module inside
        // that package (a re-export).
        let ends = |a: &str, b: &str| a == b || a.ends_with(b) && a[..a.len() - b.len()].ends_with('/');
        let inside = stem.starts_with(&format!("{module}/")) || stem.contains(&format!("/{module}/"));
        let hit = ends(stem, &module)
            || match fam {
                // A package re-exports what its modules define (`django.db.models` -> `fields/`).
                "python" => inside,
                _ => stem.ends_with("/index") && ends(dir_of(stem), &module),
            };
        // Python: a name imported from a module names that module's definition and no other;
        // a module the project does not hold (`json`) names none of the project's.
        Some(if hit {
            Source::Match
        } else if fam == "python" {
            Source::External
        } else {
            Source::Other
        })
    }
    fn hits(&self, fam: &str, name: &str, s: &Cand) -> bool {
        let dir = dir_of(&s.path);
        if fam == "jvm" {
            return self.all.iter().any(|m| {
                (m.last == "*" || m.last == name || m.last == s.parent)
                    && (m.pkg.is_empty() || dir.ends_with(&m.pkg) || !dir.is_empty() && m.pkg.ends_with(dir))
            });
        }
        self.lasts.contains(name)
            || self.lasts.contains(&s.parent)
            || fam != "go" && self.lasts.contains(stem_of(&s.path))
            || fam == "go" && self.imports_package(dir)
    }
    /// Go: a package in `dir` is imported: its full path names the directory, or a non-standard
    /// import path (`example.com/m/models`, with a domain) ends in the directory's last name. A
    /// standard library path (`log`) never imports a project package (`internal/log`).
    fn imports_package(&self, dir: &str) -> bool {
        self.all.iter().any(|m| go_import_is_dir(m, dir))
    }
    /// Go: `qual.Name` reaches a package in `dir`: the import used by that name is that package.
    fn qualifier_names_dir(&self, qual: &str, dir: &str) -> bool {
        self.all.iter().any(|m| m.qual == qual && go_import_is_dir(m, dir))
    }
    /// Go: an import path names directory `dir` itself (`example.com/pkg/b/stats` and
    /// `pkg/b/stats`, either one ending with the other), not only a package with its last name.
    fn names_dir(&self, dir: &str) -> bool {
        !dir.is_empty() && self.all.iter().any(|m| go_import_names_dir(&m.raw, dir))
    }
}

const UNIQUE_FALLBACK: &[&str] = &["c", "bash", "lua", "ruby", "php", "dart", "dotnet", "swift"];

/// Receivers that name the enclosing object itself.
pub(crate) const SELF_NAMES: &[&str] = &["self", "this", "cls", "super"];

/// Most same-score candidates a name-only guess picks among (receiver calls, C free functions).
const GUESS_MAX: usize = 3;

fn is_header(p: &str) -> bool {
    [".h", ".hh", ".hpp", ".hxx"].iter().any(|e| p.ends_with(e))
}

/// Leading directories two paths share.
pub(crate) fn common_dirs(a: &str, b: &str) -> i32 {
    dir_of(a).split('/').zip(dir_of(b).split('/')).take_while(|(x, y)| x == y && !x.is_empty()).count() as i32
}

/// How well a receiver name of three or more characters matches a type name: 20 for the same
/// name in any case, 12 when the type name ends with it as a word (`inputs` / `SessionInputs`).
pub(crate) fn via_match(via: &str, ty: &str) -> i32 {
    // One- and two-letter receivers (`t`, `ev`) say too little.
    if via.len() < 3 {
        return 0;
    }
    if via.eq_ignore_ascii_case(ty) {
        return 20;
    }
    let (v, t) = (via.to_lowercase(), ty.to_lowercase());
    let cut = ty.len().wrapping_sub(via.len());
    let word = ty.is_char_boundary(cut) && ty[cut..].starts_with(|c: char| c.is_uppercase());
    if via.len() >= 4 && t.ends_with(&v) && word { 12 } else { 0 }
}

/// Method names the standard libraries define on their own common types (JVM, Kotlin, C#, Dart,
/// JS, Rust, Python, Go, C++: `Object.toString`, `Vec::is_empty`): a call through a receiver with no scope signal is far more likely the
/// library's than a project's same-name method, so it takes no guess.
fn is_library_name(name: &str) -> bool {
    const NAMES: &str = "toString hashCode equals compareTo getClass getName getValue setValue getKey getId getType \
        isEmpty isNotEmpty forEach toList toSet toMap toArray valueOf getOrNull getOrDefault getOrElse putAll addAll \
        removeAll containsKey toLowerCase toUpperCase ToString GetHashCode Equals GetType CompareTo Dispose toJson fromJson \
        hasOwnProperty readLine writeLine is_empty to_vec to_string to_owned as_str as_ref as_mut into_iter iter_mut \
        as_slice is_some is_none is_ok is_err unwrap_or unwrap_or_default and_then or_else map_err ok_or to_lowercase \
        to_uppercase starts_with ends_with push_str extend_from_slice get_mut contains_key split_whitespace set_value \
        get_value get_name push_back emplace_back pop_back c_str";
    NAMES.split_whitespace().any(|n| n == name)
}

/// Under a test, sample or tooling directory: loses a tie against a candidate that is not.
fn is_aux(p: &str) -> bool {
    p.split('/').rev().skip(1).any(|d| matches!(d, "test" | "tests" | "testing" | "samples" | "examples" | "tools"))
}

pub(crate) fn dir_of(p: &str) -> &str {
    p.rsplit_once('/').map(|x| x.0).unwrap_or("")
}
fn stem_of(p: &str) -> &str {
    let f = p.rsplit('/').next().unwrap_or(p);
    f.split('.').next().unwrap_or(f)
}

/// Score one reference against its same-name candidates (in id order): same file > same type >
/// imported > same directory > unique elsewhere. Returns (target or 0, ambiguity): ambiguity is
/// the number of same-name candidates a name-only guess chose between, 0 when the target is
/// certain (same file, the only candidate, or the single best by scope).
fn score(r: &RefRow, cands: &[Cand], imp: Option<&FileImports>) -> (i64, i64) {
    let (d, a) = score_name(r, cands, imp);
    // C++: a receiver type in doubt never makes the name-based pick certain.
    if r.recv_doubt && d != 0 && a == 0 { (d, 2) } else { (d, a) }
}

fn score_name(r: &RefRow, cands: &[Cand], imp: Option<&FileImports>) -> (i64, i64) {
    let fam = lang::for_path(&r.path).map(|l| l.family).unwrap_or("");
    let call = r.kind == "call";
    // C++: a receiver outside the project calls no project method.
    if call && r.recv && r.recv_ext {
        return (0, 0);
    }
    // C++: a call through a receiver of a known class reaches that class's method. A class with
    // no method of the name (an inherited one, an external type) leaves the call a guess.
    if call
        && r.recv
        && let Some(t) = r.recv_ty.as_deref()
    {
        // The class's own methods: by its whole name, or by its last name (an out-of-line
        // definition `void Engine::start()`) when it is the only class of that name.
        let last = t.rsplit('.').next().unwrap_or(t);
        let own: Vec<&Cand> = cands
            .iter()
            .filter(|s| s.family == fam && is_callable_kind(&s.kind) && !s.field_fn)
            .filter(|s| s.parent_full == t || r.recv_unique && s.parent_full == last)
            .filter(|s| !(s.local && (s.path != r.path || !r.enclosing.contains(&s.pid))) && !(s.file_local && s.path != r.path))
            .collect();
        let defs = own.iter().filter(|s| !s.proto).count();
        // Overloads (two definitions, or two declarations without one) and virtual methods
        // (any of the name: an override may run instead) leave the name-based guess.
        let doubt = defs > 1 || defs == 0 && own.len() > 1 || cands.iter().any(|s| s.family == fam && s.virt);
        // The definition outside the class body stands for its declaration.
        if !doubt && let Some(best) = own.iter().find(|s| !s.proto).or(own.first()) {
            let mut classes: Vec<i64> = own.iter().map(|s| s.pid).filter(|&p| p != 0).collect();
            classes.sort_unstable();
            classes.dedup();
            return (best.id, if classes.len() > 1 { classes.len() as i64 } else { 0 });
        }
    }
    // C/C++: one global namespace. A prototype stands for its definition, so it is no candidate
    // while a visible definition of the name exists.
    let c_fam = fam == "c";
    // Languages where a receiver's type is unknown until run time.
    let dynamic = matches!(fam, "python" | "js");
    let c_file = c_fam && is_c_path(&r.path);
    // A function-pointer member is no definition of a free function of its name.
    let has_def =
        c_fam && cands.iter().any(|s| s.family == fam && !s.decl && !s.field_fn && (s.path == r.path || !s.file_local && !s.local));
    // A receiver named exactly like a candidate's type (`pl.Evaluator.Preempt()`) rules out a
    // same-file member of another type (a local interface with the same method).
    let via = if r.recv { r.via.as_deref() } else { None };
    let via_hit = via.is_some_and(|v| cands.iter().any(|s| s.family == fam && via_match(v, &s.parent) == 20));
    // Go: `pkg.Name` through an imported package's name reaches only a package of that name
    // (`io.Reader` is not this package's `Reader`).
    let pkg_qualified = fam == "go" && !r.recv_local && via.is_some_and(|v| imp.is_some_and(|i| i.lasts.contains(v)));
    // (score, id, candidates at that score, same file, member, score without the receiver match)
    let mut best = (0i32, 0i64, 0usize, false, false, 0i32);
    // The best candidate is in the module the receiver was imported from (`core.plan()`).
    let mut best_module = false;
    let mut eligible = 0i64;
    // The best candidate is a function-holding field reached without receiver evidence.
    let mut field_fn_guess = false;
    for s in cands {
        if !lang::interop(fam, s.family) || s.id == r.src && call && is_type_kind(&s.kind) {
            continue;
        }
        if via_hit && s.path == r.path && via.is_some_and(|v| via_match(v, &s.parent) < 20) {
            continue;
        }
        if pkg_qualified && via.is_some_and(|v| !imp.is_some_and(|i| i.qualifier_names_dir(v, dir_of(&s.path)))) {
            continue;
        }
        let member = s.pid != 0 || !s.parent.is_empty();
        // C: a function-pointer member is reached only through a receiver (see `c_member_score`).
        if s.field_fn && s.family == "c" && c_file {
            continue;
        }
        // JS and Python: a name imported from a module names that module's definition; one from
        // outside the project names none of the project's.
        let source = imp.and_then(|i| i.source(fam, via.unwrap_or(&r.name), &r.path, s));
        if source == Some(Source::External) && s.path != r.path {
            continue;
        }
        // Python and JS: a bare name (`f()`, `g(x)`) never reaches a method; that takes a receiver.
        if dynamic && !r.recv && s.kind == "method" {
            continue;
        }
        // Through a receiver that is not `self`/`this` nor named like the candidate's type
        // (`self.connection.close()`, `log(request.id)`), a same-file member of a dynamic
        // language is no more certain than one elsewhere.
        // A value (not a call) through any receiver inside the candidate's own type
        // (`connect(o.spider_opened)` in a method of that type) counts as its own too.
        let own_receiver = via.is_some_and(|v| SELF_NAMES.contains(&v) || via_match(v, &s.parent) == 20)
            || !call && s.pid != 0 && s.pid == r.caller_parent
            // Go: a method's receiver parameter or a local inside a method of the candidate's
            // type (`func (s *Server) a() { s.b() }`); a local of another type is no evidence.
            || fam == "go" && r.recv_local && !s.parent.is_empty() && s.parent == r.caller_type;
        // Through a receiver or qualifier, a same-file definition is certain only through its
        // own: a member through `self`/`this`/its type or its method's receiver, a free function
        // through nothing that names another module (`core.plan()`, `inner::plan()` are not this
        // file's `plan`; `self::plan`, `crate::plan` are).
        let same_file =
            s.path == r.path && (!r.recv || if member { own_receiver } else { via.is_none_or(|v| matches!(v, "self" | "crate" | "this")) });
        // A definition inside a function is visible only inside that function; C `static` ones
        // only in their own file.
        if s.local && (s.path != r.path || !r.enclosing.contains(&s.pid)) || s.path != r.path && s.file_local || has_def && s.decl {
            continue;
        }
        // A value read through a receiver (`f(res.status)`) is mostly a field: it names a member
        // only through `self`/`this`, an import, or the type itself (`Type::method`, `Base.run`).
        if !call
            && r.recv
            && !same_file
            && !own_receiver
            && (s.path == r.path || source.as_ref().map_or(!imp.is_some_and(|v| v.hits(fam, &r.name, s)), |x| *x != Source::Match))
            && via.is_none_or(|v| v != s.parent)
        {
            continue;
        }
        eligible += 1;
        let mut score = if same_file {
            100
        } else if source == Some(Source::Match) {
            70
        } else if source.is_none() && imp.is_some_and(|v| v.hits(fam, &r.name, s)) {
            // Go: a package-qualified call (`stats.Register()`) prefers the package its full
            // import path names; a method call on a value (`req.Header.Get()`) does not.
            if pkg_qualified && !member && imp.is_some_and(|v| v.names_dir(dir_of(&s.path))) { 65 } else { 60 }
        } else if dir_of(&s.path) == dir_of(&r.path) {
            40
        } else {
            10
        };
        if s.pid != 0 && s.pid == r.caller_parent {
            score += 15;
        }
        // A definition in an enclosing function (only those reach this far) shadows one outside.
        if s.local {
            score += 30;
        }
        // The receiver is named like the candidate's type (`inputs.render()` -> `Inputs`,
        // `pl.Evaluator.Preempt()` -> `Evaluator`, an embedded field), or like its last word
        // (`inputs` -> `SessionInputs`).
        let via_bonus = if !same_file && !s.parent.is_empty() { via.map_or(0, |v| via_match(v, &s.parent)) } else { 0 };
        score += via_bonus;
        if call && !(is_callable_kind(&s.kind) || is_type_kind(&s.kind)) {
            score -= 5;
        }
        if call && !same_file {
            // `f()` rarely reaches another type's member; `x.f()` rarely a free function.
            if !r.recv && s.pid != 0 && s.pid != r.caller_parent {
                score -= 25;
            }
            // A module's function through the module's name (`core.plan()`) is no such call.
            if r.recv && !member && source != Some(Source::Match) {
                score -= 20;
            }
        }
        if c_fam && score == 10 {
            // No scope signal in C: the definition nearest in the tree, then not under tests or tools.
            score += common_dirs(&s.path, &r.path).min(5) - is_aux(&s.path) as i32;
        }
        if score > best.0 {
            best = (score, s.id, 1, same_file, member, score - via_bonus);
            field_fn_guess = s.field_fn && !same_file && !own_receiver;
            best_module = r.recv && via.is_some() && source == Some(Source::Match);
        } else if score == best.0 {
            best.2 += 1;
        }
    }
    // A receiver named like the type (`state.update()` -> `State`) is the only signal: the
    // receiver's real type is unknown (a library `MutableStateFlow` named `state`), so a one-word
    // name stays unresolved and a longer one is a guess.
    let via_only = best.0 >= 20 && best.5 < 20;
    let name_only = best.0 > 0 && (best.0 < 20 || via_only);
    // No scope signal: only languages without captured imports may fall back to a unique name.
    // A C free function call, or a call through a receiver chain (`a.b.c.loadItems()`) whose
    // name has several words (so `x.push()` on a library type is no candidate), takes a guess
    // among a few same-name candidates instead.
    let unique = name_only && best.2 == 1 && UNIQUE_FALLBACK.contains(&fam);
    let distinctive = crate::search::ident_parts(&r.name).len() > 1 && !is_library_name(&r.name);
    let few = name_only && call && best.2 <= GUESS_MAX && (r.recv && best.4 && distinctive || c_fam && !r.recv);
    let via_guess = via_only && distinctive && best.2 == 1;
    let dst = if best.0 >= 20 && !via_only || via_guess || unique || few { best.1 } else { 0 };
    let guess = dst != 0
        && !best.3
        && (eligible > 1 && (best.2 > 1 || call && r.recv && !best_module || name_only) || name_only && call && r.recv || via_only);
    let guess = guess || dst != 0 && field_fn_guess;
    (dst, if guess { eligible.max(1) } else { 0 })
}

/// A candidate row: name, candidate, its C type (`meta.t`) and whether its container is a struct
/// tag (`meta.ps`).
type CandRow = (String, Cand, Option<String>, bool);

fn map_cand(r: &rusqlite::Row) -> rusqlite::Result<CandRow> {
    let lang: String = r.get(6)?;
    let path: String = r.get(2)?;
    let file_local = r.get::<_, Option<i64>>(8)?.is_some() && !is_header(&path);
    Ok((
        r.get(1)?,
        Cand {
            id: r.get(0)?,
            path,
            kind: r.get(3)?,
            pid: r.get(4)?,
            parent: r.get::<_, String>(5)?.rsplit('.').next().unwrap_or("").to_string(),
            parent_full: r.get(5)?,
            virt: r.get::<_, Option<i64>>(13)?.is_some(),
            outside: r.get(14)?,
            family: lang::by_name(&lang).map(|l| l.family).unwrap_or(""),
            local: false,
            decl: r.get::<_, Option<i64>>(7)?.is_some(),
            file_local,
            field_fn: r.get::<_, Option<i64>>(9)?.is_some(),
            proto: r.get::<_, Option<i64>>(12)?.is_some(),
        },
        r.get(10)?,
        r.get::<_, Option<i64>>(11)?.is_some(),
    ))
}

const CAND_COLS: &str = "id, name, path, json_extract(meta,'$.k'), COALESCE(json_extract(meta,'$.pid'),0), \
     COALESCE(json_extract(meta,'$.parent'),''), lang, json_extract(meta,'$.d'), json_extract(meta,'$.s'), json_extract(meta,'$.f'), \
     json_extract(meta,'$.t'), json_extract(meta,'$.ps'), json_extract(meta,'$.b'), json_extract(meta,'$.v'), json_extract(meta,'$.cq')";
const REF_COLS: &str = "rowid, path, src, name, kind, dst, recv, amb, line, via, rtype, slot";

/// The C symbol a candidate row stands for, when it is one a type path may walk through.
fn c_sym_of(c: &Cand, ty: Option<String>, tag: bool) -> Option<CSym> {
    // C++ receivers also need the classes of a name and whether a name is a method.
    let cls = matches!(c.kind.as_str(), "class" | "struct" | "union");
    let method = c.kind == "method" && !c.field_fn && is_cpp_path(&c.path);
    (c.family == "c" && (c.kind == FIELD || c.field_fn || c.kind == "type" || c.kind == "function" && ty.is_some() || cls || method)).then(
        || CSym {
            id: c.id,
            kind: c.kind.clone(),
            parent: if c.parent.is_empty() { String::new() } else { format!("{}:{}", if tag { "S" } else { "T" }, c.parent) },
            ty,
            fnptr: c.field_fn,
            full_parent: c.parent_full.clone(),
            outside: c.outside.clone(),
        },
    )
}

fn map_ref(r: &rusqlite::Row) -> rusqlite::Result<RefRow> {
    Ok(RefRow {
        rowid: r.get(0)?,
        path: r.get(1)?,
        src: r.get(2)?,
        name: r.get(3)?,
        kind: r.get(4)?,
        dst: r.get(5)?,
        recv: r.get::<_, i64>(6)? != 0,
        recv_local: r.get::<_, i64>(6)? == 2,
        amb: r.get(7)?,
        caller_parent: -1,
        caller_type: String::new(),
        enclosing: vec![],
        line: r.get(8)?,
        via: r.get(9)?,
        rtype: r.get(10)?,
        slot: r.get(11)?,
        recv_ty: None,
        recv_unique: false,
        recv_ext: false,
        recv_doubt: false,
    })
}

/// One class of a Python method resolution order: a project class, or a base the index cannot
/// follow (a library's, a call's value), which may define any name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MroEntry {
    Class(i64),
    Unknown,
}

/// Bases that define no method a call through `self` could reach.
const PY_INERT_BASES: &[&str] = &["object", "Generic", "ABC", "Protocol"];

/// Most subclasses of one class a call's walk checks; past it the call is in doubt.
const PY_SUBS_MAX: usize = 200;

/// Where a walk along a method resolution order ends for a name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MroHit {
    /// The method this class defines.
    Def(i64),
    /// No class of the order defines it.
    None,
    /// Cannot be told: an unknown base, or an attribute of that name that is no method.
    Doubt,
}

/// Python class hierarchy for method resolution: base classes in source order (as stored and
/// as freshly scored), each class's C3 linearization, and its subclasses.
struct Mro<'a> {
    tx: &'a Connection,
    pid: i64,
    /// Fresh (target, ambiguity) of the `impl` references scored in this run, by rowid.
    fresh: HashMap<i64, (i64, i64)>,
    /// Fresh `impl` links: base class -> the classes naming it.
    fresh_subs: HashMap<i64, Vec<i64>>,
    /// Names of the fresh `impl` links that are guesses.
    fresh_guesses: HashSet<String>,
    /// A node's kind (`meta.k`) and whether its bases include an expression the index cannot
    /// follow (`meta.db`).
    kinds: HashMap<i64, (Option<String>, bool)>,
    /// Linearizations; None for a hierarchy C3 rejects, a cycle or a guessed base link.
    lin: HashMap<i64, Option<Vec<MroEntry>>>,
    open: HashSet<i64>,
    /// Classes holding a symbol of a name that is no method (`step = _other`), by name.
    attrs: HashMap<String, HashSet<i64>>,
    /// [`Mro::subclasses`], by class.
    subs: HashMap<i64, Option<Vec<i64>>>,
    /// [`Mro::walk`], by (class whose order it is, start, name).
    walks: HashMap<(i64, usize, String), MroHit>,
}

impl Mro<'_> {
    fn node(&mut self, id: i64) -> Result<(Option<String>, bool)> {
        if let Some(k) = self.kinds.get(&id) {
            return Ok(k.clone());
        }
        let k: (Option<String>, bool) = self
            .tx
            .prepare_cached(
                "SELECT json_extract(meta,'$.k'), json_extract(meta,'$.db') IS NOT NULL FROM node WHERE id = ?1 AND kind = 'symbol'",
            )?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or((None, false));
        self.kinds.insert(id, k.clone());
        Ok(k)
    }

    fn kind(&mut self, id: i64) -> Result<Option<String>> {
        Ok(self.node(id)?.0)
    }

    /// The bases class `k` names, in order: project classes, and [`MroEntry::Unknown`] for one
    /// that resolves to nothing (a library's) or to no class, or an expression (`make()`).
    /// `object` is left out. None when a base link is a guess among project classes.
    fn bases(&mut self, k: i64) -> Result<Option<Vec<MroEntry>>> {
        let rows: Vec<(i64, i64, i64, String)> = self
            .tx
            .prepare_cached("SELECT rowid, dst, amb, name FROM ts_ref WHERE src = ?1 AND src != 0 AND kind = 'impl' ORDER BY rowid")?
            .query_map([k], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .flatten()
            .collect();
        let mut out = vec![];
        for (rowid, dst, amb, name) in rows {
            let (dst, amb) = self.fresh.get(&rowid).copied().unwrap_or((dst, amb));
            if dst != 0 && amb > 0 {
                return Ok(None);
            }
            let e = if dst != 0 && dst != k && self.kind(dst)?.as_deref() == Some("class") {
                MroEntry::Class(dst)
            } else if dst == 0 && PY_INERT_BASES.contains(&name.as_str()) {
                continue;
            } else {
                MroEntry::Unknown
            };
            if !(matches!(e, MroEntry::Class(_)) && out.contains(&e)) {
                out.push(e);
            }
        }
        if self.node(k)?.1 {
            out.push(MroEntry::Unknown);
        }
        Ok(Some(out))
    }

    /// Python's method resolution order of class `k` (C3; `k` first), an unknown base standing
    /// for itself alone. None for a hierarchy C3 rejects (Python refuses it), a cycle or a
    /// guessed base link.
    fn linearize(&mut self, k: i64) -> Result<Option<Vec<MroEntry>>> {
        if let Some(l) = self.lin.get(&k) {
            return Ok(l.clone());
        }
        if !self.open.insert(k) {
            return Ok(None);
        }
        let Some(bases) = self.bases(k)? else {
            self.open.remove(&k);
            self.lin.insert(k, None);
            return Ok(None);
        };
        let mut seqs: Vec<Vec<MroEntry>> = vec![];
        let mut ok = true;
        for b in &bases {
            match b {
                MroEntry::Class(c) => match self.linearize(*c)? {
                    Some(l) => seqs.push(l),
                    None => ok = false,
                },
                MroEntry::Unknown => seqs.push(vec![MroEntry::Unknown]),
            }
        }
        seqs.push(bases);
        self.open.remove(&k);
        let mut out = ok.then(|| vec![MroEntry::Class(k)]);
        while out.is_some() {
            seqs.retain(|s| !s.is_empty());
            if seqs.is_empty() {
                break;
            }
            // Unknown bases are told apart by position only: each one is its own entry, so
            // the merge never holds one back for another.
            let next = seqs.iter().map(|s| s[0]).find(|c| *c == MroEntry::Unknown || seqs.iter().all(|s| !s[1..].contains(c)));
            let Some(c) = next else {
                out = None;
                break;
            };
            if let Some(o) = out.as_mut() {
                o.push(c);
            }
            if c == MroEntry::Unknown {
                // Only the first sequence it heads gives it up.
                if let Some(s) = seqs.iter_mut().find(|s| s[0] == c) {
                    s.remove(0);
                }
            } else {
                for s in seqs.iter_mut() {
                    if s[0] == c {
                        s.remove(0);
                    }
                }
            }
        }
        self.lin.insert(k, out.clone());
        Ok(out)
    }

    /// The classes holding a non-method symbol named `name`.
    fn attrs(&mut self, name: &str) -> Result<&HashSet<i64>> {
        if !self.attrs.contains_key(name) {
            let mut q = self.tx.prepare_cached(
                "SELECT name, COALESCE(json_extract(meta,'$.pid'),0), json_extract(meta,'$.k') FROM node
                 WHERE project_id = ?1 AND kind = 'symbol' AND name = ?2 COLLATE NOCASE AND source = 'treesitter'",
            )?;
            let set: HashSet<i64> = q
                .query_map(params![self.pid, name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?)))?
                .flatten()
                .filter(|(n, pid, k)| n == name && *pid != 0 && !k.as_deref().is_some_and(is_callable_kind))
                .map(|x| x.1)
                .collect();
            self.attrs.insert(name.to_string(), set);
        }
        Ok(&self.attrs[name])
    }

    /// Where a walk along `line` from position `from` ends for `name`. With an unknown base
    /// anywhere in the order, only a definition in its first class is sure: the unknown base's
    /// own ancestors may come before any later one.
    fn walk(&mut self, owner: i64, line: &[MroEntry], from: usize, name: &str, defs: &HashMap<i64, Vec<i64>>) -> Result<MroHit> {
        let key = (owner, from, name.to_string());
        if let Some(h) = self.walks.get(&key) {
            return Ok(*h);
        }
        let unknown = line.contains(&MroEntry::Unknown);
        let mut hit = if unknown { MroHit::Doubt } else { MroHit::None };
        for (pos, e) in line.iter().enumerate().skip(from) {
            let MroEntry::Class(k) = *e else {
                hit = MroHit::Doubt;
                break;
            };
            let here = defs.get(&k).map(Vec::as_slice).unwrap_or(&[]);
            if self.attrs(name)?.contains(&k) || here.len() > 1 {
                hit = MroHit::Doubt;
                break;
            }
            if let Some(&id) = here.first() {
                hit = if unknown && pos > 0 { MroHit::Doubt } else { MroHit::Def(id) };
                break;
            }
        }
        self.walks.insert(key, hit);
        Ok(hit)
    }

    /// Every project subclass of `k`, nearest first; None past [`PY_SUBS_MAX`].
    fn subclasses(&mut self, k: i64) -> Result<Option<Vec<i64>>> {
        if let Some(s) = self.subs.get(&k) {
            return Ok(s.clone());
        }
        let s = self.subclasses_of(k)?;
        self.subs.insert(k, s.clone());
        Ok(s)
    }

    fn subclasses_of(&mut self, k: i64) -> Result<Option<Vec<i64>>> {
        // A base link guessed among same-name classes may be one to this class: some subclass
        // cannot be told.
        let name: Option<String> = self.tx.prepare_cached("SELECT name FROM node WHERE id = ?1")?.query_row([k], |r| r.get(0)).ok();
        if let Some(n) = name {
            let stored: Vec<i64> = self
                .tx
                .prepare_cached("SELECT rowid FROM ts_ref WHERE project_id = ?1 AND name = ?2 AND kind = 'impl' AND amb > 0")?
                .query_map(params![self.pid, n], |r| r.get(0))?
                .flatten()
                .collect();
            if self.fresh_guesses.contains(&n) || stored.iter().any(|r| !self.fresh.contains_key(r)) {
                return Ok(None);
            }
        }
        let mut out: Vec<i64> = vec![];
        let mut seen: HashSet<i64> = HashSet::from([k]);
        let mut todo = vec![k];
        while let Some(c) = todo.pop() {
            let stored: Vec<(i64, i64)> = self
                .tx
                .prepare_cached("SELECT rowid, src FROM ts_ref WHERE dst = ?1 AND dst != 0 AND kind = 'impl'")?
                .query_map([c], |r| Ok((r.get(0)?, r.get(1)?)))?
                .flatten()
                .collect();
            let mut subs: Vec<i64> = stored.into_iter().filter(|(rowid, _)| !self.fresh.contains_key(rowid)).map(|x| x.1).collect();
            subs.extend(self.fresh_subs.get(&c).into_iter().flatten().copied());
            for s in subs {
                if s != 0 && seen.insert(s) {
                    if out.len() >= PY_SUBS_MAX {
                        return Ok(None);
                    }
                    out.push(s);
                    todo.push(s);
                }
            }
        }
        Ok(Some(out))
    }
}

/// Python: for a call of `name` through `self`/`cls` (or `super()`) in `class`: where its own
/// order ends, and where that holds for every subclass too (else [`MroHit::Doubt`]). The first is
/// None when the class's own order is in doubt.
fn mro_decide(m: &mut Mro, class: i64, name: &str, is_super: bool, defs: &HashMap<i64, Vec<i64>>) -> Result<(MroHit, Option<MroHit>)> {
    let mut hit = match m.linearize(class)? {
        Some(line) => m.walk(class, &line, usize::from(is_super), name, defs)?,
        None => MroHit::Doubt,
    };
    let first = (hit != MroHit::Doubt).then_some(hit);
    // Every subclass must run the same definition from here (for `super()`: from past this
    // class in the subclass's own order).
    if hit != MroHit::Doubt {
        match m.subclasses(class)? {
            None => hit = MroHit::Doubt,
            Some(subs) => {
                for s in subs {
                    let theirs = match m.linearize(s)? {
                        Some(line) => match line.iter().position(|e| *e == MroEntry::Class(class)) {
                            Some(at) => m.walk(s, &line, if is_super { at + 1 } else { 0 }, name, defs)?,
                            None => MroHit::Doubt,
                        },
                        None => MroHit::Doubt,
                    };
                    if theirs != hit {
                        hit = MroHit::Doubt;
                        break;
                    }
                }
            }
        }
    }
    Ok((hit, first))
}

/// Python: a call through `self`, `cls` or `super()` reaches the method Python would run: the
/// first class of the calling class's method resolution order that defines it (`super()`: past
/// the calling class itself), with certainty, when every subclass of the calling class would run
/// the same one. Any doubt leaves a guess: a guessed base link, a base the index cannot follow,
/// a class attribute of that name that is no method, a subclass that would run another
/// definition, a hierarchy C3 rejects. `super().m()` never reaches a method of its own class; a
/// `super()` call no class of a fully known order defines stays unresolved.
fn python_mro(
    tx: &Connection,
    pid: i64,
    refs: &[RefRow],
    cands: &HashMap<String, Vec<Cand>>,
    scored: &mut [(i64, i64, i64)],
) -> Result<()> {
    let todo: Vec<usize> = refs
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.kind == "call"
                && r.recv
                && matches!(r.via.as_deref(), Some("self" | "cls" | "super"))
                && lang::for_path(&r.path).is_some_and(|l| l.family == "python")
        })
        .map(|(i, _)| i)
        .collect();
    if todo.is_empty() {
        return Ok(());
    }
    let mut fresh: HashMap<i64, (i64, i64)> = HashMap::new();
    let mut fresh_subs: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut fresh_guesses: HashSet<String> = HashSet::new();
    for (r, s) in refs.iter().zip(scored.iter()) {
        if r.kind == "impl" {
            fresh.insert(r.rowid, (s.0, s.1));
            if s.0 != 0 && s.1 > 0 {
                fresh_guesses.insert(r.name.clone());
            }
            if s.0 != 0 {
                fresh_subs.entry(s.0).or_default().push(r.src);
            }
        }
    }
    let mut m = Mro {
        tx,
        pid,
        fresh,
        fresh_subs,
        fresh_guesses,
        kinds: HashMap::new(),
        lin: HashMap::new(),
        open: HashSet::new(),
        attrs: HashMap::new(),
        subs: HashMap::new(),
        walks: HashMap::new(),
    };
    // Per name: its methods by class, and each (class, name, through `super()`) decision.
    let mut by_class: HashMap<String, HashMap<i64, Vec<i64>>> = HashMap::new();
    let mut decided: HashMap<(i64, String, bool), (MroHit, Option<MroHit>)> = HashMap::new();
    for i in todo {
        let r = &refs[i];
        // The nearest class around the call (a call in a function nested in a method too).
        let mut class = None;
        for &e in &r.enclosing {
            if m.kind(e)?.as_deref() == Some("class") {
                class = Some(e);
                break;
            }
        }
        let Some(class) = class else { continue };
        let is_super = r.via.as_deref() == Some("super");
        let all = cands.get(&r.name).map(Vec::as_slice).unwrap_or(&[]);
        let own_pick = all.iter().any(|s| s.id == scored[i].0 && s.pid == class);
        let defs = by_class.entry(r.name.clone()).or_insert_with(|| {
            let mut d: HashMap<i64, Vec<i64>> = HashMap::new();
            for s in all.iter().filter(|s| s.family == "python" && !s.local && is_callable_kind(&s.kind)) {
                d.entry(s.pid).or_default().push(s.id);
            }
            d
        });
        let key = (class, r.name.clone(), is_super);
        let (hit, first) = match decided.get(&key) {
            Some(d) => *d,
            None => {
                let d = mro_decide(&mut m, class, &r.name, is_super, defs)?;
                decided.insert(key, d);
                d
            }
        };
        if first.is_some() {
            if let MroHit::Def(id) = hit {
                scored[i] = (id, 0, scored[i].2);
                continue;
            }
            if let (MroHit::Doubt, Some(MroHit::Def(id))) = (hit, first) {
                // The calling class's own order still gives the likeliest target.
                scored[i] = (id, 2, scored[i].2);
                continue;
            }
            if hit == MroHit::None && is_super {
                scored[i] = (0, 0, scored[i].2);
                continue;
            }
        }
        // In doubt: never the calling class's own method for `super()`, never certain.
        if is_super && own_pick {
            scored[i] = (0, 0, scored[i].2);
        } else if scored[i].0 != 0 && scored[i].1 == 0 {
            scored[i].1 = 2;
        }
    }
    Ok(())
}

/// What resolution did: references scored, and how many of those now have a target.
#[derive(Debug, Default, Clone, Copy)]
pub struct Resolved {
    pub rescored: usize,
    pub resolved: usize,
    /// References whose target came from a fresh SCIP import (`amb = -1`).
    pub precise: usize,
    /// Of those, references the name resolution alone would have guessed, missed or put elsewhere.
    pub upgraded: usize,
}

/// Resolve every reference of the project by name: same file > same type > imported > same
/// directory > unique elsewhere. Ambiguous names with no scope signal stay unresolved (`dst = 0`).
/// Rebuilds the project's tree-sitter edges.
pub fn resolve(tx: &Connection, pid: i64) -> Result<Resolved> {
    resolve_scoped(tx, pid, None)
}

/// Like [`resolve`], but only re-scores references in `paths` or named in `names` (the files
/// that changed and the names they define or used to define); the rest keep their target. Only
/// those references, their files' imports and their names' candidates are read, so the cost
/// follows the change, not the project. Scoring runs in parallel.
pub fn resolve_scoped(tx: &Connection, pid: i64, scope: Option<(&HashSet<String>, &HashSet<String>)>) -> Result<Resolved> {
    resolve_scoped_c(tx, pid, scope, &CChange::default())
}

/// What changed in the C types of the files a run re-parsed or removed, so a scoped resolution
/// also re-scores the member calls and stored functions whose type path passes through it.
/// A Python class as resolution reads it: id, resolved bases (target, ambiguity), expression-base flag.
type PyClass = (i64, Vec<(i64, i64)>, bool);

#[derive(Default)]
pub(crate) struct CChange {
    /// Functions whose return type changed (a path's `F:` base).
    funcs: HashSet<String>,
    /// Fields whose type changed, and their structs (a path's middle segments).
    fields: HashSet<String>,
    /// A typedef changed: every type path may read differently.
    all: bool,
    /// C++ classes added or removed, by name: the type paths naming them read differently.
    classes: HashSet<String>,
    /// Python: the classes of re-parsed or removed files as stored before, by (path, name): id,
    /// resolved bases, expression-base flag (see [`py_rescope`]).
    py_old: HashMap<(String, String), Vec<PyClass>>,
    /// Python files re-parsed or removed.
    py_paths: HashSet<String>,
    /// Python base links (rowid) to classes of those files, with the class's old id.
    py_in: HashMap<i64, i64>,
    /// References to re-score besides the scope (see [`py_rescope`]).
    extra: Vec<i64>,
}

/// One C file's type facts, as the store keeps them: (kind, name, container, type).
type CTypeRow = (&'static str, String, String, String);

/// A typedef's type fact: its type, and what its declarators make of it (see [`Def::td`]).
fn type_sig(ty: String, td: Option<u8>, tk: Option<u8>) -> String {
    match (td, tk) {
        (None, None) => ty,
        _ => format!("{ty}\u{1}{td:?}\u{1}{tk:?}"),
    }
}

/// The type facts of a parsed C file (see [`CTypeRow`]).
/// A C++ class, struct or union counts as a typedef does (every type path is re-scored when the
/// classes of a name change: a receiver's class must be the only one of its name).
fn c_type_rows(p: &Parsed, cpp: bool) -> HashSet<CTypeRow> {
    (0..p.defs.len())
        .filter_map(|i| {
            let d = &p.defs[i];
            let kind = if d.field_fn {
                "fnptr"
            } else {
                match d.kind.as_str() {
                    FIELD => "field",
                    "type" => "type",
                    "class" | "struct" | "union" if cpp => "class",
                    "function" if d.ty.is_some() => "function",
                    _ => return None,
                }
            };
            let ty = d.ty.clone().unwrap_or_default();
            let ty = if kind == "type" { type_sig(ty, d.td, d.tk) } else { ty };
            Some((kind, d.name.clone(), p.container_of(i).unwrap_or_default(), ty))
        })
        .collect()
}

/// The type facts the store holds for C file `path` (see [`CTypeRow`]).
fn c_type_rows_stored(tx: &Connection, pid: i64, path: &str) -> Result<HashSet<CTypeRow>> {
    let mut q = tx.prepare_cached(
        "SELECT name, json_extract(meta,'$.k'), COALESCE(json_extract(meta,'$.parent'),''), COALESCE(json_extract(meta,'$.t'),''),
                json_extract(meta,'$.f'), json_extract(meta,'$.td'), json_extract(meta,'$.tk')
         FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'",
    )?;
    let rows = q.query_map(params![pid, path], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<u8>>(5)?,
            r.get::<_, Option<u8>>(6)?,
        ))
    })?;
    let cpp = is_cpp_path(path);
    Ok(rows
        .flatten()
        .filter_map(|(name, k, parent, t, f, td, tk)| {
            // A member of a typedef'd type may have been made a function member (see
            // `c_typedef_members`): the parse alone calls it a field.
            let kind = if f.is_some() && tk.is_none() {
                "fnptr"
            } else {
                match k.as_str() {
                    FIELD => "field",
                    "method" if tk.is_some() => "field",
                    "type" => "type",
                    "class" | "struct" | "union" if cpp => "class",
                    "function" if !t.is_empty() => "function",
                    _ => return None,
                }
            };
            let t = if kind == "type" { type_sig(t, td, tk) } else { t };
            Some((kind, name, parent, t))
        })
        .collect())
}

impl CChange {
    /// Note the facts that differ between a C file's stored and parsed versions.
    fn add(&mut self, before: &HashSet<CTypeRow>, after: &HashSet<CTypeRow>) {
        for (kind, name, parent, _) in before.symmetric_difference(after) {
            match *kind {
                "type" => self.all = true,
                "class" => {
                    self.classes.insert(name.clone());
                }
                "function" => {
                    self.funcs.insert(name.clone());
                }
                // A function-pointer member ends a path: its calls are re-scored by name.
                _ => {
                    self.fields.insert(name.clone());
                    self.fields.extend(parent.rsplit('.').next().filter(|s| !s.is_empty()).map(str::to_string));
                }
            }
        }
    }
}

/// C/C++: a member declared with a typedef'd function pointer (`typedef int (*op_fn)(int);
/// struct ops { op_fn run; };`, also a typedef of a typedef, or `fn_t *run` of a function typedef)
/// is a function member like `int (*run)(int)`; a member that no longer is one (its typedef
/// changed) is a field again. The typedef may be in another file, so this reads the stored
/// typedefs: for the members of `paths` only, or of every file with `None`.
fn c_typedef_members(tx: &Connection, pid: i64, paths: Option<&HashSet<String>>) -> Result<()> {
    const COLS: &str = "SELECT id, json_extract(meta,'$.t'), json_extract(meta,'$.tk'), json_extract(meta,'$.k') FROM node \
         WHERE project_id = ?1 AND kind = 'symbol' AND source = 'treesitter' AND json_extract(meta,'$.tk') IS NOT NULL \
         AND json_extract(meta,'$.k') IN ('field', 'method')";
    type Row = (i64, Option<String>, i64, String);
    let map = |r: &rusqlite::Row| -> rusqlite::Result<Row> {
        Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0), r.get::<_, Option<String>>(3)?.unwrap_or_default()))
    };
    // Nothing to do in a project without C-family files.
    let has_c =
        tx.query_row("SELECT 1 FROM ts_file WHERE project_id = ?1 AND lang IN ('c', 'cpp', 'objc') LIMIT 1", [pid], |_| Ok(())).is_ok();
    if !has_c {
        return Ok(());
    }
    let mut members: Vec<Row> = vec![];
    match paths {
        None => members.extend(tx.prepare(&format!("{COLS} ORDER BY id"))?.query_map([pid], map)?.flatten()),
        Some(paths) => {
            let mut q = tx.prepare_cached(&format!("{COLS} AND path = ?2 ORDER BY id"))?;
            let mut sorted: Vec<&String> =
                paths.iter().filter(|p| lang::for_path(p).is_some_and(|l| l.family == "c") || is_header(p)).collect();
            sorted.sort();
            for p in sorted {
                members.extend(q.query_map(params![pid, p], map)?.flatten());
            }
        }
    }
    if members.is_empty() {
        return Ok(());
    }
    // `name` with NOCASE so the lookup uses `node_sym_name`; the exact name is matched below.
    let mut named = tx.prepare_cached(
        "SELECT name, lang, json_extract(meta,'$.td'), json_extract(meta,'$.t'), json_extract(meta,'$.tk'), json_extract(meta,'$.k') FROM node
         WHERE project_id = ?1 AND name = ?2 COLLATE NOCASE AND kind = 'symbol' AND source = 'treesitter'
           AND json_extract(meta,'$.k') IN ('type', 'class', 'struct', 'union')",
    )?;
    // Pointers over a function that typedef `name` stands for: None when unknown, when it is no
    // function type, or when its definitions disagree.
    let mut depth = |name: &str| -> Option<i64> {
        fn go(q: &mut rusqlite::Statement, pid: i64, name: &str, steps: usize) -> Option<i64> {
            type Row = (String, String, Option<i64>, Option<String>, Option<i64>, String);
            let rows: Vec<Row> = q
                .query_map(params![pid, name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                .ok()?
                .flatten()
                .filter(|r: &Row| r.0 == name && lang::by_name(&r.1).is_some_and(|l| l.family == "c"))
                .collect();
            let mut vals: HashSet<i64> = HashSet::new();
            for (_, lang, td, t, tk, k) in rows {
                // A C++ class or struct of that name too (another namespace's): which one a
                // member names cannot be told. C struct tags are a namespace of their own.
                if k != "type" {
                    if lang == "cpp" {
                        return None;
                    }
                    continue;
                }
                let d = match td {
                    Some(d) => d,
                    None if steps > 0 => {
                        // C++ keeps one namespace for type names (see [`cpp_type`]).
                        let alias = if lang == "cpp" { "S:" } else { "T:" };
                        let base = t.as_deref().and_then(|t| t.strip_prefix(alias))?;
                        go(q, pid, base, steps - 1)? + tk.unwrap_or(0)
                    }
                    None => return None,
                };
                vals.insert(d);
            }
            if vals.len() == 1 { vals.into_iter().next() } else { None }
        }
        go(&mut named, pid, name, C_ALIAS_STEPS + 2)
    };
    let mut memo: HashMap<String, Option<i64>> = HashMap::new();
    let mut set = tx.prepare_cached("UPDATE node SET meta = json_set(meta, '$.k', 'method', '$.f', 1) WHERE id = ?1")?;
    let mut unset = tx.prepare_cached("UPDATE node SET meta = json_set(json_remove(meta, '$.f'), '$.k', 'field') WHERE id = ?1")?;
    for (id, t, tk, k) in members {
        // `tk` is only set on a member of a typedef name: `T:` in C, `S:` in C++.
        let Some(name) = t.as_deref().and_then(|t| t.strip_prefix("T:").or_else(|| t.strip_prefix("S:"))) else { continue };
        let d = *memo.entry(name.to_string()).or_insert_with(|| depth(name));
        let func = d.is_some_and(|d| d + tk == 1);
        if func && k != "method" {
            set.execute([id])?;
        } else if !func && k == "method" {
            unset.execute([id])?;
        }
    }
    Ok(())
}

/// [`resolve_scoped`], also re-scoring the C references whose type path `c` touches.
pub(crate) fn resolve_scoped_c(
    tx: &Connection,
    pid: i64,
    scope: Option<(&HashSet<String>, &HashSet<String>)>,
    c: &CChange,
) -> Result<Resolved> {
    let t0 = std::time::Instant::now();
    c_typedef_members(tx, pid, scope.map(|s| s.0).filter(|_| !c.all))?;
    // 1. The references to score, and the imports of their files.
    let mut refs: Vec<RefRow> = vec![];
    let mut imports: HashMap<String, FileImports> = HashMap::new();
    match scope {
        None => {
            // A sequential scan: through an index, millions of rows cost a random read each.
            let mut q = tx.prepare(&format!("SELECT {REF_COLS} FROM ts_ref NOT INDEXED WHERE project_id = ?1"))?;
            for r in q.query_map([pid], map_ref)?.flatten() {
                if r.kind == "import" {
                    let go = lang::for_path(&r.path).is_some_and(|l| l.family == "go");
                    imports.entry(r.path.clone()).or_default().push(&r.name, r.via.as_deref(), go);
                } else {
                    refs.push(r);
                }
            }
        }
        Some((paths, names)) => {
            let mut seen: HashSet<i64> = HashSet::new();
            let mut by_path = tx.prepare_cached(&format!("SELECT {REF_COLS} FROM ts_ref WHERE project_id = ?1 AND path = ?2"))?;
            let mut by_name = tx.prepare_cached(&format!("SELECT {REF_COLS} FROM ts_ref WHERE project_id = ?1 AND name = ?2"))?;
            let mut sorted_paths: Vec<&String> = paths.iter().collect();
            sorted_paths.sort();
            for path in sorted_paths {
                refs.extend(by_path.query_map(params![pid, path], map_ref)?.flatten().filter(|r| seen.insert(r.rowid)));
            }
            let mut sorted_names: Vec<&String> = names.iter().collect();
            sorted_names.sort();
            for name in sorted_names {
                refs.extend(by_name.query_map(params![pid, name], map_ref)?.flatten().filter(|r| seen.insert(r.rowid)));
            }
            // C type paths through a changed type: by their base (a function's return value)
            // through the index, by a field on the way through a scan of the paths alone.
            // No `INDEXED BY`: a store may lack the index (see `db::migrate`), then it scans.
            let mut rowids: Vec<i64> = vec![];
            let mut funcs: Vec<&String> = c.funcs.iter().collect();
            funcs.sort();
            if !funcs.is_empty() {
                let mut by_base = tx.prepare_cached(
                    "SELECT rowid FROM ts_ref WHERE project_id = ?1 AND rtype IS NOT NULL AND rtype >= ?2 AND rtype < ?3",
                )?;
                for f in funcs {
                    rowids
                        .extend(by_base.query_map(params![pid, format!("F:{f}|"), format!("F:{f}}}")], |r| r.get::<_, i64>(0))?.flatten());
                }
            }
            if c.all || !c.fields.is_empty() || !c.classes.is_empty() {
                let mut q = tx.prepare_cached("SELECT rowid, rtype FROM ts_ref WHERE project_id = ?1 AND rtype IS NOT NULL")?;
                // C++ steps lead with their operator, and a type may be qualified (see
                // [`cpp_chain`]): `S:ns::Engine` names the class `Engine`.
                // A class added or removed may change any field step's type (a field of that
                // class's name, in some namespace): every C++ path with a field step too.
                let field_step = |t: &str| {
                    let mut segs = t.split('|');
                    segs.nth(1).is_some_and(|s| s.starts_with(['.', '>'])) && segs.next().is_some()
                };
                let touches = |t: &str| {
                    c.all
                        || !c.classes.is_empty() && field_step(t)
                        || t.split('|').any(|seg| {
                            let name = seg.split_once(':').map_or(seg, |x| x.1).trim_start_matches(['.', '>']);
                            let ty = name.split('@').next().unwrap_or(name);
                            c.fields.contains(name) || c.classes.contains(ty.rsplit("::").next().unwrap_or(ty))
                        })
                };
                rowids.extend(
                    q.query_map([pid], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                        .flatten()
                        .filter(|(_, t)| touches(t))
                        .map(|x| x.0),
                );
            }
            rowids.extend(c.extra.iter().copied());
            rowids.sort_unstable();
            let mut one = tx.prepare_cached(&format!("SELECT {REF_COLS} FROM ts_ref WHERE rowid = ?1"))?;
            for id in rowids {
                if seen.insert(id) {
                    refs.extend(one.query_map([id], map_ref)?.flatten());
                }
            }
            refs.retain(|r| r.kind != "import");
            let files: HashSet<&str> = refs.iter().map(|r| r.path.as_str()).collect();
            let mut q = tx.prepare_cached("SELECT name, via FROM ts_ref WHERE project_id = ?1 AND path = ?2 AND kind = 'import'")?;
            for f in files {
                let go = lang::for_path(f).is_some_and(|l| l.family == "go");
                let e = imports.entry(f.to_string()).or_default();
                for (n, v) in q.query_map(params![pid, f], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?.flatten() {
                    e.push(&n, v.as_deref(), go);
                }
            }
        }
    }
    trace("resolve: refs loaded", t0);
    // 2. Candidates by name (path, then id order: ties break the same way in full and scoped
    //    runs, whatever ids a re-parsed file got), and
    //    the container of every definition involved.
    let mut cands: HashMap<String, Vec<Cand>> = HashMap::new();
    let mut parent_of: HashMap<i64, (i64, bool)> = HashMap::new(); // id -> (container id, callable)
    // Go calling definitions: id -> its receiver's type (methods are not nested in their type).
    let go_srcs: HashSet<i64> =
        refs.iter().filter(|r| r.recv_local && lang::for_path(&r.path).is_some_and(|l| l.family == "go")).map(|r| r.src).collect();
    let mut type_of: HashMap<i64, String> = HashMap::new();
    // C references whose target follows from a type path (member calls, functions stored in members).
    let c_refs: Vec<usize> = refs
        .iter()
        .enumerate()
        .filter(|(_, r)| (r.kind == "call" && r.recv || r.kind == "ref" && r.rtype.is_some()) && is_c_path(&r.path))
        .map(|(i, _)| i)
        .collect();
    // C++ calls through a receiver whose type path is known: the receiver's class.
    let cpp_refs: Vec<usize> = refs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == "call" && r.recv && r.rtype.is_some() && is_cpp_path(&r.path))
        .map(|(i, _)| i)
        .collect();
    let mut ctypes = CTypes { all: scope.is_none(), ..Default::default() };
    match scope {
        None => {
            let mut q = tx.prepare(&format!(
                "SELECT {CAND_COLS} FROM node WHERE project_id = ?1 AND kind = 'symbol' AND source = 'treesitter' ORDER BY id"
            ))?;
            for (name, c, ty, tag) in q.query_map([pid], map_cand)?.flatten() {
                parent_of.insert(c.id, (c.pid, is_callable_kind(&c.kind)));
                if go_srcs.contains(&c.id) {
                    type_of.insert(c.id, c.parent.clone());
                }
                if (!c_refs.is_empty() || !cpp_refs.is_empty())
                    && let Some(cs) = c_sym_of(&c, ty, tag)
                {
                    ctypes.add(&name, cs);
                }
                if c.kind != FIELD {
                    cands.entry(name).or_default().push(c);
                }
            }
        }
        Some(_) => {
            let mut q = tx.prepare_cached(&format!(
                "SELECT {CAND_COLS} FROM node WHERE project_id = ?1 AND kind = 'symbol' AND name = ?2 COLLATE NOCASE AND source = 'treesitter'"
            ))?;
            let names: HashSet<&str> = refs.iter().map(|r| r.name.as_str()).collect();
            for n in names {
                let mut v: Vec<Cand> = q
                    .query_map(params![pid, n], map_cand)?
                    .flatten()
                    .filter(|(name, c, ..)| name == n && c.kind != FIELD)
                    .map(|x| x.1)
                    .collect();
                v.sort_by(|a, b| (&a.path, a.id).cmp(&(&b.path, b.id)));
                cands.insert(n.to_string(), v);
            }
            let mut one = tx.prepare_cached(
                "SELECT COALESCE(json_extract(meta,'$.pid'),0), json_extract(meta,'$.k') FROM node WHERE id = ?1 AND kind = 'symbol'",
            )?;
            let mut ty = tx.prepare_cached("SELECT COALESCE(json_extract(meta,'$.parent'),'') FROM node WHERE id = ?1")?;
            for id in &go_srcs {
                if let Ok(t) = ty.query_row([id], |r| r.get::<_, String>(0)) {
                    type_of.insert(*id, t.rsplit('.').next().unwrap_or("").to_string());
                }
            }
            let mut want: Vec<i64> =
                refs.iter().map(|r| r.src).chain(cands.values().flatten().map(|c| c.pid)).filter(|&i| i != 0).collect();
            want.sort_unstable();
            want.dedup();
            // Up the chain of each calling definition too (a call in a nested function).
            for _ in 0..8 {
                let mut next = vec![];
                for id in want {
                    if parent_of.contains_key(&id) {
                        continue;
                    }
                    if let Ok((p, k)) = one.query_row([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) {
                        parent_of.insert(id, (p, is_callable_kind(&k)));
                        if p != 0 {
                            next.push(p);
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                want = next;
            }
        }
    }
    for v in cands.values_mut() {
        if scope.is_none() {
            v.sort_by(|a, b| (&a.path, a.id).cmp(&(&b.path, b.id)));
        }
        for c in v.iter_mut() {
            c.local = parent_of.get(&c.pid).is_some_and(|p| p.1);
        }
    }
    for r in refs.iter_mut() {
        r.caller_parent = parent_of.get(&r.src).map(|p| p.0).unwrap_or(-1);
        if let Some(t) = type_of.get(&r.src) {
            r.caller_type = t.clone();
        }
        let mut cur = r.src;
        while cur != 0 && r.enclosing.len() < 16 {
            r.enclosing.push(cur);
            cur = parent_of.get(&cur).map_or(0, |p| p.0);
        }
    }
    trace("resolve: candidates loaded", t0);
    // C type paths, each resolved once: per C reference, its result in `cres_all`.
    let mut cres_all: Vec<CRes> = vec![];
    let mut cres: HashMap<usize, usize> = HashMap::new();
    if !c_refs.is_empty() || !cpp_refs.is_empty() {
        let mut q = tx.prepare_cached(&format!(
            "SELECT {CAND_COLS} FROM node WHERE project_id = ?1 AND kind = 'symbol' AND name = ?2 COLLATE NOCASE AND source = 'treesitter'"
        ))?;
        let mut fetch = |n: &str| -> Vec<CSym> {
            q.query_map(params![pid, n], map_cand)
                .map(|it| it.flatten().filter(|(name, ..)| name == n).filter_map(|(_, c, ty, tag)| c_sym_of(&c, ty, tag)).collect())
                .unwrap_or_default()
        };
        let mut memo: HashMap<String, usize> = HashMap::new();
        for &i in &c_refs {
            let r = &refs[i];
            let path = r.rtype.clone().unwrap_or_else(|| format!("V:|{}", r.name));
            let k = match memo.get(&path) {
                Some(&k) => k,
                None => {
                    cres_all.push(ctypes.resolve(&path, &mut fetch));
                    memo.insert(path, cres_all.len() - 1);
                    cres_all.len() - 1
                }
            };
            cres.insert(i, k);
        }
        let mut cpp_memo: HashMap<String, CppRecv> = HashMap::new();
        for &i in &cpp_refs {
            let path = refs[i].rtype.clone().unwrap_or_default();
            let ty = match cpp_memo.get(&path) {
                Some(t) => t.clone(),
                None => {
                    let t = ctypes.cpp_receiver(&path, &mut fetch);
                    cpp_memo.insert(path, t.clone());
                    t
                }
            };
            match ty {
                CppRecv::Class(full, unique) => {
                    refs[i].recv_ty = Some(full);
                    refs[i].recv_unique = unique;
                }
                CppRecv::External => refs[i].recv_ext = true,
                CppRecv::Doubt => refs[i].recv_doubt = true,
                CppRecv::Unknown => {}
            }
        }
        trace("resolve: C type paths", t0);
    }
    drop(ctypes);
    // 3. Score in parallel; write only what changed. A C call through a member reaches that
    //    member; a function stored in a member records it as its slot.
    let mut scored: Vec<(i64, i64, i64)> = refs
        .par_iter()
        .enumerate()
        .map(|(i, r)| {
            let c = cres.get(&i).map(|&k| &cres_all[k]);
            if let Some(c) = c.filter(|_| r.kind == "call") {
                let (d, a) = c_member_score(c);
                return (d, a, 0);
            }
            let all = cands.get(&r.name).map(Vec::as_slice).unwrap_or(&[]);
            // A value stored in a member (`x->f = name`, `.f = name`) is a function or nothing.
            let (d, a) = if r.rtype.is_some() && r.kind == "ref" {
                let callable: Vec<Cand> = all.iter().filter(|s| is_callable_kind(&s.kind)).cloned().collect();
                score(r, &callable, imports.get(&r.path))
            } else {
                score(r, all, imports.get(&r.path))
            };
            let slot = c.filter(|_| d != 0).map_or(0, c_slot_id);
            (d, a, slot)
        })
        .collect();
    trace("resolve: scored", t0);
    python_mro(tx, pid, &refs, &cands, &mut scored)?;
    trace("resolve: python mro", t0);
    let mut out = Resolved { rescored: refs.len(), ..Default::default() };
    // 4. The precise tier: a fresh SCIP import overrides the name resolution where it knows the target.
    if crate::scip::active(tx, pid) {
        let mut by_path: HashMap<&str, HashMap<(i64, String), i64>> = HashMap::new();
        for (r, s) in refs.iter().zip(scored.iter_mut()) {
            if !by_path.contains_key(r.path.as_str()) {
                by_path.insert(&r.path, crate::scip::targets(tx, pid, &r.path)?);
            }
            if let Some(&dst) = by_path[r.path.as_str()].get(&(r.line, r.name.clone())) {
                if s.0 != dst || s.1 > 0 {
                    out.upgraded += 1;
                }
                *s = (dst, -1, s.2);
                out.precise += 1;
            }
        }
        trace("resolve: scip overlay", t0);
    }
    let mut upd = tx.prepare_cached("UPDATE ts_ref SET dst = ?2, amb = ?3, slot = ?4 WHERE rowid = ?1")?;
    let class = |k: &str| if k == "call" { "CALLS" } else { "REFERENCES" };
    let mut touched: HashSet<(i64, i64, &'static str)> = HashSet::new();
    let mut edges: HashSet<(i64, i64, &'static str)> = HashSet::new();
    for (r, &(dst, amb, slot)) in refs.iter().zip(&scored) {
        if dst != r.dst || amb != r.amb || slot != r.slot {
            upd.execute(params![r.rowid, dst, amb, slot])?;
        }
        if dst != 0 {
            out.resolved += 1;
        }
        // A pair's edge changes only when a reference's target does.
        if dst != r.dst {
            for d in [r.dst, dst] {
                if r.src != 0 && d != 0 && r.src != d {
                    touched.insert((r.src, d, class(&r.kind)));
                }
            }
        }
        if dst != 0 && r.src != 0 && r.src != dst {
            edges.insert((r.src, dst, class(&r.kind)));
        }
    }
    trace("resolve: refs written", t0);
    let py_extra = if scope.is_some() { py_rescope(tx, pid, &refs, &scored, c)? } else { vec![] };
    let mut del = tx.prepare_cached("DELETE FROM edge WHERE src = ?1 AND dst = ?2 AND kind = ?3")?;
    let mut ins = tx.prepare_cached("INSERT OR IGNORE INTO edge(src, dst, kind, source, confidence) VALUES (?1,?2,?3,'treesitter',0.8)")?;
    match scope {
        None => {
            // Write only the edge differences.
            let old: HashSet<(i64, i64, String)> = {
                let mut q = tx.prepare(
                    "SELECT e.src, e.dst, e.kind FROM edge e JOIN node n ON n.id = e.src WHERE n.project_id = ?1 AND e.source = 'treesitter'",
                )?;
                q.query_map([pid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.flatten().collect()
            };
            for (s, d, k) in &old {
                if !edges.contains(&(*s, *d, k.as_str())) {
                    del.execute(params![s, d, k])?;
                }
            }
            for (s, d, k) in edges {
                if !old.contains(&(s, d, k.to_string())) {
                    ins.execute(params![s, d, k])?;
                }
            }
        }
        Some(_) => {
            // An edge exists while any reference still carries it: re-check only the pairs whose
            // references were re-scored.
            let mut calls = tx.prepare_cached("SELECT 1 FROM ts_ref WHERE src = ?1 AND src != 0 AND dst = ?2 AND kind = 'call' LIMIT 1")?;
            let mut other = tx.prepare_cached(
                "SELECT 1 FROM ts_ref WHERE src = ?1 AND src != 0 AND dst = ?2 AND kind NOT IN ('call', 'import') LIMIT 1",
            )?;
            for (s, d, k) in touched {
                let q = if k == "CALLS" { &mut calls } else { &mut other };
                if q.exists(params![s, d])? {
                    ins.execute(params![s, d, k])?;
                } else {
                    del.execute(params![s, d, k])?;
                }
            }
        }
    }
    // Python calls whose method resolution order a changed class may touch: a second pass, over
    // what the first one stored.
    if !py_extra.is_empty() {
        let none = HashSet::new();
        let more = CChange { extra: py_extra, ..Default::default() };
        let r = resolve_scoped_c(tx, pid, Some((&none, &none)), &more)?;
        out.rescored += r.rescored;
        out.resolved += r.resolved;
        out.precise += r.precise;
        out.upgraded += r.upgraded;
    }
    Ok(out)
}

/// Does the project have native tree-sitter symbols?
pub fn has_symbols(c: &Connection, pid: i64) -> bool {
    c.query_row("SELECT 1 FROM node WHERE project_id = ?1 AND source = 'treesitter' AND kind = 'symbol' LIMIT 1", [pid], |_| Ok(())).is_ok()
}

#[cfg(test)]
mod via_tests {
    use super::{Cand, FileImports, RefRow, score};

    fn cand(id: i64, path: &str, kind: &str, parent: &str) -> Cand {
        Cand {
            id,
            path: path.into(),
            kind: kind.into(),
            pid: 0,
            parent: parent.into(),
            family: "go",
            local: false,
            decl: false,
            file_local: false,
            field_fn: false,
            proto: false,
            parent_full: parent.into(),
            virt: false,
            outside: None,
        }
    }

    fn call(name: &str, via: &str) -> RefRow {
        RefRow {
            rowid: 1,
            path: "pkg/cmd/use.go".into(),
            src: 99,
            name: name.into(),
            kind: "call".into(),
            dst: 0,
            recv: true,
            recv_local: false,
            amb: 0,
            caller_parent: -1,
            caller_type: String::new(),
            enclosing: vec![],
            via: Some(via.into()),
            line: 9,
            rtype: None,
            slot: 0,
            recv_ty: None,
            recv_unique: false,
            recv_ext: false,
            recv_doubt: false,
        }
    }

    /// Go: the full import path picks the package of a package-qualified call
    /// (`stats.Compute()`), but never breaks a tie for a method call on a value
    /// (`req.Header.Get()` in a file that imports a package with a `Get` method).
    #[test]
    fn import_path_preference_is_for_package_qualified_calls() {
        let mut imp = FileImports::default();
        imp.push("example.com/pkg/other", None, true);
        imp.push("example.com/pkg/rest", None, true);
        imp.push("example.com/pkg/b/stats", None, true);
        let cands = [cand(1, "pkg/other/thing.go", "method", "Thing"), cand(2, "pkg/rest/client.go", "method", "RESTClient")];
        assert_ne!(score(&call("Get", "Header"), &cands, Some(&imp)).0, 2);
        let cands = [cand(1, "pkg/a/stats/s.go", "function", ""), cand(2, "pkg/b/stats/s.go", "function", "")];
        assert_eq!(score(&call("Compute", "stats"), &cands, Some(&imp)).0, 2);
        // A standard library import names no project package of the same last name.
        let mut std_log = FileImports::default();
        std_log.push("log", None, true);
        let cands = [cand(1, "internal/log/log.go", "function", "")];
        assert_eq!(score(&call("Printf", "log"), &cands, Some(&std_log)).0, 0);
    }

    #[test]
    fn receiver_names_match_type_names() {
        assert_eq!(super::via_match("inputs", "Inputs"), 20);
        assert_eq!(super::via_match("Evaluator", "Evaluator"), 20);
        assert_eq!(super::via_match("inputs", "SessionInputs"), 12);
        assert_eq!(super::via_match("puts", "SessionInputs"), 0);
        assert_eq!(super::via_match("t", "T"), 0);
        assert_eq!(super::via_match("longerThanType", "Type"), 0);
    }
}
