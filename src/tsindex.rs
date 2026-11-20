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
    pos: usize,
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
}

impl Parsed {
    /// Display name of a definition's container (`Outer.Inner`).
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
const VALUE_SLOTS: &[&str] = &["literal_element", "initializer_pair", "initializer_list", "argument_list", "arguments", "keyword_argument"];

/// Parameter lists: a name declared in one outside any function body (a C prototype) is no
/// file-level local.
const PARAM_LISTS: &[&str] = &["parameter_list", "parameters", "formal_parameters"];

/// Node kinds that name a declared variable.
fn is_ident(k: &str) -> bool {
    matches!(k, "identifier" | "shorthand_property_identifier_pattern")
}

/// Nodes that open a scope for parameters and locals: functions (any depth, so a lambda's
/// parameter hides a name only inside that lambda) and Python comprehensions.
fn is_scope(k: &str) -> bool {
    FN_KINDS.contains(&k) || matches!(k, "list_comprehension" | "set_comprehension" | "dictionary_comprehension" | "generator_expression")
}

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
}

/// One top-down pass over the tree (see [`Walk`]). Linear in the tree size: ancestors are kept
/// on a stack, never looked up with `parent()`, which walks down from the root on every call
/// and made deeply nested functions (a long `else if` chain) quadratic.
fn walk_tree<'t>(root: tree_sitter::Node<'t>, src: &[u8]) -> Walk<'t> {
    let mut parents: HashMap<usize, tree_sitter::Node<'t>> = HashMap::new();
    // Scope 0 is the file. (parent scope, declared names)
    let mut scopes: Vec<(usize, HashSet<String>)> = vec![(0, HashSet::new())];
    // Open scopes: (depth of the scope node, scope index).
    let mut open: Vec<(usize, usize)> = vec![];
    // (start byte, innermost scope, name) of identifiers in value slots and receiver positions.
    let mut uses: Vec<(usize, usize, String)> = vec![];
    let mut anc: Vec<tree_sitter::Node> = vec![];
    let mut cur = root.walk();
    'walk: loop {
        let n = cur.node();
        if is_scope(n.kind()) {
            let parent = open.last().map_or(0, |o| o.1);
            scopes.push((parent, HashSet::new()));
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
            if file_ok && declares(n, &anc, src) {
                scopes[scope].1.insert(name.to_string());
            }
            if let Some(p) = anc.last()
                && (VALUE_SLOTS.contains(&p.kind())
                    || RECEIVER_PARENTS.contains(&p.kind()) && p.named_child(0).is_some_and(|f| f.id() == n.id())
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
    for (pos, mut sc, name) in uses {
        loop {
            if scopes[sc].1.contains(&name) {
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
    Walk { locals, file_locals, parents }
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
            "pointer_declarator"
            | "array_declarator"
            | "parenthesized_declarator"
            | "reference_declarator"
            | "init_declarator"
            | "function_declarator" => {
                if !is_field(p, "declarator", c) && p.kind() != "parenthesized_declarator" {
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
    let walk = walk_tree(tree.root_node(), src);
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
            // A class field holding a function (`handleClick = () => {}`) is a method.
            let field_fn = kind == FIELD
                && node
                    .child_by_field_name("value")
                    .or_else(|| node.child_by_field_name("right"))
                    .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression" | "function" | "lambda"));
            if kind == FIELD
                && node
                    .child_by_field_name("value")
                    .or_else(|| node.child_by_field_name("right"))
                    .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression" | "function" | "lambda"))
            {
                kind = "method".into();
            }
            if l.family == "python"
                && let Some(scope) = python_stub(node, src)
            {
                stubs.insert(node.start_byte(), (name.clone(), scope));
            }
            // A field spans its name: one declaration may name several (`A, B int`).
            let (node, last) = if kind == FIELD { (nn, nn) } else { (node, last) };
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
            out.refs.push(Ref {
                kind,
                name,
                line: nn.start_position().row as u32 + 1,
                caller: None,
                recv,
                via,
                recv_local,
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
    p.defs.sort_by(|a, b| (a.start, std::cmp::Reverse(a.end)).cmp(&(b.start, std::cmp::Reverse(b.end))));
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
    p.refs.sort_by(|a, b| (a.pos, rank(a.kind)).cmp(&(b.pos, rank(b.kind))));
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
    let bulk = old.is_empty() && work.len() > 5000;
    if bulk {
        tx.execute_batch("DROP INDEX IF EXISTS ts_ref_name")?;
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
    for path in &removed {
        old_names(path, &mut sc_names)?;
        sc_paths.insert(path.clone());
        drop_file(&tx, &sink, p.id, path)?;
        tx.execute("DELETE FROM ts_file WHERE project_id = ?1 AND path = ?2", params![p.id, path])?;
        st.removed += 1;
    }
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
                store_words(&tx, p.id, &path, &words)?;
                st.unchanged += 1;
            }
            Work::Parse { path, lang, hash, size, mtime, parsed, texts, words } => {
                if old.contains_key(&path) {
                    old_names(&path, &mut sc_names)?;
                    drop_file(&tx, &sink, p.id, &path)?;
                }
                sc_names.extend(parsed.defs.iter().map(|d| d.name.clone()));
                sc_paths.insert(path.clone());
                store_file(&tx, &sink, p.id, &path, lang, &parsed, &texts, fill.then_some(&mut emitted))?;
                store_words(&tx, p.id, &path, &words)?;
                tx.execute(
                    "INSERT OR REPLACE INTO ts_file(project_id, path, lang, hash, size, mtime) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![p.id, path, lang, hash, size, mtime],
                )?;
                st.parsed += 1;
                st.symbols += parsed.defs.len();
                st.refs += parsed.refs.len();
            }
        }
    }
    if bulk {
        tx.execute_batch("CREATE INDEX IF NOT EXISTS ts_ref_name ON ts_ref(project_id, name)")?;
    }
    trace("ts parse+store", t0);
    // A store upgraded past the ambiguity column re-scores every reference once.
    let rescore = db::rescore_pending(&tx, p.id);
    if st.parsed > 0 || st.removed > 0 || rescore {
        let r = resolve_scoped(&tx, p.id, (!full && !rescore).then_some((&sc_paths, &sc_names)))?;
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

fn drop_file(tx: &Connection, sink: &Sink, pid: i64, path: &str) -> Result<()> {
    let sel = "SELECT id FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'";
    for t in [TBL_NODE, TBL_TRI, TBL_BODY] {
        sink.del_where(tx, pid, t, sel, &[&pid, &path])?;
    }
    tx.execute(&format!("DELETE FROM edge WHERE src IN ({sel})"), params![pid, path])?;
    tx.execute(&format!("DELETE FROM edge WHERE dst IN ({sel})"), params![pid, path])?;
    tx.execute(&format!("DELETE FROM fts_node WHERE rowid IN ({sel})"), params![pid, path])?;
    tx.execute(&format!("DELETE FROM fts_trigram WHERE rowid IN ({sel})"), params![pid, path])?;
    tx.execute(&format!("DELETE FROM fts_body WHERE rowid IN ({sel})"), params![pid, path])?;
    tx.execute(&format!("UPDATE ts_ref SET dst = 0 WHERE dst != 0 AND dst IN ({sel})"), params![pid, path])?;
    tx.execute("DELETE FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'", params![pid, path])?;
    tx.execute("DELETE FROM ts_ref WHERE project_id = ?1 AND path = ?2", params![pid, path])?;
    tx.execute("DELETE FROM fts_word WHERE rowid = ?1", [word_rowid(pid, path)])?;
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
    let mut r = tx
        .prepare_cached("INSERT INTO ts_ref(project_id, path, src, name, line, kind, recv, dst, via) VALUES (?1,?2,?3,?4,?5,?6,?7,0,?8)")?;
    for x in &p.refs {
        let src = x.caller.map(|i| ids[i]).unwrap_or(0);
        // `recv` 2: through a receiver that is a parameter or local.
        let recv = if x.recv_local { 2 } else { x.recv as i64 };
        r.execute(params![pid, path, src, x.name, x.line, x.kind, recv, x.via])?;
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
    /// The calling definition and the definitions enclosing it, innermost first.
    enclosing: Vec<i64>,
    /// Last name of the receiver (`Evaluator` in `pl.Evaluator.Preempt()`).
    via: Option<String>,
    line: i64,
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
        }
        self.lasts.insert(i.last.clone());
        self.all.push(i);
    }
    /// JS and Python: where the module `name` was imported from puts candidate `s`. None when the
    /// name was not imported by name with a known module.
    fn source(&self, fam: &str, name: &str, from: &str, s: &Cand) -> Option<Source> {
        let src = self.sources.get(name)?;
        let stem = s.path.rsplit_once('.').map_or(s.path.as_str(), |x| x.0);
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
    let fam = lang::for_path(&r.path).map(|l| l.family).unwrap_or("");
    let call = r.kind == "call";
    // C/C++: one global namespace. A prototype stands for its definition, so it is no candidate
    // while a visible definition of the name exists.
    let c_fam = fam == "c";
    // Languages where a receiver's type is unknown until run time.
    let dynamic = matches!(fam, "python" | "js");
    let has_def = c_fam && cands.iter().any(|s| s.family == fam && !s.decl && (s.path == r.path || !s.file_local && !s.local));
    // A receiver named exactly like a candidate's type (`pl.Evaluator.Preempt()`) rules out a
    // same-file member of another type (a local interface with the same method).
    let via = if r.recv { r.via.as_deref() } else { None };
    let via_hit = via.is_some_and(|v| cands.iter().any(|s| s.family == fam && via_match(v, &s.parent) == 20));
    // Go: `pkg.Name` through an imported package's name reaches only a package of that name
    // (`io.Reader` is not this package's `Reader`).
    let pkg_qualified = fam == "go" && !r.recv_local && via.is_some_and(|v| imp.is_some_and(|i| i.lasts.contains(v)));
    // (score, id, candidates at that score, same file, member, score without the receiver match)
    let mut best = (0i32, 0i64, 0usize, false, false, 0i32);
    let mut eligible = 0i64;
    // The best candidate is a function-holding field reached without receiver evidence.
    let mut field_fn_guess = false;
    for s in cands {
        if s.family != fam || s.id == r.src && call && is_type_kind(&s.kind) {
            continue;
        }
        if via_hit && s.path == r.path && via.is_some_and(|v| via_match(v, &s.parent) < 20) {
            continue;
        }
        if pkg_qualified && via.is_some_and(|v| !imp.is_some_and(|i| i.qualifier_names_dir(v, dir_of(&s.path)))) {
            continue;
        }
        let member = s.pid != 0 || !s.parent.is_empty();
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
            // Go: a method's receiver parameter (`func (s *Server) a() { s.b() }`), or a local
            // value, reaching a method of its own file.
            || fam == "go" && r.recv_local;
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
            if r.recv && !member {
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
    let guess =
        dst != 0 && !best.3 && (eligible > 1 && (best.2 > 1 || call && r.recv || name_only) || name_only && call && r.recv || via_only);
    let guess = guess || dst != 0 && field_fn_guess;
    (dst, if guess { eligible.max(1) } else { 0 })
}

fn map_cand(r: &rusqlite::Row) -> rusqlite::Result<(String, Cand)> {
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
            family: lang::by_name(&lang).map(|l| l.family).unwrap_or(""),
            local: false,
            decl: r.get::<_, Option<i64>>(7)?.is_some(),
            file_local,
            field_fn: r.get::<_, Option<i64>>(9)?.is_some(),
        },
    ))
}

const CAND_COLS: &str = "id, name, path, json_extract(meta,'$.k'), COALESCE(json_extract(meta,'$.pid'),0), \
     COALESCE(json_extract(meta,'$.parent'),''), lang, json_extract(meta,'$.d'), json_extract(meta,'$.s'), json_extract(meta,'$.f')";
const REF_COLS: &str = "rowid, path, src, name, kind, dst, recv, amb, line, via";

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
        enclosing: vec![],
        line: r.get(8)?,
        via: r.get(9)?,
    })
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
    let t0 = std::time::Instant::now();
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
    // 2. Candidates by name (id order, so ties break the same way in full and scoped runs), and
    //    the container of every definition involved.
    let mut cands: HashMap<String, Vec<Cand>> = HashMap::new();
    let mut parent_of: HashMap<i64, (i64, bool)> = HashMap::new(); // id -> (container id, callable)
    match scope {
        None => {
            let mut q = tx.prepare(&format!(
                "SELECT {CAND_COLS} FROM node WHERE project_id = ?1 AND kind = 'symbol' AND source = 'treesitter' ORDER BY id"
            ))?;
            for (name, c) in q.query_map([pid], map_cand)?.flatten() {
                parent_of.insert(c.id, (c.pid, is_callable_kind(&c.kind)));
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
                    .filter(|(name, c)| name == n && c.kind != FIELD)
                    .map(|x| x.1)
                    .collect();
                v.sort_by_key(|c| c.id);
                cands.insert(n.to_string(), v);
            }
            let mut one = tx.prepare_cached(
                "SELECT COALESCE(json_extract(meta,'$.pid'),0), json_extract(meta,'$.k') FROM node WHERE id = ?1 AND kind = 'symbol'",
            )?;
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
        for c in v.iter_mut() {
            c.local = parent_of.get(&c.pid).is_some_and(|p| p.1);
        }
    }
    for r in refs.iter_mut() {
        r.caller_parent = parent_of.get(&r.src).map(|p| p.0).unwrap_or(-1);
        let mut cur = r.src;
        while cur != 0 && r.enclosing.len() < 16 {
            r.enclosing.push(cur);
            cur = parent_of.get(&cur).map_or(0, |p| p.0);
        }
    }
    trace("resolve: candidates loaded", t0);
    // 3. Score in parallel; write only what changed.
    let mut scored: Vec<(i64, i64)> =
        refs.par_iter().map(|r| score(r, cands.get(&r.name).map(Vec::as_slice).unwrap_or(&[]), imports.get(&r.path))).collect();
    trace("resolve: scored", t0);
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
                *s = (dst, -1);
                out.precise += 1;
            }
        }
        trace("resolve: scip overlay", t0);
    }
    let mut upd = tx.prepare_cached("UPDATE ts_ref SET dst = ?2, amb = ?3 WHERE rowid = ?1")?;
    let class = |k: &str| if k == "call" { "CALLS" } else { "REFERENCES" };
    let mut touched: HashSet<(i64, i64, &'static str)> = HashSet::new();
    let mut edges: HashSet<(i64, i64, &'static str)> = HashSet::new();
    for (r, &(dst, amb)) in refs.iter().zip(&scored) {
        if dst != r.dst || amb != r.amb {
            upd.execute(params![r.rowid, dst, amb])?;
        }
        if dst != 0 {
            out.resolved += 1;
        }
        for d in [r.dst, dst] {
            if r.src != 0 && d != 0 && r.src != d {
                touched.insert((r.src, d, class(&r.kind)));
            }
        }
        if dst != 0 && r.src != 0 && r.src != dst {
            edges.insert((r.src, dst, class(&r.kind)));
        }
    }
    trace("resolve: refs written", t0);
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
            enclosing: vec![],
            via: Some(via.into()),
            line: 9,
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
