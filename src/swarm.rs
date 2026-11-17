//! `fox swarm run <plan.toml>`: run a set of dispatched tasks with dependencies and a
//! concurrency cap, collect their reports into one summary handoff, and optionally put a
//! reviewer panel over the result. `fox swarm status <swarm>` shows where it stands.
//!
//! Every task goes through `fox dispatch` (`orch::dispatch`), so writer leases, the depth cap and
//! the per-parent child cap apply unchanged. A swarm is its plan name in its plan's directory; a
//! task's key is a hash of the swarm and the task's own fields, and the worker carries it as its
//! label (`swarm:<key>`). A re-run, after a crash or on purpose, finds the worker by that key and
//! adopts it instead of starting another, so a task is never dispatched twice. Runs of one swarm
//! at the same time take a lock around each start and each post, so they never both act. Editing
//! a task changes its key, and only that task runs again.
//!
//! Plan format (TOML):
//!
//! ```toml
//! name = "audit"            # the swarm's name; default: the file name
//! concurrency = 3           # most tasks running at once (also capped by FOXDB_MAX_CHILDREN)
//!
//! [[task]]
//! id = "scan-api"
//! harness = "codex"         # codex | claude
//! task = "List every handler that ..."   # text, or h:ID of a handoff
//! repo = "."                # relative to the plan file; default "."
//! writer = false            # take the repo's writer lease
//! reviewer = false          # task only, no brief
//! depends_on = []           # ids; their report handles are appended to this task
//!
//! [review]                  # optional reviewer panel over the collected reports
//! harnesses = ["claude", "codex", "claude"]
//! task = "Check the reports for ..."
//! ```

use crate::{agents, db, handoff, orch, project};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    #[serde(default)]
    pub name: String,
    #[serde(default = "one")]
    pub concurrency: usize,
    #[serde(default, rename = "task")]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub review: Option<Review>,
}

fn one() -> usize {
    1
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub harness: String,
    pub task: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub writer: bool,
    #[serde(default)]
    pub reviewer: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// One reviewer per entry. An odd count avoids ties.
    pub harnesses: Vec<String>,
    pub task: String,
    #[serde(default)]
    pub model: Option<String>,
}

/// The swarm tables (schema v13).
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS swarm(
  id TEXT PRIMARY KEY, name TEXT NOT NULL, plan TEXT NOT NULL, dir TEXT NOT NULL, created_at INTEGER NOT NULL,
  summary INTEGER, verdict INTEGER) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS swarm_task(
  swarm TEXT NOT NULL, task TEXT NOT NULL, key TEXT NOT NULL, agent TEXT, error TEXT, PRIMARY KEY(swarm, task)) WITHOUT ROWID;
"#;

fn fnv(parts: &[&str]) -> String {
    let h = parts.iter().fold(0xcbf29ce484222325u64, |h, p| {
        p.bytes().chain(std::iter::once(0u8)).fold(h, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
    });
    format!("{h:016x}")
}

/// The swarm's id: stable for a plan name in a directory. The directory is part of it because
/// the name defaults to the file name, and two `plan.toml` files elsewhere are two swarms.
pub fn swarm_id(name: &str, dir: &Path) -> String {
    format!("sw-{}", &fnv(&["swarm", name, &dir.to_string_lossy()])[..10])
}

/// A task's idempotency key: the swarm and every field that changes what the worker does.
pub fn task_key(swarm: &str, t: &Task, repo: &Path) -> String {
    let repo = repo.to_string_lossy();
    fnv(&[
        swarm,
        &t.id,
        &t.harness,
        &t.task,
        &repo,
        if t.writer { "w" } else { "" },
        if t.reviewer { "r" } else { "" },
        t.model.as_deref().unwrap_or(""),
    ])
}

/// An exclusive lock on one swarm, held across a check-then-act step (adopt or dispatch a task,
/// post the summary, post the verdict), so two runs of one plan at once never both act. It is
/// an `flock`, so a crashed holder releases it. Dispatch writes through its own connection,
/// which is why this is not a SQLite transaction. The lock lasts as long as the returned file.
fn lock(swarm: &str) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let dir = db::home().join("swarm");
    std::fs::create_dir_all(&dir)?;
    let name: String = swarm.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(format!("{name}.lock")))?;
    // SAFETY: flock on a descriptor this File owns; it is released when the File closes.
    while unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e).context("cannot lock the swarm");
        }
    }
    Ok(f)
}

fn label(key: &str) -> String {
    format!("swarm:{key}")
}

/// Parse and check a plan: unique ids, known dependencies, no cycles, a review panel that is
/// not empty. `file` names the default swarm name.
pub fn parse(text: &str, file: &Path) -> Result<Plan> {
    let mut p: Plan = toml::from_str(text).with_context(|| format!("{} is not a swarm plan", file.display()))?;
    if p.name.trim().is_empty() {
        p.name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "swarm".into());
    }
    if p.tasks.is_empty() {
        bail!("the plan has no [[task]]");
    }
    p.concurrency = p.concurrency.max(1);
    let ids: HashSet<&str> = p.tasks.iter().map(|t| t.id.as_str()).collect();
    if ids.len() != p.tasks.len() {
        bail!("task ids must be unique");
    }
    for t in &p.tasks {
        if !matches!(t.harness.as_str(), "codex" | "claude") {
            bail!("task {}: harness must be codex or claude", t.id);
        }
        if t.writer && t.reviewer {
            bail!("task {}: a reviewer cannot be a writer", t.id);
        }
        if let Some(d) = t.depends_on.iter().find(|d| !ids.contains(d.as_str())) {
            bail!("task {}: unknown dependency {d}", t.id);
        }
    }
    // Kahn's algorithm: every task must become ready at some point.
    let mut left: Vec<&Task> = p.tasks.iter().collect();
    let mut done: HashSet<&str> = HashSet::new();
    while !left.is_empty() {
        let before = left.len();
        left.retain(|t| {
            if t.depends_on.iter().all(|d| done.contains(d.as_str())) {
                done.insert(&t.id);
                false
            } else {
                true
            }
        });
        if left.len() == before {
            bail!("dependency cycle among {}", left.iter().map(|t| t.id.as_str()).collect::<Vec<_>>().join(", "));
        }
    }
    if let Some(r) = &p.review {
        if r.harnesses.is_empty() {
            bail!("[review] needs at least one harness");
        }
        if let Some(h) = r.harnesses.iter().find(|h| !matches!(h.as_str(), "codex" | "claude")) {
            bail!("[review]: unknown harness {h}");
        }
    }
    Ok(p)
}

/// Where a task stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Pending,
    Running,
    /// Finished with a usable result (done, partial, flagged).
    Ok(String),
    /// Finished without one (failed, blocked, stale), or could not start.
    Failed(String),
    /// A dependency failed, so it never started.
    Skipped,
}

impl State {
    fn terminal(&self) -> bool {
        !matches!(self, State::Pending | State::Running)
    }

    fn word(&self) -> String {
        match self {
            State::Pending => "pending".into(),
            State::Running => "running".into(),
            State::Ok(s) | State::Failed(s) => s.clone(),
            State::Skipped => "skipped".into(),
        }
    }
}

fn agent_state(a: &agents::Agent) -> State {
    match a.status.as_str() {
        "running" => State::Running,
        "done" | "partial" | "flagged" => State::Ok(a.status.clone()),
        s => State::Failed(s.to_string()),
    }
}

/// One task's row: (task id, key, agent, error).
/// A task's stored row: key, worker, start error.
type TaskRow = (String, Option<String>, Option<String>);

fn rows(c: &Connection, sw: &str) -> Result<HashMap<String, TaskRow>> {
    let mut q = c.prepare("SELECT task, key, agent, error FROM swarm_task WHERE swarm = ?1")?;
    let m = q.query_map([sw], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?.flatten().collect();
    Ok(m)
}

/// The worker that carries `key`, if one was ever started (the newest).
fn adopt(c: &Connection, key: &str) -> Result<Option<String>> {
    Ok(c.query_row("SELECT id FROM agent WHERE label = ?1 ORDER BY started_at DESC, id DESC LIMIT 1", [label(key)], |r| r.get(0))
        .optional()?)
}

struct Run<'a> {
    w: &'a Connection,
    id: String,
    plan: &'a Plan,
    dir: PathBuf,
    parent: String,
    keys: BTreeMap<String, (String, PathBuf)>,
}

impl Run<'_> {
    /// Each task's state now, adopting workers recorded only by their label.
    fn states(&self) -> Result<BTreeMap<String, State>> {
        agents::refresh(self.w)?;
        let rs = rows(self.w, &self.id)?;
        let mut out = BTreeMap::new();
        for t in &self.plan.tasks {
            let key = &self.keys[&t.id].0;
            let (agent, err) = match rs.get(&t.id) {
                Some((k, a, e)) if k == key => (a.clone(), e.clone()),
                _ => (None, None),
            };
            let agent = match agent {
                Some(a) => Some(a),
                None => {
                    let a = adopt(self.w, key)?;
                    if a.is_some() {
                        self.record(&t.id, key, a.as_deref(), None)?;
                    }
                    a
                }
            };
            let st = match (agent.and_then(|a| agents::get(self.w, &a)), err) {
                (Some(a), _) => agent_state(&a),
                (None, Some(e)) => State::Failed(format!("error: {e}")),
                (None, None) => State::Pending,
            };
            out.insert(t.id.clone(), st);
        }
        // Dependents of a failed or skipped task never start.
        loop {
            let mut changed = false;
            for t in &self.plan.tasks {
                if out[&t.id] == State::Pending && t.depends_on.iter().any(|d| matches!(out[d], State::Failed(_) | State::Skipped)) {
                    out.insert(t.id.clone(), State::Skipped);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Ok(out)
    }

    fn record(&self, task: &str, key: &str, agent: Option<&str>, err: Option<&str>) -> Result<()> {
        self.w.execute(
            "INSERT INTO swarm_task(swarm, task, key, agent, error) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(swarm, task) DO UPDATE SET key = excluded.key, agent = excluded.agent, error = excluded.error",
            params![self.id, task, key, agent, err],
        )?;
        Ok(())
    }

    /// The task text a worker gets: the plan's, then its dependencies' report handles.
    fn text(&self, t: &Task) -> Result<String> {
        if t.depends_on.is_empty() {
            return Ok(t.task.clone());
        }
        let rs = rows(self.w, &self.id)?;
        let mut s = format!("{}\n\nInputs from earlier tasks of this swarm (read them with fox_read):", t.task.trim_end());
        for d in &t.depends_on {
            let rep = rs.get(d).and_then(|r| r.1.as_deref()).and_then(|a| agents::get(self.w, a)).and_then(|a| a.report);
            let _ = write!(s, "\n- {d}: {}", rep.map(handoff::handle).unwrap_or_else(|| "no report".into()));
        }
        Ok(s)
    }

    /// Start `t`. Ok(None): started (or adopted). Ok(Some(holder)): it must wait for `holder`
    /// (a writer lease or the child cap).
    fn start(&self, t: &Task) -> Result<Option<String>> {
        let (key, repo) = &self.keys[&t.id];
        // Another run of this swarm may have started it since our states were read.
        let _held = lock(&self.id)?;
        if let Some(a) = adopt(self.w, key)? {
            self.record(&t.id, key, Some(&a), None)?;
            return Ok(None);
        }
        let text = self.text(t)?;
        let lbl = label(key);
        let r = orch::dispatch(&orch::Dispatch {
            harness: &t.harness,
            task: &text,
            repo,
            writer: t.writer,
            label: Some(&lbl),
            model: t.model.as_deref(),
            packet: None,
            parent: Some(&self.parent),
            reviewer: t.reviewer,
        });
        match r {
            Ok(s) => {
                self.record(&t.id, key, Some(&s.id), None)?;
                Ok(None)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if let Some(holder) = msg.split("held by ").nth(1) {
                    return Ok(Some(holder.trim().to_string()));
                }
                if msg.contains("running children") {
                    return Ok(Some(String::new()));
                }
                self.record(&t.id, key, None, Some(&msg))?;
                Ok(None)
            }
        }
    }
}

pub struct Outcome {
    pub id: String,
    /// Every task (and the panel, if any) finished.
    pub finished: bool,
    /// A task failed, was skipped, or the panel rejected.
    pub bad: bool,
    pub text: String,
}

fn plan_dir(file: &Path) -> PathBuf {
    let d = file.parent().map(Path::to_path_buf).unwrap_or_default();
    if d.as_os_str().is_empty() { PathBuf::from(".") } else { d }
}

/// `fox swarm run`: start what is ready, wait, repeat until every task finished (or, with
/// `no_wait`, after starting the ready ones), then collect and review.
pub fn run(file: &Path, no_wait: bool, timeout: Duration) -> Result<Outcome> {
    let text = std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    let plan = parse(&text, file)?;
    let dir = std::fs::canonicalize(plan_dir(file))?;
    let w = db::open_rw()?;
    let id = swarm_id(&plan.name, &dir);
    w.execute(
        "INSERT INTO swarm(id, name, plan, dir, created_at) VALUES (?1,?2,?3,?4,?5)
         ON CONFLICT(id) DO UPDATE SET plan = excluded.plan, dir = excluded.dir",
        params![id, plan.name, text, dir.to_string_lossy(), db::now()],
    )?;
    let mut keys = BTreeMap::new();
    for t in &plan.tasks {
        let repo = dir.join(t.repo.as_deref().unwrap_or("."));
        let repo = std::fs::canonicalize(&repo).with_context(|| format!("task {}: repo {}", t.id, repo.display()))?;
        keys.insert(t.id.clone(), (task_key(&id, t, &repo), repo));
    }
    let parent = crate::cli::whoami(&w);
    let run = Run { w: &w, id: id.clone(), plan: &plan, dir, parent, keys };
    let cap = plan.concurrency.min(std::env::var("FOXDB_MAX_CHILDREN").ok().and_then(|v| v.parse().ok()).unwrap_or(8)).max(1);
    let start = Instant::now();
    loop {
        let st = run.states()?;
        let mut running: Vec<String> = vec![];
        let rs = rows(&w, &id)?;
        for (t, s) in &st {
            if *s == State::Running
                && let Some(a) = rs.get(t).and_then(|r| r.1.clone())
            {
                running.push(a);
            }
        }
        let mut blocker: Option<String> = None;
        let mut started = false;
        for t in &plan.tasks {
            if running.len() >= cap {
                break;
            }
            if st[&t.id] != State::Pending || !t.depends_on.iter().all(|d| matches!(st[d], State::Ok(_))) {
                continue;
            }
            match run.start(t)? {
                None => {
                    started = true;
                    if let Some(a) = rows(&w, &id)?.get(&t.id).and_then(|r| r.1.clone()) {
                        running.push(a);
                    }
                }
                Some(h) => {
                    blocker.get_or_insert(h);
                }
            }
        }
        if started {
            continue; // re-read states: a start may have failed or finished at once
        }
        if st.values().all(State::terminal) {
            break;
        }
        if no_wait {
            return Ok(Outcome { id: id.clone(), finished: false, bad: false, text: status(&w, &id)? });
        }
        let left = timeout.saturating_sub(start.elapsed());
        if left.is_zero() {
            return Ok(Outcome { id: id.clone(), finished: false, bad: false, text: status(&w, &id)? });
        }
        // Wait for one of ours, or for whoever holds the lease we need.
        let mut targets = running.clone();
        if let Some(h) = blocker.filter(|h| !h.is_empty() && !targets.contains(h)) {
            targets.push(h);
        }
        if targets.is_empty() {
            // Blocked by the child cap on agents outside this swarm: check again shortly.
            std::thread::sleep(Duration::from_secs(2).min(left));
            continue;
        }
        if orch::wait(&targets, true, left.min(Duration::from_secs(3600))).is_err() {
            // A lease holder that is not a registered agent: check again shortly.
            std::thread::sleep(Duration::from_secs(2).min(left));
        }
    }
    let summary = collect(&run)?;
    let mut bad = run.states()?.values().any(|s| matches!(s, State::Failed(_) | State::Skipped));
    if let Some(r) = &plan.review {
        match panel(&run, r, summary, no_wait, timeout.saturating_sub(start.elapsed()))? {
            Some(approved) => bad |= !approved,
            None => return Ok(Outcome { id: id.clone(), finished: false, bad, text: status(&w, &id)? }),
        }
    }
    Ok(Outcome { id: id.clone(), finished: true, bad, text: status(&w, &id)? })
}

/// The summary handoff: one line per task with its state and report. Posted once per swarm
/// (a re-run returns the same one).
fn collect(run: &Run) -> Result<i64> {
    let _held = lock(&run.id)?;
    if let Some(h) = run.w.query_row("SELECT summary FROM swarm WHERE id = ?1", [&run.id], |r| r.get::<_, Option<i64>>(0))? {
        return Ok(h);
    }
    let st = run.states()?;
    let rs = rows(run.w, &run.id)?;
    let ok = st.values().filter(|s| matches!(s, State::Ok(_))).count();
    let mut body = format!("Swarm {} ({}): {} of {} tasks finished well.\n", run.plan.name, run.id, ok, st.len());
    for t in &run.plan.tasks {
        let a = rs.get(&t.id).and_then(|r| r.1.clone()).and_then(|a| agents::get(run.w, &a));
        let _ = write!(body, "\n- {} [{}]", t.id, st[&t.id].word());
        if let Some(a) = &a {
            let _ = write!(body, " {} {}", a.id, a.harness);
            if let Some(r) = a.report {
                let first: String = run
                    .w
                    .query_row("SELECT body FROM handoff WHERE id = ?1", [r], |x| x.get::<_, String>(0))
                    .unwrap_or_default()
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect();
                let _ = write!(body, " report {}: {first}", handoff::handle(r));
            }
        }
    }
    let loc = project::locate(&run.dir);
    let p = project::ensure(run.w, &loc)?;
    let title = format!("swarm {}: {ok}/{} ok", run.plan.name, st.len());
    let h = handoff::post(
        run.w,
        &handoff::Post {
            project_id: p.id,
            kind: "report",
            text: &body,
            title: Some(&title),
            agent: Some(&run.parent),
            topic: Some("swarm"),
            ..Default::default()
        },
    )?;
    run.w.execute("UPDATE swarm SET summary = ?2 WHERE id = ?1", params![run.id, h])?;
    Ok(h)
}

/// The reviewer panel: one reviewer per listed harness, each reading the summary. A reviewer
/// approves with report status `done`; anything else rejects. The verdict is the majority and
/// is posted as a decision under the summary. None: still running (only with `no_wait`).
fn panel(run: &Run, r: &Review, summary: i64, no_wait: bool, timeout: Duration) -> Result<Option<bool>> {
    let hs = handoff::handle(summary);
    let text = format!(
        "{}\n\nThe swarm's collected reports are in {hs} (fox_read target={hs}); read each report it lists.\n\
         Finish with status \"done\" to approve, or \"blocked\" to reject, and give the reasons in the summary.",
        r.task.trim_end()
    );
    let tasks: Vec<Task> = r
        .harnesses
        .iter()
        .enumerate()
        .map(|(i, h)| Task {
            id: format!("review-{}", i + 1),
            harness: h.clone(),
            task: text.clone(),
            repo: None,
            writer: false,
            reviewer: true,
            model: r.model.clone(),
            depends_on: vec![],
        })
        .collect();
    let mut keys = BTreeMap::new();
    for t in &tasks {
        keys.insert(t.id.clone(), (task_key(&run.id, t, &run.dir), run.dir.clone()));
    }
    let sub = Plan { name: run.plan.name.clone(), concurrency: tasks.len(), tasks, review: None };
    let pr = Run { w: run.w, id: format!("{}:review", run.id), plan: &sub, dir: run.dir.clone(), parent: run.parent.clone(), keys };
    let start = Instant::now();
    loop {
        let st = pr.states()?;
        let mut started = false;
        for t in &sub.tasks {
            if st[&t.id] == State::Pending && pr.start(t)?.is_none() {
                started = true;
            }
        }
        if started {
            continue;
        }
        if st.values().all(State::terminal) {
            break;
        }
        let left = timeout.saturating_sub(start.elapsed());
        if no_wait || left.is_zero() {
            return Ok(None);
        }
        // Wait only for reviewers still running: a finished one would end every wait at once.
        let rs = rows(run.w, &pr.id)?;
        let running: Vec<String> =
            st.iter().filter(|(_, s)| **s == State::Running).filter_map(|(t, _)| rs.get(t).and_then(|r| r.1.clone())).collect();
        if running.is_empty() || orch::wait(&running, true, left.min(Duration::from_secs(3600))).is_err() {
            // Every start was held back by the child cap on agents outside the panel.
            std::thread::sleep(Duration::from_secs(2).min(left));
        }
    }
    let st = pr.states()?;
    let yes = st.values().filter(|s| **s == State::Ok("done".into())).count();
    let n = st.len();
    let approved = yes * 2 > n;
    let _held = lock(&run.id)?;
    if run.w.query_row("SELECT verdict FROM swarm WHERE id = ?1", [&run.id], |r| r.get::<_, Option<i64>>(0))?.is_none() {
        let rs = rows(run.w, &pr.id)?;
        let mut body = format!("Review panel over {hs}: {} by {yes} of {n}.\n", if approved { "approved" } else { "rejected" });
        for t in &sub.tasks {
            let a = rs.get(&t.id).and_then(|r| r.1.clone()).and_then(|a| agents::get(run.w, &a));
            let rep = a.as_ref().and_then(|a| a.report).map(handoff::handle).unwrap_or_else(|| "no report".into());
            let _ = write!(body, "\n- {} [{}] {}", t.id, st[&t.id].word(), rep);
        }
        let loc = project::locate(&run.dir);
        let p = project::ensure(run.w, &loc)?;
        let title = format!("swarm {} review: {} {yes}/{n}", run.plan.name, if approved { "approved" } else { "rejected" });
        let h = handoff::post(
            run.w,
            &handoff::Post {
                project_id: p.id,
                kind: "decision",
                text: &body,
                title: Some(&title),
                agent: Some(&run.parent),
                topic: Some("swarm"),
                parent: Some(summary),
                ..Default::default()
            },
        )?;
        run.w.execute("UPDATE swarm SET verdict = ?2 WHERE id = ?1", params![run.id, h])?;
    }
    Ok(Some(approved))
}

/// A swarm's stored row: id, name, plan text, plan directory, summary, verdict.
type SwarmRow = (String, String, String, String, Option<i64>, Option<i64>);

/// `fox swarm status <swarm>`: by id (`sw-…`) or plan name. A name that several swarms share
/// (one file name in several directories) is refused with their ids.
pub fn status(c: &Connection, which: &str) -> Result<String> {
    let mut q = c.prepare(
        "SELECT id, name, plan, dir, summary, verdict FROM swarm WHERE id = ?1 OR name = ?1 ORDER BY id = ?1 DESC, created_at DESC",
    )?;
    let found: Vec<SwarmRow> = q
        .query_map([which], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if found.len() > 1 && found[0].0 != which {
        let ids: Vec<String> = found.iter().map(|r| format!("{} ({})", r.0, r.3)).collect();
        bail!("{} swarms are named {which}: {}; name one by id", found.len(), ids.join(", "));
    }
    let Some((id, name, text, dir, summary, verdict)) = found.into_iter().next() else { bail!("no swarm {which}") };
    let plan = parse(&text, Path::new(&format!("{name}.toml")))?;
    let dir = PathBuf::from(dir);
    let mut keys = BTreeMap::new();
    for t in &plan.tasks {
        let repo = std::fs::canonicalize(dir.join(t.repo.as_deref().unwrap_or("."))).unwrap_or_else(|_| dir.clone());
        keys.insert(t.id.clone(), (task_key(&id, t, &repo), repo));
    }
    let run = Run { w: c, id: id.clone(), plan: &plan, dir: dir.clone(), parent: String::new(), keys };
    let st = run.states()?;
    let rs = rows(c, &id)?;
    let count = |f: &dyn Fn(&State) -> bool| st.values().filter(|s| f(s)).count();
    let mut out = format!(
        "swarm {id} {name}: {} tasks, {} ok, {} running, {} pending, {} failed or skipped (cap {})\n",
        st.len(),
        count(&|s| matches!(s, State::Ok(_))),
        count(&|s| *s == State::Running),
        count(&|s| *s == State::Pending),
        count(&|s| matches!(s, State::Failed(_) | State::Skipped)),
        plan.concurrency
    );
    for t in &plan.tasks {
        let _ = write!(out, "  {} ", t.id);
        let a = rs.get(&t.id).and_then(|r| r.1.clone()).and_then(|a| agents::get(c, &a));
        match &a {
            Some(a) => {
                let _ = write!(out, "{} {} {}", a.id, a.harness, st[&t.id].word());
                if let Some(r) = a.report {
                    let _ = write!(out, " report {}", handoff::handle(r));
                }
            }
            None => {
                let _ = write!(out, "{}", st[&t.id].word());
                if st[&t.id] == State::Pending && !t.depends_on.is_empty() {
                    let _ = write!(out, " (after {})", t.depends_on.join(", "));
                }
            }
        }
        out.push('\n');
    }
    if let Some(s) = summary {
        let _ = write!(out, "summary {}", handoff::handle(s));
        if let Some(v) = verdict {
            let title: String = c.query_row("SELECT title FROM handoff WHERE id = ?1", [v], |r| r.get(0)).unwrap_or_default();
            let _ = write!(out, "  {title} ({})", handoff::handle(v));
        }
        out.push('\n');
    }
    Ok(out)
}
