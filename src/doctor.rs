//! `muzzle doctor [--json]` (spec §Interface, `doctor`).

use std::process::ExitCode;

use crate::cli::DoctorArgs;

pub fn main(_args: &DoctorArgs) -> anyhow::Result<ExitCode> {
    crate::exit::not_implemented("doctor")
}
