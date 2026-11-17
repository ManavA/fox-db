//! `foxd`: the long-lived foxdb daemon. Runs in the foreground until `fox daemon stop` (or a
//! signal); see `daemon` for the protocol.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("--version" | "-V") => {
            println!("foxd {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h") => {
            println!(
                "foxd: the foxdb daemon. Run it with no arguments; it stays in the foreground until `fox daemon stop`.\nfoxd --version prints the version."
            );
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("foxd: unknown argument {other}; run foxd with no arguments, or foxd --version");
            return ExitCode::from(2);
        }
    }
    match foxdb::daemon::serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("foxd: {e:#}");
            ExitCode::FAILURE
        }
    }
}
