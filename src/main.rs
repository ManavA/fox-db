//! `fox` CLI: find, status, index, mem brief.
//! Fail-open: `mem brief` never prints errors and always exits 0.

use foxdb::{
    bg::{lock_path, spawn_background_index},
    codanna, config, db, hook, inventory, project, query, savings, swiftimport, tsindex, watch,
};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use foxdb::cli::{self, Args, USAGE, parse};

/// Re-index in the background if never indexed or older than this.
const REFRESH_AFTER_SECS: i64 = 600;

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn cmd_brief(a: &Args) -> anyhow::Result<String> {
    let loc = project::locate(&cwd());
    let budget = a.budget.unwrap_or(400);
    let h = hook::Harness::parse(&a.harness);
    // Hot path: read-only. Register (one write) only for unknown projects.
    if let Some(c) = db::open_ro()
        && let Some(p) = project::find(&c, &loc)?
    {
        if p.indexed_at.is_none_or(|t| db::now() - t > REFRESH_AFTER_SECS) {
            spawn_background_index(&p);
        }
        return hook::brief_for(&c, &p, loc.git_dir.as_deref().and_then(project::branch).as_deref(), h, budget);
    }
    let c = db::open_rw()?;
    let p = project::ensure(&c, &loc)?;
    spawn_background_index(&p);
    hook::brief_for(&c, &p, loc.git_dir.as_deref().and_then(project::branch).as_deref(), h, budget)
}

fn cmd_index(a: &Args) -> anyhow::Result<String> {
    // `fox index optimize`: fold every full-text table into one segment (idle-time work).
    if a.pos.get(1).map(String::as_str) == Some("optimize") && !std::path::Path::new("optimize").is_dir() {
        let c = db::open_rw()?;
        let t0 = std::time::Instant::now();
        let before: Vec<i64> = db::MERGE_TABLES.iter().map(|t| db::fts_segments(&c, t)).collect();
        db::fts_optimize(&c)?;
        let after: Vec<i64> = db::MERGE_TABLES.iter().map(|t| db::fts_segments(&c, t)).collect();
        let tables = db::MERGE_TABLES.iter().zip(before.iter().zip(&after)).map(|(t, (b, a))| format!("{t} {b}->{a}")).collect::<Vec<_>>();
        return Ok(format!("optimized ({}) in {} ms", tables.join(", "), t0.elapsed().as_millis()));
    }
    // `fox index rebuild [PATH] [--backend tantivy|fts5]`: (re)build or drop a tantivy index.
    if a.pos.get(1).map(String::as_str) == Some("rebuild") && !std::path::Path::new("rebuild").is_dir() {
        return cmd_rebuild(a);
    }
    let target = a.pos.get(1).map(PathBuf::from).unwrap_or_else(cwd);
    let mut c = db::open_rw()?;
    let loc = project::locate(&target);
    let p = project::ensure(&c, &loc)?;
    let out = index_once(a, &mut c, &p)?;
    // A project past the size trigger gets a tantivy index, filled by a second pass right away.
    #[cfg(feature = "tantivy")]
    if p.kind != "huge"
        && let Ok(Some(what)) = foxdb::tantivy_index::maintain(&c, p.id, &p.name)
    {
        if what == "enabled" {
            let again = index_once(a, &mut c, &p)?;
            return Ok(format!("{out}\ntantivy index enabled for {}; filled: {again}", p.name));
        }
        return Ok(format!("{out}\ntantivy index {what} for {}", p.name));
    }
    // A build without tantivy cannot keep an index current: drop it rather than let its log grow
    // (a tantivy build makes a new one on its next index run).
    #[cfg(not(feature = "tantivy"))]
    if foxdb::outbox::state(&c, p.id).is_some() {
        foxdb::outbox::disable(&c, p.id)?;
        return Ok(format!("{out}\ntantivy index dropped for {}: this build has no tantivy backend", p.name));
    }
    Ok(out)
}

/// `fox index rebuild [PATH] [--backend tantivy|fts5]`.
fn cmd_rebuild(a: &Args) -> anyhow::Result<String> {
    let target = a.pos.get(2).map(PathBuf::from).unwrap_or_else(cwd);
    let mut c = db::open_rw()?;
    let p = project::ensure(&c, &project::locate(&target))?;
    match a.opt.get("backend").map(String::as_str).unwrap_or("tantivy") {
        "fts5" => {
            foxdb::outbox::disable(&c, p.id)?;
            Ok(format!("{}: tantivy index dropped (foxd deletes its files); FTS5 serves every search", p.name))
        }
        "tantivy" => {
            #[cfg(feature = "tantivy")]
            {
                foxdb::tantivy_index::enable(&c, p.id)?;
                let out = index_once(a, &mut c, &p)?;
                Ok(format!("{out}\n{}: tantivy index filled; foxd builds it and searches use it once it has caught up", p.name))
            }
            #[cfg(not(feature = "tantivy"))]
            {
                let _ = &mut c;
                anyhow::bail!("this build has no tantivy backend (build with --features tantivy)")
            }
        }
        other => anyhow::bail!("unknown backend {other:?}: use tantivy or fts5"),
    }
}

/// One index run of `p`: inventory, global notes, Swift index stores, tree-sitter, codanna.
fn index_once(a: &Args, c: &mut rusqlite::Connection, p: &project::Project) -> anyhow::Result<String> {
    let res = inventory::index_project(c, p);
    let delta = res.as_ref().ok().and_then(|s| s.delta.clone());
    let g = if a.no_global { Ok(0) } else { inventory::index_global(c) };
    let _ = std::fs::remove_file(lock_path(p.id));
    let st = res?;
    let g = g?;
    let sw = if a.no_swift || p.kind == "huge" || swiftimport::discover_stores(&p.root).is_empty() {
        String::new()
    } else {
        match swiftimport::import_project(c, p) {
            Ok(s) => format!(
                "; swift: {} stores, {}/{} units changed, {} records read, {} symbols, {} occurrences, {} edges, {} stale files, {} ms",
                s.stores, s.units_changed, s.units_seen, s.records_read, s.symbols, s.occurrences, s.edges, s.stale_files, s.ms
            ),
            Err(e) => format!("; swift import failed: {e:#}"),
        }
    };
    let ts = if p.kind == "huge" {
        String::new()
    } else {
        match tsindex::index_project_with(c, p, delta.as_ref()) {
            Ok(s) => format!(
                "; tree-sitter: {} files, {} parsed, {} unchanged, {} removed, {} symbols, {} refs ({} scored, {} resolved), {} ms",
                s.files, s.parsed, s.unchanged, s.removed, s.symbols, s.refs, s.rescored, s.resolved, s.ms
            ),
            Err(e) => format!("; tree-sitter index failed: {e:#}"),
        }
    };
    let vx = vectors_after_index(c, p);
    // Count memory lines that cite gone code, for the brief. Best effort.
    let _ = foxdb::memcheck::record(c, p);
    let cn = if !config::get().codanna || a.no_codanna || p.kind == "huge" || !codanna::available() || !codanna::needed(c, p) {
        String::new()
    } else {
        match codanna::sync(c, p) {
            Ok(s) => format!("; codanna mirror: {} files, {} copied, {} removed, {} ms", s.files, s.copied, s.removed, s.ms),
            Err(e) => format!("; codanna sync failed: {e:#}"),
        }
    };
    Ok(format!(
        "{}: {} files seen, {} changed, {} removed, {} sections ({} milestone), {} summary dirs{} in {} ms; global memory notes: {}{sw}{ts}{cn}",
        p.name,
        st.files_seen,
        st.files_changed,
        st.files_removed,
        st.sections,
        st.milestones,
        st.summaries,
        if st.truncated { " (TRUNCATED)" } else { "" },
        st.ms,
        g
    ) + &vx)
}

/// Refresh the vector index after an index run, only while the arm is on and set up.
#[allow(unused_variables)]
fn vectors_after_index(c: &rusqlite::Connection, p: &project::Project) -> String {
    #[cfg(feature = "vectors")]
    if config::vectors_on() && p.kind != "huge" && foxdb::vectors::model_present() {
        return match foxdb::vectors::refresh(c, p, false) {
            Ok(s) if s.current => String::new(),
            Ok(s) => format!("; vectors: {} rows, {} files parsed, {} embedded, {} ms", s.rows, s.parsed, s.embedded, s.ms),
            Err(e) => format!("; vector index failed: {e:#}"),
        };
    }
    String::new()
}

fn cmd_find(a: &Args) -> anyhow::Result<String> {
    cli::find(a, &cwd(), None)
}

fn cmd_code(a: &Args) -> anyhow::Result<String> {
    cli::code(a, &cwd(), None)
}

fn cmd_status() -> anyhow::Result<String> {
    let Some(c) = db::open_ro() else { return Ok(format!("no store yet at {}", db::db_path().display())) };
    let cur = project::find(&c, &project::locate(&cwd()))?;
    query::status(&c, cur.as_ref())
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if matches!(argv.first().map(String::as_str), Some("--version" | "-V" | "version")) {
        println!("fox {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    // Read verbs go to a running foxd first (warm graph, prepared statements, result cache).
    if let Some(r) = foxdb::daemon::call(&argv, &cwd()) {
        if r.code == 0 {
            print!("{}", r.text);
            if !r.text.ends_with('\n') {
                println!();
            }
            return ExitCode::SUCCESS;
        }
        eprintln!("fox: {}", r.text);
        return ExitCode::FAILURE;
    }
    let a = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            println!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let verb: Vec<&str> = a.pos.iter().map(String::as_str).take(2).collect();
    // `mem brief` is the SessionStart hook: fail open, silent on error, exit 0.
    if verb == ["mem", "brief"] {
        // Hard 1.5 s deadline: print nothing rather than delay session start.
        let deadline_ms = std::env::var("FOXDB_BRIEF_DEADLINE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(cmd_brief(&a));
        });
        if let Ok(Ok(s)) = rx.recv_timeout(std::time::Duration::from_millis(deadline_ms)) {
            print!("{s}");
        }
        return ExitCode::SUCCESS;
    }
    // Hooks fail open too: read stdin, print the hook JSON or nothing, always exit 0.
    if verb.first() == Some(&"hook") {
        let event = verb.get(1).map(|s| s.to_string()).unwrap_or_default();
        let deadline_ms = std::env::var("FOXDB_BRIEF_DEADLINE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
        let h = hook::Harness::parse(&a.harness);
        let mut input = String::new();
        let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(hook::run(&event, &input, h));
        });
        if let Ok(s) = rx.recv_timeout(std::time::Duration::from_millis(deadline_ms))
            && !s.is_empty()
        {
            println!("{s}");
        }
        return ExitCode::SUCCESS;
    }
    // `fox wait` exits 3 when its targets are still running at the timeout, and 4 when they
    // ended but one went stale or failed (or a handoff's workers ended without a report).
    if verb.first() == Some(&"wait") {
        return match cli::wait(&a) {
            Ok(r) => {
                print!("{}", r.text);
                if !r.met {
                    ExitCode::from(3)
                } else if r.bad {
                    ExitCode::from(4)
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(e) => {
                eprintln!("fox: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    // `fox swarm run` exits like `fox wait`: 3 still running, 4 a task failed or was rejected.
    if verb == ["swarm", "run"] {
        return match cli::swarm_run(&a) {
            Ok(o) => {
                print!("{}", o.text);
                if !o.finished {
                    ExitCode::from(3)
                } else if o.bad {
                    ExitCode::from(4)
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(e) => {
                eprintln!("fox: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    if verb.first() == Some(&"switch") {
        return match cli::switch(&a, &cwd()) {
            Ok(sw) if a.flags.contains("exec") && !sw.argv.is_empty() => {
                use std::os::unix::process::CommandExt;
                let e = std::process::Command::new(&sw.argv[0]).args(&sw.argv[1..]).envs(sw.env.iter().map(|(k, v)| (k, v))).exec();
                eprintln!("fox: cannot exec {}: {e}", sw.argv[0]);
                ExitCode::FAILURE
            }
            Ok(sw) => {
                print!("{}", sw.text);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("fox: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    // Short read verbs may open the store lockless in a read-only sandbox (see db::open_ro).
    let lockless =
        matches!(verb.first().copied(), Some("find" | "code" | "status" | "stats" | "drift" | "pack")) || verb == ["mem", "check"];
    if lockless {
        db::allow_lockless_reads();
    }
    let run = || -> Option<anyhow::Result<String>> {
        Some(match verb.first().copied() {
            Some("find") => cmd_find(&a),
            Some("status") if a.flags.contains("agents") => db::open_rw().and_then(|w| {
                foxdb::agents::refresh(&w)?;
                foxdb::agents::live(&w)
            }),
            Some("status") => cmd_status(),
            Some("agents" | "agent") => cli::agent(&a),
            Some("handoff") => cli::handoff_cmd(&a, &cwd()),
            Some("lease") => cli::lease(&a, &cwd()),
            Some("dispatch") => cli::dispatch(&a, &cwd()),
            Some("_supervise") => match a.pos.get(1) {
                Some(id) => foxdb::orch::supervise(id).map(|_| String::new()),
                None => Ok(String::new()),
            },
            Some("stats") => match db::open_ro() {
                Some(c) => savings::stats(&c),
                None => Ok("no store yet".into()),
            },
            Some("index") => cmd_index(&a),
            Some("eval") => cli::eval(&a, &cwd()),
            Some("swarm") => cli::swarm(&a, &cwd()),
            Some("crew") => cli::crew(&a, &cwd()),
            Some("code") => cmd_code(&a),
            Some("mem") if matches!(a.pos.get(1).map(String::as_str), Some("post" | "read" | "pickup")) => cli::board(&a, &cwd()),
            Some("mem") if a.pos.get(1).map(String::as_str) == Some("check") => cli::mem_check(&a, &cwd()),
            Some("drift" | "pack") => cli::docref(&a, &cwd()),
            Some("scip") => cli::scip(&a, &cwd()),
            Some("vectors") => cli::vectors(&a, &cwd()),
            Some("ingest") if a.pos.get(1).map(String::as_str) == Some("sessions") => cli::ingest(&a),
            Some("mcp") => foxdb::mcp::serve().map(|_| String::new()),
            Some("daemon") => match a.pos.get(1).map(String::as_str) {
                Some("status") => Ok(if foxdb::daemon::ping() {
                    format!("foxd running at {}", foxdb::daemon::socket_path().display())
                } else {
                    "foxd not running".into()
                }),
                Some("stop") => Ok(if foxdb::daemon::ping() {
                    foxdb::daemon::shutdown();
                    "foxd stopped".into()
                } else {
                    "foxd not running".into()
                }),
                _ => Ok("usage: fox daemon status|stop   (start it by running foxd)".into()),
            },
            Some("watch") => {
                let target = a.pos.get(1).map(PathBuf::from).unwrap_or_else(cwd);
                let loc = project::locate(&target);
                db::open_rw().and_then(|c| project::ensure(&c, &loc)).and_then(|p| watch::run(&p)).map(|_| String::new())
            }
            _ => return None,
        })
    };
    let Some(mut r) = run() else {
        println!("{USAGE}");
        return ExitCode::from(2);
    };
    // A writer checkpointed into the store while a lockless read was under way: what was read may
    // mix old and new pages. Read once more; if it moves again, say so rather than guess.
    if db::lockless_changed() {
        db::reset_lockless();
        r = run().unwrap_or_else(|| Ok(String::new()));
        if db::lockless_changed() {
            r = Err(anyhow::anyhow!("the store changed while being read without locks (read-only sandbox); run the command again"));
        }
    }
    match r {
        Ok(s) => {
            if !(s.is_empty() || a.quiet && verb.first() == Some(&"index")) {
                print!("{s}");
                if !s.ends_with('\n') {
                    println!();
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fox: {e:#}");
            ExitCode::FAILURE
        }
    }
}
