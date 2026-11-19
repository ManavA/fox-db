//! `fox mem check`: memory lines that cite code which no longer exists. Precision over recall:
//! only code-shaped tokens are checked, and only those that resolve to nothing are flagged.

use foxdb::{db, hook, inventory, mcp, memcheck, project, query, tsindex};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

const CLAUDE_MD: &str = "# Notes

- Call `live_function` from `src/lib.rs:1`.
- `removed_function` in `src/lib.rs` handles retries.
- The parser moved; see src/gone.rs for details.
- Run `demo build --verbose` or `--dry-run`.
- `Widget.render` draws; `Widget.paint` was the old name.
- Use `tokio::spawn` for tasks, and `cargo test --no-fail-fast`.
- The `main` loop and the `Seen` cache; names are `snake_case`.
- Paths like `path/to/file.rs` are examples; `~/elsewhere/x.rs` is another repo.
- Set `DEMO_LOG_LEVEL`, not `RUST_LOG`.
- codesign fails with `errSecInternalComponent`; deploy with `--set-env-vars`.
- There is no `src/old_parser.rs` any more; `Widget.render` replaced `paint_all`.
- Search lives in `src/search.Searcher`; ship `com.example.app`; see `setup.py`.

```sh
ghost_fn --phantom
```
";

struct Fx {
    _home: tempfile::TempDir,
    repo: tempfile::TempDir,
    c: rusqlite::Connection,
    p: project::Project,
    claude_note: std::path::PathBuf,
}

fn setup() -> Fx {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let claude = home.path().join("claude-projects");
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", &claude);
    }
    write(repo.path(), "Cargo.toml", "[package]\nname = \"demo\"\n\n[[bin]]\nname = \"demo\"\npath = \"src/main.rs\"\n");
    write(repo.path(), "src/lib.rs", "pub fn live_function() {}\npub struct Widget;\nimpl Widget {\n    pub fn render(&self) {}\n}\n");
    write(
        repo.path(),
        "src/main.rs",
        "fn main() {\n    let v = std::env::var(\"DEMO_LOG_LEVEL\");\n    if std::env::args().any(|a| a == \"--verbose\") {}\n}\n",
    );
    write(repo.path(), "CLAUDE.md", CLAUDE_MD);
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(repo.path())).unwrap().unwrap();
    let claude_note = claude.join(memcheck::claude_dir_name(&p.root)).join("memory/helpers.md");
    write(claude_note.parent().unwrap(), "helpers.md", "---\nname: helpers\n---\n`old_helper_fn` lives in `src/lib.rs`.\n");
    mcp::save_note(&c, p.id, "Decided: `stale_note_fn` replaces `live_function`.").unwrap();
    Fx { _home: home, repo, c, p, claude_note }
}

fn flagged(out: &str) -> Vec<String> {
    out.lines().filter(|l| l.contains(" | ")).map(String::from).collect()
}

#[test]
fn flags_only_code_shaped_tokens_that_resolve_to_nothing() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = setup();
    let before = (fs::read(fx.repo.path().join("CLAUDE.md")).unwrap(), fs::read(&fx.claude_note).unwrap());
    let out = memcheck::render(&memcheck::check(&fx.c, &fx.p).unwrap());
    let rows = flagged(&out);
    let row = |needle: &str| rows.iter().find(|r| r.contains(needle)).cloned().unwrap_or_else(|| panic!("no row for {needle}:\n{out}"));
    assert!(row("`removed_function`").starts_with("CLAUDE.md:4 | - `removed_function` in `src/lib.rs` handles retries. | "), "{out}");
    assert!(row("src/gone.rs").starts_with("CLAUDE.md:5 | "), "{out}");
    assert!(row("`--dry-run`").starts_with("CLAUDE.md:6 | "), "{out}");
    assert!(row("`Widget.paint`").starts_with("CLAUDE.md:7 | "), "{out}");
    assert!(row("`old_helper_fn`").starts_with(&format!("{}:4 | ", memcheck::display_path(&fx.claude_note))), "{out}");
    assert!(row("`stale_note_fn`").contains(":1 | Decided:"), "{out}");
    // What exists, what is foreign and what is prose stays quiet.
    for quiet in [
        "live_function`",
        "--verbose",
        "Widget.render",
        "tokio",
        "--no-fail-fast",
        "`main`",
        "Seen",
        "snake_case",
        "path/to/file.rs",
        "elsewhere",
        "DEMO_LOG_LEVEL",
        "RUST_LOG",
        "ghost_fn",
        "phantom",
        "errSecInternalComponent",
        "set-env-vars",
        "old_parser",
        "paint_all",
        "Searcher",
        "com.example",
        "setup.py",
    ] {
        assert!(rows.iter().all(|r| !r.split(" | ").nth(2).unwrap_or("").contains(quiet)), "{quiet} flagged:\n{out}");
    }
    assert_eq!(rows.len(), 6, "{out}");
    // Memory files are read, never written.
    assert_eq!(before, (fs::read(fx.repo.path().join("CLAUDE.md")).unwrap(), fs::read(&fx.claude_note).unwrap()));
}

#[test]
fn the_brief_names_the_count_only_when_something_is_stale() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = setup();
    let line = |c: &rusqlite::Connection, p: &project::Project| {
        query::brief(c, p, Some("main"), 400).unwrap().lines().find(|l| l.contains("fox mem check")).map(String::from)
    };
    assert_eq!(line(&fx.c, &fx.p), None, "nothing recorded yet");
    memcheck::record(&fx.c, &fx.p).unwrap();
    let l = line(&fx.c, &fx.p).expect("stale count in the brief");
    assert_eq!(l, "stale memory: 6 lines cite code that is gone (fox mem check)");
    let a = query::brief(&fx.c, &fx.p, Some("main"), 400).unwrap();
    memcheck::record(&fx.c, &fx.p).unwrap();
    assert_eq!(a, query::brief(&fx.c, &fx.p, Some("main"), 400).unwrap(), "a second check changes no byte");
    // Subagents never get it.
    let input = serde_json::json!({"hook_event_name": "SubagentStart", "cwd": fx.repo.path(), "session_id": "s1",
        "agent_id": "a1", "agent_type": "helper"})
    .to_string();
    assert!(!hook::run("subagent-start", &input, hook::Harness::Claude).contains("fox mem check"));
    // Fixed memory: silent again.
    write(fx.repo.path(), "CLAUDE.md", "# Notes\n\n- Call `live_function`.\n");
    fs::remove_file(&fx.claude_note).unwrap();
    fx.c.execute("DELETE FROM node WHERE project_id = ?1 AND kind = 'memory'", [fx.p.id]).unwrap();
    memcheck::record(&fx.c, &fx.p).unwrap();
    assert_eq!(line(&fx.c, &fx.p), None);
    assert!(memcheck::render(&memcheck::check(&fx.c, &fx.p).unwrap()).starts_with("no stale code references"));
}

#[test]
fn mcp_and_cli_run_the_same_check() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = setup();
    std::env::set_current_dir(fx.repo.path()).unwrap();
    let r = mcp::handle(&serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"fox_memory","arguments":{"op":"check","budget":4000}}}))
    .unwrap();
    let text = r["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let a = foxdb::cli::parse(vec!["mem".into(), "check".into()]).unwrap();
    assert_eq!(text, foxdb::cli::mem_check(&a, fx.repo.path()).unwrap());
    assert_eq!(flagged(&text).len(), 6, "{text}");
}
