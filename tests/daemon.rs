//! `foxd`: the long-lived daemon answers the CLI's read verbs over a unix socket with the same
//! bytes as the direct path, and the client falls back cleanly when no daemon is running.

use foxdb::{cli, daemon, db, inventory, project, tsindex};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn argv(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| x.to_string()).collect()
}

fn strip_notes(s: &str) -> String {
    s.lines().filter(|l| !l.starts_with("~ fox saved")).collect::<Vec<_>>().join("\n")
}

#[test]
fn daemon_answers_like_the_direct_path_and_falls_back_when_absent() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    write(
        repo.path(),
        "src/widget.rs",
        "pub struct Widget;\n\npub fn make_widget() -> Widget {\n    Widget\n}\n\npub fn use_it() {\n    make_widget();\n}\n",
    );
    // `parse_image_names` holds "page_n" as a subsequence but not as a prefix or substring.
    write(repo.path(), "src/pager.rs", "pub fn page_n() {}\n\npub fn parse_image_names() {}\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();

    // No daemon: the client says so at once and the CLI goes direct.
    assert!(!daemon::ping());
    assert!(daemon::call(&argv(&["find", "Widget"]), repo.path()).is_none());

    let server = std::thread::spawn(|| daemon::serve().unwrap());
    for _ in 0..200 {
        if daemon::ping() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(daemon::ping(), "daemon did not come up");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(daemon::socket_path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be private");
    }

    for a in [
        vec!["find", "Widget"],
        vec!["find", "widget", "--kind", "file"],
        vec!["code", "make_widget", "--rel", "callers"],
        vec!["find", "page_n"],
    ] {
        let via = daemon::call(&argv(&a), repo.path()).expect("daemon reply");
        let args = cli::parse(argv(&a)).unwrap();
        let direct = if a[0] == "find" { cli::find(&args, repo.path(), None) } else { cli::code(&args, repo.path(), None) }.unwrap();
        assert_eq!(via.code, 0, "{a:?}: {}", via.text);
        assert_eq!(strip_notes(&via.text), strip_notes(&direct), "{a:?}");
    }
    // Verbs the daemon does not serve go direct.
    assert!(daemon::call(&argv(&["index", "."]), repo.path()).is_none());
    // An index change is visible to the next answer (generation moved, cache dropped).
    write(repo.path(), "src/gadget.rs", "pub struct Gadget;\n");
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let via = daemon::call(&argv(&["find", "Gadget"]), repo.path()).unwrap();
    assert!(via.text.contains("src/gadget.rs:1"), "{}", via.text);

    daemon::shutdown();
    server.join().unwrap();
    assert!(!daemon::ping());
    assert!(!daemon::socket_path().exists(), "socket left behind");
}

/// "Already sent" is per client session: the daemon uses the session the client names in its
/// request, never its own environment.
#[test]
fn daemon_dedups_by_the_client_session_not_its_own() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
        std::env::remove_var("FOXDB_SESSION");
    }
    let body: String = (0..60).map(|i| format!("    let value_{i} = compute_something({i});\n")).collect();
    write(repo.path(), "src/lib.rs", &format!("pub fn big_function() {{\n{body}}}\n"));
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();

    let server = std::thread::spawn(|| daemon::serve().unwrap());
    for _ in 0..200 {
        if daemon::ping() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let a = argv(&["code", "big_function", "--rel", "src", "--budget", "2000"]);
    let first = daemon::call_with(&a, repo.path(), Some("s1")).expect("daemon reply").text;
    assert!(first.contains("value_59"), "{first}");
    let again = daemon::call_with(&a, repo.path(), Some("s1")).unwrap().text;
    assert!(again.starts_with("= already sent"), "same session, repeat read: {again}");
    let other = daemon::call_with(&a, repo.path(), Some("s2")).unwrap().text;
    assert!(other.contains("value_59"), "a different session gets the full text: {other}");
    let none = daemon::call_with(&a, repo.path(), None).unwrap().text;
    assert!(none.contains("value_59"), "no session, no dedup: {none}");
    daemon::shutdown();
    server.join().unwrap();
}

/// Graph answers (`fox code --rel callers` and friends) are cached in the warm state like `find`,
/// keyed by the index generation, so a changed index is never answered from the cache.
#[test]
fn warm_code_answers_are_cached_by_generation() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    write(repo.path(), "src/widget.rs", "pub fn make_widget() {}\n\npub fn use_it() {\n    make_widget();\n}\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let ro = db::open_ro().unwrap();
    let mut warm = foxdb::search::Warm::default();
    let a = cli::parse(argv(&["code", "make_widget", "--rel", "callers"])).unwrap();
    let ask =
        |warm: &mut foxdb::search::Warm| strip_notes(&cli::code(&a, repo.path(), Some(cli::Served { ro: &ro, rw: None, warm })).unwrap());
    let first = ask(&mut warm);
    let hits = warm.cache_hits();
    assert_eq!(ask(&mut warm), first);
    assert_eq!(warm.cache_hits(), hits + 1, "repeat graph answer not served from the cache");
    write(repo.path(), "src/more.rs", "pub fn another_caller() {\n    make_widget();\n}\n");
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let after = ask(&mut warm);
    assert!(after.contains("src/more.rs") || after.contains("another_caller"), "stale cached answer: {after}");
    assert_eq!(warm.cache_hits(), hits + 1);
}

/// Writes never merge full-text segments inline; an idle daemon merges them.
#[test]
fn an_idle_daemon_merges_full_text_segments() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
        std::env::set_var("FOXDB_IDLE_SECS", "1");
    }
    let c = db::open_rw().unwrap();
    let _ = repo;
    // Twelve separate writes leave twelve segments (index runs also merge a bounded step).
    for i in 0..12 {
        c.execute("INSERT INTO fts_node(rowid, name, sig, path, body) VALUES (?1, 'n', '', 'p', '')", [1000 + i]).unwrap();
    }
    let before = db::fts_segments(&c, "fts_node");
    assert!(before >= 4, "{before}");
    let server = std::thread::spawn(|| daemon::serve().unwrap());
    let mut after = before;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        after = db::fts_segments(&c, "fts_node");
        if after < before {
            break;
        }
    }
    daemon::shutdown();
    server.join().unwrap();
    unsafe { std::env::remove_var("FOXDB_IDLE_SECS") };
    assert!(after < before, "segments {before} -> {after}");
}
