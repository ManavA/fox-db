//! A cheaper change detector for very big trees. Walking a Linux-sized tree (about 100k files
//! in 6k directories, with `.gitignore` rules) took most of a one-file save. A tree of at least
//! [`MIN_FILES`] files keeps its last walk in `$FOXDB_HOME/walk/<project id>`: the directories it
//! entered with their modification times, the files in each, and the ignore files it obeyed.
//!
//! The next index run stats instead of walking: every recorded directory (a file added, removed
//! or renamed inside changes its directory's time), every ignore file and the overrides file,
//! then every recorded file (size and time, as the walk compared them). Only directories whose
//! time moved are walked again, with their whole subtree, by the same walker restricted to them,
//! so their ignore rules come out the same. It falls back to the full walk whenever it cannot be
//! sure: no or unreadable record, a changed ignore or overrides file or git config (`.git` itself
//! counts only by its presence: git touches it on nearly every command), a changed
//! root, a directory or file that vanished, or a directory time within two seconds of the last
//! record (a filesystem with coarse times could hide a change there). Smaller trees never write a
//! record and always walk.

use crate::inventory::{DirNote, Entry, Keep, Walk, mtime_ms, walk};
use crate::project::Project;
use crate::tiers::Overrides;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// Trees with at least this many files keep a walk record; smaller ones walk every time.
pub const MIN_FILES: usize = 50_000;

/// A directory time this close to the record's (or newer) is not trusted.
const RACY_NS: i128 = 2_000_000_000;

const MAGIC: &str = "foxdb-walk 1";

fn record_path(pid: i64) -> PathBuf {
    crate::db::home().join("walk").join(pid.to_string())
}

fn ns(md: &std::fs::Metadata) -> i128 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i128).unwrap_or(-1)
}

/// (modification time in ns, size) of `p`, or None when it does not exist.
fn stamp(p: &Path) -> Option<(i128, u64)> {
    std::fs::symlink_metadata(p).ok().map(|m| (ns(&m), m.len()))
}

/// Files outside the tree whose change can change what the walk keeps: the tier overrides, git's
/// global config files and global excludes file. Each with its stamp (None when absent).
fn outside(ov_path: &Path) -> Stamped {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|x| !x.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".config"));
    let mut ps = vec![ov_path.to_path_buf(), home.join(".gitconfig"), xdg.join("git/config"), xdg.join("git/ignore")];
    if let Some(g) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        ps.push(PathBuf::from(g));
    }
    if let Some(x) = ignore::gitignore::gitconfig_excludes_path() {
        ps.push(x);
    }
    ps.into_iter().map(|p| (p.clone(), stamp(&p))).collect()
}

/// The stamp of an ignore file. `.git` is listed only because its presence makes the root a
/// repository: a directory counts by its presence alone, since git rewrites files inside it (and
/// so moves its time) on nearly every command.
fn ignore_stamp(p: &Path) -> Option<(i128, u64)> {
    let md = std::fs::symlink_metadata(p).ok()?;
    Some(if md.is_dir() { (0, 0) } else { (ns(&md), md.len()) })
}

/// The ignore files the walker reads in directory `dir` (and git's exclude file at the root).
fn ignore_files(root: &Path, dir: &str) -> Vec<PathBuf> {
    let d = if dir.is_empty() { root.to_path_buf() } else { root.join(dir) };
    let mut v = vec![d.join(".gitignore"), d.join(".ignore")];
    if dir.is_empty() {
        v.push(root.join(".git"));
        v.push(root.join(".git/info/exclude"));
    }
    v
}

/// The files directly in a directory: (name, size, time in ms).
type Files = Vec<(String, u64, i64)>;
/// A file's stamp outside the tree: (path, (time in ns, size) when it exists).
type Stamped = Vec<(PathBuf, Option<(i128, u64)>)>;
/// The previous run's files: (file generation, path -> (size, time in ms)).
type Prev = (i64, HashMap<String, (u64, i64)>);

#[derive(Default)]
struct Record {
    root: String,
    taken: i128,
    /// The project's file generation (see [`filegen`]) the store had when this was written.
    filegen: i64,
    outside: Stamped,
    /// Ignore files that existed, with their stamps.
    ignores: Vec<(PathBuf, (i128, u64))>,
    /// Directory -> its time, the files directly in it (name, size, time in ms), and notes on
    /// subdirectories left out.
    dirs: Vec<(String, i128, Files, Vec<DirNote>)>,
}

fn esc(s: &str) -> Option<&str> {
    (!s.contains(['\n', '\r', '\t'])).then_some(s)
}

fn note_line(n: &DirNote) -> String {
    match n {
        DirNote::Skip(r) => format!("n\tS\t{r}"),
        DirNote::NameOnly(r) => format!("n\tN\t{r}"),
        DirNote::NestedRepo(r) => format!("n\tR\t{r}"),
    }
}

fn stamp_text(s: Option<(i128, u64)>) -> String {
    s.map(|(t, l)| format!("{t}\t{l}")).unwrap_or_else(|| "-\t-".into())
}

fn parse_stamp(t: &str, l: &str) -> Option<Option<(i128, u64)>> {
    if t == "-" {
        return Some(None);
    }
    Some(Some((t.parse().ok()?, l.parse().ok()?)))
}

impl Record {
    fn write(&self, path: &Path) -> Option<()> {
        let mut s = String::with_capacity(self.dirs.len() * 64);
        s += &format!("{MAGIC}\nroot\t{}\ntaken\t{}\ngen\t{}\n", esc(&self.root)?, self.taken, self.filegen);
        for (p, st) in &self.outside {
            s += &format!("o\t{}\t{}\n", stamp_text(*st), esc(p.to_str()?)?);
        }
        for (p, (t, l)) in &self.ignores {
            s += &format!("i\t{t}\t{l}\t{}\n", esc(p.to_str()?)?);
        }
        for (d, t, files, notes) in &self.dirs {
            s += &format!("d\t{t}\t{}\n", esc(d)?);
            for (f, size, mtime) in files {
                s += &format!("f\t{size}\t{mtime}\t{}\n", esc(f)?);
            }
            for n in notes {
                s += &note_line(n);
                s.push('\n');
            }
        }
        std::fs::create_dir_all(path.parent()?).ok()?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, s).ok()?;
        std::fs::rename(&tmp, path).ok()
    }

    fn read(path: &Path) -> Option<Record> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut lines = text.lines();
        if lines.next()? != MAGIC {
            return None;
        }
        let mut r = Record { root: lines.next()?.strip_prefix("root\t")?.to_string(), ..Default::default() };
        r.taken = lines.next()?.strip_prefix("taken\t")?.parse().ok()?;
        r.filegen = lines.next()?.strip_prefix("gen\t")?.parse().ok()?;
        for l in lines {
            let mut f = l.split('\t');
            match f.next()? {
                "o" => {
                    let st = parse_stamp(f.next()?, f.next()?)?;
                    r.outside.push((PathBuf::from(f.next()?), st));
                }
                "i" => {
                    let st = parse_stamp(f.next()?, f.next()?)??;
                    r.ignores.push((PathBuf::from(f.next()?), st));
                }
                "d" => {
                    let t: i128 = f.next()?.parse().ok()?;
                    r.dirs.push((f.next().unwrap_or("").to_string(), t, vec![], vec![]));
                }
                "f" => {
                    let size = f.next()?.parse().ok()?;
                    let mtime = f.next()?.parse().ok()?;
                    r.dirs.last_mut()?.2.push((f.next()?.to_string(), size, mtime));
                }
                "n" => {
                    let k = f.next()?;
                    let rel = f.next()?.to_string();
                    let n = match k {
                        "S" => DirNote::Skip(rel),
                        "N" => DirNote::NameOnly(rel),
                        "R" => DirNote::NestedRepo(rel),
                        _ => return None,
                    };
                    r.dirs.last_mut()?.3.push(n);
                }
                _ => return None,
            }
        }
        Some(r)
    }
}

fn parent(rel: &str) -> &str {
    rel.rsplit_once('/').map(|x| x.0).unwrap_or("")
}

fn note_rel(n: &DirNote) -> &str {
    match n {
        DirNote::Skip(r) | DirNote::NameOnly(r) | DirNote::NestedRepo(r) => r,
    }
}

/// Is `rel` `d` or below it?
fn under(rel: &str, d: &str) -> bool {
    d.is_empty() || rel == d || (rel.len() > d.len() && rel.starts_with(d) && rel.as_bytes()[d.len()] == b'/')
}

/// Turn a walk into a record (None when a path cannot be written down).
fn record(root: &Path, w: &Walk, taken: i128, outside: Stamped, filegen: i64) -> Option<Record> {
    let mut by_dir: HashMap<&str, (Files, Vec<DirNote>)> = HashMap::new();
    for e in &w.entries {
        let (d, name) = e.rel.rsplit_once('/').unwrap_or(("", &e.rel));
        by_dir.entry(d).or_default().0.push((name.to_string(), e.size, e.mtime));
    }
    for n in &w.notes {
        by_dir.entry(parent(note_rel(n))).or_default().1.push(n.clone());
    }
    let mut r = Record { root: root.to_str()?.to_string(), taken, outside, filegen, ..Default::default() };
    for d in &w.dirs {
        let md = std::fs::symlink_metadata(if d.is_empty() { root.to_path_buf() } else { root.join(d) }).ok()?;
        let (files, notes) = by_dir.remove(d.as_str()).unwrap_or_default();
        r.dirs.push((d.clone(), ns(&md), files, notes));
        for f in ignore_files(root, d) {
            if let Some(st) = ignore_stamp(&f) {
                r.ignores.push((f, st));
            }
        }
    }
    // Every file and note belongs to a directory the walk entered.
    by_dir.is_empty().then_some(r)
}

/// What [`scan`] found, and what [`save`] needs to write the next record.
pub(crate) struct Scan {
    pub(crate) walk: Walk,
    /// The files as the record saw them, when the record served this walk: (file generation it
    /// was written at, path -> (size, time in ms)). When that generation is still the store's,
    /// the store's file rows are exactly these, so a run can compare against them instead of
    /// loading every row.
    pub(crate) prev: Option<Prev>,
    /// Directories walked again (0 when nothing moved).
    pub(crate) dirty: usize,
    rec: PathBuf,
    taken: i128,
    outside: Stamped,
    min_files: usize,
}

impl Scan {
    /// Does this tree keep a record (so file changes must bump [`filegen`])?
    pub(crate) fn recorded(&self) -> bool {
        self.prev.is_some() || self.rec.exists() || self.walk.entries.len() >= self.min_files
    }
}

/// The walk of project `p`'s tree: from its record when that is safe, else a full walk.
pub(crate) fn scan(p: &Project, root: &Path, ov: &Arc<Overrides>) -> Scan {
    let ov_path = crate::db::home().join("projects").join(format!("{}.toml", p.name));
    // `FOXDB_WALK_MIN_FILES` lowers the threshold (tests and benchmarks).
    let min = std::env::var("FOXDB_WALK_MIN_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(MIN_FILES);
    scan_in(&record_path(p.id), &ov_path, root, ov, min)
}

fn scan_in(rec: &Path, ov_path: &Path, root: &Path, ov: &Arc<Overrides>, min_files: usize) -> Scan {
    let taken = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i128).unwrap_or(0);
    let outside = outside(ov_path);
    let mut sc = Scan {
        walk: Walk { entries: vec![], notes: vec![], dirs: vec![] },
        prev: None,
        dirty: 0,
        rec: rec.to_path_buf(),
        taken,
        outside,
        min_files,
    };
    if rec.exists() {
        if let Some(r) = Record::read(rec)
            && let Some((w, dirty)) = fast(root, ov, &r, &sc.outside)
        {
            let files = r
                .dirs
                .iter()
                .flat_map(|(d, _, fs, _)| {
                    fs.iter().map(move |(f, size, mtime)| (if d.is_empty() { f.clone() } else { format!("{d}/{f}") }, (*size, *mtime)))
                })
                .collect();
            sc.walk = w;
            sc.prev = Some((r.filegen, files));
            sc.dirty = dirty;
            return sc;
        }
        trace("walk record unusable: full walk");
    }
    sc.walk = walk(root, ov, None);
    sc
}

/// After the store took the walk (file generation `filegen`), write the record for the next
/// run, or remove it from a tree that is now small. A run where nothing moved keeps the old one.
pub(crate) fn save(sc: &Scan, root: &Path, filegen: i64, changed: bool) {
    if sc.walk.entries.len() < sc.min_files {
        if sc.rec.exists() {
            let _ = std::fs::remove_file(&sc.rec);
        }
        return;
    }
    if sc.prev.as_ref().is_some_and(|p| p.0 == filegen) && sc.dirty == 0 && !changed {
        return;
    }
    match record(root, &sc.walk, sc.taken, sc.outside.clone(), filegen) {
        Some(r) => {
            r.write(&sc.rec);
        }
        None => {
            let _ = std::fs::remove_file(&sc.rec);
        }
    }
}

/// Project `pid`'s file generation: moved by every index run that changes its file rows, in the
/// same transaction, for trees that keep a record (0 before the first).
pub fn filegen(c: &rusqlite::Connection, pid: i64) -> i64 {
    c.query_row("SELECT v FROM meta WHERE k = 'files:' || ?1", [pid], |r| r.get(0)).unwrap_or(0)
}

pub fn bump_filegen(c: &rusqlite::Connection, pid: i64) -> anyhow::Result<()> {
    c.execute("INSERT INTO meta(k, v) VALUES ('files:' || ?1, 1) ON CONFLICT(k) DO UPDATE SET v = v + 1", [pid])?;
    Ok(())
}

fn trace(what: &str) {
    if std::env::var_os("FOXDB_TRACE").is_some() {
        eprintln!("{what}");
    }
}

/// The walk rebuilt from record `r`, or None when anything says a full walk is needed.
fn fast(root: &Path, ov: &Arc<Overrides>, r: &Record, outside_now: &[(PathBuf, Option<(i128, u64)>)]) -> Option<(Walk, usize)> {
    if r.root != root.to_str()? || r.outside != outside_now {
        return None;
    }
    if r.ignores.par_iter().any(|(p, st)| ignore_stamp(p) != Some(*st)) {
        return None;
    }
    // Directories whose time moved (or is too close to the record to trust).
    let changed: Vec<bool> = r
        .dirs
        .par_iter()
        .map(|(d, t, _, _)| {
            let now = stamp(&if d.is_empty() { root.to_path_buf() } else { root.join(d) }).map(|s| s.0);
            now != Some(*t) || *t >= r.taken - RACY_NS
        })
        .collect();
    let mut dirty: Vec<&str> = r.dirs.iter().zip(&changed).filter(|x| *x.1).map(|x| x.0.0.as_str()).collect();
    // Keep the topmost: a subtree is walked whole.
    dirty.sort_unstable();
    let mut top: Vec<&str> = vec![];
    for d in dirty {
        if !top.iter().any(|t| under(d, t)) {
            top.push(d);
        }
    }
    if top.contains(&"") {
        return None;
    }
    for d in &top {
        // A directory that vanished means its parent changed too; be safe.
        if !root.join(d).is_dir() {
            return None;
        }
    }
    let in_dirty = |rel: &str| top.iter().any(|t| under(rel, t));
    // Recorded files outside the changed subtrees, stat'ed again.
    let kept: Vec<(&str, &str)> = r
        .dirs
        .iter()
        .filter(|x| !in_dirty(&x.0))
        .flat_map(|(d, _, files, _)| files.iter().map(move |f| (d.as_str(), f.0.as_str())))
        .collect();
    let stats: Vec<Option<Entry>> = kept
        .par_iter()
        .map(|(d, f)| {
            let rel = if d.is_empty() { f.to_string() } else { format!("{d}/{f}") };
            let abs = root.join(&rel);
            let md = std::fs::symlink_metadata(&abs).ok().filter(|m| m.is_file())?;
            Some(Entry { rel, abs, size: md.len(), mtime: mtime_ms(&md) })
        })
        .collect();
    if stats.iter().any(Option::is_none) {
        return None;
    }
    let mut entries: Vec<Entry> = stats.into_iter().flatten().collect();
    let mut notes: Vec<DirNote> = r.dirs.iter().filter(|x| !in_dirty(&x.0)).flat_map(|x| x.3.iter().cloned()).collect();
    notes.retain(|n| !in_dirty(note_rel(n)));
    let mut dirs: Vec<String> = r.dirs.iter().filter(|x| !in_dirty(&x.0)).map(|x| x.0.clone()).collect();
    if !top.is_empty() {
        // The same walker, entering only the changed subtrees and the directories above them.
        let tops: Vec<String> = top.iter().map(|t| t.to_string()).collect();
        let keep: Keep = Arc::new(move |rel: &str| tops.iter().any(|t| under(rel, t) || under(t, rel)));
        let w = walk(root, ov, Some(keep));
        entries.extend(w.entries.into_iter().filter(|e| in_dirty(parent(&e.rel))));
        notes.extend(w.notes.into_iter().filter(|n| in_dirty(note_rel(n))));
        dirs.extend(w.dirs.into_iter().filter(|d| !d.is_empty() && in_dirty(d)));
        trace(&format!("walk record: {} changed dirs walked again", top.len()));
    }
    Some((Walk { entries, notes, dirs }, top.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(w: &Walk) -> Vec<(String, u64, i64)> {
        let mut v: Vec<(String, u64, i64)> = w.entries.iter().map(|e| (e.rel.clone(), e.size, e.mtime)).collect();
        v.sort();
        v
    }

    fn notes(w: &Walk) -> Vec<String> {
        let mut v: Vec<String> = w.notes.iter().map(note_line).collect();
        v.sort();
        v
    }

    /// After each kind of change, the walk rebuilt from the record equals a full walk.
    #[test]
    fn record_walk_matches_full_walk_after_changes() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        let rec = t.path().join("rec");
        let ovp = t.path().join("ov.toml");
        let w = |p: &str, s: &str| {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, s).unwrap();
        };
        w(".gitignore", "*.o\nbuild/\n");
        for i in 0..40 {
            w(&format!("src/m{}/f{i}.c", i % 5), "int x;");
        }
        w("src/m1/deep/a.c", "a");
        w("build/out.c", "ignored");
        w("node_modules/x/index.js", "name only");
        let ov = Arc::new(Overrides::default());
        let full = || walk(&root, &ov, None);
        // `quick`: whether the record must serve (no full walk) for this change.
        let check = |label: &str, quick: bool| {
            let r = Record::read(&rec).unwrap();
            let got = fast(&root, &ov, &r, &outside(&ovp)).map(|x| x.0);
            assert_eq!(got.is_some(), quick, "{label}");
            let f = full();
            if let Some(g) = &got {
                assert_eq!(files(g), files(&f), "{label}");
                assert_eq!(notes(g), notes(&f), "{label}");
            }
            let sc = scan_in(&rec, &ovp, &root, &ov, 1);
            assert_eq!(files(&sc.walk), files(&f), "{label}");
            assert_eq!(notes(&sc.walk), notes(&f), "{label}");
            save(&sc, &root, 0, true);
        };
        let sc = scan_in(&rec, &ovp, &root, &ov, 1);
        save(&sc, &root, 0, true);
        assert!(rec.exists());
        // Make every recorded directory time older than the racy window, as after a while.
        let age = |r: &Path| {
            let mut x = Record::read(r).unwrap();
            x.taken += 10 * RACY_NS;
            x.write(r).unwrap();
        };
        age(&rec);
        check("nothing changed", true);
        std::thread::sleep(std::time::Duration::from_millis(20));
        w("src/m2/f2.c", "int changed_in_place;");
        age(&rec);
        check("a file edited in place", true);
        w("src/m3/new.c", "new");
        std::fs::rename(root.join("src/m4/f4.c"), root.join("src/m4/renamed.c")).unwrap();
        std::fs::remove_file(root.join("src/m0/f0.c")).unwrap();
        w("src/m1/deep/deeper/b.c", "b");
        w("src/m1/deep/x.o", "ignored object");
        age(&rec);
        check("files added, renamed, removed, a new subdirectory, an ignored file", true);
        w("src/m2/.gitignore", "f7.c\n");
        age(&rec);
        check("a new ignore file in a subdirectory", true);
        w("src/m2/.gitignore", "f12.c\n");
        age(&rec);
        check("an ignore file edited in place", false);
        std::fs::create_dir_all(root.join("src/m3/.git")).unwrap();
        age(&rec);
        check("a directory that became a nested repository", true);
        // Git rewrites files inside `.git` on nearly every command (status, add, commit): that
        // moves `.git`'s own time but changes nothing the walk keeps.
        w(".git/info/exclude", "*.tmp\n");
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        let sc = scan_in(&rec, &ovp, &root, &ov, 1);
        save(&sc, &root, 0, true);
        age(&rec);
        check("a new .git at the root", true);
        std::thread::sleep(std::time::Duration::from_millis(20));
        w(".git/index", "git status rewrote the index");
        w(".git/objects/ab", "an object");
        age(&rec);
        check("git wrote inside .git", true);
        w(".git/info/exclude", "*.c\n");
        age(&rec);
        check("git's exclude file edited", false);
        // Directory times too close to the record are not trusted (here the root's: a full walk).
        w("src/m0/fresh.c", "fresh");
        check("recent directory times", false);
        // A small tree keeps no record.
        let sc = scan_in(&rec, &ovp, &root, &ov, 1_000_000);
        save(&sc, &root, 0, true);
        assert!(!rec.exists());
    }

    #[test]
    fn under_matches_whole_path_segments() {
        assert!(under("a/b", "a") && under("a", "a") && under("x", ""));
        assert!(!under("ab", "a") && !under("a", "a/b"));
    }
}
