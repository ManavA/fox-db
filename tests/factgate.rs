//! The opt-in PreToolUse fact gate (`[hooks] fact_gate`): before an Edit, Write or Bash call
//! runs, a one-line correction when the index proves that a project symbol or path it cites does
//! not exist. Silent on anything it cannot prove: std and external names, names the edit defines,
//! new files, languages foxdb does not index, and files changed since indexing.

use foxdb::config::{self, Config};
use foxdb::{db, hook, inventory, project, tsindex};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

const DB_RS: &str = "pub fn open_ro() -> u32 {\n    1\n}\n\npub fn open_rw() -> u32 {\n    2\n}\n\npub struct Store;\npub mod nested;\n";
const UTIL_RS: &str = "use std::collections::HashMap;\n\npub fn helper() -> u32 {\n    let _m: HashMap<u32, u32> = HashMap::new();\n    crate::db::open_ro()\n}\n";
const CART_PY: &str = "def checkout_cart(cart):\n    return cart\n\n\ndef reorder_cart(cart):\n    return checkout_cart(cart)\n";

/// A scratch store and an indexed repo: a Rust crate `shopkit` and a Python package `shop`.
fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    let r = repo.path();
    fs::create_dir_all(r.join(".git")).unwrap();
    fs::write(r.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    write(r, "Cargo.toml", "[package]\nname = \"shopkit\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(r, "src/lib.rs", "pub mod db;\npub mod gen;\npub mod util;\n");
    write(r, "src/db.rs", DB_RS);
    write(r, "src/util.rs", UTIL_RS);
    write(r, "src/gen.rs", "make_fns!(alpha);\n\npub fn base() -> u32 {\n    0\n}\n");
    write(r, "src/db/nested.rs", "pub fn a() {}\npub fn opne_ro() {}\npub fn b() {}\n");
    write(r, "tests/t.rs", "#[test]\nfn t() {}\n");
    write(r, "shop/__init__.py", "");
    write(r, "shop/cart.py", CART_PY);
    write(r, "shop/views.py", "from shop.cart import checkout_cart\n\n\ndef view(c):\n    return checkout_cart(c)\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(r)).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(r)).unwrap().unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    (home, repo)
}

fn on() -> Config {
    let cfg = config::parse("[hooks]\nfact_gate = true\n");
    assert!(cfg.hooks.fact_gate);
    cfg
}

fn run(cfg: &Config, cwd: &Path, tool: &str, input: Value) -> String {
    let v = json!({
        "hook_event_name": "PreToolUse", "session_id": "s1", "cwd": cwd, "tool_name": tool,
        "tool_input": input,
    });
    let out = hook::run_with(cfg, "pre-tool-use", &v.to_string(), hook::Harness::Claude);
    if out.is_empty() {
        return out;
    }
    let v: Value = serde_json::from_str(&out).unwrap();
    let h = v["hookSpecificOutput"].as_object().unwrap();
    let mut keys: Vec<&String> = h.keys().collect();
    keys.sort();
    assert_eq!(keys, ["additionalContext", "hookEventName"], "the gate only adds context, never blocks: {out}");
    assert_eq!(h["hookEventName"], "PreToolUse");
    h["additionalContext"].as_str().unwrap().to_string()
}

fn edit(cwd: &Path, file: &Path, old: &str, new: &str) -> String {
    run(&on(), cwd, "Edit", json!({"file_path": file, "old_string": old, "new_string": new}))
}

fn bash(cwd: &Path, command: &str) -> String {
    run(&on(), cwd, "Bash", json!({"command": command, "description": "x"}))
}

#[test]
fn misspelt_project_fn_gets_a_correction_with_a_suggestion() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let out = edit(r, &r.join("src/util.rs"), "crate::db::open_ro()", "crate::db::open_db_ro()");
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(out.starts_with("foxdb: `crate::db::open_db_ro` does not exist"), "{out}");
    assert!(out.contains("src/db.rs"), "{out}");
    assert!(out.contains("did you mean `crate::db::open_ro` (src/db.rs:1)"), "{out}");
    // The same name through a `use` group, and through the crate's own name from outside src/.
    let out = edit(r, &r.join("src/util.rs"), "use std::collections::HashMap;", "use crate::db::{open_rw, open_db_ro};");
    assert!(out.contains("`crate::db::open_db_ro` does not exist"), "{out}");
    assert!(!out.contains("open_rw`"), "{out}");
    let out =
        run(&on(), r, "Write", json!({"file_path": r.join("tests/t.rs"), "content": "use shopkit::db::opne_ro;\n#[test]\nfn t() {}\n"}));
    assert!(out.contains("`shopkit::db::opne_ro` does not exist") && out.contains("did you mean `shopkit::db::open_ro`"), "{out}");
    // Python: a misspelt name imported from a project module.
    let out = edit(r, &r.join("shop/views.py"), "from shop.cart import checkout_cart", "from shop.cart import checkout_kart");
    assert!(out.contains("`checkout_kart`") && out.contains("shop/cart.py") && out.contains("did you mean `checkout_cart`"), "{out}");
    let out = edit(r, &r.join("shop/views.py"), "from shop.cart import checkout_cart", "from .cart import checkout_kart");
    assert!(out.contains("did you mean `checkout_cart`"), "{out}");
}

#[test]
fn moved_name_points_at_its_module() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let out = edit(r, &r.join("src/db.rs"), "pub struct Store;", "pub fn h() -> u32 { crate::util::open_rw() }");
    assert!(
        out.contains("`crate::util::open_rw` does not exist") && out.contains("did you mean `crate::db::open_rw` (src/db.rs:5)"),
        "{out}"
    );
}

#[test]
fn std_and_external_names_are_silent() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let new = "std::fs::read_to_string(\"x\").unwrap();\n    serde_json::from_str::<u32>(\"1\").unwrap();\n    \
               HashMap::<u32, u32>::new();\n    db::open_db_ro();\n    open_db_ro();\n    x.open_db_ro();\n    crate::db::open_ro()";
    assert_eq!(edit(r, &r.join("src/util.rs"), "crate::db::open_ro()", new), "");
    let out =
        edit(r, &r.join("shop/views.py"), "from shop.cart import checkout_cart", "from os.path import joinx\nfrom requests import gett");
    assert_eq!(out, "");
    let out = edit(
        r,
        &r.join("shop/views.py"),
        "from shop.cart import checkout_cart",
        "from shop.cart import (checkout_cart, # typo example, checkout_kart, discarded\n)",
    );
    assert_eq!(out, "");
    // Associated items, modules and macros are not judged; a comment or string is not a citation.
    let new = "crate::db::Store::open_db_ro();\n    crate::db::open_ro()\n    // crate::db::open_db_ro\n    \"crate::db::open_db_ro\";";
    assert_eq!(edit(r, &r.join("src/util.rs"), "crate::db::open_ro()", new), "");
    assert_eq!(edit(r, &r.join("src/util.rs"), "crate::db::open_ro()", "crate::db!()"), "");
    let out = edit(r, &r.join("src/util.rs"), "use std::collections::HashMap;", "use crate::db::{nested::{a, opne_ro, b}, open_rw};");
    assert_eq!(out, "");
}

#[test]
fn names_defined_in_the_same_edit_are_silent() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let out = run(
        &on(),
        r,
        "Write",
        json!({
            "file_path": r.join("src/db.rs"),
            "content": "pub fn g() -> u32 { crate::db::open_db_ro() }\n"
        }),
    );
    assert_eq!(out, "", "the proposed write removes the suggested definitions");
    let new = "pub fn open_db_ro() -> u32 {\n    3\n}\n\npub fn g() -> u32 {\n    crate::db::open_db_ro()\n}\n\npub struct Store;";
    assert_eq!(edit(r, &r.join("src/db.rs"), "pub struct Store;", new), "");
    let new = "pub use self::open_ro as open_db_ro;\npub fn g() -> u32 { crate::db::open_db_ro() }";
    assert_eq!(edit(r, &r.join("src/db.rs"), "pub struct Store;", new), "");
    let new = "fn open_db_ro() -> u32 { 4 }\n    crate::db::open_db_ro()";
    assert_eq!(edit(r, &r.join("src/util.rs"), "crate::db::open_ro()", new), "");
    let out = edit(
        r,
        &r.join("shop/views.py"),
        "def view(c):",
        "def checkout_kart(c):\n    pass\n\n\nfrom shop.cart import checkout_kart\n\n\ndef view(c):",
    );
    assert_eq!(out, "");
}

#[test]
fn new_files_and_unindexed_modules_are_silent() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let out =
        run(&on(), r, "Write", json!({"file_path": r.join("src/fresh.rs"), "content": "pub fn f() -> u32 { crate::db::open_db_ro() }\n"}));
    assert_eq!(out, "");
    write(r, "src/fresh.rs", "pub fn f() -> u32 { 1 }\n");
    assert_eq!(edit(r, &r.join("src/util.rs"), "crate::db::open_ro()", "crate::fresh::g()"), "");
    // A module that may generate its items (an item-position macro) proves nothing.
    assert_eq!(edit(r, &r.join("src/util.rs"), "crate::db::open_ro()", "crate::gen::open_db_ro()"), "");
}

#[test]
fn unindexed_languages_are_silent() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    write(r, "src/x.zig", "const a = 1;\n");
    write(r, "notes.txt", "x\n");
    assert_eq!(edit(r, &r.join("src/x.zig"), "const a = 1;", "crate::db::open_db_ro()"), "");
    assert_eq!(edit(r, &r.join("notes.txt"), "x", "crate::db::open_db_ro()"), "");
    // A grep whose scope holds code foxdb does not index cannot be vouched for.
    write(r, "src/x.zig", "fn open_db_ro() void {}\n");
    assert_eq!(bash(r, "grep -rn open_db_ro src"), "");
}

#[test]
fn stale_index_is_silent() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    write(r, "tests/t.rs", "#[test]\nfn t() { shopkit::db::open_ro(); }\n");
    let out = run(
        &on(),
        r,
        "Write",
        json!({
            "file_path": r.join("tests/t.rs"),
            "content": "use shopkit::db::opne_ro;\n#[test]\nfn t() {}\n"
        }),
    );
    assert_eq!(out, "");
    write(r, "src/db.rs", &format!("{DB_RS}\npub fn open_db_ro() -> u32 {{\n    3\n}}\n"));
    assert_eq!(edit(r, &r.join("src/util.rs"), "crate::db::open_ro()", "crate::db::open_db_ro()"), "");
    assert_eq!(bash(r, "grep -rn open_db_ro src"), "");
    write(r, "shop/cart.py", &format!("{CART_PY}\n\ndef checkout_kart(c):\n    return c\n"));
    assert_eq!(edit(r, &r.join("shop/views.py"), "from shop.cart import checkout_cart", "from shop.cart import checkout_kart"), "");
}

#[test]
fn bash_paths_and_grep_targets() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let out = bash(r, "cat src/dbs.rs");
    assert!(out.contains("`src/dbs.rs` does not exist") && out.contains("did you mean `src/db.rs`"), "{out}");
    let out = bash(&r.join("src"), "head -n 20 utils.rs | tail -5");
    assert!(out.contains("did you mean `src/util.rs`"), "{out}");
    let out = bash(r, "cd ./src && sed -n '1,5p' dbs.rs");
    assert!(out.contains("did you mean `src/db.rs`"), "{out}");
    assert_eq!(bash(r, "cat src/db.rs src/util.rs"), "");
    assert_eq!(bash(r, "cat src/nothing_like_it.rs"), "", "no suggestion, nothing to add");
    assert_eq!(bash(r, "touch src/dbs.rs && cat src/dbs.rs"), "", "an earlier command may create it");
    assert_eq!(bash(r, "cat /etc/nonexistent_dir/x.rs"), "", "outside the project");
    let out = bash(r, "grep -rn open_db_ro src");
    assert!(out.contains("`open_db_ro`") && out.contains("did you mean `open_ro` (src/db.rs:1)"), "{out}");
    let out = bash(r, "rg -n 'checkout_kart\\(' shop");
    assert!(out.contains("did you mean `checkout_cart`"), "{out}");
    assert_eq!(bash(r, "grep -rn open_ro src"), "");
    assert_eq!(bash(r, "grep -rni open_db_ro src"), "", "case-insensitive: not judged");
    assert_eq!(bash(r, "grep -rn 'open_db_ro|open_ro' src"), "");
    assert_eq!(bash(r, "grep -rn HashMapz src"), "", "no near name: nothing to add");
}

#[test]
fn off_by_default() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let v = json!({"hook_event_name": "PreToolUse", "cwd": r, "tool_name": "Edit",
        "tool_input": {"file_path": r.join("src/util.rs"), "old_string": "crate::db::open_ro()", "new_string": "crate::db::open_db_ro()"}});
    assert_eq!(hook::run_with(&Config::default(), "pre-tool-use", &v.to_string(), hook::Harness::Claude), "");
}

#[test]
fn relative_import_with_overridden_package_is_silent() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    write(r, "other/__init__.py", "");
    write(r, "other/cart.py", "def checkout_kart(cart):\n    return cart\n");
    let new = "__package__ = 'other'\nfrom .cart import checkout_kart";
    assert_eq!(edit(r, &r.join("shop/views.py"), "from shop.cart import checkout_cart", new), "");
}

#[test]
fn rg_does_not_suggest_ignored_definitions() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let command = "rg -n 'checkout_kart\\(' shop";
    assert!(bash(r, command).contains("did you mean `checkout_cart`"));
    write(r, ".rgignore", "shop/cart.py\n");
    assert_eq!(bash(r, command), "");
}

#[test]
fn fact_gate_leaves_an_old_store_for_the_next_writer() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    assert!(bash(repo.path(), "cat src/dbs.rs").contains("did you mean `src/db.rs`"));
    let c = db::open_rw().unwrap();
    c.pragma_update(None, "user_version", db::LATEST - 1).unwrap();
    assert_eq!(bash(repo.path(), "cat src/dbs.rs"), "");
    let version: i64 = c.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap();
    assert_eq!(version, db::LATEST - 1);
}

#[cfg(target_os = "linux")]
#[test]
fn replace_all_is_bounded_before_allocating() {
    if let Some(repo) = std::env::var_os("FOXDB_FACT_GATE_LIMIT_TEST_REPO") {
        let r = Path::new(&repo);
        assert!(bash(r, "cat src/dbs.rs").contains("did you mean `src/db.rs`"));
        let input = json!({
            "file_path": r.join("src/util.rs"), "old_string": "x",
            "new_string": "y".repeat(64 * 1024), "replace_all": true
        });
        assert_eq!(run(&on(), r, "Edit", input), "");
        return;
    }

    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    write(r, "src/util.rs", &format!("// {}\n{UTIL_RS}", "x".repeat(230_000)));
    let mut c = db::open_rw().unwrap();
    let p = project::find(&c, &project::locate(r)).unwrap().unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    let status = std::process::Command::new("bash")
        .args(["-c", "ulimit -v 524288 && exec \"$@\"", "bash"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "replace_all_is_bounded_before_allocating"])
        .env("FOXDB_FACT_GATE_LIMIT_TEST_REPO", r)
        .output()
        .unwrap();
    assert!(status.status.success(), "{}{}", String::from_utf8_lossy(&status.stdout), String::from_utf8_lossy(&status.stderr));
}
