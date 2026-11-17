//! Milestone 8, the opt-in vector arm, end to end through the `fox` binary with a tiny synthetic
//! model2vec model (`FOXDB_VECTORS_MODEL`): off or without a current, intact index, `find`
//! answers byte for byte as without the feature; on, a question in words also reaches symbols
//! that share no word with it; identifiers never use it; an index run refreshes it, parsing only
//! changed files.
#![cfg(feature = "vectors")]

use std::fs;
use std::path::Path;
use std::process::Command;

const FOX: &str = env!("CARGO_BIN_EXE_fox");

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A 4-dimensional WordPiece model2vec model: `grow` means `scale`, `pods` means `replica`.
fn tiny_model(dir: &Path) {
    let words: &[(&str, [f32; 4])] = &[
        ("[UNK]", [0.0; 4]),
        ("grow", [1.0, 0.0, 0.0, 0.0]),
        ("scale", [1.0, 0.0, 0.0, 0.0]),
        ("pods", [0.0, 1.0, 0.0, 0.0]),
        ("replica", [0.0, 1.0, 0.0, 0.0]),
        ("parse", [0.0, 0.0, 1.0, 0.0]),
        ("tokens", [0.0, 0.0, 1.0, 0.0]),
        ("cache", [0.0, 0.0, 0.0, 1.0]),
    ];
    let vocab: serde_json::Map<String, serde_json::Value> = words.iter().enumerate().map(|(i, w)| (w.0.to_string(), i.into())).collect();
    let tok = serde_json::json!({
        "version": "1.0", "truncation": null, "padding": null, "added_tokens": [],
        "normalizer": {"type": "BertNormalizer", "clean_text": true, "handle_chinese_chars": true, "strip_accents": null, "lowercase": true},
        "pre_tokenizer": {"type": "BertPreTokenizer"}, "post_processor": null, "decoder": null,
        "model": {"type": "WordPiece", "unk_token": "[UNK]", "continuing_subword_prefix": "##", "max_input_chars_per_word": 100, "vocab": vocab}
    });
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("tokenizer.json"), tok.to_string()).unwrap();
    let data: Vec<u8> = words.iter().flat_map(|w| w.1.iter().flat_map(|x| x.to_le_bytes())).collect();
    let mut head =
        serde_json::json!({"embeddings": {"dtype": "F32", "shape": [words.len(), 4], "data_offsets": [0, data.len()]}}).to_string();
    while !head.len().is_multiple_of(8) {
        head.push(' ');
    }
    let mut st = (head.len() as u64).to_le_bytes().to_vec();
    st.extend_from_slice(head.as_bytes());
    st.extend_from_slice(&data);
    fs::write(dir.join("model.safetensors"), st).unwrap();
}

struct Env {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
}

fn fox(e: &Env, args: &[&str], vectors: &str) -> (i32, String, String) {
    let o = Command::new(FOX)
        .args(args)
        .current_dir(e.repo.path())
        .env("HOME", e.home.path())
        .env("FOXDB_HOME", e.home.path().join("fx"))
        .env("FOXDB_NO_DAEMON", "1")
        .env("FOXDB_CLAUDE_PROJECTS", e.home.path().join("no-claude"))
        .env("FOXDB_CODEX_SESSIONS", e.home.path().join("no-codex"))
        .env("FOXDB_VECTORS_MODEL", e.home.path().join("model"))
        .env("FOXDB_VECTORS", vectors)
        .output()
        .unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

const DEPLOY: &str = "/// Changes the size of a set.\npub fn scale_replica_set(n: u32) -> u32 {\n    n + 1\n}\n";
const LEXER: &str = "pub fn parse_tokens(s: &str) -> usize {\n    s.len()\n}\n";

#[test]
fn the_vector_arm_is_opt_in_exact_and_falls_back() {
    let e = Env { home: tempfile::tempdir().unwrap(), repo: tempfile::tempdir().unwrap() };
    tiny_model(&e.home.path().join("model"));
    write(e.repo.path(), "src/deploy.rs", DEPLOY);
    write(e.repo.path(), "src/lexer.rs", LEXER);
    let (rc, out, err) = fox(&e, &["index", "--no-global", "."], "0");
    assert_eq!(rc, 0, "{out}{err}");
    assert!(!out.contains("vectors"), "off: no vector work in an index run: {out}");
    let q = "please grow the pods now";
    let off = fox(&e, &["find", q], "0").1;
    assert!(off.contains("no matches"), "{off}");
    // On, but no index yet: the same answer.
    assert_eq!(fox(&e, &["find", q], "1").1, off);
    assert!(!e.home.path().join("fx/vectors").exists(), "nothing written while the arm has no index");

    let (rc, out, err) = fox(&e, &["vectors", "build"], "1");
    assert_eq!(rc, 0, "{out}{err}");
    assert!(out.contains("vector index of 2 rows (2 files parsed, 2 texts embedded)"), "{out}");
    assert!(fox(&e, &["vectors", "status"], "1").1.contains("2 rows, current"));
    let on = fox(&e, &["find", q], "1").1;
    assert!(on.contains("scale_replica_set") && !on.contains("parse_tokens"), "{on}");
    // Off again: byte-identical to before. Identifiers and single tokens never use the arm.
    assert_eq!(fox(&e, &["find", q], "0").1, off);
    // A phrase of fewer than five words does not either.
    assert_eq!(fox(&e, &["find", "grow the pods please"], "1").1, fox(&e, &["find", "grow the pods please"], "0").1);
    for id in ["scale_replica_set", "parse_tokens", "deploy"] {
        assert_eq!(fox(&e, &["find", id], "1").1, fox(&e, &["find", id], "0").1, "{id}");
    }
    // A built index is current: building again embeds nothing.
    assert!(fox(&e, &["vectors", "build"], "1").1.contains("already current"));

    // A change indexed with the arm off leaves the index stale: unused, the answer is as off.
    write(e.repo.path(), "src/lexer.rs", &format!("{LEXER}\npub fn cache_tokens() {{}}\n"));
    assert_eq!(fox(&e, &["index", "--no-global", "."], "0").0, 0);
    assert!(fox(&e, &["vectors", "status"], "1").1.contains("stale"));
    let off2 = fox(&e, &["find", q], "0").1;
    assert_eq!(fox(&e, &["find", q], "1").1, off2);

    // An index run with the arm on refreshes it: only the changed file is parsed, and only the
    // new text is embedded (the unchanged one is reused by its hash).
    write(e.repo.path(), "src/lexer.rs", &format!("{LEXER}\npub fn cache_tokens() {{ let _ = 1; }}\n"));
    let (rc, out, err) = fox(&e, &["index", "--no-global", "."], "1");
    assert_eq!(rc, 0, "{out}{err}");
    assert!(out.contains("; vectors: 3 rows, 1 files parsed, 1 embedded"), "{out}");
    assert!(fox(&e, &["find", q], "1").1.contains("scale_replica_set"));
    // A run that changes nothing the index covers moves only its stamp: no parse, no embedding.
    write(e.repo.path(), "notes.txt", "unrelated\n");
    let (rc, out, _) = fox(&e, &["index", "--no-global", "."], "1");
    assert!(rc == 0 && !out.contains("vectors"), "{out}");
    assert!(fox(&e, &["vectors", "status"], "1").1.contains("3 rows, current"));
    assert!(fox(&e, &["find", q], "1").1.contains("scale_replica_set"));

    // A damaged index is refused: the answer is the one without the arm.
    let ix = fs::read_dir(e.home.path().join("fx/vectors"))
        .unwrap()
        .flatten()
        .find(|f| f.path().extension().is_some_and(|x| x == "fv"))
        .unwrap()
        .path();
    let mut b = fs::read(&ix).unwrap();
    let n = b.len();
    b[n - 1] ^= 0x10;
    fs::write(&ix, &b).unwrap();
    assert_eq!(fox(&e, &["find", q], "1").1, fox(&e, &["find", q], "0").1);
    assert!(fox(&e, &["vectors", "status"], "1").1.contains("unreadable"));
    // ... and rebuilt in full by the next build.
    assert!(fox(&e, &["vectors", "build"], "1").1.contains("vector index of 3 rows (2 files parsed"));
    assert!(fox(&e, &["find", q], "1").1.contains("scale_replica_set"));
}

#[test]
fn setup_names_the_model_and_downloads_nothing_without_consent() {
    let e = Env { home: tempfile::tempdir().unwrap(), repo: tempfile::tempdir().unwrap() };
    let o = Command::new(FOX)
        .args(["vectors", "setup"])
        .current_dir(e.repo.path())
        .env("HOME", e.home.path())
        .env("FOXDB_HOME", e.home.path().join("fx"))
        .env("FOXDB_NO_DAEMON", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success());
    for want in ["potion-code-16M", "huggingface.co/minishlab/potion-code-16M", "65.3 MB", "MIT", "nothing downloaded"] {
        assert!(err.contains(want), "{want}: {err}");
    }
    assert!(!e.home.path().join("fx/models").exists());
}
