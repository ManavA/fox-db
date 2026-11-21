//! What the agent registry needs to know about a process, portably: whether it exists (and is
//! not a zombie), its command name and parent, and a start-time token that tells a reused pid
//! apart. Linux reads `/proc/<pid>/stat`; macOS asks the kernel (`proc_pidinfo`); other Unixes
//! only learn existence, from `kill(pid, 0)`.

/// One process, as far as the platform tells.
#[derive(Debug, Clone)]
pub struct Info {
    /// Command name, as the kernel truncates it (15 or 16 bytes).
    pub comm: String,
    /// Parent pid; 0 when unknown.
    pub ppid: i64,
    /// Opaque start-time token: equal for one process, different for a later one on the same
    /// pid. Linux: clock ticks since boot; macOS: microseconds since the epoch.
    pub start: Option<i64>,
    /// Exited but not yet reaped.
    pub zombie: bool,
}

/// The process's start token (see [`Info::start`]); None when it is gone or the platform has none.
pub fn start(pid: i64) -> Option<i64> {
    info(pid).and_then(|i| i.start)
}

#[cfg(target_os = "linux")]
pub fn info(pid: i64) -> Option<Info> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parens; fields resume after the last ')'.
    let (open, close) = (s.find('(')?, s.rfind(')')?);
    let f: Vec<&str> = s[close + 1..].split_whitespace().collect();
    Some(Info {
        comm: s[open + 1..close].to_string(),
        ppid: f.get(1)?.parse().ok()?,
        start: f.get(19).and_then(|v| v.parse().ok()),
        zombie: matches!(f.first(), Some(&"Z" | &"X")),
    })
}

#[cfg(target_os = "macos")]
pub fn info(pid: i64) -> Option<Info> {
    let pid = i32::try_from(pid).ok().filter(|p| *p > 0)?;
    let mut b: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: the buffer is a zeroed proc_bsdinfo of exactly the size passed.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut b as *mut libc::proc_bsdinfo).cast(), size) };
    if n != size {
        return None;
    }
    let comm: Vec<u8> = b.pbi_comm.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    Some(Info {
        comm: String::from_utf8_lossy(&comm).into_owned(),
        ppid: b.pbi_ppid as i64,
        start: Some(b.pbi_start_tvsec as i64 * 1_000_000 + b.pbi_start_tvusec as i64),
        zombie: b.pbi_status == libc::SZOMB,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn info(pid: i64) -> Option<Info> {
    let pid = i32::try_from(pid).ok().filter(|p| *p > 0)?;
    // SAFETY: signal 0 only checks that the process exists and may be signalled.
    let r = unsafe { libc::kill(pid, 0) };
    let exists = r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    exists.then(|| Info { comm: String::new(), ppid: 0, start: None, zombie: false })
}
