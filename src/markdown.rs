//! Markdown splitting and the milestone-log parser.

use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone)]
pub struct Section {
    pub title: String,
    pub level: usize,
    /// 1-based line of the heading (0 for preamble).
    pub line: usize,
    pub end_line: usize,
    pub body: String,
}

/// Split on ATX headings, ignoring fenced code blocks. `only_level` restricts splitting to one level.
pub fn split(text: &str, only_level: Option<usize>) -> Vec<Section> {
    let mut out = Vec::new();
    let mut cur = Section { title: String::new(), level: 0, line: 0, end_line: 0, body: String::new() };
    let mut fence = false;
    let mut n = 0;
    for (i, l) in text.lines().enumerate() {
        n = i + 1;
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
        }
        let level = if !fence && l.starts_with('#') { l.chars().take_while(|&c| c == '#').count() } else { 0 };
        let is_heading = (1..=6).contains(&level) && l[level..].starts_with(' ') && only_level.is_none_or(|w| w == level);
        if is_heading {
            cur.end_line = i;
            if cur.line > 0 || !cur.body.trim().is_empty() {
                out.push(cur);
            }
            cur = Section {
                title: l[level..].trim().trim_end_matches('#').trim().to_string(),
                level,
                line: i + 1,
                end_line: 0,
                body: String::new(),
            };
        } else {
            cur.body.push_str(l);
            cur.body.push('\n');
        }
    }
    cur.end_line = n;
    if cur.line > 0 || !cur.body.trim().is_empty() {
        out.push(cur);
    }
    out
}

pub const STATES: &[&str] = &[
    "NOT_STARTED",
    "IN_PROGRESS",
    "READY_FOR_REVIEW",
    "WAITING_FOR_REVIEW",
    "CHANGES_REQUESTED",
    "APPROVED",
    "BLOCKED",
    "DONE_WITH_CONCERNS",
    "DONE",
    "SUPERSEDED",
];
/// States meaning "this milestone is still open".
pub const OPEN_STATES: &[&str] = &["IN_PROGRESS", "READY_FOR_REVIEW", "WAITING_FOR_REVIEW", "CHANGES_REQUESTED", "BLOCKED", "NOT_STARTED"];

static STATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(NOT_STARTED|IN_PROGRESS|READY_FOR_REVIEW|WAITING_FOR_REVIEW|CHANGES_REQUESTED|APPROVED|BLOCKED|DONE_WITH_CONCERNS|DONE|SUPERSEDED)\b").unwrap()
});
static STATE_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^\s*(?:[-*]\s*)?\**\s*(?:state|status|milestone state|gate)\s*\**\s*[:=|]").unwrap());
static DATE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(20\d\d-\d\d-\d\d)\b").unwrap());
static SHA_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9a-f]{7,40}\b").unwrap());
static ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    // P2.3, PERF2-P2.4, PXL-31, I10-R, S2V2-21, M6, P2.S5, IOS-P1
    Regex::new(r"\b([A-Z][A-Z0-9]*(?:[-.][A-Z0-9]+)+|M\d+(?:\.\d+)?)\b").unwrap()
});

#[derive(Debug, Clone, Default)]
pub struct Milestone {
    pub id: Option<String>,
    pub state: Option<String>,
    pub date: Option<String>,
    pub shas: Vec<String>,
    pub pinned: bool,
}

fn milestone_id(title: &str) -> Option<String> {
    ID_RE
        .find_iter(title)
        .map(|m| m.as_str())
        .find(|s| {
            s.chars().any(|c| c.is_ascii_digit())
                && !DATE_RE.is_match(s)
                && !STATES.contains(s)
                && !s.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '.' || c == ':')
        })
        .map(str::to_string)
}

pub fn parse_milestone(s: &Section) -> Milestone {
    let pinned = s.title.eq_ignore_ascii_case("current gate");
    let state = STATE_RE
        .find(&s.title)
        .map(|m| m.as_str().to_string())
        .or_else(|| {
            STATE_LINE_RE.find_iter(&s.body).find_map(|m| {
                let line_end = s.body[m.end()..].find('\n').map(|e| m.end() + e).unwrap_or(s.body.len());
                STATE_RE.find(&s.body[m.end()..line_end]).map(|x| x.as_str().to_string())
            })
        })
        .or_else(|| STATE_RE.find(&s.body).map(|m| m.as_str().to_string()));
    let date = DATE_RE.captures(&s.title).or_else(|| DATE_RE.captures(&s.body)).map(|c| c[1].to_string());
    let mut shas: Vec<String> = Vec::new();
    for m in SHA_RE.find_iter(&s.body) {
        let x = m.as_str();
        if x.chars().any(|c| c.is_ascii_digit()) && x.chars().any(|c| c.is_ascii_alphabetic()) && !shas.iter().any(|y| y == x) {
            shas.push(x.to_string());
            if shas.len() >= 20 {
                break;
            }
        }
    }
    Milestone { id: if pinned { None } else { milestone_id(&s.title) }, state, date, shas, pinned }
}

/// Files parsed as milestone logs (split on `## ` only).
pub fn is_milestone_log(name: &str) -> bool {
    let u = name.to_ascii_uppercase();
    u.ends_with(".MD") && (u.contains("MILESTONE_LOG") || u == "IOS_AGENT_LOG.MD" || (u.contains("AGENT") && u.ends_with("_LOG.MD")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn splits_and_parses() {
        let t = "# Log\n\npre\n\n## Current gate\n\n- gate line\n\n## 2026-10-19 — PERF2-P2.4 parallel Home sources — IN_PROGRESS\n\nbase `75bd5b124223c8a1735df2be04f22c2e07b3637d`\n```\n## not a heading\n```\n## 2026-08-23 14:39 PDT | CODEX | M6 STANDALONE\n\nState: READY_FOR_REVIEW\n";
        let s = split(t, Some(2));
        assert_eq!(s.len(), 4, "{s:#?}");
        let gate = parse_milestone(&s[1]);
        assert!(gate.pinned);
        let m = parse_milestone(&s[2]);
        assert_eq!(m.id.as_deref(), Some("PERF2-P2.4"));
        assert_eq!(m.state.as_deref(), Some("IN_PROGRESS"));
        assert_eq!(m.date.as_deref(), Some("2026-10-19"));
        assert_eq!(m.shas, vec!["75bd5b124223c8a1735df2be04f22c2e07b3637d"]);
        let m6 = parse_milestone(&s[3]);
        assert_eq!(m6.id.as_deref(), Some("M6"));
        assert_eq!(m6.state.as_deref(), Some("READY_FOR_REVIEW"));
    }
    #[test]
    fn ids() {
        assert_eq!(milestone_id("S2V2-21 start — 2026-09-26").as_deref(), Some("S2V2-21"));
        assert_eq!(milestone_id("PXL-31 Astra liveness").as_deref(), Some("PXL-31"));
        assert_eq!(milestone_id("2026-09-06 | CODEX | I10-R focused").as_deref(), Some("I10-R"));
        assert!(is_milestone_log("AGENT_MILESTONE_LOG.md"));
        assert!(is_milestone_log("IOS_AGENT_LOG.md"));
        assert!(!is_milestone_log("AGENTS.md"));
    }
}
