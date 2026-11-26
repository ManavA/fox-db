//! The Claude Code plugin in plugin/ and the marketplace in .claude-plugin/: the manifests agree
//! with the crate, the bundled skill is the repository's skill, the hooks are the four
//! cache-safe ones, and the bin/fox and hook wrappers find the installed fox or say how to
//! install it.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn json(rel: &str) -> Value {
    let p = repo().join(rel);
    let text = fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn manifest_matches_the_crate() {
    let m = json("plugin/.claude-plugin/plugin.json");
    assert_eq!(m["name"], "foxdb");
    assert_eq!(m["version"], env!("CARGO_PKG_VERSION"), "bump plugin.json with Cargo.toml");
    assert_eq!(m["license"], "AGPL-3.0-or-later");
}

#[test]
fn marketplace_lists_the_plugin() {
    let m = json(".claude-plugin/marketplace.json");
    assert_eq!(m["name"], "foxdb");
    assert!(m["owner"]["name"].is_string());
    let plugins = m["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["name"], "foxdb");
    assert_eq!(plugins[0]["source"], "./plugin");
    // plugin.json owns the version; a second copy here would only drift.
    assert!(plugins[0].get("version").is_none());
}

#[test]
fn bundled_skill_is_the_repository_skill() {
    let ours = fs::read_to_string(repo().join("skill/SKILL.md")).unwrap();
    let bundled = fs::read_to_string(repo().join("plugin/skills/foxdb/SKILL.md")).unwrap();
    assert!(bundled == ours, "copy skill/SKILL.md to plugin/skills/foxdb/SKILL.md");
}

#[test]
fn mcp_server_runs_fox_mcp_through_the_wrapper() {
    let m = json("plugin/.mcp.json");
    let servers = m["mcpServers"].as_object().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers["foxdb"]["command"], "${CLAUDE_PLUGIN_ROOT}/bin/fox");
    assert_eq!(servers["foxdb"]["args"], serde_json::json!(["mcp"]));
}

#[test]
fn hooks_are_the_four_cache_safe_events() {
    let h = json("plugin/hooks/hooks.json");
    let events = h["hooks"].as_object().unwrap();
    let mut names: Vec<&str> = events.keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(names, ["PreCompact", "SessionStart", "SubagentStart", "SubagentStop"]);
    for (event, arg) in [
        ("SessionStart", "session-start"),
        ("SubagentStart", "subagent-start"),
        ("SubagentStop", "subagent-stop"),
        ("PreCompact", "pre-compact"),
    ] {
        let groups = events[event].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{event}");
        let hooks = groups[0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1, "{event}");
        assert_eq!(hooks[0]["type"], "command");
        assert_eq!(hooks[0]["command"], format!("\"${{CLAUDE_PLUGIN_ROOT}}/scripts/hook.sh\" {arg}"), "{event}");
        assert_eq!(hooks[0]["timeout"], if event == "SubagentStop" { 30 } else { 5 }, "{event}");
    }
}

#[test]
fn wrappers_are_executable() {
    for rel in ["plugin/bin/fox", "plugin/scripts/hook.sh"] {
        let mode = fs::metadata(repo().join(rel)).unwrap().permissions().mode();
        assert!(mode & 0o111 == 0o111, "{rel} is not executable");
    }
}

/// A FOXDB_HOME whose bin/fox is a stub that prints its arguments and exits with `code`.
fn home_with_fox(code: i32) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fox = bin.join("fox");
    fs::write(&fox, format!("#!/bin/sh\necho \"stub fox $*\"\nexit {code}\n")).unwrap();
    fs::set_permissions(&fox, fs::Permissions::from_mode(0o755)).unwrap();
    home
}

fn run(rel: &str, args: &[&str], foxdb_home: &Path) -> Output {
    Command::new(repo().join(rel)).args(args).env("FOXDB_HOME", foxdb_home).env("HOME", foxdb_home).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn bin_fox_runs_the_installed_fox() {
    let home = home_with_fox(3);
    let o = run("plugin/bin/fox", &["find", "a b"], home.path());
    assert_eq!(stdout(&o), "stub fox find a b\n");
    assert_eq!(o.status.code(), Some(3), "the exit status passes through");
}

#[test]
fn bin_fox_falls_back_to_the_default_home() {
    // No FOXDB_HOME: ~/.foxdb/bin/fox.
    let home = tempfile::tempdir().unwrap();
    let foxdb = home_with_fox(0);
    fs::rename(foxdb.path(), home.path().join(".foxdb")).unwrap();
    let o =
        Command::new(repo().join("plugin/bin/fox")).arg("--version").env_remove("FOXDB_HOME").env("HOME", home.path()).output().unwrap();
    assert_eq!(stdout(&o), "stub fox --version\n");
}

#[test]
fn bin_fox_without_an_install_says_how_to_install() {
    let empty = tempfile::tempdir().unwrap();
    let o = run("plugin/bin/fox", &["mcp"], empty.path());
    assert!(!o.status.success());
    assert!(o.stdout.is_empty());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("scripts/install.sh"), "{err}");
}

#[test]
fn hook_runs_fox_hook_for_claude_and_never_fails() {
    let home = home_with_fox(0);
    let o = run("plugin/scripts/hook.sh", &["subagent-start"], home.path());
    assert_eq!(stdout(&o), "stub fox hook subagent-start --harness claude\n");
    assert!(o.status.success());

    let failing = home_with_fox(1);
    let o = run("plugin/scripts/hook.sh", &["session-start"], failing.path());
    assert!(o.status.success(), "a failing fox must not fail the hook");
}

#[test]
fn hook_without_an_install_tells_the_user_once_at_session_start() {
    let empty = tempfile::tempdir().unwrap();
    let o = run("plugin/scripts/hook.sh", &["session-start"], empty.path());
    assert!(o.status.success());
    let v: Value = serde_json::from_str(&stdout(&o)).expect("session-start prints JSON");
    let msg = v["systemMessage"].as_str().unwrap();
    assert!(msg.contains("scripts/install.sh"), "{msg}");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("fox_"));

    for event in ["subagent-start", "subagent-stop", "pre-compact"] {
        let o = run("plugin/scripts/hook.sh", &[event], empty.path());
        assert!(o.status.success(), "{event}");
        assert!(o.stdout.is_empty(), "{event} prints nothing");
    }
}

// ---- Codex: plugin/.codex-plugin/plugin.json overlays the same plugin directory ----

#[test]
fn codex_manifest_matches_the_crate_and_points_at_codex_files() {
    let m = json("plugin/.codex-plugin/plugin.json");
    assert_eq!(m["name"], "foxdb");
    assert_eq!(m["version"], env!("CARGO_PKG_VERSION"), "bump .codex-plugin/plugin.json too");
    assert_eq!(m["skills"], "./skills/");
    assert_eq!(m["mcpServers"], "./codex/mcp.json");
    assert_eq!(m["hooks"], "./codex/hooks.json");
}

#[test]
fn codex_mcp_server_forwards_the_agent_identity() {
    let m = json("plugin/codex/mcp.json");
    let s = &m["mcpServers"]["foxdb"];
    assert_eq!(s["command"], "sh");
    assert_eq!(s["env_vars"], serde_json::json!(["FOXDB_AGENT", "FOXDB_PARENT", "FOXDB_HOME", "FOXDB_SESSION"]));
    assert_eq!(s["tool_timeout_sec"], 60);
}

#[test]
fn codex_mcp_command_runs_the_installed_fox() {
    let m = json("plugin/codex/mcp.json");
    let args: Vec<&str> = m["mcpServers"]["foxdb"]["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
    let home = home_with_fox(0);
    let o = Command::new("sh").args(&args).env("FOXDB_HOME", home.path()).output().unwrap();
    assert_eq!(stdout(&o), "stub fox mcp\n");

    // No FOXDB_HOME: ~/.foxdb.
    let user = tempfile::tempdir().unwrap();
    fs::rename(home.path(), user.path().join(".foxdb")).unwrap();
    let o = Command::new("sh").args(&args).env_remove("FOXDB_HOME").env("HOME", user.path()).output().unwrap();
    assert_eq!(stdout(&o), "stub fox mcp\n");
}

#[test]
fn codex_hooks_are_the_same_four_with_harness_codex() {
    let h = json("plugin/codex/hooks.json");
    let events = h["hooks"].as_object().unwrap();
    assert_eq!(events.len(), 4);
    for (event, arg) in [
        ("SessionStart", "session-start"),
        ("SubagentStart", "subagent-start"),
        ("SubagentStop", "subagent-stop"),
        ("PreCompact", "pre-compact"),
    ] {
        let hooks = events[event][0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1, "{event}");
        assert_eq!(hooks[0]["command"], format!("\"$PLUGIN_ROOT/scripts/hook.sh\" {arg} codex"), "{event}");
        assert_eq!(hooks[0]["timeout"], if event == "SubagentStop" { 30 } else { 5 }, "{event}");
    }
    let home = home_with_fox(0);
    let o = run("plugin/scripts/hook.sh", &["pre-compact", "codex"], home.path());
    assert_eq!(stdout(&o), "stub fox hook pre-compact --harness codex\n");
}

#[test]
fn codex_hook_without_an_install_adds_context_only() {
    let empty = tempfile::tempdir().unwrap();
    let o = run("plugin/scripts/hook.sh", &["session-start", "codex"], empty.path());
    assert!(o.status.success());
    let v: Value = serde_json::from_str(&stdout(&o)).expect("session-start prints JSON");
    assert!(v.get("systemMessage").is_none(), "Codex gets only hookSpecificOutput");
    let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(ctx.contains("scripts/install.sh"), "{ctx}");
}
