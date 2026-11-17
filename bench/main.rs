//! `fox-bench`: latency, answer size and token savings for fixed query sets.
//!
//! Each query runs through a real `fox` process (the cold CLI, or the CLI talking to a running
//! `foxd` when `--daemon` is given), so the numbers include process spawn, exactly what an agent's
//! Bash call pays. Tokens are estimated at 4 bytes per token. The naive alternative for a query is
//! what an agent without foxdb would read: `rg -n` output for the query's rarest term (at most
//! [`GREP_LINES`] lines, as agent grep tools truncate) plus one file read (at most [`READ_LINES`]
//! lines, an agent file reader's default) (the expected file when grep finds it, else grep's first file). It depends only
//! on the query, never on foxdb's answer, so a noisier answer cannot inflate its own baseline.
//! See the README (Benchmarks).

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const USAGE: &str = "usage:
  fox-bench run --set FILE --repo DIR [--fox PATH] [--foxd PATH] [--daemon] [--iters N] [--json OUT] [--label TEXT]
  fox-bench tools [--fox PATH]
  fox-bench orch --repo DIR [--fox PATH] [--n N] [--json OUT]   packet size, dispatch/wait overhead (stub codex, scratch store)

Set FOXDB_HOME to a scratch store that already indexes --repo. Headroom mode is reported from
FOXBENCH_HEADROOM=1 or an ANTHROPIC_BASE_URL pointing at 127.0.0.1:8787; nothing is changed.";

#[derive(Deserialize)]
struct Set {
    name: String,
    q: Vec<Query>,
}

#[derive(Deserialize, Clone)]
struct Query {
    cat: String,
    q: String,
    #[serde(default)]
    expect: Option<String>,
}

struct Res {
    cat: String,
    ms: Vec<f64>,
    out_tokens: i64,
    naive_tokens: i64,
    rank: Option<usize>,
}

fn tokens(n: usize) -> i64 {
    n.div_ceil(4) as i64
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i.min(v.len() - 1)]
}

fn fox_args(q: &Query) -> Vec<String> {
    let mut a: Vec<String> = match q.cat.as_str() {
        "callers" => vec!["code".into(), q.q.clone(), "--rel".into(), "callers".into()],
        "doc" => vec!["find".into(), q.q.clone(), "--kind".into(), "doc".into()],
        "memory" => vec!["find".into(), q.q.clone(), "--kind".into(), "memory".into()],
        _ => vec!["find".into(), q.q.clone()],
    };
    a.extend(["--budget".into(), "500".into()]);
    a
}

fn run_fox(fox: &Path, repo: &Path, args: &[String]) -> (f64, String) {
    let t = Instant::now();
    let out = Command::new(fox).args(args).current_dir(repo).stdin(Stdio::null()).stderr(Stdio::null()).output();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    (ms, out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default())
}

/// The rarest-looking term: the longest identifier-ish word.
fn grep_term(q: &str) -> String {
    let base = q.rsplit('.').next().unwrap_or(q);
    let words: Vec<&str> = base.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| w.len() > 2).collect();
    words.iter().max_by_key(|w| w.len()).map(|w| w.to_string()).unwrap_or_else(|| q.to_string())
}

/// The one file the naive agent reads whole: the file `expect` names (a file name or path) if grep
/// listed it, else the first file grep listed. `grep_out` is `rg -n --no-heading --sort path`.
fn naive_file(expect: Option<&str>, grep_out: &str) -> Option<String> {
    let mut files: Vec<&str> = vec![];
    for f in grep_out.lines().filter_map(|l| l.split(':').next()) {
        if files.last() != Some(&f) {
            files.push(f);
        }
    }
    let want = expect.map(|e| e.trim_end_matches(':')).filter(|e| e.contains('.') || e.contains('/'));
    want.and_then(|w| files.iter().find(|f| f.ends_with(w) && (f.len() == w.len() || f[..f.len() - w.len()].ends_with('/'))))
        .or(files.first())
        .map(|f| f.to_string())
}

/// Grep lines an agent sees before its tool truncates the output.
const GREP_LINES: usize = 250;
/// Lines an agent's file reader returns by default.
const READ_LINES: usize = 2000;

/// The first `n` lines of `s`.
fn head(s: &str, n: usize) -> &str {
    match s.match_indices('\n').nth(n.saturating_sub(1)) {
        Some((i, _)) if n > 0 => &s[..=i],
        _ if n == 0 => "",
        _ => s,
    }
}

fn naive(repo: &Path, q: &Query) -> i64 {
    let term = grep_term(&q.q);
    let mut rg = Command::new("rg");
    rg.args(["-n", "--no-heading", "--sort", "path", "-i", "-F"]).arg(&term).current_dir(repo).stderr(Stdio::null());
    let grep = rg.output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let read = naive_file(q.expect.as_deref(), &grep)
        .and_then(|p| std::fs::read(repo.join(p)).ok())
        .map(|b| head(&String::from_utf8_lossy(&b), READ_LINES).len());
    let grep = head(&grep, GREP_LINES);
    tokens(grep.len()) + tokens(read.unwrap_or(0))
}

fn rank_of(answer: &str, expect: &str) -> Option<usize> {
    answer.lines().filter(|l| !l.starts_with('+')).position(|l| l.contains(expect)).map(|i| i + 1)
}

fn headroom_mode() -> &'static str {
    let on = std::env::var("FOXBENCH_HEADROOM").is_ok_and(|v| v == "1")
        || std::env::var("ANTHROPIC_BASE_URL").is_ok_and(|v| v.contains("127.0.0.1:8787") || v.contains("localhost:8787"));
    if on { "headroom-on" } else { "headroom-off" }
}

/// Size of the MCP tool definitions plus server instructions, as the client loads them.
fn tool_def_bytes(fox: &Path) -> (usize, usize) {
    let mut child = match Command::new(fox).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    {
        use std::io::Write;
        let mut si = child.stdin.take().unwrap();
        let _ = writeln!(si, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18"}}}}"#);
        let _ = writeln!(si, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#);
    }
    let out = child.wait_with_output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let mut tools = 0;
    let mut instr = 0;
    for l in out.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else { continue };
        if let Some(t) = v["result"].get("tools") {
            tools = t.to_string().len();
        }
        if let Some(i) = v["result"]["instructions"].as_str() {
            instr = i.len();
        }
    }
    (tools, instr)
}

struct Daemon(Option<std::process::Child>);
impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            foxdb::daemon::shutdown();
            for _ in 0..40 {
                if c.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn start_daemon(foxd: &Path) -> Daemon {
    let sock = foxdb::daemon::socket_path();
    let _ = std::fs::remove_file(&sock);
    let child = Command::new(foxd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
    for _ in 0..200 {
        if foxdb::daemon::ping() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Daemon(child)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn sibling(name: &str) -> PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name))).unwrap_or_else(|| PathBuf::from(name))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fox = flag(&args, "--fox").map(PathBuf::from).unwrap_or_else(|| sibling("fox"));
    match args.first().map(String::as_str) {
        Some("tools") => {
            let (t, i) = tool_def_bytes(&fox);
            println!("tool definitions: {t} bytes (~{} tokens); server instructions: {i} bytes (~{} tokens)", tokens(t), tokens(i));
        }
        Some("run") => run(&args, &fox),
        Some("orch") => orch(&args, &fox),
        _ => {
            println!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn run(args: &[String], fox: &Path) {
    let (Some(set), Some(repo)) = (flag(args, "--set"), flag(args, "--repo")) else {
        println!("{USAGE}");
        std::process::exit(2);
    };
    let set: Set = toml::from_str(&std::fs::read_to_string(&set).expect("read --set")).expect("parse --set");
    let repo = std::fs::canonicalize(repo).expect("--repo");
    let iters: usize = flag(args, "--iters").and_then(|s| s.parse().ok()).unwrap_or(5);
    let daemon = args.iter().any(|a| a == "--daemon");
    let _d = if daemon { start_daemon(&flag(args, "--foxd").map(PathBuf::from).unwrap_or_else(|| sibling("foxd"))) } else { Daemon(None) };
    let client = if daemon { "daemon" } else { "cold-cli" };
    let label = flag(args, "--label").unwrap_or_default();

    let mut results: Vec<Res> = vec![];
    for q in &set.q {
        let a = fox_args(q);
        let (_, answer) = run_fox(fox, &repo, &a); // warm the page cache
        let mut ms = vec![];
        for _ in 0..iters {
            ms.push(run_fox(fox, &repo, &a).0);
        }
        let rank = q.expect.as_deref().and_then(|e| rank_of(&answer, e));
        results.push(Res { cat: q.cat.clone(), ms, out_tokens: tokens(answer.len()), naive_tokens: naive(&repo, q), rank });
    }

    let (tdef, instr) = tool_def_bytes(fox);
    println!("set {} · {} queries x {iters} · client {client} · {} {label}", set.name, set.q.len(), headroom_mode());
    println!(
        "{:<8} {:>3} {:>8} {:>8} {:>8} {:>8} {:>8} {:>6} {:>5}",
        "cat", "n", "p50ms", "p95ms", "tok/ans", "naive", "saved", "hit@5", "mrr"
    );
    let mut cats: Vec<String> = results.iter().map(|r| r.cat.clone()).collect();
    cats.dedup();
    cats.sort();
    cats.dedup();
    cats.push("all".into());
    let mut json_rows = vec![];
    for cat in &cats {
        let rs: Vec<&Res> = results.iter().filter(|r| cat == "all" || &r.cat == cat).collect();
        let mut ms: Vec<f64> = rs.iter().flat_map(|r| r.ms.clone()).collect();
        let n = rs.len() as i64;
        let out: i64 = rs.iter().map(|r| r.out_tokens).sum::<i64>() / n.max(1);
        let nv: i64 = rs.iter().map(|r| r.naive_tokens).sum::<i64>() / n.max(1);
        let saved: i64 = rs.iter().map(|r| (r.naive_tokens - r.out_tokens).max(0)).sum::<i64>() / n.max(1);
        let hits = rs.iter().filter(|r| r.rank.is_some_and(|k| k <= 5)).count();
        let mrr: f64 = rs.iter().map(|r| r.rank.map(|k| 1.0 / k as f64).unwrap_or(0.0)).sum::<f64>() / n.max(1) as f64;
        let (p50, p95) = (pct(&mut ms, 0.5), pct(&mut ms, 0.95));
        println!("{cat:<8} {n:>3} {p50:>8.2} {p95:>8.2} {out:>8} {nv:>8} {saved:>8} {:>3}/{:<2} {mrr:>5.2}", hits, n);
        json_rows.push(serde_json::json!({"cat": cat, "n": n, "p50_ms": p50, "p95_ms": p95, "tokens_per_answer": out,
            "naive_tokens": nv, "saved_tokens": saved, "hit5": hits, "mrr": mrr}));
    }
    let store = std::fs::metadata(foxdb::db::db_path()).map(|m| m.len()).unwrap_or(0);
    println!("tool definitions ~{} tokens + instructions ~{} tokens · store {:.1} MB", tokens(tdef), tokens(instr), store as f64 / 1e6);
    if let Some(out) = flag(args, "--json") {
        let doc = serde_json::json!({"set": set.name, "client": client, "headroom": headroom_mode(), "label": label, "iters": iters,
            "tool_def_tokens": tokens(tdef), "instructions_tokens": tokens(instr), "store_bytes": store, "rows": json_rows});
        let _ = std::fs::write(out, serde_json::to_string_pretty(&doc).unwrap());
    }
}

/// Milestone 2 overhead: packet size for `--repo`, and what `fox dispatch` + `fox wait` add on top
/// of a worker that does nothing (a stub `codex` that writes its last message and exits). Uses a
/// scratch FOXDB_HOME; never calls a model.
fn orch(args: &[String], fox: &Path) {
    let Some(repo) = flag(args, "--repo") else {
        println!("{USAGE}");
        std::process::exit(2);
    };
    let n: usize = flag(args, "--n").and_then(|s| s.parse().ok()).unwrap_or(10);
    let scratch = std::env::temp_dir().join(format!("fox-bench-orch-{}", std::process::id()));
    let stubs = scratch.join("bin");
    std::fs::create_dir_all(&stubs).unwrap();
    let stub = stubs.join("codex");
    std::fs::write(
        &stub,
        "#!/bin/sh\nout=\"\"; prev=\"\"\nfor a in \"$@\"; do [ \"$prev\" = \"-o\" ] && out=\"$a\"; prev=\"$a\"; done\ncat >/dev/null\necho '{\"type\":\"thread.started\",\"thread_id\":\"t\"}'\nprintf '%s\\n' '{\"status\":\"done\",\"summary\":\"bench\",\"files\":[],\"tests\":{\"ran\":[],\"passed\":false},\"decisions\":[],\"next\":[]}' > \"$out\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!("{}:{}", stubs.display(), std::env::var("PATH").unwrap_or_default());
    let fx = |a: &[&str]| -> (f64, String) {
        let t = std::time::Instant::now();
        let o = Command::new(fox)
            .args(a)
            .current_dir(&repo)
            .env("FOXDB_HOME", scratch.join("fx"))
            .env("FOXDB_NO_DAEMON", "1")
            .env("PATH", &path)
            .env_remove("FOXDB_AGENT")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (t.elapsed().as_secs_f64() * 1000.0, String::from_utf8_lossy(&o.stdout).into_owned())
    };
    fx(&["index", "--no-global", "--quiet"]);
    fx(&["mem", "post", "Bench decision: keep packets handle-based", "--kind", "decision"]);
    let (create_ms, out) = fx(&["handoff", "create", "--goal", "bench packet", "--next", "run the gate"]);
    let h = out.split_whitespace().nth(1).unwrap_or_default().to_string();
    let packet_tokens = |f: &str| fx(&["handoff", "render", &h, "--for", f]).1.len().div_ceil(4);
    let (pc, px, pp) = (packet_tokens("claude"), packet_tokens("codex"), packet_tokens("plain"));
    let (mut d, mut w) = (vec![], vec![]);
    for i in 0..n {
        let (dm, out) = fx(&["dispatch", "--harness", "codex", "--task", &format!("bench task {i}")]);
        let id = out.split_whitespace().next().unwrap_or_default().to_string();
        let (wm, _) = fx(&["wait", &id, "--timeout", "30"]);
        d.push(dm);
        w.push(wm);
    }
    let (done_ms, _) = fx(&["wait", "cx-1", "--timeout", "1"]);
    let real = realistic_packet(fox, &repo, &scratch, &path);
    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let (dm, wm) = (med(&mut d), med(&mut w));
    println!("packet: ~{pp} tokens (claude cap ~{pc}, codex cap ~{px}); create {create_ms:.0} ms");
    println!(
        "realistic packet (12 edited files, 3 new, 6 decisions, 4 requests, 8 reports/findings): ~{} tokens (claude cap ~{}, codex cap ~{}), uncapped ~{}",
        real.0, real.1, real.2, real.3
    );
    println!(
        "dispatch median {dm:.1} ms; wait for a no-op stub worker median {wm:.1} ms; wait on a finished agent {done_ms:.1} ms (n={n})"
    );
    if let Some(out) = flag(args, "--json") {
        let doc = serde_json::json!({"repo": repo, "n": n, "packet_tokens": pp, "packet_tokens_claude": pc, "packet_tokens_codex": px,
            "packet_create_ms": create_ms, "dispatch_ms_median": dm, "wait_ms_median": wm, "wait_done_ms": done_ms,
            "realistic_packet_tokens": real.0, "realistic_packet_tokens_claude": real.1, "realistic_packet_tokens_codex": real.2,
            "realistic_packet_tokens_uncapped": real.3});
        let _ = std::fs::write(out, serde_json::to_string_pretty(&doc).unwrap());
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A packet for a mid-task worktree rather than a clean one: a scratch clone of `repo` with 12
/// edited tracked files, 3 new files, and a board with decisions (with reasons), open requests,
/// findings and reports. Returns (plain, claude, codex, uncapped) token counts.
fn realistic_packet(fox: &Path, repo: &str, scratch: &Path, path: &str) -> (usize, usize, usize, usize) {
    let clone = scratch.join("clone");
    let git = |args: &[&str], dir: &Path| {
        Command::new("git").args(args).current_dir(dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().ok();
    };
    git(&["clone", "-q", "--no-hardlinks", repo, &clone.to_string_lossy()], scratch);
    if !clone.join(".git").exists() {
        eprintln!("realistic packet: cannot clone {repo}");
        return (0, 0, 0, 0);
    }
    let files = Command::new("git").args(["ls-files", "*.rs"]).current_dir(&clone).output().map(|o| o.stdout).unwrap_or_default();
    let files = String::from_utf8_lossy(&files).to_string();
    for f in files.lines().take(12) {
        let p = clone.join(f);
        if let Ok(mut t) = std::fs::read_to_string(&p) {
            t.push_str("\n// bench: work in progress\nfn bench_wip() {}\n");
            let _ = std::fs::write(&p, t);
        }
    }
    for i in 0..3 {
        let _ = std::fs::write(clone.join(format!("src/bench_new_{i}.rs")), "pub fn fresh() {}\n");
    }
    let fx = |a: &[&str]| -> String {
        let o = Command::new(fox)
            .args(a)
            .current_dir(&clone)
            .env("FOXDB_HOME", scratch.join("fx-real"))
            .env("FOXDB_NO_DAEMON", "1")
            .env("PATH", path)
            .env("FOXDB_AGENT", "bench-ctl")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    fx(&["index", "--no-global", "--quiet"]);
    for i in 0..6 {
        fx(&[
            "mem",
            "post",
            &format!(
                "Decision {i}: keep the store layout for module {i}\nBecause migrating it now would invalidate every open lease and packet; revisit after the bench run."
            ),
            "--kind",
            "decision",
        ]);
    }
    for i in 0..4 {
        fx(&["mem", "post", &format!("Request {i}: review the error path in module {i} before merge"), "--kind", "request"]);
    }
    for i in 0..4 {
        fx(&[
            "mem",
            "post",
            &format!("Finding {i}: src/module_{i}.rs:42 returns early on an empty batch; callers assume one row"),
            "--kind",
            "finding",
        ]);
        fx(&[
            "mem",
            "post",
            &format!("done: worker {i} split the arm\nfiles: src/module_{i}.rs:10\ntests: `cargo test` passed"),
            "--kind",
            "report",
        ]);
    }
    let out = fx(&[
        "handoff",
        "create",
        "--goal",
        "Split search arms into modules and keep ranking identical",
        "--next",
        "finish the trigram arm",
        "--next",
        "run the full gate",
        "--next",
        "update the README search section",
        "--question",
        "keep the RRF constant at 60?",
        "--question",
        "drop the legacy cascade path now or in the next milestone?",
    ]);
    let h = out.split_whitespace().nth(1).unwrap_or_default().to_string();
    let t = |f: &str, b: Option<&str>| {
        let mut a = vec!["handoff", "render", h.as_str(), "--for", f];
        if let Some(b) = b {
            a.extend(["--budget", b]);
        }
        fx(&a).len().div_ceil(4)
    };
    (t("plain", None), t("claude", None), t("codex", None), t("claude", Some("100000")))
}

#[cfg(test)]
mod tests {
    use super::naive_file;

    const GREP: &str =
        "app/src/a/Helper.kt:3:x\napp/src/a/Helper.kt:9:x\napp/src/b/PlayerViewModel.kt:12:x\napp/src/b/XPlayerViewModel.kt:1:x\n";

    #[test]
    fn naive_reads_the_expected_file_when_grep_finds_it() {
        assert_eq!(naive_file(Some("PlayerViewModel.kt:"), GREP).as_deref(), Some("app/src/b/PlayerViewModel.kt"));
        assert_eq!(naive_file(Some("src/b/PlayerViewModel.kt:"), GREP).as_deref(), Some("app/src/b/PlayerViewModel.kt"));
    }

    #[test]
    fn grep_output_is_capped_like_an_agent_tool() {
        assert_eq!(super::head("a\nb\nc\n", 2), "a\nb\n");
        assert_eq!(super::head("a\nb", 5), "a\nb");
        assert_eq!(super::head("a\n", 0), "");
    }

    #[test]
    fn otherwise_grep_s_first_file() {
        assert_eq!(naive_file(Some("open_ro"), GREP).as_deref(), Some("app/src/a/Helper.kt"));
        assert_eq!(naive_file(Some("Missing.kt:"), GREP).as_deref(), Some("app/src/a/Helper.kt"));
        assert_eq!(naive_file(None, ""), None);
    }
}
