//! CLI argument parsing and the read verbs (`find`, `code`) shared by `fox` and `foxd`.

use crate::bg::spawn_background_index;
use crate::search::Warm;
use crate::{code, db, project, query, savings};
use rusqlite::Connection;
use std::path::Path;

pub const USAGE: &str = "usage:
  fox --version
  fox find <query> [--kind any|symbol|file|doc|milestone|dir|memory] [--all] [--limit N] [--budget TOKENS]
  fox code <symbol|handle|path> [--rel callers|callees|refs|conforms|subtypes|extensions|overrides|outline|src|impact] [--depth N] [--budget TOKENS] [--cursor N] [--fallback] [--fresh]
  fox check [--project P] [--no-fresh]   certain callers of changed signatures the diff missed (exit 3)
  fox watch [PATH]
  fox mcp
  fox daemon status|stop        (run `foxd` to start the daemon)
  fox status
  fox stats
  fox index [PATH] [--quiet] [--no-global]      fox index optimize   (merge the full-text tables now)
  fox index rebuild [PATH] [--backend tantivy|fts5]   build (or drop) a big project's tantivy index
  fox eval retrieval --set bench/eval/SET.json [--repo DIR] [--limit K] [--json] [--gate BASELINE.json]
  fox mem brief [--budget TOKENS] [--harness claude|codex]
  fox mem post <text> [--kind fact|failed|question|summary|finding|claim|request|decision|report] [--title T] [--evidence E]
  fox mem read [query] [--kind K]          fox mem pickup h:ID [--by NAME]
  fox mem check                            memory lines citing symbols, paths or flags that are gone
  fox drift [doc.md]                       cited symbols/paths changed, moved or vanished since the doc's last commit
  fox pack <doc.md> [--budget TOKENS]      the doc plus skeletons of the code it cites (for a fresh reviewer)
  fox scip import <index.scip> [--project P] [--trust-mtime]   precise tier from a SCIP index your own build produced
  fox vectors setup [--yes] | build [--project P] | status   the opt-in vector arm (builds with --features vectors)
  fox ingest sessions [--since-days N] [--claude DIR] [--codex DIR]   (read-only transcript digests)
  fox hook session-start|subagent-start|subagent-stop|stop|pre-compact|post-tool-use|user-prompt-submit [--harness claude|codex]   (hook JSON on stdin)
multi-agent:
  fox dispatch --harness codex|claude --task <text|h:ID> [--repo DIR] [--writer|--reviewer] [--label L] [--model M] [--packet h:ID]
  fox wait <agent|h:ID>... [--any|--all] [--timeout SECS]       exit 3 still running at the timeout, 4 stale/failed
  fox agents [--tree] [--all]      fox status --agents      fox lease list|take|release [--repo DIR] [--agent A] [--pid N]
  fox agent register [--id ID] [--harness H] [--role R] [--parent P] [--model M] [--pid N] [--transcript PATH] [--session S]
  fox agent heartbeat|done [ID] [--status failed] [--report h:ID]          (ID defaults to $FOXDB_AGENT)
  fox handoff create [--goal G] [--state S] [--next N] [--question Q] [--parent h:ID] [--agent A] [--transcript PATH]
  fox handoff render h:ID [--for claude|codex|t3|plain] [--budget TOKENS]     fox handoff take h:ID [--by A] [--lease SECS]
  fox swarm run <plan.toml> [--no-wait] [--timeout SECS]    fox swarm status <swarm>
  fox crew up <plan.toml|N> [--harness claude|codex] [--backend herdr|tmux] [--model M]   native sessions in worktrees and panes
  fox crew ls | attach <name> | down [<name>] [--prune]
  fox switch --to codex|claude|--for t3 [--from H] [--handoff h:ID] [--agent A] [--session ID] [--transcript PATH] [--inline] [--exec]
  fox mem post <text> [--topic T]          fox mem read --since me|h:ID [--topic T]   (me: new to $FOXDB_AGENT)";

/// Default answer budget (tokens) for `find` and `code`: small, page with `--cursor`.
pub const DEFAULT_BUDGET: usize = 500;

#[derive(Clone)]
pub struct Args {
    pub pos: Vec<String>,
    pub kind: String,
    pub all: bool,
    pub quiet: bool,
    pub no_global: bool,
    pub no_swift: bool,
    pub no_codanna: bool,
    pub fallback: bool,
    pub fresh: bool,
    /// Bare flags for the multi-agent verbs: tree, any, writer, reviewer, inline, exec, agents.
    pub flags: std::collections::BTreeSet<String>,
    /// `--harness` was given explicitly.
    pub harness_set: bool,
    /// Misc `--name value` options for the less common verbs (title, evidence, by, since-days, ...).
    pub opt: std::collections::BTreeMap<String, String>,
    pub rel: String,
    pub depth: usize,
    pub cursor: usize,
    pub limit: usize,
    pub budget: Option<usize>,
    pub harness: String,
    /// The caller's session id for "already sent" dedup and handoff attribution. `parse` reads
    /// it from the caller's `FOXDB_SESSION`; the daemon replaces it with the one the client sent.
    pub session: Option<String>,
}

pub fn parse(argv: Vec<String>) -> Result<Args, String> {
    let mut a = Args {
        pos: vec![],
        kind: "any".into(),
        all: false,
        quiet: false,
        no_global: false,
        no_swift: false,
        no_codanna: false,
        fallback: false,
        fresh: false,
        flags: Default::default(),
        harness_set: false,
        opt: Default::default(),
        rel: "callers".into(),
        depth: 2,
        cursor: 0,
        limit: 20,
        budget: None,
        harness: "claude".into(),
        session: std::env::var("FOXDB_SESSION").ok().filter(|s| !s.is_empty()),
    };
    let mut it = argv.into_iter();
    while let Some(x) = it.next() {
        let mut val = |n: &str| it.next().ok_or(format!("{n} needs a value"));
        match x.as_str() {
            "--kind" => a.kind = val("--kind")?,
            "--all" => a.all = true,
            "--quiet" | "-q" => a.quiet = true,
            "--no-global" => a.no_global = true,
            "--no-swift" => a.no_swift = true,
            "--no-codanna" => a.no_codanna = true,
            "--fallback" => a.fallback = true,
            "--fresh" => a.fresh = true,
            "--tree" | "--any" | "--writer" | "--reviewer" | "--inline" | "--exec" | "--agents" | "--json" | "--no-wait"
            | "--trust-mtime" | "--yes" | "--no-fresh" => {
                a.flags.insert(x.trim_start_matches('-').to_string());
            }
            "--title" | "--evidence" | "--by" | "--since-days" | "--claude" | "--codex" | "--goal" | "--state" | "--next"
            | "--question" | "--parent" | "--transcript" | "--agent" | "--for" | "--lease" | "--to" | "--from" | "--session" | "--task"
            | "--repo" | "--label" | "--model" | "--packet" | "--timeout" | "--since" | "--topic" | "--id" | "--role" | "--pid"
            | "--status" | "--report" | "--handoff" | "--set" | "--gate" | "--project" | "--backend" => {
                let k = x.trim_start_matches('-').to_string();
                let v = val(&x)?;
                a.opt.insert(k, v);
            }
            "--rel" => a.rel = val("--rel")?,
            "--depth" => a.depth = val("--depth")?.parse().map_err(|_| "bad --depth")?,
            "--cursor" => a.cursor = val("--cursor")?.parse().map_err(|_| "bad --cursor")?,
            "--limit" => a.limit = val("--limit")?.parse().map_err(|_| "bad --limit")?,
            "--budget" => a.budget = Some(val("--budget")?.parse().map_err(|_| "bad --budget")?),
            "--harness" => {
                a.harness = val("--harness")?;
                a.harness_set = true;
            }
            "-h" | "--help" => return Err(String::new()),
            _ => a.pos.push(x),
        }
    }
    Ok(a)
}

/// Long-lived state a daemon passes in: a read connection and the warm search state.
pub struct Served<'a> {
    pub ro: &'a Connection,
    pub rw: Option<&'a Connection>,
    pub warm: &'a mut Warm,
}

fn account(served: &Option<Served>, pid: Option<i64>, tool: &str, out: String) -> String {
    match served.as_ref().and_then(|s| s.rw) {
        Some(w) => savings::account_with(w, pid, tool, out),
        None => savings::account(pid, tool, out),
    }
}

/// `fox mem post|read|pickup`: the handoff board for agents that use the CLI (Codex, scripts).
pub fn board(a: &Args, cwd: &Path) -> anyhow::Result<String> {
    let w = db::open_rw()?;
    let Some(p) = project::find(&w, &project::locate(cwd))? else { anyhow::bail!("project not indexed; run `fox index`") };
    let rest = a.pos.get(2..).map(|v| v.join(" ")).unwrap_or_default();
    let kind = (a.kind != "any").then_some(a.kind.as_str());
    match a.pos.get(1).map(String::as_str) {
        Some("post") => {
            let id = crate::handoff::post(
                &w,
                &crate::handoff::Post {
                    project_id: p.id,
                    kind: kind.unwrap_or("finding"),
                    text: &rest,
                    title: a.opt.get("title").map(String::as_str),
                    evidence: a.opt.get("evidence").map(String::as_str),
                    harness: Some("cli"),
                    session: a.session.as_deref(),
                    agent: Some(&whoami(&w)),
                    topic: a.opt.get("topic").map(String::as_str),
                    ..Default::default()
                },
            )?;
            Ok(format!("posted {}", crate::handoff::handle(id)))
        }
        Some("read") if a.opt.contains_key("since") => {
            let s = &a.opt["since"];
            let who = whoami(&w);
            let since = match crate::handoff::parse_handle(s) {
                Some(n) => crate::handoff::Since::After(n),
                None if s == "auto" || s == "me" => crate::handoff::Since::Auto(&who),
                None => anyhow::bail!("--since takes me (or auto) or h:ID"),
            };
            crate::handoff::read_since(&w, p.id, since, kind, a.opt.get("topic").map(String::as_str), a.budget.unwrap_or(DEFAULT_BUDGET))
        }
        Some("read") => match crate::handoff::parse_handle(&rest) {
            Some(id) => crate::handoff::show(&w, id),
            None => crate::handoff::read(
                &w,
                p.id,
                kind,
                (!rest.is_empty()).then_some(rest.as_str()),
                a.budget.unwrap_or(DEFAULT_BUDGET),
                a.cursor,
            ),
        },
        Some("pickup") => {
            let id = crate::handoff::parse_handle(&rest).ok_or_else(|| anyhow::anyhow!("pickup needs h:ID"))?;
            let by = a.opt.get("by").cloned().unwrap_or_else(|| whoami(&w));
            crate::handoff::pickup(&w, id, &by)
        }
        _ => anyhow::bail!("{USAGE}"),
    }
}

/// This process's agent identity (`FOXDB_AGENT`).
pub fn me() -> Option<String> {
    std::env::var("FOXDB_AGENT").ok().filter(|s| !s.is_empty())
}

/// Who this command speaks for when it must be someone (takes, read cursors, posts, dispatch
/// parents): `FOXDB_AGENT`, else the session or shell it runs in (`cli-<shell pid>` at worst).
pub fn whoami(c: &rusqlite::Connection) -> String {
    crate::agents::ambient_id(Some(c), "cli")
}

fn opt<'a>(a: &'a Args, k: &str) -> Option<&'a str> {
    a.opt.get(k).map(String::as_str).filter(|s| !s.is_empty())
}

fn opt_n(a: &Args, k: &str) -> anyhow::Result<Option<i64>> {
    opt(a, k).map(|v| v.parse::<i64>().map_err(|_| anyhow::anyhow!("--{k} needs a number"))).transpose()
}

/// `fox agent register|heartbeat|done` and `fox agents`.
pub fn agent(a: &Args) -> anyhow::Result<String> {
    let w = db::open_rw()?;
    let sub = a.pos.get(1).map(String::as_str);
    if a.pos[0] == "agents" {
        crate::agents::refresh(&w)?;
        return crate::agents::list(&w, a.flags.contains("tree"), a.all);
    }
    let id = opt(a, "id").map(String::from).or_else(|| a.pos.get(2).cloned()).or_else(me);
    match sub {
        Some("register") => {
            let harness = if a.harness_set { a.harness.clone() } else { std::env::var("FOXDB_HARNESS").unwrap_or_else(|_| "other".into()) };
            let id = match id {
                Some(i) => i,
                None => crate::agents::next_id(&w, "ag")?,
            };
            let cwd = std::env::current_dir()?;
            let pid = crate::project::find(&w, &crate::project::locate(&cwd))?.map(|p| p.id);
            let cwd_s = cwd.to_string_lossy().into_owned();
            let parent = opt(a, "parent").map(String::from).or_else(|| std::env::var("FOXDB_PARENT").ok().filter(|s| !s.is_empty()));
            crate::agents::register(
                &w,
                &crate::agents::Reg {
                    id: &id,
                    harness: &harness,
                    model: opt(a, "model"),
                    role: opt(a, "role"),
                    label: opt(a, "label"),
                    parent: parent.as_deref(),
                    project_id: pid,
                    cwd: Some(&cwd_s),
                    pid: opt_n(a, "pid")?,
                    session: opt(a, "session"),
                    transcript: opt(a, "transcript"),
                    ..Default::default()
                },
            )?;
            Ok(format!("{id}\n"))
        }
        Some("heartbeat") => {
            let id = id.ok_or_else(|| anyhow::anyhow!("which agent? pass an id or set FOXDB_AGENT"))?;
            crate::agents::heartbeat(&w, &id)?;
            Ok(String::new())
        }
        Some("done") => {
            let id = id.ok_or_else(|| anyhow::anyhow!("which agent? pass an id or set FOXDB_AGENT"))?;
            let report = opt(a, "report").and_then(crate::handoff::parse_handle);
            crate::agents::finish(&w, &id, opt(a, "status").unwrap_or("done"), report)?;
            Ok(format!("{id} {}\n", opt(a, "status").unwrap_or("done")))
        }
        _ => anyhow::bail!("usage: fox agent register|heartbeat|done"),
    }
}

/// `fox handoff create|render|take`.
pub fn handoff_cmd(a: &Args, cwd: &Path) -> anyhow::Result<String> {
    let w = db::open_rw()?;
    let target = a.pos.get(2).and_then(|h| crate::handoff::parse_handle(h));
    match a.pos.get(1).map(String::as_str) {
        Some("create") => {
            let loc = project::locate(cwd);
            let p = project::ensure(&w, &loc)?;
            let who = opt(a, "agent").map(String::from).or_else(|| Some(whoami(&w)));
            let reg = who.as_deref().and_then(|id| crate::agents::get(&w, id));
            let tx = opt(a, "transcript")
                .map(std::path::PathBuf::from)
                .or_else(|| reg.as_ref().and_then(|r| r.transcript.clone()).map(Into::into));
            let spec = crate::packet::Spec {
                goal: opt(a, "goal"),
                state: opt(a, "state"),
                next: opt(a, "next").into_iter().collect(),
                questions: opt(a, "question").into_iter().collect(),
                parent: opt(a, "parent").and_then(crate::handoff::parse_handle),
                agent: who.as_deref(),
                harness: reg.as_ref().map(|r| r.harness.as_str()),
                transcript: tx.as_deref(),
            };
            let id = crate::packet::create(&w, &p, &spec)?;
            let n = crate::packet::render(&w, id, usize::MAX)?.len().div_ceil(4);
            Ok(format!("packet {} (~{n} tokens)\n", crate::handoff::handle(id)))
        }
        Some("render") => {
            let id = target.ok_or_else(|| anyhow::anyhow!("render needs h:ID"))?;
            let h = opt(a, "for").filter(|f| *f != "plain");
            crate::packet::render(&w, id, crate::packet::cap_for(h, a.budget))
        }
        Some("take") => {
            let id = target.ok_or_else(|| anyhow::anyhow!("take needs h:ID"))?;
            let by = opt(a, "by").map(String::from).unwrap_or_else(|| whoami(&w));
            let lease = match opt_n(a, "lease")? {
                Some(s) => Some(s),
                None => Some(crate::mcp::TAKE_LEASE_SECS),
            };
            crate::handoff::take(&w, id, &by, lease)
        }
        _ => anyhow::bail!("usage: fox handoff create|render|take"),
    }
}

/// `fox lease list|take|release`.
pub fn lease(a: &Args, cwd: &Path) -> anyhow::Result<String> {
    let w = db::open_rw()?;
    let repo = opt(a, "repo").map(std::path::PathBuf::from).unwrap_or_else(|| cwd.to_path_buf());
    let key = crate::agents::repo_key(&repo);
    match a.pos.get(1).map(String::as_str) {
        Some("list") | None => crate::agents::lease_list(&w),
        Some("take") => {
            let who = opt(a, "agent").map(String::from).or_else(me).ok_or_else(|| anyhow::anyhow!("take needs --agent or FOXDB_AGENT"))?;
            let pid = opt_n(a, "pid")?;
            // (Re)register: a done, failed or stale agent would hold a lease that is void at once.
            let known = crate::agents::get(&w, &who);
            crate::agents::register(
                &w,
                &crate::agents::Reg {
                    id: &who,
                    harness: known.as_ref().map(|k| k.harness.as_str()).unwrap_or("other"),
                    role: known.is_none().then_some("writer"),
                    pid,
                    writer: true,
                    ..Default::default()
                },
            )?;
            match crate::agents::lease_take(&w, &key, &who, pid)? {
                Ok(()) => Ok(format!("writer lease on {key} held by {who}\n")),
                Err(h) => anyhow::bail!("writer lease on {key} held by {h}"),
            }
        }
        Some("release") => {
            let n = crate::agents::lease_release(&w, &key, opt(a, "agent"))?;
            Ok(if n > 0 { format!("released {key}\n") } else { format!("no lease on {key}\n") })
        }
        _ => anyhow::bail!("usage: fox lease list|take|release"),
    }
}

/// `fox dispatch`.
pub fn dispatch(a: &Args, cwd: &Path) -> anyhow::Result<String> {
    if !a.harness_set {
        anyhow::bail!("dispatch needs --harness codex|claude");
    }
    let task = opt(a, "task").map(String::from).unwrap_or_else(|| a.pos.get(1..).map(|v| v.join(" ")).unwrap_or_default());
    let repo = opt(a, "repo").map(std::path::PathBuf::from).unwrap_or_else(|| cwd.to_path_buf());
    let parent = Some(whoami(&db::open_rw()?));
    let s = crate::orch::dispatch(&crate::orch::Dispatch {
        harness: &a.harness,
        task: &task,
        repo: &repo,
        writer: a.flags.contains("writer"),
        label: opt(a, "label"),
        model: opt(a, "model"),
        packet: opt(a, "packet"),
        parent: parent.as_deref(),
        reviewer: a.flags.contains("reviewer"),
    })?;
    Ok(s.line + "\n")
}

/// `fox wait`.
pub fn wait(a: &Args) -> anyhow::Result<crate::orch::Waited> {
    let secs: u64 =
        opt(a, "timeout").map(|t| t.parse().map_err(|_| anyhow::anyhow!("--timeout needs seconds"))).transpose()?.unwrap_or(3600);
    crate::orch::wait(&a.pos[1..], a.flags.contains("any"), std::time::Duration::from_secs(secs))
}

/// `fox switch`.
pub fn switch(a: &Args, cwd: &Path) -> anyhow::Result<crate::orch::SwitchCmd> {
    let for_t3 = opt(a, "for") == Some("t3");
    let to = match opt(a, "to") {
        Some(t) => t,
        None if for_t3 => "t3",
        None => anyhow::bail!("switch needs --to codex|claude (or --for t3)"),
    };
    let tx = opt(a, "transcript").map(std::path::PathBuf::from);
    crate::orch::switch(&crate::orch::Switch {
        to,
        from: opt(a, "from"),
        handoff: opt(a, "handoff").or(a.opt.get("packet").map(String::as_str)),
        agent: opt(a, "agent"),
        session: opt(a, "session"),
        transcript: tx.as_deref(),
        inline: a.flags.contains("inline"),
        cwd,
        exec: a.flags.contains("exec") && !for_t3,
        for_t3,
    })
}

/// `fox mem check`: memory lines that cite code which no longer exists (read-only).
pub fn mem_check(_a: &Args, cwd: &Path) -> anyhow::Result<String> {
    let Some(c) = db::open_ro() else { anyhow::bail!("no store yet; run `fox index`") };
    let Some(p) = project::find(&c, &project::locate(cwd))? else { anyhow::bail!("project not indexed; run `fox index`") };
    Ok(crate::memcheck::render(&crate::memcheck::check(&c, &p)?))
}

/// `fox drift [doc]` and `fox pack <doc>`.
pub fn docref(a: &Args, cwd: &Path) -> anyhow::Result<String> {
    let c = match db::open_ro() {
        Some(c) => c,
        None => anyhow::bail!("no store yet; run `fox index`"),
    };
    let loc = project::locate(cwd);
    let Some(p) = project::find(&c, &loc)? else { anyhow::bail!("project not indexed; run `fox index`") };
    let doc = a.pos.get(1).map(|d| {
        let abs = std::fs::canonicalize(cwd.join(d)).unwrap_or_else(|_| cwd.join(d));
        abs.strip_prefix(&p.root).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| d.clone())
    });
    match (a.pos[0].as_str(), doc) {
        ("drift", Some(d)) => crate::docref::drift(&c, &p, &d),
        ("drift", None) => crate::docref::drift_all(&c, &p),
        ("pack", Some(d)) => crate::docref::pack(&c, &p, &d, a.budget.unwrap_or(4000)),
        _ => anyhow::bail!("{USAGE}"),
    }
}

/// `fox vectors setup [--yes] | build [--project P] | status`: the opt-in vector arm.
pub fn vectors(a: &Args, cwd: &Path) -> anyhow::Result<String> {
    #[cfg(not(feature = "vectors"))]
    {
        let _ = (a, cwd);
        anyhow::bail!("this build has no vector arm (build with --features vectors)")
    }
    #[cfg(feature = "vectors")]
    match a.pos.get(1).map(String::as_str) {
        Some("setup") => {
            use std::io::{BufRead, IsTerminal, Write};
            let plan = crate::vectors::setup_plan();
            if !a.flags.contains("yes") {
                if !std::io::stdin().is_terminal() {
                    anyhow::bail!("{plan}nothing downloaded: run `fox vectors setup --yes` to fetch it");
                }
                print!("{plan}download it? [y/N] ");
                std::io::stdout().flush()?;
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    return Ok("nothing downloaded".into());
                }
            } else {
                print!("{plan}");
            }
            let log = crate::vectors::setup()?;
            Ok(format!(
                "{log}
set `vectors = true` in {} and run `fox vectors build` in a project",
                db::home().join("config.toml").display()
            ))
        }
        Some("build") => {
            if !crate::vectors::model_present() {
                anyhow::bail!("no model yet: run `fox vectors setup` first");
            }
            let c = db::open_rw()?;
            let p = match a.opt.get("project") {
                Some(name) => project::by_name(&c, name)?.ok_or_else(|| anyhow::anyhow!("no project named {name}; see fox status"))?,
                None => project::find(&c, &project::locate(cwd))?
                    .ok_or_else(|| anyhow::anyhow!("project not indexed; run `fox index` first"))?,
            };
            let s = crate::vectors::refresh(&c, &p, true)?;
            let mut out = if s.current {
                format!("{}: vector index already current ({} rows)", p.name, s.rows)
            } else {
                format!(
                    "{}: vector index of {} rows ({} files parsed, {} texts embedded) in {} ms",
                    p.name, s.rows, s.parsed, s.embedded, s.ms
                )
            };
            if !crate::config::vectors_on() {
                out += "
the arm is off: set `vectors = true` in config.toml to use it";
            }
            Ok(out)
        }
        Some("status") => match db::open_ro() {
            Some(c) => crate::vectors::status(&c),
            None => Ok("no store yet".into()),
        },
        _ => anyhow::bail!("usage: fox vectors setup [--yes] | build [--project P] | status"),
    }
}

/// `fox scip import <index.scip> [--project P] [--trust-mtime]`: the precise tier (see [`crate::scip`]).
pub fn scip(a: &Args, cwd: &Path) -> anyhow::Result<String> {
    let (Some("import"), Some(file)) = (a.pos.get(1).map(String::as_str), a.pos.get(2)) else {
        anyhow::bail!("usage: fox scip import <index.scip> [--project P] [--trust-mtime]")
    };
    let mut c = db::open_rw()?;
    let p = match a.opt.get("project") {
        Some(name) => project::by_name(&c, name)?.ok_or_else(|| anyhow::anyhow!("no project named {name}; see fox status"))?,
        None => project::find(&c, &project::locate(cwd))?.ok_or_else(|| anyhow::anyhow!("project not indexed; run `fox index` first"))?,
    };
    let s = crate::scip::import(&mut c, &p, &cwd.join(file), a.flags.contains("trust-mtime"))?;
    let mut out = format!(
        "{}: scip import of {} documents ({} fresh, {} changed since the index was built, {} without text, {} outside the project), {} definitions, {} references; {} tree-sitter references now precise ({} of them were guesses, unresolved or elsewhere) in {} ms",
        p.name, s.documents, s.fresh, s.stale, s.unverified, s.outside, s.definitions, s.references, s.precise, s.upgraded, s.ms
    );
    if s.stale > 0 {
        out.push_str("\nchanged files keep their tree-sitter answers; rebuild the index and import it again to cover them");
    }
    if s.unverified > 0 {
        out.push_str(&format!(
            "\n{} documents carry no text, so nothing proves the index read the files as they are now; they keep their tree-sitter answers. Rebuild with an indexer option that embeds document text, or pass --trust-mtime to trust files not modified after the index file (wrong answers if a file changed while the index was being built, or the index file was copied or touched later)",
            s.unverified
        ));
    }
    Ok(out)
}

/// `fox ingest sessions`.
pub fn ingest(a: &Args) -> anyhow::Result<String> {
    let mut w = db::open_rw()?;
    let since = a.opt.get("since-days").and_then(|d| d.parse::<i64>().ok()).map(|d| (db::now() - d * 86_400) * 1000);
    let claude = a.opt.get("claude").map(std::path::PathBuf::from).unwrap_or_else(crate::sessions::claude_root);
    let codex = a.opt.get("codex").map(std::path::PathBuf::from).unwrap_or_else(crate::sessions::codex_root);
    let t = std::time::Instant::now();
    let st = crate::sessions::ingest_since(&mut w, Some(&claude), Some(&codex), since)?;
    Ok(format!(
        "sessions: {} files seen, {} ingested, {} unchanged or older, {} ms",
        st.files,
        st.sessions,
        st.skipped,
        t.elapsed().as_millis()
    ))
}

pub fn find(a: &Args, cwd: &Path, served: Option<Served>) -> anyhow::Result<String> {
    let q = a.pos[1..].join(" ");
    if q.trim().is_empty() {
        anyhow::bail!("find needs a query\n{USAGE}");
    }
    let loc = project::locate(cwd);
    let owned;
    let c: &Connection = match &served {
        Some(s) => s.ro,
        None => {
            owned = match db::open_ro() {
                Some(c) => c,
                // A store this read-only process cannot open (an older schema in a sandbox: the
                // lockless open said why on stderr) is not created or migrated from here.
                None if db::rw_forbidden() => anyhow::bail!("cannot read the store at {}", db::db_path().display()),
                None => db::open_rw()?,
            };
            &owned
        }
    };
    let cur = project::find(c, &loc)?;
    if cur.is_none() && loc.kind != "huge" && !db::rw_forbidden() {
        // Auto-register and index in the background; this query searches everything known.
        if let Ok(w) = db::open_rw()
            && let Ok(p) = project::ensure(&w, &loc)
        {
            spawn_background_index(&p);
        }
    }
    let opts = query::FindOpts { kind: &a.kind, all: a.all, budget: a.budget.unwrap_or(DEFAULT_BUDGET), limit: a.limit, offset: a.cursor };
    let mut served = served;
    let out = match served.as_mut() {
        Some(s) => crate::search::find_warm(s.warm, c, cur.as_ref(), &q, &opts)?,
        None => query::find(c, cur.as_ref(), &q, &opts)?,
    };
    Ok(account(&served, cur.as_ref().map(|p| p.id), "find", out))
}

pub fn code(a: &Args, cwd: &Path, served: Option<Served>) -> anyhow::Result<String> {
    let sym = a.pos[1..].join(" ");
    let loc = project::locate(cwd);
    let owned;
    let c: &Connection = match &served {
        Some(s) => s.ro,
        None => {
            let Some(o) = db::open_ro() else { anyhow::bail!("no store yet; run `fox index`") };
            owned = o;
            &owned
        }
    };
    let Some(p) = project::find(c, &loc)? else { anyhow::bail!("project not indexed; run `fox index`") };
    let o =
        code::CodeOpts { rel: &a.rel, depth: a.depth, budget: a.budget.unwrap_or(DEFAULT_BUDGET), cursor: a.cursor, fallback: a.fallback };
    // Graph answers come from the index alone, so a warm process caches them by generation like
    // `find`. `src` reads files live and `impact` without a target reads the git diff: never cached.
    let cacheable = !matches!(a.rel.as_str(), "src" | "impact") && !a.fallback;
    let mut served = served;
    let out = match served.as_mut() {
        Some(s) if cacheable => {
            let key = format!("code|{}|{}|{}|{}|{}|{sym}", p.id, a.rel, a.depth, o.budget, a.cursor);
            s.warm.cached(c, key, || code::run(c, &p, &sym, &o))?
        }
        _ => code::run(c, &p, &sym, &o)?,
    };
    // Per-session "already sent" for the big readers, when the caller names its session.
    let out = match (a.rel.as_str(), a.session.as_deref()) {
        ("src" | "outline", Some(session)) => match served.as_ref().and_then(|s| s.rw) {
            Some(w) => crate::spare::filter_db(w, Some(session), &sym, out, a.fresh),
            None if db::rw_forbidden() => out,
            None => match db::open_rw() {
                Ok(w) => crate::spare::filter_db(&w, Some(session), &sym, out, a.fresh),
                Err(_) => out,
            },
        },
        _ => out,
    };
    Ok(account(&served, Some(p.id), &format!("code {}", a.rel), out))
}

/// `fox eval retrieval`: score a query set against the indexed repo; `--gate` exits non-zero
/// (through the error) when recall@5 or MRR of any query type fell below the baseline's.
pub fn eval(a: &Args, cwd: &Path) -> anyhow::Result<String> {
    if a.pos.get(1).map(String::as_str) != Some("retrieval") {
        anyhow::bail!("usage: fox eval retrieval --set FILE [--repo DIR] [--limit K] [--json] [--gate BASELINE]");
    }
    let set_path = a.opt.get("set").ok_or_else(|| anyhow::anyhow!("--set FILE is required"))?;
    let set = crate::eval::load(Path::new(set_path))?;
    let repo = a.opt.get("repo").map(std::path::PathBuf::from).unwrap_or_else(|| cwd.to_path_buf());
    let c = db::open_ro().ok_or_else(|| anyhow::anyhow!("no store yet: run `fox index` in the repo first"))?;
    let p = project::find(&c, &project::locate(&repo))?
        .ok_or_else(|| anyhow::anyhow!("{} is not indexed: run `fox index` there first", repo.display()))?;
    let k = if a.limit == 20 { 50 } else { a.limit.max(1) };
    let r = crate::eval::run(&c, &p, &set, k)?;
    let mut out = if a.flags.contains("json") { serde_json::to_string_pretty(&r)? + "\n" } else { crate::eval::render(&r) };
    if let Some(g) = a.opt.get("gate") {
        let text = std::fs::read_to_string(g).map_err(|e| anyhow::anyhow!("cannot read {g}: {e}"))?;
        let base: crate::eval::Report = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{g} is not an eval report: {e}"))?;
        let bad = crate::eval::regressions(&r, &base);
        if !bad.is_empty() {
            anyhow::bail!("{out}quality gate failed against {g}:\n  {}", bad.join("\n  "));
        }
        if !a.flags.contains("json") {
            out += &format!("quality gate passed against {g}\n");
        }
    }
    Ok(out)
}

/// `fox swarm run <plan.toml> [--no-wait] [--timeout SECS]` and `fox swarm status <swarm>`.
/// `run` ends with exit code 3 while tasks are still running and 4 when one failed, was
/// skipped, or the review panel rejected (see `main`).
pub fn swarm(a: &Args, _cwd: &Path) -> anyhow::Result<String> {
    match a.pos.get(1).map(String::as_str) {
        Some("status") => {
            let which = a.pos.get(2).ok_or_else(|| anyhow::anyhow!("usage: fox swarm status <swarm id or name>"))?;
            crate::swarm::status(&db::open_rw()?, which)
        }
        _ => anyhow::bail!("usage: fox swarm run <plan.toml> [--no-wait] [--timeout SECS] | fox swarm status <swarm>"),
    }
}

/// `fox crew up|ls|attach|down`: native interactive sessions, one per worktree and pane (crew.rs).
pub fn crew(a: &Args, cwd: &Path) -> anyhow::Result<String> {
    let backend = a.opt.get("backend").map(String::as_str);
    crate::crew::run(&a.pos, &a.harness, backend, a.opt.get("model").map(String::as_str), cwd)
}

/// `fox swarm run`.
pub fn swarm_run(a: &Args) -> anyhow::Result<crate::swarm::Outcome> {
    let plan = a.pos.get(2).ok_or_else(|| anyhow::anyhow!("usage: fox swarm run <plan.toml> [--no-wait] [--timeout SECS]"))?;
    let secs: u64 =
        opt(a, "timeout").map(|t| t.parse().map_err(|_| anyhow::anyhow!("--timeout needs seconds"))).transpose()?.unwrap_or(6 * 3600);
    crate::swarm::run(Path::new(plan), a.flags.contains("no-wait"), std::time::Duration::from_secs(secs))
}
