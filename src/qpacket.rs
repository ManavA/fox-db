//! `fox packet "<question>"` (MCP: `fox_graph rel=packet target=<question>`): the answer context
//! for a question, computed up front. A small model rarely chains graph calls itself, so the packet
//! does the hops for it: for each symbol the question names, the definition line, `callers` and
//! 2-hop `impact`, as `fox code` prints them (accounting line and verdict included).
//!
//! Rows resolved only by a guess by name move under a `guesses by name` heading after the certain
//! rows of their answer. Over the budget, guesses go first, then deeper rows, then code and direct rows. A question naming no indexed symbol gets `fox find`
//! results instead, and the packet says so.

use crate::code::{self, CodeOpts};
use crate::project::Project;
use crate::query::{self, FindOpts, clip};
use anyhow::Result;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension};
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
static PATH_HINT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+):(\d+)`|`([^`]+)`:(\d+)|([\w./-]+\.\w+):(\d+)").unwrap());

fn location_captures(question: &str) -> impl Iterator<Item = regex::Captures<'_>> {
    // A quoted handle such as `c:4` also fits the quoted path:line pattern.
    PATH_HINT.captures_iter(question).filter(|m| m.get(1).is_none_or(|path| !matches!(path.as_str(), "c" | "s")))
}

/// Whether the token looks like a source file.
fn looks_like_source_file(s: &str) -> bool {
    crate::lang::for_path(s).is_some()
}

/// Candidate symbol names in the question, in order: backticked names, `a::b` and `a.b` paths,
/// snake_case and camelCase words.
pub fn names(question: &str) -> Vec<Name> {
    let mut out: Vec<Name> = vec![];
    let locations: Vec<_> = location_captures(question).filter_map(|m| m.get(0)).collect();
    for m in CODE_NAME.captures_iter(question) {
        let span = m.get(0).unwrap();
        if locations.iter().any(|loc| loc.start() < span.end() && span.start() < loc.end()) {
            continue;
        }
        let (raw, explicit) = match (m.get(1), m.get(2)) {
            (Some(b), _) => (b.as_str(), true),
            (_, Some(w)) => (w.as_str(), w.as_str().contains("::") || w.as_str().contains('.')),
            _ => continue,
        };
        let text = raw.trim_end_matches("()").trim_matches(|c: char| ".,;:()".contains(c));
        // `e.g` and `i.e` are prose.
        let prose = m.get(1).is_none() && matches!(text, "e.g" | "i.e" | "a.m" | "p.m");
        if text.is_empty()
            || prose
            || text.contains('/')
            || (m.get(1).is_none() && looks_like_source_file(text))
            || !text.chars().all(|c| c.is_alphanumeric() || "_:.".contains(c))
        {
            continue;
        }
        if !out.iter().any(|n| n.text == text) {
            out.push(Name { text: text.to_string(), explicit });
        }
    }
    out
}

/// `path:line` mentions in the question (`defined at diff.c:360`).
fn hints(question: &str, candidates: &[Target]) -> Vec<(String, i64)> {
    location_captures(question)
        .filter_map(|m| {
            let (path, line) = [(1, 2), (3, 4), (5, 6)].into_iter().find_map(|(p, l)| Some((m.get(p)?, m.get(l)?)))?;
            let mut full_path = path.as_str().trim_start_matches("./");
            if m.get(5).is_some() {
                // An unquoted path with spaces may have matched only its final word.
                let prefix = &question[..path.end()];
                for candidate in candidates.iter().filter(|t| t.path.contains(' ')) {
                    if candidate.path.len() > full_path.len()
                        && let Some(before) = prefix.strip_suffix(&candidate.path)
                        && !before.ends_with(|c: char| c.is_alphanumeric() || "_./-".contains(c))
                    {
                        full_path = &candidate.path;
                    }
                }
            }
            Some((full_path.to_string(), line.as_str().parse().ok()?))
        })
        .collect()
}

struct Target {
    handle: String,
    row: String,
    path: String,
    line: i64,
}

enum Resolution {
    Unique(Target),
    Ambiguous,
    Missing,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LocationMatch {
    ExactLine,
    SuffixLine,
    WrongLine,
    NoMatch,
}

/// A location hint can break a tie among indexed definitions.
fn resolve(c: &Connection, p: &Project, name: &str, question: &str) -> Result<Resolution> {
    let q = name.replace("::", ".");
    if let Some((prefix, hex)) = q.split_once(':')
        && matches!(prefix, "c" | "s")
        && let Ok(id) = i64::from_str_radix(hex, 16)
        && c.query_row("SELECT project_id FROM node WHERE id = ?1", [id], |row| row.get::<_, i64>(0)).optional()? != Some(p.id)
    {
        return Ok(Resolution::Missing);
    }
    let mut cands: Vec<Target> = vec![];
    if crate::swiftimport::has_symbols(c, p.id) {
        // The graph resolver only checks the final qualifier component. Check the whole name
        // against the indexed container and module before using a graph answer as evidence.
        let base = q.rsplit('.').next().unwrap_or(&q);
        cands = code::resolve(c, p.id, base)?
            .into_iter()
            .filter(|s| {
                !q.contains('.')
                    || s.qname().eq_ignore_ascii_case(&q)
                    || (!s.module.is_empty() && format!("{}.{}", s.module, s.qname()).eq_ignore_ascii_case(&q))
            })
            .filter_map(|s| Some(Target { handle: s.h(), row: s.row(), line: s.line, path: s.path.clone()? }))
            .collect();
    }
    if crate::tsindex::has_symbols(c, p.id) {
        let mut is_definition = c.prepare_cached("SELECT json_extract(meta, '$.d') IS NULL FROM node WHERE id = ?1")?;
        let mut tree_defs = Vec::new();
        for s in crate::tscode::resolve(c, p.id, &q)? {
            if is_definition.query_row([s.id], |row| row.get::<_, bool>(0))? {
                tree_defs.push(Target { handle: s.h(), row: s.row(), path: s.path, line: s.line });
            }
        }
        let same_location = |a: &Target, b: &Target| a.path == b.path && a.line == b.line;
        let additional: Vec<_> = tree_defs
            .iter()
            .enumerate()
            .filter(|(_, t)| !cands.iter().any(|s| same_location(s, t)) || tree_defs.iter().filter(|s| same_location(s, t)).count() > 1)
            .map(|(i, _)| i)
            .collect();
        cands.extend(tree_defs.into_iter().enumerate().filter_map(|(i, t)| additional.contains(&i).then_some(t)));
    }
    let hints = hints(question, &cands);
    let in_file = |t: &Target, f: &str| t.path == f || t.path.ends_with(&format!("/{f}"));
    let hints: Vec<_> = hints
        .into_iter()
        .map(|(file, line)| {
            let exact = cands.iter().any(|t| t.path == file);
            (file, line, exact)
        })
        .collect();
    let fit = |t: &Target| {
        hints
            .iter()
            .map(|(file, line, exact)| {
                if t.path != *file && (*exact || !in_file(t, file)) {
                    LocationMatch::NoMatch
                } else if t.line != *line {
                    LocationMatch::WrongLine
                } else if *exact {
                    LocationMatch::ExactLine
                } else {
                    LocationMatch::SuffixLine
                }
            })
            .min()
            .unwrap_or(LocationMatch::NoMatch)
    };
    let mut ranked: Vec<_> = cands.into_iter().map(|t| (fit(&t), t)).collect();
    ranked.sort_unstable_by_key(|(rank, _)| *rank);
    let mut ranked = ranked.into_iter();
    let Some((best, target)) = ranked.next() else {
        return Ok(Resolution::Missing);
    };
    if best == LocationMatch::WrongLine || ranked.next().is_some_and(|(second, _)| second == best) {
        return Ok(Resolution::Ambiguous);
    }
    Ok(Resolution::Unique(target))
}

/// What a line of a `fox code` answer is, for trimming.
#[derive(Clone, Copy, PartialEq)]
enum Part {
    /// Accounting, counts and notes: always kept.
    Note,
    /// A directory, file, depth or group heading: kept while a row under it is.
    Head,
    /// A row; `rank` orders dropping (lowest first).
    Row {
        rank: u8,
    },
    Guess {
        rank: u8,
    },
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

/// Rank certain rows by depth and ledger group. The trimmer handles marked guesses first.
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
                guessed.push(Line { text: format!("  {d}{at}"), part: Part::Guess { rank: r }, heads: vec![], code, kept: true });
            } else {
                handles.extend(h.map(String::from));
                certain.push(Line { text, part: Part::Row { rank: r }, heads, code, kept: true });
            }
            continue;
        }
        last = Dest::Certain;
        let t = l.trim_start();
        if let Some(d) = l
            .strip_prefix("depth ")
            .and_then(|s| s.split_once(':').map(|(d, _)| d))
            .or_else(|| l.strip_prefix('d'))
            .and_then(|d| d.parse::<usize>().ok())
        {
            depth = d;
            (dir, file, group, h_dir, h_file, h_group) = (String::new(), String::new(), None, None, None, None);
            h_depth = push_head(&mut certain, text, vec![]);
            if d == 1 {
                d1_at = Some(certain.len());
            }
        } else if [ASIDE_GROUPS, UNSURE_GROUPS, USE_GROUPS].concat().iter().any(|g| t.starts_with(g)) && !l.starts_with(' ') {
            (depth, group, h_depth, h_dir, h_file) = (1, Some(t.to_string()), None, None, None);
            h_group = push_head(&mut certain, text, vec![]);
        } else if !l.starts_with("  ") && t.ends_with('/') {
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
        && !f.is_empty()
    {
        return Some(f);
    }
    (l.starts_with(' ') && (t.contains('/') || looks_like_source_file(t))).then_some(t)
}

/// Characters kept for the note saying what the trim left out.
const NOTE_ROOM: usize = 170;

/// Drop guesses, then deeper rows, then code and direct rows until `lines` fit `room`
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
    let mut order: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| match l.part {
            Part::Guess { rank } => Some((false, rank, i)),
            Part::Row { rank } => Some((true, rank, i)),
            _ => None,
        })
        .collect();
    order.sort_by_key(|&(certain, rank, i)| (certain, rank, std::cmp::Reverse(i)));
    let deep = order.iter().take_while(|(certain, rank, _)| !certain || *rank < 3).count();
    let steps = order[..deep]
        .iter()
        .map(|&(_, _, i)| (i, true))
        .chain(order[deep..].iter().filter(|(_, _, i)| lines[*i].code).map(|&(_, _, i)| (i, false)))
        .chain(order[deep..].iter().map(|&(_, _, i)| (i, true)))
        .collect::<Vec<_>>();
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
    let room = budget.saturating_mul(4);
    if room == 0 {
        return Ok(String::new());
    }
    let mut targets: Vec<(String, Target)> = vec![];
    let mut missing: Vec<String> = vec![];
    let mut ambiguous: Vec<String> = vec![];
    for n in names(question) {
        if targets.len() == MAX_TARGETS {
            break;
        }
        match resolve(c, p, &n.text, question)? {
            Resolution::Unique(t) if !targets.iter().any(|(_, x)| x.handle == t.handle) => targets.push((n.text, t)),
            Resolution::Ambiguous => ambiguous.push(n.text),
            Resolution::Missing if n.explicit && !looks_like_source_file(&n.text) => missing.push(n.text),
            _ => {}
        }
    }
    if targets.is_empty() {
        return fallback(c, p, question, room, &missing, &ambiguous);
    }
    let mut lines: Vec<Line> = vec![];
    let note = |text: String| Line { text, part: Part::Note, heads: vec![], code: false, kept: true };
    let names: Vec<&str> = targets.iter().map(|(n, _)| n.as_str()).collect();
    lines.push(note(format!("fox packet: definition, callers and 2-hop impact of {}", names.join(", "))));
    if !ambiguous.is_empty() {
        lines.push(note(format!("ambiguous: {} (give a path:line to choose a definition)", ambiguous.join(", "))));
    }
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
            let out = if t.handle.starts_with("c:") {
                crate::tscode::run(c, p, &t.handle, rel, depth, 0, ANSWER_BUDGET)?.unwrap_or_default()
            } else {
                code::run(c, p, &t.handle, &o)?
            };
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
            format!("{what} left out to fit the budget (guesses and deeper rows first); a larger --budget or fox code NAME --rel callers|impact shows them"),
        );
    }
    Ok(fit_budget(out.join("\n") + "\n", room))
}

fn fit_budget(mut text: String, room: usize) -> String {
    const CUT: &str = "[packet cut at the budget]\n";
    if text.len() > room {
        let end = text.floor_char_boundary(room.saturating_sub(CUT.len()));
        if let Some(newline) = text[..end].rfind('\n') {
            text.truncate(newline + 1);
            text.push_str(CUT);
        } else {
            text.truncate(text.floor_char_boundary(room));
        }
    }
    text
}

/// No name resolves: `fox find` on the question, and a line saying so.
fn fallback(c: &Connection, p: &Project, question: &str, room: usize, missing: &[String], ambiguous: &[String]) -> Result<String> {
    static NOISE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bdo not edit files\.?").unwrap());
    let mut head =
        "no name in the question resolves to a unique indexed definition; fox find results for the question instead:\n".to_string();
    if !ambiguous.is_empty() {
        head += &format!("ambiguous: {} (give a path:line to choose a definition)\n", ambiguous.join(", "));
    }
    if !missing.is_empty() {
        head += &format!("not indexed: {}\n", missing.join(", "));
    }
    let remaining = room.saturating_sub(head.len());
    if remaining > 0 {
        let q = NOISE.replace_all(question, "");
        let o = FindOpts { kind: "any", all: false, budget: remaining / 4, limit: 12, offset: 0 };
        head += &query::find(c, Some(p), q.trim(), &o)?;
    }
    Ok(fit_budget(head, room))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_backticks_paths_and_code_words() {
        let got: Vec<String> = names("Who calls `set_helper_option` (defined at transport-helper.c:331), db::open_ro, Graph.walk and getUser? See `diff.c`, `c:4`, `s:8`, e.g. A plain word stays out; so does x.")
            .into_iter()
            .map(|n| n.text)
            .collect();
        assert_eq!(got, ["set_helper_option", "db::open_ro", "Graph.walk", "getUser", "diff.c", "c:4", "s:8"]);
        assert_eq!(
            hints("defined at transport-helper.c:331 and ./src/a.rs:9", &[]),
            [("transport-helper.c".into(), 331), ("src/a.rs".into(), 9)]
        );
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;

    #[test]
    fn explicit_single_character_names_are_kept() {
        assert_eq!(names("Who calls `f` and A.b?").len(), 2);
    }

    #[test]
    fn trimming_drops_guesses_before_certain_rows() {
        let mut lines = vec![];
        parse(&format!("d2\n  1 certain c:aa {}\n", "a".repeat(200)), true, &[], &mut lines);
        parse(&format!("  1 uncertain c:bb guess by name {}\n", "b".repeat(200)), false, &[], &mut lines);
        let room = lines.iter().map(|l| l.text.len() + 1).sum::<usize>() - 1;
        trim(&mut lines, room);
        assert!(lines.iter().any(|l| l.kept && l.text.contains("certain c:aa")));
        assert!(!lines.iter().any(|l| l.kept && l.text.contains("uncertain c:bb")));
    }

    #[test]
    fn guessed_rows_keep_paths_with_spaces() {
        let mut lines = vec![];
        parse("first.c (1)\n  :1 one c:aa\nfile with spaces.c (1)\n  :2 two c:bb guess by name\n", false, &[], &mut lines);
        assert!(lines.iter().any(|l| l.text.contains("file with spaces.c:2")));
    }

    #[test]
    fn swift_depth_and_dedup_accounting_are_preserved() {
        let mut lines = vec![];
        parse("depth 1: 1 symbols\n  1 caller s:aa\ndepth 2: 1 symbols\n  1 indirect s:bb\n", true, &["s:aa".into()], &mut lines);
        assert!(lines.iter().any(|l| l.text.contains("not repeated")));
        assert!(lines.iter().any(|l| l.text.contains("s:bb") && l.part == Part::Row { rank: 1 }));
    }
}
