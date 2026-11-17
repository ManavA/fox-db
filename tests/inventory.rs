//! Integration tests. FOXDB_HOME is process-global, so tests serialize on a mutex.

use foxdb::{db, guard, inventory, markdown, project, query};
use std::fs;
use std::os::unix::fs::PermissionsExt;
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

const SECRET_TOKEN: &str = "zzsecrettokenqq";

/// A repo shaped like example-ios-app: secrets at the root, a git-TRACKED .agent-review snapshot tree,
/// DerivedData, a worktree dir, a nested repo, and real sources/docs.
fn fixture(root: &Path) {
    write(root, "Sources/App/HomeShelfStore.swift", "struct HomeShelfStore {}\n");
    write(root, "AGENTS.md", "# Agents\n\n## Rules\n\nnever create a new DerivedData path\n");
    write(
        root,
        "AGENT_MILESTONE_LOG.md",
        "# Log\n\n## Current gate\n\n- Gate: PERF2-P2.4 is active.\n\n## 2026-10-19 — PERF2-P2.4 parallel Home sources — IN_PROGRESS\n\nbase `75bd5b1242`\n\n## 2026-10-18 — PERF2-P2.3 thing — APPROVED\n\nok\n",
    );
    for s in [
        "AuthKey_ABCDE12345.p8",
        "certificate.p12",
        "distribution.p12",
        ".env",
        ".env.local",
        "Config/Local.xcconfig",
        "keys/id_ed25519",
        "sign/app.mobileprovision",
    ] {
        write(root, s, &format!("{SECRET_TOKEN}\n"));
    }
    write(root, ".agent-review/PXL-31/Snapshot.swift", &format!("struct Snapshot {{ let t = \"{SECRET_TOKEN}\" }}\n"));
    write(root, ".agent-review/PXL-31/notes.md", &format!("# {SECRET_TOKEN}\n"));
    write(root, ".agent-review/AU-1/Copy.kt", "class Copy\n");
    write(root, ".DerivedDataPhysicalRelease/Index.noindex/x.swift", &format!("// {SECRET_TOKEN}\n"));
    write(root, ".claude/worktrees/wt1/Sources/Dup.swift", &format!("// {SECRET_TOKEN}\n"));
    write(root, ".gitignore", ".DerivedData*/\n");
    git(root, &["init", "-q"]);
    // Track everything, including the secrets and .agent-review (as in example-ios-app: tracked files must still be denied).
    git(root, &["add", "-f", "."]);
    git(root, &["commit", "-qm", "init"]);
    write(root, ".server-work/example-android/app/Main.kt", &format!("// {SECRET_TOKEN}\n"));
    git(&root.join(".server-work/example-android"), &["init", "-q"]);
    write(root, "tools/nested/inner.py", "x = 1\n");
    git(&root.join("tools/nested"), &["init", "-q"]);
    // Make secrets unreadable: if anything tried to read them, it would fail loudly below.
    for s in ["AuthKey_ABCDE12345.p8", "certificate.p12", ".env"] {
        fs::set_permissions(root.join(s), fs::Permissions::from_mode(0o000)).unwrap();
    }
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    (home, repo)
}

#[test]
fn no_secret_or_agent_review_file_is_ever_opened_or_indexed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    fixture(repo.path());
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();

    guard::trace_opens(true);
    let st = inventory::index_project(&mut c, &p).unwrap();
    let opened = guard::opened();
    guard::trace_opens(false);

    assert!(st.files_seen >= 3, "{st:?}");
    // 1. Nothing denied was opened.
    for rel in &opened {
        assert!(!foxdb::tiers::is_denied_path(rel), "opened denied path {rel}");
        let name = rel.rsplit('/').next().unwrap();
        assert!(!foxdb::tiers::is_secret_name(name), "opened secret {rel}");
        for bad in [".agent-review", ".DerivedData", ".claude/worktrees", ".server-work", "tools/nested"] {
            assert!(!rel.starts_with(bad), "opened {rel}");
        }
    }
    assert!(opened.iter().any(|r| r == "AGENTS.md"), "sanity: guard saw real reads: {opened:?}");

    // 2. No node for any secret file or any path inside a denied dir; one summary node per dir.
    let bad: i64 = c.query_row(
        "SELECT COUNT(*) FROM node WHERE kind != 'artifact_dir' AND (path LIKE '.agent-review/%' OR path LIKE '.DerivedData%' OR path LIKE '.claude/%'
           OR path LIKE '.server-work/%' OR path LIKE 'tools/nested/%' OR name LIKE 'AuthKey%' OR name LIKE '%.p12' OR name LIKE '.env%'
           OR name = 'Local.xcconfig' OR name LIKE 'id_ed25519%' OR name LIKE '%.mobileprovision')",
        [], |r| r.get(0)).unwrap();
    assert_eq!(bad, 0);
    let summaries: Vec<(String, i64)> = {
        let mut s = c.prepare("SELECT path, tier FROM node WHERE kind = 'artifact_dir' ORDER BY path").unwrap();
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().flatten().collect()
    };
    for (dir, tier) in
        [(".agent-review", 1), (".DerivedDataPhysicalRelease", 0), (".claude/worktrees", 0), (".server-work", 0), ("tools/nested", 0)]
    {
        assert!(summaries.iter().any(|(p, t)| p == dir && *t == tier), "missing summary {dir}: {summaries:?}");
    }
    let ar: String = c.query_row("SELECT meta FROM node WHERE kind = 'artifact_dir' AND path = '.agent-review'", [], |r| r.get(0)).unwrap();
    assert!(ar.contains("\"files\":3"), "{ar}");

    // 3. The secret token is not searchable anywhere.
    let hits: i64 = c.query_row("SELECT COUNT(*) FROM fts_node WHERE fts_node MATCH ?1", [SECRET_TOKEN], |r| r.get(0)).unwrap();
    assert_eq!(hits, 0);
    let out =
        query::find(&c, Some(&p), SECRET_TOKEN, &query::FindOpts { kind: "any", all: true, budget: 800, limit: 20, offset: 0 }).unwrap();
    assert!(out.starts_with("no matches"), "{out}");

    // 4. Guard refuses denied paths even if asked directly.
    assert!(guard::read_text(&repo.path().join(".agent-review/PXL-31/notes.md"), ".agent-review/PXL-31/notes.md", 1024, false).is_none());
    assert!(guard::read_text(&repo.path().join("Config/Local.xcconfig"), "Config/Local.xcconfig", 1024, false).is_none());

    // Restore permissions so tempdir cleanup works.
    for s in ["AuthKey_ABCDE12345.p8", "certificate.p12", ".env"] {
        let _ = fs::set_permissions(repo.path().join(s), fs::Permissions::from_mode(0o600));
    }
}

#[test]
fn milestones_find_brief_and_incremental() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    fixture(repo.path());
    let mut c = db::open_rw().unwrap();
    let loc = project::locate(repo.path());
    let p = project::ensure(&c, &loc).unwrap();
    let st = inventory::index_project(&mut c, &p).unwrap();
    assert_eq!(st.milestones, 3);
    let p = project::find(&c, &loc).unwrap().unwrap();

    let out = query::find(&c, Some(&p), "HomeShelfStore", &query::FindOpts { kind: "any", all: false, budget: 800, limit: 5, offset: 0 })
        .unwrap();
    assert!(out.contains("Sources/App/HomeShelfStore.swift"), "{out}");
    let out =
        query::find(&c, Some(&p), "ShelfSto", &query::FindOpts { kind: "file", all: false, budget: 800, limit: 5, offset: 0 }).unwrap();
    assert!(out.contains("HomeShelfStore.swift"), "trigram: {out}");
    let out = query::find(&c, Some(&p), "PERF2-P2.4", &query::FindOpts { kind: "milestone", all: false, budget: 800, limit: 5, offset: 0 })
        .unwrap();
    assert!(out.contains("IN_PROGRESS"), "{out}");

    let b = query::brief(&c, &p, Some("main"), 400).unwrap();
    assert!(b.len() <= 1600, "brief over 400 tokens: {}", b.len());
    assert!(b.contains("Gate: PERF2-P2.4 is active."), "{b}");
    assert!(b.contains("PERF2-P2.4"), "{b}");
    assert!(!b.contains("PERF2-P2.3 thing"), "approved milestone listed as open: {b}");
    let tiny = query::brief(&c, &p, Some("main"), 40).unwrap();
    assert!(tiny.len() <= 40 * 4 + 40, "budget not enforced: {}", tiny.len());

    // Incremental: unchanged -> 0 changed; edit -> 1 changed; delete -> removed.
    assert_eq!(inventory::index_project(&mut c, &p).unwrap().files_changed, 0);
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(repo.path(), "Sources/App/HomeShelfStore.swift", "struct HomeShelfStore { var x = 1 }\n");
    write(repo.path(), "Sources/App/NewThing.swift", "struct NewThing {}\n");
    let st = inventory::index_project(&mut c, &p).unwrap();
    assert_eq!(st.files_changed, 2, "{st:?}");
    fs::remove_file(repo.path().join("Sources/App/NewThing.swift")).unwrap();
    assert_eq!(inventory::index_project(&mut c, &p).unwrap().files_removed, 1);
    for s in ["AuthKey_ABCDE12345.p8", "certificate.p12", ".env"] {
        let _ = fs::set_permissions(repo.path().join(s), fs::Permissions::from_mode(0o600));
    }
}

#[test]
fn worktrees_map_to_main_project_and_home_is_name_only() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_home, repo) = setup();
    write(repo.path(), "a.swift", "struct A {}\n");
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "i"]);
    let wt = tempfile::tempdir().unwrap();
    let wtp = wt.path().join("wt");
    git(repo.path(), &["worktree", "add", "-q", wtp.to_str().unwrap()]);
    let main = project::locate(repo.path());
    let sub = project::locate(&wtp);
    assert_eq!(main.common_dir, sub.common_dir);
    assert_eq!(main.root, sub.root);
    assert!(project::branch(sub.git_dir.as_ref().unwrap()).is_some());

    let c = db::open_rw().unwrap();
    let a = project::ensure(&c, &main).unwrap();
    let b = project::ensure(&c, &sub).unwrap();
    assert_eq!(a.id, b.id);

    // Non-git dir registers by directory; $HOME is "huge" (name-only).
    let plain = tempfile::tempdir().unwrap();
    assert_eq!(project::locate(plain.path()).kind, "dir");
    let home = std::env::var("HOME").unwrap();
    assert_eq!(project::locate(Path::new(&home)).kind, "huge");
}

#[test]
fn milestone_parser_unit_shape() {
    let s = markdown::split("## Current gate\nx\n## M6 thing\nState: BLOCKED\n", Some(2));
    assert_eq!(s.len(), 2);
    assert_eq!(markdown::parse_milestone(&s[1]).state.as_deref(), Some("BLOCKED"));
}

/// Milestone 7: a tree that keeps a walk record (forced small here) ends every run with the same
/// files, sections and symbols as a store that walks in full, through edits in place, new and
/// removed files, renames, new directories and ignore-file changes.
#[test]
fn walk_record_runs_match_full_walk_runs() {
    use foxdb::{search, tsindex};
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let repo = tempfile::tempdir().unwrap();
    let r = repo.path();
    let homes = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let run = |i: usize| -> (Vec<String>, bool) {
        unsafe {
            std::env::set_var("FOXDB_HOME", homes[i].path());
            std::env::set_var("FOXDB_CLAUDE_PROJECTS", homes[i].path().join("no-claude"));
            std::env::set_var("FOXDB_WALK_MIN_FILES", if i == 0 { "1" } else { "100000000" });
        }
        let mut c = db::open_rw().unwrap();
        let p = project::ensure(&c, &project::locate(r)).unwrap();
        let st = inventory::index_project(&mut c, &p).unwrap();
        let quick = st.delta.is_some();
        tsindex::index_project_with(&mut c, &p, st.delta.as_ref()).unwrap();
        let mut q = c
            .prepare("SELECT kind || ' ' || COALESCE(path, '') || ' ' || name || ' ' || COALESCE(size, -1) FROM node WHERE project_id = ?1 AND kind != 'artifact_dir' ORDER BY 1")
            .unwrap();
        let mut rows: Vec<String> = q.query_map([p.id], |x| x.get(0)).unwrap().flatten().collect();
        let o = query::FindOpts { kind: "any", all: false, budget: usize::MAX, limit: 5, offset: 0 };
        rows.extend(search::hits(&c, Some(&p), "freshSymbol", &o, 5).unwrap().into_iter().map(|h| format!("hit {}", h.name)));
        unsafe { std::env::remove_var("FOXDB_WALK_MIN_FILES") };
        (rows, quick)
    };
    // `quick`: the recorded store must have skipped reading every file row.
    let both = |label: &str, quick: bool| {
        let ((a, qa), (b, qb)) = (run(0), run(1));
        assert_eq!(a, b, "{label}");
        assert!(!qb, "{label}: a small tree never uses a record");
        assert_eq!(qa, quick, "{label}");
    };
    write(r, ".gitignore", "*.o\n");
    for i in 0..30 {
        write(r, &format!("src/m{}/f{i}.rs", i % 4), &format!("pub fn f{i}() {{}}\n"));
    }
    write(r, "docs/guide.md", "# Guide\n\n## Start\n\ntext\n");
    both("first run", false);
    assert!(homes[0].path().join("walk").read_dir().unwrap().next().is_some(), "the record is written");
    // Directory times must be older than the record's two-second window to be trusted.
    std::thread::sleep(std::time::Duration::from_millis(2100));
    both("no change (record too fresh to trust)", false);
    both("no change", true);
    write(r, "src/m1/f1.rs", "pub fn freshSymbol() {}\n");
    both("a file edited in place", true);
    write(r, "src/m2/new.rs", "pub fn added() {}\n");
    fs::rename(r.join("src/m3/f3.rs"), r.join("src/m3/moved.rs")).unwrap();
    fs::remove_file(r.join("src/m0/f0.rs")).unwrap();
    write(r, "src/m0/sub/deep.rs", "pub fn deep() {}\n");
    write(r, "src/m0/sub/skip.o", "x");
    both("added, renamed, removed, a new directory", true);
    std::thread::sleep(std::time::Duration::from_millis(2100));
    write(r, "src/m2/.gitignore", "f6.rs\n");
    both("a new ignore file", true);
    write(r, "docs/guide.md", "# Guide\n\n## Start\n\ntext\n\n## More\n\nmore text\n");
    both("a markdown file edited", true);
}
