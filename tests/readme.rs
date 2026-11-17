//! The wake hook in docs/multi-agent.md, run as Claude Code would run it, against a stub `fox`.

use std::io::Write;
use std::process::{Command, Stdio};

const README: &str = include_str!("../docs/multi-agent.md");

/// The PostToolUse command from the README's `asyncRewake` block.
fn hook_command() -> String {
    let start = README.find("\"asyncRewake\"").expect("docs/multi-agent.md has the wake hook");
    let open = README[..start].rfind("```json").unwrap() + "```json".len();
    let close = open + README[open..].find("```").unwrap();
    let v: serde_json::Value = serde_json::from_str(&README[open..close]).expect("the hook block is valid JSON");
    v["hooks"]["PostToolUse"][0]["hooks"][0]["command"].as_str().unwrap().to_string()
}

/// Run the hook on a Bash tool result with `stdout`: (exit code, stderr).
fn run_hook(stdout: &str) -> (i32, String) {
    let bin = tempfile::tempdir().unwrap();
    let fox = bin.path().join("fox");
    std::fs::write(&fox, "#!/bin/sh\necho \"waited: $*\"\nexit 4\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fox, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default());
    let input = serde_json::json!({"hook_event_name": "PostToolUse", "tool_name": "Bash",
        "tool_input": {"command": "fox dispatch …"}, "tool_response": {"stdout": stdout, "stderr": ""}});
    let mut child = Command::new("sh")
        .args(["-c", &hook_command()])
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stderr).into_owned())
}

fn have_jq() -> bool {
    Command::new("jq").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// Every worker a Bash call dispatched is waited on, not only the first.
#[test]
fn the_wake_hook_waits_on_every_dispatched_worker() {
    if !have_jq() {
        return; // the hook needs jq; nothing to check without it
    }
    let (code, err) = run_hook("cx-5 dispatched (codex, writer)\ncx-6 dispatched (codex)\n");
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("waited: wait cx-5 cx-6 --timeout"), "{err}");
    // A call that dispatched nothing visible does not wake anyone.
    assert_eq!(run_hook("").0, 0);
}

/// The worked example captures ids with `$(…)`, which hides the dispatch lines from the hook;
/// the example prints them again, and the hook then waits on all of them.
#[test]
fn the_worked_example_is_visible_to_the_wake_hook() {
    if !have_jq() {
        return;
    }
    let line = README.lines().find(|l| l.starts_with("printf '%s dispatched")).expect("worked example prints the ids");
    let script = format!("W=cx-1 R1=cx-2 R2=cx-3; {line}");
    let o = Command::new("sh").args(["-c", &script]).output().unwrap();
    let (code, err) = run_hook(&String::from_utf8_lossy(&o.stdout));
    assert!(code == 2 && err.contains("waited: wait cx-1 cx-2 cx-3 --timeout"), "rc {code}: {err}");
}
