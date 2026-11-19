//! Tiered file/folder inventory with incremental refresh (size+mtime), markdown/milestone parsing,
//! and the global memory corpus (~/.claude/projects/*/memory).

use crate::markdown;
use crate::outbox::{self, Sink, TBL_BODY, TBL_NODE, TBL_TRI};
use crate::project::Project;
use crate::tiers::{self, Overrides, Tier};
use crate::{db, guard};
use anyhow::Result;
use ignore::WalkBuilder;
use rusqlite::{Connection, Transaction, params};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SUMMARY_MAX_ENTRIES: usize = 300_000;
const FTS_BODY_MAX: usize = 32 * 1024;

/// The files one index run changed or removed, when it knew them without reading every file row
/// (a tree with a walk record): the tree-sitter pass of the same run then looks only at these.
#[derive(Debug, Clone)]
pub struct Delta {
    /// The project's file generation before this run.
    pub filegen: i64,
    pub paths: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub delta: Option<Delta>,
    pub files_seen: usize,
    pub files_changed: usize,
    pub files_removed: usize,
    pub sections: usize,
    pub milestones: usize,
    pub summaries: usize,
    pub truncated: bool,
    pub ms: u128,
}

pub(crate) struct Entry {
    pub(crate) rel: String,
    pub(crate) abs: PathBuf,
    pub(crate) size: u64,
    pub(crate) mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DirNote {
    NameOnly(String),
    Skip(String),
    NestedRepo(String),
}

pub(crate) fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub(crate) fn rel_of(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

fn lang_of(rel: &str) -> Option<&'static str> {
    let ext = rel.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "py" => "python",
        "rs" => "rust",
        "java" => "java",
        "m" | "mm" | "h" => "objc",
        "c" => "c",
        "cpp" | "cc" | "hpp" => "cpp",
        "js" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "go" => "go",
        "md" | "markdown" => "markdown",
        "sh" | "bash" | "zsh" => "shell",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "scala" => "scala",
        "dart" => "dart",
        "lua" => "lua",
        _ => return None,
    })
}

pub fn overrides_for(name: &str) -> Overrides {
    Overrides::load(&db::home().join("projects").join(format!("{name}.toml")))
}

/// Index (or refresh) one project.
pub fn index_project(c: &mut Connection, p: &Project) -> Result<Stats> {
    let t0 = std::time::Instant::now();
    let mut st = Stats::default();
    if p.kind == "huge" {
        index_huge(c, p, &mut st)?;
    } else {
        index_tree(c, p, &mut st)?;
    }
    st.ms = t0.elapsed().as_millis();
    c.execute(
        "UPDATE project SET indexed_at = ?1, index_ms = ?2, truncated = ?3 WHERE id = ?4",
        params![db::now(), st.ms as i64, st.truncated as i64, p.id],
    )?;
    if st.files_changed + st.files_removed + st.summaries > 0 {
        db::bump_project(c, p.id)?;
        db::optimize(c, false);
        trace("inventory optimize", t0);
    }
    Ok(st)
}

fn trace(what: &str, t0: std::time::Instant) {
    if std::env::var_os("FOXDB_TRACE").is_some() {
        eprintln!("{what} {:?}", t0.elapsed());
    }
}

/// Home directory or `/`: top-level names only, never descend, never open.
fn index_huge(c: &mut Connection, p: &Project, st: &mut Stats) -> Result<()> {
    let tx = c.transaction()?;
    let sink = Sink::open(&tx);
    for t in [TBL_NODE, TBL_TRI, TBL_BODY] {
        sink.del_where(&tx, p.id, t, "SELECT id FROM node WHERE project_id = ?1", &[&p.id])?;
    }
    for t in crate::db::FTS_TABLES {
        tx.execute(&format!("DELETE FROM {t} WHERE rowid IN (SELECT id FROM node WHERE project_id = ?1)"), [p.id])?;
    }
    tx.execute("DELETE FROM node WHERE project_id = ?1", [p.id])?;
    if let Ok(rd) = std::fs::read_dir(&p.root) {
        for e in rd.flatten().take(5000) {
            let name = e.file_name().to_string_lossy().into_owned();
            if tiers::is_secret_name(&name) {
                continue;
            }
            let Ok(md) = e.path().symlink_metadata() else { continue };
            let kind = if md.is_dir() { "dir" } else { "file" };
            let id = insert_node(
                &tx,
                p.id,
                kind,
                &format!("{}:{name}", p.id),
                &name,
                Some(&name),
                None,
                None,
                1,
                Some(md.len()),
                mtime_ms(&md),
                "inventory",
                None,
            )?;
            tx.execute("INSERT INTO fts_trigram(rowid, path, name) VALUES (?1, ?2, ?3)", params![id, name, name])?;
            sink.put(&tx, p.id, TBL_TRI, id, &[&name, &name])?;
            st.files_seen += 1;
        }
    }
    tx.commit()?;
    Ok(())
}

/// What a walk of a project tree found: the files it would index, notes on directories it did
/// not enter, and every directory it entered (`""` is the root), in walk order.
pub(crate) struct Walk {
    pub(crate) entries: Vec<Entry>,
    pub(crate) notes: Vec<DirNote>,
    pub(crate) dirs: Vec<String>,
}

/// Directory paths a restricted walk may enter (see `walkcache`); None walks everything.
pub(crate) type Keep = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Walk `root` as an index run does: `.gitignore`, `.ignore`, git excludes and the tier rules;
/// secrets, skip, name-only and nested-repository directories left out (the last three noted).
pub(crate) fn walk(root: &Path, ov: &Arc<Overrides>, keep: Option<Keep>) -> Walk {
    let notes: Arc<Mutex<Vec<DirNote>>> = Arc::default();
    let mut wb = WalkBuilder::new(root);
    wb.hidden(false).git_ignore(true).git_global(true).git_exclude(true).ignore(true).parents(false).require_git(false).follow_links(false);
    {
        let (root, ov, notes) = (root.to_path_buf(), ov.clone(), notes.clone());
        wb.filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            let rel = rel_of(&root, e.path());
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if !is_dir {
                // Secrets are dropped here: never stat'ed for content, never recorded.
                return !tiers::is_secret_name(&e.file_name().to_string_lossy());
            }
            if keep.as_ref().is_some_and(|k| !k(&rel)) {
                return false;
            }
            match tiers::classify_dir_at(&root, &rel, &ov) {
                Tier::Skip => {
                    notes.lock().unwrap().push(DirNote::Skip(rel));
                    false
                }
                Tier::NameOnly => {
                    notes.lock().unwrap().push(DirNote::NameOnly(rel));
                    false
                }
                _ => {
                    if e.path().join(".git").symlink_metadata().is_ok() {
                        notes.lock().unwrap().push(DirNote::NestedRepo(rel));
                        false
                    } else {
                        true
                    }
                }
            }
        });
    }
    let mut entries = Vec::new();
    let mut dirs = vec![String::new()];
    for r in wb.build() {
        let Ok(e) = r else { continue };
        if e.depth() > 0 && e.file_type().is_some_and(|t| t.is_dir()) {
            dirs.push(rel_of(root, e.path()));
            continue;
        }
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        entries.push(Entry { rel: rel_of(root, e.path()), abs: e.path().to_path_buf(), size: md.len(), mtime: mtime_ms(&md) });
    }
    let notes = notes.lock().unwrap().clone();
    Walk { entries, notes, dirs }
}

fn index_tree(c: &mut Connection, p: &Project, st: &mut Stats) -> Result<()> {
    let root = p.root.clone();
    let ov = Arc::new(overrides_for(&p.name));
    // Big trees reuse their last walk and re-read only directories that changed since.
    let t0 = std::time::Instant::now();
    let scan = crate::walkcache::scan(p, &root, &ov);
    trace("inventory walk", t0);
    let recorded = scan.recorded();
    let filegen = if recorded { crate::walkcache::filegen(c, p.id) } else { 0 };
    let fill = outbox::fill_pending(c, p.id);
    // The record's file list is the store's when no run changed the files since it was written:
    // then only the files whose size or time moved are looked up, not every row.
    let delta = scan.prev.as_ref().filter(|pv| pv.0 == filegen && !fill).map(|pv| &pv.1);
    let entries = &scan.walk.entries;

    let mut existing: HashMap<String, (i64, i64, i64)> = HashMap::new();
    if delta.is_none() {
        let mut q = c.prepare("SELECT id, path, size, mtime FROM node WHERE project_id = ?1 AND kind = 'file'")?;
        let rows = q.query_map([p.id], |r| {
            Ok((r.get::<_, String>(1)?, (r.get(0)?, r.get::<_, Option<i64>>(2)?.unwrap_or(-1), r.get::<_, Option<i64>>(3)?.unwrap_or(-1))))
        })?;
        for r in rows.flatten() {
            existing.insert(r.0, r.1);
        }
    }
    trace("inventory known files", t0);

    // Summary nodes for skipped / name-only / nested-repo directories.
    let mut notes = scan.walk.notes.clone();
    // Top-level dirs hidden by .gitignore (e.g. .DerivedData*) still get one summary node each.
    let noted: HashSet<String> = notes
        .iter()
        .map(|n| match n {
            DirNote::NameOnly(r) | DirNote::Skip(r) | DirNote::NestedRepo(r) => r.clone(),
        })
        .collect();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let rel = e.file_name().to_string_lossy().into_owned();
            if noted.contains(&rel) || !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            match tiers::classify_dir_at(&root, &rel, &ov) {
                Tier::Skip => notes.push(DirNote::Skip(rel)),
                Tier::NameOnly => notes.push(DirNote::NameOnly(rel)),
                _ => {}
            }
        }
    }
    let summaries: Vec<(String, i64, serde_json::Value)> = notes
        .iter()
        .map(|n| match n {
            DirNote::NameOnly(rel) => (rel.clone(), 1, summarize_dir(&root.join(rel))),
            DirNote::Skip(rel) => (rel.clone(), 0, serde_json::json!({"policy": "skip"})),
            DirNote::NestedRepo(rel) => (rel.clone(), 0, serde_json::json!({"policy": "nested_repo"})),
        })
        .collect();

    let tx = c.transaction()?;
    let mut sink = Sink::open(&tx);
    // A derived search index's fill starts here: a reset, then every row written again (files as
    // if changed; the tree-sitter pass of this run writes the symbols and finishes the fill).
    if fill {
        tx.execute("UPDATE search_index SET state = 'filling' WHERE project_id = ?1", [p.id])?;
        sink = Sink::open(&tx);
        sink.reset(&tx, p.id)?;
    }
    let mut changed_paths: Vec<String> = vec![];
    let mut seen: HashSet<&str> = HashSet::with_capacity(entries.len());
    for e in entries {
        st.files_seen += 1;
        seen.insert(e.rel.as_str());
        let same = match delta {
            Some(d) => d.get(&e.rel) == Some(&(e.size, e.mtime)),
            None => existing.get(&e.rel).is_some_and(|&(_, size, mtime)| size == e.size as i64 && mtime == e.mtime),
        };
        if same && !fill {
            continue;
        }
        st.files_changed += 1;
        if delta.is_some() {
            changed_paths.push(e.rel.clone());
        }
        remove_file(&tx, &sink, p.id, &e.rel)?;
        let tier = tiers::classify_file(&e.rel, e.size, &ov);
        if tier == Tier::Skip {
            continue;
        }
        index_file(&tx, &sink, p.id, e, tier, st)?;
    }
    let gone: Vec<String> = match delta {
        Some(d) => d.keys().filter(|k| !seen.contains(k.as_str())).cloned().collect(),
        None => existing.keys().filter(|k| !seen.contains(k.as_str())).cloned().collect(),
    };
    for rel in &gone {
        remove_file(&tx, &sink, p.id, rel)?;
        st.files_removed += 1;
    }
    if delta.is_some() {
        changed_paths.extend(gone);
    }

    // A tree with a record rewrites its summary rows only when they changed (or files did).
    let unchanged_summaries = recorded
        && st.files_changed + st.files_removed == 0
        && !fill
        && {
            let mut have: Vec<(String, i64, String)> = {
                let mut q = tx.prepare("SELECT COALESCE(path, ''), COALESCE(tier, 0), COALESCE(meta, '') FROM node WHERE project_id = ?1 AND kind = 'artifact_dir'")?;
                q.query_map([p.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.flatten().collect()
            };
            let mut want: Vec<(String, i64, String)> = summaries.iter().map(|(r, t, m)| (r.clone(), *t, m.to_string())).collect();
            have.sort();
            want.sort();
            have == want
        };
    if !unchanged_summaries {
        for t in [TBL_TRI, TBL_NODE] {
            sink.del_where(&tx, p.id, t, "SELECT id FROM node WHERE project_id = ?1 AND kind = 'artifact_dir'", &[&p.id])?;
        }
        tx.execute("DELETE FROM fts_trigram WHERE rowid IN (SELECT id FROM node WHERE project_id = ?1 AND kind = 'artifact_dir')", [p.id])?;
        tx.execute("DELETE FROM fts_node WHERE rowid IN (SELECT id FROM node WHERE project_id = ?1 AND kind = 'artifact_dir')", [p.id])?;
        tx.execute("DELETE FROM node WHERE project_id = ?1 AND kind = 'artifact_dir'", [p.id])?;
        for (rel, tier, meta) in &summaries {
            let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
            let id = insert_node(
                &tx,
                p.id,
                "artifact_dir",
                &format!("{}:{rel}/", p.id),
                &name,
                Some(rel),
                None,
                None,
                *tier,
                meta.get("bytes").and_then(|b| b.as_u64()),
                0,
                "inventory",
                Some(&meta.to_string()),
            )?;
            tx.execute("INSERT INTO fts_trigram(rowid, path, name) VALUES (?1, ?2, ?3)", params![id, rel, name])?;
            tx.execute("INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1, ?2, '', ?3, '')", params![id, name, rel])?;
            sink.put(&tx, p.id, TBL_TRI, id, &[rel, &name])?;
            sink.put(&tx, p.id, TBL_NODE, id, &[&name, "", rel, ""])?;
            st.summaries += 1;
        }
    }
    let files_moved = st.files_changed + st.files_removed > 0;
    if recorded && files_moved {
        crate::walkcache::bump_filegen(&tx, p.id)?;
    } else if recorded {
        // The tree-sitter pass records the generation it took only once the counter exists.
        tx.execute("INSERT OR IGNORE INTO meta(k, v) VALUES ('files:' || ?1, 0)", [p.id])?;
    }
    if delta.is_some() {
        st.delta = Some(Delta { filegen, paths: changed_paths });
    }
    tx.commit()?;
    trace("inventory store", t0);
    let now = if recorded { crate::walkcache::filegen(c, p.id) } else { 0 };
    crate::walkcache::save(&scan, &root, now, files_moved);
    Ok(())
}

/// Name-only summary: counts and sizes from lstat only. Files are never opened.
fn summarize_dir(abs: &Path) -> serde_json::Value {
    let (mut files, mut dirs, mut bytes, mut n) = (0u64, 0u64, 0u64, 0usize);
    let mut by_ext: HashMap<String, u64> = HashMap::new();
    let mut newest: Vec<(i64, String)> = Vec::new();
    let mut stack = vec![abs.to_path_buf()];
    let mut truncated = false;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            n += 1;
            if n > SUMMARY_MAX_ENTRIES {
                truncated = true;
                break;
            }
            let Ok(md) = e.path().symlink_metadata() else { continue };
            if md.is_dir() {
                dirs += 1;
                stack.push(e.path());
                if d == abs {
                    newest.push((mtime_ms(&md), e.file_name().to_string_lossy().into_owned()));
                }
            } else if md.is_file() {
                files += 1;
                bytes += md.len();
                let name = e.file_name().to_string_lossy().into_owned();
                let ext = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase()).unwrap_or_else(|| "-".into());
                *by_ext.entry(ext).or_default() += 1;
                if d == abs {
                    newest.push((mtime_ms(&md), name));
                }
            }
        }
        if truncated {
            break;
        }
    }
    let mut ext: Vec<_> = by_ext.into_iter().collect();
    ext.sort_by(|a, b| b.1.cmp(&a.1));
    ext.truncate(8);
    newest.sort_by(|a, b| b.0.cmp(&a.0));
    newest.truncate(5);
    serde_json::json!({
        "policy": "name_only", "files": files, "dirs": dirs, "bytes": bytes, "truncated": truncated,
        "by_ext": ext.into_iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>(),
        "newest": newest.into_iter().map(|x| x.1).collect::<Vec<_>>(),
    })
}

/// Drops the inventory nodes for a path. Code-graph symbols (owned by the importers) are kept.
fn remove_file(tx: &Transaction, sink: &Sink, pid: i64, rel: &str) -> Result<()> {
    for t in [TBL_NODE, TBL_TRI] {
        sink.del_where(tx, pid, t, "SELECT id FROM node WHERE project_id = ?1 AND path = ?2 AND kind != 'symbol'", &[&pid, &rel])?;
    }
    tx.execute(
        "DELETE FROM fts_node WHERE rowid IN (SELECT id FROM node WHERE project_id = ?1 AND path = ?2 AND kind != 'symbol')",
        params![pid, rel],
    )?;
    tx.execute(
        "DELETE FROM fts_trigram WHERE rowid IN (SELECT id FROM node WHERE project_id = ?1 AND path = ?2 AND kind != 'symbol')",
        params![pid, rel],
    )?;
    tx.execute("DELETE FROM node WHERE project_id = ?1 AND path = ?2 AND kind != 'symbol'", params![pid, rel])?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_node(
    tx: &Transaction,
    pid: i64,
    kind: &str,
    key: &str,
    name: &str,
    path: Option<&str>,
    line: Option<usize>,
    end_line: Option<usize>,
    tier: i64,
    size: Option<u64>,
    mtime: i64,
    source: &str,
    meta: Option<&str>,
) -> Result<i64> {
    tx.execute(
        "INSERT OR REPLACE INTO node(project_id, kind, key, name, path, line, end_line, lang, tier, size, mtime, source, meta, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            pid,
            kind,
            key,
            name,
            path,
            line.map(|x| x as i64),
            end_line.map(|x| x as i64),
            path.and_then(lang_of),
            tier,
            size.map(|s| s as i64),
            mtime,
            source,
            meta,
            db::now()
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

fn cap(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut i = n;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

fn index_file(tx: &Transaction, sink: &Sink, pid: i64, e: &Entry, tier: Tier, st: &mut Stats) -> Result<()> {
    let name = e.rel.rsplit('/').next().unwrap_or(&e.rel).to_string();
    let file_id = insert_node(
        tx,
        pid,
        "file",
        &format!("{pid}:{}", e.rel),
        &name,
        Some(&e.rel),
        None,
        None,
        tier as i64,
        Some(e.size),
        e.mtime,
        "inventory",
        None,
    )?;
    tx.execute("INSERT INTO fts_trigram(rowid, path, name) VALUES (?1, ?2, ?3)", params![file_id, e.rel, name])?;
    sink.put(tx, pid, TBL_TRI, file_id, &[&e.rel, &name])?;
    if tier != Tier::Text {
        tx.execute(
            "INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1, ?2, '', ?3, '')",
            params![file_id, split_ident(&name), e.rel],
        )?;
        sink.put(tx, pid, TBL_NODE, file_id, &[&split_ident(&name), "", &e.rel, ""])?;
        return Ok(());
    }
    let is_md = name.to_ascii_lowercase().ends_with(".md") || name.to_ascii_lowercase().ends_with(".markdown");
    let is_log = name.to_ascii_lowercase().ends_with(".log");
    let limit = if is_md {
        tiers::MARKDOWN_MAX_BYTES as usize
    } else if is_log {
        tiers::LOG_EDGE_BYTES * 2
    } else {
        tiers::TEXT_MAX_BYTES as usize
    };
    let Some(text) = guard::read_text(&e.abs, &e.rel, limit, is_log) else { return Ok(()) };
    if !is_md {
        tx.execute(
            "INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1, ?2, '', ?3, ?4)",
            params![file_id, split_ident(&name), e.rel, cap(&text, FTS_BODY_MAX)],
        )?;
        sink.put(tx, pid, TBL_NODE, file_id, &[&split_ident(&name), "", &e.rel, cap(&text, FTS_BODY_MAX)])?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1, ?2, '', ?3, '')",
        params![file_id, split_ident(&name), e.rel],
    )?;
    sink.put(tx, pid, TBL_NODE, file_id, &[&split_ident(&name), "", &e.rel, ""])?;
    let mlog = markdown::is_milestone_log(&name);
    for s in markdown::split(&text, if mlog { Some(2) } else { None }) {
        if s.line == 0 {
            continue; // preamble belongs to the file node
        }
        let (kind, meta) = if mlog {
            let m = markdown::parse_milestone(&s);
            st.milestones += 1;
            (
                "milestone",
                serde_json::json!({"id": m.id, "state": m.state, "date": m.date, "shas": m.shas, "pinned": m.pinned, "title": s.title}),
            )
        } else {
            ("doc_section", serde_json::json!({"level": s.level, "title": s.title}))
        };
        st.sections += 1;
        let display = if mlog { meta["id"].as_str().unwrap_or(&s.title).to_string() } else { s.title.clone() };
        let id = insert_node(
            tx,
            pid,
            kind,
            &format!("{pid}:{}#{}", e.rel, s.line),
            &display,
            Some(&e.rel),
            Some(s.line),
            Some(s.end_line),
            2,
            None,
            e.mtime,
            "doc",
            Some(&meta.to_string()),
        )?;
        tx.execute(
            "INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, s.title, meta["state"].as_str().unwrap_or(""), e.rel, cap(&s.body, FTS_BODY_MAX)],
        )?;
        sink.put(tx, pid, TBL_NODE, id, &[&s.title, meta["state"].as_str().unwrap_or(""), &e.rel, cap(&s.body, FTS_BODY_MAX)])?;
    }
    Ok(())
}

/// "HomeShelfStore.swift" -> "HomeShelfStore.swift Home Shelf Store swift" so FTS matches parts.
pub fn split_ident(s: &str) -> String {
    let mut out = String::from(s);
    let mut word = String::new();
    let mut prev_lower = false;
    out.push(' ');
    for ch in s.chars() {
        if !ch.is_alphanumeric() {
            if !word.is_empty() {
                out.push_str(&word);
                out.push(' ');
                word.clear();
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !word.is_empty() {
            out.push_str(&word);
            out.push(' ');
            word.clear();
        }
        prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
        word.push(ch);
    }
    out.push_str(&word);
    // snake_case words also as one squashed term, as unicode61 keeps camelCase whole.
    for w in s.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let parts: Vec<&str> = w.split('_').filter(|x| !x.is_empty()).collect();
        if parts.len() > 1 {
            out.push(' ');
            out.push_str(&parts.concat());
        }
    }
    out
}

/// Claude Code's per-project directories: `~/.claude/projects` (or `FOXDB_CLAUDE_PROJECTS`).
pub fn claude_projects_root() -> PathBuf {
    std::env::var_os("FOXDB_CLAUDE_PROJECTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude/projects"))
}

/// Global memory: ~/.claude/projects/*/memory/*.md (override root with FOXDB_CLAUDE_PROJECTS).
pub fn index_global(c: &mut Connection) -> Result<usize> {
    let g = crate::project::global(c)?;
    let root = claude_projects_root();
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for proj in rd.flatten() {
            let mem = proj.path().join("memory");
            let Ok(rd2) = std::fs::read_dir(&mem) else { continue };
            for f in rd2.flatten() {
                let n = f.file_name().to_string_lossy().into_owned();
                if n.ends_with(".md")
                    && !tiers::is_secret_name(&n)
                    && let Ok(md) = f.path().symlink_metadata()
                    && md.is_file()
                {
                    files.push((proj.file_name().to_string_lossy().into_owned(), n, f.path(), md.len(), mtime_ms(&md)));
                }
            }
        }
    }
    let mut existing: HashMap<String, (i64, i64)> = HashMap::new();
    {
        let mut q = c.prepare("SELECT key, size, mtime FROM node WHERE project_id = ?1 AND kind = 'memory'")?;
        for r in q.query_map([g.id], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))))?.flatten() {
            existing.insert(r.0, r.1);
        }
    }
    let tx = c.transaction()?;
    let sink = Sink::open(&tx);
    let mut seen = HashSet::new();
    for (proj, name, abs, size, mtime) in &files {
        let rel = format!("{proj}/memory/{name}");
        let key = format!("g:{rel}");
        seen.insert(key.clone());
        if existing.get(&key) == Some(&(*size as i64, *mtime)) {
            continue;
        }
        remove_file(&tx, &sink, g.id, &rel)?;
        let Some(text) = guard::read_text(abs, &rel, 256 * 1024, false) else { continue };
        let title = text
            .lines()
            .find_map(|l| l.strip_prefix("name:").map(|x| x.trim().to_string()))
            .unwrap_or_else(|| name.trim_end_matches(".md").to_string());
        let id = insert_node(
            &tx,
            g.id,
            "memory",
            &key,
            &title,
            Some(&rel),
            None,
            None,
            2,
            Some(*size),
            *mtime,
            "import",
            Some(&serde_json::json!({"claude_project": proj}).to_string()),
        )?;
        tx.execute(
            "INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, split_ident(&title), proj, rel, cap(&text, FTS_BODY_MAX)],
        )?;
        tx.execute("INSERT INTO fts_trigram(rowid, path, name) VALUES (?1, ?2, ?3)", params![id, rel, title])?;
        sink.put(&tx, g.id, TBL_NODE, id, &[&split_ident(&title), proj, &rel, cap(&text, FTS_BODY_MAX)])?;
        sink.put(&tx, g.id, TBL_TRI, id, &[&rel, &title])?;
    }
    for key in existing.keys() {
        if !seen.contains(key) {
            let rel = key.trim_start_matches("g:").to_string();
            remove_file(&tx, &sink, g.id, &rel)?;
        }
    }
    tx.commit()?;
    c.execute("UPDATE project SET indexed_at = ?1 WHERE id = ?2", params![db::now(), g.id])?;
    db::bump_generation(c)?;
    Ok(files.len())
}
