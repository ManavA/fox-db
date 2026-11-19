//! Opt-in hooks that meet the model where it already searches. Both are off unless `[hooks]` in
//! config.toml turns them on, and both prefer silence to a wrong answer.
//!
//! - `post-tool-use` (`grep_context`): after a Bash `grep`/`rg`/`git grep` that printed
//!   `path:line:text` hits in an indexed project, name each hit's enclosing definition and its
//!   handle. It only adds `additionalContext`; the tool output is never changed. Anything it
//!   cannot attribute (compound commands, pipes into something that rewrites lines, counts, file
//!   lists, files changed since indexing, projects foxdb has not indexed) gets nothing.
//! - `user-prompt-submit` (`prompt_context`): when the prompt names an identifier or path that
//!   resolves to exactly one indexed definition or file, add its location, caller count and
//!   handle (at most [`PROMPT_ROWS`] rows).

use crate::project::{self, Project};
use crate::query::handle;
use crate::tsindex::is_callable_kind;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// Most definitions named after one grep.
pub const GREP_ROWS: usize = 12;
/// Most characters added after one grep.
pub const GREP_CHARS: usize = 1_200;
/// Most rows added to one prompt.
pub const PROMPT_ROWS: usize = 5;
/// Output lines a grep hook reads before it stops (speed bound on huge outputs).
const MAX_SCAN_LINES: usize = 4_000;
/// Distinct files a grep hook looks up before it stops.
const MAX_FILES: usize = 64;

// ---------------------------------------------------------------- shell words

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    /// An unquoted control operator or redirection: `|`, `||`, `&&`, `;`, `&`, `>`, `2>`, ...
    Op(String),
}

/// Split a command line into words and operators the way a POSIX shell would, for the subset
/// this hook attributes. None on command substitution, unbalanced quotes, or a newline.
fn shell_words(s: &str) -> Option<Vec<Tok>> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut in_word = false;
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    let flush = |cur: &mut String, in_word: &mut bool, out: &mut Vec<Tok>| {
        if *in_word {
            out.push(Tok::Word(std::mem::take(cur)));
            *in_word = false;
        }
    };
    while i < cs.len() {
        let ch = cs[i];
        match ch {
            '\'' => {
                let end = cs[i + 1..].iter().position(|&c| c == '\'')? + i + 1;
                cur.extend(&cs[i + 1..end]);
                in_word = true;
                i = end + 1;
            }
            '"' => {
                i += 1;
                loop {
                    let c = *cs.get(i)?;
                    match c {
                        '"' => break,
                        '`' => return None,
                        '$' if cs.get(i + 1) == Some(&'(') => return None,
                        '\\' if matches!(cs.get(i + 1), Some('"' | '\\' | '$' | '`')) => {
                            cur.push(cs[i + 1]);
                            i += 1;
                        }
                        _ => cur.push(c),
                    }
                    i += 1;
                }
                in_word = true;
                i += 1;
            }
            '\\' => {
                cur.push(*cs.get(i + 1)?);
                in_word = true;
                i += 2;
            }
            '`' | '\n' | '\r' => return None,
            '$' if cs.get(i + 1) == Some(&'(') => return None,
            ' ' | '\t' => {
                flush(&mut cur, &mut in_word, &mut out);
                i += 1;
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' => {
                // `2>` / `2>>` / `2>&1`: a file-descriptor digit glued to the redirection.
                let mut op = String::new();
                if (ch == '>' || ch == '<') && in_word && (cur == "2" || cur == "1") {
                    op.push_str(&cur);
                    cur.clear();
                    in_word = false;
                }
                flush(&mut cur, &mut in_word, &mut out);
                op.push(ch);
                i += 1;
                while let Some(&n) = cs.get(i) {
                    if (ch == '|' && n == '|' && op == "|")
                        || (ch == '&' && n == '&' && op == "&")
                        || (ch == '>' && n == '>')
                        || (ch == '>' && n == '&')
                        || (ch == '&' && n == '>')
                    {
                        op.push(n);
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(Tok::Op(op));
            }
            _ => {
                cur.push(ch);
                in_word = true;
                i += 1;
            }
        }
    }
    flush(&mut cur, &mut in_word, &mut out);
    Some(out)
}

// ---------------------------------------------------------------- grep command shape

/// What the search command's output looks like.
#[derive(Debug, Default, PartialEq, Eq)]
struct Search {
    /// Directory the search ran in (cwd, after a leading `cd` or `git -C`).
    dir: PathBuf,
    /// Lines carry no path (`line:text`): the one file searched.
    single_file: Option<String>,
}

/// Which pieces of a short-flag cluster take an argument, per tool.
struct Flags {
    /// Short flags that take an argument (`-e PAT`, `-A 3`).
    short_arg: &'static str,
    /// Long flags that take an argument when not written `--flag=value`.
    long_arg: &'static [&'static str],
    /// Short flags whose output this hook cannot attribute.
    short_bad: &'static str,
    long_bad: &'static [&'static str],
    /// Flags that turn line numbers on / off.
    short_num_on: &'static str,
    long_num_on: &'static [&'static str],
    short_num_off: &'static str,
    /// Flags that force / drop the file name.
    short_with_name: &'static str,
    short_no_name: &'static str,
    long_no_name: &'static [&'static str],
}

const GREP: Flags = Flags {
    short_arg: "efABCmdD",
    long_arg: &[
        "--regexp",
        "--file",
        "--after-context",
        "--before-context",
        "--context",
        "--max-count",
        "--include",
        "--exclude",
        "--exclude-dir",
        "--exclude-from",
        "--label",
        "--binary-files",
        "--devices",
        "--directories",
    ],
    short_bad: "lLcqZzbTu",
    long_bad: &[
        "--files-with-matches",
        "--files-without-match",
        "--count",
        "--quiet",
        "--silent",
        "--null",
        "--null-data",
        "--byte-offset",
        "--initial-tab",
        "--color=always",
        "--colour=always",
    ],
    short_num_on: "n",
    long_num_on: &["--line-number"],
    short_num_off: "",
    short_with_name: "H",
    short_no_name: "h",
    long_no_name: &["--no-filename"],
};

const RG: Flags = Flags {
    short_arg: "efgtTABCmMjErdS",
    long_arg: &[
        "--regexp",
        "--file",
        "--glob",
        "--iglob",
        "--type",
        "--type-not",
        "--type-add",
        "--after-context",
        "--before-context",
        "--context",
        "--max-count",
        "--max-columns",
        "--threads",
        "--encoding",
        "--replace",
        "--max-depth",
        "--sort",
        "--sortr",
        "--max-filesize",
        "--pre",
        "--pre-glob",
        "--engine",
        "--path-separator",
        "--context-separator",
        "--field-match-separator",
        "--field-context-separator",
        "--colors",
        "--color",
    ],
    short_bad: "lcq0b",
    long_bad: &[
        "--files",
        "--files-with-matches",
        "--files-without-match",
        "--count",
        "--count-matches",
        "--quiet",
        "--null",
        "--json",
        "--heading",
        "--byte-offset",
        "--type-list",
        "--color=always",
        "--pretty",
        "-p",
        "--stats",
    ],
    short_num_on: "n",
    long_num_on: &["--line-number", "--vimgrep"],
    short_num_off: "N",
    short_with_name: "H",
    short_no_name: "I",
    long_no_name: &["--no-filename"],
};

const GIT_GREP: Flags = Flags {
    short_arg: "efABCmO",
    long_arg: &["--regexp", "--file", "--after-context", "--before-context", "--context", "--max-count", "--threads", "--max-depth"],
    short_bad: "lLcqzhpWo",
    long_bad: &[
        "--files-with-matches",
        "--name-only",
        "--files-without-match",
        "--count",
        "--quiet",
        "--null",
        "--heading",
        "--show-function",
        "--function-context",
        "--color=always",
        "--column",
        "--only-matching",
        "--cached",
        "--untracked",
    ],
    short_num_on: "n",
    long_num_on: &["--line-number"],
    short_num_off: "",
    short_with_name: "",
    short_no_name: "",
    long_no_name: &[],
};

/// The positional arguments (pattern, then paths) of a grep-like command, or None when a flag
/// makes the output unattributable. Also whether line numbers are on and a file name forced/dropped.
struct Parsed {
    positional: Vec<String>,
    pattern_given: bool,
    numbers: bool,
    with_name: bool,
    no_name: bool,
}

fn parse_flags(words: &[String], f: &Flags) -> Option<Parsed> {
    let mut p = Parsed { positional: vec![], pattern_given: false, numbers: false, with_name: false, no_name: false };
    let mut i = 0;
    let mut opts_done = false;
    while i < words.len() {
        let w = &words[i];
        i += 1;
        if opts_done || !w.starts_with('-') || w == "-" {
            p.positional.push(w.clone());
            continue;
        }
        if w == "--" {
            opts_done = true;
            continue;
        }
        if let Some(long) = w.strip_prefix("--") {
            let (name, has_val) = match long.split_once('=') {
                Some((n, _)) => (format!("--{n}"), true),
                None => (w.clone(), false),
            };
            if f.long_bad.iter().any(|b| *b == w || *b == name) {
                return None;
            }
            if f.long_num_on.contains(&name.as_str()) {
                p.numbers = true;
            }
            if name == "--no-line-number" {
                p.numbers = false;
            }
            if name == "--with-filename" {
                p.with_name = true;
            }
            if f.long_no_name.contains(&name.as_str()) {
                p.no_name = true;
            }
            if name == "--regexp" || name == "--file" {
                p.pattern_given = true;
            }
            if f.long_arg.contains(&name.as_str()) && !has_val {
                i += 1;
            }
            continue;
        }
        if f.long_bad.contains(&w.as_str()) {
            return None;
        }
        // A short-flag cluster: `-rniE`, `-A3`, `-e PAT`.
        let cs: Vec<char> = w[1..].chars().collect();
        for (k, c) in cs.iter().enumerate() {
            if f.short_bad.contains(*c) {
                return None;
            }
            if f.short_num_on.contains(*c) {
                p.numbers = true;
            }
            if f.short_num_off.contains(*c) {
                p.numbers = false;
            }
            if f.short_with_name.contains(*c) {
                p.with_name = true;
            }
            if f.short_no_name.contains(*c) {
                p.no_name = true;
            }
            if c.is_ascii_digit() {
                continue; // `-3` = context 3
            }
            if f.short_arg.contains(*c) {
                if *c == 'e' || *c == 'f' {
                    p.pattern_given = true;
                }
                if k + 1 == cs.len() {
                    i += 1; // the argument is the next word
                }
                break; // the rest of the cluster is the argument
            }
        }
    }
    Some(p)
}

/// A pipe stage after the search that keeps each line as it is (drops, reorders or dedupes).
fn keeps_lines(stage: &[String]) -> bool {
    let Some(cmd) = stage.first() else { return false };
    let args = &stage[1..];
    let flags_only = |ok: &dyn Fn(&str) -> bool| args.iter().all(|a| a.starts_with('-') && ok(a));
    match cmd.as_str() {
        "head" | "tail" => {
            let mut i = 0;
            while i < args.len() {
                let a = &args[i];
                if a == "-n" || a == "--lines" {
                    i += 2;
                    continue;
                }
                let ok = a.strip_prefix("-n").or_else(|| a.strip_prefix("--lines=")).or_else(|| a.strip_prefix('-')).is_some_and(|n| {
                    let n = n.trim_start_matches('+');
                    !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
                });
                if !ok {
                    return false;
                }
                i += 1;
            }
            true
        }
        "sort" => {
            let mut i = 0;
            while i < args.len() {
                let a = &args[i];
                if !a.starts_with('-') || a == "-o" || a.starts_with("--output") {
                    return false;
                }
                if a == "-t" || a == "-k" {
                    i += 1;
                }
                i += 1;
            }
            true
        }
        "uniq" => flags_only(&|a| !a.contains('c') && !a.contains('D')),
        "grep" | "egrep" | "fgrep" => {
            // A filter: flags that select lines, one pattern, no files.
            let mut pos = 0;
            let mut i = 0;
            while i < args.len() {
                let a = &args[i];
                if a == "-e" {
                    i += 2;
                    continue;
                }
                if a.starts_with("--") {
                    if !matches!(a.as_str(), "--invert-match" | "--ignore-case" | "--extended-regexp" | "--fixed-strings" | "--word-regexp")
                    {
                        return false;
                    }
                } else if a.starts_with('-') && a.len() > 1 {
                    if !a[1..].chars().all(|c| "viEFwP".contains(c)) {
                        return false;
                    }
                } else {
                    pos += 1;
                }
                i += 1;
            }
            pos <= 1
        }
        _ => false,
    }
}

fn is_redirect_ok(op: &str, target: Option<&Tok>) -> Option<usize> {
    // `2>/dev/null` and `2>&1` keep stdout as it is; returns how many tokens the redirect uses.
    match op {
        "2>&" => matches!(target, Some(Tok::Word(w)) if w == "1").then_some(2),
        "2>" | "2>>" => matches!(target, Some(Tok::Word(w)) if w == "/dev/null").then_some(2),
        _ => None,
    }
}

/// Is this Bash command one search whose stdout this hook can attribute? Returns where it ran
/// and whether its lines name a file.
fn search_shape(command: &str, cwd: &Path) -> Option<Search> {
    let toks = shell_words(command)?;
    // Split into `&&`-joined commands, each a `|` pipeline of stages.
    let mut cmds: Vec<Vec<Vec<String>>> = vec![vec![vec![]]];
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Word(w) => cmds.last_mut()?.last_mut()?.push(w.clone()),
            Tok::Op(op) if op == "|" => cmds.last_mut()?.push(vec![]),
            Tok::Op(op) if op == "&&" => cmds.push(vec![vec![]]),
            Tok::Op(op) => {
                let n = is_redirect_ok(op, toks.get(i + 1))?;
                i += n;
                continue;
            }
        }
        i += 1;
    }
    let mut dir = cwd.to_path_buf();
    // At most one leading `cd DIR &&`.
    if cmds.len() == 2 {
        let first = &cmds[0];
        if first.len() != 1 || first[0].len() != 2 || first[0][0] != "cd" {
            return None;
        }
        dir = join(&dir, &first[0][1]);
        cmds.remove(0);
    }
    if cmds.len() != 1 {
        return None;
    }
    let stages = &cmds[0];
    if stages.iter().any(|s| s.is_empty()) || !stages[1..].iter().all(|s| keeps_lines(s)) {
        return None;
    }
    let mut words: &[String] = &stages[0];
    // Leading `VAR=value` assignments (e.g. `LC_ALL=C grep`).
    while words.first().is_some_and(|w| w.contains('=') && !w.starts_with('-') && w.split('=').next().is_some_and(is_ident)) {
        words = &words[1..];
    }
    let cmd = words.first()?.rsplit('/').next()?.to_string();
    let (flags, rest, git) = match cmd.as_str() {
        "grep" | "egrep" | "fgrep" => (&GREP, &words[1..], false),
        "rg" => (&RG, &words[1..], false),
        "git" => {
            let mut k = 1;
            while k < words.len() && words[k] != "grep" {
                match words[k].as_str() {
                    "-C" => {
                        dir = join(&dir, words.get(k + 1)?);
                        k += 2;
                    }
                    "--no-pager" | "-P" => k += 1,
                    _ => return None,
                }
            }
            if k >= words.len() {
                return None;
            }
            (&GIT_GREP, &words[k + 1..], true)
        }
        _ => return None,
    };
    let p = parse_flags(rest, flags)?;
    if !p.numbers {
        return None;
    }
    let paths: Vec<&String> = p.positional.iter().skip(if p.pattern_given { 0 } else { 1 }).collect();
    if git {
        return (!p.no_name).then_some(Search { dir, single_file: None });
    }
    // One plain file named and no forced file name: grep and rg print `line:text`.
    let single = paths.len() == 1 && !paths[0].contains(['*', '?', '[']) && join(&dir, paths[0]).is_file();
    if p.no_name || (single && !p.with_name) {
        return if single { Some(Search { dir, single_file: Some(paths[0].clone()) }) } else { None };
    }
    Some(Search { dir, single_file: None })
}

fn is_ident(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `base` joined with `p` (`~` expanded), with `.` and `..` folded away without touching the disk.
fn join(base: &Path, p: &str) -> PathBuf {
    let p = match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)).unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    };
    let full = if p.is_absolute() { p } else { base.join(p) };
    let mut out = PathBuf::new();
    for c in full.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `path:line:text` (or `line:text` for a single-file search) -> (path, line).
fn split_hit<'a>(l: &'a str, single: Option<&'a str>) -> Option<(&'a str, i64)> {
    if l.contains('\u{1b}') {
        return None;
    }
    let num_colon = |s: &str| -> Option<i64> {
        let d = s.find(|c: char| !c.is_ascii_digit())?;
        (d > 0 && s[d..].starts_with(':')).then(|| s[..d].parse().ok()).flatten()
    };
    if let Some(f) = single {
        return Some((f, num_colon(l)?));
    }
    let mut from = 0;
    while let Some(k) = l[from..].find(':') {
        let at = from + k;
        if at > 0
            && let Some(n) = num_colon(&l[at + 1..])
        {
            return Some((&l[..at], n));
        }
        from = at + 1;
    }
    None
}

// ---------------------------------------------------------------- index lookups

/// A tree-sitter definition from the index.
struct Def {
    id: i64,
    name: String,
    kind: String,
    path: String,
    line: i64,
    end: i64,
    parent: Option<String>,
}

impl Def {
    fn qname(&self) -> String {
        match &self.parent {
            Some(p) if !p.is_empty() => format!("{p}.{}", self.name),
            _ => self.name.clone(),
        }
    }
    fn h(&self) -> String {
        format!("c:{:x}", self.id)
    }
}

const DEF_COLS: &str = "n.id, n.name, COALESCE(json_extract(n.meta,'$.k'),''), n.path, COALESCE(n.line,0), COALESCE(n.end_line,0), json_extract(n.meta,'$.parent')";

fn map_def(r: &rusqlite::Row) -> rusqlite::Result<Def> {
    Ok(Def {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: r.get(2)?,
        path: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        line: r.get(4)?,
        end: r.get(5)?,
        parent: r.get(6)?,
    })
}

/// Has the file on disk changed since foxdb indexed it? (Its line numbers would be off.)
fn fresh(c: &Connection, pid: i64, rel: &str, abs: &Path) -> bool {
    let Ok(md) = std::fs::metadata(abs) else { return false };
    let row: Option<(i64, i64)> = c
        .query_row(
            "SELECT COALESCE(size,-1), COALESCE(mtime,-1) FROM node WHERE project_id = ?1 AND kind = 'file' AND path = ?2",
            params![pid, rel],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten();
    row.is_some_and(|(size, mtime)| size == md.len() as i64 && mtime == crate::inventory::mtime_ms(&md))
}

fn defs_in_file(c: &Connection, pid: i64, rel: &str) -> Vec<Def> {
    let Ok(mut q) = c.prepare_cached(&format!(
        "SELECT {DEF_COLS} FROM node n WHERE n.project_id = ?1 AND n.path = ?2 AND n.kind = 'symbol' AND n.source = 'treesitter'"
    )) else {
        return vec![];
    };
    q.query_map(params![pid, rel], map_def).map(|it| it.flatten().collect()).unwrap_or_default()
}

/// The innermost definition whose span holds `line`.
fn enclosing(defs: &[Def], line: i64) -> Option<&Def> {
    defs.iter().filter(|d| d.line <= line && line <= d.end.max(d.line)).min_by_key(|d| (d.end.max(d.line) - d.line, -d.line))
}

// ---------------------------------------------------------------- post-tool-use

/// Context for a Bash grep's hits, or "" when there is nothing it can attribute.
pub fn grep_context(input: &Value) -> Result<String> {
    if input["tool_name"] != "Bash" {
        return Ok(String::new());
    }
    let Some(command) = input["tool_input"]["command"].as_str() else { return Ok(String::new()) };
    let stdout = match &input["tool_response"] {
        Value::String(s) => s.as_str(),
        v => v["stdout"].as_str().unwrap_or(""),
    };
    if stdout.trim().is_empty() {
        return Ok(String::new());
    }
    let cwd = input["cwd"].as_str().map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let Some(shape) = search_shape(command, &cwd) else { return Ok(String::new()) };
    let Some(c) = crate::db::open_ro() else { return Ok(String::new()) };
    let Some(p) = project::find(&c, &project::locate(&shape.dir))? else { return Ok(String::new()) };
    Ok(attribute(&c, &p, &shape, stdout))
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// A test file, by the common naming conventions.
fn is_test_path(p: &str) -> bool {
    let file = p.rsplit('/').next().unwrap_or(p);
    let stem = file.split('.').next().unwrap_or(file);
    p.split('/').any(|d| matches!(d, "test" | "tests" | "testing" | "testdata" | "__tests__" | "spec"))
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.ends_with("Test")
        || stem.ends_with("Tests")
        || file.contains(".test.")
        || file.contains(".spec.")
}

/// A file's path in the project and its definitions, when its hits can be attributed.
type FileDefs = Option<(String, Vec<Def>)>;

/// The definitions of the file at `abs`, if it lies in project `p` (whose canonical root is
/// `root`) and is unchanged since indexing.
fn file_defs(c: &Connection, pid: i64, root: &Path, abs: &Path) -> FileDefs {
    let rel = canon(abs).strip_prefix(root).ok()?.to_string_lossy().into_owned();
    if !fresh(c, pid, &rel, abs) {
        return None;
    }
    let defs = defs_in_file(c, pid, &rel);
    (!defs.is_empty()).then_some((rel, defs))
}

fn attribute(c: &Connection, p: &Project, shape: &Search, stdout: &str) -> String {
    let root = canon(&p.root);
    let mut files: HashMap<String, FileDefs> = HashMap::new();
    // Per definition, in first-hit order: (path, kind, qname, handle, hit lines).
    let mut rows: Vec<(String, String, String, String, Vec<i64>)> = vec![];
    let mut row_of: HashMap<i64, usize> = HashMap::new();
    for l in stdout.lines().take(MAX_SCAN_LINES) {
        let Some((path, line)) = split_hit(l, shape.single_file.as_deref()) else { continue };
        if !files.contains_key(path) {
            if files.len() >= MAX_FILES {
                continue;
            }
            let v = file_defs(c, p.id, &root, &join(&shape.dir, path));
            files.insert(path.to_string(), v);
        }
        let Some(Some((rel, defs))) = files.get(path) else { continue };
        let Some(d) = enclosing(defs, line) else { continue };
        match row_of.get(&d.id) {
            Some(&k) => rows[k].4.push(line),
            None => {
                row_of.insert(d.id, rows.len());
                rows.push((rel.clone(), d.kind.clone(), d.qname(), d.h(), vec![line]));
            }
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    // Code before tests, otherwise in the order grep printed them.
    rows.sort_by_key(|r| is_test_path(&r.0));
    let head = "foxdb: definitions holding these hits (fox_read target=<handle> reads one; fox_graph target=<handle> rel=callers)\n";
    let mut out = String::from(head);
    let mut shown = 0;
    for (path, kind, q, h, lines) in &rows {
        if shown == GREP_ROWS {
            break;
        }
        let mut at: Vec<String> = lines.iter().take(4).map(i64::to_string).collect();
        if lines.len() > 4 {
            at.push("...".into());
        }
        let row = format!("{path}:{} {kind} {} {h}\n", at.join(","), crate::query::clip(q, 80));
        // Leave room for the closing line.
        if out.chars().count() + row.chars().count() > GREP_CHARS - 60 {
            break;
        }
        out.push_str(&row);
        shown += 1;
    }
    if shown < rows.len() {
        out.push_str(&format!("+{} more definitions with hits not shown\n", rows.len() - shown));
    }
    out
}

// ---------------------------------------------------------------- user-prompt-submit

/// Words in the prompt shaped like code: `snake_case`, `camelCase`, `PascalCase` with two or
/// more humps, `Type.member`, backticked spans, and paths with a file extension. Plain words
/// never qualify, so ordinary prose resolves nothing.
fn prompt_terms(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let push = |t: String, out: &mut Vec<String>| {
        if !out.contains(&t) {
            out.push(t);
        }
    };
    // Backticked spans count even when single-hump (`Cart`), since the user marked them as code.
    let parts: Vec<&str> = prompt.split('`').collect();
    for k in (1..parts.len().saturating_sub(1)).step_by(2) {
        let s = parts[k].trim();
        let s = clean(s);
        if !s.contains(char::is_whitespace) && (is_path_like(&s) || is_dotted(&s) || (is_ident(&s) && s.len() >= 3)) {
            push(s, &mut out);
        }
    }
    for w in prompt
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '"' | '`' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'))
    {
        let w = clean(w);
        if w.contains("://") || w.is_empty() {
            continue;
        }
        if is_path_like(&w) || (is_dotted(&w) && w.split('.').any(code_shaped)) || (is_ident(&w) && code_shaped(&w)) {
            push(w, &mut out);
        }
    }
    out
}

/// Trim punctuation around a word and a trailing call `()` or `:line`.
fn clean(w: &str) -> String {
    let mut w = w.trim_matches(|c: char| matches!(c, '.' | ',' | ':' | ';' | '!' | '?' | '*' | '"' | '\'' | '(' | ')' | '[' | ']'));
    if let Some(i) = w.find('(') {
        w = &w[..i];
    }
    if let Some((a, b)) = w.rsplit_once(':')
        && !b.is_empty()
        && b.chars().all(|c| c.is_ascii_digit())
    {
        w = a;
    }
    w.trim_start_matches("./").to_string()
}

fn code_shaped(w: &str) -> bool {
    if w.len() < 4 || !is_ident(w) {
        return false;
    }
    let inner_underscore = w.trim_matches('_').contains('_') && w.chars().any(|c| c.is_ascii_alphabetic());
    let cs: Vec<char> = w.chars().collect();
    let camel = cs.windows(2).any(|p| p[0].is_ascii_lowercase() && p[1].is_ascii_uppercase());
    inner_underscore || camel
}

fn is_dotted(w: &str) -> bool {
    let parts: Vec<&str> = w.split('.').collect();
    parts.len() >= 2
        && parts.iter().all(|p| is_ident(p))
        && parts.last().is_some_and(|l| l.len() >= 3)
        && crate::lang::for_path(w).is_none()
}

fn is_path_like(w: &str) -> bool {
    let last = w.rsplit('/').next().unwrap_or(w);
    last.contains('.')
        && crate::lang::for_path(last).is_some()
        && last.rsplit_once('.').is_some_and(|(stem, _)| !stem.is_empty())
        && w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
}

/// The one tree-sitter definition `term` names (exact case, `Type.member` qualified), or None
/// when there are none or several.
fn one_def(c: &Connection, pid: i64, term: &str) -> Option<Def> {
    let (qual, name) = match term.rsplit_once('.') {
        Some((a, b)) => (Some(a.rsplit('.').next().unwrap_or(a)), b),
        None => (None, term),
    };
    let mut q = c
        .prepare_cached(&format!(
            "SELECT {DEF_COLS} FROM node n INDEXED BY node_sym_name
             WHERE n.project_id = ?1 AND n.kind = 'symbol' AND n.source = 'treesitter' AND n.name = ?2 COLLATE NOCASE LIMIT 50"
        ))
        .ok()?;
    let v: Vec<Def> = q
        .query_map(params![pid, name], map_def)
        .ok()?
        .flatten()
        .filter(|d| d.name == name)
        .filter(|d| qual.is_none_or(|w| d.parent.as_deref().is_some_and(|p| p.rsplit('.').next() == Some(w))))
        .collect();
    if v.len() == 1 { v.into_iter().next() } else { None }
}

/// The one indexed file `term` names: its exact path, or a unique path ending in it.
fn one_file(c: &Connection, pid: i64, term: &str) -> Option<(i64, String)> {
    let base = term.rsplit('/').next()?;
    let mut q = c
        .prepare_cached(
            "SELECT id, path FROM node INDEXED BY node_name WHERE name = ?1 COLLATE NOCASE AND project_id = ?2 AND kind = 'file' LIMIT 200",
        )
        .ok()?;
    let v: Vec<(i64, String)> = q
        .query_map(params![base, pid], |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default())))
        .ok()?
        .flatten()
        .filter(|(_, p)| p == term || p.ends_with(&format!("/{term}")))
        .collect();
    if let Some(x) = v.iter().find(|(_, p)| p == term) {
        return Some(x.clone());
    }
    if v.len() == 1 { v.into_iter().next() } else { None }
}

fn caller_note(c: &Connection, d: &Def) -> String {
    if is_callable_kind(&d.kind) {
        let n: i64 = c
            .query_row("SELECT COUNT(DISTINCT src) FROM ts_ref WHERE dst = ?1 AND dst != 0 AND kind = 'call'", [d.id], |r| r.get(0))
            .unwrap_or(0);
        format!("{n} caller{}", if n == 1 { "" } else { "s" })
    } else {
        let n: i64 = c.query_row("SELECT COUNT(*) FROM ts_ref WHERE dst = ?1 AND dst != 0", [d.id], |r| r.get(0)).unwrap_or(0);
        format!("{n} ref{}", if n == 1 { "" } else { "s" })
    }
}

/// Context for a prompt that names indexed definitions or files, or "".
pub fn prompt_context(input: &Value) -> Result<String> {
    let Some(prompt) = input["prompt"].as_str() else { return Ok(String::new()) };
    let terms = prompt_terms(prompt);
    if terms.is_empty() {
        return Ok(String::new());
    }
    let cwd = input["cwd"].as_str().map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let Some(c) = crate::db::open_ro() else { return Ok(String::new()) };
    let Some(p) = project::find(&c, &project::locate(&cwd))? else { return Ok(String::new()) };
    let mut rows = vec![];
    let mut seen = std::collections::HashSet::new();
    for t in terms.iter().take(40) {
        if rows.len() == PROMPT_ROWS {
            break;
        }
        let row = if is_path_like(t) {
            one_file(&c, p.id, t).filter(|(id, _)| seen.insert(*id)).map(|(id, path)| {
                let n: i64 = c
                    .query_row(
                        "SELECT COUNT(*) FROM node WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'",
                        params![p.id, path],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                format!("file {path}  {n} definitions  {}", handle("file", id))
            })
        } else {
            one_def(&c, p.id, t)
                .filter(|d| seen.insert(d.id))
                .map(|d| format!("{} {}  {}:{}  {}  {}", d.kind, d.qname(), d.path, d.line, caller_note(&c, &d), d.h()))
        };
        if let Some(r) = row {
            rows.push(r);
        }
    }
    if rows.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::from(
        "foxdb: the one indexed match for names in this prompt (fox_read target=<handle>; fox_graph target=<handle> rel=callers)\n",
    );
    for r in rows {
        out.push_str(&r);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_and_ops() {
        let w = |s: &str| shell_words(s).unwrap();
        assert_eq!(
            w("grep -rn 'a b' \"c\\\"d\" x 2>/dev/null | head -5"),
            vec![
                Tok::Word("grep".into()),
                Tok::Word("-rn".into()),
                Tok::Word("a b".into()),
                Tok::Word("c\"d".into()),
                Tok::Word("x".into()),
                Tok::Op("2>".into()),
                Tok::Word("/dev/null".into()),
                Tok::Op("|".into()),
                Tok::Word("head".into()),
                Tok::Word("-5".into()),
            ]
        );
        assert!(shell_words("grep $(echo x) .").is_none());
        assert!(shell_words("grep 'x .").is_none());
        assert_eq!(w("a && b")[1], Tok::Op("&&".into()));
        assert_eq!(w("a 2>&1")[1], Tok::Op("2>&".into()));
    }

    #[test]
    fn hit_lines() {
        assert_eq!(split_hit("a/b.go:12:x := 1", None), Some(("a/b.go", 12)));
        assert_eq!(split_hit("a/b.go:12:7:x", None), Some(("a/b.go", 12)));
        assert_eq!(split_hit("a:b.go:3:y", None), Some(("a:b.go", 3)));
        assert_eq!(split_hit("a/b.go-12-x", None), None);
        assert_eq!(split_hit("a/b.go:x 12:y", None), None);
        assert_eq!(split_hit("12:x", Some("f.go")), Some(("f.go", 12)));
        assert_eq!(split_hit("--", None), None);
    }

    #[test]
    fn test_paths() {
        for p in ["pkg/a/b_test.go", "tests/x.py", "a/test_x.py", "src/FooTest.java", "web/a.spec.ts", "pkg/scheduler/testing/f.go"] {
            assert!(is_test_path(p), "{p}");
        }
        for p in ["pkg/a/b.go", "src/contest.c", "django/test_utils_x/a.py"] {
            assert!(!is_test_path(p), "{p}");
        }
    }

    #[test]
    fn terms() {
        let t = prompt_terms(
            "Why does `Cart` call checkout_cart, PlaceOrder and pkg/x/y.go:12? Also Cart.total_price(). The Kubelet is fine; e.g. this.",
        );
        assert_eq!(t, ["Cart", "checkout_cart", "PlaceOrder", "pkg/x/y.go", "Cart.total_price"]);
        assert!(prompt_terms("Fix the bug in the scheduler, see https://x.io/a_b.go").is_empty());
    }
}
