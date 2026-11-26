//! `fox crew`: start native interactive `claude` and `codex` sessions, one per git worktree, each
//! in its own terminal pane (herdr when it is on PATH, else tmux).
//!
//! The session is the harness's own interactive CLI, started the way a person would start it:
//! the same binary, the user's own config, auth, hooks, skills and compaction. Crew never proxies
//! it, never touches its system prompt (`--system-prompt`, `--append-system-prompt`) and never
//! uses `--bare`. The plan's prompt is the CLI's initial argument, the first user message. What
//! crew adds is the environment: `FOXDB_AGENT`, `FOXDB_PARENT` and `FOXDB_HOME`, which the foxdb
//! hooks and MCP server read to put the session in the agent tree.
//!
//! Each worktree is indexed (`fox index`) before its session starts, so the SessionStart brief
//! and the packet hooks find the project. Session records live in `$FOXDB_HOME/crew/<name>.json`.
//!
//! Plan format (TOML):
//!
//! ```toml
//! name = "auth"           # the crew's name; default: the file name
//! harness = "claude"      # default for sessions: claude | codex
//! model = "..."           # default model, optional
//! base = "main"           # where new branches start; default HEAD
//!
//! [[session]]
//! name = "api"            # letters, digits, - and _
//! harness = "codex"
//! model = "..."
//! branch = "crew/api"     # default crew/<name>; an existing branch is checked out
//! prompt = "Add rate limiting to the login handler"
//! writer = false          # take the repo's writer lease (at most one per plan)
//! args = []               # optional Claude --permission-mode and its value
//! ```

use crate::{agents, db, gitsnap, orch, project};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Write as _;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// The tmux session that holds every crew window.
pub const TMUX_SESSION: &str = "fox-crew";

/// Flags that would replace or strip the harness's own system prompt. Crew never passes them.
pub const FORBIDDEN: &[&str] =
    &["--bare", "--system-prompt", "--system-prompt-file", "--append-system-prompt", "--append-system-prompt-file"];

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct PlanFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default, rename = "session")]
    sessions: Vec<SessionFile>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct SessionFile {
    name: String,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    writer: bool,
    #[serde(default)]
    args: Vec<String>,
}

/// A checked plan with every session's defaults filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub name: String,
    pub base: Option<String>,
    pub sessions: Vec<Spec>,
}

/// One session to start.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub name: String,
    pub harness: String,
    pub model: Option<String>,
    pub branch: String,
    pub prompt: Option<String>,
    pub writer: bool,
    pub args: Vec<String>,
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parse and check a plan. `harness` is the default when neither the plan nor a session names
/// one; `file` names the default crew.
pub fn parse(text: &str, file: &Path, harness: &str) -> Result<Plan> {
    let p: PlanFile = toml::from_str(text).with_context(|| format!("{} is not a crew plan", file.display()))?;
    let name = if p.name.trim().is_empty() {
        file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "crew".into())
    } else {
        p.name.trim().to_string()
    };
    if p.sessions.is_empty() {
        bail!("the plan has no [[session]]");
    }
    let mut seen = std::collections::HashSet::new();
    let mut sessions = vec![];
    for s in p.sessions {
        if !valid_name(&s.name) {
            bail!("session name {:?}: use letters, digits, - and _ (at most 40)", s.name);
        }
        if !seen.insert(s.name.clone()) {
            bail!("session names must be unique: {}", s.name);
        }
        let h = s.harness.or_else(|| p.harness.clone()).unwrap_or_else(|| harness.to_string());
        if !matches!(h.as_str(), "claude" | "codex") {
            bail!("session {}: harness must be claude or codex", s.name);
        }
        if let Some(f) = s.args.iter().find(|a| FORBIDDEN.contains(&a.split('=').next().unwrap_or(""))) {
            bail!("session {}: {f} replaces the harness's system prompt; crew never passes it", s.name);
        }
        if !s.args.is_empty()
            && !matches!(s.args.as_slice(), [flag, value] if h == "claude" && flag == "--permission-mode" && !value.starts_with('-'))
        {
            bail!("session {}: args supports only Claude's --permission-mode and its value", s.name);
        }
        sessions.push(Spec {
            branch: s.branch.filter(|b| !b.trim().is_empty()).unwrap_or_else(|| format!("crew/{}", s.name)),
            name: s.name,
            harness: h,
            model: s.model.or_else(|| p.model.clone()),
            prompt: s.prompt.filter(|t| !t.trim().is_empty()),
            writer: s.writer,
            args: s.args,
        });
    }
    if sessions.iter().filter(|s| s.writer).count() > 1 {
        bail!("at most one session may be the writer (the lease is per repo)");
    }
    Ok(Plan { name, base: p.base.filter(|b| !b.trim().is_empty()), sessions })
}

/// `fox crew up N`: N bare sessions named s1, s2, ... skipping names already taken.
pub fn numbered(n: usize, harness: &str, model: Option<&str>, taken: &[String]) -> Result<Plan> {
    if n == 0 || n > 16 {
        bail!("crew up N takes 1 to 16 sessions");
    }
    if !matches!(harness, "claude" | "codex") {
        bail!("--harness must be claude or codex");
    }
    let names = (1..).map(|i| format!("s{i}")).filter(|s| !taken.contains(s)).take(n);
    let sessions = names
        .map(|name| Spec {
            branch: format!("crew/{name}"),
            name,
            harness: harness.to_string(),
            model: model.map(String::from),
            prompt: None,
            writer: false,
            args: vec![],
        })
        .collect();
    Ok(Plan { name: "crew".into(), base: None, sessions })
}

/// The environment crew adds to a session: its agent id, its parent and the store.
pub fn session_env(agent: &str, parent: &str, home: &Path) -> Vec<(String, String)> {
    vec![
        ("FOXDB_AGENT".into(), agent.into()),
        ("FOXDB_PARENT".into(), parent.into()),
        ("FOXDB_HOME".into(), home.to_string_lossy().into_owned()),
    ]
}

/// The native interactive CLI command. The prompt is its last argument (the first user turn);
/// `--` keeps it positional even if it looks like a flag or subcommand.
pub fn cli_argv(s: &Spec) -> Vec<String> {
    let mut v = vec![orch::harness_bin(&s.harness)];
    if let Some(m) = &s.model {
        v.push(if s.harness == "codex" { "-m" } else { "--model" }.into());
        v.push(m.clone());
    }
    v.extend(s.args.iter().cloned());
    if let Some(p) = &s.prompt {
        v.push("--".into());
        v.push(p.clone());
    }
    v
}

/// One shell line for a pane that runs a shell (herdr's `pane run`).
pub fn shell_line(argv: &[String]) -> String {
    argv.iter().map(|a| orch::sh_quote(a)).collect::<Vec<_>>().join(" ")
}

/// Which terminal multiplexer holds the panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Herdr,
    Tmux,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Herdr => "herdr",
            Backend::Tmux => "tmux",
        }
    }
}

fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "herdr".into())
}

fn on_path(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// `--backend` when given, else herdr when it is on PATH, else tmux.
pub fn pick_backend(asked: Option<&str>) -> Result<Backend> {
    match asked {
        Some("herdr") => Ok(Backend::Herdr),
        Some("tmux") => Ok(Backend::Tmux),
        Some(b) => bail!("--backend must be herdr or tmux, not {b}"),
        None if on_path(&herdr_bin()) => Ok(Backend::Herdr),
        None if on_path("tmux") => Ok(Backend::Tmux),
        None => bail!("crew needs herdr or tmux on PATH"),
    }
}

/// tmux: a detached window running the CLI directly, with the crew env, printing its pane id.
/// The first window creates the crew's tmux session.
pub fn tmux_launch(session_exists: bool, name: &str, wt: &Path, env: &[(String, String)], argv: &[String]) -> Vec<String> {
    let mut v: Vec<String> = vec!["tmux".into()];
    if session_exists {
        v.extend(["new-window".into(), "-d".into(), "-t".into(), format!("{TMUX_SESSION}:")]);
    } else {
        v.extend(["new-session".into(), "-d".into(), "-s".into(), TMUX_SESSION.into()]);
    }
    v.extend(["-n".into(), name.into(), "-c".into(), wt.to_string_lossy().into_owned()]);
    for (k, val) in env {
        v.extend(["-e".into(), format!("{k}={val}")]);
    }
    v.extend(["-P".into(), "-F".into(), "#{pane_id}".into()]);
    v.push("env".into());
    for key in SESSION_MARKERS {
        v.extend(["-u".into(), (*key).into()]);
    }
    v.extend(argv.iter().cloned());
    v
}

/// herdr step 1: a workspace in the worktree whose root shell has the crew env.
pub fn herdr_create(name: &str, wt: &Path, env: &[(String, String)]) -> Vec<String> {
    let mut v = vec![herdr_bin(), "workspace".into(), "create".into(), "--cwd".into(), wt.to_string_lossy().into_owned()];
    v.extend(["--label".into(), name.into()]);
    for (k, val) in env {
        v.extend(["--env".into(), format!("{k}={val}")]);
    }
    v.push("--no-focus".into());
    v
}

/// herdr step 2: type the CLI command into the workspace's root pane and submit it.
pub fn herdr_run(pane: &str, argv: &[String]) -> Vec<String> {
    vec![herdr_bin(), "pane".into(), "run".into(), pane.into(), shell_line(argv)]
}

/// A session crew started.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Record {
    pub name: String,
    pub crew: String,
    pub harness: String,
    pub agent: String,
    pub parent: String,
    /// The main checkout the worktree belongs to.
    pub repo: String,
    pub worktree: String,
    pub branch: String,
    pub backend: Backend,
    /// tmux pane id (`%3`) or herdr pane id (`w1:p1`).
    pub pane: String,
    /// herdr workspace id.
    #[serde(default)]
    pub workspace: Option<String>,
    pub started_at: i64,
}

/// Close the session's pane (herdr: its workspace).
pub fn close_cmd(r: &Record) -> Vec<String> {
    match r.backend {
        Backend::Tmux => vec!["tmux".into(), "kill-pane".into(), "-t".into(), r.pane.clone()],
        Backend::Herdr => {
            vec![herdr_bin(), "workspace".into(), "close".into(), r.workspace.clone().unwrap_or_else(|| r.pane.clone())]
        }
    }
}

/// Ask for the pane's state.
pub fn status_cmd(r: &Record) -> Vec<String> {
    match r.backend {
        Backend::Tmux => vec![
            "tmux".into(),
            "display-message".into(),
            "-p".into(),
            "-t".into(),
            r.pane.clone(),
            "#{?pane_dead,dead,live} #{pane_current_command}".into(),
        ],
        Backend::Herdr => vec![herdr_bin(), "pane".into(), "get".into(), r.pane.clone()],
    }
}

/// Bring the session's pane forward. `in_tmux`: the caller already runs inside tmux.
pub fn attach_cmds(r: &Record, in_tmux: bool) -> Vec<Vec<String>> {
    match r.backend {
        Backend::Herdr => {
            vec![vec![herdr_bin(), "workspace".into(), "focus".into(), r.workspace.clone().unwrap_or_else(|| r.pane.clone())]]
        }
        Backend::Tmux if in_tmux => vec![vec!["tmux".into(), "switch-client".into(), "-t".into(), r.pane.clone()]],
        Backend::Tmux => vec![
            vec!["tmux".into(), "select-window".into(), "-t".into(), r.pane.clone()],
            vec!["tmux".into(), "attach-session".into(), "-t".into(), TMUX_SESSION.into()],
        ],
    }
}

/// The first string under `key` anywhere in `v`, depth first, preferring the object `under`.
fn find_str<'a>(v: &'a Value, under: Option<&str>, key: &str) -> Option<&'a str> {
    if let Some(u) = under
        && let Some(s) = find_obj(v, u).and_then(|o| find_str(o, None, key))
    {
        return Some(s);
    }
    match v {
        Value::Object(m) => m.get(key).and_then(Value::as_str).or_else(|| m.values().find_map(|x| find_str(x, None, key))),
        Value::Array(a) => a.iter().find_map(|x| find_str(x, None, key)),
        _ => None,
    }
}

fn find_obj<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => m.get(key).filter(|x| x.is_object()).or_else(|| m.values().find_map(|x| find_obj(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find_obj(x, key)),
        _ => None,
    }
}

/// `herdr workspace create` JSON: (workspace id, root pane id).
pub fn herdr_ids(out: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(out.trim()).ok()?;
    let ws = find_str(&v, Some("workspace"), "workspace_id")?;
    let pane = find_str(&v, Some("root_pane"), "pane_id")?;
    Some((ws.to_string(), pane.to_string()))
}

/// A pane's state from `herdr pane get` JSON: the agent status when herdr reports one.
pub fn herdr_status(out: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(out.trim()) else { return "open".into() };
    find_str(&v, Some("agent"), "status").or_else(|| find_str(&v, None, "agent_status")).unwrap_or("open").to_string()
}

fn dir() -> PathBuf {
    db::home().join("crew")
}

fn record_path(name: &str) -> PathBuf {
    dir().join(format!("{name}.json"))
}

pub fn records() -> Vec<Record> {
    let mut v: Vec<Record> = std::fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect();
    v.sort_by(|a: &Record, b| (a.started_at, &a.name).cmp(&(b.started_at, &b.name)));
    v
}

fn lock() -> Result<std::fs::File> {
    std::fs::create_dir_all(dir())?;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir().join("lock"))?;
    f.lock()?;
    Ok(f)
}

fn save(r: &Record) -> Result<()> {
    let path = record_path(&r.name);
    let pending = path.with_extension("pending");
    std::fs::write(&pending, serde_json::to_string_pretty(r)?)?;
    std::fs::rename(pending, path)?;
    Ok(())
}

/// The caller's own harness session markers. A session crew starts is a new top-level session,
/// so these never reach it (herdr drops them itself; a tmux server would keep them).
pub const SESSION_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_CHILD_NOTES_DIR",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SSE_PORT",
    "CODEX_THREAD_ID",
    "FOXDB_SESSION",
];

fn command(argv: &[String]) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    for k in SESSION_MARKERS {
        c.env_remove(k);
    }
    c
}

fn exec(argv: &[String]) -> Result<Output> {
    command(argv).output().with_context(|| format!("cannot run {}", argv[0]))
}

fn bounded_output(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    let child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn()?;
    let group = child.id() as libc::pid_t;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(out) => Ok(out?),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The child leads its own process group so its descendants cannot keep the pipes open.
            unsafe { libc::kill(-group, libc::SIGKILL) };
            bail!("timed out after {}s", timeout.as_secs_f64());
        }
        Err(e) => Err(e.into()),
    }
}

const PANE_TIMEOUT: Duration = Duration::from_secs(1);
const HOOK_INDEX_TIMEOUT: Duration = Duration::from_secs(5);

fn launch_output(argv: &[String], operation: &str) -> Result<String> {
    let out = exec(argv).with_context(|| format!("{operation} failed"))?;
    // The command and the backend's output can both contain the session prompt.
    if !out.status.success() {
        bail!("{operation} failed ({})", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_ok(argv: &[String]) -> Result<String> {
    let o = exec(argv)?;
    if !o.status.success() {
        bail!("{} failed: {}", shell_line(argv), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut v = vec!["git".to_string(), "-C".into(), dir.to_string_lossy().into_owned()];
    v.extend(args.iter().map(|s| s.to_string()));
    run_ok(&v)
}

/// Where a session's worktree goes: under the main checkout's `.claude/worktrees`, which the
/// indexer skips and Claude Code uses for its own worktrees.
pub fn worktree_path(root: &Path, name: &str) -> PathBuf {
    root.join(".claude").join("worktrees").join(format!("crew-{name}"))
}

/// Create the session's worktree (or reuse one already there). Returns whether it was new.
fn ensure_worktree(root: &Path, wt: &Path, branch: &str, base: Option<&str>) -> Result<bool> {
    if wt.join(".git").exists() {
        let loc = project::locate(wt);
        if loc.common_dir != project::locate(root).common_dir || loc.git_dir.as_deref().and_then(project::branch).as_deref() != Some(branch)
        {
            bail!("{} is not a worktree of {} on branch {branch}", wt.display(), root.display());
        }
        return Ok(false);
    }
    let wt_s = wt.to_string_lossy();
    let exists = git(root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
    if exists {
        git(root, &["worktree", "add", "--", &wt_s, branch])?;
    } else {
        git(root, &["worktree", "add", "-b", branch, "--", &wt_s, base.unwrap_or("HEAD")])?;
    }
    Ok(true)
}

/// One `fox index` run for the worktree, so its project is in the store before the session's
/// SessionStart hook asks for it.
fn index(wt: &Path, timeout: Option<Duration>) -> Result<()> {
    let loc = project::locate(wt);
    if let Some(git_dir) = loc.git_dir.as_ref().filter(|gd| Some(*gd) != loc.common_dir.as_ref()) {
        std::fs::write(git_dir.join(project::SEPARATE_INDEX), "")?;
    }
    let mut cmd = Command::new(orch::fox_exe());
    cmd.arg("index").arg(wt).args(["--quiet", "--no-global"]);
    let o = match timeout {
        Some(timeout) => bounded_output(&mut cmd, timeout),
        None => cmd.output().map_err(Into::into),
    }
    .with_context(|| format!("fox index {} failed", wt.display()))?;
    if !o.status.success() {
        bail!("fox index {} failed: {}", wt.display(), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

fn tmux_session_exists() -> bool {
    Command::new("tmux").args(["has-session", "-t", &format!("={TMUX_SESSION}")]).output().is_ok_and(|o| o.status.success())
}

/// Start every session of the plan from the repo at `cwd`.
pub fn up(plan: &Plan, cwd: &Path, backend: Backend) -> Result<String> {
    let loc = project::locate(cwd);
    if loc.kind != "repo" {
        bail!("crew needs a git repo: {} is not in one", cwd.display());
    }
    let root = loc.repo_root().to_path_buf();
    let _lock = lock()?;
    for s in &plan.sessions {
        if record_path(&s.name).exists() {
            bail!("session {} is already up (fox crew down {} first)", s.name, s.name);
        }
    }
    let parent = crate::cli::me().unwrap_or_else(|| format!("crew:{}", plan.name));
    let mut out = String::new();
    for s in &plan.sessions {
        let wt = worktree_path(&root, &s.name);
        let fresh = ensure_worktree(&root, &wt, &s.branch, plan.base.as_deref())?;
        index(&wt, None)?;
        let home = std::fs::canonicalize(db::home()).unwrap_or_else(|_| db::home());
        let w = db::open_rw()?;
        let p = project::ensure(&w, &project::locate(&wt))?;
        let agent = agents::next_id(&w, if s.harness == "codex" { "cx" } else { "cl" })?;
        let label = format!("crew:{}", s.name);
        let wt_s = wt.to_string_lossy().into_owned();
        agents::register(
            &w,
            &agents::Reg {
                id: &agent,
                harness: &s.harness,
                model: s.model.as_deref(),
                role: Some(if s.writer { "writer" } else { "session" }),
                label: Some(&label),
                parent: Some(&parent),
                project_id: Some(p.id),
                cwd: Some(&wt_s),
                task: s.prompt.as_deref(),
                writer: s.writer,
                ..Default::default()
            },
        )?;
        let launch = (|| -> Result<Record> {
            if s.writer
                && let Err(holder) = agents::lease_take(&w, &agents::repo_key(&wt), &agent, None)?
            {
                bail!("writer lease on {} held by {holder}", root.display());
            }
            if let Some(base) = gitsnap::snapshot(&wt) {
                agents::set_commit(&w, &agent, Some(&base), None)?;
            }
            db::bump_generation(&w)?;
            let env = session_env(&agent, &parent, &home);
            let argv = cli_argv(s);
            let (pane, workspace) = match backend {
                Backend::Tmux => {
                    let cmd = tmux_launch(tmux_session_exists(), &s.name, &wt, &env, &argv);
                    let o = launch_output(&cmd, "tmux pane launch")?;
                    let id = o.trim();
                    if id.is_empty() {
                        bail!("tmux did not report the pane id for {}", s.name);
                    }
                    (id.to_string(), None)
                }
                Backend::Herdr => {
                    let o = run_ok(&herdr_create(&s.name, &wt, &env))?;
                    let (ws, pane) =
                        herdr_ids(&o).with_context(|| format!("herdr workspace create printed no workspace and pane ids: {}", o.trim()))?;
                    (pane, Some(ws))
                }
            };
            let r = Record {
                name: s.name.clone(),
                crew: plan.name.clone(),
                harness: s.harness.clone(),
                agent: agent.clone(),
                parent: parent.clone(),
                repo: root.to_string_lossy().into_owned(),
                worktree: wt_s,
                branch: s.branch.clone(),
                backend,
                pane,
                workspace,
                started_at: db::now(),
            };
            if let Err(e) = save(&r) {
                close(&r).with_context(|| format!("session record could not be saved ({e:#})"))?;
                return Err(e);
            }
            if backend == Backend::Herdr {
                launch_output(&herdr_run(&r.pane, &argv), "herdr pane run")
                    .with_context(|| format!("session start failed; use fox crew down {} to close its workspace", r.name))?;
            }
            Ok(r)
        })();
        let r = match launch {
            Ok(r) => r,
            Err(e) => {
                if !record_path(&s.name).exists() {
                    agents::finish(&w, &agent, "failed", None).with_context(|| format!("session start failed ({e:#})"))?;
                }
                return Err(e);
            }
        };
        let _ = writeln!(
            out,
            "{} {} {} in {} ({}{}), {} pane {}",
            r.name,
            r.agent,
            r.harness,
            rel(&root, &wt),
            r.branch,
            if fresh { ", new" } else { "" },
            backend.name(),
            r.pane
        );
    }
    let _ = writeln!(out, "fox crew ls; fox crew attach <name>; fox crew down [<name>] [--prune]");
    Ok(out)
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()
}

/// A missing pane is distinct from a multiplexer we could not query.
fn pane_state(r: &Record) -> Result<Option<String>> {
    if r.backend == Backend::Tmux && !tmux_owns_pane(r)? {
        return Ok(None);
    }
    let out = bounded_output(&mut command(&status_cmd(r)), PANE_TIMEOUT)?;
    if !out.status.success() {
        if (r.backend == Backend::Herdr && herdr_error(&out, "pane_not_found"))
            || (r.backend == Backend::Tmux && tmux_pane_missing(&out, &r.pane))
        {
            return Ok(None);
        }
        bail!("{} could not query pane {}", r.backend.name(), r.pane);
    }
    let output = String::from_utf8_lossy(&out.stdout);
    match r.backend {
        Backend::Tmux => {
            let state = output.trim();
            if state == "dead" || state.starts_with("dead ") {
                return Ok(None);
            }
            if state != "live" && !state.starts_with("live ") {
                bail!("tmux did not report a state for pane {}", r.pane);
            }
            Ok(Some(state.to_string()))
        }
        Backend::Herdr => {
            let v: Value = serde_json::from_str(&output)?;
            if find_obj(&v, "pane").and_then(|p| p.get("pane_id")).and_then(Value::as_str) != Some(r.pane.as_str()) {
                bail!("herdr did not identify pane {}", r.pane);
            }
            Ok(Some(herdr_status(&output)))
        }
    }
}

/// Consult the pane only after the agent's heartbeat and process evidence have expired.
pub(crate) fn pane_liveness(label: Option<&str>, agent: &str) -> Option<bool> {
    let Some(name) = label.and_then(|label| label.strip_prefix("crew:")) else { return Some(false) };
    if !valid_name(name) {
        return Some(false);
    }
    let r = match find(name) {
        Ok(r) => r,
        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => return Some(false),
        Err(_) => return None,
    };
    if r.agent != agent {
        return Some(false);
    }
    pane_state(&r).ok().map(|state| state.is_some())
}

/// `fox crew ls`: each session with its pane's state, then `fox agents --tree`.
pub fn ls() -> Result<String> {
    let rs = records();
    let mut out = String::new();
    if rs.is_empty() {
        out.push_str("no crew sessions (fox crew up <plan.toml|N>)\n");
    }
    for r in &rs {
        let state = pane_state(r).map(|state| state.unwrap_or_else(|| "closed".into())).unwrap_or_else(|_| "unknown".into());
        let _ = writeln!(
            out,
            "{} {} {} [{}] {} {} {}  {}",
            r.name,
            r.agent,
            r.harness,
            r.crew,
            r.backend.name(),
            r.pane,
            state,
            rel(Path::new(&r.repo), Path::new(&r.worktree))
        );
    }
    if let Ok(w) = db::open_rw() {
        agents::refresh(&w)?;
        out.push_str("\nagents:\n");
        out.push_str(&agents::list(&w, true, false)?);
    }
    Ok(out)
}

fn find(name: &str) -> Result<Record> {
    if !valid_name(name) {
        bail!("invalid crew session name {name:?}");
    }
    let s = std::fs::read_to_string(record_path(name)).with_context(|| format!("no crew session {name} (fox crew ls)"))?;
    Ok(serde_json::from_str(&s)?)
}

/// `fox crew attach <name>`: the last command replaces this process (tmux attach).
pub fn attach(name: &str) -> Result<String> {
    let r = find(name)?;
    if r.backend == Backend::Tmux && !tmux_owns_pane(&r)? {
        bail!("crew session {} has no pane", r.name);
    }
    let mut cmds = attach_cmds(&r, std::env::var_os("TMUX").is_some());
    let last = cmds.pop().unwrap_or_default();
    for c in &cmds {
        run_ok(c)?;
    }
    if r.backend == Backend::Tmux && std::env::var_os("TMUX").is_none() {
        use std::os::unix::process::CommandExt;
        let e = Command::new(&last[0]).args(&last[1..]).exec();
        bail!("cannot exec {}: {e}", last[0]);
    }
    run_ok(&last)?;
    Ok(format!("{} focused", r.name))
}

const TMUX_IDENTITY: &str = "#{pane_id}\t#{session_name}\t#{pane_start_path}";

fn tmux_owns_pane(r: &Record) -> Result<bool> {
    let cmd = ["tmux", "display-message", "-p", "-t", &r.pane, TMUX_IDENTITY].map(String::from);
    let out = bounded_output(&mut command(&cmd), PANE_TIMEOUT)?;
    let identity = format!("{}\t{TMUX_SESSION}\t{}", r.pane, r.worktree);
    if !out.status.success() {
        if tmux_pane_missing(&out, &r.pane) {
            return Ok(false);
        }
        bail!("tmux could not identify pane {}: {}", r.pane, String::from_utf8_lossy(&out.stderr).trim());
    }
    let actual = String::from_utf8_lossy(&out.stdout);
    if actual.trim_end_matches('\n') != identity {
        bail!("tmux pane {} does not match session {} and worktree {}", r.pane, r.name, r.worktree);
    }
    Ok(true)
}

fn tmux_pane_missing(out: &Output, pane: &str) -> bool {
    let err = String::from_utf8_lossy(&out.stderr);
    err.trim() == format!("can't find pane: {pane}")
        || err.starts_with("no server running on ")
        || (err.starts_with("error connecting to ") && err.contains("No such file or directory"))
}

fn herdr_error(out: &Output, code: &str) -> bool {
    [&out.stderr, &out.stdout].into_iter().any(|stream| {
        serde_json::from_slice::<Value>(stream).ok().is_some_and(|v| v.pointer("/error/code").and_then(Value::as_str) == Some(code))
    })
}

fn close(r: &Record) -> Result<bool> {
    if r.backend == Backend::Tmux && !tmux_owns_pane(r)? {
        return Ok(false);
    }
    let cmd = close_cmd(r);
    let o = exec(&cmd)?;
    if o.status.success() {
        return Ok(true);
    }
    if r.backend == Backend::Herdr && herdr_error(&o, "workspace_not_found") {
        return Ok(false);
    }
    bail!("{} failed: {}", shell_line(&cmd), String::from_utf8_lossy(&o.stderr).trim());
}

/// `fox crew down [<name>] [--prune]`: close the panes, mark the agents done, free their leases.
/// The worktree stays unless `prune` and it is clean. Without a name: every session of the repo
/// at `cwd` (every session when `cwd` is not in a repo).
pub fn down(name: Option<&str>, prune: bool, cwd: &Path) -> Result<String> {
    let _lock = lock()?;
    let targets: Vec<Record> = match name {
        Some(n) => vec![find(n)?],
        None => {
            let loc = project::locate(cwd);
            let root = loc.repo_root().to_string_lossy().into_owned();
            records().into_iter().filter(|r| loc.kind != "repo" || r.repo == root).collect()
        }
    };
    if targets.is_empty() {
        return Ok("no crew sessions here\n".into());
    }
    let mut out = String::new();
    for r in targets {
        let closed = close(&r).with_context(|| format!("could not close crew session {}; its record was kept", r.name))?;
        let wt = Path::new(&r.worktree);
        if let Ok(w) = db::open_rw() {
            if let Some(head) = gitsnap::snapshot(wt) {
                agents::set_commit(&w, &r.agent, None, Some(&head))?;
            }
            agents::finish(&w, &r.agent, "done", None)?;
        }
        let kept = if !prune || !wt.exists() {
            format!("worktree kept: {}", rel(Path::new(&r.repo), wt))
        } else if git(wt, &["status", "--porcelain", "--untracked-files=all"]).is_ok_and(|s| s.trim().is_empty()) {
            git(Path::new(&r.repo), &["worktree", "remove", &r.worktree])?;
            format!("worktree removed (branch {} kept)", r.branch)
        } else {
            format!("worktree kept, not clean: {}", rel(Path::new(&r.repo), wt))
        };
        std::fs::remove_file(record_path(&r.name))?;
        let _ = writeln!(out, "{} {} {}; {kept}", r.name, r.agent, if closed { "pane closed" } else { "pane already gone" });
    }
    Ok(out)
}

/// The herdr plugin's entry points (plugin/herdr/herdr-plugin.toml). `new`: one new session
/// for the workspace the action ran in. `worktree-created`: index the worktree herdr made.
pub fn herdr_hook(what: &str, harness: &str) -> Result<String> {
    match what {
        "new" => {
            let ctx: Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default()).unwrap_or(Value::Null);
            let cwd = find_str(&ctx, Some("workspace"), "cwd")
                .or_else(|| find_str(&ctx, None, "foreground_cwd"))
                .map(PathBuf::from)
                .context("HERDR_PLUGIN_CONTEXT_JSON names no workspace cwd")?;
            let taken: Vec<String> = records().into_iter().map(|r| r.name).collect();
            up(&numbered(1, harness, None, &taken)?, &cwd, Backend::Herdr)
        }
        "worktree-created" => {
            let ev: Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default()).unwrap_or(Value::Null);
            let path = find_obj(&ev, "worktree")
                .and_then(|worktree| {
                    ["path", "cwd"].iter().find_map(|key| worktree.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()))
                })
                .map(PathBuf::from)
                .context("HERDR_PLUGIN_EVENT_JSON names no worktree path")?;
            index(&path, Some(HOOK_INDEX_TIMEOUT))?;
            Ok(format!("indexed {}", path.display()))
        }
        _ => bail!("usage: fox crew herdr new|worktree-created"),
    }
}

pub const USAGE: &str = "usage: fox crew up <plan.toml|N> [--harness claude|codex] [--backend herdr|tmux] [--model M]
       fox crew ls | attach <name> | down [<name>] [--prune]";

/// `fox crew ...`. `pos[0]` is "crew"; `--prune` arrives as a positional.
pub fn run(pos: &[String], harness: &str, backend: Option<&str>, model: Option<&str>, cwd: &Path) -> Result<String> {
    let prune = pos.iter().any(|p| p == "--prune");
    let pos: Vec<&str> = pos.iter().map(String::as_str).filter(|p| *p != "--prune").collect();
    match pos.get(1..).unwrap_or_default() {
        ["up", what] => {
            let plan = match what.parse::<usize>() {
                Ok(n) => numbered(n, harness, model, &records().into_iter().map(|r| r.name).collect::<Vec<_>>())?,
                Err(_) => {
                    let f = Path::new(what);
                    let text = std::fs::read_to_string(f).with_context(|| format!("cannot read {what}"))?;
                    parse(&text, f, harness)?
                }
            };
            up(&plan, cwd, pick_backend(backend)?)
        }
        ["ls"] => ls(),
        ["attach", name] => attach(name),
        ["down"] => down(None, prune, cwd),
        ["down", name] => down(Some(*name), prune, cwd),
        ["herdr", what] => herdr_hook(what, harness),
        _ => bail!("{USAGE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = r#"
name = "auth"
harness = "codex"
model = "m-default"

[[session]]
name = "api"
prompt = "Add rate limiting"

[[session]]
name = "ui"
harness = "claude"
model = "haiku"
branch = "feat/ui"
writer = true
args = ["--permission-mode", "acceptEdits"]
"#;

    fn spec(h: &str, model: Option<&str>, prompt: Option<&str>) -> Spec {
        Spec {
            name: "api".into(),
            harness: h.into(),
            model: model.map(String::from),
            branch: "crew/api".into(),
            prompt: prompt.map(String::from),
            writer: false,
            args: vec![],
        }
    }

    #[test]
    fn plan_fills_defaults_from_the_plan_then_the_flag() {
        let p = parse(PLAN, Path::new("x/plan.toml"), "claude").unwrap();
        assert_eq!(p.name, "auth");
        assert_eq!(
            p.sessions[0],
            Spec {
                name: "api".into(),
                harness: "codex".into(),
                model: Some("m-default".into()),
                branch: "crew/api".into(),
                prompt: Some("Add rate limiting".into()),
                writer: false,
                args: vec![],
            }
        );
        let ui = &p.sessions[1];
        assert_eq!((ui.harness.as_str(), ui.model.as_deref(), ui.branch.as_str(), ui.writer), ("claude", Some("haiku"), "feat/ui", true));
        let bare = parse("[[session]]\nname = \"a\"\n", Path::new("dir/team.toml"), "codex").unwrap();
        assert_eq!((bare.name.as_str(), bare.sessions[0].harness.as_str(), bare.sessions[0].prompt.as_ref()), ("team", "codex", None));
    }

    #[test]
    fn plan_rejects_bad_sessions() {
        let bad = |t: &str| parse(t, Path::new("p.toml"), "claude").unwrap_err().to_string();
        assert!(bad("name = \"x\"\n").contains("no [[session]]"));
        assert!(bad("[[session]]\nname = \"a\"\n[[session]]\nname = \"a\"\n").contains("unique"));
        assert!(bad("[[session]]\nname = \"a b\"\n").contains("session name"));
        assert!(bad("[[session]]\nname = \"a\"\nharness = \"gemini\"\n").contains("claude or codex"));
        assert!(bad("[[session]]\nname = \"a\"\nwriter = true\n[[session]]\nname = \"b\"\nwriter = true\n").contains("one session"));
        assert!(bad("[[session]]\nname = \"a\"\nsystem_prompt = \"x\"\n").contains("not a crew plan"));
        for f in ["-p", "--print", "exec", "--"] {
            assert!(bad(&format!("[[session]]\nname = \"a\"\nargs = [\"{f}\"]\n")).contains("permission-mode"), "{f}");
        }
        for f in ["--bare", "--system-prompt", "--append-system-prompt=x"] {
            assert!(bad(&format!("[[session]]\nname = \"a\"\nargs = [\"{f}\"]\n")).contains("system prompt"), "{f}");
        }
    }

    #[test]
    fn numbered_sessions_skip_taken_names() {
        let p = numbered(2, "codex", Some("m"), &["s1".into()]).unwrap();
        let names: Vec<&str> = p.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["s2", "s3"]);
        assert_eq!((p.sessions[0].branch.as_str(), p.sessions[0].model.as_deref()), ("crew/s2", Some("m")));
        assert!(numbered(0, "claude", None, &[]).is_err());
        assert!(numbered(1, "gemini", None, &[]).is_err());
    }

    #[test]
    fn env_is_the_three_foxdb_variables() {
        let e = session_env("cl-7", "crew:auth", Path::new("/store"));
        let want: Vec<(String, String)> = [("FOXDB_AGENT", "cl-7"), ("FOXDB_PARENT", "crew:auth"), ("FOXDB_HOME", "/store")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(e, want);
    }

    fn assert_native(argv: &[String]) {
        for a in argv {
            assert!(!FORBIDDEN.contains(&a.as_str()), "{a} in {argv:?}");
            assert!(!["-p", "--print", "exec"].contains(&a.as_str()), "{a}: crew starts the interactive CLI, {argv:?}");
        }
    }

    #[test]
    fn claude_gets_the_prompt_as_its_initial_argument() {
        let v = cli_argv(&spec("claude", Some("haiku"), Some("fix the login bug")));
        assert_eq!(v, ["claude", "--model", "haiku", "--", "fix the login bug"]);
        assert_native(&v);
        assert_eq!(cli_argv(&spec("claude", None, None)), ["claude"]);
    }

    #[test]
    fn codex_gets_the_prompt_as_its_initial_argument() {
        let v = cli_argv(&spec("codex", Some("gpt"), Some("fix it")));
        assert_eq!(v, ["codex", "-m", "gpt", "--", "fix it"]);
        assert_native(&v);
    }

    #[test]
    fn a_prompt_that_looks_like_a_flag_follows_a_double_dash() {
        let mut s = spec("claude", None, Some("--bare is not a flag here"));
        s.args = vec!["--permission-mode".into(), "acceptEdits".into()];
        assert_eq!(cli_argv(&s), ["claude", "--permission-mode", "acceptEdits", "--", "--bare is not a flag here"]);
    }

    fn env() -> Vec<(String, String)> {
        session_env("cl-1", "crew:x", Path::new("/fx"))
    }

    #[test]
    fn tmux_runs_the_cli_directly_in_a_detached_window() {
        let argv = cli_argv(&spec("claude", None, Some("go")));
        let first = tmux_launch(false, "api", Path::new("/r/.claude/worktrees/crew-api"), &env(), &argv);
        let mut want: Vec<&str> = vec![
            "tmux",
            "new-session",
            "-d",
            "-s",
            "fox-crew",
            "-n",
            "api",
            "-c",
            "/r/.claude/worktrees/crew-api",
            "-e",
            "FOXDB_AGENT=cl-1",
            "-e",
            "FOXDB_PARENT=crew:x",
            "-e",
            "FOXDB_HOME=/fx",
            "-P",
            "-F",
            "#{pane_id}",
            "env",
        ];
        for key in SESSION_MARKERS {
            want.extend(["-u", key]);
        }
        want.extend(["claude", "--", "go"]);
        assert_eq!(first, want);
        let next = tmux_launch(true, "api", Path::new("/w"), &env(), &argv);
        assert_eq!(&next[..5], ["tmux", "new-window", "-d", "-t", "fox-crew:"]);
        assert_eq!(next.last().map(String::as_str), Some("go"));
        assert_native(&next);
    }

    #[test]
    fn herdr_creates_a_workspace_with_the_env_then_runs_the_cli_in_its_pane() {
        let c = herdr_create("api", Path::new("/w"), &env());
        let want: Vec<&str> = vec![
            "herdr",
            "workspace",
            "create",
            "--cwd",
            "/w",
            "--label",
            "api",
            "--env",
            "FOXDB_AGENT=cl-1",
            "--env",
            "FOXDB_PARENT=crew:x",
            "--env",
            "FOXDB_HOME=/fx",
            "--no-focus",
        ];
        assert_eq!(c, want);
        let r = herdr_run("w2:p1", &cli_argv(&spec("codex", None, Some("it's done?"))));
        assert_eq!(r, ["herdr", "pane", "run", "w2:p1", r"codex -- 'it'\''s done?'"]);
        assert_native(&r);
    }

    #[test]
    fn herdr_json_gives_ids_and_status() {
        let out = r#"{"id":"1","result":{"workspace":{"workspace_id":"w3","label":"api"},"tab":{"tab_id":"w3:t1"},"root_pane":{"pane_id":"w3:p1"}}}"#;
        assert_eq!(herdr_ids(out), Some(("w3".into(), "w3:p1".into())));
        assert_eq!(herdr_ids("not json"), None);
        assert_eq!(herdr_status(r#"{"result":{"pane":{"pane_id":"w3:p1","agent":{"label":"claude","status":"working"}}}}"#), "working");
        assert_eq!(herdr_status(r#"{"result":{"pane":{"pane_id":"w3:p1"}}}"#), "open");
    }

    #[test]
    fn close_status_and_attach_commands_per_backend() {
        let mut r = Record {
            name: "api".into(),
            crew: "c".into(),
            harness: "claude".into(),
            agent: "cl-1".into(),
            parent: "crew:c".into(),
            repo: "/r".into(),
            worktree: "/r/w".into(),
            branch: "crew/api".into(),
            backend: Backend::Tmux,
            pane: "%4".into(),
            workspace: None,
            started_at: 0,
        };
        assert_eq!(close_cmd(&r), ["tmux", "kill-pane", "-t", "%4"]);
        assert_eq!(&status_cmd(&r)[..5], ["tmux", "display-message", "-p", "-t", "%4"]);
        assert_eq!(attach_cmds(&r, true), [["tmux", "switch-client", "-t", "%4"]]);
        assert_eq!(attach_cmds(&r, false)[1], ["tmux", "attach-session", "-t", "fox-crew"]);
        r.backend = Backend::Herdr;
        r.pane = "w3:p1".into();
        r.workspace = Some("w3".into());
        assert_eq!(close_cmd(&r), ["herdr", "workspace", "close", "w3"]);
        assert_eq!(status_cmd(&r), ["herdr", "pane", "get", "w3:p1"]);
        assert_eq!(attach_cmds(&r, false), [["herdr", "workspace", "focus", "w3"]]);
    }

    #[test]
    fn backend_flag_wins_and_unknown_is_an_error() {
        assert_eq!(pick_backend(Some("tmux")).unwrap(), Backend::Tmux);
        assert_eq!(pick_backend(Some("herdr")).unwrap(), Backend::Herdr);
        assert!(pick_backend(Some("zellij")).is_err());
    }
}
