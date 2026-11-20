//! `fox swarm`: plans of dispatched tasks with dependencies, a concurrency cap, writer leases,
//! a summary handoff, a reviewer panel with a majority vote, and crash-safe re-runs. Workers
//! are stub `codex` and `claude` scripts: no model is ever called.

use std::fs;
use std::process::Command;

const FOX: &str = env!("CARGO_BIN_EXE_fox");

/// A stub harness for both names. It logs `start`/`end` lines with nanosecond times, sleeps
/// `STUB_SLEEP` (`STUB_SLEEP_CLAUDE` instead, when set and run as `claude`), fails (no report) when its prompt contains `STUB_FAIL`, and reports
/// `blocked` when its prompt contains `STUB_REJECT`.
const STUB: &str = r#"#!/bin/sh
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
prompt=$(cat)
printf '%s\n' "$prompt" > "$STUB_LOG/prompt-$FOXDB_AGENT"
echo "start $FOXDB_AGENT $(perl -MTime::HiRes=time -e 'printf "%.0f", time() * 1e9')" >> "$STUB_LOG/times"
nap="$STUB_SLEEP"
case "$0" in *claude) [ -n "$STUB_SLEEP_CLAUDE" ] && nap="$STUB_SLEEP_CLAUDE";; esac
[ -n "$nap" ] && sleep "$nap"
echo "end $FOXDB_AGENT $(perl -MTime::HiRes=time -e 'printf "%.0f", time() * 1e9')" >> "$STUB_LOG/times"
case "$prompt" in *"$STUB_FAIL"*) [ -n "$STUB_FAIL" ] && exit 1;; esac
st=done
case "$prompt" in *"$STUB_REJECT"*) [ -n "$STUB_REJECT" ] && st=blocked;; esac
rep='{"status":"'"$st"'","summary":"stub '"$FOXDB_AGENT"' finished","files":[],"tests":{"ran":[],"passed":false},"decisions":[],"next":[]}'
if [ -n "$out" ]; then
  echo '{"type":"thread.started","thread_id":"th-'"$FOXDB_AGENT"'"}'
  echo '{"type":"turn.completed"}'
  printf '%s\n' "$rep" > "$out"
else
  printf '{"type":"result","subtype":"success","is_error":false,"result":"done","structured_output":%s,"session_id":"s"}\n' "$rep"
fi
"#;

struct Env {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    stubs: tempfile::TempDir,
    log: tempfile::TempDir,
}

fn setup() -> Env {
    let e = Env {
        home: tempfile::tempdir().unwrap(),
        repo: tempfile::tempdir().unwrap(),
        stubs: tempfile::tempdir().unwrap(),
        log: tempfile::tempdir().unwrap(),
    };
    for n in ["codex", "claude"] {
        let p = e.stubs.path().join(n);
        fs::write(&p, STUB).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::create_dir_all(e.repo.path().join("src")).unwrap();
    fs::write(e.repo.path().join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();
    let (c, out, err) = fox(&e, &["index", "--no-global", "--quiet"], &[]);
    assert_eq!(c, 0, "{out}{err}");
    e
}

fn fox(e: &Env, args: &[&str], extra: &[(&str, &str)]) -> (i32, String, String) {
    let o = cmd(e, args, extra).output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

fn cmd(e: &Env, args: &[&str], extra: &[(&str, &str)]) -> Command {
    let path = format!("{}:{}", e.stubs.path().display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = Command::new(FOX);
    cmd.args(args)
        .current_dir(e.repo.path())
        .env("PATH", path)
        .env("HOME", e.home.path())
        .env("FOXDB_HOME", e.home.path().join("fx"))
        .env("FOXDB_NO_DAEMON", "1")
        .env("FOXDB_CLAUDE_PROJECTS", e.home.path().join("no-claude"))
        .env("FOXDB_CODEX_SESSIONS", e.home.path().join("no-codex"))
        .env("STUB_LOG", e.log.path())
        .env_remove("FOXDB_AGENT")
        .env_remove("FOXDB_PARENT");
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    cmd
}

fn plan(e: &Env, name: &str, body: &str) -> String {
    let p = e.repo.path().join(name);
    fs::write(&p, body).unwrap();
    p.to_string_lossy().into_owned()
}

/// (agent, start ns, end ns) per worker run, from the stub's log.
fn spans(e: &Env) -> Vec<(String, u128, u128)> {
    let text = fs::read_to_string(e.log.path().join("times")).unwrap_or_default();
    let mut starts = std::collections::HashMap::new();
    let mut out = vec![];
    for l in text.lines() {
        let v: Vec<&str> = l.split_whitespace().collect();
        let t: u128 = v[2].parse().unwrap();
        if v[0] == "start" {
            starts.insert(v[1].to_string(), t);
        } else {
            out.push((v[1].to_string(), starts[v[1]], t));
        }
    }
    out
}

fn most_at_once(s: &[(String, u128, u128)]) -> usize {
    s.iter().map(|a| s.iter().filter(|b| b.1 <= a.1 && a.1 < b.2).count()).max().unwrap_or(0)
}

fn agent_count(e: &Env) -> usize {
    let (_, out, _) = fox(e, &["agents", "--all"], &[]);
    out.lines().filter(|l| l.starts_with("cx-") || l.starts_with("cl-")).count()
}

const THREE: &str = r#"
name = "trio"
concurrency = 2

[[task]]
id = "scan"
harness = "codex"
task = "scan the handlers"

[[task]]
id = "audit"
harness = "claude"
task = "audit the config"

[[task]]
id = "merge"
harness = "codex"
task = "merge the findings"
depends_on = ["scan", "audit"]
"#;

#[test]
fn a_plan_runs_in_dependency_order_under_its_cap_and_collects_one_summary() {
    let e = setup();
    let p = plan(&e, "trio.toml", THREE);
    let (c, out, err) = fox(&e, &["swarm", "run", &p], &[("STUB_SLEEP", "1")]);
    assert_eq!(c, 0, "{out}{err}");
    assert!(out.contains("3 tasks, 3 ok"), "{out}");
    assert!(out.contains("summary h:"), "{out}");
    let s = spans(&e);
    assert_eq!(s.len(), 3, "{s:?}");
    assert!(most_at_once(&s) <= 2, "{s:?}");
    // `merge` started after both of its dependencies ended, and got their report handles.
    let merge = s.iter().max_by_key(|x| x.1).unwrap();
    assert!(s.iter().filter(|x| x.0 != merge.0).all(|x| x.2 <= merge.1), "{s:?}");
    let prompt = fs::read_to_string(e.log.path().join(format!("prompt-{}", merge.0))).unwrap();
    assert!(prompt.contains("- scan: h:") && prompt.contains("- audit: h:"), "{prompt}");
    // The summary lists every task with its report.
    let h = out.split("summary ").nth(1).unwrap().split_whitespace().next().unwrap();
    let (_, shown, _) = fox(&e, &["handoff", "render", h, "--for", "plain"], &[]);
    for t in ["scan [done]", "audit [done]", "merge [done]"] {
        assert!(shown.contains(t), "{shown}");
    }
    // A second run is a no-op: same summary, no new workers.
    let (c, again, _) = fox(&e, &["swarm", "run", &p], &[]);
    assert_eq!(c, 0);
    assert_eq!(again, out);
    assert_eq!(agent_count(&e), 3);
    let (_, st, _) = fox(&e, &["swarm", "status", "trio"], &[]);
    assert_eq!(st, out);
}

#[test]
fn a_rerun_after_a_crash_adopts_started_workers_instead_of_dispatching_again() {
    let e = setup();
    let p = plan(&e, "trio.toml", THREE);
    // Start the first wave and stop, as a crashed controller would.
    let (c, out, err) = fox(&e, &["swarm", "run", &p, "--no-wait"], &[("STUB_SLEEP", "1")]);
    assert_eq!(c, 3, "{out}{err}");
    assert_eq!(agent_count(&e), 2);
    // Lose the swarm's own bookkeeping too: the workers' keys still find them.
    let db = e.home.path().join("fx/fox.db");
    let w = rusqlite::Connection::open(&db).unwrap();
    w.execute("DELETE FROM swarm_task", []).unwrap();
    drop(w);
    let (c, out, err) = fox(&e, &["swarm", "run", &p], &[("STUB_SLEEP", "1")]);
    assert_eq!(c, 0, "{out}{err}");
    assert!(out.contains("3 ok"), "{out}");
    assert_eq!(agent_count(&e), 3, "each task dispatched exactly once");
    assert_eq!(spans(&e).len(), 3);
}

#[test]
fn a_failed_task_skips_its_dependents_and_the_run_reports_it() {
    let e = setup();
    let p = plan(&e, "trio.toml", THREE);
    let (c, out, _) = fox(&e, &["swarm", "run", &p], &[("STUB_FAIL", "scan the handlers")]);
    assert_eq!(c, 4, "{out}");
    assert!(out.contains("  merge skipped"), "{out}");
    assert!(out.contains("audit") && out.contains("done"), "{out}");
}

#[test]
fn writer_tasks_on_one_repo_never_overlap() {
    let e = setup();
    let p = plan(
        &e,
        "writers.toml",
        r#"
concurrency = 3
[[task]]
id = "w1"
harness = "codex"
task = "edit one"
writer = true
[[task]]
id = "w2"
harness = "codex"
task = "edit two"
writer = true
[[task]]
id = "r1"
harness = "claude"
task = "read along"
"#,
    );
    let (c, out, err) = fox(&e, &["swarm", "run", &p], &[("STUB_SLEEP", "1")]);
    assert_eq!(c, 0, "{out}{err}");
    let s = spans(&e);
    let ws: Vec<_> = s.iter().filter(|x| x.0.starts_with("cx-")).collect();
    assert_eq!(ws.len(), 2);
    assert!(ws[0].2 <= ws[1].1 || ws[1].2 <= ws[0].1, "writers overlapped: {s:?}");
    // The reader ran beside a writer.
    assert_eq!(most_at_once(&s), 2, "{s:?}");
}

#[test]
fn a_reviewer_panel_votes_by_majority_once() {
    let e = setup();
    let body = r#"
name = "reviewed"
[[task]]
id = "work"
harness = "codex"
task = "do the work"
[review]
harnesses = ["claude", "codex", "claude"]
task = "Check the work. PANEL"
"#;
    let p = plan(&e, "reviewed.toml", body);
    let (c, out, err) = fox(&e, &["swarm", "run", &p], &[]);
    assert_eq!(c, 0, "{out}{err}");
    assert!(out.contains("review: approved 3/3"), "{out}");
    // The reviewers were started as reviewers: the task only, pointing at the summary.
    let reviewers: Vec<String> = fs::read_dir(e.log.path())
        .unwrap()
        .flatten()
        .map(|f| fs::read_to_string(f.path()).unwrap_or_default())
        .filter(|t| t.contains("PANEL"))
        .collect();
    assert_eq!(reviewers.len(), 3);
    assert!(reviewers.iter().all(|t| t.starts_with("Check the work.") && t.contains("fox_read target=h:")));

    // A second swarm where two of three reject.
    let e2 = setup();
    let p2 = plan(&e2, "reviewed.toml", &body.replace("name = \"reviewed\"", "name = \"rejected\""));
    let (c, out, _) = fox(&e2, &["swarm", "run", &p2], &[("STUB_REJECT", "PANEL")]);
    assert_eq!(c, 4, "{out}");
    assert!(out.contains("review: rejected 0/3"), "{out}");
}

#[test]
fn bad_plans_are_refused_before_anything_starts() {
    let e = setup();
    for (body, want) in [
        ("[[task]]\nid=\"a\"\nharness=\"codex\"\ntask=\"x\"\ndepends_on=[\"b\"]\n", "unknown dependency"),
        (
            "[[task]]\nid=\"a\"\nharness=\"codex\"\ntask=\"x\"\ndepends_on=[\"b\"]\n[[task]]\nid=\"b\"\nharness=\"codex\"\ntask=\"y\"\ndepends_on=[\"a\"]\n",
            "dependency cycle",
        ),
        ("[[task]]\nid=\"a\"\nharness=\"gpt\"\ntask=\"x\"\n", "harness must be"),
        ("[[task]]\nid=\"a\"\nharness=\"codex\"\ntask=\"x\"\nwriter=true\nreviewer=true\n", "reviewer cannot"),
    ] {
        let p = plan(&e, "bad.toml", body);
        let (c, _, err) = fox(&e, &["swarm", "run", &p], &[]);
        assert_ne!(c, 0);
        assert!(err.contains(want), "{err}");
    }
    assert_eq!(agent_count(&e), 0);
}

fn summaries(e: &Env) -> i64 {
    let w = rusqlite::Connection::open(e.home.path().join("fx/fox.db")).unwrap();
    w.query_row("SELECT COUNT(*) FROM handoff WHERE kind = 'report' AND title LIKE 'swarm %'", [], |r| r.get(0)).unwrap()
}

/// Wait until the stub log holds at least `n` lines that start with `prefix`.
fn wait_log(e: &Env, prefix: &str, n: usize) {
    let t = std::time::Instant::now();
    loop {
        let text = fs::read_to_string(e.log.path().join("times")).unwrap_or_default();
        if text.lines().filter(|l| l.starts_with(prefix)).count() >= n {
            return;
        }
        assert!(t.elapsed() < std::time::Duration::from_secs(30), "no {prefix} in {text}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// User plus system CPU time a process has used so far, in hundredths of a second.
#[cfg(target_os = "linux")]
fn cpu_ticks(pid: u32) -> u64 {
    // Fields 14 and 15 of /proc/<pid>/stat, in clock ticks (100 a second on Linux).
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let f: Vec<&str> = s.rsplit_once(')').unwrap().1.split_whitespace().collect();
    f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
}

/// User plus system CPU time a process has used so far, in hundredths of a second, from
/// `ps -o time=` (`[[dd-]hh:]mm:ss.cc` on macOS and the BSDs).
#[cfg(not(target_os = "linux"))]
fn cpu_ticks(pid: u32) -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "time=", "-p", &pid.to_string()]).output().unwrap();
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (rest, days) = match t.split_once('-') {
        Some((d, r)) => (r.to_string(), d.parse::<u64>().unwrap()),
        None => (t.clone(), 0),
    };
    let mut secs = 0.0f64;
    for part in rest.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().unwrap_or_else(|_| panic!("ps time {t:?}"));
    }
    ((days as f64 * 86_400.0 + secs) * 100.0).round() as u64
}

#[test]
fn concurrent_runs_of_one_plan_dispatch_each_task_once_and_post_one_summary() {
    let e = setup();
    let p = plan(
        &e,
        "race.toml",
        r#"
concurrency = 3
[[task]]
id = "a"
harness = "codex"
task = "task a"
[[task]]
id = "b"
harness = "claude"
task = "task b"
[[task]]
id = "c"
harness = "codex"
task = "task c"
depends_on = ["a"]
"#,
    );
    let kids: Vec<_> = (0..4).map(|_| cmd(&e, &["swarm", "run", &p], &[("STUB_SLEEP", "1")]).spawn().unwrap()).collect();
    for k in kids {
        let o = k.wait_with_output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    assert_eq!(agent_count(&e), 3, "each task dispatched once");
    assert_eq!(spans(&e).len(), 3);
    assert_eq!(summaries(&e), 1, "one summary");
}

#[test]
fn the_panel_does_not_spin_while_its_slower_reviewers_run() {
    let e = setup();
    let p = plan(
        &e,
        "spin.toml",
        "[[task]]\nid=\"w\"\nharness=\"codex\"\ntask=\"work\"\n[review]\nharnesses=[\"codex\",\"claude\",\"claude\"]\ntask=\"check\"\n",
    );
    let mut k = cmd(&e, &["swarm", "run", &p], &[("STUB_SLEEP_CLAUDE", "6")]).spawn().unwrap();
    // The worker and the codex reviewer have ended; the two claude reviewers are still asleep.
    wait_log(&e, "end cx-", 2);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let before = cpu_ticks(k.id());
    std::thread::sleep(std::time::Duration::from_secs(2));
    let used = cpu_ticks(k.id()) - before;
    assert!(k.wait().unwrap().success());
    assert!(used < 40, "the run used {used} CPU ticks in 2s while waiting");
}

#[test]
fn a_panel_blocked_by_the_child_cap_waits_instead_of_failing() {
    let e = setup();
    let p =
        plan(&e, "capped.toml", "[[task]]\nid=\"w\"\nharness=\"codex\"\ntask=\"work\"\n[review]\nharnesses=[\"codex\"]\ntask=\"check\"\n");
    let run = cmd(&e, &["swarm", "run", &p], &[("FOXDB_AGENT", "ctl"), ("FOXDB_MAX_CHILDREN", "1"), ("STUB_SLEEP", "1")]).spawn().unwrap();
    // While the swarm's worker runs, someone else starts a child of the same parent.
    wait_log(&e, "start cx-", 1);
    let (c, out, err) = fox(
        &e,
        &["dispatch", "--harness", "claude", "--task", "outside work"],
        &[("FOXDB_AGENT", "ctl"), ("FOXDB_MAX_CHILDREN", "2"), ("STUB_SLEEP_CLAUDE", "4")],
    );
    assert_eq!(c, 0, "{out}{err}");
    let o = run.wait_with_output().unwrap();
    let (out, err) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(o.status.success(), "{out}{err}");
    assert!(out.contains("review: approved 1/1"), "{out}");
}

#[test]
fn plans_with_one_file_name_in_two_directories_are_two_swarms() {
    let e = setup();
    for (d, task) in [("x", "the x task"), ("y", "the y task")] {
        fs::create_dir_all(e.repo.path().join(d)).unwrap();
        plan(&e, &format!("{d}/plan.toml"), &format!("[[task]]\nid=\"{d}\"\nharness=\"codex\"\ntask=\"{task}\"\n"));
    }
    let x = e.repo.path().join("x/plan.toml").to_string_lossy().into_owned();
    let y = e.repo.path().join("y/plan.toml").to_string_lossy().into_owned();
    let (c, ox, err) = fox(&e, &["swarm", "run", &x], &[]);
    assert_eq!(c, 0, "{ox}{err}");
    let (c, oy, err) = fox(&e, &["swarm", "run", &y], &[]);
    assert_eq!(c, 0, "{oy}{err}");
    let id = |o: &str| o.split_whitespace().nth(1).unwrap().to_string();
    assert_ne!(id(&ox), id(&oy), "{ox}{oy}");
    assert!(ox.contains("  x ") && oy.contains("  y "), "{ox}{oy}");
    assert_eq!(summaries(&e), 2);
    // Each is still found by its id; the shared name is ambiguous and says so.
    let (_, sx, _) = fox(&e, &["swarm", "status", &id(&ox)], &[]);
    assert_eq!(sx, ox);
    let (c, _, err) = fox(&e, &["swarm", "status", "plan"], &[]);
    assert_ne!(c, 0);
    assert!(err.contains(&id(&ox)) && err.contains(&id(&oy)), "{err}");
}
