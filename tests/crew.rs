//! `fox crew`: worktrees, one `fox index` each, and panes through fake `tmux` and `herdr` shims on
//! PATH. The shims log their argv and never start anything, so no harness or model runs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const FOX: &str = env!("CARGO_BIN_EXE_fox");

/// Logs each call as one line of NUL-free, tab-separated args. At each pane launch it also logs
/// `fox status` so the test can see the worktree was indexed first.
const TMUX: &str = r#"#!/bin/sh
printf '%s' tmux >> "$SHIM_LOG/calls"; for a in "$@"; do printf '\t%s' "$a" >> "$SHIM_LOG/calls"; done; echo >> "$SHIM_LOG/calls"
case "$1" in
  has-session) [ -f "$SHIM_LOG/session" ] ;;
  new-session|new-window)
    shift
    while [ "$#" -gt 0 ]; do
      case "$1" in
        -n) name=$2; shift ;;
        -c) cwd=$2; shift ;;
        -F) break ;;
      esac
      shift
    done
    touch "$SHIM_LOG/session"
    n=$(ls "$SHIM_LOG" | grep -c '^pane-')
    while [ -f "$SHIM_LOG/pane-$n" ]; do n=$((n + 1)); done
    printf 'fox-crew\t%s\t%s\n' "$name" "$cwd" > "$SHIM_LOG/pane-$n"
    cd "$cwd" || exit 1
    "$FOX_BIN" status > "$SHIM_LOG/status-at-launch-$n" 2>&1
    env | grep -E '^(CLAUDECODE|CLAUDE_CODE_SESSION_ID|FOXDB_SESSION)=' >> "$SHIM_LOG/markers"
    echo "%$n" ;;
  display-message)
    p=$(echo "$4" | tr -d %)
    [ -f "$SHIM_LOG/pane-$p" ] || { echo "can't find pane: $4" >&2; exit 1; }
    case "$5" in
      *window_name*) cat "$SHIM_LOG/pane-$p" ;;
      *pane_start_path*) printf '%s\t' "$4"; cut -f1,3- "$SHIM_LOG/pane-$p" ;;
      *) echo "live claude" ;;
    esac ;;
  kill-pane)
    [ ! -f "$SHIM_LOG/deny-close" ] || { echo "close refused" >&2; exit 1; }
    p=$(echo "$3" | tr -d %)
    rm "$SHIM_LOG/pane-$p" ;;
esac
"#;

const HERDR: &str = r#"#!/bin/sh
printf '%s' herdr >> "$SHIM_LOG/calls"; for a in "$@"; do printf '\t%s' "$a" >> "$SHIM_LOG/calls"; done; echo >> "$SHIM_LOG/calls"
case "$1 $2" in
  "workspace create") echo '{"id":"r","result":{"workspace":{"workspace_id":"w7"},"tab":{"tab_id":"w7:t1"},"root_pane":{"pane_id":"w7:p1"}}}' ;;
  "pane get") echo '{"result":{"pane":{"pane_id":"w7:p1","agent":{"label":"claude","status":"idle"}}}}' ;;
esac
"#;

struct Env {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    bin: tempfile::TempDir,
    log: tempfile::TempDir,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@example.com"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn setup(shims: &[(&str, &str)]) -> Env {
    let e = Env {
        home: tempfile::tempdir().unwrap(),
        repo: tempfile::tempdir().unwrap(),
        bin: tempfile::tempdir().unwrap(),
        log: tempfile::tempdir().unwrap(),
    };
    for (n, body) in shims {
        let p = e.bin.path().join(n);
        fs::write(&p, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let r = e.repo.path();
    fs::create_dir_all(r.join("src")).unwrap();
    fs::write(r.join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();
    fs::write(r.join(".gitignore"), ".claude/\n").unwrap();
    git(r, &["init", "-q", "-b", "main"]);
    git(r, &["add", "."]);
    git(r, &["commit", "-q", "-m", "init"]);
    e
}

fn fox_command(e: &Env, binary: impl AsRef<std::ffi::OsStr>) -> Command {
    let path = format!("{}:/usr/bin:/bin", e.bin.path().display());
    let mut cmd = Command::new(binary);
    cmd.current_dir(e.repo.path())
        .env("PATH", path)
        .env("HOME", e.home.path())
        .env("FOXDB_HOME", e.home.path().join("fx"))
        .env("FOXDB_NO_DAEMON", "1")
        .env("FOXDB_CLAUDE_PROJECTS", e.home.path().join("no-claude"))
        .env("FOXDB_CODEX_SESSIONS", e.home.path().join("no-codex"))
        .env("SHIM_LOG", e.log.path())
        .env("FOX_BIN", FOX)
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_SESSION_ID", "caller-session")
        .env("FOXDB_SESSION", "caller-session")
        .env_remove("FOXDB_AGENT")
        .env_remove("FOXDB_PARENT")
        .env_remove("FOXDB_CLAUDE_BIN")
        .env_remove("FOXDB_CODEX_BIN")
        .env_remove("HERDR_BIN_PATH")
        .env_remove("TMUX");
    cmd
}

fn fox(e: &Env, args: &[&str]) -> (i32, String) {
    fox_output(fox_command(e, FOX).args(args))
}

fn fox_output(cmd: &mut Command) -> (i32, String) {
    let o = cmd.output().unwrap();
    (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

fn calls(e: &Env) -> Vec<Vec<String>> {
    fs::read_to_string(e.log.path().join("calls")).unwrap_or_default().lines().map(|l| l.split('\t').map(String::from).collect()).collect()
}

fn launches(e: &Env, verb: &str) -> Vec<Vec<String>> {
    calls(e).into_iter().filter(|c| c.get(1).is_some_and(|v| v == verb)).collect()
}

fn wt(e: &Env, name: &str) -> PathBuf {
    fs::canonicalize(e.repo.path()).unwrap().join(".claude/worktrees").join(format!("crew-{name}"))
}

fn after<'a>(c: &'a [String], flag: &str) -> Vec<&'a str> {
    c.windows(2).filter(|w| w[0] == flag).map(|w| w[1].as_str()).collect()
}

const PLAN: &str = r#"
name = "demo"
[[session]]
name = "api"
harness = "claude"
model = "haiku"
prompt = "Add a /health route"

[[session]]
name = "cli"
harness = "codex"
branch = "feat/cli"
prompt = "--help text for the CLI"
"#;

#[test]
fn up_makes_a_worktree_indexes_it_and_starts_the_native_cli_in_a_tmux_pane() {
    let e = setup(&[("tmux", TMUX)]);
    let repo = e.repo.path();
    git(repo, &["checkout", "-qb", "feat/cli"]);
    fs::write(repo.join("src/only_in_cli.rs"), "pub fn cli_branch_only() {}\n").unwrap();
    git(repo, &["add", "src/only_in_cli.rs"]);
    git(repo, &["commit", "-qm", "add cli code"]);
    git(repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("plan.toml"), PLAN).unwrap();
    let (c, out) = fox(&e, &["crew", "up", "plan.toml"]);
    assert_eq!(c, 0, "{out}");
    assert!(out.contains("api cl-") && out.contains("cli cx-"), "{out}");

    for (name, branch) in [("api", "crew/api"), ("cli", "feat/cli")] {
        let w = wt(&e, name);
        assert!(w.join("src/lib.rs").exists(), "{name} worktree");
        assert_eq!(git(&w, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(), branch);
    }
    let first = &launches(&e, "new-session")[0];
    let second = &launches(&e, "new-window")[0];
    let home = fs::canonicalize(e.home.path().join("fx")).unwrap();
    for (l, name) in [(first, "api"), (second, "cli")] {
        assert_eq!(after(l, "-n"), [name]);
        assert_eq!(after(l, "-c"), [wt(&e, name).to_string_lossy()]);
        let env = after(l, "-e");
        assert_eq!(env.len(), 3, "{l:?}");
        assert!(env[0].starts_with("FOXDB_AGENT=c"), "{env:?}");
        assert_eq!(env[1], "FOXDB_PARENT=crew:demo");
        assert_eq!(env[2], format!("FOXDB_HOME={}", home.display()));
        assert!(!l.iter().any(|a| a == "--bare" || a.contains("system-prompt") || a == "-p"), "{l:?}");
    }
    let tail = |l: &[String]| {
        let (env, command) =
            l[l.iter().position(|a| a == "#{pane_id}").unwrap() + 1..].split_at(1 + 2 * foxdb::crew::SESSION_MARKERS.len());
        let mut expected = vec!["env"];
        for key in foxdb::crew::SESSION_MARKERS {
            expected.extend(["-u", key]);
        }
        assert_eq!(env, expected);
        command.to_vec()
    };
    assert_eq!(tail(first), ["claude", "--model", "haiku", "--", "Add a /health route"]);
    assert_eq!(tail(second), ["codex", "--", "--help text for the CLI"]);

    let markers = fs::read_to_string(e.log.path().join("markers")).unwrap_or_default();
    assert!(markers.is_empty(), "the caller's session markers reached the pane: {markers}");

    // The project was in the store when each pane started.
    for n in 0..2 {
        let s = fs::read_to_string(e.log.path().join(format!("status-at-launch-{n}"))).unwrap();
        assert!(s.contains("indexed ") && !s.contains("not indexed"), "launch {n}: {s}");
    }

    let db = rusqlite::Connection::open(e.home.path().join("fx/fox.db")).unwrap();
    let indexed_at: String = db
        .query_row(
            "SELECT project.root FROM node JOIN project ON project.id = node.project_id WHERE node.name = 'cli_branch_only'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexed_at, wt(&e, "cli").to_string_lossy());

    let (c, ls) = fox(&e, &["crew", "ls"]);
    assert_eq!(c, 0, "{ls}");
    assert!(ls.contains("api cl-") && ls.contains("tmux %0 live claude"), "{ls}");
    assert!(ls.contains("crew:demo") && ls.contains("[crew:api]"), "agents tree in {ls}");

    let (c, out) = fox(&e, &["crew", "up", "plan.toml"]);
    assert_ne!(c, 0, "a second up of the same names must refuse: {out}");
}

#[test]
fn down_closes_panes_and_prunes_only_clean_worktrees() {
    let e = setup(&[("tmux", TMUX)]);
    let (c, out) = fox(&e, &["crew", "up", "2", "--harness", "codex", "--backend", "tmux"]);
    assert_eq!(c, 0, "{out}");
    assert!(wt(&e, "s1").exists() && wt(&e, "s2").exists(), "{out}");
    fs::write(wt(&e, "s2").join("scratch.txt"), "wip").unwrap();

    let (c, out) = fox(&e, &["crew", "down", "../s1", "--prune"]);
    assert_ne!(c, 0, "{out}");
    assert!(launches(&e, "kill-pane").is_empty());

    let deny = e.log.path().join("deny-close");
    fs::write(&deny, "").unwrap();
    let (c, out) = fox(&e, &["crew", "down", "s1", "--prune"]);
    assert_ne!(c, 0, "{out}");
    assert!(out.contains("record was kept") && wt(&e, "s1").exists(), "{out}");
    assert!(e.home.path().join("fx/crew/s1.json").is_file());
    fs::remove_file(deny).unwrap();

    let (c, out) = fox(&e, &["crew", "down", "s1"]);
    assert_eq!(c, 0, "{out}");
    assert!(out.contains("pane closed") && out.contains("worktree kept"), "{out}");
    assert!(wt(&e, "s1").exists(), "down without --prune keeps the worktree");
    assert_eq!(launches(&e, "kill-pane")[0][1..], ["kill-pane", "-t", "%0"]);

    let (c, out) = fox(&e, &["crew", "up", "1", "--backend", "tmux"]);
    assert_eq!(c, 0, "{out}");
    assert!(out.starts_with("s1 "), "the freed name is reused: {out}");

    let (c, out) = fox(&e, &["crew", "down", "--prune"]);
    assert_eq!(c, 0, "{out}");
    assert!(!wt(&e, "s1").exists(), "clean worktree pruned: {out}");
    assert!(wt(&e, "s2").exists() && out.contains("not clean"), "dirty worktree kept: {out}");
    assert!(git(e.repo.path(), &["branch", "--list", "crew/s1"]).contains("crew/s1"), "the branch stays");
    let (_, ls) = fox(&e, &["crew", "ls"]);
    assert!(ls.contains("no crew sessions"), "{ls}");
}

#[test]
fn herdr_on_path_is_the_default_backend() {
    let e = setup(&[("tmux", TMUX), ("herdr", HERDR)]);
    let (c, out) = fox(&e, &["crew", "up", "1", "--model", "haiku"]);
    assert_eq!(c, 0, "{out}");
    let cs = calls(&e);
    let create = cs.iter().find(|c| c[0] == "herdr" && c[1] == "workspace").expect("workspace create");
    assert_eq!(after(create, "--cwd"), [wt(&e, "s1").to_string_lossy()]);
    let home = fs::canonicalize(e.home.path().join("fx")).unwrap();
    assert_eq!(after(create, "--env"), ["FOXDB_AGENT=cl-1", "FOXDB_PARENT=crew:crew", &format!("FOXDB_HOME={}", home.display())]);
    assert_eq!(after(create, "--label"), ["s1"]);
    assert_eq!(create.last().map(String::as_str), Some("--no-focus"));
    let run = cs.iter().find(|c| c[0] == "herdr" && c[1] == "pane").expect("pane run");
    assert_eq!(run[1..], ["pane", "run", "w7:p1", "claude --model haiku"]);
    assert!(!cs.iter().any(|c| c[0] == "tmux"), "{cs:?}");

    let (_, ls) = fox(&e, &["crew", "ls"]);
    assert!(ls.contains("herdr w7:p1 idle"), "{ls}");
    let (c, out) = fox(&e, &["crew", "attach", "s1"]);
    assert_eq!(c, 0, "{out}");
    let (c, out) = fox(&e, &["crew", "down", "s1"]);
    assert_eq!(c, 0, "{out}");
    let cs = calls(&e);
    assert!(cs.iter().any(|c| c[1..] == ["workspace", "focus", "w7"]), "{cs:?}");
    assert!(cs.iter().any(|c| c[1..] == ["workspace", "close", "w7"]), "{cs:?}");
}

#[test]
fn herdr_worktree_created_event_indexes_the_worktree() {
    let e = setup(&[("herdr", HERDR)]);
    let ev = format!(
        r#"{{"event":"worktree.created","workspace":{{"workspace_id":"w2","cwd":"/x"}},"worktree":{{"path":"{}"}}}}"#,
        e.repo.path().display()
    );
    let path = format!("{}:/usr/bin:/bin", e.bin.path().display());
    let o = Command::new(FOX)
        .args(["crew", "herdr", "worktree-created"])
        .env("PATH", path)
        .env("HOME", e.home.path())
        .env("FOXDB_HOME", e.home.path().join("fx"))
        .env("FOXDB_NO_DAEMON", "1")
        .env("HERDR_PLUGIN_EVENT_JSON", ev)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let (_, st) = fox(&e, &["status"]);
    assert!(st.contains("indexed ") && !st.contains("not indexed"), "{st}");
}

#[test]
fn herdr_plugin_manifest_declares_the_action_and_the_event() {
    let m: toml::Value =
        toml::from_str(&fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin/herdr/herdr-plugin.toml")).unwrap()).unwrap();
    assert_eq!(m["version"].as_str(), Some(env!("CARGO_PKG_VERSION")), "bump plugin/herdr/herdr-plugin.toml");
    for k in ["id", "name", "min_herdr_version"] {
        assert!(m[k].is_str(), "{k}");
    }
    let actions = m["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 1);
    let cmd = |v: &toml::Value| v["command"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(cmd(&actions[0]), ["fox", "crew", "herdr", "new"]);
    let events = m["events"].as_array().unwrap();
    assert_eq!(events[0]["on"].as_str(), Some("worktree.created"));
    assert_eq!(cmd(&events[0]), ["fox", "crew", "herdr", "worktree-created"]);
}

#[test]
fn open_herdr_pane_keeps_an_idle_writers_lease() {
    let herdr = HERDR.replace(
        r#""pane get")"#,
        r#""pane get")
          [ ! -f "$SHIM_LOG/backend-unavailable" ] || { echo 'connection failed' >&2; exit 1; }
          [ ! -f "$SHIM_LOG/pane-gone" ] || { echo '{"error":{"code":"pane_not_found"}}' >&2; exit 1; };"#,
    );
    let e = setup(&[("herdr", &herdr)]);
    fs::write(e.repo.path().join("plan.toml"), "[[session]]\nname = \"api\"\nwriter = true\n").unwrap();
    let (c, out) = fox(&e, &["crew", "up", "plan.toml"]);
    assert_eq!(c, 0, "{out}");

    let expired = |args: &[&str]| fox_output(fox_command(&e, FOX).args(args).env("FOXDB_STALE_SECS", "-1"));
    let (c, out) = expired(&["lease", "list"]);
    assert_eq!(c, 0, "{out}");
    assert!(out.contains("writer cl-1"), "{out}");
    let (c, out) = expired(&["lease", "take", "--agent", "challenger"]);
    assert_ne!(c, 0, "{out}");
    assert!(out.contains("held by cl-1"), "{out}");

    let unavailable = e.log.path().join("backend-unavailable");
    fs::write(&unavailable, "").unwrap();
    let (c, out) = expired(&["crew", "ls"]);
    assert_eq!(c, 0, "{out}");
    assert!(out.contains("herdr w7:p1 unknown"), "{out}");
    let (c, out) = expired(&["lease", "list"]);
    assert_eq!(c, 0, "{out}");
    assert!(out.contains("writer cl-1") && out.contains("(unverified)"), "{out}");
    fs::remove_file(unavailable).unwrap();

    let (c, out) = expired(&["crew", "ls"]);
    assert_eq!(c, 0, "{out}");
    assert!(out.contains("herdr w7:p1 idle") && out.contains("writer running"), "{out}");

    fs::write(e.log.path().join("pane-gone"), "").unwrap();
    let (c, out) = expired(&["lease", "list"]);
    assert_eq!(c, 0, "{out}");
    assert_eq!(out, "no writer leases\n");
}

#[test]
fn tmux_window_title_can_change_while_the_pane_stays_owned() {
    let e = setup(&[("tmux", TMUX)]);
    let (c, out) = fox(&e, &["crew", "up", "1", "--backend", "tmux"]);
    assert_eq!(c, 0, "{out}");

    let pane = e.log.path().join("pane-0");
    let identity = fs::read_to_string(&pane).unwrap().replace("\ts1\t", "\trenamed\t");
    fs::write(pane, identity).unwrap();
    let (c, out) = fox(&e, &["crew", "ls"]);
    assert_eq!(c, 0, "{out}");
    assert!(out.contains("tmux %0 live claude"), "{out}");
    let (c, out) = fox(&e, &["crew", "down", "s1"]);
    assert_eq!(c, 0, "{out}");
    assert!(out.contains("pane closed"), "{out}");
}

#[test]
fn herdr_index_hook_prefers_worktree_cwd_to_workspace_path() {
    let e = setup(&[]);
    let ev = serde_json::json!({
        "event": "worktree.created",
        "workspace": { "path": e.home.path() },
        "worktree": { "cwd": e.repo.path() }
    });
    let (c, out) =
        fox_output(fox_command(&e, FOX).args(["crew", "herdr", "worktree-created"]).env("HERDR_PLUGIN_EVENT_JSON", ev.to_string()));
    assert_eq!(c, 0, "{out}");
    assert_eq!(out.trim(), format!("indexed {}", e.repo.path().display()));
    let (_, out) = fox(&e, &["status"]);
    assert!(out.contains("indexed ") && !out.contains("not indexed"), "{out}");
}

#[test]
fn herdr_index_hook_has_a_deadline() {
    use std::time::{Duration, Instant};

    let e = setup(&[("fox", "#!/bin/sh\nsleep 8\n")]);
    let hook = e.bin.path().join("hook");
    fs::hard_link(FOX, &hook).or_else(|_| fs::copy(FOX, &hook).map(|_| ())).unwrap();
    let ev = serde_json::json!({ "worktree": { "path": e.repo.path() } });
    let start = Instant::now();
    let (c, out) =
        fox_output(fox_command(&e, hook).args(["crew", "herdr", "worktree-created"]).env("HERDR_PLUGIN_EVENT_JSON", ev.to_string()));
    assert!(start.elapsed() < Duration::from_secs(7), "{out}");
    assert_ne!(c, 0, "{out}");
    assert!(out.contains("timed out"), "{out}");
}

#[test]
fn launch_failure_keeps_the_prompt_out_of_diagnostics() {
    let herdr = HERDR.replace(
        r#""pane get")"#,
        r#""pane run") printf 'mock refused to submit: %s\n' "$4" >&2; exit 1 ;;
"pane get")"#,
    );
    let tmux = TMUX.replace(
        "new-session|new-window)",
        r#"new-session|new-window) printf 'mock refused to start: %s\n' "$@" >&2; exit 1 ;;
unused)"#,
    );
    for (backend, shim) in [("herdr", &herdr), ("tmux", &tmux)] {
        let e = setup(&[(backend, shim)]);
        fs::write(e.repo.path().join("plan.toml"), PLAN).unwrap();
        let (c, out) = fox(&e, &["crew", "up", "plan.toml", "--backend", backend]);
        assert_ne!(c, 0, "{out}");
        assert!(out.contains(backend) && out.contains("failed"), "{out}");
        assert!(!out.contains("Add a /health route"), "{out}");
    }
}

#[test]
fn lease_take_checks_a_crew_pane_without_holding_a_database_write_lock() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let herdr = HERDR.replace(
        r#""pane get")"#,
        r#""pane get")
          touch "$SHIM_LOG/probing"
          while [ ! -f "$SHIM_LOG/reply" ]; do sleep 0.02; done;"#,
    );
    let e = setup(&[("herdr", &herdr)]);
    fs::write(e.repo.path().join("plan.toml"), "[[session]]\nname = \"api\"\nwriter = true\n").unwrap();
    let (c, out) = fox(&e, &["crew", "up", "plan.toml"]);
    assert_eq!(c, 0, "{out}");

    let child = fox_command(&e, FOX)
        .args(["lease", "take", "--agent", "challenger"])
        .env("FOXDB_STALE_SECS", "-1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !e.log.path().join("probing").exists() && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(e.log.path().join("probing").exists(), "the lease take did not query its incumbent's pane");

    let db = rusqlite::Connection::open(e.home.path().join("fx/fox.db")).unwrap();
    db.busy_timeout(Duration::from_millis(200)).unwrap();
    let write = db.execute("UPDATE agent SET task = 'unrelated write' WHERE id = 'challenger'", []);
    fs::write(e.log.path().join("reply"), "").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("held by cl-1"));
    assert_eq!(write.unwrap(), 1);
}
