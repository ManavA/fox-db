//! Swift code graph: import Xcode index-store data into the foxdb store.
//!
//! Symbols are keyed by USR (`<project id>:usr:<USR>`). Raw occurrences are kept in `occ`, per
//! content-addressed record, and the CALLS / REFERENCES / CONFORMS_TO / INHERITS / EXTENDS / OVERRIDES
//! edges are derived from them in SQL. Incremental by unit mtime: unchanged units are skipped, and a
//! record that is already stored is never read again.

use crate::indexstore::{self as ix, NONE, OccRow, Store, SymInfo};
use crate::inventory::split_ident;
use crate::project::Project;
use crate::{db, guard, tiers};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct ImportStats {
    pub stores: usize,
    pub units_seen: usize,
    pub units_changed: usize,
    pub records_read: usize,
    pub symbols: i64,
    pub occurrences: i64,
    pub edges: i64,
    pub stale_files: usize,
    pub ms: u128,
}

/// A record (one source file's data) read from a store. `syms` indexes `StoreData::syms`.
pub struct RecData {
    pub name: String,
    pub path: String, // project-relative
    pub occs: Vec<OccRow>,
}
pub struct UnitData {
    pub unit: String,
    pub mtime: i64, // ms
    pub module: String,
    /// All project-relative records of the unit; `new` ones carry their occurrences.
    pub recs: Vec<(String, String)>, // (record name, rel path)
}
pub struct StoreData {
    pub store: String,
    pub syms: Vec<SymInfo>,
    pub units: Vec<UnitData>,
    pub new_recs: Vec<RecData>,
    /// Every unit currently in the store (changed or not), for garbage collection.
    pub present: HashSet<String>,
}

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Index stores to read for a project: `FOXDB_SWIFT_STORES` (colon separated) if set, else every
/// `<root>/.DerivedData*/Index.noindex/DataStore` plus Xcode's own DerivedData folders whose
/// `info.plist` names a workspace under the root. All are opened read-only.
pub fn discover_stores(root: &Path) -> Vec<PathBuf> {
    // The index store is read through Xcode's libIndexStore.dylib. Elsewhere (Linux) Swift falls back
    // to codanna's tree-sitter index, unless a libIndexStore is named explicitly.
    if !cfg!(target_os = "macos") && std::env::var_os("FOXDB_INDEXSTORE_LIB").is_none() {
        return vec![];
    }
    if let Some(v) = std::env::var_os("FOXDB_SWIFT_STORES") {
        return std::env::split_paths(&v).filter(|p| p.join("v5").exists()).collect();
    }
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with(".DerivedData") {
                let ds = e.path().join("Index.noindex/DataStore");
                if ds.join("v5").exists() {
                    out.push(ds);
                }
            }
        }
    }
    out.sort();
    if let Some(home) = std::env::var_os("HOME") {
        let dd = PathBuf::from(home).join("Library/Developer/Xcode/DerivedData");
        let needle = root.to_string_lossy().into_owned();
        if let Ok(rd) = std::fs::read_dir(dd) {
            let mut extra: Vec<PathBuf> = vec![];
            for e in rd.flatten() {
                let ds = e.path().join("Index.noindex/DataStore");
                if !ds.join("v5").exists() {
                    continue;
                }
                let Ok(plist) = std::fs::read(e.path().join("info.plist")) else { continue };
                if plist.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    extra.push(ds);
                }
            }
            extra.sort();
            out.extend(extra);
        }
    }
    out
}

/// Read changed units from one store (the heavy, FFI side). Nothing here writes to the database.
pub fn read_store(dir: &Path, root: &Path, known_units: &HashMap<String, i64>, known_recs: &HashSet<String>) -> Result<StoreData> {
    let mut s = Store::open(dir)?;
    let mut sd =
        StoreData { store: dir.to_string_lossy().into_owned(), syms: vec![], units: vec![], new_recs: vec![], present: HashSet::new() };
    let units_dir = dir.join("v5/units");
    let root_s = root.to_string_lossy().into_owned();
    let mut done_recs: HashSet<String> = HashSet::new();
    for name in s.unit_names() {
        sd.present.insert(name.clone());
        let mt = mtime_ms(&units_dir.join(&name));
        if known_units.get(&name) == Some(&mt) {
            continue;
        }
        let Some(info) = s.unit_info(&name) else { continue };
        let mut recs = vec![];
        for (path, rec) in &info.files {
            let Some(rel) = path.strip_prefix(&root_s).map(|r| r.trim_start_matches('/')) else { continue };
            if rel.is_empty() || tiers::is_denied_path(rel) {
                continue;
            }
            recs.push((rec.clone(), rel.to_string()));
            if known_recs.contains(rec) || !done_recs.insert(rec.clone()) {
                continue;
            }
            if let Some(occs) = s.read_record(rec) {
                sd.new_recs.push(RecData { name: rec.clone(), path: rel.to_string(), occs });
            }
        }
        sd.units.push(UnitData { unit: name, mtime: mt, module: info.module, recs });
    }
    sd.syms = std::mem::take(&mut s.syms);
    Ok(sd)
}

fn is_funcish(k: i32) -> bool {
    matches!(k, 12 | 16 | 17 | 18 | 22 | 23)
}
fn is_accessor(s: &SymInfo) -> bool {
    (1..=4).contains(&s.subkind) && is_funcish(s.kind) || s.name.starts_with("getter:") || s.name.starts_with("setter:")
}

fn base_name(full: &str) -> &str {
    let b = full.split('(').next().unwrap_or(full);
    if b.is_empty() { full } else { b }
}

struct Def {
    usr: String,
    name: String,
    sig: String,
    kind: i32,
    path: String,
    line: u32,
    parent: Option<String>, // parent usr
    module: String,
}

struct Ctx<'a> {
    tx: &'a Transaction<'a>,
    pid: i64,
    ids: HashMap<String, i64>, // usr -> node id (project defs + externals created this run)
}

impl Ctx<'_> {
    fn key(&self, usr: &str) -> String {
        format!("{}:usr:{usr}", self.pid)
    }
    fn external(&mut self, s: &SymInfo) -> Result<i64> {
        if let Some(&i) = self.ids.get(&s.usr) {
            return Ok(i);
        }
        let k = self.key(&s.usr);
        let meta = serde_json::json!({"k": ix::kind_name(s.kind), "external": true}).to_string();
        self.tx.execute(
            "INSERT INTO node(project_id, kind, key, name, sig, lang, tier, source, meta, updated_at) VALUES (?1,'symbol',?2,?3,?4,'swift',3,'indexstore',?5,?6)
             ON CONFLICT(key) DO NOTHING",
            params![self.pid, k, base_name(&s.name), s.name, meta, db::now()],
        )?;
        let id: i64 = self.tx.query_row("SELECT id FROM node WHERE key = ?1", [k], |r| r.get(0))?;
        self.ids.insert(s.usr.clone(), id);
        Ok(id)
    }
}

/// Apply read data to the database (one transaction).
pub fn apply(c: &mut Connection, p: &Project, stores: &[StoreData], reached: &HashSet<String>) -> Result<ImportStats> {
    let mut st = ImportStats { stores: stores.len(), ..Default::default() };
    let tx = c.transaction()?;
    // 1. Units that vanished from stores we could read.
    {
        let rows: Vec<(i64, String)> = {
            let mut q = tx.prepare("SELECT id, store || '\u{1}' || unit FROM swift_unit WHERE project_id = ?1")?;
            q.query_map([p.id], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect()
        };
        let present: HashSet<String> = stores.iter().flat_map(|s| s.present.iter().map(|u| format!("{}\u{1}{u}", s.store))).collect();
        for (id, k) in rows {
            let store = k.split('\u{1}').next().unwrap_or("");
            if reached.contains(store) && !present.contains(&k) {
                tx.execute("DELETE FROM swift_file WHERE unit = ?1", [id])?;
                tx.execute("DELETE FROM swift_unit WHERE id = ?1", [id])?;
            }
        }
    }
    // 2. Units + file links.
    let mut rec_ids: HashMap<String, i64> = HashMap::new();
    {
        let mut q = tx.prepare("SELECT name, id FROM swift_rec")?;
        for r in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.flatten() {
            rec_ids.insert(r.0, r.1);
        }
    }
    for sd in stores {
        st.units_seen += sd.present.len();
        for r in &sd.new_recs {
            tx.execute("INSERT OR IGNORE INTO swift_rec(name, path) VALUES (?1, ?2)", params![r.name, r.path])?;
            let id: i64 = tx.query_row("SELECT id FROM swift_rec WHERE name = ?1", [&r.name], |r| r.get(0))?;
            rec_ids.insert(r.name.clone(), id);
        }
        for u in &sd.units {
            st.units_changed += 1;
            tx.execute(
                "INSERT INTO swift_unit(project_id, store, unit, mtime, module) VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(store, unit) DO UPDATE SET mtime = excluded.mtime, module = excluded.module, project_id = excluded.project_id",
                params![p.id, sd.store, u.unit, u.mtime, u.module],
            )?;
            let uid: i64 =
                tx.query_row("SELECT id FROM swift_unit WHERE store = ?1 AND unit = ?2", params![sd.store, u.unit], |r| r.get(0))?;
            tx.execute("DELETE FROM swift_file WHERE unit = ?1", [uid])?;
            for (rec, _) in &u.recs {
                if let Some(&rid) = rec_ids.get(rec) {
                    tx.execute("INSERT OR IGNORE INTO swift_file(unit, rec) VALUES (?1, ?2)", params![uid, rid])?;
                }
            }
        }
    }
    // 3. Garbage-collect records no unit references.
    let dead: Vec<i64> = {
        let mut q = tx.prepare("SELECT id FROM swift_rec WHERE id NOT IN (SELECT rec FROM swift_file)")?;
        q.query_map([], |r| r.get(0))?.flatten().collect()
    };
    for id in &dead {
        tx.execute("DELETE FROM occ WHERE rec = ?1", [id])?;
        tx.execute("DELETE FROM swift_rec WHERE id = ?1", [id])?;
    }

    // 4. Definitions from new records.
    let mut ctx = Ctx { tx: &tx, pid: p.id, ids: HashMap::new() };
    {
        let mut q = tx.prepare("SELECT key, id FROM node WHERE project_id = ?1 AND source = 'indexstore' AND kind = 'symbol'")?;
        let prefix = format!("{}:usr:", p.id);
        for r in q.query_map([p.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.flatten() {
            if let Some(u) = r.0.strip_prefix(&prefix) {
                ctx.ids.insert(u.to_string(), r.1);
            }
        }
    }
    let known_before: HashSet<String> = ctx.ids.keys().cloned().collect();
    // module per record (first unit that links it)
    let mut rec_module: HashMap<&str, &str> = HashMap::new();
    for sd in stores {
        for u in &sd.units {
            for (r, _) in &u.recs {
                rec_module.entry(r.as_str()).or_insert(u.module.as_str());
            }
        }
    }
    let mut defs: HashMap<String, Def> = HashMap::new();
    // per record: local symbol idx set, accessor->property, child->parent
    struct RecInfo {
        local: HashSet<u32>,
        acc_prop: HashMap<u32, u32>,
        parent: HashMap<u32, u32>,
    }
    let mut infos: HashMap<(usize, usize), RecInfo> = HashMap::new();
    for (si, sd) in stores.iter().enumerate() {
        for (ri, r) in sd.new_recs.iter().enumerate() {
            let mut ri_info = RecInfo { local: HashSet::new(), acc_prop: HashMap::new(), parent: HashMap::new() };
            for o in &r.occs {
                if o.roles & (ix::R_DEF | ix::R_DECL) == 0 || o.roles & ix::R_IMPLICIT != 0 {
                    continue;
                }
                if o.rel_roles & ix::REL_CHILD_OF != 0 && o.rel_sym != NONE {
                    ri_info.parent.insert(o.sym, o.rel_sym);
                }
                if o.rel_roles & ix::REL_ACCESSOR_OF != 0 && o.rel_sym != NONE {
                    ri_info.acc_prop.insert(o.sym, o.rel_sym);
                }
            }
            for o in &r.occs {
                if o.roles & (ix::R_DEF | ix::R_DECL) == 0 || o.roles & ix::R_IMPLICIT != 0 {
                    continue;
                }
                let s = &sd.syms[o.sym as usize];
                let local = s.kind == 25
                    || is_accessor(s)
                    || ri_info.parent.get(&o.sym).is_some_and(|&pa| {
                        let ps = &sd.syms[pa as usize];
                        is_funcish(ps.kind) || is_accessor(ps)
                    });
                if local {
                    ri_info.local.insert(o.sym);
                }
            }
            for o in &r.occs {
                if o.roles & ix::R_DEF == 0 || o.roles & ix::R_IMPLICIT != 0 || ri_info.local.contains(&o.sym) {
                    continue;
                }
                let s = &sd.syms[o.sym as usize];
                if matches!(s.kind, 0 | 1) || s.lang != 100 {
                    continue;
                }
                let parent = ri_info.parent.get(&o.sym).map(|&pa| sd.syms[pa as usize].usr.clone());
                let d = Def {
                    usr: s.usr.clone(),
                    name: base_name(&s.name).to_string(),
                    sig: s.name.clone(),
                    kind: s.kind,
                    path: r.path.clone(),
                    line: o.line,
                    parent,
                    module: rec_module.get(r.name.as_str()).copied().unwrap_or("").to_string(),
                };
                defs.entry(s.usr.clone()).or_insert(d);
            }
            infos.insert((si, ri), ri_info);
        }
    }
    // Upsert definition nodes.
    let usr_name: HashMap<String, (String, i32)> = defs.iter().map(|(u, d)| (u.clone(), (d.name.clone(), d.kind))).collect();
    for d in defs.values() {
        let parent_name = d.parent.as_ref().and_then(|u| usr_name.get(u).map(|x| x.0.clone()));
        let meta = serde_json::json!({"k": ix::kind_name(d.kind), "mod": d.module, "parent": parent_name}).to_string();
        let key = ctx.key(&d.usr);
        let id: i64 = tx.query_row(
            "INSERT INTO node(project_id, kind, key, name, path, line, sig, lang, tier, source, stale, meta, updated_at)
             VALUES (?1,'symbol',?2,?3,?4,?5,?6,'swift',3,'indexstore',0,?7,?8)
             ON CONFLICT(key) DO UPDATE SET name=excluded.name, path=excluded.path, line=excluded.line, sig=excluded.sig,
               meta=excluded.meta, stale=0, updated_at=excluded.updated_at
             RETURNING id",
            params![p.id, key, d.name, d.path, d.line, d.sig, meta, db::now()],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM fts_node WHERE rowid = ?1", [id])?;
        tx.execute("DELETE FROM fts_trigram WHERE rowid = ?1", [id])?;
        tx.execute("DELETE FROM fts_body WHERE rowid = ?1", [id])?;
        let ident = match parent_name.as_deref() {
            Some(pn) => format!("{pn}.{}", d.name),
            None => d.name.clone(),
        };
        tx.execute(
            "INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1,?2,?3,?4,'')",
            params![id, split_ident(&d.name), d.sig, d.path],
        )?;
        // No source text from the index store: the body arm gets the names only.
        tx.execute(
            "INSERT INTO fts_body(rowid, name, sig, path, body) VALUES (?1,?2,?3,?4,'')",
            params![id, split_ident(&d.name), d.sig, d.path],
        )?;
        tx.execute("INSERT INTO fts_trigram(rowid, path, name) VALUES (?1,?2,?3)", params![id, d.path, ident])?;
        ctx.ids.insert(d.usr.clone(), id);
    }
    st.symbols = defs.len() as i64;
    let project_syms: HashSet<String> = known_before.into_iter().chain(defs.keys().cloned()).collect();

    // 5. Occurrence rows.
    {
        let mut ins = tx.prepare_cached("INSERT INTO occ(rec, sym, line, col, roles, rel_roles, rel_sym) VALUES (?1,?2,?3,?4,?5,?6,?7)")?;
        for (si, sd) in stores.iter().enumerate() {
            for (ri, r) in sd.new_recs.iter().enumerate() {
                let rid = rec_ids[&r.name];
                tx.execute("DELETE FROM occ WHERE rec = ?1", [rid])?;
                st.records_read += 1;
                let info = &infos[&(si, ri)];
                // Climb to the nearest enclosing project symbol (skips locals, folds accessors into properties).
                let norm = |mut x: u32| -> Option<u32> {
                    for _ in 0..16 {
                        if let Some(&prop) = info.acc_prop.get(&x) {
                            x = prop;
                            continue;
                        }
                        if !info.local.contains(&x) && project_syms.contains(&sd.syms[x as usize].usr) {
                            return Some(x);
                        }
                        x = *info.parent.get(&x)?;
                    }
                    None
                };
                let mut seen: HashSet<(u32, u32, u32, u32, u64)> = HashSet::new();
                for o in &r.occs {
                    let s = &sd.syms[o.sym as usize];
                    if s.lang != 100 || matches!(s.kind, 0 | 1 | 25) {
                        continue;
                    }
                    let structural = o.rel_roles & (ix::REL_BASE_OF | ix::REL_OVERRIDE_OF | ix::REL_EXTENDED_BY) != 0;
                    let (sym_id, rel_id);
                    if o.rel_roles & ix::REL_BASE_OF != 0 {
                        // sym = base type / protocol (maybe external), rel = conforming type or extension.
                        let Some(rel) = o.rel_sym.ne(&NONE).then(|| norm(o.rel_sym)).flatten() else { continue };
                        sym_id = ctx.external(s)?;
                        rel_id = ctx.ids[&sd.syms[rel as usize].usr];
                    } else if o.rel_roles & ix::REL_OVERRIDE_OF != 0 {
                        // sym = overriding impl (project), rel = overridden requirement (maybe external).
                        if !project_syms.contains(&s.usr) || info.local.contains(&o.sym) || o.rel_sym == NONE {
                            continue;
                        }
                        sym_id = ctx.ids[&s.usr];
                        rel_id = ctx.external(&sd.syms[o.rel_sym as usize].clone())?;
                    } else if o.rel_roles & ix::REL_EXTENDED_BY != 0 {
                        // sym = extended type (maybe external), rel = the extension.
                        let Some(rel) = o.rel_sym.ne(&NONE).then(|| norm(o.rel_sym)).flatten() else { continue };
                        sym_id = ctx.external(s)?;
                        rel_id = ctx.ids[&sd.syms[rel as usize].usr];
                    } else {
                        if structural || !project_syms.contains(&s.usr) || info.local.contains(&o.sym) {
                            continue;
                        }
                        sym_id = ctx.ids[&s.usr];
                        rel_id = if o.rel_sym == NONE {
                            0
                        } else {
                            match norm(o.rel_sym) {
                                Some(x) => ctx.ids[&sd.syms[x as usize].usr],
                                None => 0,
                            }
                        };
                    }
                    if !seen.insert((sym_id as u32, rel_id as u32, o.line, o.col, o.rel_roles | (o.roles << 20))) {
                        continue;
                    }
                    ins.execute(params![rid, sym_id, o.line, o.col, o.roles as i64, o.rel_roles as i64, rel_id])?;
                    st.occurrences += 1;
                }
            }
        }
    }
    crate::code::link_parents(&tx, p.id)?;
    // 6. Prune symbols nothing refers to any more, then rebuild derived edges.
    prune_and_edges(&tx, p, &mut st)?;
    tx.commit()?;
    Ok(st)
}

fn prune_and_edges(tx: &Transaction, p: &Project, st: &mut ImportStats) -> Result<()> {
    let mut dead_syms: Vec<i64> = vec![];
    {
        let mut q = tx.prepare(
            "SELECT id FROM node WHERE project_id = ?1 AND source = 'indexstore' AND kind = 'symbol'
               AND id NOT IN (SELECT sym FROM occ) AND id NOT IN (SELECT rel_sym FROM occ WHERE rel_sym != 0)",
        )?;
        dead_syms.extend(q.query_map([p.id], |r| r.get::<_, i64>(0))?.flatten());
    }
    for id in &dead_syms {
        tx.execute("DELETE FROM fts_node WHERE rowid = ?1", [id])?;
        tx.execute("DELETE FROM fts_trigram WHERE rowid = ?1", [id])?;
        tx.execute("DELETE FROM fts_body WHERE rowid = ?1", [id])?;
        tx.execute("DELETE FROM node WHERE id = ?1", [id])?;
    }
    tx.execute("DELETE FROM edge WHERE source = 'indexstore' AND src IN (SELECT id FROM node WHERE project_id = ?1)", [p.id])?;
    let mask = |b: u64| b as i64;
    // CALLS: caller (rel) -> callee (sym).
    tx.execute(
        "INSERT OR REPLACE INTO edge(src, dst, kind, source, confidence, meta)
         SELECT rel_sym, sym, 'CALLS', 'indexstore', 1.0, json_object('n', COUNT(*)) FROM occ
         WHERE roles & ?1 != 0 AND rel_roles & ?2 != 0 AND rel_sym != 0 AND rel_sym != sym GROUP BY rel_sym, sym",
        params![mask(ix::R_CALL), mask(ix::REL_CALLED_BY)],
    )?;
    // REFERENCES: container -> referenced symbol (non-call references).
    tx.execute(
        "INSERT OR IGNORE INTO edge(src, dst, kind, source, confidence, meta)
         SELECT rel_sym, sym, 'REFERENCES', 'indexstore', 1.0, json_object('n', COUNT(*)) FROM occ
         WHERE roles & ?1 != 0 AND roles & ?2 = 0 AND rel_roles & ?3 != 0 AND rel_sym != 0 AND rel_sym != sym GROUP BY rel_sym, sym",
        params![mask(ix::R_REF), mask(ix::R_CALL), mask(ix::REL_CONTAINED_BY)],
    )?;
    // Conformance / inheritance: rel (type or extension) -> sym (base).
    tx.execute(
        "INSERT OR IGNORE INTO edge(src, dst, kind, source, confidence, meta)
         SELECT o.rel_sym, o.sym,
                CASE WHEN json_extract(b.meta, '$.k') = 'protocol' THEN 'CONFORMS_TO' ELSE 'INHERITS' END,
                'indexstore', 1.0, NULL
         FROM occ o JOIN node b ON b.id = o.sym
         WHERE o.rel_roles & ?1 != 0 AND o.rel_sym != 0 GROUP BY o.rel_sym, o.sym",
        params![mask(ix::REL_BASE_OF)],
    )?;
    // Extensions: extension (rel) -> extended type (sym).
    tx.execute(
        "INSERT OR IGNORE INTO edge(src, dst, kind, source, confidence, meta)
         SELECT rel_sym, sym, 'EXTENDS', 'indexstore', 1.0, NULL FROM occ WHERE rel_roles & ?1 != 0 AND rel_sym != 0 GROUP BY rel_sym, sym",
        params![mask(ix::REL_EXTENDED_BY)],
    )?;
    // OVERRIDES: implementation (sym) -> overridden / witnessed requirement (rel).
    tx.execute(
        "INSERT OR IGNORE INTO edge(src, dst, kind, source, confidence, meta)
         SELECT sym, rel_sym, 'OVERRIDES', 'indexstore', 1.0, NULL FROM occ WHERE rel_roles & ?1 != 0 AND rel_sym != 0 GROUP BY sym, rel_sym",
        params![mask(ix::REL_OVERRIDE_OF)],
    )?;
    // `extension T: P` also makes T conform to P.
    tx.execute(
        "INSERT OR IGNORE INTO edge(src, dst, kind, source, confidence, meta)
         SELECT x.dst, c.dst, c.kind, 'indexstore', 1.0, json_object('via', 'ext') FROM edge c
         JOIN edge x ON x.src = c.src AND x.kind = 'EXTENDS' AND x.source = 'indexstore'
         WHERE c.source = 'indexstore' AND c.kind IN ('CONFORMS_TO','INHERITS')
           AND c.src IN (SELECT id FROM node WHERE project_id = ?1 AND json_extract(meta, '$.k') = 'extension')",
        [p.id],
    )?;
    st.edges = tx.query_row(
        "SELECT COUNT(*) FROM edge WHERE source = 'indexstore' AND src IN (SELECT id FROM node WHERE project_id = ?1)",
        [p.id],
        |r| r.get(0),
    )?;
    Ok(())
}

/// Mark symbols stale when their source file changed after the newest unit that covers it was written.
pub fn refresh_stale(c: &Connection, p: &Project) -> Result<usize> {
    let mut q = c.prepare(
        "SELECT r.path, MAX(u.mtime) FROM swift_file f JOIN swift_rec r ON r.id = f.rec JOIN swift_unit u ON u.id = f.unit
         WHERE u.project_id = ?1 GROUP BY r.path",
    )?;
    let rows: Vec<(String, i64)> = q.query_map([p.id], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
    let mut stale_files = 0;
    let mut set = c.prepare("UPDATE node SET stale = ?3 WHERE project_id = ?1 AND source = 'indexstore' AND path = ?2 AND stale != ?3")?;
    for (path, unit_mt) in rows {
        let fm = mtime_ms(&p.root.join(&path));
        // Unit mtime is when the compile finished; any later write to the file may have changed it.
        let stale = fm == 0 || fm > unit_mt + 1000;
        if stale {
            stale_files += 1;
        }
        set.execute(params![p.id, path, stale as i64])?;
    }
    Ok(stale_files)
}

/// Import (incrementally) every discovered store for the project.
pub fn import_project(c: &mut Connection, p: &Project) -> Result<ImportStats> {
    let t0 = std::time::Instant::now();
    let stores = discover_stores(&p.root);
    let mut data = vec![];
    let mut reached = HashSet::new();
    let known_recs: HashSet<String> = {
        let mut q = c.prepare("SELECT name FROM swift_rec")?;
        q.query_map([], |r| r.get::<_, String>(0))?.flatten().collect()
    };
    for dir in &stores {
        let skey = dir.to_string_lossy().into_owned();
        let known: HashMap<String, i64> = {
            let mut q = c.prepare("SELECT unit, mtime FROM swift_unit WHERE store = ?1")?;
            q.query_map([&skey], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.flatten().collect()
        };
        match read_store(dir, &p.root, &known, &known_recs) {
            Ok(sd) => {
                reached.insert(skey);
                data.push(sd);
            }
            Err(e) => eprintln!("fox: skipping store {}: {e:#}", dir.display()),
        }
    }
    let first_import = c.query_row("SELECT COUNT(*) FROM swift_unit WHERE project_id = ?1", [p.id], |r| r.get::<_, i64>(0))? == 0;
    let changed = data.iter().any(|d| !d.units.is_empty());
    let vanished = {
        let present: HashSet<String> = data.iter().flat_map(|s| s.present.iter().map(|u| format!("{}\u{1}{u}", s.store))).collect();
        let mut q = c.prepare("SELECT store || '\u{1}' || unit FROM swift_unit WHERE project_id = ?1")?;
        let all: Vec<String> = q.query_map([p.id], |r| r.get(0))?.flatten().collect();
        all.iter().any(|k| reached.contains(k.split('\u{1}').next().unwrap_or("")) && !present.contains(k))
    };
    let mut st = if changed || vanished || first_import {
        apply(c, p, &data, &reached)?
    } else {
        ImportStats { stores: data.len(), units_seen: data.iter().map(|d| d.present.len()).sum(), ..Default::default() }
    };
    st.stale_files = refresh_stale(c, p)?;
    st.ms = t0.elapsed().as_millis();
    c.execute(
        "INSERT OR REPLACE INTO ingest_state(source, project_id, cursor, updated_at) VALUES ('indexstore', ?1, ?2, ?3)",
        params![p.id, st.units_seen.to_string(), db::now()],
    )?;
    let _ = guard::opened; // guard is the only reader of project files; the importer only stats them
    if st.units_changed > 0 || st.stale_files > 0 {
        db::bump_project(c, p.id)?;
    }
    Ok(st)
}

pub fn has_symbols(c: &Connection, pid: i64) -> bool {
    c.query_row("SELECT 1 FROM node WHERE project_id = ?1 AND source = 'indexstore' AND kind = 'symbol' LIMIT 1", [pid], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}
