//! Milestone 2: agent registry, handoff packets, harness switching, dispatch/wait, writer leases
//! and shared board reads. Child harnesses are stub `codex` and `claude` scripts on a test PATH:
//! no model is ever called. `FOXDB_REAL_SMOKE=1` opts in to one real `codex exec` round-trip.

use foxdb::{agents, db, handoff, hook, inventory, project};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

const FOX: &str = env!("CARGO_BIN_EXE_fox");

/// Default stub report: a valid fixed-format report. `STUB_REPORT` replaces it (raw text),
/// `STUB_WRITE` makes the worker create that file, `STUB_RAN` logs a command execution event.
const STUB_CODEX: &str = r#"#!/bin/sh
out=""; schema=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; [ "$prev" = "--output-schema" ] && schema="$a"; prev="$a"; done
printf '%s\n' "$@" > "$STUB_LOG/codex-args-$FOXDB_AGENT"
[ -n "$schema" ] && cp "$schema" "$STUB_LOG/codex-schema-$FOXDB_AGENT"
cat > "$STUB_LOG/codex-prompt-$FOXDB_AGENT"
env | grep '^FOXDB_' | sort > "$STUB_LOG/codex-env-$FOXDB_AGENT"
echo '{"type":"thread.started","thread_id":"th-'"$FOXDB_AGENT"'"}'
[ -n "$STUB_SLEEP" ] && sleep "$STUB_SLEEP"
[ -n "$STUB_WRITE" ] && mkdir -p "$(dirname "$STUB_WRITE")" && echo "written by $FOXDB_AGENT" > "$STUB_WRITE"
[ -n "$STUB_RAN" ] && echo '{"type":"item.completed","item":{"type":"command_execution","command":"'"$STUB_RAN"'","exit_code":0}}'
echo '{"type":"turn.completed"}'
if [ -n "$STUB_REPORT" ]; then printf '%s\n' "$STUB_REPORT" > "$out"; else
printf '{"status":"done","summary":"codex finished %s token=abcdefghijklmnop1234","files":[],"tests":{"ran":[],"passed":false},"decisions":[],"next":[]}\n' "$FOXDB_AGENT" > "$out"; fi
"#;

const STUB_CLAUDE: &str = r#"#!/bin/sh
sid=""; prev=""
for a in "$@"; do [ "$prev" = "--session-id" ] && sid="$a"; prev="$a"; done
printf '%s\n' "$@" > "$STUB_LOG/claude-args-$FOXDB_AGENT"
cat > "$STUB_LOG/claude-prompt-$FOXDB_AGENT"
[ -n "$STUB_SLEEP" ] && sleep "$STUB_SLEEP"
[ -n "$STUB_WRITE" ] && mkdir -p "$(dirname "$STUB_WRITE")" && echo "written by $FOXDB_AGENT" > "$STUB_WRITE"
r=${STUB_REPORT:-'{"status":"done","summary":"claude finished '"$FOXDB_AGENT"'","files":[],"tests":{"ran":[],"passed":false},"decisions":[],"next":[]}'}
case "$r" in "{"*) so="$r";; *) so=null;; esac
printf '{"type":"result","subtype":"success","is_error":false,"result":"done","structured_output":%s,"session_id":"%s"}\n' "$so" "$sid"
"#;

struct Env {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    stubs: tempfile::TempDir,
    log: tempfile::TempDir,
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn git(repo: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .env("HOME", repo)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

fn setup() -> Env {
    let e = Env {
        home: tempfile::tempdir().unwrap(),
        repo: tempfile::tempdir().unwrap(),
        stubs: tempfile::tempdir().unwrap(),
        log: tempfile::tempdir().unwrap(),
    };
    for (n, body) in [("codex", STUB_CODEX), ("claude", STUB_CLAUDE)] {
        let p = e.stubs.path().join(n);
        fs::write(&p, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let r = e.repo.path();
    write(r, "AGENT_MILESTONE_LOG.md", "# Log\n\n## Current gate\n\n- Gate: OR-2 is active.\n");
    write(r, "src/lib.rs", "pub fn alpha() {}\n\npub fn beta() {}\n");
    git(r, &["init", "-q", "-b", "main"]);
    git(r, &["add", "."]);
    git(r, &["commit", "-q", "-m", "init"]);
    let (c, out, err) = fox(&e, &["index", "--no-global", "--quiet"], r, &[]);
    assert_eq!(c, 0, "{out}{err}");
    e
}

/// Run the real `fox` binary with a scratch FOXDB_HOME and HOME and the stub harnesses first on PATH.
fn fox(e: &Env, args: &[&str], cwd: &Path, extra: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = env_cmd(e, FOX, cwd);
    cmd.args(args);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let o = cmd.output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

/// `program` with the scratch environment and no inherited agent identity.
fn env_cmd(e: &Env, program: &str, cwd: &Path) -> Command {
    let path = format!("{}:{}", e.stubs.path().display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd)
        .env("PATH", path)
        .env("HOME", e.home.path())
        .env("FOXDB_HOME", e.home.path().join("fx"))
        .env("FOXDB_NO_DAEMON", "1")
        .env("FOXDB_CLAUDE_PROJECTS", e.home.path().join("no-claude"))
        .env("FOXDB_CODEX_SESSIONS", e.home.path().join("no-codex"))
        .env("STUB_LOG", e.log.path())
        .env_remove("FOXDB_AGENT")
        .env_remove("FOXDB_PARENT")
        .env_remove("FOXDB_SESSION")
        .env_remove("FOXDB_HARNESS")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("STUB_SLEEP")
        .env_remove("STUB_REPORT")
        .env_remove("STUB_WRITE")
        .env_remove("STUB_RAN");
    cmd
}

fn ok(e: &Env, args: &[&str], extra: &[(&str, &str)]) -> String {
    let (c, out, err) = fox(e, args, e.repo.path(), extra);
    assert_eq!(c, 0, "fox {args:?} failed: {out}{err}");
    out
}

fn first_word(s: &str) -> String {
    s.split_whitespace().next().unwrap_or_default().to_string()
}

fn handle_in(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c == ',' || c == ')' || c == '(')
        .find(|w| w.starts_with("h:"))
        .unwrap_or_else(|| panic!("no handle in {s}"))
        .to_string()
}

fn log(e: &Env, name: &str) -> String {
    fs::read_to_string(e.log.path().join(name)).unwrap_or_else(|_| panic!("stub log {name} missing"))
}

#[test]
fn dispatch_report_wait_round_trip_for_both_harnesses() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let cx = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Add a test for alpha", "--label", "impl"], &[]));
    let cl = first_word(&ok(&e, &["dispatch", "--harness", "claude", "--task", "Review beta"], &[]));
    assert_ne!(cx, cl);
    let w = ok(&e, &["wait", &cx, &cl, "--all", "--timeout", "30"], &[]);
    assert!(w.contains(&format!("{cx} done")) && w.contains(&format!("{cl} done")), "{w}");

    // Final output became report handoffs, redacted, attributed to the worker.
    let reports = ok(&e, &["mem", "read", "--kind", "report"], &[]);
    assert!(reports.contains(&format!("codex finished {cx}")) && reports.contains(&format!("claude finished {cl}")), "{reports}");
    assert!(!reports.contains("abcdefghijklmnop1234"), "report not redacted: {reports}");

    // Codex: JSON events, last message to a file, foxdb MCP and identity via per-process config.
    let a = log(&e, &format!("codex-args-{cx}"));
    for want in ["exec", "--json", "-o", "-C", "-s", "read-only", "mcp_servers.foxdb.command=", "mcp_servers.foxdb.env_vars="] {
        assert!(a.contains(want), "codex args lack {want}: {a}");
    }
    assert!(!a.contains("--ephemeral") && !a.contains("dangerously"), "{a}");
    let env = log(&e, &format!("codex-env-{cx}"));
    assert!(env.contains(&format!("FOXDB_AGENT={cx}")), "{env}");

    // Claude: print mode with a pre-assigned session id, explicit MCP config and permission.
    let a = log(&e, &format!("claude-args-{cl}"));
    for want in [
        "-p",
        "--output-format",
        "json",
        "--session-id",
        "--mcp-config",
        "--allowedTools",
        "mcp__foxdb__*",
        "--exclude-dynamic-system-prompt-sections",
    ] {
        assert!(a.lines().any(|l| l == want || l.starts_with(want)), "claude args lack {want}: {a}");
    }
    assert!(!a.contains("--no-session-persistence") && !a.lines().any(|l| l.ends_with("-system-prompt") || l == "--system-prompt"), "{a}");

    // The registry knows how to resume both natively.
    let list = ok(&e, &["agents"], &[]);
    assert!(list.contains(&format!("th-{cx}")), "codex thread id not recorded: {list}");
    let sw = ok(&e, &["switch", "--to", "codex", "--from", "codex", "--agent", &cx], &[]);
    assert!(sw.contains(&format!("codex exec resume th-{cx}")) || sw.contains(&format!("codex resume th-{cx}")), "{sw}");

    // A task given as a handoff: the report is its child, and `fox wait h:` sees it.
    let req = handle_in(&ok(&e, &["mem", "post", "Rename beta to gamma", "--kind", "request"], &[]));
    let id = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", &req], &[]));
    let w = ok(&e, &["wait", &req, "--timeout", "30"], &[]);
    assert!(w.contains(&req) && w.contains("report h:"), "{w}");
    let prompt = log(&e, &format!("codex-prompt-{id}"));
    assert!(prompt.contains("Rename beta to gamma") && prompt.contains(&req), "{prompt}");
    let shown = ok(&e, &["mem", "read", &req], &[]);
    assert!(shown.contains("children: h:"), "lineage missing: {shown}");
}

#[test]
fn writer_lease_refuses_a_second_writer_and_frees_on_exit() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    // Each writer writes, so its done report carries no "changed nothing" check.
    let slow = [("STUB_SLEEP", "3"), ("STUB_WRITE", "src/a.rs")];
    let a = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "edit alpha", "--writer"], &slow));
    let (code, out, err) = fox(&e, &["dispatch", "--harness", "claude", "--task", "edit beta", "--writer"], e.repo.path(), &[]);
    assert_ne!(code, 0, "second writer was admitted: {out}");
    assert!(err.contains(&format!("held by {a}")), "{err}");
    let leases = ok(&e, &["lease", "list"], &[]);
    assert!(leases.contains(&a), "{leases}");
    // A reader is never refused.
    ok(&e, &["dispatch", "--harness", "claude", "--task", "read beta"], &[]);
    let args = log(&e, &format!("codex-args-{a}"));
    assert!(args.contains("workspace-write"), "{args}");
    ok(&e, &["wait", &a, "--timeout", "30"], &[]);
    let leases = ok(&e, &["lease", "list"], &[]);
    assert!(!leases.contains(&a), "lease outlived its holder: {leases}");
    let b = first_word(&ok(&e, &["dispatch", "--harness", "claude", "--task", "edit beta", "--writer"], &[("STUB_WRITE", "src/b.rs")]));
    ok(&e, &["wait", &b, "--timeout", "30"], &[]);
    // Manual lease for an interactive controller: held while its process lives.
    let me = std::process::id().to_string();
    ok(&e, &["lease", "take", "--agent", "ctl", "--pid", &me], &[]);
    let (code, _, err) = fox(&e, &["dispatch", "--harness", "codex", "--task", "x", "--writer"], e.repo.path(), &[]);
    assert!(code != 0 && err.contains("held by ctl"), "{err}");
    ok(&e, &["lease", "release"], &[]);
    assert!(!ok(&e, &["lease", "list"], &[]).contains("ctl"));
}

#[test]
fn take_is_exactly_once_under_concurrency_and_expired_leases_are_retaken_once() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    write(repo.path(), "src/lib.rs", "pub fn alpha() {}\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let id = handoff::post(&c, &handoff::Post { project_id: p.id, kind: "request", text: "do it", ..Default::default() }).unwrap();

    let race = |who: &'static str, lease: Option<i64>| -> Vec<String> {
        let ts: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    let w = db::open_rw().unwrap();
                    handoff::take(&w, id, &format!("{who}{i}"), lease).unwrap()
                })
            })
            .collect();
        ts.into_iter().map(|t| t.join().unwrap()).collect()
    };
    // A 1 s lease: long enough that a busy host can't let it expire mid-race (leases have
    // whole-second resolution, so a 0 s lease could legitimately lapse between racers).
    let r = race("a", Some(1));
    assert_eq!(r.iter().filter(|s| s.starts_with("claimed")).count(), 1, "{r:?}");
    // The lease has expired: exactly one of eight retakers wins again.
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let r = race("b", Some(600));
    assert_eq!(r.iter().filter(|s| s.starts_with("claimed")).count(), 1, "{r:?}");
    // A live lease holds against everyone else.
    let r = race("c", Some(600));
    assert_eq!(r.iter().filter(|s| s.starts_with("claimed")).count(), 0, "{r:?}");
    assert!(handoff::show(&c, id).unwrap().contains("lease until"), "lease not shown");
}

#[test]
fn switch_prints_the_command_both_ways_and_reads_either_transcript() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let pk = handle_in(&ok(&e, &["handoff", "create", "--goal", "Ship the trigram arm", "--next", "run the gate"], &[]));
    let to_codex = ok(&e, &["switch", "--to", "codex", "--from", "claude", "--handoff", &pk], &[]);
    assert!(to_codex.starts_with("codex ") || to_codex.contains("\ncodex ") || to_codex.contains(" codex "), "{to_codex}");
    assert!(to_codex.contains("mcp_servers.foxdb.command=") && to_codex.contains(&pk), "{to_codex}");
    assert!(!to_codex.contains("Ship the trigram arm"), "packet should be referenced, not inlined: {to_codex}");
    let to_claude = ok(&e, &["switch", "--to", "claude", "--from", "codex", "--handoff", &pk], &[]);
    assert!(to_claude.contains("claude ") && to_claude.contains("--mcp-config") && to_claude.contains(&pk), "{to_claude}");
    let inline = ok(&e, &["switch", "--to", "claude", "--from", "codex", "--handoff", &pk, "--inline"], &[]);
    assert!(inline.contains("Ship the trigram arm"), "{inline}");
    // Same harness: the native resume, no packet.
    let same = ok(&e, &["switch", "--to", "claude", "--from", "claude", "--session", "abc-123"], &[]);
    assert!(same.contains("claude --resume abc-123"), "{same}");

    // From a Claude transcript (.jsonl) and a Codex rollout (.jsonl.zst).
    let cwd = e.repo.path().to_string_lossy().to_string();
    let tx = tempfile::tempdir().unwrap();
    let cj = tx.path().join("c-1.jsonl");
    fs::write(
        &cj,
        format!(
            "{}\n{}\n",
            json!({"type":"user","message":{"role":"user","content":"Make search faster"},"sessionId":"c-1","cwd":cwd,"timestamp":"2026-11-01T10:00:01Z"}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Halfway: src/lib.rs:1 needs a cache. password=hunter2hunter2"}]},"sessionId":"c-1","timestamp":"2026-11-01T10:05:00Z"})
        ),
    )
    .unwrap();
    let out = ok(&e, &["switch", "--to", "codex", "--transcript", cj.to_str().unwrap()], &[]);
    let h = handle_in(&out);
    let r = ok(&e, &["handoff", "render", &h, "--for", "codex"], &[]);
    assert!(r.contains("Make search faster") && r.contains("Halfway") && !r.contains("hunter2"), "{r}");
    let raw = format!(
        "{}\n{}\n{}\n",
        json!({"timestamp":"2026-11-02T09:00:00Z","type":"session_meta","payload":{"id":"x-9","cwd":cwd,"timestamp":"2026-11-02T09:00:00Z"}}),
        json!({"timestamp":"2026-11-02T09:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Add a trigram arm"}}),
        json!({"timestamp":"2026-11-02T09:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"Trigram arm half done."}})
    );
    let zst = tx.path().join("rollout-2026-11-02T09-00-00-x-9.jsonl.zst");
    fs::write(&zst, ruzstd::encoding::compress_to_vec(raw.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest)).unwrap();
    let out = ok(&e, &["switch", "--to", "claude", "--transcript", zst.to_str().unwrap()], &[]);
    assert!(out.contains("claude ") && out.contains("--mcp-config"), "{out}");
    let r = ok(&e, &["handoff", "render", &handle_in(&out), "--for", "claude"], &[]);
    assert!(r.contains("Add a trigram arm") && r.contains("Trigram arm half done"), "{r}");
}

#[test]
fn sibling_briefs_are_byte_identical_and_reviewers_start_empty() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    ok(&e, &["mem", "post", "Keep SQLite as the system of record", "--kind", "decision"], &[]);
    let a = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "task A"], &[]));
    // A post between siblings must not change the shared prefix.
    ok(&e, &["mem", "post", "Prefer RRF", "--kind", "decision"], &[]);
    let b = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "task B is longer"], &[]));
    let r = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "review it", "--label", "egm-reviewer"], &[]));
    ok(&e, &["wait", &a, &b, &r, "--timeout", "30"], &[]);
    let (pa, pb) = (log(&e, &format!("codex-prompt-{a}")), log(&e, &format!("codex-prompt-{b}")));
    let brief = |p: &str| p.split("task A").next().unwrap().split("task B").next().unwrap().to_string();
    assert!(pa.ends_with("task A\n") || pa.ends_with("task A"), "task must come last: {pa}");
    assert_eq!(brief(&pa), brief(&pb), "sibling dispatch briefs differ");
    let pk = handle_in(&pa);
    assert!(!pa.contains("Keep SQLite"), "brief should carry the packet pointer, not its contents: {pa}");
    let shown = ok(&e, &["handoff", "render", &pk], &[]);
    assert!(shown.contains("Keep SQLite"), "{shown}");
    let pr = log(&e, &format!("codex-prompt-{r}"));
    assert!(!pr.contains("h:") && !pr.contains("foxdb"), "reviewer got context: {pr}");
}

#[test]
fn subagent_start_briefs_stay_identical_across_a_board_post() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
    }
    write(repo.path(), "AGENT_MILESTONE_LOG.md", "# Log\n\n## Current gate\n\n- Gate: HX-1 is active.\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    handoff::post(&c, &handoff::Post { project_id: p.id, kind: "decision", text: "Keep SQLite", ..Default::default() }).unwrap();
    let input = |id: &str| {
        json!({"hook_event_name": "SubagentStart", "session_id": "s1", "cwd": repo.path(), "agent_id": id, "agent_type": "hana-impl"})
            .to_string()
    };
    let a = hook::run("subagent-start", &input("a1"), hook::Harness::Claude);
    handoff::post(&c, &handoff::Post { project_id: p.id, kind: "decision", text: "Prefer RRF", ..Default::default() }).unwrap();
    let b = hook::run("subagent-start", &input("b2"), hook::Harness::Claude);
    assert!(!a.is_empty());
    assert_eq!(a, b);
    // Subagents are registered under the parent session and finished by SubagentStop.
    let tree = agents::list(&c, true, true).unwrap();
    assert!(tree.contains("a1") && tree.contains("s1"), "{tree}");
    let stop = json!({"hook_event_name": "SubagentStop", "session_id": "s1", "cwd": repo.path(), "agent_id": "a1",
        "agent_type": "hana-impl", "last_assistant_message": "Done."})
    .to_string();
    hook::run("subagent-stop", &stop, hook::Harness::Claude);
    assert_eq!(agents::get(&c, "a1").unwrap().status, "done");
}

#[test]
fn pre_compact_saves_a_packet_and_the_compact_session_start_points_at_it() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("FOXDB_HOME", home.path());
        std::env::set_var("FOXDB_CLAUDE_PROJECTS", home.path().join("no-claude"));
        std::env::remove_var("FOXDB_AGENT");
    }
    write(repo.path(), "AGENT_MILESTONE_LOG.md", "# Log\n\n## Current gate\n\n- Gate: PC-1 is active.\n");
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo.path())).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    let tx = repo.path().join("s9.jsonl");
    fs::write(
        &tx,
        format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"Port the lease table"},"sessionId":"s9","cwd":repo.path(),"timestamp":"2026-11-01T10:00:01Z"})
        ),
    )
    .unwrap();
    let ev = |e: &str, extra: serde_json::Value| {
        let mut v = json!({"hook_event_name": e, "session_id": "s9", "cwd": repo.path(), "transcript_path": tx});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v.to_string()
    };
    let plain = hook::run("session-start", &ev("SessionStart", json!({"source": "startup"})), hook::Harness::Claude);
    assert!(!plain.contains("pre-compact packet"));
    assert_eq!(hook::run("pre-compact", &ev("PreCompact", json!({"trigger": "auto"})), hook::Harness::Claude), "");
    let after = hook::run("session-start", &ev("SessionStart", json!({"source": "compact"})), hook::Harness::Claude);
    assert!(after.contains("pre-compact packet h:"), "{after}");
    let v: serde_json::Value = serde_json::from_str(&after).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    let h = ctx.split_whitespace().find(|w| w.starts_with("h:")).unwrap();
    assert!(foxdb::packet::render(&c, handoff::parse_handle(h).unwrap(), 4000).unwrap().contains("Port the lease table"));
    // The session itself was registered by the hook.
    assert_eq!(agents::get(&c, "s9").unwrap().harness, "claude");
}

#[test]
fn stale_agents_are_detected_by_process_and_heartbeat() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id().to_string();
    ok(&e, &["agent", "register", "--id", "proc-1", "--harness", "codex", "--pid", &pid], &[]);
    ok(&e, &["agent", "register", "--id", "live-1", "--harness", "claude", "--pid", &std::process::id().to_string()], &[]);
    ok(&e, &["agent", "register", "--id", "beat-1", "--harness", "opencode", "--role", "worker", "--parent", "live-1"], &[]);
    let l = ok(&e, &["agents"], &[]);
    assert!(l.contains("proc-1") && !l.contains("stale"), "{l}");
    child.kill().unwrap();
    child.wait().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let l = ok(&e, &["agents", "--tree"], &[("FOXDB_STALE_SECS", "1")]);
    let line = |id: &str| l.lines().find(|x| x.contains(id)).unwrap_or_default().to_string();
    assert!(line("proc-1").contains("stale"), "{l}");
    assert!(line("live-1").contains("running"), "{l}");
    assert!(line("beat-1").contains("stale"), "missed heartbeat not stale: {l}");
    assert!(line("beat-1").starts_with(' '), "tree should nest beat-1 under live-1: {l}");
    ok(&e, &["agent", "heartbeat", "beat-1"], &[]);
    let st = ok(&e, &["status", "--agents"], &[("FOXDB_STALE_SECS", "60")]);
    assert!(st.contains("beat-1") && st.contains("live-1") && !st.contains("proc-1"), "{st}");
    // Waiting on a dead agent ends at once instead of hanging, and does not report success.
    let (code, w, _) = fox(&e, &["wait", "proc-1", "--timeout", "5"], e.repo.path(), &[]);
    assert!(w.contains("stale") && code == 4, "rc {code}: {w}");
}

#[test]
fn packets_are_redacted_budgeted_and_carry_no_raw_diff() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    ok(&e, &["mem", "post", "Use key api_key=sk-abcdefghijklmnop1234 for staging\nbecause it is the only one", "--kind", "decision"], &[]);
    write(e.repo.path(), "src/lib.rs", "pub fn alpha() { let s = \"RAW_DIFF_LINE_xyz\"; }\n\npub fn beta() {}\n");
    let out = ok(&e, &["handoff", "create", "--goal", "deploy with password=hunter2hunter2", "--question", "is beta used?"], &[]);
    let h = handle_in(&out);
    let r = ok(&e, &["handoff", "render", &h], &[]);
    assert!(!r.contains("hunter2") && !r.contains("sk-abcdefghijklmnop1234"), "{r}");
    assert!(r.contains("src/lib.rs") && !r.contains("RAW_DIFF_LINE_xyz"), "{r}");
    assert!(r.contains("is beta used?") && r.contains("because it is the only one"), "{r}");
    assert!(r.contains("alpha"), "touched symbols missing: {r}");
    let small = ok(&e, &["handoff", "render", &h, "--budget", "40"], &[]);
    assert!(small.chars().count() <= 40 * 4, "budget ignored: {}", small.chars().count());
    let codex = ok(&e, &["handoff", "render", &h, "--for", "codex", "--budget", "100000"], &[]);
    assert!(codex.chars().count() <= hook::Harness::Codex.cap_chars());
    // Same input, same bytes.
    assert_eq!(r, ok(&e, &["handoff", "render", &h], &[]));
    let (c1, t1, _) = fox(&e, &["handoff", "take", &h, "--by", "a"], e.repo.path(), &[]);
    let (_, t2, _) = fox(&e, &["handoff", "take", &h, "--by", "b"], e.repo.path(), &[]);
    assert!(c1 == 0 && t1.starts_with("claimed") && t2.contains("already_taken by a"), "{t1}{t2}");
}

#[test]
fn board_reads_since_a_cursor_by_topic_without_own_posts() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    ok(&e, &["mem", "post", "cache key needs the generation", "--topic", "search"], &[("FOXDB_AGENT", "a")]);
    ok(&e, &["mem", "post", "lease table added", "--topic", "orch"], &[("FOXDB_AGENT", "a")]);
    let b = [("FOXDB_AGENT", "b")];
    let r = ok(&e, &["mem", "read", "--since", "auto"], &b);
    assert!(r.contains("cache key") && r.contains("lease table") && r.contains("#search"), "{r}");
    let r = ok(&e, &["mem", "read", "--since", "auto"], &b);
    assert!(r.contains("nothing new"), "{r}");
    ok(&e, &["mem", "post", "my own note", "--topic", "search"], &b);
    ok(&e, &["mem", "post", "trigram arm merged", "--topic", "search"], &[("FOXDB_AGENT", "a")]);
    ok(&e, &["mem", "post", "unrelated", "--topic", "orch"], &[("FOXDB_AGENT", "a")]);
    let r = ok(&e, &["mem", "read", "--since", "auto", "--topic", "search"], &b);
    assert!(r.contains("trigram arm merged") && !r.contains("my own note") && !r.contains("unrelated"), "{r}");
    // An explicit cursor is stateless.
    let first = handle_in(&ok(&e, &["mem", "read", "--kind", "finding", "lease table"], &[]));
    let r = ok(&e, &["mem", "read", "--since", &first], &[]);
    assert!(!r.contains("lease table") && r.contains("trigram arm merged"), "{r}");
}

#[test]
fn mcp_exposes_agents_take_packet_and_bounded_wait() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    ok(&e, &["agent", "register", "--id", "w1", "--harness", "codex", "--pid", &std::process::id().to_string()], &[]);
    let pk = handle_in(&ok(&e, &["handoff", "create", "--goal", "Finish M2"], &[]));
    let msgs = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"codex-mcp-client"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"fox_memory","arguments":{"op":"agents"}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fox_memory","arguments":{"op":"packet","id":pk}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fox_memory","arguments":{"op":"take","id":pk}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"fox_memory","arguments":{"op":"wait","id":"w1","timeout":1}}}),
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"fox_memory","arguments":{"op":"post","text":"from mcp","topic":"orch"}}}),
    ];
    let input: String = msgs.iter().map(|m| format!("{m}\n")).collect();
    let path = format!("{}:{}", e.stubs.path().display(), std::env::var("PATH").unwrap_or_default());
    let mut child = Command::new(FOX)
        .arg("mcp")
        .current_dir(e.repo.path())
        .env("PATH", path)
        .env("HOME", e.home.path())
        .env("FOXDB_HOME", e.home.path().join("fx"))
        .env("FOXDB_NO_DAEMON", "1")
        .env("FOXDB_AGENT", "ctl-9")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let t = std::time::Instant::now();
    let out = child.wait_with_output().unwrap();
    assert!(t.elapsed().as_secs() < 10, "MCP wait not bounded");
    let replies: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let text = |i: usize| replies[i]["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    assert!(text(1).contains("w1"), "{}", text(1));
    assert!(text(2).contains("Finish M2"), "{}", text(2));
    assert!(text(3).starts_with("claimed") && text(3).contains("ctl-9"), "identity from FOXDB_AGENT: {}", text(3));
    assert!(text(4).contains("still running"), "{}", text(4));
    let board = ok(&e, &["mem", "read"], &[]);
    assert!(board.contains("ctl-9") && board.contains("from mcp"), "{board}");
}

/// Opt-in: one real `codex exec` through `fox dispatch`, checking the sandbox lets the child reach
/// foxdb through MCP without an approval prompt, and that a real model answers in the report
/// format. Costs a real model call; off unless FOXDB_REAL_SMOKE=1. Uses the real `codex` on PATH
/// with a scratch repo and store.
#[test]
fn real_codex_smoke_opt_in() {
    if std::env::var("FOXDB_REAL_SMOKE").ok().as_deref() != Some("1") {
        return;
    }
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let real_path = std::env::var("PATH").unwrap_or_default();
    let mut cmd = Command::new(FOX);
    cmd.args(["dispatch", "--harness", "codex", "--task", "Call fox_memory op=post with text 'smoke ok', then report status done."])
        .current_dir(e.repo.path())
        .env("PATH", &real_path)
        .env("FOXDB_HOME", e.home.path().join("fx"));
    let o = cmd.output().unwrap();
    let id = first_word(&String::from_utf8_lossy(&o.stdout));
    let w = Command::new(FOX).args(["wait", &id, "--timeout", "280"]).env("FOXDB_HOME", e.home.path().join("fx")).output().unwrap();
    let w = String::from_utf8_lossy(&w.stdout).to_string();
    assert!(w.contains(&format!("{id} done")), "{w}");
    let board =
        Command::new(FOX).args(["mem", "read"]).current_dir(e.repo.path()).env("FOXDB_HOME", e.home.path().join("fx")).output().unwrap();
    let board = String::from_utf8_lossy(&board.stdout).to_string();
    assert!(board.contains("smoke ok"), "the MCP post never landed (approval prompt?): {board}");
    assert!(!report_of(&e, &id).starts_with("malformed report"), "{}", report_of(&e, &id));
}

// ---- review fixes ----

fn store(e: &Env) -> rusqlite::Connection {
    rusqlite::Connection::open(e.home.path().join("fx").join("fox.db")).unwrap()
}

/// The supervisor and child pids of a dispatched worker, once the child has started.
fn pids(e: &Env, id: &str) -> (i32, i32) {
    let c = store(e);
    for _ in 0..100 {
        let r: (Option<i64>, Option<i64>) =
            c.query_row("SELECT pid, child_pid FROM agent WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        if let (Some(p), Some(k)) = r {
            return (p as i32, k as i32);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("{id} never started its child");
}

fn kill(pid: i32) {
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// Run `fox mcp` (optionally under `wrapper`, which then stands for the harness process) on `msgs`.
fn mcp(e: &Env, wrapper: &[&str], extra: &[(&str, &str)], msgs: &[serde_json::Value]) -> Vec<String> {
    let (prog, args): (&str, Vec<&str>) = match wrapper.split_first() {
        Some((p, rest)) => (p, rest.iter().copied().chain([FOX, "mcp"]).collect()),
        None => (FOX, vec!["mcp"]),
    };
    let mut cmd = env_cmd(e, prog, e.repo.path());
    cmd.args(&args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let input: String = msgs.iter().map(|m| format!("{m}\n")).collect();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            let t = v["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
            if v["result"]["isError"] == true { format!("ERR {t}") } else { t }
        })
        .collect()
}

fn init(name: &str) -> serde_json::Value {
    json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"clientInfo":{"name":name}}})
}

fn call(id: i64, args: serde_json::Value) -> serde_json::Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"fox_memory","arguments":args}})
}

#[test]
fn sessions_without_foxdb_agent_get_their_own_identity() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let req = handle_in(&ok(&e, &["mem", "post", "Do the thing", "--kind", "request"], &[]));
    // Two Claude sessions (each `timeout` stands in for its harness process), same client name.
    let a = mcp(
        &e,
        &["timeout", "30"],
        &[],
        &[init("claude-code"), call(1, json!({"op":"take","id":req})), call(2, json!({"op":"post","text":"A found it"}))],
    );
    assert!(a[1].starts_with("claimed"), "{a:?}");
    let b = mcp(
        &e,
        &["timeout", "30"],
        &[],
        &[init("claude-code"), call(1, json!({"op":"take","id":req})), call(2, json!({"op":"read","since":"auto"}))],
    );
    assert!(b[1].contains("already_taken"), "second session took it too: {b:?}");
    assert!(b[2].contains("A found it"), "A's post hidden from B: {b:?}");
    // The CLI: two shells (two `timeout` parents) are two takers.
    let req2 = handle_in(&ok(&e, &["mem", "post", "Another", "--kind", "request"], &[]));
    let take = |w: &str| {
        let mut c = env_cmd(&e, "timeout", e.repo.path());
        c.args([w, FOX, "handoff", "take", &req2]);
        String::from_utf8_lossy(&c.output().unwrap().stdout).into_owned()
    };
    assert!(take("30").starts_with("claimed"));
    assert!(take("31").contains("already_taken"), "two shells shared an identity");
    // A hook-registered session and its MCP server are the same agent.
    let hook_in = e.home.path().join("ss.json");
    fs::write(&hook_in, json!({"session_id":"sess-xyz","cwd":e.repo.path(),"source":"startup"}).to_string()).unwrap();
    let script = format!("{FOX} hook session-start --harness claude < '{}' > /dev/null; {FOX} mcp", hook_in.display());
    let r = mcp(
        &e,
        &["timeout", "30", "sh", "-c", &script],
        &[],
        &[init("claude-code"), call(1, json!({"op":"post","text":"from the hooked session"}))],
    );
    // `mcp` appends `fox mcp` to the wrapper; the script's own `fox mcp` reads stdin first.
    assert!(r.iter().any(|t| t.starts_with("posted")), "{r:?}");
    let board = ok(&e, &["mem", "read"], &[]);
    let line = board.lines().find(|l| l.contains("from the hooked session")).unwrap_or_default().to_string();
    assert!(line.contains("sess-xyz"), "MCP identity differs from the hook's: {board}");
}

#[test]
fn snapshots_are_one_per_owner_under_concurrency() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    ok(&e, &["mem", "post", "Keep SQLite", "--kind", "decision"], &[]);
    for trial in 0..3 {
        let kids: Vec<_> = (0..6)
            .map(|i| {
                let v = json!({"hook_event_name": "SubagentStart", "session_id": format!("par{trial}"), "cwd": e.repo.path(),
                    "agent_type": "hana-impl", "agent_id": format!("k{trial}{i}")});
                let mut c = env_cmd(&e, FOX, e.repo.path());
                c.args(["hook", "subagent-start", "--harness", "claude"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped());
                let mut ch = c.spawn().unwrap();
                use std::io::Write;
                ch.stdin.take().unwrap().write_all(v.to_string().as_bytes()).unwrap();
                ch
            })
            .collect();
        let briefs: Vec<String> =
            kids.into_iter().map(|k| String::from_utf8_lossy(&k.wait_with_output().unwrap().stdout).into_owned()).collect();
        assert!(briefs.iter().all(|b| b == &briefs[0] && !b.is_empty()), "sibling briefs differ: {briefs:#?}");
    }
    // Parallel dispatches from one parent share one pointer too.
    let ids: Vec<String> = {
        let kids: Vec<_> = (0..4)
            .map(|i| {
                let mut c = env_cmd(&e, FOX, e.repo.path());
                c.args(["dispatch", "--harness", "codex", "--task", &format!("t{i}")])
                    .env("FOXDB_AGENT", "ctl2")
                    .stdout(std::process::Stdio::piped());
                c.spawn().unwrap()
            })
            .collect();
        kids.into_iter().map(|k| first_word(&String::from_utf8_lossy(&k.wait_with_output().unwrap().stdout))).collect()
    };
    ok(&e, &["wait", &ids[0], &ids[1], &ids[2], &ids[3], "--timeout", "30"], &[]);
    let ptr: Vec<String> = ids.iter().map(|i| handle_in(&log(&e, &format!("codex-prompt-{i}")))).collect();
    assert!(ptr.iter().all(|p| p == &ptr[0]), "{ptr:?}");
}

#[test]
fn a_writer_lives_while_its_child_runs_after_the_supervisor_dies() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let w = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "W job", "--writer"], &[("STUB_SLEEP", "20")]));
    let (sup, child) = pids(&e, &w);
    kill(sup);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let l = ok(&e, &["agents"], &[]);
    assert!(l.lines().any(|x| x.starts_with(&w) && x.contains("running")), "{l}");
    assert!(ok(&e, &["lease", "list"], &[]).contains(&w), "lease dropped while the child still writes");
    let (code, _, err) = fox(&e, &["dispatch", "--harness", "codex", "--task", "second", "--writer"], e.repo.path(), &[]);
    assert!(code != 0 && err.contains(&format!("held by {w}")), "second writer admitted: {err}");
    kill(-sup); // the rest of the worker's process group: the child and its sleep
    kill(child);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let l = ok(&e, &["agents"], &[]);
    assert!(l.lines().any(|x| x.starts_with(&w) && x.contains("stale")), "{l}");
    assert!(!ok(&e, &["lease", "list"], &[]).contains(&w));
}

#[test]
fn lease_take_revives_a_finished_agent() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let ctl = [("FOXDB_AGENT", "ctl")];
    ok(&e, &["agent", "register"], &ctl);
    ok(&e, &["agent", "done"], &ctl);
    let t = ok(&e, &["lease", "take"], &ctl);
    assert!(t.contains("held by ctl"), "{t}");
    assert!(ok(&e, &["lease", "list"], &[]).contains("ctl"), "lease void at once");
    let (code, _, err) = fox(&e, &["dispatch", "--harness", "codex", "--task", "x", "--writer"], e.repo.path(), &[]);
    assert!(code != 0 && err.contains("held by ctl"), "{err}");
}

#[test]
fn read_only_mcp_paths_migrate_an_old_store() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    ok(&e, &["mem", "post", "old board row"], &[]);
    let c = store(&e);
    c.execute_batch(
        "DROP INDEX IF EXISTS handoff_parent; ALTER TABLE handoff DROP COLUMN topic; ALTER TABLE handoff DROP COLUMN parent_id;
         ALTER TABLE handoff DROP COLUMN lease_until; DROP TABLE agent; DROP TABLE lease; DROP TABLE read_cursor; PRAGMA user_version = 5;",
    )
    .unwrap();
    drop(c);
    let r = mcp(&e, &[], &[], &[init("codex-mcp-client"), call(1, json!({"op":"read"}))]);
    assert!(!r[1].starts_with("ERR") && r[1].contains("old board row"), "{r:?}");
    let v: i64 = store(&e).query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert!(v >= 6, "store not migrated: {v}");
}

#[test]
fn waiting_on_a_handoff_ends_when_its_workers_die() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let req = handle_in(&ok(&e, &["mem", "post", "Long job", "--kind", "request"], &[]));
    let id = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", &req], &[("STUB_SLEEP", "30")]));
    let (sup, child) = pids(&e, &id);
    kill(-sup);
    kill(child);
    let t = std::time::Instant::now();
    let (code, out, _) = fox(&e, &["wait", &req, "--timeout", "20"], e.repo.path(), &[]);
    assert!(t.elapsed().as_secs() < 10, "wait hung on a dead worker: {out}");
    assert!(code == 4 && out.contains(&id) && out.contains("stale"), "rc {code}: {out}");
}

#[test]
fn switch_to_claude_passes_the_prompt_and_an_identity() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let pk = handle_in(&ok(&e, &["handoff", "create", "--goal", "Ship it"], &[]));
    let out = ok(&e, &["switch", "--to", "claude", "--from", "codex", "--handoff", &pk], &[]);
    let (p, t) = (out.find("Continue the task").unwrap(), out.find("--allowedTools").unwrap());
    assert!(p < t, "prompt after the variadic --allowedTools: {out}");
    assert!(out.contains("FOXDB_AGENT=cl-") && out.contains("FOXDB_HOME="), "{out}");
    // --exec: the stub sees the prompt as its positional argument and runs as a registered agent.
    ok(&e, &["switch", "--to", "claude", "--from", "codex", "--handoff", &pk, "--exec"], &[("FOXDB_AGENT", "cx-src")]);
    let f = fs::read_dir(e.log.path())
        .unwrap()
        .flatten()
        .map(|f| f.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("claude-args-cl-"));
    let f = f.expect("switched session had no FOXDB_AGENT");
    let args = log(&e, &f);
    let after: Vec<&str> = args.lines().skip_while(|l| *l != "mcp__foxdb__*").skip(1).collect();
    assert!(after.iter().all(|l| l.starts_with("--")), "prompt parsed as a tool pattern: {args}");
    let id = f.trim_start_matches("claude-args-");
    let tree = ok(&e, &["agents", "--tree", "--all"], &[]);
    assert!(tree.contains(id) && tree.contains("cx-src"), "{tree}");
    // Rerunning a switch from one transcript reuses its packet.
    let tx = e.home.path().join("5e55-1.jsonl");
    fs::write(
        &tx,
        format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"Split search.rs"},"sessionId":"5e55-1","cwd":e.repo.path(),"timestamp":"2026-11-01T10:00:01Z"})
        ),
    )
    .unwrap();
    let a = handle_in(&ok(&e, &["switch", "--to", "codex", "--transcript", tx.to_str().unwrap()], &[]));
    let b = handle_in(&ok(&e, &["switch", "--to", "codex", "--transcript", tx.to_str().unwrap()], &[]));
    assert_eq!(a, b);
}

#[test]
fn workers_cannot_escalate_or_recurse_without_bound() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let w = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "read only job"], &[]));
    ok(&e, &["wait", &w, "--timeout", "30"], &[]);
    let me = [("FOXDB_AGENT", w.as_str())];
    let r = mcp(&e, &[], &me, &[init("codex-mcp-client"), call(1, json!({"op":"dispatch","harness":"codex","text":"edit","writer":true}))]);
    assert!(r[1].starts_with("ERR") && r[1].contains("writer"), "read-only worker started a writer: {r:?}");
    let r = mcp(
        &e,
        &[],
        &[("FOXDB_AGENT", w.as_str()), ("FOXDB_MAX_DEPTH", "1")],
        &[init("codex-mcp-client"), call(1, json!({"op":"dispatch","harness":"codex","text":"look"}))],
    );
    assert!(r[1].starts_with("ERR") && r[1].contains("depth"), "{r:?}");
    let rv = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Review it", "--label", "code-reviewer"], &[]));
    let r = mcp(
        &e,
        &[],
        &[("FOXDB_AGENT", rv.as_str())],
        &[init("codex-mcp-client"), call(1, json!({"op":"dispatch","harness":"codex","text":"look"}))],
    );
    assert!(r[1].starts_with("ERR") && r[1].contains("reviewer"), "{r:?}");
    // At most FOXDB_MAX_CHILDREN running children per parent.
    let cap = [("FOXDB_AGENT", "ctl"), ("FOXDB_MAX_CHILDREN", "1"), ("STUB_SLEEP", "5")];
    let first = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "one"], &cap));
    let (code, _, err) = fox(&e, &["dispatch", "--harness", "codex", "--task", "two"], e.repo.path(), &cap);
    assert!(code != 0 && err.contains("running children"), "{err}");
    let (sup, _) = pids(&e, &first);
    kill(-sup);
}

#[test]
fn a_dispatched_reviewer_gets_no_session_start_brief() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let rv = first_word(&ok(&e, &["dispatch", "--harness", "claude", "--task", "Review src/lib.rs", "--label", "code-reviewer"], &[]));
    let input = json!({"session_id":"rs-1","cwd":e.repo.path(),"source":"startup"}).to_string();
    let run = |agent: Option<&str>| {
        let mut c = env_cmd(&e, FOX, e.repo.path());
        c.args(["hook", "session-start", "--harness", "claude"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
        if let Some(a) = agent {
            c.env("FOXDB_AGENT", a);
        }
        let mut ch = c.spawn().unwrap();
        use std::io::Write;
        ch.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        String::from_utf8_lossy(&ch.wait_with_output().unwrap().stdout).into_owned()
    };
    assert!(run(None).contains("OR-2"), "plain session lost its brief");
    let r = run(Some(&rv));
    assert!(!r.contains("OR-2") && !r.contains("additionalContext"), "reviewer got the brief: {r}");
}

#[test]
fn a_stale_codex_worker_resumes_its_own_thread_and_its_packet_says_what_it_did() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    ok(&e, &["mem", "post", "unrelated board post"], &[("FOXDB_AGENT", "someone-else")]);
    let done = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Quick job"], &[("FOXDB_AGENT", "ctl")]));
    ok(&e, &["wait", &done, "--timeout", "30"], &[]);
    let id = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Long refactor of search"], &[("STUB_SLEEP", "30")]));
    let (sup, child) = pids(&e, &id);
    let out = e.home.path().join("fx/agents").join(&id).join("out.jsonl");
    for _ in 0..100 {
        if fs::read_to_string(&out).unwrap_or_default().contains("thread.started") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    kill(-sup);
    kill(child);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let sw = ok(&e, &["switch", "--to", "codex", "--from", "codex", "--agent", &id], &[]);
    assert!(sw.contains(&format!("resume th-{id}")) && !sw.contains("--last"), "{sw}");
    let pk = handle_in(&ok(&e, &["handoff", "create", "--agent", &id], &[]));
    let r = ok(&e, &["handoff", "render", &pk], &[]);
    assert!(r.contains("Long refactor of search") && !r.contains("(unstated)"), "{r}");
    // Evidence is this agent's tree, not the project's latest rows.
    let ctl_pk = handle_in(&ok(&e, &["handoff", "create", "--agent", "ctl"], &[]));
    let r = ok(&e, &["handoff", "render", &ctl_pk], &[]);
    let ev = r.lines().find(|l| l.starts_with("evidence:")).unwrap_or_default().to_string();
    let board = ok(&e, &["mem", "read"], &[]);
    let unrelated = handle_in(board.lines().find(|l| l.contains("unrelated board post")).unwrap());
    assert!(ev.contains("report") && !ev.contains(&format!("{unrelated} ")), "{ev}");
}

#[test]
fn dispatch_points_children_at_the_parents_latest_packet() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let ctl = [("FOXDB_AGENT", "ctl")];
    let a = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "first"], &ctl));
    ok(&e, &["mem", "post", "NEW decision", "--kind", "decision"], &ctl);
    let pk = handle_in(&ok(&e, &["handoff", "create", "--goal", "ctl real packet"], &ctl));
    let b = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "second"], &ctl));
    ok(&e, &["wait", &a, &b, "--timeout", "30"], &[]);
    let pb = log(&e, &format!("codex-prompt-{b}"));
    assert!(pb.contains(&format!("parent packet {pk} ")), "child not pointed at the parent's packet {pk}: {pb}");
    // Without FOXDB_AGENT too: the shell's own packet reaches its workers.
    let pk2 = handle_in(&ok(&e, &["handoff", "create", "--goal", "anon packet"], &[]));
    let c = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "third"], &[]));
    ok(&e, &["wait", &c, "--timeout", "30"], &[]);
    assert!(log(&e, &format!("codex-prompt-{c}")).contains(&format!("parent packet {pk2} ")));
}

#[test]
fn a_new_reader_starts_at_the_newest_board_rows() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    for i in 1..=40 {
        ok(&e, &["mem", "post", &format!("old finding number {i}")], &[("FOXDB_AGENT", "a")]);
    }
    ok(&e, &["mem", "post", "FRESH decision", "--kind", "decision"], &[("FOXDB_AGENT", "a")]);
    let r = ok(&e, &["mem", "read", "--since", "auto", "--budget", "100"], &[("FOXDB_AGENT", "child2")]);
    assert!(r.contains("FRESH decision") && !r.contains("old finding number 1 "), "{r}");
    let r = ok(&e, &["mem", "read", "--since", "auto"], &[("FOXDB_AGENT", "child2")]);
    assert!(r.contains("nothing new"), "{r}");
}

// ---- milestone 3: verifiable reports ----

fn git_out(repo: &Path, args: &[&str]) -> String {
    let o = Command::new("git").args(args).current_dir(repo).env("HOME", repo).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn commits(e: &Env, id: &str) -> (String, String) {
    let c = store(e);
    let (b, h): (Option<String>, Option<String>) =
        c.query_row("SELECT base_commit, head_commit FROM agent WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    (b.unwrap_or_default(), h.unwrap_or_default())
}

fn report_of(e: &Env, id: &str) -> String {
    let c = store(e);
    c.query_row("SELECT h.body FROM agent a JOIN handoff h ON h.id = a.report WHERE a.id = ?1", [id], |r| r.get(0)).unwrap()
}

/// Every worker records the tree it started from and the tree it left, as commit ids, so the
/// report points at an exact `git diff base head`. Untracked files count; the controller's own
/// uncommitted edits sit in the base, not in the worker's change; the user's index, HEAD and refs
/// are untouched.
#[test]
fn workers_record_base_and_head_commits_including_untracked_files() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let r = e.repo.path();
    write(r, "src/lib.rs", "pub fn alpha() {}\n\npub fn beta() {}\n// controller edit, uncommitted\n");
    let head0 = git_out(r, &["rev-parse", "HEAD"]);
    let refs0 = git_out(r, &["for-each-ref"]);
    let id = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Add gamma", "--writer"], &[("STUB_WRITE", "src/gamma.rs")]));
    ok(&e, &["wait", &id, "--timeout", "30"], &[]);
    let (base, head) = commits(&e, &id);
    assert!(base.len() == 40 && head.len() == 40 && base != head, "base {base} head {head}");
    assert_eq!(git_out(r, &["diff", "--name-only", &base, &head]), "src/gamma.rs");
    let rep = report_of(&e, &id);
    assert!(rep.contains(&format!("change: git diff {base} {head}")), "{rep}");
    assert_eq!(git_out(r, &["rev-parse", "HEAD"]), head0);
    assert_eq!(git_out(r, &["for-each-ref"]), refs0);
    let st = git_out(r, &["status", "--porcelain"]);
    assert!(st.contains("?? src/gamma.rs") && st.contains("M src/lib.rs") && !st.contains("A "), "index touched: {st}");
    // A read-only worker that changes nothing: base = head, and the packet names the change.
    let rd = first_word(&ok(&e, &["dispatch", "--harness", "claude", "--task", "Look around"], &[]));
    ok(&e, &["wait", &rd, "--timeout", "30"], &[]);
    let (b2, h2) = commits(&e, &rd);
    assert_eq!(b2, h2);
    let pk = handle_in(&ok(&e, &["handoff", "create", "--agent", &id], &[]));
    let shown = ok(&e, &["handoff", "render", &pk], &[]);
    assert!(shown.contains(&format!("change: git diff {base} {head}")), "{shown}");
}

/// Reports have one fixed format, enforced by the harness (`--output-schema`, `--json-schema`)
/// and checked again on arrival. A malformed report fails the worker instead of being guessed at.
#[test]
fn reports_are_schema_checked_and_malformed_ones_fail() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let cx = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "One"], &[]));
    let cl = first_word(&ok(&e, &["dispatch", "--harness", "claude", "--task", "Two"], &[]));
    ok(&e, &["wait", &cx, &cl, "--timeout", "30"], &[]);
    assert_eq!(log(&e, &format!("codex-schema-{cx}")), foxdb::report::SCHEMA);
    let a = log(&e, &format!("claude-args-{cl}"));
    let lines: Vec<&str> = a.lines().collect();
    let i = lines.iter().position(|l| *l == "--json-schema").expect("claude gets --json-schema");
    assert_eq!(lines[i + 1], foxdb::report::SCHEMA);
    let rep = report_of(&e, &cx);
    assert!(rep.starts_with("done: codex finished") && rep.contains("tests: none run"), "{rep}");
    assert!(report_of(&e, &cl).starts_with("done: claude finished"));

    for bad in [
        "just prose, no JSON",
        r#"{"status":"done","summary":"x"}"#,
        r#"{"status":"maybe","summary":"x","files":[],"tests":{"ran":[],"passed":false},"decisions":[],"next":[]}"#,
    ] {
        let id = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Bad"], &[("STUB_REPORT", bad)]));
        let (code, out, _) = fox(&e, &["wait", &id, "--timeout", "30"], e.repo.path(), &[]);
        assert_eq!(code, 4, "a malformed report must fail the wait: {out}");
        assert!(out.contains(&format!("{id} failed")), "{out}");
        let rep = report_of(&e, &id);
        assert!(rep.starts_with("malformed report:"), "{rep}");
    }
}

/// Cheap rule checks on each report, no model calls: a writer that changed nothing, tests
/// claimed to pass with none run, a test command the event log never shows.
#[test]
fn report_checks_flag_empty_writers_and_unrun_tests() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let claim = |ran: &str| {
        format!(
            r#"{{"status":"done","summary":"all good","files":["src/lib.rs:1"],"tests":{{"ran":[{ran}],"passed":true}},"decisions":[],"next":[]}}"#
        )
    };
    let w = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Edit", "--writer"], &[("STUB_REPORT", &claim(""))]));
    let (code, out, _) = fox(&e, &["wait", &w, "--timeout", "30"], e.repo.path(), &[]);
    assert_eq!(code, 4, "a flagged report must fail the wait: {out}");
    let rep = report_of(&e, &w);
    assert!(rep.contains("check: writer changed nothing"), "{rep}");
    assert!(rep.contains("check: claims tests pass but ran none"), "{rep}");
    assert!(out.contains("checks"), "wait line should flag checks: {out}");
    let r1 = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Test"], &[("STUB_REPORT", &claim(r#""cargo test""#))]));
    fox(&e, &["wait", &r1, "--timeout", "30"], e.repo.path(), &[]);
    assert!(report_of(&e, &r1).contains("check: claims it ran `cargo test`"), "{}", report_of(&e, &r1));
    let r2 = first_word(&ok(
        &e,
        &["dispatch", "--harness", "codex", "--task", "Test"],
        &[("STUB_REPORT", &claim(r#""cargo test""#)), ("STUB_RAN", "cargo test --release")],
    ));
    ok(&e, &["wait", &r2, "--timeout", "30"], &[]);
    assert!(!report_of(&e, &r2).contains("check:"), "{}", report_of(&e, &r2));
}

/// MCP `op=wait` with `any`, and a reviewer requested over MCP dispatch.
#[test]
fn mcp_wait_any_and_reviewer_dispatch() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let slow = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Slow"], &[("STUB_SLEEP", "30")]));
    let fast = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Fast"], &[]));
    let t = std::time::Instant::now();
    let out = mcp(
        &e,
        &[],
        &[("FOXDB_AGENT", "ctl")],
        &[
            init("claude-code"),
            call(1, json!({"op": "wait", "id": format!("{slow} {fast}"), "any": true, "timeout": 20})),
            call(2, json!({"op": "dispatch", "harness": "codex", "text": "Review the diff", "reviewer": true})),
        ],
    );
    assert!(t.elapsed().as_secs() < 15, "wait any blocked on the slow worker");
    assert!(out[1].contains(&format!("{fast} done")) && !out[1].contains("still running"), "{out:?}");
    let rv = first_word(&out[2]);
    assert!(out[2].contains("reviewer"), "{out:?}");
    ok(&e, &["wait", &rv, "--timeout", "30"], &[]);
    let prompt = log(&e, &format!("codex-prompt-{rv}"));
    assert_eq!(prompt.trim(), "Review the diff", "a reviewer gets the task alone");
    let (sup, child) = pids(&e, &slow);
    kill(child);
    kill(sup);
}

/// A failed report does not close its handoff: a retry worker can take it, and `fox wait h:N`
/// fails on the failed report but waits for a retry that is still running.
#[test]
fn a_failed_report_leaves_the_handoff_open_for_a_retry() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let req = handle_in(&ok(&e, &["mem", "post", "Fix the thing", "--kind", "request"], &[]));
    let bad = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", &req], &[("STUB_REPORT", "prose only")]));
    let (code, out, _) = fox(&e, &["wait", &bad, "--timeout", "30"], e.repo.path(), &[]);
    assert_eq!(code, 4, "{out}");
    let (code, out, _) = fox(&e, &["wait", &req, "--timeout", "30"], e.repo.path(), &[]);
    assert_eq!(code, 4, "waiting on a handoff whose only report failed must fail: {out}");
    assert!(out.contains(&format!("{bad} failed")), "{out}");
    let took = ok(&e, &["handoff", "take", &req, "--by", "retry-worker"], &[]);
    assert!(took.starts_with("claimed"), "a failed report must not mark the handoff done: {took}");
    let retry = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", &req], &[("STUB_SLEEP", "3")]));
    let t = std::time::Instant::now();
    let (code, out, _) = fox(&e, &["wait", &req, "--timeout", "30"], e.repo.path(), &[]);
    assert!(t.elapsed().as_secs_f64() > 1.5, "wait returned the old failed report while the retry ran: {out}");
    assert!(code == 0 && out.contains(&format!("by {retry}")), "rc {code}: {out}");
    let took = ok(&e, &["handoff", "take", &req, "--by", "third"], &[]);
    assert!(took.contains("already_done"), "{took}");
}

/// A worker's own status counts: `blocked` and `partial` reports, and reports with check
/// findings, end as that status and make `fox wait` exit 4, so `fox wait $W && merge` stops.
#[test]
fn blocked_partial_and_flagged_reports_fail_the_wait() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let rep = |status: &str| {
        format!(
            r#"{{"status":"{status}","summary":"could not build","files":[],"tests":{{"ran":[],"passed":false}},"decisions":[],"next":[]}}"#
        )
    };
    for st in ["blocked", "partial"] {
        let id = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Build"], &[("STUB_REPORT", &rep(st))]));
        let (code, out, _) = fox(&e, &["wait", &id, "--timeout", "30"], e.repo.path(), &[]);
        assert!(code == 4 && out.contains(&format!("{id} {st}")), "rc {code}: {out}");
    }
    let id = first_word(&ok(&e, &["dispatch", "--harness", "codex", "--task", "Edit", "--writer"], &[("STUB_REPORT", &rep("done"))]));
    let (code, out, _) = fox(&e, &["wait", &id, "--timeout", "30"], e.repo.path(), &[]);
    assert!(code == 4 && out.contains(&format!("{id} flagged")) && out.contains("checks"), "rc {code}: {out}");
}

/// A worker that fails still records where it left the tree, so its partial work is addressable.
#[test]
fn a_failed_writer_still_records_its_head_commit() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let id = first_word(&ok(
        &e,
        &["dispatch", "--harness", "codex", "--task", "Edit", "--writer"],
        &[("STUB_REPORT", "prose"), ("STUB_WRITE", "src/z.rs")],
    ));
    fox(&e, &["wait", &id, "--timeout", "30"], e.repo.path(), &[]);
    let (base, head) = commits(&e, &id);
    assert!(base.len() == 40 && head.len() == 40, "base {base} head {head}");
    assert_eq!(git_out(e.repo.path(), &["diff", "--name-only", &base, &head]), "src/z.rs");
    assert!(report_of(&e, &id).contains(&format!("change: git diff {base} {head}")), "{}", report_of(&e, &id));
}

/// `--for t3` only picks a cap: the same packet, short enough to sit beside T3 Code's own
/// portable handoff; `fox switch --for t3` prints the message for a T3 thread, not a command.
#[test]
fn for_t3_caps_the_packet_and_switch_prints_a_message_to_paste() {
    let _g = SERIAL.lock().unwrap_or_else(|x| x.into_inner());
    let e = setup();
    let long: String = (0..80).map(|i| format!("Ship the trigram arm, detail line {i}\n")).collect();
    let pk = handle_in(&ok(&e, &["mem", "post", &long, "--kind", "finding", "--title", "Ship the trigram arm"], &[]));
    let plain = ok(&e, &["handoff", "render", &pk, "--for", "plain"], &[]);
    let t3 = ok(&e, &["handoff", "render", &pk, "--for", "t3"], &[]);
    assert!(t3.len() <= foxdb::packet::T3_CAP_CHARS + 1 && t3.len() < plain.len(), "{} vs {}", t3.len(), plain.len());
    assert_eq!(t3.lines().next(), plain.lines().next(), "one format, only the cap differs");
    let sw = ok(&e, &["switch", "--for", "t3", "--handoff", &pk], &[]);
    assert!(sw.contains("paste into a T3 Code thread") && sw.contains(&format!("fox_read target={pk}")), "{sw}");
    assert!(!sw.contains("--mcp-config") && !sw.contains("mcp_servers.foxdb"), "{sw}");
    let inline = ok(&e, &["switch", "--for", "t3", "--handoff", &pk, "--inline"], &[]);
    assert!(inline.contains("Ship the trigram arm") && inline.len() < plain.len() + 300, "{inline}");
}
