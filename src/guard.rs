//! The only place foxdb opens a project file for its contents.
//! Re-checks the deny list, so a classification bug upstream still cannot read a secret.

use crate::tiers;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static TRACE: AtomicBool = AtomicBool::new(false);
static OPENED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Test hook: record every relative path opened through the guard.
pub fn trace_opens(on: bool) {
    TRACE.store(on, Ordering::SeqCst);
    OPENED.lock().unwrap().clear();
}
pub fn opened() -> Vec<String> {
    OPENED.lock().unwrap().clone()
}

/// Read up to `cap` bytes of a text file. For `head_tail`, read the first and last `cap/2` bytes.
/// Returns None for denied paths, unreadable or non-UTF-8-ish content.
pub fn read_text(abs: &Path, rel: &str, cap: usize, head_tail: bool) -> Option<String> {
    if tiers::is_denied_path(rel) {
        return None;
    }
    if TRACE.load(Ordering::SeqCst) {
        OPENED.lock().unwrap().push(rel.to_string());
    }
    let mut f = std::fs::File::open(abs).ok()?;
    let len = f.metadata().ok()?.len() as usize;
    let mut buf = Vec::with_capacity(len.min(cap));
    if head_tail && len > cap {
        let half = cap / 2;
        let mut head = vec![0; half];
        f.read_exact(&mut head).ok()?;
        f.seek(SeekFrom::End(-(half as i64))).ok()?;
        let mut tail = Vec::with_capacity(half);
        f.read_to_end(&mut tail).ok()?;
        buf.extend_from_slice(&head);
        buf.extend_from_slice(b"\n...\n");
        buf.extend_from_slice(&tail);
    } else {
        f.take(cap as u64).read_to_end(&mut buf).ok()?;
    }
    if buf.iter().take(8192).any(|&b| b == 0) {
        return None; // binary
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}
