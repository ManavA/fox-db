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
        let before: Vec<i64> = db::FTS_TABLES.iter().map(|t| db::fts_segments(&c, t)).collect();
        db::fts_optimize(&c)?;
        let after: Vec<i64> = db::FTS_TABLES.iter().map(|t| db::fts_segments(&c, t)).collect();
        let tables = db::FTS_TABLES.iter().zip(before.iter().zip(&after)).map(|(t, (b, a))| format!("{t} {b}->{a}")).collect::<Vec<_>>();
        return Ok(format!("optimized ({}) in {} ms", tables.join(", "), t0.elapsed().as_millis()));
    }
    let target = a.pos.get(1).map(PathBuf::from).unwrap_or_else(cwd);
    let mut c = db::open_rw()?;
    let loc = project::locate(&target);
    let p = project::ensure(&c, &loc)?;
    let res = inventory::index_project(&mut c, &p);
    let g = if a.no_global { Ok(0) } else { inventory::index_global(&mut c) };
    let _ = std::fs::remove_file(lock_path(p.id));
    let st = res?;
    let g = g?;
    let sw = if a.no_swift || p.kind == "huge" || swiftimport::discover_stores(&p.root).is_empty() {
        String::new()
    } else {
        match swiftimport::import_project(&mut c, &p) {
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
        match tsindex::index_project(&mut c, &p) {
            Ok(s) => format!(
                "; tree-sitter: {} files, {} parsed, {} unchanged, {} removed, {} symbols, {} refs ({} scored, {} resolved), {} ms",
                s.files, s.parsed, s.unchanged, s.removed, s.symbols, s.refs, s.rescored, s.resolved, s.ms
            ),
            Err(e) => format!("; tree-sitter index failed: {e:#}"),
        }
    };
    let cn = if !config::get().codanna || a.no_codanna || p.kind == "huge" || !codanna::available() || !codanna::needed(&c, &p) {
        String::new()
    } else {
        match codanna::sync(&c, &p) {
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
    ))
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
    let r = match verb.first().copied() {
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
        Some("code") => cmd_code(&a),
        Some("mem") if matches!(a.pos.get(1).map(String::as_str), Some("post" | "read" | "pickup")) => cli::board(&a, &cwd()),
        Some("drift" | "pack") => cli::docref(&a, &cwd()),
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
        _ => {
            println!("{USAGE}");
            return ExitCode::from(2);
        }
    };
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
