//! `fox watch`: FSEvents/inotify (via notify) watcher that keeps the code graph fresh.
//!
//! * a build writes new units under `DataStore/v5/units` -> incremental Swift import;
//! * an edit to a source file -> its symbols are marked stale at once (no waiting for a build);
//! * other changes -> incremental inventory refresh, plus a native tree-sitter refresh
//!   (only changed files are re-parsed) and, if enabled, a codanna mirror sync.
//!
//! On macOS FSEvents watches the root recursively for free. inotify needs one watch per directory,
//! so elsewhere only directories the inventory would walk are watched (gitignored, skip and
//! name-only dirs such as node_modules or DerivedData are left out). Directories created after
//! start are picked up when the watcher is restarted (the systemd unit restarts it daily).
//! Events are debounced; reads of the index store are read-only and nothing here ever builds.

use crate::project::Project;
use crate::{db, inventory, swiftimport, tiers};
use anyhow::Result;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dirty {
    pub store: bool,
    pub swift_src: bool,
    pub other: bool,
    pub ts_src: bool,
}

fn is_ts_src(rel: &str) -> bool {
    crate::lang::for_path(rel).is_some_and(|l| l.name != "swift")
}

/// Directories to watch non-recursively when the platform has no cheap recursive watch.
pub fn watch_dirs(p: &Project) -> Vec<PathBuf> {
    let root = p.root.clone();
    let ov = std::sync::Arc::new(inventory::overrides_for(&p.name));
    let mut wb = ignore::WalkBuilder::new(&root);
    wb.hidden(false).git_ignore(true).git_global(true).git_exclude(true).ignore(true).parents(false).require_git(false).follow_links(false);
    let r2 = root.clone();
    wb.filter_entry(move |e| {
        if e.depth() == 0 {
            return true;
        }
        if !e.file_type().is_some_and(|t| t.is_dir()) {
            return false;
        }
        let rel = e.path().strip_prefix(&r2).unwrap_or(e.path()).to_string_lossy().replace('\\', "/");
        rel != ".git" && tiers::classify_dir(&rel, &ov) >= tiers::Tier::Text
    });
    wb.build().flatten().filter(|e| e.file_type().is_some_and(|t| t.is_dir())).map(|e| e.into_path()).collect()
}

/// Classify one changed path.
pub fn classify(root: &Path, stores: &[PathBuf], path: &Path, d: &mut Dirty) {
    if stores.iter().any(|s| path.starts_with(s)) {
        if path.components().any(|c| c.as_os_str() == "units") {
            d.store = true;
        }
        return;
    }
    let Ok(rel) = path.strip_prefix(root) else { return };
    let rel = rel.to_string_lossy();
    if rel.starts_with(".git/")
        || rel == ".git"
        || tiers::is_denied_path(&rel)
        || tiers::is_secret_name(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
    {
        return;
    }
    if rel.ends_with(".swift") {
        d.swift_src = true;
    }
    if is_ts_src(&rel) {
        d.ts_src = true;
    }
    d.other = true;
}

/// Apply accumulated changes. Returns a one-line log.
pub fn apply(p: &Project, d: Dirty) -> Result<String> {
    let mut c = db::open_rw()?;
    let mut log = vec![];
    let mut delta = None;
    if d.other {
        let st = inventory::index_project(&mut c, p)?;
        log.push(format!("inventory {} changed", st.files_changed));
        delta = st.delta;
    }
    if d.ts_src || d.other {
        let st = crate::tsindex::index_project_with(&mut c, p, delta.as_ref())?;
        if st.parsed + st.removed > 0 {
            log.push(format!("tree-sitter {} parsed, {} removed", st.parsed, st.removed));
        }
    }
    if d.ts_src && crate::config::get().codanna && crate::codanna::available() && crate::codanna::needed(&c, p) {
        let st = crate::codanna::sync(&c, p)?;
        log.push(format!("codanna {} copied, {} removed", st.copied, st.removed));
    }
    #[cfg(feature = "vectors")]
    if crate::config::vectors_on() && crate::vectors::model_present() && (d.ts_src || d.other) {
        let st = crate::vectors::refresh(&c, p, false)?;
        if !st.current {
            log.push(format!("vectors {} files parsed, {} embedded", st.parsed, st.embedded));
        }
    }
    if d.store {
        let st = swiftimport::import_project(&mut c, p)?;
        log.push(format!("swift {}/{} units, {} stale files, {} ms", st.units_changed, st.units_seen, st.stale_files, st.ms));
    } else if d.swift_src {
        let n = swiftimport::refresh_stale(&c, p)?;
        log.push(format!("{n} stale files"));
    }
    Ok(log.join("; "))
}

pub struct Handle {
    // Dropping stops the watcher.
    _w: RecommendedWatcher,
    pub rx: mpsc::Receiver<notify::Result<notify::Event>>,
}

pub fn start(p: &Project) -> Result<(Handle, Vec<PathBuf>)> {
    let (tx, rx) = mpsc::channel();
    let mut w = RecommendedWatcher::new(
        move |e| {
            let _ = tx.send(e);
        },
        Config::default(),
    )?;
    let stores = swiftimport::discover_stores(&p.root);
    for s in &stores {
        let _ = w.watch(&s.join("v5/units"), RecursiveMode::NonRecursive);
    }
    if cfg!(target_os = "macos") {
        w.watch(&p.root, RecursiveMode::Recursive)?;
    } else {
        for d in watch_dirs(p) {
            let _ = w.watch(&d, RecursiveMode::NonRecursive);
        }
    }
    Ok((Handle { _w: w, rx }, stores))
}

/// Block until the next quiet batch of events; returns what changed (None on timeout).
pub fn next_batch(h: &Handle, root: &Path, stores: &[PathBuf], debounce: Duration, timeout: Duration) -> Option<Dirty> {
    let deadline = Instant::now() + timeout;
    let mut d = Dirty::default();
    let mut any = false;
    loop {
        let wait = if any { debounce } else { deadline.saturating_duration_since(Instant::now()) };
        match h.rx.recv_timeout(wait) {
            Ok(Ok(ev)) => {
                let before = d;
                for pth in &ev.paths {
                    classify(root, stores, pth, &mut d);
                }
                any |= d != before || any;
            }
            Ok(Err(_)) => {}
            Err(_) => return if any { Some(d) } else { None },
        }
    }
}

/// Foreground watch loop (`fox watch`).
pub fn run(p: &Project) -> Result<()> {
    let (h, stores) = start(p)?;
    eprintln!("fox watch: {} ({} index stores)", p.root.display(), stores.len());
    loop {
        if let Some(d) = next_batch(&h, &p.root, &stores, Duration::from_millis(400), Duration::from_secs(3600)) {
            let t = Instant::now();
            match apply(p, d) {
                Ok(l) => eprintln!("fox watch: {l} ({} ms)", t.elapsed().as_millis()),
                Err(e) => eprintln!("fox watch: {e:#}"),
            }
        }
    }
}
