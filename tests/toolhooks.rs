//! Opt-in tool hooks: `post-tool-use` names the enclosing symbol of each grep hit, and
//! `user-prompt-submit` answers an identifier or path the prompt names that resolves to exactly
//! one indexed definition. Both are silent unless `[hooks]` in config.toml turns them on, and
//! silent on anything they cannot attribute.

use foxdb::config::{self, Config};
use foxdb::{db, hook, inventory, project, tsindex};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hookrepo")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()));
        } else {
            fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

/// A scratch store and an indexed copy of `tests/fixtures/hookrepo` (a git repo by its `.git/HEAD`).
fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    copy_dir(&fixture(), repo.path());
    fs::create_dir_all(repo.path().join(".git")).unwrap();
    fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    index(repo.path());
    (home, repo)
}

fn index(repo: &Path) {
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo)).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let p = project::find(&c, &project::locate(repo)).unwrap().unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
}

fn on() -> Config {
    config::parse("[hooks]\ngrep_context = true\nprompt_context = true\n")
}

/// What `grep -rn <needle> <dir>` prints when run from `root`: `dir/rel:line:text`, sorted.
fn grep(root: &Path, dir: &str, needle: &str) -> String {
    fn walk(base: &Path, d: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_path_buf());
            }
        }
    }
    let base = root.join(dir);
    let mut files = vec![];
    walk(&base, &base, &mut files);
    files.sort();
    let mut s = String::new();
    for f in files {
        let text = fs::read_to_string(base.join(&f)).unwrap();
        for (i, l) in text.lines().enumerate() {
            if l.contains(needle) {
                let shown = if dir == "." { format!("./{}", f.display()) } else { format!("{dir}/{}", f.display()) };
                s.push_str(&format!("{shown}:{}:{l}\n", i + 1));
            }
        }
    }
    s
}

fn line_of(root: &Path, rel: &str, needle: &str) -> usize {
    fs::read_to_string(root.join(rel)).unwrap().lines().position(|l| l.contains(needle)).unwrap() + 1
}

fn post(cfg: &Config, cwd: &Path, command: &str, stdout: &str) -> String {
    let input = json!({
        "hook_event_name": "PostToolUse", "session_id": "s1", "cwd": cwd, "tool_name": "Bash",
        "tool_input": {"command": command, "description": "search"},
        "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false},
    });
    hook::run_with(cfg, "post-tool-use", &input.to_string(), hook::Harness::Claude)
}

fn prompt(cfg: &Config, cwd: &Path, text: &str) -> String {
    let input = json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "cwd": cwd, "prompt": text});
    hook::run_with(cfg, "user-prompt-submit", &input.to_string(), hook::Harness::Claude)
}

/// The injected text, after checking the output carries nothing but `additionalContext` for `event`.
fn ctx(out: &str, event: &str) -> String {
    if out.is_empty() {
        return String::new();
    }
    let v: Value = serde_json::from_str(out).unwrap();
    let o = v.as_object().unwrap();
    assert_eq!(o.keys().collect::<Vec<_>>(), ["hookSpecificOutput"], "only hookSpecificOutput: {out}");
    let h = v["hookSpecificOutput"].as_object().unwrap();
    let mut keys: Vec<&String> = h.keys().collect();
    keys.sort();
    assert_eq!(keys, ["additionalContext", "hookEventName"], "tool output must never change: {out}");
    assert_eq!(h["hookEventName"], event);
    h["additionalContext"].as_str().unwrap().to_string()
}

// ---- PostToolUse on Bash grep ----

#[test]
fn grep_hits_name_their_enclosing_symbol_and_handle() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let out = grep(r, "shop", "TOKEN_MARK");
    let t = ctx(&post(&on(), r, "grep -rn TOKEN_MARK shop", &out), "PostToolUse");
    let add = line_of(r, "shop/cart.py", "TOKEN_MARK add");
    let place = line_of(r, "shop/orders.go", "TOKEN_MARK place");
    assert!(t.contains(&format!("shop/cart.py:{add}")), "{t}");
    assert!(t.contains("Cart.add_item"), "{t}");
    assert!(t.contains("Cart.total_price"), "{t}");
    assert!(t.contains("checkout_cart"), "{t}");
    assert!(t.contains(&format!("shop/orders.go:{place}")) && t.contains("PlaceOrder"), "{t}");
    assert!(t.contains("c:"), "a fox handle per symbol: {t}");
    // A hit outside any definition (the Go file's top comment) is not attributed.
    let top = line_of(r, "shop/orders.go", "top level comment");
    assert!(!t.contains(&format!("shop/orders.go:{top}")), "{t}");
}

#[test]
fn grep_context_is_off_unless_enabled() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let out = grep(r, "shop", "TOKEN_MARK");
    assert_eq!(post(&Config::default(), r, "grep -rn TOKEN_MARK shop", &out), "");
    assert_eq!(post(&config::parse("[hooks]\nprompt_context = true\n"), r, "grep -rn TOKEN_MARK shop", &out), "");
    // The default `run` reads the store's config.toml, which has no such key here.
    let input = json!({"hook_event_name": "PostToolUse", "cwd": r, "tool_name": "Bash",
        "tool_input": {"command": "grep -rn TOKEN_MARK shop"}, "tool_response": {"stdout": out}});
    assert_eq!(hook::run("post-tool-use", &input.to_string(), hook::Harness::Claude), "");
}

#[test]
fn grep_variants_it_can_attribute() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let cfg = on();
    let out = grep(r, "shop", "TOKEN_MARK");
    for cmd in [
        "rg -n TOKEN_MARK shop",
        "git grep -n TOKEN_MARK -- shop",
        "grep -rn 'TOKEN_MARK' shop | head -20",
        "grep -rn \"TOKEN_MARK\" shop 2>/dev/null | head -n 5",
        "egrep -rn 'TOKEN_MARK|nothing' shop",
        "rg -n --no-heading TOKEN_MARK shop | sort | uniq",
    ] {
        let t = ctx(&post(&cfg, r, cmd, &out), "PostToolUse");
        assert!(t.contains("Cart.add_item"), "{cmd}: {t}");
    }
    // `cd <repo> && grep` from another cwd.
    let elsewhere = tempfile::tempdir().unwrap();
    let cmd = format!("cd {} && grep -rn TOKEN_MARK shop", r.display());
    assert!(ctx(&post(&cfg, elsewhere.path(), &cmd, &out), "PostToolUse").contains("checkout_cart"));
    // From a subdirectory: `./cart.py:9:...` resolves against the cwd.
    let sub = r.join("shop");
    let out = grep(&sub, ".", "TOKEN_MARK");
    assert!(ctx(&post(&cfg, &sub, "grep -rn TOKEN_MARK .", &out), "PostToolUse").contains("Cart.total_price"));
    // Absolute paths in the output.
    let abs = grep(r, "shop", "TOKEN_MARK").lines().map(|l| format!("{}/{l}\n", r.display())).collect::<String>();
    let cmd = format!("grep -rn TOKEN_MARK {}/shop", r.display());
    assert!(ctx(&post(&cfg, r, &cmd, &abs), "PostToolUse").contains("PlaceOrder"));
    // One file named: grep prints `line:text` with no path.
    let one: String = grep(r, "shop", "TOKEN_MARK")
        .lines()
        .filter(|l| l.starts_with("shop/cart.py:"))
        .map(|l| format!("{}\n", l.trim_start_matches("shop/cart.py:")))
        .collect();
    let t = ctx(&post(&cfg, r, "grep -n TOKEN_MARK shop/cart.py", &one), "PostToolUse");
    assert!(t.contains("Cart.add_item") && t.contains("shop/cart.py"), "{t}");
}

#[test]
fn grep_silent_on_what_it_cannot_attribute() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let cfg = on();
    let out = grep(r, "shop", "TOKEN_MARK");
    for cmd in [
        "grep -rn TOKEN_MARK shop; rm -rf shop",
        "grep -rn TOKEN_MARK shop && echo done",
        "grep -rn TOKEN_MARK shop | xargs sed -i s/a/b/",
        "grep -rn TOKEN_MARK shop | awk -F: '{print $1}'",
        "echo $(grep -rn TOKEN_MARK shop)",
        "grep -rn `echo TOKEN_MARK` shop",
        "grep -rn TOKEN_MARK shop > out.txt",
        "grep -rln TOKEN_MARK shop",
        "grep -rc TOKEN_MARK shop",
        "grep -rn --count TOKEN_MARK shop",
        "find shop -name '*.py' | xargs grep -n TOKEN_MARK",
        "cat shop/cart.py",
        "ls -la",
        "for f in shop/*; do grep -n TOKEN_MARK $f; done",
    ] {
        assert_eq!(post(&cfg, r, cmd, &out), "", "{cmd}");
    }
    // grep without line numbers prints `path:text`.
    let no_lines: String = out
        .lines()
        .map(|l| {
            let mut it = l.splitn(3, ':');
            let (p, _, t) = (it.next().unwrap(), it.next(), it.next().unwrap());
            format!("{p}:{t}\n")
        })
        .collect();
    assert_eq!(post(&cfg, r, "grep -r TOKEN_MARK shop", &no_lines), "");
    // Context lines (`path-line-text`) and separators are not hits.
    let ctx_only = "shop/cart.py-10-        self.items.append((sku, qty))\n--\n";
    assert_eq!(post(&cfg, r, "grep -rn -A1 TOKEN_MARK shop", ctx_only), "");
    // Another tool, an empty result, and a directory foxdb has not indexed.
    let input = json!({"hook_event_name": "PostToolUse", "cwd": r, "tool_name": "Read",
        "tool_input": {"file_path": "shop/cart.py"}, "tool_response": {"stdout": out}});
    assert_eq!(hook::run_with(&cfg, "post-tool-use", &input.to_string(), hook::Harness::Claude), "");
    assert_eq!(post(&cfg, r, "grep -rn NOTHING shop", ""), "");
    let other = tempfile::tempdir().unwrap();
    copy_dir(&fixture(), other.path());
    fs::create_dir_all(other.path().join(".git")).unwrap();
    fs::write(other.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(post(&cfg, other.path(), "grep -rn TOKEN_MARK shop", &out), "");
    // Garbage input.
    assert_eq!(hook::run_with(&cfg, "post-tool-use", "not json", hook::Harness::Claude), "");
}

#[test]
fn grep_skips_a_file_changed_since_it_was_indexed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    // Two lines pushed in at the top: the index's line numbers no longer hold for cart.py.
    let p = r.join("shop/cart.py");
    let body = fs::read_to_string(&p).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(&p, format!("# moved\n# moved\n{body}")).unwrap();
    let out = grep(r, "shop", "TOKEN_MARK");
    let t = ctx(&post(&on(), r, "grep -rn TOKEN_MARK shop", &out), "PostToolUse");
    assert!(!t.contains("cart.py"), "stale file attributed: {t}");
    assert!(t.contains("PlaceOrder"), "{t}");
}

#[test]
fn grep_context_is_capped() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let mut big = String::new();
    for i in 0..60 {
        big.push_str(&format!("def a_rather_long_function_name_number_{i}(argument_one, argument_two):\n    return 1  # BIG_MARK\n\n\n"));
    }
    fs::write(r.join("shop/big.py"), big).unwrap();
    index(r);
    let out = grep(r, "shop", "BIG_MARK");
    assert_eq!(out.lines().count(), 60);
    let t = ctx(&post(&on(), r, "grep -rn BIG_MARK shop", &out), "PostToolUse");
    assert!(t.chars().count() <= 1_200, "{} chars: {t}", t.chars().count());
    let rows = t.lines().filter(|l| l.contains("a_rather_long_function_name")).count();
    assert!((1..=12).contains(&rows), "{rows} rows: {t}");
    assert!(t.contains("more"), "says hits were left out: {t}");
}

// ---- UserPromptSubmit pre-answer ----

#[test]
fn prompt_naming_one_symbol_gets_its_definition() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let t = ctx(&prompt(&on(), r, "Why does checkout_cart ignore the discount?"), "UserPromptSubmit");
    let def = line_of(r, "shop/cart.py", "def checkout_cart");
    assert!(t.contains(&format!("shop/cart.py:{def}")), "{t}");
    assert!(t.contains("checkout_cart") && t.contains("c:"), "{t}");
    assert!(t.contains("1 caller"), "reorder_cart calls it: {t}");
    // Backticks, `Type.member` and camelCase forms.
    let t = ctx(&prompt(&on(), r, "Look at `Cart.total_price` and PlaceOrder please"), "UserPromptSubmit");
    assert!(t.contains("Cart.total_price") && t.contains("PlaceOrder"), "{t}");
    assert!(t.lines().filter(|l| l.contains("c:")).count() <= 5, "{t}");
}

#[test]
fn prompt_naming_one_file_gets_it() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let t = ctx(&prompt(&on(), r, "what does shop/orders.go do?"), "UserPromptSubmit");
    assert!(t.contains("shop/orders.go"), "{t}");
    // A unique basename resolves too; a basename shared by none or many does not.
    assert!(ctx(&prompt(&on(), r, "explain orders.go"), "UserPromptSubmit").contains("shop/orders.go"));
    assert_eq!(prompt(&on(), r, "explain missing.go"), "");
}

#[test]
fn prompt_is_silent_unless_exactly_one_match() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let cfg = on();
    for p in [
        "Fix the bug in the cart please, the total is wrong.",
        "render_page shows the wrong text",
        "Where is NoSuchThing defined?",
        "Cart is broken",
        "",
    ] {
        assert_eq!(prompt(&cfg, r, p), "", "{p}");
    }
    assert_eq!(prompt(&Config::default(), r, "Why does checkout_cart fail?"), "");
    let other = tempfile::tempdir().unwrap();
    assert_eq!(prompt(&cfg, other.path(), "Why does checkout_cart fail?"), "");
}

#[test]
fn prompt_injects_at_most_five_rows() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    let mut s = String::new();
    for i in 0..9 {
        s.push_str(&format!("def unique_helper_{i}():\n    return {i}\n\n\n"));
    }
    fs::write(r.join("shop/helpers.py"), s).unwrap();
    index(r);
    let names: Vec<String> = (0..9).map(|i| format!("unique_helper_{i}")).collect();
    let t = ctx(&prompt(&on(), r, &format!("compare {}", names.join(", "))), "UserPromptSubmit");
    assert_eq!(t.lines().filter(|l| l.contains("unique_helper_")).count(), 5, "{t}");
}

#[test]
fn grep_rows_put_non_test_files_first() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, repo) = setup();
    let r = repo.path();
    fs::write(r.join("shop/a_test.go"), "package shop\n\nfunc TestPlace(t *T) {\n\tPlaceOrder(\"x\") // TOKEN_MARK test\n}\n").unwrap();
    index(r);
    let out = grep(r, "shop", "TOKEN_MARK");
    assert!(out.starts_with("shop/a_test.go:"), "grep lists the test file first: {out}");
    let t = ctx(&post(&on(), r, "grep -rn TOKEN_MARK shop", &out), "PostToolUse");
    let rows: Vec<&str> = t.lines().skip(1).collect();
    assert!(rows.last().is_some_and(|l| l.contains("TestPlace")), "{t}");
}
