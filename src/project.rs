//! Project identity from a cwd: keyed by git common dir (so worktrees map to their main repo),
//! or by the directory itself when it is not inside a git repo. No `git` subprocess.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Located {
    /// Directory that gets indexed (main worktree root, or the cwd for non-git dirs).
    pub root: PathBuf,
    /// Canonical git common dir, if any.
    pub common_dir: Option<PathBuf>,
    /// The worktree's own git dir (for HEAD/branch).
    pub git_dir: Option<PathBuf>,
    /// "repo" | "dir" | "huge" (home or filesystem root: name-only, never parsed).
    pub kind: &'static str,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub root: PathBuf,
    pub kind: String,
    pub indexed_at: Option<i64>,
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

pub fn locate(cwd: &Path) -> Located {
    let cwd = canon(cwd);
    let home = std::env::var_os("HOME").map(|h| canon(Path::new(&h)));
    let mut dir: Option<&Path> = Some(&cwd);
    while let Some(d) = dir {
        if home.as_deref() == Some(d) || d.parent().is_none() {
            break; // never treat ~ or / as a repo root, even if ~ is a git repo
        }
        let dotgit = d.join(".git");
        if let Ok(md) = std::fs::symlink_metadata(&dotgit) {
            let git_dir = if md.is_dir() {
                Some(dotgit.clone())
            } else {
                std::fs::read_to_string(&dotgit).ok().and_then(|s| {
                    s.lines().find_map(|l| l.strip_prefix("gitdir:")).map(|g| {
                        let g = Path::new(g.trim());
                        canon(&if g.is_absolute() { g.to_path_buf() } else { d.join(g) })
                    })
                })
            };
            // Like git itself, only accept a git dir that has a HEAD: a stray empty `.git`
            // (e.g. `/tmp/.git` left by some tool) must not swallow every directory below it.
            if let Some(gd) = git_dir.filter(|g| g.join("HEAD").exists()) {
                let common =
                    std::fs::read_to_string(gd.join("commondir")).ok().map(|c| canon(&gd.join(c.trim()))).unwrap_or_else(|| canon(&gd));
                let root = if common.file_name().is_some_and(|n| n == ".git") {
                    common.parent().map(Path::to_path_buf).unwrap_or_else(|| d.to_path_buf())
                } else {
                    d.to_path_buf()
                };
                return Located { root, common_dir: Some(common), git_dir: Some(gd), kind: "repo" };
            }
        }
        dir = d.parent();
    }
    let huge = home.as_deref() == Some(cwd.as_path()) || cwd.parent().is_none();
    Located { root: cwd, common_dir: None, git_dir: None, kind: if huge { "huge" } else { "dir" } }
}

pub fn branch(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(b) => b.to_string(),
        None => format!("detached {}", &head[..head.len().min(10)]),
    })
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Project> {
    Ok(Project { id: r.get(0)?, name: r.get(1)?, root: PathBuf::from(r.get::<_, String>(2)?), kind: r.get(3)?, indexed_at: r.get(4)? })
}
const COLS: &str = "id, name, root, kind, indexed_at";

pub fn find(c: &Connection, loc: &Located) -> Result<Option<Project>> {
    let key = loc.common_dir.as_ref().unwrap_or(&loc.root).to_string_lossy().into_owned();
    Ok(c.query_row(&format!("SELECT {COLS} FROM project WHERE git_common_dir = ?1"), [key], row).optional()?)
}

pub fn by_name(c: &Connection, name: &str) -> Result<Option<Project>> {
    Ok(c.query_row(&format!("SELECT {COLS} FROM project WHERE name = ?1"), [name], row).optional()?)
}

/// Find or register the project for `loc`.
pub fn ensure(c: &Connection, loc: &Located) -> Result<Project> {
    if let Some(p) = find(c, loc)? {
        return Ok(p);
    }
    let key = loc.common_dir.as_ref().unwrap_or(&loc.root).to_string_lossy().into_owned();
    let base = loc.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into());
    let mut name = base.clone();
    let mut i = 2;
    while by_name(c, &name)?.is_some() {
        name = format!("{base}-{i}");
        i += 1;
    }
    c.execute(
        "INSERT INTO project(name, root, git_common_dir, kind, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![name, loc.root.to_string_lossy(), key, loc.kind, crate::db::now()],
    )?;
    Ok(find(c, loc)?.expect("just inserted"))
}

/// The global project (cross-project memory: ~/.claude/projects/*/memory).
pub fn global(c: &Connection) -> Result<Project> {
    if let Some(p) = by_name(c, "~global")? {
        return Ok(p);
    }
    c.execute(
        "INSERT INTO project(name, root, git_common_dir, kind, created_at) VALUES ('~global', ?1, '~global', 'global', ?2)",
        params![crate::db::home().to_string_lossy(), crate::db::now()],
    )?;
    Ok(by_name(c, "~global")?.expect("just inserted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dot_git_is_not_a_repo() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join(".git")).unwrap();
        let sub = t.path().join("proj");
        std::fs::create_dir(&sub).unwrap();
        let l = locate(&sub);
        assert_eq!(l.kind, "dir");
        assert_eq!(l.root, canon(&sub));
        std::fs::write(t.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let l = locate(&sub);
        assert_eq!(l.kind, "repo");
        assert_eq!(l.root, canon(t.path()));
    }
}
