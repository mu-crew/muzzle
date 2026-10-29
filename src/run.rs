//! Unit `run` (spec §Architecture 4, D14-D19): nesting, enqueue and poll,
//! spawn, signals, keeper handoff, release, runs.jsonl, `--report`.

use std::process::ExitCode;

use crate::cli::RunArgs;

pub fn main(_args: &RunArgs) -> anyhow::Result<ExitCode> {
    crate::exit::not_implemented("run")
}
