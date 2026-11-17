//! Design docs anchored to code: `fox drift` lists cited symbols and paths that changed, moved or
//! vanished since the doc's last commit (git + index, no LLM); `fox pack` emits the doc plus the
//! exact skeletons of what it cites, within a budget.

use foxdb::{db, docref, inventory, project, tsindex};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

const DOC: &str = "# Design\n\n## Storage\n\nThe entry point is `alpha` in `src/a.rs`; it builds a `Gamma`.\n\n## Legacy\n\n`delta_gone` lived in `src/old.rs:3` and `beta` stays as is.\nPlain words like `the` and `cargo test` are not code.\n";

fn setup() -> (tempfile::TempDir, tempfile::TempDir, rusqlite::Connection, project::Project) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    let r = repo.path();
    write(r, "src/a.rs", "pub fn alpha(x: u32) -> u32 {\n    x + 1\n}\n\npub fn beta() {}\n");
    write(r, "src/b.rs", "pub struct Gamma {\n    pub n: u32,\n}\n");
    let shared =
        "// legacy helpers kept for the migration\n// see the storage section of the design doc\n// nothing here is on the hot path\n";
    write(r, "src/old.rs", &format!("{shared}pub fn delta_gone() {{}}\n"));
    write(r, "docs/design.md", DOC);
    git(r, &["init", "-q"]);
    git(r, &["add", "."]);
    git(r, &["commit", "-q", "-m", "doc"]);
    // After the doc was written: alpha changes, old.rs moves and loses delta_gone.
    write(r, "src/a.rs", "pub fn alpha(x: u32, y: u32) -> u32 {\n    x + y\n}\n\npub fn beta() {}\n");
    git(r, &["mv", "src/old.rs", "src/new.rs"]);
    write(r, "src/new.rs", &format!("{shared}pub fn delta_renamed() {{}}\n"));
    git(r, &["commit", "-q", "-am", "code moves on"]);
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(r)).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(r)).unwrap().unwrap();
    (home, repo, c, p)
}

#[test]
fn citations_are_code_shaped_backtick_spans() {
    let cites = docref::citations(DOC);
    let names: Vec<&str> = cites.iter().map(|c| c.text.as_str()).collect();
    assert!(names.contains(&"alpha") && names.contains(&"src/a.rs") && names.contains(&"Gamma") && names.contains(&"src/old.rs:3"));
    assert!(!names.contains(&"cargo test"), "{names:?}");
    let old = cites.iter().find(|c| c.text == "src/old.rs:3").unwrap();
    assert_eq!(old.section, "Legacy");
}

#[test]
fn drift_reports_changed_moved_and_vanished_deterministically() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, c, p) = setup();
    let out = docref::drift(&c, &p, "docs/design.md").unwrap();
    assert!(out.lines().any(|l| l.contains("changed") && l.contains("alpha") && l.contains("src/a.rs:1")), "{out}");
    assert!(out.lines().any(|l| l.contains("moved") && l.contains("src/old.rs") && l.contains("src/new.rs")), "{out}");
    assert!(out.lines().any(|l| l.contains("vanished") && l.contains("delta_gone")), "{out}");
    assert!(!out.lines().any(|l| l.contains("Gamma") && !l.starts_with('#')), "unchanged citations are not drift:\n{out}");
    assert!(!out.contains("`the`") && !out.contains("cargo test"), "{out}");
    assert_eq!(out, docref::drift(&c, &p, "docs/design.md").unwrap());
}

#[test]
fn pack_is_the_doc_plus_cited_skeletons_within_budget() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, c, p) = setup();
    let out = docref::pack(&c, &p, "docs/design.md", 600).unwrap();
    assert!(out.contains("## Storage") && out.contains("The entry point is `alpha`"), "{out}");
    assert!(out.contains("pub fn alpha(x: u32, y: u32) -> u32 {"), "{out}");
    assert!(out.contains("pub struct Gamma {"), "{out}");
    assert!(!out.contains("x + y"), "skeletons only, no bodies:\n{out}");
    assert!(out.contains("not found: delta_gone"), "{out}");
    assert!(out.len() <= 600 * 4, "{}", out.len());
    let tiny = docref::pack(&c, &p, "docs/design.md", 60).unwrap();
    assert!(tiny.len() <= 60 * 4 + 80, "{}", tiny.len());
    assert_eq!(out, docref::pack(&c, &p, "docs/design.md", 600).unwrap());
}
