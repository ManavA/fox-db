//! `fox packet "<question>"` (MCP: `fox_graph rel=packet target=<question>`): the answer context
//! for a question, computed up front. A small model rarely chains graph calls itself, so the packet
//! does the hops for it: for each symbol the question names, the definition line, `callers` and
//! 2-hop `impact`, as `fox code` prints them (accounting line and verdict included).
//!
//! Rows resolved only by a guess by name move under a `guesses by name` heading after the certain
//! rows of their answer. Over the budget, rows go deepest first and guesses before certain rows
//! (code first, then the rows themselves). A question naming no indexed symbol gets `fox find`
//! results instead, and the packet says so.

use crate::code::{self, CodeOpts};
use crate::project::Project;
use crate::query::{self, FindOpts, clip};
use anyhow::Result;
use regex::Regex;
use rusqlite::Connection;
use std::sync::LazyLock;

/// Default packet budget in tokens (about 4 characters each).
pub const DEFAULT_BUDGET: usize = 800;
/// Most symbols a packet expands.
const MAX_TARGETS: usize = 4;
/// Budget each underlying `fox code` answer gets before the packet trims the whole.
const ANSWER_BUDGET: usize = 4000;

/// A name the question mentions; `explicit` when backticked or qualified (worth saying it is not indexed).
#[derive(Debug, PartialEq)]
pub struct Name {
    pub text: String,
    pub explicit: bool,
}

static CODE_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"`([^`\s]+)`|\b([A-Za-z_]\w*(?:(?:::|\.)[A-Za-z_]\w*)+|[a-z]\w*_\w+|[A-Z][a-z0-9]+[A-Z]\w*|[a-z][a-z0-9]+[A-Z]\w*)\b")
        .unwrap()
});
static PATH_HINT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([\w./-]+\.\w+):(\d+)").unwrap());

/// A source file name (`diff.c`, `src/a.rs`) is a path, not a symbol.
fn is_path(s: &str) -> bool {
    s.contains('/') || crate::lang::for_path(s).is_some()
}

/// Candidate symbol names in the question, in order: backticked names, `a::b` and `a.b` paths,
/// snake_case and camelCase words.
pub fn names(question: &str) -> Vec<Name> {
    let mut out: Vec<Name> = vec![];
    for m in CODE_NAME.captures_iter(question) {
        let (raw, explicit) = match (m.get(1), m.get(2)) {
            (Some(b), _) => (b.as_str(), true),
            (_, Some(w)) => (w.as_str(), w.as_str().contains("::") || w.as_str().contains('.')),
            _ => continue,
        };
        let text = raw.trim_end_matches("()").trim_matches(|c: char| ".,;:()".contains(c));
        // `e.g` and `i.e` are prose.
        let prose = text.split([':', '.']).filter(|s| !s.is_empty()).all(|s| s.len() == 1);
        if text.is_empty() || prose || is_path(text) || !text.chars().all(|c| c.is_alphanumeric() || "_:.".contains(c)) {
            continue;
        }
        if !out.iter().any(|n| n.text == text) {
            out.push(Name { text: text.to_string(), explicit });
        }
    }
    out
}

/// `path:line` mentions in the question (`defined at diff.c:360`).
fn hints(question: &str) -> Vec<(String, i64)> {
    PATH_HINT.captures_iter(question).filter_map(|m| Some((m[1].trim_start_matches("./").to_string(), m[2].parse().ok()?))).collect()
}

struct Target {
    handle: String,
    row: String,
    path: String,
    line: i64,
}

/// Best definition for `name`: one at a `path:line` the question gives, else one in a file it
/// names, else the index's own pick.
fn resolve(c: &Connection, p: &Project, name: &str, hints: &[(String, i64)]) -> Result<Option<Target>> {
    let mut cands: Vec<Target> = vec![];
    for q in dedup([name.replace("::", "."), name.rsplit(['.', ':']).next().unwrap_or(name).to_string()]) {
        if crate::swiftimport::has_symbols(c, p.id) {
            cands = code::resolve(c, p.id, &q)?
                .into_iter()
                .filter_map(|s| Some(Target { handle: s.h(), row: s.row(), line: s.line, path: s.path.clone()? }))
                .collect();
        }
        if cands.is_empty() && crate::tsindex::has_symbols(c, p.id) {
            cands = crate::tscode::resolve(c, p.id, &q)?
                .into_iter()
                .map(|s| Target { handle: s.h(), row: s.row(), path: s.path.clone(), line: s.line })
                .collect();
        }
        if !cands.is_empty() {
            break;
        }
    }
    let in_file = |t: &Target, f: &str| t.path == f || t.path.ends_with(&format!("/{f}"));
    let fit = |t: &Target| {
        if hints.iter().any(|(f, l)| t.path == *f && t.line == *l) {
            0
        } else if hints.iter().any(|(f, l)| in_file(t, f) && t.line == *l) {
            1
        } else if hints.iter().any(|(f, _)| t.path == *f) {
            2
        } else if hints.iter().any(|(f, _)| in_file(t, f)) {
            3
        } else {
            4
        }
    };
    let best = cands.iter().enumerate().min_by_key(|(i, t)| (fit(t), *i)).map(|(i, _)| i);
    Ok(best.map(|i| cands.swap_remove(i)))
}

fn dedup<const N: usize>(v: [String; N]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for s in v {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// What a line of a `fox code` answer is, for trimming.
#[derive(Clone, Copy, PartialEq)]
enum Part {
    /// Accounting, counts and notes: always kept.
    Note,
    /// A directory, file, depth or group heading: kept while a row under it is.
    Head,
    /// A row; `rank` orders dropping (lowest first).
    Row { rank: u8 },
    /// A code line under the row before it.
    Code,
}

struct Line {
    text: String,
    part: Part,
    /// Indices of the headings this line sits under.
    heads: Vec<usize>,
    /// For a row, its code lines and inline ` | code`.
    code: bool,
    kept: bool,
}

/// Ledger groups that only round out the count (dropped before any d1 row).
const ASIDE_GROUPS: &[&str] =
    &["declarations and prototypes", "same-name definitions elsewhere", "import lines", "comment lines", "strings"];
/// Ledger groups whose rows may or may not use the target.
const UNSURE_GROUPS: &[&str] = &["unresolved mentions", "mentions in its own file", "not parsed"];
/// Ledger groups listing certain non-call uses.
const USE_GROUPS: &[&str] = &["references, not calls", "stores a function in it"];

fn is_guess(row: &str) -> bool {
    let head = row.split_once(" | ").map_or(row, |x| x.0);
    head.contains(" guess 1 of ") || head.contains(" guess by name") || head.contains(" guess (via")
}

/// Drop rank of a row (lowest goes first): d2+ guesses, d2+ rows, aside groups, impact d1
/// guesses, caller guesses and unsure mentions, the ledger's certain non-call uses (impact d1
/// names their functions), then certain callers and impact d1 rows.
fn rank(impact: bool, depth: usize, guess: bool, group: Option<&str>) -> u8 {
    match group {
        Some(g) if ASIDE_GROUPS.iter().any(|a| g.starts_with(a)) => return 2,
        Some(g) if UNSURE_GROUPS.iter().any(|a| g.starts_with(a)) => return 4,
        Some(g) if USE_GROUPS.iter().any(|a| g.starts_with(a)) => return 5,
        _ => {}
    }
    match (depth >= 2, impact, guess) {
        (true, _, true) => 0,
        (true, _, false) => 1,
        (false, true, true) => 3,
        (false, false, true) => 4,
        (false, _, false) => 6,
    }
}

static HANDLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[cs]:[0-9a-f]+\b").unwrap());

/// The symbol handle a row names (before its code).
fn row_handle(row: &str) -> Option<&str> {
    HANDLE.find(row.split_once(" | ").map_or(row, |x| x.0)).map(|m| m.as_str())
}

/// Where a row and its code lines go.
#[derive(Clone, Copy, PartialEq)]
enum Dest {
    Certain,
    Guess,
    /// An impact d1 row naming a caller already listed.
    Repeat,
}

/// Split one `fox code` answer into lines, moving guessed rows (with their full path) under a
/// `guesses by name` heading at the end of the answer. Impact d1 rows naming a symbol in
/// `callers` are counted, not repeated. Returns the handles of the certain rows.
fn parse(answer: &str, impact: bool, callers: &[String], lines: &mut Vec<Line>) -> Vec<String> {
    let mut certain: Vec<Line> = vec![];
    let mut guessed: Vec<Line> = vec![];
    let (mut dir, mut file, mut depth) = (String::new(), String::new(), 1usize);
    let mut group: Option<String> = None;
    // Heading slots in `certain`: depth, dir, file, group.
    let (mut h_depth, mut h_dir, mut h_file, mut h_group): (Option<usize>, Option<usize>, Option<usize>, Option<usize>) =
        (None, None, None, None);
    let mut last = Dest::Certain;
    let (mut repeats, mut d1_at) = (0usize, None);
    let mut handles: Vec<String> = vec![];
    let base = lines.len();
    for l in answer.lines() {
        let text = l.to_string();
        let push_head = |certain: &mut Vec<Line>, text: String, heads: Vec<usize>| {
            certain.push(Line { text, part: Part::Head, heads, code: false, kept: true });
            Some(base + certain.len() - 1)
        };
        if l.starts_with("   :") {
            if last == Dest::Repeat {
                continue;
            }
            let to = if last == Dest::Guess { &mut guessed } else { &mut certain };
            let heads = to.last().map(|r| r.heads.clone()).unwrap_or_default();
            if let Some(r) = to.last_mut() {
                r.code = true;
            }
            to.push(Line { text, part: Part::Code, heads, code: true, kept: true });
            continue;
        }
        if let Some(rest) = l.strip_prefix("  ")
            && !rest.starts_with(' ')
            && !rest.is_empty()
        {
            let guess = group.is_none() && is_guess(l);
            let r = rank(impact, depth, guess, group.as_deref());
            let heads: Vec<usize> = [h_depth, h_dir, h_file, h_group].into_iter().flatten().collect();
            let code = l.contains(" | ");
            let h = row_handle(l);
            last = if guess {
                Dest::Guess
            } else if impact && depth == 1 && group.is_none() && h.is_some_and(|h| callers.iter().any(|c| c == h)) {
                Dest::Repeat
            } else {
                Dest::Certain
            };
            if last == Dest::Repeat {
                repeats += 1;
            } else if guess {
                let at = rest.strip_prefix(':').map_or(rest.to_string(), |r| format!("{file}:{r}"));
                let d = if impact { format!("d{depth} ") } else { String::new() };
                guessed.push(Line { text: format!("  {d}{at}"), part: Part::Row { rank: r }, heads: vec![], code, kept: true });
            } else {
                handles.extend(h.map(String::from));
                certain.push(Line { text, part: Part::Row { rank: r }, heads, code, kept: true });
            }
            continue;
        }
        last = Dest::Certain;
        let t = l.trim_start();
        if let Some(d) = l.strip_prefix('d').and_then(|d| d.parse::<usize>().ok()) {
            depth = d;
            (dir, file, group, h_dir, h_file, h_group) = (String::new(), String::new(), None, None, None, None);
            h_depth = push_head(&mut certain, text, vec![]);
            if d == 1 {
                d1_at = Some(certain.len());
            }
        } else if [ASIDE_GROUPS, UNSURE_GROUPS, USE_GROUPS].concat().iter().any(|g| t.starts_with(g)) && !l.starts_with(' ') {
            (depth, group, h_depth, h_dir, h_file) = (1, Some(t.to_string()), None, None, None);
            h_group = push_head(&mut certain, text, vec![]);
        } else if !l.starts_with("  ") && t.ends_with('/') && !t.contains(' ') {
            (dir, file) = (t.to_string(), String::new());
            h_dir = push_head(&mut certain, text, h_depth.into_iter().collect());
            h_file = None;
        } else if let Some(f) = file_heading(l) {
            if !l.starts_with(' ') {
                dir.clear();
                h_dir = None;
            }
            file = if f.contains('/') { f.to_string() } else { format!("{dir}{f}") };
            h_file = push_head(&mut certain, text, [h_depth, h_dir].into_iter().flatten().collect());
        } else {
            certain.push(Line { text, part: Part::Note, heads: vec![], code: false, kept: true });
        }
    }
    if let Some(at) = d1_at.filter(|_| repeats > 0) {
        let text = format!("({repeats} more d1 dependents are the callers listed above, not repeated)");
        certain.insert(at, Line { text, part: Part::Note, heads: vec![], code: false, kept: true });
        // Heading indices past `at` moved down one.
        for l in certain.iter_mut().skip(at + 1) {
            for h in l.heads.iter_mut().filter(|h| **h >= base + at) {
                *h += 1;
            }
        }
    }
    // The answer's last line (`~ tree-sitter, name-resolved`) stays last.
    let tail = certain.last().filter(|l| l.part == Part::Note && l.text.starts_with('~')).is_some().then(|| certain.pop()).flatten();
    lines.extend(certain);
    if !guessed.is_empty() {
        lines.push(Line {
            text: "guesses by name (not certain; confirm each before relying on it):".into(),
            part: Part::Head,
            heads: vec![],
            code: false,
            kept: true,
        });
        let at = lines.len() - 1;
        lines.extend(guessed.into_iter().map(|mut g| {
            g.heads = vec![at];
            g
        }));
    }
    lines.extend(tail);
    handles
}

/// `transport.c (19)`, ` exec.c (1)` (callers) or ` builtin/diff.c` (impact): the file named.
fn file_heading(l: &str) -> Option<&str> {
    let t = l.trim_start();
    if l.len() - t.len() > 1 || t.is_empty() {
        return None;
    }
    if let Some((f, n)) = t.rsplit_once(" (")
        && n.strip_suffix(')').is_some_and(|n| n.parse::<usize>().is_ok())
        && !f.contains(' ')
    {
        return Some(f);
    }
    (l.starts_with(' ') && !t.contains(' ') && is_path(t)).then_some(t)
}

/// Characters kept for the note saying what the trim left out.
const NOTE_ROOM: usize = 170;

/// Drop rows (deepest and guesses first; code before rows from d1 on) until `lines` fit `room`
/// characters with the note. Headings left with no row go too. Returns (rows dropped, rows whose
/// code was dropped).
fn trim(lines: &mut [Line], room: usize) -> (usize, usize) {
    let mut size: usize = lines.iter().filter(|l| l.kept).map(|l| l.text.len() + 1).sum();
    if size <= room {
        return (0, 0);
    }
    let mut alive = vec![0usize; lines.len()];
    for l in lines.iter().filter(|l| l.kept && l.part != Part::Head) {
        for &h in &l.heads {
            alive[h] += 1;
        }
    }
    let mut order: Vec<(u8, usize)> =
        lines.iter().enumerate().filter_map(|(i, l)| if let Part::Row { rank } = l.part { Some((rank, i)) } else { None }).collect();
    order.sort_by_key(|&(r, i)| (r, std::cmp::Reverse(i)));
    let deep = order.iter().take_while(|(r, _)| *r < 3).count();
    let steps: Vec<(usize, bool)> = order[..deep]
        .iter()
        .map(|&(_, i)| (i, true))
        .chain(order[deep..].iter().filter(|(_, i)| lines[*i].code).map(|&(_, i)| (i, false)))
        .chain(order[deep..].iter().map(|&(_, i)| (i, true)))
        .collect();
    let (mut dropped, mut stripped) = (0, 0);
    let mut drop = |lines: &mut [Line], i: usize, size: &mut usize| {
        lines[i].kept = false;
        *size -= lines[i].text.len() + 1;
        for h in lines[i].heads.clone() {
            alive[h] -= 1;
            if alive[h] == 0 && lines[h].kept {
                lines[h].kept = false;
                *size -= lines[h].text.len() + 1;
            }
        }
    };
    for (i, whole) in steps {
        if size + NOTE_ROOM <= room {
            break;
        }
        if !lines[i].kept {
            continue;
        }
        if whole {
            dropped += 1;
        } else {
            stripped += 1;
            if let Some((a, b)) = lines[i].text.split_once(" | ") {
                size -= b.len() + 3;
                lines[i].text = a.to_string();
            }
        }
        let mut j = i + 1;
        while j < lines.len() && lines[j].part == Part::Code {
            if lines[j].kept {
                drop(lines, j, &mut size);
            }
            j += 1;
        }
        if whole {
            drop(lines, i, &mut size);
        }
    }
    (dropped, stripped)
}

/// Drop headings with no kept row under them (rows moved under `guesses by name` leave some).
fn prune(lines: &mut [Line]) {
    let mut alive = vec![false; lines.len()];
    for l in lines.iter().filter(|l| l.kept && l.part != Part::Head) {
        for &h in &l.heads {
            alive[h] = true;
        }
    }
    for (l, a) in lines.iter_mut().zip(alive) {
        if l.part == Part::Head && !a {
            l.kept = false;
        }
    }
}

/// The packet for `question` in project `p`, at most `budget` tokens.
pub fn run(c: &Connection, p: &Project, question: &str, budget: usize) -> Result<String> {
    let room = budget.max(64) * 4;
    let hints = hints(question);
    let mut targets: Vec<(String, Target)> = vec![];
    let mut missing: Vec<String> = vec![];
    for n in names(question) {
        if targets.len() == MAX_TARGETS {
            break;
        }
        match resolve(c, p, &n.text, &hints)? {
            Some(t) if !targets.iter().any(|(_, x)| x.handle == t.handle) => targets.push((n.text, t)),
            Some(_) => {}
            None if n.explicit => missing.push(n.text),
            None => {}
        }
    }
    if targets.is_empty() {
        return fallback(c, p, question, budget, &missing);
    }
    let mut lines: Vec<Line> = vec![];
    let note = |text: String| Line { text, part: Part::Note, heads: vec![], code: false, kept: true };
    let names: Vec<&str> = targets.iter().map(|(n, _)| n.as_str()).collect();
    lines.push(note(format!("fox packet: definition, callers and 2-hop impact of {}", names.join(", "))));
    if !missing.is_empty() {
        lines.push(note(format!("not indexed: {}", missing.join(", "))));
    }
    for (name, t) in &targets {
        let src = crate::guard::read_text(&p.root.join(&t.path), &t.path, 16 << 20, false)
            .and_then(|s| s.lines().nth((t.line as usize).saturating_sub(1)).map(|l| clip(l.trim(), 160)));
        lines.push(note(format!("== {name}")));
        lines.push(note(format!("def {}{}", t.row, src.map(|s| format!(" | {s}")).unwrap_or_default())));
        let mut callers: Vec<String> = vec![];
        for (rel, depth) in [("callers", 1), ("impact", 2)] {
            let o = CodeOpts { rel, depth, budget: ANSWER_BUDGET, cursor: 0, fallback: false };
            let out = code::run(c, p, &t.handle, &o)?;
            callers = parse(&out, rel == "impact", &callers, &mut lines);
        }
    }
    prune(&mut lines);
    let (dropped, stripped) = trim(&mut lines, room);
    let mut out: Vec<String> = lines.into_iter().filter(|l| l.kept).map(|l| l.text).collect();
    if dropped + stripped > 0 {
        let what = match (dropped, stripped) {
            (0, s) => format!("code of {s} rows"),
            (d, 0) => format!("{d} rows"),
            (d, s) => format!("{d} rows and the code of {s} more"),
        };
        out.insert(
            1,
            format!("{what} left out to fit the budget (deepest and guesses first); a larger --budget or fox code NAME --rel callers|impact shows them"),
        );
    }
    let mut text = String::new();
    let whole = out.iter().map(|l| l.len() + 1).sum::<usize>() <= room;
    for l in out {
        if !whole && text.len() + l.len() + 30 > room {
            text.push_str("[packet cut at the budget]\n");
            break;
        }
        text.push_str(&l);
        text.push('\n');
    }
    Ok(text)
}

/// No name resolves: `fox find` on the question, and a line saying so.
fn fallback(c: &Connection, p: &Project, question: &str, budget: usize, missing: &[String]) -> Result<String> {
    static NOISE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bdo not edit files\.?").unwrap());
    let q = NOISE.replace_all(question, "");
    let mut head = "no name in the question resolves to an indexed definition".to_string();
    if !missing.is_empty() {
        head += &format!(" (not indexed: {})", missing.join(", "));
    }
    head += "; fox find results for the question instead:\n";
    let o = FindOpts { kind: "any", all: false, budget: budget.saturating_sub(head.len() / 4 + 4).max(32), limit: 12, offset: 0 };
    Ok(head + &query::find(c, Some(p), q.trim(), &o)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_backticks_paths_and_code_words() {
        let got: Vec<String> = names("Who calls `set_helper_option` (defined at transport-helper.c:331), db::open_ro, Graph.walk and getUser? See `diff.c`, e.g. A plain word stays out; so does x.")
            .into_iter()
            .map(|n| n.text)
            .collect();
        assert_eq!(got, ["set_helper_option", "db::open_ro", "Graph.walk", "getUser"]);
        assert_eq!(
            hints("defined at transport-helper.c:331 and ./src/a.rs:9"),
            [("transport-helper.c".into(), 331), ("src/a.rs".into(), 9)]
        );
    }
}
