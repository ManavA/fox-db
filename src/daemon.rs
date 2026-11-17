//! `foxd`: a long-lived per-user daemon that answers the CLI's read verbs (`find`, `code`) over a
//! unix socket at `$FOXDB_HOME/foxd.sock`. It keeps one read connection with its prepared
//! statements, one write connection for the savings ledger, and the warm search state (name index,
//! CSR call graph, result cache), all refreshed when the index generation moves.
//!
//! Protocol: one JSON request line, one JSON reply line per connection.
//! Request `{"argv": [...], "cwd": "/abs", "session": "id" | null}`; reply `{"code": 0, "out": "..."}`, or
//! `{"code": 1, "err": "..."}`, or `{"code": 2}` for a verb the daemon does not serve.
//! `{"ping": true}` and `{"shutdown": true}` are control requests.
//!
//! The CLI tries the socket first and falls back to opening the store directly, so a missing,
//! stale or busy daemon only costs a failed connect.
//!
//! When no request has arrived for [`IDLE_SECS`], the daemon merges the full-text indexes a
//! bounded step at a time (writes never merge inline; see `db::fts_merge`), stopping as soon as
//! a request comes in or nothing is left to merge.
//!
//! In builds with the `tantivy` feature it is also the only writer of the derived tantivy
//! indexes of big projects: once a second it applies their change log (see `tantivy_index`).
//! A store with no such index has no `tantivy` directory, and that check is all it costs.

use crate::search::Warm;
use crate::{cli, db};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

/// `$FOXDB_HOME/foxd.sock`, or, when that path is too long for a unix socket address (108
/// bytes), a per-home name in `$XDG_RUNTIME_DIR` (else `/tmp`).
pub fn socket_path() -> PathBuf {
    let home = db::home();
    let p = home.join("foxd.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in home.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join(format!("foxd-{h:016x}.sock"))
}

fn roundtrip(req: &Value, timeout: Duration) -> Option<Value> {
    let mut s = UnixStream::connect(socket_path()).ok()?;
    s.set_read_timeout(Some(timeout)).ok()?;
    s.set_write_timeout(Some(timeout)).ok()?;
    writeln!(s, "{req}").ok()?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// True when a daemon answers on the socket.
pub fn ping() -> bool {
    roundtrip(&json!({"ping": true}), Duration::from_millis(200)).is_some_and(|v| v["pong"] == true)
}

/// Ask a running daemon to exit.
pub fn shutdown() {
    let _ = roundtrip(&json!({"shutdown": true}), Duration::from_millis(500));
}

pub struct Reply {
    pub code: i32,
    pub text: String,
}

/// Verbs the daemon serves.
pub fn serves(verb: &str) -> bool {
    matches!(verb, "find" | "code")
}

/// Run `argv` in the daemon on behalf of this process's session (`FOXDB_SESSION`). None means
/// "do it directly": no daemon, an unsupported verb, or no answer in time.
pub fn call(argv: &[String], cwd: &Path) -> Option<Reply> {
    let session = std::env::var("FOXDB_SESSION").ok().filter(|s| !s.is_empty());
    call_with(argv, cwd, session.as_deref())
}

/// [`call`] with an explicit client session. The daemon never reads its own environment for it.
pub fn call_with(argv: &[String], cwd: &Path, session: Option<&str>) -> Option<Reply> {
    if !argv.first().is_some_and(|v| serves(v)) || std::env::var_os("FOXDB_NO_DAEMON").is_some() || !socket_path().exists() {
        return None;
    }
    let v = roundtrip(&json!({"argv": argv, "cwd": cwd, "session": session}), Duration::from_secs(5))?;
    match v["code"].as_i64()? {
        0 => Some(Reply { code: 0, text: v["out"].as_str().unwrap_or_default().to_string() }),
        1 => Some(Reply { code: 1, text: v["err"].as_str().unwrap_or_default().to_string() }),
        _ => None,
    }
}

struct State {
    ro: Option<Connection>,
    rw: Option<Connection>,
    warm: Warm,
}

fn answer(state: &Mutex<State>, req: &Value) -> Value {
    let argv: Vec<String> =
        req["argv"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let cwd = PathBuf::from(req["cwd"].as_str().unwrap_or("."));
    let Some(verb) = argv.first().cloned().filter(|v| serves(v)) else { return json!({"code": 2}) };
    let Ok(mut a) = cli::parse(argv) else { return json!({"code": 2}) };
    // The client's session, not whatever the daemon was started with.
    a.session = req["session"].as_str().filter(|s| !s.is_empty()).map(String::from);
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    if st.ro.is_none() {
        st.ro = db::open_ro();
    }
    if st.rw.is_none() {
        st.rw = db::open_rw().ok();
    }
    let State { ro, rw, warm } = &mut *st;
    let Some(ro) = ro.as_ref() else { return json!({"code": 2}) };
    let served = cli::Served { ro, rw: rw.as_ref(), warm };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match verb.as_str() {
        "find" => cli::find(&a, &cwd, Some(served)),
        _ => cli::code(&a, &cwd, Some(served)),
    }));
    match r {
        Ok(Ok(out)) => json!({"code": 0, "out": out}),
        Ok(Err(e)) => json!({"code": 1, "err": format!("{e:#}")}),
        Err(_) => json!({"code": 2}),
    }
}

/// Seconds without a request before the daemon merges full-text segments
/// (`FOXDB_IDLE_SECS` overrides it).
pub const IDLE_SECS: i64 = 20;

fn idle_secs() -> i64 {
    std::env::var("FOXDB_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(IDLE_SECS)
}

/// Pages one merge step may write per table: small enough to yield to a request quickly.
const MERGE_PAGES: i64 = 256;

/// Merge full-text segments while the daemon is idle; once settled, wait for the next write.
fn idle_merge(stop: &AtomicBool, last: &AtomicI64) {
    let mut settled_at: Option<i64> = None;
    // Writers of the derived (tantivy) indexes: none, and no work, in a store without any.
    #[cfg(feature = "tantivy")]
    let mut writers = crate::tantivy_index::Writers::default();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(1));
        #[cfg(feature = "tantivy")]
        if crate::tantivy_index::root().is_dir()
            && let Ok(c) = db::open_rw()
            && let Err(e) = writers.drain(&c)
            && std::env::var_os("FOXDB_TRACE").is_some()
        {
            eprintln!("foxd: tantivy: {e:#}");
        }
        if db::now() - last.load(Ordering::SeqCst) < idle_secs() {
            continue;
        }
        let Ok(c) = db::open_rw() else { continue };
        let generation = db::generation(&c);
        if settled_at == Some(generation) {
            continue;
        }
        while !stop.load(Ordering::SeqCst) && db::now() - last.load(Ordering::SeqCst) >= idle_secs() {
            match db::fts_merge(&c, MERGE_PAGES) {
                Ok(true) => continue,
                Ok(false) => {
                    settled_at = Some(generation);
                    break;
                }
                Err(_) => break,
            }
        }
    }
}

/// Serve until a `shutdown` request. Refuses to start if another daemon answers on the socket;
/// removes a stale socket left by one that died.
pub fn serve() -> Result<()> {
    let path = socket_path();
    if path.exists() {
        if ping() {
            anyhow::bail!("foxd is already running at {}", path.display());
        }
        let _ = std::fs::remove_file(&path);
    }
    std::fs::create_dir_all(db::home())?;
    let listener = UnixListener::bind(&path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    let state = Mutex::new(State { ro: None, rw: None, warm: Warm::default() });
    let stop = AtomicBool::new(false);
    let last = AtomicI64::new(db::now());
    std::thread::scope(|s| {
        {
            let (stop, last) = (&stop, &last);
            s.spawn(move || idle_merge(stop, last));
        }
        for conn in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(mut conn) = conn else { continue };
            let (state, stop, path, last) = (&state, &stop, &path, &last);
            s.spawn(move || {
                let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
                let mut line = String::new();
                if BufReader::new(&conn).read_line(&mut line).is_err() {
                    return;
                }
                let Ok(req) = serde_json::from_str::<Value>(&line) else { return };
                last.store(db::now(), Ordering::SeqCst);
                let reply = if req["ping"] == true {
                    json!({"pong": true, "pid": std::process::id()})
                } else if req["shutdown"] == true {
                    stop.store(true, Ordering::SeqCst);
                    // Wake the accept loop so it sees the flag.
                    let _ = UnixStream::connect(path);
                    json!({"bye": true})
                } else {
                    answer(state, &req)
                };
                let _ = writeln!(conn, "{reply}");
            });
        }
    });
    let _ = std::fs::remove_file(&path);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn socket_path_fits_a_sockaddr_even_for_deep_homes() {
        let deep = format!("/tmp/{}", "d".repeat(150));
        // SAFETY: single-threaded unit test; restores nothing because nothing else reads it here.
        unsafe { std::env::set_var("FOXDB_HOME", &deep) };
        let p = super::socket_path();
        assert!(p.as_os_str().len() < 108, "{}", p.display());
        unsafe { std::env::set_var("FOXDB_HOME", "/tmp/fx") };
        assert_eq!(super::socket_path(), std::path::PathBuf::from("/tmp/fx/foxd.sock"));
    }
}
