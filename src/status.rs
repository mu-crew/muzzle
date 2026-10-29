//! `muzzle status [--json]` (spec §Interface, `status --json`).

use std::process::ExitCode;

use crate::cli::StatusArgs;

pub fn main(_args: &StatusArgs) -> anyhow::Result<ExitCode> {
    crate::exit::not_implemented("status")
}
