//! Language table for the native tree-sitter indexer. Adding a language is one row here plus a
//! `languages/<name>.scm` tag query (tree-sitter `tags.scm` captures: `@definition.<kind>` /
//! `@reference.<kind>` around a `@name`).

use std::sync::OnceLock;
use tree_sitter::{Language, Query};

pub struct Lang {
    pub name: &'static str,
    pub exts: &'static [&'static str],
    /// Languages whose symbols a call in this language may resolve to (interop families).
    pub family: &'static str,
    language: fn() -> Language,
    queries: &'static [&'static str],
}

impl Lang {
    pub fn language(&self) -> Language {
        (self.language)()
    }
    /// Compiled tag query (cached per language). None if the grammar rejects it.
    pub fn query(&self) -> Option<&'static Query> {
        static Q: [OnceLock<Option<Query>>; 32] = [const { OnceLock::new() }; 32];
        let i = LANGS.iter().position(|l| std::ptr::eq(l, self))?;
        Q[i].get_or_init(|| Query::new(&self.language(), &self.queries.concat()).ok()).as_ref()
    }
    pub fn query_error(&self) -> Option<String> {
        Query::new(&self.language(), &self.queries.concat()).err().map(|e| format!("{e}"))
    }
}

macro_rules! q {
    ($f:literal) => {
        include_str!(concat!("../languages/", $f, ".scm"))
    };
}

pub static LANGS: &[Lang] = &[
    Lang {
        name: "kotlin",
        exts: &["kt", "kts"],
        family: "jvm",
        language: || tree_sitter_kotlin_ng::LANGUAGE.into(),
        queries: &[q!("kotlin")],
    },
    Lang { name: "java", exts: &["java"], family: "jvm", language: || tree_sitter_java::LANGUAGE.into(), queries: &[q!("java")] },
    Lang {
        name: "python",
        exts: &["py", "pyi"],
        family: "python",
        language: || tree_sitter_python::LANGUAGE.into(),
        queries: &[q!("python")],
    },
    Lang {
        name: "typescript",
        exts: &["ts", "mts", "cts"],
        family: "js",
        language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        queries: &[q!("ecma"), q!("typescript")],
    },
    Lang {
        name: "tsx",
        exts: &["tsx"],
        family: "js",
        language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        queries: &[q!("ecma"), q!("jsx"), q!("typescript")],
    },
    Lang {
        name: "javascript",
        exts: &["js", "jsx", "mjs", "cjs"],
        family: "js",
        language: || tree_sitter_javascript::LANGUAGE.into(),
        queries: &[q!("ecma"), q!("jsx")],
    },
    Lang { name: "go", exts: &["go"], family: "go", language: || tree_sitter_go::LANGUAGE.into(), queries: &[q!("go")] },
    Lang { name: "rust", exts: &["rs"], family: "rust", language: || tree_sitter_rust::LANGUAGE.into(), queries: &[q!("rust")] },
    Lang { name: "c", exts: &["c", "h"], family: "c", language: || tree_sitter_c::LANGUAGE.into(), queries: &[q!("c")] },
    Lang {
        name: "cpp",
        exts: &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
        family: "c",
        language: || tree_sitter_cpp::LANGUAGE.into(),
        queries: &[q!("cpp")],
    },
    Lang { name: "csharp", exts: &["cs"], family: "dotnet", language: || tree_sitter_c_sharp::LANGUAGE.into(), queries: &[q!("csharp")] },
    Lang { name: "swift", exts: &["swift"], family: "swift", language: || tree_sitter_swift::LANGUAGE.into(), queries: &[q!("swift")] },
    Lang { name: "ruby", exts: &["rb"], family: "ruby", language: || tree_sitter_ruby::LANGUAGE.into(), queries: &[q!("ruby")] },
    Lang { name: "php", exts: &["php"], family: "php", language: || tree_sitter_php::LANGUAGE_PHP.into(), queries: &[q!("php")] },
    Lang { name: "bash", exts: &["sh", "bash"], family: "bash", language: || tree_sitter_bash::LANGUAGE.into(), queries: &[q!("bash")] },
    Lang { name: "scala", exts: &["scala", "sc"], family: "jvm", language: || tree_sitter_scala::LANGUAGE.into(), queries: &[q!("scala")] },
    Lang { name: "dart", exts: &["dart"], family: "dart", language: || tree_sitter_dart::LANGUAGE.into(), queries: &[q!("dart")] },
    Lang { name: "lua", exts: &["lua"], family: "lua", language: || tree_sitter_lua::LANGUAGE.into(), queries: &[q!("lua")] },
    Lang { name: "objc", exts: &["m", "mm"], family: "c", language: || tree_sitter_objc::LANGUAGE.into(), queries: &[q!("objc")] },
];

pub fn by_name(name: &str) -> Option<&'static Lang> {
    LANGS.iter().find(|l| l.name == name)
}

pub fn for_path(path: &str) -> Option<&'static Lang> {
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    LANGS.iter().find(|l| l.exts.contains(&ext.as_str()))
}
