//! Worktree snapshots as commit ids, so a handoff points at an exact change.
//!
//! [`snapshot`] returns HEAD when the worktree is clean, and otherwise a commit of the whole
//! worktree (tracked changes and untracked, non-ignored files) whose parent is HEAD. It is built
//! with a private copy of the index (`GIT_INDEX_FILE`), `write-tree` and `commit-tree`: the user's
//! index, HEAD, refs and files are untouched; only objects are added to the object store. No ref
//! points at a snapshot, so `git gc` may prune it after its expiry (two weeks by default).
//! Dates and identity are fixed, so the same tree on the same HEAD is the same commit id.

use std::path::Path;
use std::process::{Command, Stdio};

fn git(repo: &Path, index: Option<&Path>, args: &[&str]) -> Option<String> {
    let mut c = Command::new("git");
    c.arg("-C").arg(repo).args(args).stdin(Stdio::null()).stderr(Stdio::null());
    if let Some(i) = index {
        c.env("GIT_INDEX_FILE", i);
    }
    for (k, v) in [
        ("GIT_AUTHOR_NAME", "foxdb"),
        ("GIT_AUTHOR_EMAIL", "foxdb@localhost"),
        ("GIT_COMMITTER_NAME", "foxdb"),
        ("GIT_COMMITTER_EMAIL", "foxdb@localhost"),
        ("GIT_AUTHOR_DATE", "1000000000 +0000"),
        ("GIT_COMMITTER_DATE", "1000000000 +0000"),
    ] {
        c.env(k, v);
    }
    let o = c.output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The worktree of `repo` as a commit id; None outside git.
pub fn snapshot(repo: &Path) -> Option<String> {
    git(repo, None, &["rev-parse", "--is-inside-work-tree"]).filter(|s| s == "true")?;
    let head = git(repo, None, &["rev-parse", "--verify", "-q", "HEAD"]).filter(|s| !s.is_empty());
    let dirty = git(repo, None, &["status", "--porcelain", "--untracked-files=all"])?;
    if dirty.is_empty()
        && let Some(h) = head
    {
        return Some(h);
    }
    // A private index, seeded from the real one so unchanged files are not re-hashed.
    let real = git(repo, None, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
    let tmp = std::env::temp_dir().join(format!(
        "foxdb-index-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    if std::fs::copy(&real, &tmp).is_err() {
        // No index yet (a fresh repo): start empty.
        let _ = std::fs::remove_file(&tmp);
    }
    let r = (|| {
        git(repo, Some(&tmp), &["add", "-A"])?;
        let tree = git(repo, Some(&tmp), &["write-tree"])?;
        let mut args = vec!["commit-tree", "--no-gpg-sign", tree.as_str(), "-m", "foxdb snapshot"];
        if let Some(h) = &head {
            args.extend(["-p", h.as_str()]);
        }
        git(repo, Some(&tmp), &args)
    })();
    let _ = std::fs::remove_file(&tmp);
    r.filter(|s| !s.is_empty())
}

/// Paths that differ between two commits.
pub fn changed(repo: &Path, base: &str, head: &str) -> Option<Vec<String>> {
    if base == head {
        return Some(vec![]);
    }
    let out = git(repo, None, &["diff", "--name-only", "--no-renames", base, head])?;
    Some(out.lines().filter(|l| !l.is_empty()).map(String::from).collect())
}
