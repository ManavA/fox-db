//! Optional user config: `~/.foxdb/config.toml` (or `$FOXDB_HOME/config.toml`).
//! See `config.example.toml` in the repo root. Missing or invalid files mean defaults.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Build module/target name -> short platform tag shown after symbol hits, e.g. `MyAppTV = "tvOS"`.
    #[serde(default)]
    pub module_tags: HashMap<String, String>,
    /// Use the external codanna binary instead of the native tree-sitter index (default off).
    #[serde(default)]
    pub codanna: bool,
    #[serde(default)]
    pub hooks: Hooks,
    #[serde(default)]
    pub search: Search,
    /// The vector arm (builds with the `vectors` feature, after `fox vectors setup`). Off unless set.
    #[serde(default)]
    pub vectors: bool,
}

/// Is the vector arm switched on? `FOXDB_VECTORS` (`1` or `0`) overrides the file. It also needs
/// a build with the `vectors` feature and the model in place.
pub fn vectors_on() -> bool {
    match std::env::var("FOXDB_VECTORS").ok().as_deref() {
        Some("1" | "true") => true,
        Some("0" | "false") => false,
        _ => get().vectors,
    }
}

/// `[search]` in config.toml: which full-text engine serves a project. `fts5` (the default) never
/// uses tantivy; `auto` keeps SQLite FTS5 for every project below `tantivy_min_docs` rows and
/// gives a bigger one a tantivy index (in builds with the `tantivy` feature); `tantivy` gives
/// every indexed project one. `[search.projects]` overrides it per project name. tantivy is
/// opt-in because its recall trails FTS5 on the kubernetes eval sets (docs/benchmarks.md).
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Search {
    pub backend: String,
    pub tantivy_min_docs: i64,
    pub projects: HashMap<String, String>,
    /// Indexing memory of the tantivy writer inside `foxd`, in MB (one thread).
    pub writer_mb: usize,
}

impl Default for Search {
    fn default() -> Search {
        Search { backend: "fts5".into(), tantivy_min_docs: 1_000_000, projects: HashMap::new(), writer_mb: 64 }
    }
}

impl Search {
    /// The backend setting for project `name`: `auto`, `fts5` or `tantivy`. `FOXDB_SEARCH_BACKEND`
    /// overrides the file's global value (not a per-project one).
    pub fn backend_for(&self, name: &str) -> String {
        self.projects.get(name).cloned().unwrap_or_else(|| self.global())
    }

    /// The global backend setting (`FOXDB_SEARCH_BACKEND`, else the file's).
    pub fn global(&self) -> String {
        std::env::var("FOXDB_SEARCH_BACKEND").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| self.backend.clone())
    }

    /// Could any project use tantivy under these settings?
    pub fn tantivy_possible(&self) -> bool {
        self.global() != "fts5" || self.projects.values().any(|b| b != "fts5")
    }
}

/// `[hooks]` in config.toml.
#[derive(Debug, Default, Deserialize)]
pub struct Hooks {
    /// Agent name/type globs that get no SubagentStart brief (case-insensitive). Replaces the
    /// default `["egm-*", "*goldfish*", "*reviewer*"]`: zero-context reviewers stay zero-context.
    #[serde(default)]
    pub skip_agents: Option<Vec<String>>,
    /// PostToolUse on Bash: name the enclosing symbol and handle of each `grep`/`rg`/`git grep`
    /// hit (`fox hook post-tool-use`). Off unless set.
    #[serde(default)]
    pub grep_context: bool,
    /// UserPromptSubmit: when the prompt names an identifier or path that resolves to exactly one
    /// indexed definition or file, add its location (`fox hook user-prompt-submit`). Off unless set.
    #[serde(default)]
    pub prompt_context: bool,
}

pub const DEFAULT_SKIP_AGENTS: &[&str] = &["egm-*", "*goldfish*", "*reviewer*"];

pub fn parse(s: &str) -> Config {
    toml::from_str(s).unwrap_or_default()
}

pub fn get() -> &'static Config {
    static C: OnceLock<Config> = OnceLock::new();
    C.get_or_init(|| std::fs::read_to_string(crate::db::home().join("config.toml")).map(|s| parse(&s)).unwrap_or_default())
}

pub fn module_tag(module: &str) -> Option<&'static str> {
    get().module_tags.get(module).map(String::as_str)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_module_tags() {
        let c = super::parse("[module_tags]\nMyAppTV = \"tvOS\"\n");
        assert_eq!(c.module_tags.get("MyAppTV").map(String::as_str), Some("tvOS"));
        assert!(super::parse("garbage =").module_tags.is_empty());
        assert!(!super::parse("").vectors);
        assert!(super::parse("vectors = true\n").vectors);
    }

    #[test]
    fn search_backend_defaults_to_fts5_with_opt_in_overrides() {
        let c = super::parse("");
        // tantivy is opt-in: its recall trails FTS5 on the kubernetes eval sets (docs/benchmarks.md).
        assert_eq!((c.search.backend.as_str(), c.search.tantivy_min_docs), ("fts5", 1_000_000));
        assert!(!c.search.tantivy_possible() || std::env::var_os("FOXDB_SEARCH_BACKEND").is_some());
        let c = super::parse("[search]\nbackend = \"auto\"\n");
        assert_eq!(c.search.backend, "auto");
        let c = super::parse("[search]\nbackend = \"fts5\"\n[search.projects]\nlinux = \"tantivy\"\n");
        assert_eq!(c.search.backend_for("linux"), "tantivy");
        assert!(c.search.tantivy_possible());
    }
}
