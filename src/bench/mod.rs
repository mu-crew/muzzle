//! `muzzle bench` (spec §Benchmark runner, D35): trace replay and the
//! interactive probe. Touches admission only through `muzzle run`.

pub mod probe;
pub mod replay;

use std::process::ExitCode;

use crate::cli::{BenchProbeArgs, BenchRunArgs};

pub fn run_main(_args: &BenchRunArgs) -> anyhow::Result<ExitCode> {
    crate::exit::not_implemented("bench run")
}

pub fn probe_main(_args: &BenchProbeArgs) -> anyhow::Result<ExitCode> {
    crate::exit::not_implemented("bench probe")
}
