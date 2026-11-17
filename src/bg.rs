//! Detached background indexing, shared by the CLI hooks and the MCP server.
use crate::{db, project};
use std::path::PathBuf;

pub fn lock_path(pid: i64) -> PathBuf {
    db::home().join("locks").join(format!("index-{pid}.lock"))
}

/// Spawn a detached `fox index` for the project unless one is already running (fresh lock).
pub fn spawn_background_index(p: &project::Project) {
    let lp = lock_path(p.id);
    let _ = std::fs::create_dir_all(lp.parent().unwrap());
    if let Ok(md) = std::fs::metadata(&lp)
        && md.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|e| e.as_secs() < 1800)
    {
        return;
    }
    if std::fs::write(&lp, std::process::id().to_string()).is_err() {
        return;
    }
    let Ok(mut exe) = std::env::current_exe() else { return };
    // foxd and fox-bench live next to fox; indexing is fox's job.
    if exe.file_name().is_some_and(|n| n == "foxd" || n == "fox-bench") {
        exe.set_file_name("fox");
    }
    use std::os::unix::process::CommandExt;
    let _ = std::process::Command::new(exe)
        .args(["index", "--quiet"])
        .arg(&p.root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
}
