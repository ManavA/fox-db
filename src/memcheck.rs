//! `fox mem check`: lines in agent memory that cite code which no longer exists.
//!
//! Sources are the memory foxdb already ingests for a project: Claude Code's auto-memory files for
//! the project's root, its `AGENTS.md` / `CLAUDE.md` / `CLAUDE.local.md` files, and `fox_memory
//! op=note` notes. Memory files are only read, never written.
//!
//! Precision over recall. Only code-shaped tokens are checked: backticked identifiers with a code
//! signal (`snake_case`, `camelCase`, `Type.method`, `a::b`, `f()`), `--flags` on a line that
//! names one of the project's own commands, and paths with a `/` and a known extension. A token
//! is flagged only when it resolves to nothing: no symbol of that name, no whole-word occurrence
//! in the project's code and config files (Markdown and changelogs excluded, so docs that repeat
//! a stale name do not hide it), no such file. Tokens that cannot be judged stay quiet: a lone
//! name on a line with nothing else that resolves (more often an OS error code or an API field
//! than this project's), a qualified name whose head is not an indexed symbol (another library,
//! a bundle id), a path whose first directory is not in the project, bare file names, all-caps
//! names without the project's prefix, lines that record an absence ("removed", "no `x`") and
//! anything in fenced blocks.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// One memory line with what it cites that is gone.
#[derive(Debug, Clone)]
pub struct Stale {
    /// File path (repo-relative, or `~/…` for Claude memory) or a note handle.
    pub source: String,
    /// 1-based.
    pub line: usize,
    pub text: String,
    pub missing: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub sources: usize,
    pub stale: Vec<Stale>,
    /// The code scan stopped at its byte cap: names it did not reach were not flagged.
    pub partial: bool,
}

struct Source {
    label: String,
    text: String,
    /// Repo-relative path, left out of the code scan.
    own: Option<String>,
    /// Directory its relative paths may also be relative to.
    base: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Cite {
    Path(String),
    Flag(String),
    Ident(String),
}

/// Claude Code's directory name for a project root: every non-alphanumeric character becomes `-`.
pub fn claude_dir_name(root: &Path) -> String {
    root.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// `path` with the home directory shown as `~`.
pub fn display_path(path: &Path) -> String {
    if let Some(h) = std::env::var_os("HOME").map(PathBuf::from).filter(|h| !h.as_os_str().is_empty())
        && let Ok(rest) = path.strip_prefix(&h)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", "CLAUDE.local.md"];

fn sources(c: &Connection, p: &crate::project::Project) -> Result<Vec<Source>> {
    let mut out = vec![];
    let mem = crate::inventory::claude_projects_root().join(claude_dir_name(&p.root)).join("memory");
    if let Ok(rd) = std::fs::read_dir(&mem) {
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|f| {
                f.extension().is_some_and(|x| x == "md")
                    && f.symlink_metadata().is_ok_and(|m| m.is_file())
                    && !crate::tiers::is_secret_name(&f.file_name().unwrap_or_default().to_string_lossy())
            })
            .collect();
        files.sort();
        for f in files {
            let name = f.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if let Some(text) = crate::guard::read_text(&f, &name, 1 << 20, false) {
                out.push(Source { label: display_path(&f), text, own: None, base: None });
            }
        }
    }
    let mut rels: BTreeSet<String> = BTreeSet::new();
    {
        let mut q = c.prepare_cached("SELECT path FROM node WHERE project_id = ?1 AND kind = 'file' AND path LIKE '%.md'")?;
        for r in q.query_map([p.id], |r| r.get::<_, String>(0))?.flatten() {
            if INSTRUCTION_FILES.contains(&r.rsplit('/').next().unwrap_or(&r)) {
                rels.insert(r);
            }
        }
    }
    for n in INSTRUCTION_FILES {
        if p.root.join(n).is_file() {
            rels.insert(n.to_string());
        }
    }
    for rel in rels {
        let abs = p.root.join(&rel);
        if let Some(text) = crate::guard::read_text(&abs, &rel, 1 << 20, false) {
            let base = abs.parent().map(Path::to_path_buf);
            out.push(Source { label: rel.clone(), text, own: Some(rel), base });
        }
    }
    let mut q = c.prepare_cached(
        "SELECT id, json_extract(meta, '$.text') FROM node WHERE project_id = ?1 AND kind = 'memory' AND source = 'note' ORDER BY id",
    )?;
    for (id, text) in q.query_map([p.id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)))?.flatten() {
        if let Some(text) = text {
            out.push(Source { label: crate::query::handle("memory", id), text, own: None, base: None });
        }
    }
    Ok(out)
}

/// The project's own command names: Cargo `[package]` and `[[bin]]` names, `package.json` name.
fn own_bins(root: &Path) -> HashSet<String> {
    let mut v = HashSet::new();
    if let Ok(t) = std::fs::read_to_string(root.join("Cargo.toml")) {
        let mut sec = String::new();
        for l in t.lines().map(str::trim) {
            if l.starts_with('[') {
                sec = l.to_string();
            } else if (sec == "[package]" || sec == "[[bin]]")
                && let Some(n) = l.strip_prefix("name").map(str::trim_start).and_then(|x| x.strip_prefix('='))
            {
                v.insert(n.trim().trim_matches('"').to_string());
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(root.join("package.json"))
        && let Ok(j) = serde_json::from_str::<serde_json::Value>(&t)
    {
        if let Some(n) = j["name"].as_str() {
            v.insert(n.rsplit('/').next().unwrap_or(n).to_string());
        }
        if let Some(b) = j["bin"].as_object() {
            v.extend(b.keys().cloned());
        }
    }
    v.retain(|n| !n.is_empty());
    v
}

fn flag_name(s: &str) -> Option<&str> {
    let f = s.strip_prefix("--")?;
    let f = f.split('=').next().unwrap_or(f);
    let mut ch = f.chars();
    (ch.next().is_some_and(|c| c.is_ascii_alphanumeric()) && f.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then_some(f)
}

/// Names that are about code style, not code.
const STYLE_WORDS: &[&str] = &["snake_case", "camelCase", "PascalCase", "SCREAMING_SNAKE_CASE", "kebab_case"];

/// An identifier with a code signal: `_`, `::`, `.`, `()`, or a lower-to-upper case change.
/// All-caps names count only with the project's prefix (`FOXDB_HOME`), not other tools' (`RUST_LOG`).
fn code_shaped(s: &str, caps_prefix: &str) -> bool {
    let t = s.strip_suffix("()").unwrap_or(s);
    if STYLE_WORDS.contains(&t) {
        return false;
    }
    if t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
        return !caps_prefix.is_empty() && t.starts_with(caps_prefix) && t.len() > caps_prefix.len();
    }
    let camel = t.chars().zip(t.chars().skip(1)).any(|(a, b)| a.is_lowercase() && b.is_uppercase());
    s.ends_with("()") || t.contains("::") || t.contains('.') || t.contains('_') || camel
}

fn strip_anchor(s: &str) -> &str {
    let s = s.split('#').next().unwrap_or(s);
    // `path:12` and `path:12-30`.
    match s.rsplit_once(':') {
        Some((p, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit() || c == '-') => p,
        _ => s,
    }
}

/// A path cite: has a `/` and its last part a known extension. `pkg/sub.Type` (a Go qualified
/// name) and bare file names (`setup.py`, often generic advice) are not paths.
fn path_like(p: &str) -> bool {
    let Some((_, last)) = p.trim_end_matches('/').rsplit_once('/') else { return false };
    let Some((stem, ext)) = last.rsplit_once('.') else { return false };
    !stem.is_empty()
        && (crate::lang::for_path(last).is_some() || PATH_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
        && crate::docref::is_path(p)
}

/// One backticked span. `own_cmd`: the line names one of the project's own commands, the only
/// context in which a standalone `--flag` is judged (other tools' flags look the same).
fn classify(span: &str, bins: &HashSet<String>, caps_prefix: &str, own_cmd: bool, out: &mut Vec<Cite>) {
    let s = span.trim();
    if s.contains(char::is_whitespace) {
        let mut words = s.split_whitespace();
        if words.next().is_some_and(|w| bins.contains(w)) {
            out.extend(words.filter_map(flag_name).map(|f| Cite::Flag(f.to_string())));
        }
        return;
    }
    if let Some(f) = flag_name(s) {
        if own_cmd {
            out.push(Cite::Flag(f.to_string()));
        }
        return;
    }
    let p = strip_anchor(s);
    if path_like(p) {
        out.push(Cite::Path(p.trim_start_matches("./").to_string()));
    } else if !s.contains('/') && crate::docref::is_ident(s) && code_shaped(s, caps_prefix) {
        out.push(Cite::Ident(s.to_string()));
    }
}

const PATH_EXTS: &[&str] = &["md", "json", "toml", "yaml", "yml", "sh", "txt", "lock", "cfg", "ini", "xml", "gradle", "plist"];

/// Words that say a line records an absence or a change: its cites are history, not claims.
fn records_absence(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    const WORDS: &[&str] = &[
        "removed",
        "deleted",
        "dropped",
        "renamed",
        "replaced",
        "deprecated",
        "no longer",
        "retired",
        "obsolete",
        "instead of",
        "gone",
        "used to",
        "formerly",
        "previously",
    ];
    // Whole words in the prose, not inside a cited name (`removed_function`, `gone.rs`).
    let prose: String = l.split('`').step_by(2).collect::<Vec<_>>().join(" ` ");
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '.' || c == '/';
    let in_prose = |w: &str| {
        prose.match_indices(w).any(|(i, _)| {
            let mut after = prose[i + w.len()..].chars();
            // A sentence's full stop ends the word; `gone.rs` does not.
            let ends = match after.next() {
                None => true,
                Some('.') => !after.next().is_some_and(char::is_alphanumeric),
                Some(c) => !word(c),
            };
            !prose[..i].chars().next_back().is_some_and(word) && ends
        })
    };
    WORDS.iter().any(|w| in_prose(w)) || ["no `", "not `", "without `", "never `"].iter().any(|w| l.contains(w))
}

/// Code-shaped citations per line, outside fenced blocks and lines that record an absence.
fn cites(text: &str, bins: &HashSet<String>, caps_prefix: &str) -> Vec<(usize, String, Vec<Cite>)> {
    let bare = regex::Regex::new(r"(?:^|[\s(\[<])(/?[A-Za-z0-9_@+.\-]+(?:/[A-Za-z0-9_@+.\-]+)+)").unwrap();
    let mut out = vec![];
    let mut fence = false;
    for (i, l) in text.lines().enumerate() {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence || records_absence(l) {
            continue;
        }
        let own_cmd = l.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_')).any(|w| bins.contains(w));
        let mut v = vec![];
        let parts: Vec<&str> = l.split('`').collect();
        let mut prose = String::new();
        for (k, part) in parts.iter().enumerate() {
            if k % 2 == 1 && k + 1 < parts.len() {
                classify(part, bins, caps_prefix, own_cmd, &mut v);
                prose.push(' ');
            } else {
                prose.push_str(part);
            }
        }
        for m in bare.captures_iter(&prose) {
            let p = strip_anchor(m[1].trim_end_matches(['.', ',', ';', ':', ')', '!', '?']));
            if path_like(p) {
                v.push(Cite::Path(p.trim_start_matches("./").to_string()));
            }
        }
        let mut seen = HashSet::new();
        v.retain(|c| seen.insert(c.clone()));
        if !v.is_empty() {
            out.push((i + 1, l.trim().to_string(), v));
        }
    }
    out
}

/// What the project holds: indexed file paths and symbol names, and a lazy scan of its code text.
struct Code<'a> {
    c: &'a Connection,
    p: &'a crate::project::Project,
    files: Vec<String>,
    exts: HashSet<String>,
}

impl<'a> Code<'a> {
    fn new(c: &'a Connection, p: &'a crate::project::Project) -> Result<Self> {
        let mut q = c.prepare_cached("SELECT path FROM node WHERE project_id = ?1 AND kind = 'file' AND path IS NOT NULL ORDER BY path")?;
        let files: Vec<String> = q.query_map([p.id], |r| r.get::<_, String>(0))?.flatten().collect();
        let exts = files.iter().filter_map(|f| f.rsplit('/').next()?.rsplit_once('.').map(|x| x.1.to_ascii_lowercase())).collect();
        Ok(Code { c, p, files, exts })
    }

    fn symbol(&self, name: &str) -> bool {
        self.c
            .query_row(
                "SELECT 1 FROM node WHERE project_id = ?1 AND kind = 'symbol' AND name = ?2 LIMIT 1",
                params![self.p.id, name],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    fn file_exists(&self, rel: &str) -> bool {
        let suffix = format!("/{rel}");
        self.p.root.join(rel).exists() || self.files.iter().any(|f| f == rel || f.ends_with(&suffix))
    }

    /// `Some(true)` exists, `Some(false)` gone, `None` cannot judge.
    fn path(&self, raw: &str, base: Option<&Path>) -> Option<bool> {
        let root = self.p.root.to_string_lossy().into_owned();
        let rel = if raw.starts_with('/') { raw.strip_prefix(&root)?.trim_start_matches('/').to_string() } else { raw.to_string() };
        let rel = rel.trim_end_matches('/');
        if rel.is_empty() || rel.starts_with("..") {
            return None;
        }
        if base.is_some_and(|b| b.join(rel).exists()) {
            return Some(true);
        }
        match rel.split_once('/') {
            Some((first, _)) => {
                if !self.p.root.join(first).exists() && base.is_none_or(|b| !b.join(first).exists()) {
                    return None;
                }
                Some(self.file_exists(rel))
            }
            None => {
                // A bare file name: only code files, and only languages the project has.
                crate::lang::for_path(rel)?;
                let ext = rel.rsplit_once('.')?.1.to_ascii_lowercase();
                if !self.exts.contains(&ext) {
                    return None;
                }
                Some(self.file_exists(rel))
            }
        }
    }
}

/// Bytes of code text scanned at most.
const SCAN_CAP: usize = 64 << 20;
const SCAN_FILE_MAX: u64 = 2 << 20;

fn scan_excluded(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let ext = name.rsplit_once('.').map(|x| x.1).unwrap_or("");
    matches!(ext, "md" | "markdown" | "mdx" | "txt" | "rst" | "adoc")
        || name.starts_with("changelog")
        || name.starts_with("changes")
        || name.starts_with("history")
}

/// Which `needles` occur in the project's code and config text (not docs, not `skip`).
/// Returns the found set and whether the scan stopped at its cap.
fn scan(pr: &Code, needles: &HashSet<String>, skip: &HashSet<String>) -> (HashSet<String>, bool) {
    let mut found = HashSet::new();
    if needles.is_empty() {
        return (found, false);
    }
    let mut total = 0usize;
    for f in &pr.files {
        if found.len() == needles.len() {
            break;
        }
        if scan_excluded(f) || skip.contains(f) {
            continue;
        }
        let abs = pr.p.root.join(f);
        let Ok(md) = std::fs::metadata(&abs) else { continue };
        if !md.is_file() || md.len() > SCAN_FILE_MAX {
            continue;
        }
        if total + md.len() as usize > SCAN_CAP {
            return (found, true);
        }
        total += md.len() as usize;
        let Some(text) = crate::guard::read_text(&abs, f, SCAN_FILE_MAX as usize, false) else { continue };
        for n in needles {
            if !found.contains(n) && has_word(&text, n) {
                found.insert(n.clone());
            }
        }
    }
    (found, false)
}

/// `needle` occurs in `text` as a whole word: not inside a longer identifier (or flag).
fn has_word(text: &str, needle: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_' || (needle.starts_with('-') && c == '-');
    text.match_indices(needle)
        .any(|(i, _)| !text[..i].chars().next_back().is_some_and(word) && !text[i + needle.len()..].chars().next().is_some_and(word))
}

fn segments(ident: &str) -> Vec<String> {
    let s = ident.strip_suffix("()").unwrap_or(ident);
    s.split("::").flat_map(|p| p.split('.')).map(String::from).collect()
}

/// Check every memory source of `p`.
pub fn check(c: &Connection, p: &crate::project::Project) -> Result<Report> {
    let srcs = sources(c, p)?;
    let bins = own_bins(&p.root);
    let caps_prefix = {
        let n: String = p.name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
        if n.is_empty() { n } else { format!("{n}_") }
    };
    let pr = Code::new(c, p)?;
    let per: Vec<Vec<(usize, String, Vec<Cite>)>> = srcs.iter().map(|s| cites(&s.text, &bins, &caps_prefix)).collect();
    // Words to look for in the code text: identifier segments that are not symbols, and flags.
    let mut words: HashMap<String, bool> = HashMap::new();
    let mut needles = HashSet::new();
    for c in per.iter().flatten().flat_map(|x| &x.2) {
        match c {
            Cite::Ident(s) => {
                for seg in segments(s) {
                    let sym = *words.entry(seg.clone()).or_insert_with(|| pr.symbol(&seg));
                    if !sym {
                        needles.insert(seg);
                    }
                }
            }
            Cite::Flag(f) => {
                needles.insert(format!("--{f}"));
                needles.insert(format!("\"{f}\""));
            }
            Cite::Path(_) => {}
        }
    }
    let skip: HashSet<String> = srcs.iter().filter_map(|s| s.own.clone()).collect();
    let (found, partial) = scan(&pr, &needles, &skip);
    // Unreached by a capped scan: unknown, so treated as present.
    let known = |w: &str| words.get(w).copied().unwrap_or(false) || found.contains(w) || (partial && needles.contains(w));
    // A line is about this project when one of its cites resolves: an existing project path or
    // an indexed symbol. A lone unknown name on a line with no such anchor is more likely another
    // system's (an OS error code, an API field) than this project's, so it is not judged.
    let anchored = |s: &Source, cs: &[Cite]| {
        cs.iter().any(|c| match c {
            Cite::Path(raw) => pr.path(raw, s.base.as_deref()) == Some(true),
            Cite::Ident(id) => segments(id).first().is_some_and(|g| words.get(g).copied().unwrap_or(false)),
            Cite::Flag(_) => false,
        })
    };
    let mut stale = vec![];
    for (s, lines) in srcs.iter().zip(per) {
        for (line, text, cs) in lines {
            let mut missing = vec![];
            let anchor = anchored(s, &cs);
            for c in cs {
                match c {
                    Cite::Path(raw) => {
                        if pr.path(&raw, s.base.as_deref()) == Some(false) {
                            missing.push(format!("`{raw}` (no such file)"));
                        }
                    }
                    Cite::Flag(f) => {
                        let gone = !(found.contains(&format!("--{f}")) || found.contains(&format!("\"{f}\"")) || partial);
                        if gone {
                            missing.push(format!("`--{f}` (no such flag)"));
                        }
                    }
                    Cite::Ident(id) => {
                        let segs = segments(&id);
                        // A qualified name is judged only when its head is an indexed symbol.
                        let head_sym = words.get(&segs[0]).copied().unwrap_or(false);
                        if segs.len() == 1 {
                            if anchor && !known(&segs[0]) {
                                missing.push(format!("`{id}` (not in the code)"));
                            }
                        } else if head_sym {
                            let gone: Vec<&String> = segs[1..].iter().filter(|g| !known(g)).collect();
                            if !gone.is_empty() {
                                let names: Vec<String> = gone.iter().map(|g| format!("`{g}`")).collect();
                                missing.push(format!("`{id}` (no {})", names.join(", ")));
                            }
                        }
                    }
                }
            }
            if !missing.is_empty() {
                stale.push(Stale { source: s.label.clone(), line, text, missing });
            }
        }
    }
    Ok(Report { sources: srcs.len(), stale, partial })
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n - 1).collect();
    t.push('…');
    t
}

/// `path:line | the line | what is missing`, one row per stale line.
pub fn render(r: &Report) -> String {
    let mut s = String::new();
    if r.stale.is_empty() {
        let _ = writeln!(s, "no stale code references in {} memory sources", r.sources);
    } else {
        let _ = writeln!(s, "{} memory lines cite code that is gone ({} sources checked):", r.stale.len(), r.sources);
        for x in &r.stale {
            let _ = writeln!(s, "{}:{} | {} | {}", x.source, x.line, clip(&x.text, 140), x.missing.join(", "));
        }
    }
    if r.partial {
        let _ = writeln!(s, "code scan stopped at {} MB: names it did not reach were not flagged", SCAN_CAP >> 20);
    }
    s
}

fn meta_get(c: &Connection, k: &str) -> Option<i64> {
    c.query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0)).optional().ok().flatten()
}

/// Run the check after an index run and store its count for the brief. Skipped when neither the
/// memory text nor the project's files changed since the last run.
pub fn record(w: &Connection, p: &crate::project::Project) -> Result<()> {
    if p.kind == "huge" {
        return Ok(());
    }
    let srcs = sources(w, p)?;
    let (n, newest): (i64, i64) = w.query_row(
        "SELECT COUNT(*), COALESCE(MAX(updated_at), 0) FROM node WHERE project_id = ?1 AND kind IN ('file', 'symbol')",
        [p.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut fp = format!("{n}|{newest}");
    for s in &srcs {
        fp.push_str(&s.label);
        fp.push('\0');
        fp.push_str(&s.text);
    }
    let fp = i64::from_str_radix(&crate::spare::hash(&fp)[..15], 16).unwrap_or(0);
    let (kc, kf) = (format!("memcheck:{}", p.id), format!("memcheck_fp:{}", p.id));
    if meta_get(w, &kf) == Some(fp) && meta_get(w, &kc).is_some() {
        return Ok(());
    }
    let count = check(w, p)?.stale.len() as i64;
    w.execute("INSERT INTO meta(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v", params![kc, count])?;
    w.execute("INSERT INTO meta(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v", params![kf, fp])?;
    Ok(())
}

/// The brief's line, only when the last recorded check found stale lines.
pub fn brief_line(c: &Connection, pid: i64) -> Option<String> {
    let n = meta_get(c, &format!("memcheck:{pid}"))?;
    (n > 0).then(|| format!("stale memory: {n} lines cite code that is gone (fox mem check)"))
}
