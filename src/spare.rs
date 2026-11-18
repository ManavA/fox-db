//! Token sparing for foxdb's own answers (not a general output compressor):
//!
//! - **Tiered rows**: each row has a full, a skeleton and an id form. If the whole page fits as
//!   full rows it is sent as is. Otherwise the page shrinks to the top few rows, in the richest
//!   tier those fit, and the footer gives the cursor for
//!   the rest and says how to expand. A tight budget never trades the best rows' detail for a
//!   long tail of low-ranked ids.
//! - **Already sent**: a large answer that this session already received comes back as a short
//!   reference with its handle, so a re-read costs a line. `fresh=true` (MCP) or
//!   `FOXDB_FRESH=1` (CLI) always returns the full text, for agents that never saw the first copy.

use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::fmt::Write;

pub struct Row {
    pub full: String,
    pub skeleton: String,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Full,
    Skeleton,
    Ids,
}

fn fits(rows: &[&String], max: usize) -> bool {
    rows.iter().map(|r| r.len() + 1).sum::<usize>() <= max
}

/// Rows kept on a page that does not fit whole as full rows.
pub const TOP_ROWS: usize = 5;

/// Render `rows[offset..offset+max_rows]` within `budget_tokens` (4 bytes per token): all full
/// rows if they fit, else the top [`TOP_ROWS`] in the richest tier they fit, else as many ids as
/// fit. Hidden rows are counted in a cursor footer.
pub fn tiered(rows: &[Row], offset: usize, budget_tokens: usize, max_rows: usize) -> (String, Tier) {
    let max = budget_tokens.clamp(32, 4000) * 4;
    let window: Vec<&Row> = rows.iter().skip(offset).take(max_rows).collect();
    let after = rows.len().saturating_sub(offset + window.len());
    // Leave room for the footer line.
    let room = max.saturating_sub(64);
    let full: Vec<&String> = window.iter().map(|r| &r.full).collect();
    let whole = fits(&full, room);
    let top = if whole { window.len() } else { window.len().min(TOP_ROWS) };
    let (tier, pick): (Tier, fn(&Row) -> &String) = {
        let full: Vec<&String> = window.iter().take(top).map(|r| &r.full).collect();
        let skel: Vec<&String> = window.iter().take(top).map(|r| &r.skeleton).collect();
        if fits(&full, room) {
            (Tier::Full, |r| &r.full)
        } else if fits(&skel, room) {
            (Tier::Skeleton, |r| &r.skeleton)
        } else {
            (Tier::Ids, |r| &r.id)
        }
    };
    let mut out = String::new();
    let mut shown = 0;
    for r in window.iter().take(top) {
        let l = pick(r);
        if shown > 0 && out.len() + l.len() + 1 > room {
            break;
        }
        out.push_str(l);
        out.push('\n');
        shown += 1;
    }
    let hidden = window.len() - shown + after;
    let next = offset + shown;
    match (tier, hidden) {
        (Tier::Full, 0) => {}
        (Tier::Full, n) => {
            let _ = writeln!(out, "+{n} more \u{b7} cursor={next}");
        }
        (t, n) => {
            let what = if t == Tier::Skeleton { "skeleton rows" } else { "ids only" };
            let more = if n > 0 { format!("+{n} more \u{b7} cursor={next} \u{b7} ") } else { String::new() };
            let _ = writeln!(out, "{more}{what} to fit the budget; fox_read <handle> expands one, or raise budget");
        }
    }
    (out, tier)
}

/// Stable 64-bit FNV-1a of `s`, hex. Used for "already sent" keys, not for security.
pub fn hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Answers shorter than this are cheaper to re-send than to reference.
pub const DEDUP_MIN_BYTES: usize = 600;

fn reference(handle: &str, h: &str, bytes: usize) -> String {
    format!(
        "= already sent in this session: {handle} (unchanged, #{}, ~{} tokens). Pass fresh=true if it is not in your context.\n",
        &h[..8],
        bytes.div_ceil(4)
    )
}

/// In-memory seen-set for one MCP server process (one harness session).
#[derive(Default)]
pub struct Seen(HashSet<String>);

impl Seen {
    /// `text`, or a one-line reference if this exact text was already returned.
    pub fn filter(&mut self, handle: &str, text: String, fresh: bool) -> String {
        if text.len() < DEDUP_MIN_BYTES {
            return text;
        }
        let h = hash(&text);
        if !self.0.insert(h.clone()) && !fresh {
            return reference(handle, &h, text.len());
        }
        text
    }
}

/// The CLI's seen-set, persisted per client session id (`FOXDB_SESSION`, sent through foxd). No
/// session, no dedup.
pub fn filter_db(w: &Connection, session: Option<&str>, handle: &str, text: String, fresh: bool) -> String {
    let Some(session) = session.filter(|s| !s.is_empty()) else { return text };
    if text.len() < DEDUP_MIN_BYTES {
        return text;
    }
    let h = hash(&text);
    let inserted = w
        .execute(
            "INSERT OR IGNORE INTO seen(session, hash, handle, at) VALUES (?1, ?2, ?3, ?4)",
            params![session, h, handle, crate::db::now()],
        )
        .unwrap_or(1);
    if inserted == 0 && !fresh { reference(handle, &h, text.len()) } else { text }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| Row {
                full: format!("s:{i:x} function some_long_function_name_{i}(arg: Type) -> Result  src/file_{i}.rs:{i}  refs:{i}"),
                skeleton: format!("s:{i:x} function some_long_function_name_{i}  src/file_{i}.rs:{i}"),
                id: format!("s:{i:x} src/file_{i}.rs:{i}"),
            })
            .collect()
    }

    fn shown(out: &str) -> Vec<&str> {
        out.lines().filter(|l| l.starts_with("s:")).collect()
    }

    #[test]
    fn whole_page_as_full_rows_when_it_fits() {
        let r = rows(10);
        let (out, t) = tiered(&r, 0, 1000, 10);
        assert_eq!(t, Tier::Full);
        assert_eq!(out.lines().count(), 10, "{out}");
    }

    /// A page that does not fit whole keeps the top rows' detail and pages the tail, rather than
    /// shrinking every row to an id and filling the budget with low-ranked ones.
    #[test]
    fn tight_budget_keeps_top_rows_full_and_pages_the_tail() {
        let r = rows(20);
        let (out, t) = tiered(&r, 0, 190, 20);
        assert_eq!(t, Tier::Full, "{out}");
        assert_eq!(shown(&out), r.iter().take(TOP_ROWS).map(|x| x.full.as_str()).collect::<Vec<_>>(), "{out}");
        assert!(out.ends_with(&format!("+15 more \u{b7} cursor={TOP_ROWS}\n")), "{out}");
        // The cursor continues where the page stopped.
        let (next, _) = tiered(&r, TOP_ROWS, 190, 20);
        assert_eq!(shown(&next)[0], r[TOP_ROWS].full, "{next}");
    }

    #[test]
    fn shorter_tiers_only_when_the_top_rows_do_not_fit_full() {
        let r = rows(20);
        let (out, t) = tiered(&r, 0, 100, 20);
        assert_eq!(t, Tier::Skeleton, "{out}");
        assert_eq!(shown(&out).len(), TOP_ROWS, "{out}");
        assert!(out.lines().last().unwrap().contains("skeleton rows"), "{out}");
        let (out, t) = tiered(&r, 0, 50, 20);
        assert_eq!(t, Tier::Ids, "{out}");
        assert!(out.len() <= 50 * 4, "{}", out.len());
        let (out, _) = tiered(&r, 0, 32, 20);
        assert!(out.contains("cursor="), "{out}");
    }

    #[test]
    fn seen_set_turns_repeats_into_references() {
        let mut s = Seen::default();
        let big = "x".repeat(DEDUP_MIN_BYTES + 10);
        assert_eq!(s.filter("s:1", big.clone(), false), big);
        let again = s.filter("s:1", big.clone(), false);
        assert!(again.starts_with("= already sent") && again.contains("s:1") && again.len() < 200, "{again}");
        assert_eq!(s.filter("s:1", big.clone(), true), big);
        assert_eq!(s.filter("s:2", "small".into(), false), "small");
        assert_eq!(s.filter("s:2", "small".into(), false), "small");
    }
}
