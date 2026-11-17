//! Worker reports: one fixed format, enforced by the harness and checked again on arrival.
//!
//! A dispatched worker's final answer must match [`SCHEMA`]: Codex gets it as
//! `--output-schema <file>`, Claude as `--json-schema <schema>`. The supervisor then parses the
//! answer with [`parse`], which accepts exactly that shape. A malformed answer fails the worker
//! with the reason; it is never guessed at. [`checks`] adds cheap rule findings (no model calls)
//! and [`render`] turns the report into the plain text stored on the board.

use std::fmt::Write;

/// The report schema. Strict-mode compatible (every property required, no extra properties),
/// which Codex's structured output needs. It sits in every worker's request, so it is part of
/// the context budget (`context-budget.json`).
pub const SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["status","summary","files","tests","decisions","next"],"properties":{"status":{"type":"string","enum":["done","partial","blocked"]},"summary":{"type":"string","description":"what changed or was found, and why"},"files":{"type":"array","items":{"type":"string","description":"path or path:line touched"}},"tests":{"type":"object","additionalProperties":false,"required":["ran","passed"],"properties":{"ran":{"type":"array","items":{"type":"string","description":"exact command run"}},"passed":{"type":"boolean"}}},"decisions":{"type":"array","items":{"type":"string"}},"next":{"type":"array","items":{"type":"string"}}}}"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub status: String,
    pub summary: String,
    pub files: Vec<String>,
    pub tests_ran: Vec<String>,
    pub tests_passed: bool,
    pub decisions: Vec<String>,
    pub next: Vec<String>,
}

fn strings(v: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    let a = v.get(key).and_then(|x| x.as_array()).ok_or_else(|| format!("{key} must be an array"))?;
    a.iter().map(|x| x.as_str().map(|s| s.trim().to_string()).ok_or_else(|| format!("{key} must hold strings"))).collect()
}

fn exact_keys(v: &serde_json::Map<String, serde_json::Value>, keys: &[&str], what: &str) -> Result<(), String> {
    for k in keys {
        if !v.contains_key(*k) {
            return Err(format!("{what} lacks {k}"));
        }
    }
    if let Some(k) = v.keys().find(|k| !keys.contains(&k.as_str())) {
        return Err(format!("{what} has unknown field {k}"));
    }
    Ok(())
}

/// Parse a final answer as a report. The whole answer must be the JSON object (surrounding
/// whitespace and one Markdown code fence are tolerated: a harness may add them).
pub fn parse(text: &str) -> Result<Report, String> {
    let t = text.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).and_then(|x| x.trim().strip_suffix("```")).unwrap_or(t).trim();
    let v: serde_json::Value = serde_json::from_str(t).map_err(|_| "not a JSON object".to_string())?;
    let o = v.as_object().ok_or("not a JSON object")?;
    exact_keys(o, &["status", "summary", "files", "tests", "decisions", "next"], "report")?;
    let status = o["status"].as_str().unwrap_or_default();
    if !matches!(status, "done" | "partial" | "blocked") {
        return Err(format!("status must be done, partial or blocked, not {}", o["status"]));
    }
    let summary = o["summary"].as_str().ok_or("summary must be a string")?.trim().to_string();
    if summary.is_empty() {
        return Err("summary is empty".into());
    }
    let tests = o["tests"].as_object().ok_or("tests must be an object")?;
    exact_keys(tests, &["ran", "passed"], "tests")?;
    Ok(Report {
        status: status.to_string(),
        summary,
        files: strings(&v, "files")?,
        tests_ran: strings(&o["tests"], "ran")?,
        tests_passed: tests["passed"].as_bool().ok_or("tests.passed must be a boolean")?,
        decisions: strings(&v, "decisions")?,
        next: strings(&v, "next")?,
    })
}

/// What the checks look at besides the report.
#[derive(Default)]
pub struct Ctx<'a> {
    pub writer: bool,
    /// Paths changed between the worker's base and head commits; None outside git.
    pub changed: Option<&'a [String]>,
    /// The worker's raw event log (`out.jsonl`) when it records every command it ran (Codex
    /// `--json` does; Claude's `--output-format json` holds the final result only).
    pub events: Option<&'a str>,
}

/// The leading words of a command, enough to recognise it in an event log.
fn lead(cmd: &str) -> String {
    cmd.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

/// Rule findings on a report: each is a short line, never a model call.
pub fn checks(r: &Report, cx: &Ctx) -> Vec<String> {
    let mut out = vec![];
    if cx.writer && cx.changed.is_some_and(|c| c.is_empty()) {
        out.push("writer changed nothing (base = head)".to_string());
    }
    if r.tests_passed && r.tests_ran.is_empty() {
        out.push("claims tests pass but ran none".to_string());
    }
    if let Some(events) = cx.events {
        for cmd in r.tests_ran.iter().filter(|c| !c.trim().is_empty()) {
            if !events.contains(&lead(cmd)) {
                out.push(format!("claims it ran `{}` but the event log shows no such command", crate::query::clip(cmd, 80)));
            }
        }
    }
    if cx.writer
        && let Some(changed) = cx.changed
        && !changed.is_empty()
    {
        for f in &r.files {
            let path = f.split(':').next().unwrap_or(f).trim_start_matches("./");
            if !path.is_empty() && !changed.iter().any(|c| c == path || c.ends_with(&format!("/{path}"))) {
                out.push(format!("claims {path} but the diff does not touch it"));
            }
        }
    }
    out
}

/// Plain text for the board: `<status>: <summary>`, then the fixed fields, the exact change and
/// any check findings. The first line is the handoff's title.
pub fn render(r: &Report, change: Option<(&str, &str)>, checks: &[String]) -> String {
    let mut s = format!("{}: {}\n", r.status, r.summary);
    if !r.files.is_empty() {
        let _ = writeln!(s, "files: {}", r.files.join(", "));
    }
    let ran: Vec<String> = r.tests_ran.iter().map(|c| format!("`{c}`")).collect();
    match (ran.is_empty(), r.tests_passed) {
        (true, _) => s.push_str("tests: none run\n"),
        (false, p) => {
            let _ = writeln!(s, "tests: {} {}", ran.join(", "), if p { "passed" } else { "failed" });
        }
    }
    for d in &r.decisions {
        let _ = writeln!(s, "decision: {d}");
    }
    for n in &r.next {
        let _ = writeln!(s, "next: {n}");
    }
    if let Some((b, h)) = change {
        let _ = writeln!(s, "change: git diff {b} {h}");
    }
    for c in checks {
        let _ = writeln!(s, "check: {c}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"{"status":"done","summary":"Added gamma","files":["src/gamma.rs:1"],"tests":{"ran":["cargo test"],"passed":true},"decisions":["kept the name"],"next":[]}"#;

    #[test]
    fn schema_is_valid_json_and_strict() {
        let v: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        let props = v["properties"].as_object().unwrap();
        let req: Vec<&str> = v["required"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
        assert_eq!(props.len(), req.len());
        assert_eq!(v["additionalProperties"], false);
        assert_eq!(v["properties"]["tests"]["additionalProperties"], false);
    }

    #[test]
    fn parses_the_fixed_format_and_rejects_anything_else() {
        let r = parse(OK).unwrap();
        assert_eq!(r.status, "done");
        assert_eq!(r.tests_ran, vec!["cargo test"]);
        assert_eq!(parse(&format!("```json\n{OK}\n```")).unwrap(), r);
        for bad in [
            "All done, tests pass.",
            r#"{"status":"done","summary":"x"}"#,
            &OK.replace("\"done\"", "\"finished\""),
            &OK.replace("\"next\":[]", "\"next\":[],\"extra\":1"),
            &OK.replace("\"passed\":true", "\"passed\":\"yes\""),
            &OK.replace("Added gamma", " "),
            &format!("Here is my report: {OK}"),
        ] {
            assert!(parse(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn checks_catch_empty_writers_and_unrun_tests() {
        let mut r = parse(OK).unwrap();
        let none: Vec<String> = vec![];
        let c = checks(&r, &Ctx { writer: true, changed: Some(&none), events: None });
        assert_eq!(c, vec!["writer changed nothing (base = head)"]);
        let events = r#"{"type":"item.completed","item":{"type":"command_execution","command":"ls"}}"#;
        let changed = vec!["src/gamma.rs".to_string()];
        let c = checks(&r, &Ctx { writer: true, changed: Some(&changed), events: Some(events) });
        assert_eq!(c.len(), 1, "{c:?}");
        assert!(c[0].contains("cargo test"));
        r.tests_ran.clear();
        r.files.push("src/other.rs".into());
        let c = checks(&r, &Ctx { writer: true, changed: Some(&changed), events: None });
        assert_eq!(c, vec!["claims tests pass but ran none", "claims src/other.rs but the diff does not touch it"]);
    }

    #[test]
    fn render_puts_status_and_summary_first() {
        let r = parse(OK).unwrap();
        let s = render(&r, Some(("a", "b")), &["x".into()]);
        assert!(s.starts_with("done: Added gamma\n"));
        assert!(s.contains("tests: `cargo test` passed\n") && s.contains("change: git diff a b\n") && s.ends_with("check: x\n"));
    }
}
