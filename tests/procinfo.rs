//! Process facts the agent registry relies on, on every platform foxdb ships for (Linux reads
//! `/proc`, macOS asks the kernel with `proc_pidinfo`): a live process is alive, a dead or zombie
//! one is not, a start-time token tells a reused pid apart, and the parent chain is readable.

use foxdb::{agents, procinfo};
use std::process::Command;

#[test]
fn this_process_is_alive_with_a_stable_start_token_and_its_parent() {
    let me = std::process::id() as i64;
    let info = procinfo::info(me).expect("own process");
    assert!(!info.zombie);
    assert_eq!(info.ppid, std::os::unix::process::parent_id() as i64);
    assert!(!info.comm.is_empty());
    let start = procinfo::start(me);
    assert!(start.is_some());
    assert_eq!(start, procinfo::start(me), "the token is stable");
    assert!(agents::alive(me, start));
    // A different start token is a different process that reused the pid.
    assert!(!agents::alive(me, start.map(|s| s + 1)));
}

#[test]
fn a_child_names_itself_and_is_dead_once_it_exits_even_before_it_is_reaped() {
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id() as i64;
    let info = procinfo::info(pid).expect("child");
    assert_eq!(info.comm, "sleep");
    assert_eq!(info.ppid, std::process::id() as i64);
    let start = procinfo::start(pid);
    assert!(agents::alive(pid, start));
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    // Killed but not yet reaped: a zombie, which is not alive.
    let t = std::time::Instant::now();
    while agents::alive(pid, start) {
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "a killed child still reads as alive");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.wait().unwrap();
    assert!(!agents::alive(pid, None));
    assert!(procinfo::info(pid).is_none_or(|i| i.zombie));
}
