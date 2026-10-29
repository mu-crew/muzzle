//! Exit codes (spec §Interface, Exit codes). The command's own code and
//! 128 + signo pass through and have no constant.

use std::process::ExitCode;

/// `--run-timeout` fired.
pub const RUN_TIMEOUT: u8 = 124;
/// `--timeout` fired while queued.
pub const QUEUE_TIMEOUT: u8 = 3;
/// Bad usage or config. clap exits 2 for bad usage on its own.
pub const USAGE: u8 = 2;
/// muzzle itself failed.
pub const INTERNAL: u8 = 125;

/// The body of every command a unit has not filled in yet.
pub fn not_implemented(command: &str) -> anyhow::Result<ExitCode> {
    eprintln!("muzzle: {command}: not implemented");
    Ok(ExitCode::from(INTERNAL))
}
