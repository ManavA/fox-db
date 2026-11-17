//! Wrapper around the pinned codanna (`~/.foxdb/bin/codanna`, tree-sitter): the code graph for
//! Kotlin and Python, and the `~`-labelled fallback for Swift files the index store does not cover
//! or that were edited since their unit was built.
//!
//! codanna writes `.codanna/` and `.codannaignore` into its cwd, so it only ever runs inside a
//! mirror at `~/.foxdb/mirrors/<project>-src`, never inside a project. The mirror holds code files
//! copied from the project's tier-3 inventory (secrets and artifact dirs are never in it).

use crate::code::Sym;
use crate::db;
use crate::project::Project;
use anyhow::{Result, bail};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Languages codanna parses for us (Swift only where the index store does not cover a file).
/// Go/TypeScript/JavaScript/Rust were added for Linux, where there is no index store at all.
pub const TS_LANGS: &[&str] = &["kotlin", "python", "go", "typescript", "javascript", "rust"];

fn sql_list(langs: &[&str]) -> String {
    langs.iter().map(|l| format!("'{l}'")).collect::<Vec<_>>().join(",")
}

pub fn bin() -> PathBuf {
    db::home().join("bin/codanna")
}
pub fn available() -> bool {
    bin().exists()
}
pub fn mirror_dir(p: &Project) -> PathBuf {
    db::home().join("mirrors").join(format!("{}-src", p.name))
}
pub fn mirror_ready(p: &Project) -> bool {
    mirror_dir(p).join(".codanna/index").exists()
}

#[derive(Debug, Default)]
pub struct SyncStats {
    pub copied: usize,
    pub removed: usize,
    pub files: usize,
    pub ms: u128,
}

/// Does this project need codanna at all (Kotlin/Python files, or Swift files outside the index store)?
pub fn needed(c: &Connection, p: &Project) -> bool {
    let q = |sql: &str| c.query_row(sql, [p.id], |r| r.get::<_, i64>(0)).unwrap_or(0);
    if q(&format!("SELECT COUNT(*) FROM node WHERE project_id = ?1 AND kind = 'file' AND tier = 3 AND lang IN ({})", sql_list(TS_LANGS)))
        > 0
    {
        return true;
    }
    q("SELECT COUNT(*) FROM node f WHERE f.project_id = ?1 AND f.kind = 'file' AND f.tier = 3 AND f.lang = 'swift'
         AND NOT EXISTS (SELECT 1 FROM swift_rec r WHERE r.path = f.path)")
        > 0
}

/// Refresh the mirror from the inventory, then let codanna (incrementally) index it.
pub fn sync(c: &Connection, p: &Project) -> Result<SyncStats> {
    let t0 = std::time::Instant::now();
    if !available() {
        bail!("codanna not installed at {}", bin().display());
    }
    let mir = mirror_dir(p);
    std::fs::create_dir_all(&mir)?;
    let manifest_p = mir.join(".foxdb-manifest.json");
    let mut old: HashMap<String, (i64, i64)> =
        std::fs::read_to_string(&manifest_p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut st = SyncStats::default();
    let mut changed: Vec<String> = vec![];
    let mut now: HashMap<String, (i64, i64)> = HashMap::new();
    {
        let mut langs = TS_LANGS.to_vec();
        langs.push("swift");
        let mut q = c.prepare(&format!(
            "SELECT path, COALESCE(size,0), COALESCE(mtime,0) FROM node WHERE project_id = ?1 AND kind = 'file' AND tier = 3 AND lang IN ({})",
            sql_list(&langs)
        ))?;
        let rows = q.query_map([p.id], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))))?;
        for (path, sm) in rows.flatten() {
            if crate::tiers::is_denied_path(&path) || sm.0 > 2 << 20 {
                continue;
            }
            now.insert(path, sm);
        }
    }
    for (path, sm) in &now {
        if old.get(path) == Some(sm) && mir.join(path).exists() {
            continue;
        }
        let dst = mir.join(path);
        if let Some(d) = dst.parent() {
            std::fs::create_dir_all(d)?;
        }
        if std::fs::copy(p.root.join(path), &dst).is_ok() {
            st.copied += 1;
            changed.push(path.clone());
        }
    }
    for path in old.keys() {
        if !now.contains_key(path) {
            let _ = std::fs::remove_file(mir.join(path));
            st.removed += 1;
        }
    }
    old = now;
    st.files = old.len();
    if st.copied > 0 || st.removed > 0 || !mirror_ready(p) {
        if !mir.join(".codanna").exists() {
            run(&mir, &["init"])?;
            disable_semantic(&mir.join(".codanna/settings.toml"))?;
        }
        // Incremental: hand codanna only the files that changed. Cold build, or removals, re-index everything.
        let mut args: Vec<&str> = vec!["index", "--no-progress"];
        if !mirror_ready(p) || st.removed > 0 || changed.len() > 300 {
            args.push(".");
            if st.removed > 0 {
                args.push("--force");
            }
        } else {
            args.extend(changed.iter().map(String::as_str));
        }
        run(&mir, &args)?;
    }
    // Written last: an interrupted codanna run is retried on the next sync.
    std::fs::write(&manifest_p, serde_json::to_string(&old)?)?;
    st.ms = t0.elapsed().as_millis();
    Ok(st)
}

/// codanna's `init` enables semantic search, which downloads an embedding model (~90 MB) and
/// embeds every symbol. foxdb only uses the symbol/relationship index, so switch it off.
pub fn disable_semantic(settings: &std::path::Path) -> Result<()> {
    let t = std::fs::read_to_string(settings)?;
    let mut out = String::with_capacity(t.len());
    let mut in_sem = false;
    for line in t.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_sem = l == "[semantic_search]";
        }
        if in_sem && l.replace(' ', "") == "enabled=true" {
            out.push_str("enabled = false\n");
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    std::fs::write(settings, out)?;
    Ok(())
}

fn run(dir: &std::path::Path, args: &[&str]) -> Result<std::process::Output> {
    let o = Command::new(bin()).args(args).current_dir(dir).stdin(Stdio::null()).output()?;
    if !o.status.success() && args[0] != "retrieve" {
        bail!("codanna {} failed: {}", args[0], String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or(""));
    }
    Ok(o)
}

/// One codanna symbol.
#[derive(Debug, Clone)]
pub struct CSym {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: i64,
    pub end_line: i64,
    pub class: Option<String>,
    pub sig: String,
}
impl CSym {
    fn from(v: &Value) -> Option<CSym> {
        let s = v.get("symbol").unwrap_or(v);
        Some(CSym {
            id: s["id"].as_i64()?,
            name: s["name"].as_str()?.to_string(),
            kind: s["kind"].as_str().unwrap_or("").to_lowercase(),
            path: s["file_path"].as_str()?.to_string(),
            line: s["range"]["start_line"].as_i64().unwrap_or(0) + 1,
            end_line: s["range"]["end_line"].as_i64().unwrap_or(0) + 1,
            class: s["scope_context"]["ClassMember"]["class_name"].as_str().map(str::to_string),
            sig: s["signature"].as_str().unwrap_or("").lines().next().unwrap_or("").trim().to_string(),
        })
    }
    pub fn qname(&self) -> String {
        match &self.class {
            Some(c) => format!("{c}.{}", self.name),
            None => self.name.clone(),
        }
    }
    pub fn handle(&self) -> String {
        format!("c:{:x}", self.id)
    }
    pub fn row(&self) -> String {
        format!("~{} {} {}  {}:{}", self.handle(), self.kind, self.qname(), self.path, self.line)
    }
}

/// `codanna retrieve <verb> <target> --json` in the mirror. Ambiguous names are narrowed by `near`
/// (path, line) when given. Returns the `data` array.
fn retrieve(p: &Project, verb: &str, target: &str, near: Option<(&str, i64)>) -> Result<Vec<Value>> {
    let mir = mirror_dir(p);
    let o = run(&mir, &["retrieve", verb, target, "--json"])?;
    let v: Value = serde_json::from_slice(&o.stdout)?;
    if v["code"] == "INVALID_QUERY"
        && let Some(ctx) = v["error"]["context"].as_array()
    {
        let pick = ctx.iter().min_by_key(|e| match near {
            Some((path, line)) if e["file_path"] == path => (e["line"].as_i64().unwrap_or(0) - line).abs(),
            _ => i64::MAX,
        });
        if let Some(e) = pick
            && near.is_none_or(|(path, _)| e["file_path"] == path)
            && let Some(id) = e["symbol_id"].as_i64()
        {
            let o = run(&mir, &["retrieve", verb, &format!("symbol_id:{id}"), "--json"])?;
            let v: Value = serde_json::from_slice(&o.stdout)?;
            return Ok(v["data"].as_array().cloned().unwrap_or_default());
        }
        return Ok(vec![]);
    }
    Ok(v["data"].as_array().cloned().unwrap_or_default())
}

/// Merge tree-sitter callers into `lines` for files the index store cannot vouch for.
pub fn merge_callers(
    c: &Connection,
    p: &Project,
    top: &Sym,
    stale: &HashSet<String>,
    known: &HashSet<(String, String)>,
    lines: &mut Vec<String>,
) {
    if !mirror_ready(p) {
        return;
    }
    let near = top.path.as_deref().map(|x| (x, top.line));
    let Ok(rows) = retrieve(p, "callers", &top.name, near) else { return };
    let mut add: Vec<String> = vec![];
    for r in rows.iter().filter_map(CSym::from) {
        let uncovered = c.query_row("SELECT 1 FROM swift_rec WHERE path = ?1", [&r.path], |_| Ok(())).is_err();
        if !(uncovered || stale.contains(&r.path)) || known.contains(&(r.path.clone(), r.name.clone())) {
            continue;
        }
        add.push(format!("~ {}:{} {}  [tree-sitter]", r.path, r.line, r.qname()));
    }
    if !add.is_empty() {
        lines.push(format!("~ tree-sitter callers in stale/uncovered files ({}; caller definition lines, not call sites)", add.len()));
        lines.extend(add);
    }
}

/// `fox code` for projects without index-store symbols (Kotlin, Python, unbuilt Swift).
pub fn run_code(p: &Project, sym: &str, rel: &str, depth: usize) -> Result<Vec<String>> {
    if !mirror_ready(p) {
        bail!("no codanna index for {} yet; run `fox index` in the project", p.name);
    }
    let mut lines = vec![];
    let (name, _) = sym.split_once('(').unwrap_or((sym, ""));
    let name = name.rsplit('.').next().unwrap_or(name);
    let resolved: Vec<CSym> = if let Some(h) = sym.strip_prefix("c:") {
        let id = i64::from_str_radix(h, 16)?;
        retrieve(p, "symbol", &format!("symbol_id:{id}"), None)?.iter().filter_map(CSym::from).collect()
    } else {
        retrieve_symbols(p, name)?
    };
    let want_class = sym.rsplit_once('.').map(|(a, _)| a.rsplit('.').next().unwrap_or(a).to_string());
    let mut cands: Vec<&CSym> =
        resolved.iter().filter(|s| want_class.as_ref().is_none_or(|w| s.class.as_deref() == Some(w.as_str()) || s.name == *w)).collect();
    cands.sort_by_key(|s| {
        (!matches!(s.kind.as_str(), "method" | "function" | "class" | "interface" | "struct"), s.path.contains("/test/") as u8)
    });
    let Some(top) = cands.first().copied() else {
        return Ok(vec![format!("no symbol matches {sym:?} in the {} tree-sitter index", p.name)]);
    };
    if cands.len() > 1 {
        let others: Vec<String> = cands.iter().skip(1).take(5).map(|s| format!("{} {}", s.handle(), s.qname())).collect();
        lines.push(format!(
            "~ {} matches for {sym:?}; using {} {} (others: {})",
            cands.len(),
            top.handle(),
            top.qname(),
            others.join(", ")
        ));
    }
    let id_target = format!("symbol_id:{}", top.id);
    let list =
        |verb: &str, target: &str| -> Result<Vec<CSym>> { Ok(retrieve(p, verb, target, None)?.iter().filter_map(CSym::from).collect()) };
    match rel {
        "callers" | "refs" => {
            let v = list("callers", &id_target)?;
            lines.push(format!("~ callers of {}  (tree-sitter: caller definitions, name-resolved)", top.row().trim_start_matches('~')));
            lines.push(format!("{} callers", v.len()));
            lines.extend(v.iter().map(CSym::row));
        }
        "callees" => {
            let v = list("calls", &id_target)?;
            lines.push(format!("~ callees of {}", top.row().trim_start_matches('~')));
            lines.push(format!("{} callees", v.len()));
            lines.extend(v.iter().map(CSym::row));
        }
        "conforms" | "subtypes" | "extensions" | "overrides" => {
            let v = list("implementations", &id_target)?;
            lines.push(format!("~ implementations of {}", top.row().trim_start_matches('~')));
            lines.push(format!("{} found", v.len()));
            lines.extend(v.iter().map(CSym::row));
        }
        "src" => {
            let abs = mirror_dir(p).join(&top.path);
            let text = std::fs::read_to_string(&abs).unwrap_or_default();
            lines.push(format!("~ {} {}:{}-{}", top.handle(), top.path, top.line, top.end_line));
            for (i, l) in text.lines().enumerate().take(top.end_line as usize).skip(top.line as usize - 1) {
                lines.push(format!("{}\t{}", i + 1, crate::query::clip(l, 220)));
            }
        }
        "outline" => {
            lines.push(format!("~ outline {}", top.row().trim_start_matches('~')));
            let abs = mirror_dir(p).join(&top.path);
            lines.extend(outline_text(&abs, &top.path));
        }
        "impact" => {
            let mut seen: HashSet<i64> = HashSet::from([top.id]);
            let mut frontier = vec![top.id];
            let mut total = 0;
            for d in 0..depth.clamp(1, 3) {
                let mut next = vec![];
                for id in frontier.iter().take(40) {
                    for s in list("callers", &format!("symbol_id:{id}"))? {
                        if seen.insert(s.id) {
                            next.push(s.id);
                            if lines.len() < 400 {
                                lines.push(format!("  d{} {}", d + 1, s.row()));
                            }
                            total += 1;
                        }
                    }
                }
                frontier = next;
            }
            lines.insert(0, format!("~ impact of {}: {total} transitive callers (name-resolved)", top.qname()));
        }
        _ => bail!("unsupported --rel {rel}"),
    }
    Ok(lines)
}

fn retrieve_symbols(p: &Project, name: &str) -> Result<Vec<CSym>> {
    let mir = mirror_dir(p);
    let o = run(&mir, &["retrieve", "symbol", name, "--json"])?;
    let v: Value = serde_json::from_slice(&o.stdout)?;
    Ok(v["data"].as_array().map(|a| a.iter().filter_map(CSym::from).collect()).unwrap_or_default())
}

/// Structural outline of a mirrored source file (declaration lines), for the tree-sitter fallback.
pub fn outline_text(abs: &std::path::Path, rel: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(abs) else { return vec![] };
    let re = regex::Regex::new(r"^\s*(?:(?:public|private|internal|protected|open|final|override|abstract|sealed|data|enum|suspend|inline|static|fileprivate|mutating|async)\s+)*(class|struct|enum|protocol|extension|interface|object|fun|func|def|init|var|val|let|typealias)\b\s*([A-Za-z_][A-Za-z0-9_.<>]*)?").unwrap();
    let mut out = vec![];
    for (i, l) in text.lines().enumerate() {
        if let Some(m) = re.captures(l) {
            let kind = &m[1];
            let top_level = !l.starts_with(' ') && !l.starts_with('\t');
            if matches!(kind, "var" | "val" | "let") && !top_level && l.len() - l.trim_start().len() > 4 {
                continue;
            }
            out.push(format!(
                "{}{} {} {}",
                if l.starts_with("    ") || l.starts_with('\t') { "  " } else { "" },
                i + 1,
                kind,
                crate::query::clip(l.trim(), 110)
            ));
        }
    }
    let _ = rel;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_semantic_only_touches_semantic_section() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("settings.toml");
        std::fs::write(
            &f,
            "[indexing]\nenabled = true\n\n[semantic_search]\n# x\nenabled = true\nmodel = \"A\"\n\n[languages.go]\nenabled = true\n",
        )
        .unwrap();
        disable_semantic(&f).unwrap();
        let v: toml::Value = toml::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["semantic_search"]["enabled"].as_bool(), Some(false));
        assert_eq!(v["indexing"]["enabled"].as_bool(), Some(true));
        assert_eq!(v["languages"]["go"]["enabled"].as_bool(), Some(true));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn no_index_store_off_macos() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".DerivedDataX/Index.noindex/DataStore/v5")).unwrap();
        if std::env::var_os("FOXDB_INDEXSTORE_LIB").is_none() {
            assert!(crate::swiftimport::discover_stores(d.path()).is_empty());
        }
    }

    #[test]
    fn tree_sitter_langs_cover_linux_targets() {
        for l in ["go", "typescript", "javascript", "rust", "kotlin", "python"] {
            assert!(TS_LANGS.contains(&l), "{l}");
        }
        assert_eq!(sql_list(&["a", "b"]), "'a','b'");
    }
}
