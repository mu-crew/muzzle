//! `muzzle bench` (spec §Benchmark runner, D35): trace replay and the
//! interactive probe. Touches admission only through `muzzle run`.

pub mod probe;
pub mod replay;

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;

use crate::cli::{BenchBurnArgs, BenchProbeArgs, BenchRunArgs};

pub fn run_main(args: &BenchRunArgs) -> anyhow::Result<ExitCode> {
    replay::run_main(args)
}

pub fn burn_main(args: &BenchBurnArgs) -> anyhow::Result<ExitCode> {
    replay::burn_main(args)
}

/// `bench probe`: `--samples` bursts (default 1, so cron runs one per
/// tick), `--interval` seconds apart (default 60), one JSON line each,
/// appended to `--out` or printed. The scratch repo lives under the state
/// dir and is reused across runs.
pub fn probe_main(args: &BenchProbeArgs) -> anyhow::Result<ExitCode> {
    let samples = args.samples.unwrap_or(1);
    let interval = args.interval.unwrap_or(60.0);
    if !(interval.is_finite() && interval >= 0.0) {
        eprintln!("muzzle: --interval {interval}: must be a number of seconds");
        return Ok(ExitCode::from(crate::exit::USAGE));
    }
    let store = crate::state::Store::open_default()?;
    let p = probe::Probe::open(&store.dir().join("bench").join("probe-repo"))?;
    for i in 0..samples {
        if i > 0 {
            std::thread::sleep(Duration::from_secs_f64(interval));
        }
        let line = serde_json::to_string(&p.burst()?.sample(replay::now_ms()))? + "\n";
        match &args.out {
            Some(path) => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| f.write_all(line.as_bytes()))
                .with_context(|| format!("appending to {}", path.display()))?,
            None => {
                print!("{line}");
                std::io::stdout().flush()?;
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
