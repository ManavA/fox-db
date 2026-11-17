//! Tiered ignore rules. Resolution order: built-in deny list -> per-project override
//! (`~/.foxdb/projects/<name>.toml`) -> `.gitignore` (applied by the walker) -> defaults by type.
//!
//! The deny list is absolute: it applies to git-tracked files too and is re-checked at the
//! single file-open choke point (`guard::read_text`).

use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Never read, never recorded beyond (at most) the directory name.
    Skip = 0,
    /// Path, size, mtime and counts only. Contents never opened.
    NameOnly = 1,
    /// Full-text indexed.
    Text = 2,
    /// Code (symbols arrive in M2; M1 records the file).
    Code = 3,
}

/// Directory names that are never descended into, anywhere in the tree.
const SKIP_DIR_NAMES: &[&str] = &[
    ".git",
    ".build",
    ".Archives",
    ".Exports",
    ".DeviceBuilds",
    "DerivedData",
    "node_modules",
    ".gradle",
    "target",
    "Pods",
    ".swiftpm",
    ".venv",
    "venv",
    "__pycache__",
    ".codanna",
    ".fastembed_cache",
    ".codebase-memory",
    ".graphicle",
    ".foxdb",
];
/// Directory names recorded as one name-only summary node, never descended for content.
const NAME_ONLY_DIR_NAMES: &[&str] = &[".agent-review", ".playwright-cli", ".remember-archive"];
/// Relative directory paths (from project root) recorded name-only.
const NAME_ONLY_DIR_PATHS: &[&str] = &["docs/design", "docs/captures", "SP"];
/// Relative directory paths skipped entirely.
const SKIP_DIR_PATHS: &[&str] = &[".claude/worktrees", "exports", ".server-work"];

const CODE_EXT: &[&str] = &[
    "swift", "kt", "kts", "java", "py", "rs", "c", "h", "m", "mm", "cpp", "cc", "hpp", "go", "js", "jsx", "ts", "tsx", "rb", "php", "cs",
    "scala", "lua", "zig", "dart", "sql", "proto", "pyi", "mjs", "cjs", "mts", "cts", "cxx", "hh", "hxx", "sc",
];
const TEXT_EXT: &[&str] = &[
    "md",
    "markdown",
    "txt",
    "rst",
    "adoc",
    "json",
    "yaml",
    "yml",
    "toml",
    "sh",
    "bash",
    "zsh",
    "fish",
    "plist",
    "xml",
    "gradle",
    "properties",
    "cfg",
    "ini",
    "csv",
    "tsv",
    "html",
    "css",
    "xcconfig",
    "entitlements",
    "strings",
    "log",
];

pub const TEXT_MAX_BYTES: u64 = 256 * 1024;
pub const MARKDOWN_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const LOG_EDGE_BYTES: usize = 64 * 1024;

/// Hard secret deny list, by file name. Applies even to tracked files. Never opened.
pub fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const EXT: &[&str] = &[
        ".p8",
        ".p12",
        ".pfx",
        ".cer",
        ".crt",
        ".der",
        ".pem",
        ".key",
        ".mobileprovision",
        ".provisionprofile",
        ".keystore",
        ".jks",
        ".gpg",
        ".asc",
        ".kdbx",
        ".ovpn",
    ];
    if EXT.iter().any(|e| lower.ends_with(e)) {
        return true;
    }
    if lower.starts_with("authkey_")
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ecdsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("id_dsa")
    {
        return true;
    }
    if lower == ".env" || lower.starts_with(".env.") || lower.ends_with(".env") {
        return true;
    }
    matches!(
        lower.as_str(),
        "local.xcconfig"
            | ".netrc"
            | ".npmrc"
            | ".pypirc"
            | "credentials"
            | "credentials.json"
            | "secrets.json"
            | "secrets.yaml"
            | "secrets.yml"
            | "service-account.json"
            | "google-services.json"
            | "googleservice-info.plist"
            | ".htpasswd"
            | "keychain.db"
            | "token.json"
    )
}

fn name_of(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// Per-project override globs from `~/.foxdb/projects/<name>.toml`:
/// `skip = ["glob", ...]`, `name_only = ["glob", ...]`.
#[derive(Default)]
pub struct Overrides {
    skip: Option<GlobSet>,
    name_only: Option<GlobSet>,
}

#[derive(serde::Deserialize, Default)]
struct OverrideFile {
    #[serde(default)]
    skip: Vec<String>,
    #[serde(default)]
    name_only: Vec<String>,
}

impl Overrides {
    pub fn load(path: &Path) -> Overrides {
        let Ok(text) = std::fs::read_to_string(path) else { return Overrides::default() };
        let Ok(f) = toml::from_str::<OverrideFile>(&text) else { return Overrides::default() };
        Overrides { skip: build(&f.skip), name_only: build(&f.name_only) }
    }
    fn hit(set: &Option<GlobSet>, rel: &str) -> bool {
        set.as_ref().is_some_and(|s| s.is_match(rel))
    }
}

fn build(globs: &[String]) -> Option<GlobSet> {
    if globs.is_empty() {
        return None;
    }
    let mut b = GlobSetBuilder::new();
    for g in globs {
        if let Ok(g) = Glob::new(g) {
            b.add(g);
        }
    }
    b.build().ok()
}

/// Built-in decision for a directory (relative path, `/`-separated, no leading `./`).
pub fn classify_dir(rel: &str, ov: &Overrides) -> Tier {
    let name = name_of(rel);
    if SKIP_DIR_NAMES.contains(&name) || name.starts_with(".DerivedData") || name.ends_with(".xcarchive") || SKIP_DIR_PATHS.contains(&rel) {
        return Tier::Skip;
    }
    if NAME_ONLY_DIR_NAMES.contains(&name)
        || NAME_ONLY_DIR_PATHS.contains(&rel)
        || name.ends_with("-Captures")
        || name.ends_with(".xcassets")
        || name.ends_with(".imageset")
    {
        return Tier::NameOnly;
    }
    if Overrides::hit(&ov.skip, rel) {
        return Tier::Skip;
    }
    if Overrides::hit(&ov.name_only, rel) {
        return Tier::NameOnly;
    }
    Tier::Code // "descend"
}

/// Built-in decision for a file.
pub fn classify_file(rel: &str, size: u64, ov: &Overrides) -> Tier {
    let name = name_of(rel);
    if is_secret_name(name) || Overrides::hit(&ov.skip, rel) {
        return Tier::Skip;
    }
    if Overrides::hit(&ov.name_only, rel) {
        return Tier::NameOnly;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    if CODE_EXT.contains(&ext.as_str()) {
        return if size <= 2 * 1024 * 1024 { Tier::Code } else { Tier::NameOnly };
    }
    if TEXT_EXT.contains(&ext.as_str()) {
        let cap = if ext == "md" || ext == "markdown" {
            MARKDOWN_MAX_BYTES
        } else if ext == "log" {
            u64::MAX
        } else {
            TEXT_MAX_BYTES
        };
        return if size <= cap { Tier::Text } else { Tier::NameOnly };
    }
    Tier::NameOnly
}

/// True if any component of `rel` is denied (skip or name-only dir) or the file is a secret.
/// Used as the last line of defence at the file-open choke point.
pub fn is_denied_path(rel: &str) -> bool {
    let ov = Overrides::default();
    let mut acc = String::new();
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    for (i, p) in parts.iter().enumerate() {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(p);
        if i + 1 == parts.len() {
            return is_secret_name(p);
        }
        if classify_dir(&acc, &ov) != Tier::Code {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets() {
        for n in ["AuthKey_ABCDE12345.p8", "certificate.p12", "a.mobileprovision", ".env", ".env.local", "Local.xcconfig", "id_ed25519"] {
            assert!(is_secret_name(n), "{n}");
        }
        for n in ["README.md", "env.swift", "Key.swift", "Base.xcconfig"] {
            assert!(!is_secret_name(n), "{n}");
        }
    }
    #[test]
    fn dirs() {
        let o = Overrides::default();
        assert_eq!(classify_dir(".agent-review", &o), Tier::NameOnly);
        assert_eq!(classify_dir(".DerivedDataPhysicalRelease", &o), Tier::Skip);
        assert_eq!(classify_dir(".claude/worktrees", &o), Tier::Skip);
        assert_eq!(classify_dir("App-Physical-Captures", &o), Tier::NameOnly);
        assert_eq!(classify_dir("Sources", &o), Tier::Code);
        assert!(is_denied_path(".agent-review/PXL-31/x.swift"));
        assert!(is_denied_path("Config/Local.xcconfig"));
        assert!(!is_denied_path("Sources/App/Main.swift"));
    }
    #[test]
    fn files() {
        let o = Overrides::default();
        assert_eq!(classify_file("Sources/A.swift", 10, &o), Tier::Code);
        assert_eq!(classify_file("AGENTS.md", 10, &o), Tier::Text);
        assert_eq!(classify_file("logo.png", 10, &o), Tier::NameOnly);
        assert_eq!(classify_file("big.json", 10 << 20, &o), Tier::NameOnly);
        assert_eq!(classify_file("AuthKey_X.p8", 10, &o), Tier::Skip);
    }
}
