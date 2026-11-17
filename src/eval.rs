//! `fox eval retrieval`: search quality over a checked-in query set (`bench/eval/*.json`).
//!
//! Each query has a type (vague, keyword, exact, ...) and one or more targets. A target names a
//! path and optionally a symbol, or matches by containment (`path_contains`, `name_contains`).
//! A query hits at rank r when its first matching result is the r-th. Reported per type:
//! recall@5 (hit@5: almost every query has one target), hit@10 and MRR over the top k.
//! `--gate BASELINE` fails when any type's recall@5 or MRR fell below the baseline's.

use crate::project::Project;
use crate::query::{FindOpts, Hit};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Set {
    pub name: String,
    pub repo: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub about: Option<String>,
    pub queries: Vec<Query>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Query {
    pub id: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub q: String,
    pub targets: Vec<Target>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct Target {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_contains: Option<String>,
}

impl Target {
    /// Does result `h` satisfy every field this target sets? A symbol name matches bare or
    /// qualified by its parent (`Quantity.Mul`).
    pub fn matches(&self, h: &Hit) -> bool {
        let path = h.path.as_deref().unwrap_or("");
        let parent = h
            .meta
            .as_deref()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .and_then(|v| v.get("parent").and_then(|p| p.as_str()).map(|p| format!("{p}.{}", h.name)));
        self.path.as_deref().is_none_or(|p| p == path)
            && self.path_contains.as_deref().is_none_or(|p| path.contains(p))
            && self.name.as_deref().is_none_or(|n| n == h.name || parent.as_deref() == Some(n))
            && self.name_contains.as_deref().is_none_or(|n| h.name.to_lowercase().contains(&n.to_lowercase()))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Score {
    pub n: usize,
    /// Share of queries with a target in the top 5 (recall@5 for single-target queries).
    pub recall5: f64,
    pub hit10: f64,
    pub mrr: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Report {
    pub set: String,
    #[serde(default)]
    pub commit: Option<String>,
    pub k: usize,
    /// Per query type, plus `all`.
    pub types: BTreeMap<String, Score>,
    /// Per query: its id and the rank of its first target (None: not in the top k).
    pub queries: Vec<(String, Option<usize>)>,
}

pub fn load(path: &std::path::Path) -> Result<Set> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not a query set", path.display()))
}

fn round(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Score ranks (None: missed) of queries with types `tys`.
pub fn score(ranks: &[(String, Option<usize>)]) -> BTreeMap<String, Score> {
    let mut acc: BTreeMap<String, (usize, usize, usize, f64)> = BTreeMap::new();
    for (ty, r) in ranks {
        for key in [ty.as_str(), "all"] {
            let a = acc.entry(key.to_string()).or_default();
            a.0 += 1;
            if let Some(r) = r {
                a.1 += (*r <= 5) as usize;
                a.2 += (*r <= 10) as usize;
                a.3 += 1.0 / *r as f64;
            }
        }
    }
    acc.into_iter()
        .map(|(t, (n, h5, h10, rr))| {
            let d = n.max(1) as f64;
            (t, Score { n, recall5: round(h5 as f64 / d), hit10: round(h10 as f64 / d), mrr: round(rr / d) })
        })
        .collect()
}

/// Run every query of `set` against project `p` (cold search, as the CLI runs it), reading the
/// top `k` results.
pub fn run(c: &Connection, p: &Project, set: &Set, k: usize) -> Result<Report> {
    let o = FindOpts { kind: "any", all: false, budget: usize::MAX, limit: k, offset: 0 };
    let mut typed = vec![];
    let mut queries = vec![];
    for q in &set.queries {
        let hits = crate::search::hits(c, Some(p), &q.q, &o, k)?;
        let rank = hits.iter().take(k).position(|h| q.targets.iter().any(|t| t.matches(h))).map(|i| i + 1);
        typed.push((q.ty.clone(), rank));
        queries.push((q.id.clone(), rank));
    }
    Ok(Report { set: set.name.clone(), commit: set.commit.clone(), k, types: score(&typed), queries })
}

/// Types whose recall@5 or MRR fell below `base`'s. Empty: the gate passes.
pub fn regressions(now: &Report, base: &Report) -> Vec<String> {
    let mut out = vec![];
    for (ty, b) in &base.types {
        let Some(n) = now.types.get(ty) else {
            out.push(format!("{ty}: missing (baseline has {} queries)", b.n));
            continue;
        };
        if n.recall5 + 1e-9 < b.recall5 {
            out.push(format!("{ty}: recall@5 {:.3} < baseline {:.3}", n.recall5, b.recall5));
        }
        if n.mrr + 1e-9 < b.mrr {
            out.push(format!("{ty}: MRR {:.3} < baseline {:.3}", n.mrr, b.mrr));
        }
    }
    out
}

/// The text table: one row per type.
pub fn render(r: &Report) -> String {
    let mut out = format!("{} (top {}):\n", r.set, r.k);
    out += "type        n  recall@5  hit@10   MRR\n";
    for (t, s) in &r.types {
        out += &format!("{t:<9} {:>3}  {:>8.2}  {:>6.2}  {:>4.2}\n", s.n, s.recall5, s.hit10, s.mrr);
    }
    let missed: Vec<&str> = r.queries.iter().filter(|q| q.1.is_none()).map(|q| q.0.as_str()).collect();
    if !missed.is_empty() {
        out += &format!("missed: {}\n", missed.join(" "));
    }
    out
}
