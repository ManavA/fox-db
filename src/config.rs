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
}

/// `[hooks]` in config.toml.
#[derive(Debug, Default, Deserialize)]
pub struct Hooks {
    /// Agent name/type globs that get no SubagentStart brief (case-insensitive). Replaces the
    /// default `["egm-*", "*goldfish*", "*reviewer*"]`: zero-context reviewers stay zero-context.
    #[serde(default)]
    pub skip_agents: Option<Vec<String>>,
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
    }
}
