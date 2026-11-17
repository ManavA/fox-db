//! Repository hygiene: no machine-specific home paths in the tracked tree. Examples use
//! `~/…`, `/path/to/home/…` or a temp dir.

use std::process::Command;

#[test]
fn tracked_tree_has_no_machine_specific_home_paths() {
    let root = env!("CARGO_MANIFEST_DIR");
    let Ok(o) = Command::new("git").args(["-C", root, "ls-files", "-z"]).output() else { return };
    if !o.status.success() {
        return; // not a git checkout (a packaged crate): nothing to check
    }
    // A home path starts a path: `/home/x` or `/Users/x` not preceded by a path character
    // (so `/path/to/home/` is fine).
    let re = regex::Regex::new(r"(^|[^A-Za-z0-9_.~/-])/(home|Users)/[A-Za-z0-9_.-]+").unwrap();
    let mut hits = vec![];
    for f in String::from_utf8_lossy(&o.stdout).split('\0').filter(|f| !f.is_empty()) {
        if f == "tests/hygiene.rs" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(std::path::Path::new(root).join(f)) else { continue };
        for (i, l) in text.lines().enumerate() {
            if let Some(m) = re.find(l) {
                hits.push(format!("{f}:{}: {}", i + 1, m.as_str().trim()));
            }
        }
    }
    assert!(hits.is_empty(), "machine-specific paths in tracked files:\n{}", hits.join("\n"));
}

/// FNV-1a 64 of a lowercased word: the deny list below holds hashes, so the tracked tree never
/// spells the names it guards against.
fn fnv(word: &str) -> u64 {
    word.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b.to_ascii_lowercase() as u64).wrapping_mul(0x100000001b3))
}

/// Real project names the examples must not use (hashes of the lowercased word).
const DENIED: &[u64] = &[0xb4383676a7ae0e6a, 0x15a973b0af78410d];

#[test]
fn tracked_tree_has_no_real_project_names() {
    let root = env!("CARGO_MANIFEST_DIR");
    let Ok(o) = Command::new("git").args(["-C", root, "ls-files", "-z"]).output() else { return };
    if !o.status.success() {
        return;
    }
    let mut hits = vec![];
    for f in String::from_utf8_lossy(&o.stdout).split('\0').filter(|f| !f.is_empty()) {
        let text = std::fs::read_to_string(std::path::Path::new(root).join(f)).unwrap_or_default();
        for (i, l) in [f.to_string()].iter().chain(text.lines().map(String::from).collect::<Vec<_>>().iter()).enumerate() {
            if l.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| DENIED.contains(&fnv(w))) {
                hits.push(if i == 0 { format!("{f}: file name") } else { format!("{f}:{i}") });
            }
        }
    }
    assert!(hits.is_empty(), "real project names in tracked files:\n{}", hits.join("\n"));
}
