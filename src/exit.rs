//! Exit codes (spec §Interface, Exit codes). The command's own code and
//! 128 + signo pass through and have no constant.

use std::process::ExitCode;

/// `--run-timeout` fired.
pub const RUN_TIMEOUT: u8 = 124;
/// `--timeout` fired while queued: 75, sysexits' EX_TEMPFAIL ("try again
/// later"). Not 3: mu 4.2.0 reads exit 3 from a host template as a hand-back
/// to the human, and mule uses 3 for "no control master".
pub const QUEUE_TIMEOUT: u8 = 75;
/// Bad usage or config. clap exits 2 for bad usage on its own.
pub const USAGE: u8 = 2;
/// muzzle itself failed.
pub const INTERNAL: u8 = 125;

/// The body of every command a unit has not filled in yet.
pub fn not_implemented(command: &str) -> anyhow::Result<ExitCode> {
    eprintln!("muzzle: {command}: not implemented");
    Ok(ExitCode::from(INTERNAL))
}
