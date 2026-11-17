//! The opt-in vector arm: questions in words also read the nearest symbols by meaning.
//!
//! Built only with the Cargo feature `vectors`, and used only with `vectors = true` in
//! config.toml (or `FOXDB_VECTORS=1`) once `fox vectors setup` has fetched the model. Without all
//! three nothing here runs: no file is read, no thread starts, `find` answers as before.
//!
//! - **Model:** a static model2vec model ([`MODEL`]): a token embedding table read from its
//!   safetensors file and a Hugging Face tokenizer. A text's vector is the mean of its tokens'
//!   rows, L2-normalized. No neural network runs, so embedding is cheap on a CPU.
//! - **What is embedded:** every tree-sitter symbol, as its qualified name, path and the text the
//!   body arm reads (`tsindex::body_text`: comment block above it, then its source); and every
//!   doc heading and milestone, as its title and path. (Symbols alone pushed headings that
//!   answer a phrase down the list, by agreeing with the keyword arm on code.)
//! - **Index:** one flat file per project under `FOXDB_HOME/vectors/`, int8 rows with a scale
//!   each, searched exactly (no approximate index: a project of a few hundred thousand symbols
//!   scans in milliseconds). The file carries the project generation it was built at and a
//!   checksum; a file that is stale, damaged or from another model is not read, and `find`
//!   answers without the arm, as it does with the feature off.
//! - **Refresh:** after each index run (`fox index`, `fox watch`) while the arm is on, and by
//!   `fox vectors build`. Only files whose content hash moved are parsed again, and a text
//!   embedded before is reused by its hash.
//! - **Query:** only sentences ([`crate::search::Route::Text`] queries of [`MIN_WORDS`] words or
//!   more), never a bare identifier, a single token or a short phrase, and only for `any`, `symbol` or `doc` searches in the current
//!   project. The arm
//!   is fused after the others with weight [`W_VECTOR`].

use crate::db;
use crate::project::Project;
use crate::search::{Embedder, Scope, VectorBackend};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use rusqlite::Connection;
use std::collections::HashMap;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// RRF weight of the vector arm (the keyword arm has 1.0).
pub const W_VECTOR: f64 = 0.5;

/// Fewest words a query needs to use the arm: a sentence, not a few keywords or a phrase. On the
/// lexical eval sets (two to four words) the arm moved a heading that answers a phrase from
/// first to second place.
pub const MIN_WORDS: usize = 5;

/// Does `q` read the vector arm (given the arm is on)? Sentences only.
pub fn wants(q: &str) -> bool {
    q.split_whitespace().count() >= MIN_WORDS
}

/// Most tokens embedded per text (model2vec's default).
const MAX_TOKENS: usize = 512;

pub struct ModelSpec {
    pub name: &'static str,
    pub repo: &'static str,
    /// The pinned commit of the model repository.
    pub rev: &'static str,
    pub licence: &'static str,
    /// (file, size in bytes, sha256)
    pub files: &'static [(&'static str, u64, &'static str)],
}

/// The one supported model: model2vec `potion-code-16M` (256 dimensions, MIT).
pub const MODEL: ModelSpec = ModelSpec {
    name: "potion-code-16M",
    repo: "minishlab/potion-code-16M",
    rev: "1b0ff71095656b23306542bbad34a09109673720",
    licence: "MIT",
    files: &[
        ("config.json", 97, "edf07552b5d768d556ded176d19f3a34f25360548de3a246d226ce8e28647914"),
        ("tokenizer.json", 1_041_917, "8e84217af15e70e8127c855435fc3d8a4cd91d7bbe686f72e75f188118ec78ae"),
        ("model.safetensors", 64_299_272, "ca6159081a6e96cebe4ad878e5e8437bfccc761e8db16223370149cd2faa6c0b"),
    ],
};

impl ModelSpec {
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.1).sum()
    }

    pub fn url(&self, file: &str) -> String {
        format!("https://huggingface.co/{}/resolve/{}/{file}", self.repo, self.rev)
    }
}

/// Where the model lives: `FOXDB_HOME/models/<name>`, or `FOXDB_VECTORS_MODEL` (a directory
/// holding a model2vec model's `tokenizer.json` and `model.safetensors`, for a copy you already
/// have; its files are not checked against [`MODEL`]).
pub fn model_dir() -> PathBuf {
    match std::env::var_os("FOXDB_VECTORS_MODEL").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => db::home().join("models").join(MODEL.name),
    }
}

fn index_dir() -> PathBuf {
    db::home().join("vectors")
}

fn index_path(pid: i64) -> PathBuf {
    index_dir().join(format!("p{pid}.fv"))
}

fn stamp_path(pid: i64) -> PathBuf {
    index_dir().join(format!("p{pid}.stamp"))
}

/// Are the model's files in place (by size; `setup` checked their digests)?
pub fn model_present() -> bool {
    if std::env::var_os("FOXDB_VECTORS_MODEL").is_some_and(|v| !v.is_empty()) {
        return ["tokenizer.json", "model.safetensors"].iter().all(|f| model_dir().join(f).is_file());
    }
    MODEL.files.iter().all(|(f, size, _)| std::fs::metadata(model_dir().join(f)).is_ok_and(|m| m.len() == *size))
}

// ---------------------------------------------------------------------------------------------
// Setup: the only network access, and only on request.

/// What `fox vectors setup` will fetch, shown before it fetches anything.
pub fn setup_plan() -> String {
    let mut out = format!(
        "model: {} (model2vec static embeddings, 256 dimensions, licence {})\nsource: https://huggingface.co/{} at commit {}\nsize: {:.1} MB in {} files\ndestination: {}\n",
        MODEL.name,
        MODEL.licence,
        MODEL.repo,
        MODEL.rev,
        MODEL.size() as f64 / 1e6,
        MODEL.files.len(),
        model_dir().display()
    );
    for (f, size, _) in MODEL.files {
        out += &format!("  {f} ({size} bytes) from {}\n", MODEL.url(f));
    }
    out
}

/// Download the model's files over HTTPS, check each one's size and sha256, and move it into
/// place. A file already in place with the right digest is kept.
pub fn setup() -> Result<String> {
    use sha2::Digest;
    let dir = model_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut log = vec![];
    for (f, size, sha) in MODEL.files {
        let dst = dir.join(f);
        if std::fs::read(&dst).is_ok_and(|b| b.len() as u64 == *size && hex(&sha2::Sha256::digest(&b)) == *sha) {
            log.push(format!("{f}: already in place"));
            continue;
        }
        let url = MODEL.url(f);
        let mut resp = ureq::get(&url).call().with_context(|| format!("cannot fetch {url}"))?;
        let body = resp.body_mut().with_config().limit(size + 1).read_to_vec().with_context(|| format!("cannot read {url}"))?;
        if body.len() as u64 != *size {
            bail!("{url}: got {} bytes, expected {size}", body.len());
        }
        let got = hex(&sha2::Sha256::digest(&body));
        if got != *sha {
            bail!("{url}: sha256 {got}, expected {sha}");
        }
        let part = dir.join(format!("{f}.part"));
        std::fs::write(&part, &body)?;
        std::fs::rename(&part, &dst)?;
        log.push(format!("{f}: {} bytes, sha256 ok", body.len()));
    }
    Ok(log.join("\n"))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------------------------------------------------------------------------------------
// The tokenizer: BERT's (the one model2vec's potion models use), read from `tokenizer.json`.

/// A BERT WordPiece tokenizer: `BertNormalizer` (clean text, spaces around CJK ideographs,
/// lowercase, accents stripped), `BertPreTokenizer` (split on whitespace and punctuation), then
/// greedy longest-match WordPiece with `##` continuations. It matches the reference tokenizer on
/// ASCII text; elsewhere it treats every non-alphanumeric character as punctuation, the same at
/// index and at query time.
pub struct WordPiece {
    vocab: HashMap<String, u32>,
    unk: u32,
    prefix: String,
    max_chars: usize,
}

impl WordPiece {
    pub fn open(path: &Path) -> Result<WordPiece> {
        #[derive(serde::Deserialize)]
        struct File<'a> {
            #[serde(borrow)]
            normalizer: Option<Kind<'a>>,
            #[serde(borrow)]
            pre_tokenizer: Option<Kind<'a>>,
            #[serde(borrow)]
            model: Wp<'a>,
        }
        #[derive(serde::Deserialize)]
        struct Kind<'a> {
            #[serde(rename = "type")]
            ty: &'a str,
            #[serde(default)]
            lowercase: Option<bool>,
            #[serde(default)]
            strip_accents: Option<bool>,
        }
        #[derive(serde::Deserialize)]
        struct Wp<'a> {
            #[serde(rename = "type")]
            ty: &'a str,
            unk_token: String,
            continuing_subword_prefix: Option<String>,
            max_input_chars_per_word: Option<usize>,
            #[serde(borrow)]
            vocab: HashMap<std::borrow::Cow<'a, str>, u32>,
        }
        let text = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let f: File = serde_json::from_slice(&text).with_context(|| format!("{} is not a tokenizer", path.display()))?;
        let bert = |k: &Option<Kind>, ty: &str| k.as_ref().is_some_and(|k| k.ty == ty);
        if f.model.ty != "WordPiece"
            || !bert(&f.normalizer, "BertNormalizer")
            || !bert(&f.pre_tokenizer, "BertPreTokenizer")
            || f.normalizer.as_ref().is_some_and(|n| n.lowercase == Some(false) || n.strip_accents == Some(false))
        {
            bail!("{}: only lowercasing BERT WordPiece tokenizers are supported", path.display());
        }
        let vocab: HashMap<String, u32> = f.model.vocab.into_iter().map(|(k, v)| (k.into_owned(), v)).collect();
        let unk = *vocab.get(&f.model.unk_token).ok_or_else(|| anyhow::anyhow!("the unknown token is not in the vocabulary"))?;
        Ok(WordPiece {
            vocab,
            unk,
            prefix: f.model.continuing_subword_prefix.unwrap_or_else(|| "##".into()),
            max_chars: f.model.max_input_chars_per_word.unwrap_or(100),
        })
    }

    /// Token ids of `text`, at most `max`.
    pub fn encode(&self, text: &str, max: usize) -> Vec<u32> {
        let mut out = vec![];
        for word in pre_tokenize(&normalize_text(text)) {
            if out.len() >= max {
                break;
            }
            self.word(&word, &mut out);
        }
        out.truncate(max);
        out
    }

    fn word(&self, w: &str, out: &mut Vec<u32>) {
        let chars: Vec<(usize, char)> = w.char_indices().collect();
        if chars.len() > self.max_chars {
            out.push(self.unk);
            return;
        }
        let at = out.len();
        let mut start = 0;
        let mut piece = String::new();
        while start < chars.len() {
            let mut end = chars.len();
            let mut found = None;
            while end > start {
                let (a, b) = (chars[start].0, chars.get(end).map_or(w.len(), |c| c.0));
                piece.clear();
                if start > 0 {
                    piece.push_str(&self.prefix);
                }
                piece.push_str(&w[a..b]);
                if let Some(id) = self.vocab.get(piece.as_str()) {
                    found = Some(*id);
                    break;
                }
                end -= 1;
            }
            match found {
                Some(id) => out.push(id),
                None => {
                    out.truncate(at);
                    out.push(self.unk);
                    return;
                }
            }
            start = end;
        }
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF | 0x2A700..=0x2B73F | 0x2B740..=0x2B81F | 0x2B820..=0x2CEAF | 0xF900..=0xFAFF | 0x2F800..=0x2FA1F)
}

/// `BertNormalizer`: drop control characters, whitespace to spaces, spaces around CJK ideographs,
/// lowercase, strip accents.
fn normalize_text(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let mut s = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\0' || c == '\u{fffd}' || (c.is_control() && !matches!(c, '\t' | '\n' | '\r')) {
            continue;
        }
        if c.is_whitespace() {
            s.push(' ');
        } else if is_cjk(c) {
            s.push(' ');
            s.push(c);
            s.push(' ');
        } else {
            s.push(c);
        }
    }
    if s.is_ascii() {
        s.make_ascii_lowercase();
        return s;
    }
    s.to_lowercase().nfd().filter(|c| !unicode_normalization::char::is_combining_mark(*c)).collect()
}

/// `BertPreTokenizer`: words split on whitespace, each punctuation character its own word.
fn pre_tokenize(s: &str) -> Vec<String> {
    let mut out = vec![];
    for w in s.split_whitespace() {
        let mut cur = String::new();
        for c in w.chars() {
            if c.is_ascii_punctuation() || !(c.is_alphanumeric() || c.is_ascii()) {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(c.to_string());
            } else {
                cur.push(c);
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The model.

#[derive(Clone, Copy)]
enum Dtype {
    F32,
    F16,
    F64,
}

enum Table {
    /// Rows read on demand from the file (a query embeds a few tokens).
    File { file: std::fs::File, offset: u64, dtype: Dtype },
    /// The whole table in memory (building an index).
    Full(Vec<f32>),
}

pub struct Model {
    tok: WordPiece,
    table: Table,
    rows: usize,
    dims: usize,
    /// Per-token weights, when the model has them.
    weights: Option<Vec<f32>>,
    /// Token id to table row, when the model's vocabulary was quantized.
    mapping: Option<Vec<u32>>,
    unk: Option<u32>,
}

#[derive(serde::Deserialize)]
struct Tensor {
    dtype: String,
    shape: Vec<usize>,
    data_offsets: (u64, u64),
}

impl Dtype {
    fn width(self) -> usize {
        match self {
            Dtype::F16 => 2,
            Dtype::F32 => 4,
            Dtype::F64 => 8,
        }
    }
}

fn dtype(s: &str) -> Result<Dtype> {
    match s {
        "F32" => Ok(Dtype::F32),
        "F16" => Ok(Dtype::F16),
        "F64" => Ok(Dtype::F64),
        other => bail!("unsupported tensor type {other}"),
    }
}

fn decode(bytes: &[u8], t: Dtype) -> Vec<f32> {
    match t {
        Dtype::F32 => bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect(),
        Dtype::F16 => bytes.chunks_exact(2).map(|b| f16_to_f32(u16::from_le_bytes([b[0], b[1]]))).collect(),
        Dtype::F64 => bytes.chunks_exact(8).map(|b| f64::from_le_bytes(b.try_into().unwrap()) as f32).collect(),
    }
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = match (exp, frac) {
        (0, 0) => sign,
        (0, _) => {
            // Subnormal: normalize.
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((f & 0x3ff) << 13)
        }
        (0x1f, _) => sign | 0x7f80_0000 | (frac << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}

impl Model {
    /// Open the model in `dir`. `full` reads the whole table (for building); otherwise rows are
    /// read from the file as a query needs them.
    pub fn open(dir: &Path, full: bool) -> Result<Model> {
        let tok = WordPiece::open(&dir.join("tokenizer.json"))?;
        let unk = Some(tok.unk);
        let path = dir.join("model.safetensors");
        let file = std::fs::File::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
        let mut len = [0u8; 8];
        file.read_exact_at(&mut len, 0)?;
        let hlen = u64::from_le_bytes(len);
        if hlen > 100 << 20 {
            bail!("{}: not a safetensors file", path.display());
        }
        let mut head = vec![0u8; hlen as usize];
        file.read_exact_at(&mut head, 8)?;
        let mut tensors: HashMap<String, serde_json::Value> = serde_json::from_slice(&head)?;
        tensors.remove("__metadata__");
        let tensor = |name: &str| -> Result<Option<Tensor>> {
            tensors.get(name).map(|v| serde_json::from_value::<Tensor>(v.clone()).map_err(Into::into)).transpose()
        };
        let base = 8 + hlen;
        let emb = tensor("embeddings")?.ok_or_else(|| anyhow::anyhow!("{}: no embeddings tensor", path.display()))?;
        let [rows, dims] = emb.shape[..] else { bail!("embeddings are not a matrix") };
        let t = dtype(&emb.dtype)?;
        let width = t.width();
        if emb.data_offsets.1 - emb.data_offsets.0 != (rows * dims * width) as u64 {
            bail!("embeddings size does not match their shape");
        }
        let read = |t: &Tensor| -> Result<Vec<u8>> {
            let mut b = vec![0u8; (t.data_offsets.1 - t.data_offsets.0) as usize];
            file.read_exact_at(&mut b, base + t.data_offsets.0)?;
            Ok(b)
        };
        let weights = match tensor("weights")? {
            Some(w) => Some(decode(&read(&w)?, dtype(&w.dtype)?)),
            None => None,
        };
        let mapping = match tensor("mapping")?.or(tensor("mappings")?) {
            Some(m) => {
                let b = read(&m)?;
                Some(match m.dtype.as_str() {
                    "I32" => b.chunks_exact(4).map(|x| i32::from_le_bytes([x[0], x[1], x[2], x[3]]) as u32).collect(),
                    "I64" => b.chunks_exact(8).map(|x| i64::from_le_bytes(x.try_into().unwrap()) as u32).collect(),
                    other => bail!("unsupported mapping type {other}"),
                })
            }
            None => None,
        };
        let table =
            if full { Table::Full(decode(&read(&emb)?, t)) } else { Table::File { file, offset: base + emb.data_offsets.0, dtype: t } };
        Ok(Model { tok, table, rows, dims, weights, mapping, unk })
    }

    fn row(&self, id: u32, out: &mut [f32]) -> Result<f32> {
        let r = match &self.mapping {
            Some(m) => *m.get(id as usize).unwrap_or(&u32::MAX) as usize,
            None => id as usize,
        };
        if r >= self.rows {
            return Ok(0.0);
        }
        let w = self.weights.as_ref().and_then(|w| w.get(id as usize).copied()).unwrap_or(1.0);
        match &self.table {
            Table::Full(v) => {
                for (o, x) in out.iter_mut().zip(&v[r * self.dims..(r + 1) * self.dims]) {
                    *o += w * x;
                }
            }
            Table::File { file, offset, dtype } => {
                let width = dtype.width();
                let mut b = vec![0u8; self.dims * width];
                file.read_exact_at(&mut b, offset + (r * self.dims * width) as u64)?;
                for (o, x) in out.iter_mut().zip(decode(&b, *dtype)) {
                    *o += w * x;
                }
            }
        }
        Ok(1.0)
    }

    /// One text's vector: mean of its token rows, L2-normalized (all zeros for no tokens).
    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let ids = self.tok.encode(text, MAX_TOKENS);
        let mut v = vec![0f32; self.dims];
        let mut n = 0f32;
        for &id in ids.iter().filter(|&&id| Some(id) != self.unk) {
            n += self.row(id, &mut v)?;
        }
        if n > 0.0 {
            normalize(&mut v);
        }
        Ok(v)
    }
}

fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

impl Embedder for Model {
    fn dims(&self) -> usize {
        self.dims
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.par_iter().map(|t| self.embed_one(t)).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// The index file.

const MAGIC: &[u8; 8] = b"FOXVEC01";

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
struct Header {
    model: String,
    project: i64,
    dims: usize,
    count: usize,
    /// What the rows were built from (see [`fingerprint`]): equal means nothing to embed again.
    fingerprint: u64,
    checksum: u64,
}

/// Which store generation an index file is current for. A separate small file, so an index run
/// that changed nothing the index covers moves only this, and tied to one index file by its
/// checksum: a stamp left from another file never makes it current.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
struct Stamp {
    generation: i64,
    checksum: u64,
}

fn read_stamp(pid: i64) -> Option<Stamp> {
    serde_json::from_slice(&std::fs::read(stamp_path(pid)).ok()?).ok()
}

fn write_stamp(pid: i64, s: &Stamp) -> Result<()> {
    let tmp = stamp_path(pid).with_extension("stamp.tmp");
    std::fs::write(&tmp, serde_json::to_vec(s)?)?;
    std::fs::rename(&tmp, stamp_path(pid))?;
    Ok(())
}

/// One project's vectors: node ids in ascending order, an int8 row and scale each, the hash of
/// the text each row was embedded from (to reuse it) and of the file version it came from.
pub struct Index {
    head: Header,
    ids: Vec<i64>,
    texts: Vec<u64>,
    files: Vec<u64>,
    scales: Vec<f32>,
    vecs: Vec<i8>,
}

/// Fast 64-bit hash for checksums and text keys (not cryptographic): four independent lanes over
/// 32-byte blocks, so a 60 MB index checks in a few milliseconds.
fn hash64(b: &[u8]) -> u64 {
    const M: u64 = 0x9e37_79b9_7f4a_7c15;
    let mix = |h: u64, w: u64| (h ^ w).wrapping_mul(M).rotate_left(31);
    let mut lanes = [M ^ b.len() as u64, 0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210, 0x0f1e_2d3c_4b5a_6978];
    let mut it = b.chunks_exact(32);
    for c in &mut it {
        for (i, l) in lanes.iter_mut().enumerate() {
            *l = mix(*l, u64::from_le_bytes(c[i * 8..i * 8 + 8].try_into().unwrap()));
        }
    }
    let mut h = lanes.iter().fold(M, |h, l| mix(h, *l));
    for (i, x) in it.remainder().iter().enumerate() {
        h = mix(h, (*x as u64) << (i % 8 * 8) | (i as u64) << 56);
    }
    h ^ (h >> 29)
}

fn quantize(v: &[f32]) -> (f32, Vec<i8>) {
    let max = v.iter().fold(0f32, |m, x| m.max(x.abs()));
    if max == 0.0 {
        return (0.0, vec![0; v.len()]);
    }
    let s = max / 127.0;
    (s, v.iter().map(|x| (x / s).round().clamp(-127.0, 127.0) as i8).collect())
}

/// Bytes per row: id, text hash, file hash, scale, vector.
fn row_bytes(dims: usize) -> usize {
    8 + 8 + 8 + 4 + dims
}

impl Index {
    fn body(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(self.ids.len() * row_bytes(self.head.dims));
        self.ids.iter().for_each(|x| b.extend_from_slice(&x.to_le_bytes()));
        self.texts.iter().for_each(|x| b.extend_from_slice(&x.to_le_bytes()));
        self.files.iter().for_each(|x| b.extend_from_slice(&x.to_le_bytes()));
        self.scales.iter().for_each(|x| b.extend_from_slice(&x.to_le_bytes()));
        b.extend(self.vecs.iter().map(|x| *x as u8));
        b
    }

    fn write(&mut self, path: &Path) -> Result<()> {
        let body = self.body();
        self.head.checksum = hash64(&body);
        let head = serde_json::to_vec(&self.head)?;
        let mut out = Vec::with_capacity(12 + head.len() + body.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(head.len() as u32).to_le_bytes());
        out.extend_from_slice(&head);
        out.extend_from_slice(&body);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The header of an index file, and whether the file has the size it names (the body is not
    /// read or checked).
    fn read_header(path: &Path) -> Result<Header> {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut pre = [0u8; 12];
        f.read_exact(&mut pre)?;
        if &pre[..8] != MAGIC {
            bail!("{}: not a vector index", path.display());
        }
        let hl = u32::from_le_bytes(pre[8..12].try_into().unwrap()) as usize;
        let mut h = vec![0u8; hl];
        f.read_exact(&mut h)?;
        let head: Header = serde_json::from_slice(&h)?;
        if f.metadata()?.len() != (12 + hl + head.count * row_bytes(head.dims)) as u64 {
            bail!("{}: size does not match its header", path.display());
        }
        Ok(head)
    }

    /// Read and check an index file: magic, header, sizes and checksum.
    fn read(path: &Path) -> Result<Index> {
        let b = std::fs::read(path)?;
        if b.len() < 12 || &b[..8] != MAGIC {
            bail!("{}: not a vector index", path.display());
        }
        let hl = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        let head: Header = serde_json::from_slice(b.get(12..12 + hl).ok_or_else(|| anyhow::anyhow!("truncated header"))?)?;
        let body = &b[12 + hl..];
        let n = head.count;
        if body.len() != n * row_bytes(head.dims) {
            bail!("{}: size does not match its header", path.display());
        }
        if hash64(body) != head.checksum {
            bail!("{}: checksum mismatch", path.display());
        }
        let u64s = |b: &[u8]| -> Vec<u64> { b.chunks_exact(8).map(|x| u64::from_le_bytes(x.try_into().unwrap())).collect() };
        let (ids, rest) = body.split_at(n * 8);
        let (texts, rest) = rest.split_at(n * 8);
        let (files, rest) = rest.split_at(n * 8);
        let (scales, vecs) = rest.split_at(n * 4);
        Ok(Index {
            ids: ids.chunks_exact(8).map(|x| i64::from_le_bytes(x.try_into().unwrap())).collect(),
            texts: u64s(texts),
            files: u64s(files),
            scales: scales.chunks_exact(4).map(|x| f32::from_le_bytes(x.try_into().unwrap())).collect(),
            vecs: vecs.iter().map(|x| *x as i8).collect(),
            head,
        })
    }

    /// Is this index current for project `pid` as `c` sees it now? Its stamp must name this file
    /// and the project's present generation, and the model must be the one in use.
    fn current(&self, c: &Connection, pid: i64) -> bool {
        self.head.model == MODEL.rev
            && self.head.project == pid
            && read_stamp(pid).is_some_and(|s| s.checksum == self.head.checksum && s.generation == db::project_generation(c, pid))
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

impl VectorBackend for Index {
    /// Exact nearest neighbours by cosine similarity. The scope is this index's project.
    fn nearest(&self, v: &[f32], _s: &Scope, k: usize) -> Result<Vec<(i64, f32)>> {
        let d = self.head.dims;
        if v.len() != d || k == 0 {
            return Ok(vec![]);
        }
        // The query in int8 too: an integer dot product per row.
        let (qs, q) = quantize(v);
        let top = |range: std::ops::Range<usize>| -> Vec<(f32, usize)> {
            let mut best: Vec<(f32, usize)> = Vec::with_capacity(k + 1);
            for i in range {
                let row = &self.vecs[i * d..(i + 1) * d];
                let dot: i32 = row.iter().zip(&q).map(|(a, b)| *a as i32 * *b as i32).sum();
                let s = self.scales[i] * qs * dot as f32;
                if best.len() < k || s > best[best.len() - 1].0 {
                    let at = best.partition_point(|x| x.0 >= s);
                    best.insert(at, (s, i));
                    best.truncate(k);
                }
            }
            best
        };
        let n = self.ids.len();
        let chunk = 65_536;
        // One thread below a chunk: starting the pool would cost more than the scan.
        let mut all: Vec<(f32, usize)> = if n <= chunk {
            top(0..n)
        } else {
            (0..n.div_ceil(chunk)).into_par_iter().flat_map_iter(|c| top(c * chunk..((c + 1) * chunk).min(n))).collect()
        };
        // Best first; ties by node id, so the order is deterministic.
        all.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(self.ids[a.1].cmp(&self.ids[b.1])));
        Ok(all.into_iter().filter(|x| x.0 > 0.0).take(k).map(|(s, i)| (self.ids[i], s)).collect())
    }
}

// ---------------------------------------------------------------------------------------------
// Process caches: the query model and each project's loaded index.

fn query_model() -> Option<Arc<Model>> {
    static M: Mutex<Option<Arc<Model>>> = Mutex::new(None);
    let mut g = M.lock().ok()?;
    if g.is_none() && model_present() {
        *g = Model::open(&model_dir(), false).ok().map(Arc::new);
    }
    g.clone()
}

/// Project `pid`'s index if its file is intact, from this process's cache while the file is
/// unchanged (a damaged file is remembered as such until it changes).
fn loaded(pid: i64) -> Option<Arc<Index>> {
    type FileStamp = (u64, std::time::SystemTime);
    type Cache = HashMap<i64, (FileStamp, Option<Arc<Index>>)>;
    static C: Mutex<Option<Cache>> = Mutex::new(None);
    let path = index_path(pid);
    let m = std::fs::metadata(&path).ok()?;
    let stamp = (m.len(), m.modified().ok()?);
    let mut g = C.lock().ok()?;
    let map = g.get_or_insert_with(HashMap::new);
    if let Some((s, ix)) = map.get(&pid)
        && *s == stamp
    {
        return ix.clone();
    }
    let ix = Index::read(&path).ok().map(Arc::new);
    map.insert(pid, (stamp, ix.clone()));
    ix
}

/// The vector arm of a search: node ids nearest to `q` in project `pid`, best first. Empty when
/// the model or a current index is missing, or anything fails (the other arms answer alone).
pub fn arm(c: &Connection, pid: i64, q: &str, k: usize) -> Result<Vec<i64>> {
    let run = || -> Result<Vec<i64>> {
        let t = std::time::Instant::now();
        // A stale stamp is the common case after a save: settle it before reading the index.
        if read_stamp(pid).is_none_or(|s| s.generation != db::project_generation(c, pid)) {
            return Ok(vec![]);
        }
        let Some(ix) = loaded(pid) else { return Ok(vec![]) };
        trace("vectors load index", t);
        if !ix.current(c, pid) {
            return Ok(vec![]);
        }
        let Some(m) = query_model() else { return Ok(vec![]) };
        trace("vectors load model", t);
        let v = m.embed_one(&crate::tsindex::split_words(q))?;
        trace("vectors embed", t);
        let out = ix.nearest(&v, &Scope::default(), k)?.into_iter().map(|x| x.0).collect();
        trace("vectors nearest", t);
        Ok(out)
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|_| Ok(vec![]))
}

fn trace(what: &str, t0: std::time::Instant) {
    if std::env::var_os("FOXDB_TRACE").is_some() {
        eprintln!("{what} {:?}", t0.elapsed());
    }
}

// ---------------------------------------------------------------------------------------------
// Building and refreshing.

#[derive(Debug, Default)]
pub struct Stats {
    pub rows: usize,
    /// Files parsed again (their content hash moved, or no earlier index).
    pub parsed: usize,
    /// Texts embedded now (not reused by hash).
    pub embedded: usize,
    /// Nothing the index covers changed (at most its stamp moved).
    pub current: bool,
    pub ms: u128,
}

/// The text a symbol is embedded from: its qualified name and path as words, then its body text.
fn chunk_text(qualified: &str, path: &str, body: &str) -> String {
    format!("{}\n{}\n{}", crate::tsindex::split_words(qualified), crate::tsindex::split_words(path), body)
}

/// What a project's vector rows are built from, read in one snapshot: each tree-sitter file's
/// content hash, the tree-sitter symbols' ids (a re-parsed file gets new ones) and the doc
/// headings.
struct Source {
    files: HashMap<String, String>,
    /// (node id, path, definition index)
    syms: Vec<(i64, String, usize)>,
    /// (node id, title, path)
    docs: Vec<(i64, String, String)>,
    fingerprint: u64,
}

fn source(tx: &Connection, pid: i64) -> Result<Source> {
    let mut acc: Vec<u8> = vec![];
    let mut files = HashMap::new();
    let mut s = tx.prepare("SELECT path, hash FROM ts_file WHERE project_id = ?1 ORDER BY path")?;
    for r in s.query_map([pid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (path, hash) = r?;
        acc.extend_from_slice(path.as_bytes());
        acc.push(0);
        acc.extend_from_slice(hash.as_bytes());
        acc.push(0);
        files.insert(path, hash);
    }
    let prefix = format!("t:{pid}:");
    let mut syms = vec![];
    let mut s =
        tx.prepare("SELECT id, key, path FROM node WHERE project_id = ?1 AND kind = 'symbol' AND source = 'treesitter' ORDER BY id")?;
    for r in s.query_map([pid], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))? {
        let (id, key, path) = r?;
        let (Some(path), Some(rest)) = (path, key.strip_prefix(&prefix)) else { continue };
        let Some(i) = rest.rsplit_once('#').and_then(|(_, i)| i.parse::<usize>().ok()) else { continue };
        acc.extend_from_slice(&id.to_le_bytes());
        syms.push((id, path, i));
    }
    let mut docs = vec![];
    let mut s = tx.prepare(
        "SELECT id, name, COALESCE(path, '') FROM node WHERE project_id = ?1 AND kind IN ('doc_section', 'milestone') ORDER BY id",
    )?;
    for r in s.query_map([pid], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
        let (id, title, path) = r?;
        acc.extend_from_slice(&id.to_le_bytes());
        acc.extend_from_slice(title.as_bytes());
        acc.push(0);
        acc.extend_from_slice(path.as_bytes());
        acc.push(0);
        docs.push((id, title, path));
    }
    Ok(Source { files, syms, docs, fingerprint: hash64(&acc) })
}

/// A symbol to embed: (definition index in its file, node id, file version key).
type Def = (usize, i64, u64);

/// The version of `path` a symbol row came from.
fn file_key(path: &str, hash: &str) -> u64 {
    hash64(format!("{path}\0{hash}").as_bytes())
}

/// Bring project `p`'s vector index up to date with the store. When nothing it covers changed
/// (the usual index run), only its stamp moves. Otherwise rows of files whose content hash is
/// unchanged are kept, changed files are parsed again, and a text embedded before is reused by
/// its hash. `verify` reads and checks the whole old file first (`fox vectors build`), so a
/// damaged one is rebuilt; an index run reads only its header.
pub fn refresh(c: &Connection, p: &Project, verify: bool) -> Result<Stats> {
    let t0 = std::time::Instant::now();
    let mut st = Stats::default();
    // One read snapshot: the generation and the rows it covers.
    let tx = c.unchecked_transaction()?;
    let generation = db::project_generation(&tx, p.id);
    let src = source(&tx, p.id)?;
    trace("refresh: store read", t0);
    let head = if verify { Index::read(&index_path(p.id)).map(|ix| ix.head) } else { Index::read_header(&index_path(p.id)) };
    if let Ok(h) = head.as_ref()
        && h.model == MODEL.rev
        && h.project == p.id
        && h.fingerprint == src.fingerprint
    {
        let stamp = Stamp { generation, checksum: h.checksum };
        if read_stamp(p.id).as_ref() != Some(&stamp) {
            write_stamp(p.id, &stamp)?;
        }
        st.current = true;
        st.rows = h.count;
        st.ms = t0.elapsed().as_millis();
        return Ok(st);
    }
    let old = if head.is_ok() {
        Index::read(&index_path(p.id)).ok().filter(|ix| ix.head.model == MODEL.rev && ix.head.project == p.id)
    } else {
        None
    };
    trace("refresh: old index", t0);
    let old_row: HashMap<i64, usize> =
        old.as_ref().map(|ix| ix.ids.iter().enumerate().map(|(i, id)| (*id, i)).collect()).unwrap_or_default();
    let by_text: HashMap<u64, usize> =
        old.as_ref().map(|ix| ix.texts.iter().enumerate().map(|(i, h)| (*h, i)).collect()).unwrap_or_default();
    // Symbols kept as they were: the same node id from the same version of the same file.
    let mut keep: Vec<(i64, u64)> = vec![];
    let mut todo: HashMap<&str, Vec<Def>> = HashMap::new();
    for (id, path, i) in &src.syms {
        let fk = src.files.get(path).map_or(0, |h| file_key(path, h));
        match (old.as_ref(), old_row.get(id)) {
            (Some(ix), Some(&r)) if fk != 0 && ix.files[r] == fk => keep.push((*id, fk)),
            _ => todo.entry(path).or_default().push((*i, *id, fk)),
        }
    }
    st.parsed = todo.len();
    // Qualified names of the symbols to embed, from the same snapshot.
    let mut qual: HashMap<i64, String> = HashMap::new();
    {
        let mut s = tx.prepare_cached(
            "SELECT id, COALESCE(json_extract(meta, '$.parent') || '.' || name, name) FROM node
             WHERE project_id = ?1 AND path = ?2 AND kind = 'symbol' AND source = 'treesitter'",
        )?;
        for path in todo.keys() {
            for r in s.query_map(rusqlite::params![p.id, path], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
                let (id, q) = r?;
                qual.insert(id, q);
            }
        }
    }
    drop(tx);
    trace("refresh: plan", t0);
    // Parse changed files and build each symbol's text, as the body arm sees it.
    let todo: Vec<(&str, Vec<Def>)> = todo.into_iter().collect();
    let texts: Vec<(i64, u64, String)> = todo
        .par_iter()
        .flat_map_iter(|(path, defs)| {
            let bytes = std::fs::read(p.root.join(path)).unwrap_or_default();
            let parsed = crate::lang::for_path(path).map(|l| crate::tsindex::extract(l, &bytes));
            defs.iter()
                .map(|(i, id, fk)| {
                    let body =
                        parsed.as_ref().and_then(|x| x.defs.get(*i)).map(|d| crate::tsindex::body_text(&bytes, d)).unwrap_or_default();
                    let q = qual.get(id).map(String::as_str).unwrap_or("");
                    (*id, *fk, chunk_text(q, path, &body))
                })
                .collect::<Vec<_>>()
        })
        .chain(src.docs.par_iter().map(|(id, title, path)| (*id, 0, chunk_text(title, path, ""))))
        .collect();
    trace("refresh: parse", t0);
    // Embed texts not seen before (the model's full table is read only when there are some).
    let hashes: Vec<u64> = texts.iter().map(|t| hash64(t.2.as_bytes())).collect();
    let need: Vec<usize> = (0..texts.len()).filter(|&i| !by_text.contains_key(&hashes[i])).collect();
    st.embedded = need.len();
    let fresh: HashMap<usize, (f32, Vec<i8>)> = if need.is_empty() {
        HashMap::new()
    } else {
        let m = Model::open(&model_dir(), true)?;
        need.par_iter().map(|&i| m.embed_one(&texts[i].2).map(|v| (i, quantize(&v)))).collect::<Result<_>>()?
    };
    trace("refresh: embed", t0);
    let dims = old.as_ref().map(|ix| ix.head.dims).or_else(|| fresh.values().next().map(|x| x.1.len())).unwrap_or(0);
    // Assemble rows in node-id order: (id, text hash, file key, scale, vector).
    let mut rows: Vec<(i64, u64, u64, f32, &[i8])> = Vec::with_capacity(keep.len() + texts.len());
    if let Some(ix) = old.as_ref() {
        for (id, fk) in &keep {
            let r = old_row[id];
            rows.push((*id, ix.texts[r], *fk, ix.scales[r], &ix.vecs[r * dims..(r + 1) * dims]));
        }
    }
    for (i, (id, fk, _)) in texts.iter().enumerate() {
        match (fresh.get(&i), old.as_ref().and_then(|ix| by_text.get(&hashes[i]).map(|r| (ix, *r)))) {
            (Some((s, v)), _) => rows.push((*id, hashes[i], *fk, *s, v)),
            (None, Some((ix, r))) => rows.push((*id, hashes[i], *fk, ix.scales[r], &ix.vecs[r * dims..(r + 1) * dims])),
            (None, None) => bail!("internal: no vector for node {id}"),
        }
    }
    rows.sort_by_key(|r| r.0);
    rows.dedup_by_key(|r| r.0);
    let mut ix = Index {
        head: Header { model: MODEL.rev.into(), project: p.id, dims, count: rows.len(), fingerprint: src.fingerprint, checksum: 0 },
        ids: rows.iter().map(|r| r.0).collect(),
        texts: rows.iter().map(|r| r.1).collect(),
        files: rows.iter().map(|r| r.2).collect(),
        scales: rows.iter().map(|r| r.3).collect(),
        vecs: rows.iter().flat_map(|r| r.4.iter().copied()).collect(),
    };
    if ix.vecs.len() != ix.ids.len() * dims {
        bail!("internal: vector sizes differ");
    }
    st.rows = ix.len();
    std::fs::create_dir_all(index_dir())?;
    // The index first, then its stamp: a crash between the two leaves a stamp naming the old
    // file's checksum, so the new file is not read until the next refresh stamps it.
    ix.write(&index_path(p.id))?;
    write_stamp(p.id, &Stamp { generation, checksum: ix.head.checksum })?;
    trace("refresh: written", t0);
    st.ms = t0.elapsed().as_millis();
    Ok(st)
}

/// `fox vectors status`: the setting, the model and each project's index.
pub fn status(c: &Connection) -> Result<String> {
    let mut out = format!(
        "vector arm: {} (vectors = true in config.toml, or FOXDB_VECTORS=1)\nmodel: {} {}\n",
        if crate::config::vectors_on() { "on" } else { "off" },
        MODEL.name,
        if model_present() { format!("at {}", model_dir().display()) } else { "not set up (fox vectors setup)".into() }
    );
    let mut s = c.prepare("SELECT id, name FROM project ORDER BY name")?;
    let projects: Vec<(i64, String)> = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (pid, name) in projects {
        let path = index_path(pid);
        if !path.exists() {
            continue;
        }
        let line = match Index::read(&path) {
            Ok(ix) if ix.current(c, pid) => format!("{} rows, current", ix.len()),
            Ok(ix) => format!("{} rows, stale (unused until the next index run or fox vectors build)", ix.len()),
            Err(e) => format!("unreadable, unused until fox vectors build rebuilds it ({e:#})"),
        };
        out += &format!("  {name}: {line}\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_decodes() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0), 0.0);
        assert!((f16_to_f32(0x0001) - 5.96e-8).abs() < 1e-9);
    }

    #[test]
    fn quantized_rows_keep_their_order() {
        let (s, q) = quantize(&[0.5, -1.0, 0.25]);
        assert!((s * q[1] as f32 + 1.0).abs() < 1e-6);
        assert_eq!(quantize(&[0.0, 0.0]).0, 0.0);
    }

    fn index(rows: &[(i64, [f32; 3])]) -> Index {
        let mut ix = Index {
            head: Header { model: MODEL.rev.into(), project: 1, dims: 3, count: rows.len(), fingerprint: 7, checksum: 0 },
            ids: vec![],
            texts: vec![],
            files: vec![],
            scales: vec![],
            vecs: vec![],
        };
        for (id, v) in rows {
            let mut v = v.to_vec();
            normalize(&mut v);
            let (s, q) = quantize(&v);
            ix.ids.push(*id);
            ix.texts.push(*id as u64);
            ix.files.push(1);
            ix.scales.push(s);
            ix.vecs.extend(q);
        }
        ix
    }

    #[test]
    fn nearest_is_exact_and_deterministic() {
        let ix =
            index(&[(10, [1.0, 0.0, 0.0]), (11, [0.0, 1.0, 0.0]), (12, [0.9, 0.1, 0.0]), (13, [0.9, 0.1, 0.0]), (14, [-1.0, 0.0, 0.0])]);
        let got: Vec<i64> = ix.nearest(&[1.0, 0.0, 0.0], &Scope::default(), 3).unwrap().into_iter().map(|x| x.0).collect();
        assert_eq!(got, vec![10, 12, 13]);
        // Rows pointing away are never neighbours, whatever k.
        let all: Vec<i64> = ix.nearest(&[1.0, 0.0, 0.0], &Scope::default(), 10).unwrap().into_iter().map(|x| x.0).collect();
        assert!(!all.contains(&14));
        assert!(ix.nearest(&[1.0, 0.0], &Scope::default(), 3).unwrap().is_empty());
    }

    #[test]
    fn a_damaged_or_truncated_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p1.fv");
        let mut ix = index(&[(10, [1.0, 0.0, 0.0]), (11, [0.0, 1.0, 0.0])]);
        ix.write(&path).unwrap();
        let back = Index::read(&path).unwrap();
        assert_eq!((back.ids.clone(), back.head.fingerprint), (vec![10, 11], 7));
        assert_eq!(Index::read_header(&path).unwrap(), back.head);
        let mut b = std::fs::read(&path).unwrap();
        let last = b.len() - 1;
        b[last] ^= 0x40;
        std::fs::write(&path, &b).unwrap();
        assert!(Index::read(&path).err().unwrap().to_string().contains("checksum"));
        std::fs::write(&path, &b[..b.len() - 2]).unwrap();
        assert!(Index::read(&path).is_err());
        std::fs::write(&path, b"garbage").unwrap();
        assert!(Index::read(&path).is_err());
    }

    /// Parity with the reference (Hugging Face) tokenizer: `FOXDB_TOKENIZER_PARITY=DIR` holding
    /// the model's `tokenizer.json` and `cases.json` (`[{text, ids}]` from the reference with no
    /// special tokens). Reports texts whose ids differ; ASCII ones must all match.
    #[test]
    #[ignore]
    fn tokenizer_matches_the_reference() {
        let dir = PathBuf::from(std::env::var("FOXDB_TOKENIZER_PARITY").expect("FOXDB_TOKENIZER_PARITY"));
        let wp = WordPiece::open(&dir.join("tokenizer.json")).unwrap();
        #[derive(serde::Deserialize)]
        struct Case {
            text: String,
            ids: Vec<u32>,
        }
        let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(dir.join("cases.json")).unwrap()).unwrap();
        let bad: Vec<&Case> = cases.iter().filter(|c| wp.encode(&c.text, usize::MAX) != c.ids).collect();
        for c in &bad {
            eprintln!("differs: {:?}", c.text.chars().take(80).collect::<String>());
        }
        eprintln!("{} of {} texts match", cases.len() - bad.len(), cases.len());
        assert!(bad.iter().all(|c| !c.text.is_ascii()));
    }

    #[test]
    fn wordpiece_splits_like_bert() {
        let dir = tempfile::tempdir().unwrap();
        let vocab = serde_json::json!({"[UNK]": 0, "replica": 1, "set": 2, "##s": 3, "_": 4, "cafe": 5, "un": 6, "##known": 7});
        let tok = serde_json::json!({
            "normalizer": {"type": "BertNormalizer", "lowercase": true, "strip_accents": null},
            "pre_tokenizer": {"type": "BertPreTokenizer"},
            "model": {"type": "WordPiece", "unk_token": "[UNK]", "continuing_subword_prefix": "##", "max_input_chars_per_word": 100, "vocab": vocab}
        });
        std::fs::write(dir.path().join("t.json"), tok.to_string()).unwrap();
        let wp = WordPiece::open(&dir.path().join("t.json")).unwrap();
        assert_eq!(wp.encode("Replicas replica_set", 99), vec![1, 3, 1, 4, 2]);
        assert_eq!(wp.encode("CAFÉ unknown zzz", 99), vec![5, 6, 7, 0]);
        assert_eq!(wp.encode("replica replica replica", 2), vec![1, 1]);
    }

    #[test]
    fn hash_differs_by_content_and_length() {
        assert_ne!(hash64(b"abc"), hash64(b"abd"));
        assert_ne!(hash64(b""), hash64(b"\0"));
        assert_eq!(hash64(b"same text"), hash64(b"same text"));
    }

    #[test]
    fn the_arm_is_empty_without_an_index() {
        let c = Connection::open_in_memory().unwrap();
        // No index file for this project id under any FOXDB_HOME: nothing, and no error.
        assert!(arm(&c, i64::MAX, "how are replicas scaled", 10).unwrap().is_empty());
    }
}
