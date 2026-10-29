//! `muzzle report` (spec §Interface, `report`; D30, D30a, D41).

use std::process::ExitCode;

use crate::cli::ReportArgs;

pub fn main(_args: &ReportArgs) -> anyhow::Result<ExitCode> {
    crate::exit::not_implemented("report")
}
