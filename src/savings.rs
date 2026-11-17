//! Token-savings ledger. Each answer is compared with the naive alternative: reading every file
//! the answer cites, whole (~4 chars per token). Totals live in the `saving` table per day and
//! tool; `fox stats` reports them, and a coarse weekly line ends the session brief. Tool answers
//! never carry a note: identical answers stay byte-identical (prompt caches, headroom dedup).

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;

const TEXT_EXTS: &[&str] = &["md", "txt", "json", "yaml", "yml", "toml", "xml", "gradle", "html", "css", "sql", "properties"];

pub fn tokens(bytes: usize) -> i64 {
    bytes.div_ceil(4) as i64
}

/// Repo-relative file paths an answer cites: `path:line`, `path (n)` headers, bare paths.
pub fn cited_paths(out: &str) -> Vec<String> {
    let re = regex::Regex::new(r"(?:^|\s)([A-Za-z0-9_@+\-][A-Za-z0-9_@+\-./]*\.[A-Za-z0-9]{1,8})(?::\d+|\s\(\d+\)|\s|$)").unwrap();
    let mut seen = HashSet::new();
    let mut v = vec![];
    // `dir/` headers followed by ` file (n)` rows (see tscode::group_by_dir).
    let mut dir = String::new();
    for line in out.lines() {
        if line.ends_with('/') && !line.contains(' ') {
            dir = line.to_string();
            continue;
        }
        if !dir.is_empty()
            && let Some(rest) = line.strip_prefix(' ')
            && !rest.starts_with(' ')
            && let Some((file, _)) = rest.split_once(" (")
        {
            let p = format!("{dir}{file}");
            if seen.insert(p.clone()) {
                v.push(p);
            }
            continue;
        }
        for m in re.captures_iter(line) {
            let p = m[1].to_string();
            let ext = p.rsplit_once('.').map(|x| x.1.to_ascii_lowercase()).unwrap_or_default();
            let known = crate::lang::for_path(&p).is_some() || TEXT_EXTS.contains(&ext.as_str());
            if known && seen.insert(p.clone()) {
                v.push(p);
            }
        }
    }
    v
}

/// Estimated tokens saved by `out` versus reading the cited files whole.
pub fn estimate(c: &Connection, pid: Option<i64>, out: &str) -> i64 {
    let mut naive: i64 = 0;
    // (project_id, path) is indexed; without a project there is nothing cheap to compare against.
    let Some(pid) = pid else { return 0 };
    let mut q = match c.prepare_cached("SELECT size FROM node WHERE project_id = ?2 AND path = ?1 AND kind = 'file' LIMIT 1") {
        Ok(q) => q,
        Err(_) => return 0,
    };
    for p in cited_paths(out) {
        if let Ok(Some(sz)) = q.query_row(params![p, pid], |r| r.get::<_, Option<i64>>(0)) {
            naive += tokens(sz.max(0) as usize);
        }
    }
    (naive - tokens(out.len())).max(0)
}

fn day(ts: i64) -> i64 {
    ts / 86_400
}

/// Add one answer to the ledger.
pub fn record(w: &Connection, tool: &str, saved: i64) -> Result<()> {
    w.execute(
        "INSERT INTO saving(day, tool, answers, saved) VALUES (?1, ?2, 1, ?3)
         ON CONFLICT(day, tool) DO UPDATE SET answers = answers + 1, saved = saved + excluded.saved",
        params![day(crate::db::now()), tool, saved.max(0)],
    )?;
    Ok(())
}

/// Estimate and record in one go, fail-open. Returns `out` unchanged.
pub fn account(pid: Option<i64>, tool: &str, out: String) -> String {
    let Ok(w) = crate::db::open_rw() else { return out };
    account_with(&w, pid, tool, out)
}

/// `account` on an already open write connection (the daemon keeps one).
pub fn account_with(w: &Connection, pid: Option<i64>, tool: &str, out: String) -> String {
    let _ = record(w, tool, estimate(w, pid, &out));
    out
}

/// Round to two significant digits.
pub fn coarse(n: i64) -> i64 {
    let mut m = 1;
    while n / m >= 100 {
        m *= 10;
    }
    n / m * m
}

pub fn human(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{}k", n / 1000),
        n => n.to_string(),
    }
}

fn sum_since(c: &Connection, from_day: i64) -> (i64, i64) {
    c.query_row("SELECT COALESCE(SUM(saved),0), COALESCE(SUM(answers),0) FROM saving WHERE day >= ?1", [from_day], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .unwrap_or((0, 0))
}

/// `fox stats`.
pub fn stats(c: &Connection) -> Result<String> {
    let today = day(crate::db::now());
    let mut s = String::from("tokens saved (est., vs reading cited files whole)\n");
    for (label, from) in [("today", today), ("7 days", today - 6), ("all time", 0)] {
        let (saved, n) = sum_since(c, from);
        s.push_str(&format!("{label}: ~{} in {n} answers\n", human(saved)));
    }
    let mut q = c.prepare("SELECT tool, SUM(saved), SUM(answers) FROM saving GROUP BY tool ORDER BY 2 DESC LIMIT 5")?;
    let top: Vec<String> =
        q.query_map([], |r| Ok(format!("{} ~{}/{}", r.get::<_, String>(0)?, human(r.get(1)?), r.get::<_, i64>(2)?)))?.flatten().collect();
    if !top.is_empty() {
        s.push_str(&format!("top: {}\n", top.join(", ")));
    }
    Ok(s)
}

/// One line for the session brief, if anything was saved this week.
pub fn brief_line(c: &Connection) -> Option<String> {
    c.query_row("SELECT 1 FROM sqlite_master WHERE name = 'saving'", [], |_| Ok(())).optional().ok()??;
    let (saved, n) = sum_since(c, day(crate::db::now()) - 6);
    // Coarse (two significant digits, no answer count) so it rarely changes the cached brief.
    let _ = n;
    (saved > 0).then(|| format!("fox saved ~{} tokens this week", human(coarse(saved))))
}
