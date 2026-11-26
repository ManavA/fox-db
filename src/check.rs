//! `fox check`: the edit-completeness gate. It reads the working-tree diff against HEAD, finds
//! the functions whose signature lines the diff touches, and lists each caller whose enclosing
//! function (or, at file top level, whose line) the diff leaves alone. Callers resolved with
//! certainty block; callers guessed by name are listed apart and never block.
//!
//! The tree-sitter index answers, so `fresh` re-indexes changed files first: the agent's own
//! edits move lines and add callers the index has not seen yet.

use crate::project::Project;
use anyhow::{Result, bail};
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

/// One call site the diff did not touch.
#[derive(Debug, Clone)]
pub struct Site {
    pub path: String,
    pub line: i64,
    /// `Type.name c:1f`, or `(top)` for a file-level use.
    pub caller: String,
    /// The changed definitions it uses, comma-separated.
    pub target: String,
    /// Same-name candidates of a guessed resolution (0 or -1: certain).
    pub amb: i64,
}

impl Site {
    fn add_target(&mut self, target: &str) {
        if !self.target.split(", ").any(|name| name == target) {
            if !self.target.is_empty() {
                self.target.push_str(", ");
            }
            self.target.push_str(target);
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Changed definitions as `name path:line`.
    pub changed: Vec<String>,
    /// Certain call sites of the changed definitions, touched or not.
    pub certain: usize,
    pub missed: Vec<Site>,
    pub guesses: Vec<Site>,
}

impl Report {
    pub fn blocks(&self) -> bool {
        !self.missed.is_empty()
    }
}

/// Most rows of each list that [`render`] prints.
const MISSED_ROWS: usize = 30;
const GUESS_ROWS: usize = 10;
type Hunks = HashMap<String, Vec<(i64, i64)>>;

#[derive(Clone, Copy)]
struct Hunk {
    old: i64,
    old_len: i64,
    new: i64,
    new_len: i64,
}

impl Hunk {
    fn working_line(self, old: i64) -> Option<i64> {
        let after = self.old + self.old_len + i64::from(self.old_len == 0);
        let new_after = self.new + self.new_len + i64::from(self.new_len == 0);
        if old < after && self.old_len > 0 && old >= self.old {
            None
        } else if old >= after {
            Some(old + new_after - after)
        } else {
            Some(old)
        }
    }
}

#[derive(Default)]
struct FileDiff {
    tracked: bool,
    hunks: Vec<Hunk>,
}

impl FileDiff {
    fn working_line(&self, old: i64) -> Option<i64> {
        self.hunks.iter().rev().find(|h| h.old < old || h.old == old && h.old_len > 0).map_or(Some(old), |h| h.working_line(old))
    }

    fn touches_old(&self, first: i64, last: i64) -> bool {
        self.hunks.iter().any(|h| if h.old_len == 0 { first <= h.old && h.old < last } else { h.old <= last && first < h.old + h.old_len })
    }

    fn touches_new(&self, first: i64, last: i64) -> bool {
        !self.tracked
            || self
                .hunks
                .iter()
                .any(|h| if h.new_len == 0 { first <= h.new && h.new < last } else { h.new <= last && first < h.new + h.new_len })
    }
}

fn git_output(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd.output()?;
    if !out.status.success() {
        bail!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim())
    }
    Ok(out.stdout)
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_output(Command::new("git").arg("-C").arg(root).args(args))?)?)
}

fn diff(root: &Path, head: &str) -> Result<BTreeMap<String, FileDiff>> {
    let names = git_output(Command::new("git").arg("-C").arg(root).args(["diff", "--name-status", "-z", "--no-renames", head, "--"]))?;
    let mut files = BTreeMap::new();
    let mut entries = names.split(|b| *b == 0).filter(|s| !s.is_empty());
    while let Some(status) = entries.next() {
        let Some(path) = entries.next() else { bail!("missing path in git diff") };
        let Ok(path) = std::str::from_utf8(path) else { continue };
        if crate::lang::for_path(path).is_none() {
            continue;
        }
        // Git selects the literal path, so patch headers need no filename decoding.
        let patch = git(
            root,
            &["--literal-pathspecs", "diff", "-U0", "--no-color", "--no-ext-diff", "--no-textconv", "--no-renames", head, "--", path],
        )?;
        let mut hunks = Vec::new();
        for line in patch.lines().filter_map(|line| line.strip_prefix("@@ ")) {
            let range = |word: &str, sign| -> Result<(i64, i64)> {
                let word = word.strip_prefix(sign).ok_or_else(|| anyhow::anyhow!("invalid git hunk"))?;
                let (start, len) = word.split_once(',').unwrap_or((word, "1"));
                Ok((start.parse()?, len.parse()?))
            };
            let mut words = line.split_whitespace();
            let (old, old_len) = range(words.next().unwrap_or(""), '-')?;
            let (new, new_len) = range(words.next().unwrap_or(""), '+')?;
            hunks.push(Hunk { old, old_len, new, new_len });
        }
        files.insert(path.to_string(), FileDiff { tracked: status != b"A", hunks });
    }
    let untracked = git_output(Command::new("git").arg("-C").arg(root).args(["ls-files", "-z", "--others", "--exclude-standard"]))?;
    for path in untracked.split(|b| *b == 0).filter(|s| !s.is_empty()).filter_map(|s| std::str::from_utf8(s).ok()) {
        files.entry(path.to_string()).or_default();
    }
    Ok(files)
}

pub fn hunks(root: &Path) -> Result<Hunks> {
    Ok(diff(root, "HEAD")?
        .into_iter()
        .map(|(path, file)| {
            let ranges = if file.tracked {
                file.hunks.iter().map(|h| (h.new.max(1), (h.new + h.new_len - 1).max(h.new).max(1))).collect()
            } else {
                vec![(1, i64::MAX)]
            };
            (path, ranges)
        })
        .collect())
}

/// The checkout `cwd` is in must be the one the project indexes: a linked worktree's files are
/// not in the index.
fn same_checkout(p: &Project, cwd: &Path) -> Result<()> {
    let top = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?.trim());
    let canon = |x: &Path| std::fs::canonicalize(x).unwrap_or_else(|_| x.to_path_buf());
    if canon(&top) != canon(&p.root) {
        bail!("{} is a linked worktree; fox indexes {} only, so run fox check there", top.display(), p.root.display());
    }
    Ok(())
}

struct Signature<'a> {
    name: String,
    text: Option<&'a str>,
    line: i64,
    start: i64,
    end: i64,
}

fn signatures<'a>(path: &str, text: &'a str) -> Vec<Signature<'a>> {
    let Some(lang) = crate::lang::for_path(path) else { return Vec::new() };
    let parsed = crate::tsindex::extract(lang, text.as_bytes());
    parsed
        .defs
        .iter()
        .enumerate()
        .filter_map(|(i, def)| {
            if !crate::tsindex::is_callable_kind(&def.kind) && !def.field_fn {
                return None;
            }
            let name = parsed.container_of(i).map_or_else(|| def.name.clone(), |parent| format!("{parent}.{}", def.name));
            let (header, start) = def.signature(text).map_or((None, i64::from(def.line)), |(header, line)| (Some(header), line));
            let end = start + header.map_or(0, |header| header.bytes().filter(|&b| b == b'\n').count() as i64);
            Some(Signature { name, text: header, line: i64::from(def.line), start, end })
        })
        .collect()
}

#[derive(Default)]
struct Definitions {
    current: Vec<(String, String, i64)>,
    removed: Vec<(String, String, i64)>,
}

fn changed_definitions(root: &Path, head: &str, files: &BTreeMap<String, FileDiff>) -> Result<Definitions> {
    let mut out = Definitions::default();
    for (path, diff) in files {
        if crate::lang::for_path(path).is_none() || crate::tiers::is_denied_path(path) {
            continue;
        }
        let absolute = root.join(path);
        if std::fs::symlink_metadata(&absolute).is_ok_and(|m| !m.is_file() || m.len() > crate::tsindex::MAX_FILE) {
            continue;
        }
        let current = match std::fs::read_to_string(absolute) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => continue,
        };
        let old = if diff.tracked {
            let object = format!("{head}:{path}");
            if git(root, &["cat-file", "-s", &object])?.trim().parse::<u64>()? > crate::tsindex::MAX_FILE {
                continue;
            }
            match git(root, &["show", &object]) {
                Ok(text) => text,
                Err(_) => continue,
            }
        } else {
            String::new()
        };
        let old_defs = signatures(path, &old);
        let new_defs = signatures(path, &current);
        for new in &new_defs {
            if new.text.is_some() && !old_defs.iter().any(|old| old.name == new.name && (old.text.is_none() || old.text == new.text)) {
                out.current.push((path.clone(), new.name.clone(), new.line));
            }
        }
        for old in &old_defs {
            if old.text.is_some() && diff.touches_old(old.start, old.end) && !new_defs.iter().any(|new| new.name == old.name) {
                out.removed.push((path.clone(), old.name.clone(), old.line));
            }
        }
    }
    Ok(out)
}

// Use the ordinary resolver on HEAD so a rename or deletion has the same evidence on later runs.
fn head_index(root: &Path, head: &str, project_name: &str) -> Result<(tempfile::TempDir, Connection, Project)> {
    let dir = tempfile::tempdir()?;
    let tree = dir.path().join("tree");
    std::fs::create_dir(&tree)?;
    let mut git = Command::new("git");
    git.arg("-C").arg(root).args(["--work-tree"]).arg(&tree).env("GIT_INDEX_FILE", dir.path().join("index"));
    git_output(Command::new("git").arg("-C").arg(root).env("GIT_INDEX_FILE", dir.path().join("index")).args(["read-tree", head]))?;
    git_output(git.args(["checkout-index", "--all", "--prefix", &format!("{}/", tree.display())]))?;
    let mut c = Connection::open_in_memory()?;
    crate::db::migrate(&c)?;
    let p = crate::project::ensure(&c, &crate::project::locate(&tree))?;
    let overrides = std::sync::Arc::new(crate::inventory::overrides_for(project_name));
    {
        let tx = c.transaction()?;
        let mut add = tx.prepare(
            "INSERT INTO node(project_id,kind,key,name,path,size,mtime,tier,source,updated_at)
             VALUES (?1,'file',?2,?2,?2,?3,?4,3,'filesystem',0)",
        )?;
        for entry in crate::inventory::walk(&tree, &overrides, None).entries {
            if crate::lang::for_path(&entry.rel).is_some()
                && crate::tiers::classify_file(&entry.rel, entry.size, &overrides) == crate::tiers::Tier::Code
            {
                add.execute(rusqlite::params![p.id, entry.rel, entry.size as i64, entry.mtime])?;
            }
        }
        drop(add);
        tx.commit()?;
    }
    crate::tsindex::index_project(&mut c, &p)?;
    Ok((dir, c, p))
}

fn index(c: &mut Connection, p: &Project) -> Result<()> {
    let st = crate::inventory::index_project(c, p)?;
    crate::tsindex::index_project_with(c, p, st.delta.as_ref())?;
    Ok(())
}

#[derive(Default)]
struct Sites {
    certain: BTreeMap<(String, i64), (Site, bool)>,
    guesses: BTreeMap<(String, i64), Site>,
}

fn collect_sites(
    c: &Connection,
    p: &Project,
    root: &Path,
    files: &BTreeMap<String, FileDiff>,
    defs: &[(String, String, i64)],
    historical: bool,
    head: &str,
) -> Result<Sites> {
    let mut sites = Sites::default();
    let mut targets = HashMap::new();
    let mut q = c.prepare(
        "SELECT id, name, json_extract(meta,'$.parent'), json_extract(meta,'$.k'), COALESCE(json_extract(meta,'$.f'),0) FROM node
         WHERE project_id = ?1 AND kind = 'symbol' AND source = 'treesitter' AND path = ?2 AND line = ?3",
    )?;
    for (path, key, line) in defs {
        let name = key.rsplit('.').next().unwrap_or(key);
        let ids = q.query_map(rusqlite::params![p.id, path, line], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?, r.get::<_, bool>(4)?))
        })?;
        for id in ids {
            let (id, indexed, parent, kind, field_fn) = id?;
            let qualified = parent.filter(|p| !p.is_empty()).map_or_else(|| indexed.clone(), |p| format!("{p}.{indexed}"));
            if qualified == *key && (field_fn || crate::tsindex::is_callable_kind(&kind)) {
                targets.insert(id, name);
            }
        }
    }
    if targets.is_empty() {
        return Ok(sites);
    }
    let list = targets.keys().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut q = c.prepare(&format!(
        "SELECT r.path, r.line, r.src, s.name, json_extract(s.meta,'$.parent'), s.line, s.end_line, r.amb, r.dst, f.hash
         FROM ts_ref r LEFT JOIN node s ON s.id = r.src
         JOIN ts_file f ON f.project_id = r.project_id AND f.path = r.path
         WHERE r.project_id = ?1 AND r.kind = 'call' AND r.dst IN ({list}) ORDER BY r.path, r.line"
    ))?;
    let mut hashes = HashMap::new();
    let mut same_line =
        c.prepare("SELECT count(*) FROM ts_ref WHERE project_id = ?1 AND path = ?2 AND line = ?3 AND kind = 'call' AND name = ?4")?;
    let rows = q.query_map([p.id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, String>(9)?,
        ))
    })?;
    for row in rows {
        let (path, mut line, src, name, parent, start, end, amb, dst, indexed_hash) = row?;
        let target = targets[&dst];
        if historical && same_line.query_row(rusqlite::params![p.id, path, line, target], |r| r.get::<_, i64>(0))? != 1 {
            continue;
        }
        let file = files.get(&path);
        let (start, end) = (start.unwrap_or(line), end.unwrap_or(line));
        let working_hash = hashes
            .entry((false, path.clone()))
            .or_insert_with(|| std::fs::read(root.join(&path)).ok().map(|bytes| crate::tsindex::content_hash(&bytes)));
        let hit = if !historical && working_hash.as_ref() == Some(&indexed_hash) {
            file.is_some_and(|f| f.touches_new(start, end))
        } else {
            if working_hash.is_none() || file.is_some_and(|f| f.touches_old(line, line)) {
                continue;
            }
            let baseline_matches = hashes
                .entry((true, path.clone()))
                .or_insert_with(|| {
                    git(root, &["show", &format!("{head}:{path}")]).ok().map(|text| crate::tsindex::content_hash(text.as_bytes()))
                })
                .as_ref()
                == Some(&indexed_hash);
            if !baseline_matches {
                continue;
            }
            if let Some(file) = file {
                let Some(mapped) = file.working_line(line) else { continue };
                line = mapped;
            }
            file.is_some_and(|f| f.touches_old(start, end))
        };
        let caller = match (name, parent) {
            (Some(n), Some(p)) if !p.is_empty() && !historical => format!("{p}.{n} c:{src:x}"),
            (Some(n), _) if !historical => format!("{n} c:{src:x}"),
            (Some(n), Some(p)) if !p.is_empty() => format!("{p}.{n}"),
            (Some(n), _) => n,
            _ => "(top)".into(),
        };
        let key = (path.clone(), line);
        let site = || Site { path, line, caller, target: String::new(), amb };
        let entry = if matches!(amb, -1 | 0) {
            let (entry, touched) = sites.certain.entry(key).or_insert_with(|| (site(), hit));
            *touched &= hit;
            entry
        } else if !hit {
            sites.guesses.entry(key).or_insert_with(site)
        } else {
            continue;
        };
        entry.add_target(target);
    }
    Ok(sites)
}

/// Run the gate for the checkout at `cwd` (re-indexing first when `fresh`).
pub fn run(c: &mut Connection, p: &Project, cwd: &Path, fresh: bool) -> Result<Report> {
    same_checkout(p, cwd)?;
    let head = git(&p.root, &["rev-parse", "HEAD"])?;
    let head = head.trim();
    let files = diff(&p.root, head)?;
    let defs = changed_definitions(&p.root, head, &files)?;
    if defs.current.is_empty() && defs.removed.is_empty() {
        return Ok(Report::default());
    }
    if fresh {
        index(c, p)?;
    }
    let mut sites = collect_sites(c, p, &p.root, &files, &defs.current, false, head)?;
    if !defs.removed.is_empty() {
        let (_scratch, baseline, at_head) = head_index(&p.root, head, &p.name)?;
        let old_sites = collect_sites(&baseline, &at_head, &p.root, &files, &defs.removed, true, head)?;
        let mut live_calls = c.prepare(
            "SELECT r.name, r.dst, r.amb, f.hash FROM ts_ref r
             JOIN ts_file f ON f.project_id = r.project_id AND f.path = r.path
             WHERE r.project_id = ?1 AND r.path = ?2 AND r.line = ?3 AND r.kind = 'call'",
        )?;
        let mut hashes = HashMap::new();
        for (key, (site, hit)) in old_sites.certain {
            let hash = hashes
                .entry(site.path.clone())
                .or_insert_with(|| std::fs::read(p.root.join(&site.path)).ok().map(|bytes| crate::tsindex::content_hash(&bytes)));
            let calls = live_calls.query_map(rusqlite::params![p.id, site.path, site.line], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))
            })?;
            let mut calls_by_name = HashMap::new();
            for call in calls {
                let (name, dst, amb, indexed) = call?;
                if hash.as_ref() == Some(&indexed) {
                    calls_by_name.entry(name).and_modify(|single| *single = false).or_insert(dst == 0 || amb > 0);
                }
            }
            let remaining = site.target.split(", ").filter(|name| calls_by_name.get(*name) == Some(&true)).collect::<Vec<_>>();
            if !remaining.is_empty() {
                let (entry, touched) = sites.certain.entry(key).or_insert_with(|| {
                    let site = Site { target: String::new(), ..site.clone() };
                    (site, hit)
                });
                *touched &= hit;
                for name in remaining {
                    entry.add_target(name);
                }
            }
        }
        sites.guesses.extend(old_sites.guesses);
    }
    Ok(Report {
        changed: defs.current.iter().chain(&defs.removed).map(|(path, name, line)| format!("{name} {path}:{line}")).collect(),
        certain: sites.certain.len(),
        missed: sites.certain.into_values().filter_map(|(site, hit)| (!hit).then_some(site)).collect(),
        guesses: sites.guesses.into_values().collect(),
    })
}

fn row(s: &Site) -> String {
    format!("  {}:{} {} calls {}{}", s.path, s.line, s.caller, s.target, crate::tscode::guess_note(s.amb))
}

fn rows(out: &mut String, v: &[Site], cap: usize) {
    for s in v.iter().take(cap) {
        out.push_str(&row(s));
        out.push('\n');
    }
    if v.len() > cap {
        out.push_str(&format!("  +{} more\n", v.len() - cap));
    }
}

/// The gate's answer: a head line, then one row per missed site, then the guessed ones.
pub fn render(r: &Report) -> String {
    let mut out = if r.changed.is_empty() {
        "fox check: ok; the diff changes no function signature\n".to_string()
    } else if r.certain == 0 {
        "fox check: ok; the changed definitions have no certain callers\n".to_string()
    } else if r.missed.is_empty() {
        format!("fox check: ok; all {} certain callers of the changed definitions are in the diff\n", r.certain)
    } else {
        format!("fox check: {} of {} certain callers of the changed definitions are not in the diff\n", r.missed.len(), r.certain)
    };
    if !r.changed.is_empty() {
        out.push_str(&format!("changed: {}\n", r.changed.join(", ")));
    }
    rows(&mut out, &r.missed, MISSED_ROWS);
    if !r.guesses.is_empty() {
        out.push_str("guessed callers not in the diff (not blocking; confirm the ones that matter):\n");
        rows(&mut out, &r.guesses, GUESS_ROWS);
    }
    out
}

/// `fox check [--project P] [--no-fresh]` (`--fresh`, the default, is accepted too).
pub fn cli(a: &crate::cli::Args, cwd: &Path) -> Result<Report> {
    let mut c = crate::db::open_rw()?;
    let p = match a.opt.get("project") {
        Some(name) => crate::project::by_name(&c, name)?.ok_or_else(|| anyhow::anyhow!("no project named {name}; see fox status"))?,
        None => crate::project::find(&c, &crate::project::locate(cwd))?
            .ok_or_else(|| anyhow::anyhow!("project not indexed; run `fox index` first"))?,
    };
    let at = if a.opt.contains_key("project") { p.root.clone() } else { cwd.to_path_buf() };
    run(&mut c, &p, &at, !a.flags.contains("no-fresh"))
}
