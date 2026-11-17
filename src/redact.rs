//! Best-effort secret redaction for text foxdb stores on purpose: memory notes, handoffs, subagent
//! reports and session digests.
use regex::Regex;
use std::sync::OnceLock;

fn patterns() -> &'static [Regex] {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        [
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(-----END [A-Z ]*PRIVATE KEY-----|$)",
            r"AKIA[0-9A-Z]{16}",
            r"\bsk-[A-Za-z0-9_\-]{16,}",
            r"\b[sr]k_(live|test)_[A-Za-z0-9]{16,}",
            r"\bglpat-[A-Za-z0-9_\-]{20,}",
            r"\bgh[pousr]_[A-Za-z0-9]{20,}",
            r"\bxox[abprs]-[A-Za-z0-9\-]{10,}",
            r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
            r"(?i)\bbearer\s+[A-Za-z0-9._\-]{16,}",
            // key = value, key: value, "key": "value". The key may carry a prefix or suffix
            // (DB_PASSWORD, OPENAI_API_KEY, SECRET_KEY, client_secret) or be camelCase (apiKey).
            r#"(?i)\b(?:[a-z0-9]+[_-])*(pass(word|wd)?|secret|token|api[_-]?key|auth[_-]?key)(?:[_-][a-z0-9]+)*\b["']?\s*[:=]\s*["']?[^\s"']{6,}"#,
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    })
}

/// Long base64/url-safe runs: tokens and keys, unless the run reads as a name (see [`is_name`]).
fn long_run() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\b[A-Za-z0-9+/_\-]{40,}={0,2}").unwrap())
}

/// A git object id or content hash: 40 (SHA-1) or 64 (SHA-256) hex digits.
fn is_hash(seg: &str) -> bool {
    matches!(seg.len(), 40 | 64) && seg.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Split a run into word chunks: letters split at case starts (`PlayerV2View` gives `Player`,
/// `V`, `View`; `HTTPServer` gives `HTTP`, `Server`), digit runs apart, separators dropped.
fn chunks(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut start: Option<usize> = None;
    for i in 0..b.len() {
        let c = b[i];
        if !c.is_ascii_alphanumeric() {
            if let Some(st) = start.take() {
                out.push(&s[st..i]);
            }
            continue;
        }
        let Some(st) = start else {
            start = Some(i);
            continue;
        };
        let p = b[i - 1];
        let split = p.is_ascii_digit() != c.is_ascii_digit()
            || (p.is_ascii_lowercase() && c.is_ascii_uppercase())
            // An upper run then a lower letter: the last capital starts the next word.
            || (p.is_ascii_uppercase() && c.is_ascii_lowercase() && i - st > 1 && b[i - 2].is_ascii_uppercase());
        if split {
            let at = if p.is_ascii_uppercase() && c.is_ascii_lowercase() { i - 1 } else { i };
            out.push(&s[st..at]);
            start = Some(at);
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

/// Does a chunk read as a word: letters only, 3 to 24 long, with vowels in a natural share and no
/// run of more than four consonants. Random letters rarely manage all of that.
fn wordy(ch: &str) -> bool {
    if !(3..=24).contains(&ch.len()) || !ch.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    let vowel = |b: u8| b"aeiouyAEIOUY".contains(&b);
    let v = ch.bytes().filter(|&b| vowel(b)).count();
    let (mut run, mut most) = (0, 0);
    for b in ch.bytes() {
        run = if vowel(b) { 0 } else { run + 1 };
        most = most.max(run);
    }
    v > 0 && v * 5 >= ch.len() && most <= 4
}

/// The share of a run's characters outside word chunks (digits, stray letters, unpronounceable
/// letter runs). An all-capitals chunk longer than an acronym counts against the run when the
/// run also has lowercase letters (`MXIPNX` in a token; `SNAPSHOT_SECS` alone is a constant).
fn rough(s: &str) -> f64 {
    let lower = s.bytes().any(|b| b.is_ascii_lowercase());
    let (mut total, mut odd) = (0usize, 0usize);
    for ch in chunks(s) {
        total += ch.len();
        let caps = ch.bytes().all(|b| b.is_ascii_uppercase());
        if !wordy(ch) || (caps && lower && ch.len() > 5) {
            odd += ch.len();
        }
    }
    if total == 0 { 0.0 } else { odd as f64 / total as f64 }
}

/// Runs at least this rough are tokens.
const ROUGH: f64 = 0.4;

/// Does a separator-free segment switch character class like base64? Words switch at word starts
/// (`PlayerV2ViewModel`: a few times); a token switches on most characters. Upper-to-lower is a
/// word's own capital, so it does not count.
fn switches_often(seg: &str) -> bool {
    if seg.len() < 16 || is_hash(seg) {
        return false;
    }
    let class = |c: u8| {
        if c.is_ascii_digit() {
            0
        } else if c.is_ascii_uppercase() {
            1
        } else {
            2
        }
    };
    let b = seg.as_bytes();
    let switches = b.windows(2).filter(|w| class(w[0]) != class(w[1]) && !(class(w[0]) == 1 && class(w[1]) == 2)).count();
    switches * 3 >= seg.len()
}

/// A path, identifier, commit id or hash rather than a token.
///
/// - A run with `/` is a path when no component is a mixed-case random string (base64 is
///   mixed-case; hashes in paths are single-case: hex, Nix base32, `agent-<hex>`) and at least two
///   components read as words.
/// - Otherwise the run is a name when it is a bare SHA-1/SHA-256, letters and digits do not
///   alternate, no `_`/`-` segment switches class like base64, and it is mostly word chunks.
/// - `+` and trailing `=` are base64 only.
fn is_name(m: &str) -> bool {
    if m.contains('+') || m.ends_with('=') {
        return false;
    }
    if m.contains('/') {
        let parts: Vec<&str> = m.split('/').filter(|p| !p.is_empty()).collect();
        let mixed_random = |p: &&str| {
            let mixed = p.bytes().any(|b| b.is_ascii_uppercase()) && p.bytes().any(|b| b.is_ascii_lowercase());
            mixed && p.len() >= 8 && (rough(p) >= ROUGH || p.split(['_', '-']).any(switches_often))
        };
        return !parts.iter().any(mixed_random) && parts.iter().filter(|p| rough(p) < ROUGH).count() >= 2;
    }
    if is_hash(m) {
        return true;
    }
    let bare: Vec<u8> = m.bytes().filter(|b| b.is_ascii_alphanumeric()).collect();
    let flips = bare.windows(2).filter(|w| w[0].is_ascii_digit() != w[1].is_ascii_digit()).count();
    flips * 4 < bare.len() && !m.split(['_', '-']).any(switches_often) && rough(m) < ROUGH
}

pub fn redact(text: &str) -> String {
    let mut t = text.to_string();
    for re in patterns() {
        t = re.replace_all(&t, "[redacted]").into_owned();
    }
    long_run().replace_all(&t, |c: &regex::Captures| if is_name(&c[0]) { c[0].to_string() } else { "[redacted]".to_string() }).into_owned()
}

#[cfg(test)]
mod tests {
    use super::redact;
    #[test]
    fn seeded_secrets_are_removed() {
        for s in [
            "key AKIAIOSFODNN7EXAMPLE end",
            "token: sk-abcdefghijklmnopqrstuvwx",
            "password = hunter2hunter2",
            "Authorization: Bearer abcdefghijklmnop1234",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop",
            "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkq\n-----END PRIVATE KEY-----",
        ] {
            let r = redact(s);
            assert!(r.contains("[redacted]"), "{s} -> {r}");
            assert!(!r.contains("AKIAIOSFODNN7EXAMPLE") && !r.contains("hunter2") && !r.contains("MIIEvQ"), "{r}");
        }
        assert_eq!(redact("plain note about OrderQueueController"), "plain note about OrderQueueController");
    }

    /// The forms transcripts actually carry: prefixed env-var names and JSON-quoted keys.
    #[test]
    fn prefixed_env_names_and_json_keys_are_redacted() {
        for (s, secret) in [
            ("DB_PASSWORD=hunter2hunter2", "hunter2"),
            ("OPENAI_API_KEY=abc123def456xyz", "abc123def456xyz"),
            (r#"{"password": "hunter2hunter2"}"#, "hunter2"),
            (r#"{"apiKey":"abc123def456xyz"}"#, "abc123def456xyz"),
            ("GITHUB_TOKEN=abcdef123456", "abcdef123456"),
            ("export SECRET_KEY='qwertyuiop12'", "qwertyuiop12"),
            ("client_secret: zzyyxxwwvv99", "zzyyxxwwvv99"),
        ] {
            let r = redact(s);
            assert!(r.contains("[redacted]") && !r.contains(secret), "{s} -> {r}");
        }
        // Words that merely contain a keyword are left alone.
        for s in ["tokenizer = unicode61", "the password reset flow", "passwordless login works"] {
            assert_eq!(redact(s), s);
        }
    }

    /// Reports cite path:line; long paths are not tokens.
    #[test]
    fn paths_survive_and_slashed_tokens_do_not() {
        for s in [
            "changed /srv/build/projects/example-app/worktrees/feature-branch/app/src/main/Foo.kt:12",
            "and src/main/java/com/example/app/feature/player/PlayerViewModel.kt:40",
            "@/var/tmp/someone/.foxdb-scratch/a-very-long-home-directory/agents/cx-1/out.jsonl",
        ] {
            assert_eq!(redact(s), s);
        }
        for s in ["key Zm9vYmFyYmF6cXV4MTIz/NDU2Nzg5MGFiY2RlZmdoaWprbG1ub3A=", "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bC1dF3hJ5kL7nP9rS1"] {
            assert!(redact(s).contains("[redacted]"), "{s} -> {}", redact(s));
        }
    }

    /// Commit ids, content hashes, long paths and long identifiers are the evidence a handoff
    /// carries; none of them is a secret.
    #[test]
    fn shas_long_paths_and_identifiers_survive() {
        for s in [
            "base 3cc4fd6a9b0e1d2c3f4a5b6c7d8e9f0a1b2c3d4e head 9959f44e0d1c2b3a4f5e6d7c8b9a0f1e2d3c4b5a",
            "sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "git diff 3cc4fd6a9b0e1d2c3f4a5b6c7d8e9f0a1b2c3d4e..9959f44e0d1c2b3a4f5e6d7c8b9a0f1e2d3c4b5a",
            "app/src/main/java/com/example/feature/player/ui/PlayerV2ViewModelTest",
            "core/src/test/kotlin/example/playback/Mp4ExtractorH264ParserRegressionSuite",
            "the test dispatch_report_wait_round_trip_for_both_harnesses_and_more failed",
            "OrderQueueControllerIntegrationTestsWithLongName is flaky",
            "see src/orchestration/supervisor/worker_report_schema_validation_v2.rs",
        ] {
            assert_eq!(redact(s), s);
        }
        // Real secrets of the same shapes are still caught.
        for s in ["password = 3cc4fd6a9b0e1d2c3f4a5b6c7d8e9f0a1b2c3d4e", "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bC1dF3hJ5kL7nP9rS1x"] {
            assert!(redact(s).contains("[redacted]"), "{s} -> {}", redact(s));
        }
    }

    /// Cache, store and transcript paths carry one hash-like component; the path is not a token.
    #[test]
    fn paths_with_one_hash_component_survive() {
        for s in [
            "/srv/u/.gradle/caches/transforms-3/0a1b2c3d4e5f60718293a4b5c6d7e8f9/transformed/core-1.12.0/jars/classes.jar",
            "/nix/store/0c7zrbqh2hsx6yva1ymkbz4m7l5k4xd2-openssl-3.0.13/lib/libssl.so",
            "/srv/u/.claude/projects/-srv-u-repo/5e55aa00-1111-2222-3333-444455556666/subagents/agent-a3f9c2e1b7d4058e6.jsonl",
            "repo/.git/objects/3c/abababababababababababababababababab01",
        ] {
            assert_eq!(redact(s), s);
        }
    }

    /// Tiny deterministic generator, so the leak counts below are exact and repeatable.
    fn tokens(alphabet: &[u8], len: usize, n: usize, seed: u64) -> Vec<String> {
        let mut x = seed;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut out = vec![];
        while out.len() < n {
            let t: String = (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize] as char).collect();
            // A run must start and end on a word character to be one token in running text.
            if t.as_bytes()[0].is_ascii_alphanumeric() && t.as_bytes()[len - 1].is_ascii_alphanumeric() {
                out.push(t);
            }
        }
        out
    }

    /// Random secrets with no keyword nearby are redacted whatever their alphabet: base64 with
    /// `/`, url-safe base64 with `_` and `-`, lowercase, lowercase with digits, long hex. Bare
    /// 40 and 64 hex digits are commit ids and content hashes and stay (a deliberate trade-off).
    #[test]
    fn random_tokens_without_a_keyword_are_redacted() {
        for s in [
            "99nKSNrh9UCauSDmLhuVtcqcYezdZ/tDDj8hYs5s",
            "Y_1Kgd2vd_Er1uyZAlIa_ZnYd7chlN_Xc-1HSyGbDS1",
            "sk_live_51HqLyjWDarjtT1zdp7dcXAMPLEkeyZZZZ",
            "glpat-xxxxxxxxxxxxxxxxxxxx1234567890abcdefgh",
        ] {
            let r = redact(&format!("x {s} y"));
            assert!(r.contains("[redacted]") && !r.contains(s), "{s} -> {r}");
        }
        let lower = b"abcdefghijklmnopqrstuvwxyz";
        let lower_digit = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let b64 = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let url = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
        let mut leaks = vec![];
        for (name, alphabet, len) in [
            ("base64-40", &b64[..], 40),
            ("url-43", &url[..], 43),
            ("url-64", &url[..], 64),
            ("lower-40", &lower[..], 40),
            ("lower-digit-40", &lower_digit[..], 40),
            ("lower-digit-48", &lower_digit[..], 48),
            ("hex-48", &b"0123456789abcdef"[..], 48),
        ] {
            let n = tokens(alphabet, len, 500, 0x9e3779b97f4a7c15 ^ len as u64)
                .iter()
                .filter(|t| !redact(&format!("x {t} y")).contains("[redacted]"))
                .count();
            // At most 1% of any shape may slip through.
            if n > 5 {
                leaks.push(format!("{name}: {n}/500 unredacted"));
            }
        }
        assert!(leaks.is_empty(), "{}", leaks.join("\n"));
    }
}
