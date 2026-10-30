//! The CLI surface, spec §Interface/CLI. Every subcommand is declared here so
//! units fill in bodies without touching the parser. Flags may be added;
//! renaming one breaks the integrations that call it.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "muzzle",
    version,
    about = "Per-host admission for heavy commands, with a run log that tunes itself."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Queue for slots, then run CMD ARGS… directly (argv, not a shell).
    Run(RunArgs),
    /// Print the rule a shell string matches, as JSON.
    Match(MatchArgs),
    /// Print the `muzzle run …` rewrite of a shell string, as JSON.
    Wrap(WrapArgs),
    /// Answer a harness PreToolUse hook: payload on stdin, reply on stdout.
    Hook(HookArgs),
    /// Show the budget, holders and queue.
    Status(StatusArgs),
    /// Check the budget, enforcement, state, config and harness setup.
    Doctor(DoctorArgs),
    /// Summarise runs.jsonl: machine figures, per-rule rows, suggestions.
    Report(ReportArgs),
    /// Replay traces and sample the interactive probe.
    Bench(BenchArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Class {
    High,
    Normal,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Slots to claim; capped to the budget.
    #[arg(long, value_name = "N")]
    pub slots: Option<u32>,
    #[arg(long, value_enum)]
    pub class: Option<Class>,
    /// The rule label shown in status, reports and runs.jsonl.
    #[arg(long, value_name = "LABEL")]
    pub rule: Option<String>,
    /// Kill the job after S seconds of running (exit 124).
    #[arg(long, value_name = "S")]
    pub run_timeout: Option<f64>,
    /// Give up after S seconds queued (exit 75).
    #[arg(long, value_name = "S")]
    pub timeout: Option<f64>,
    /// Write the run's figures as JSON to FILE on exit.
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
    /// The command and its arguments.
    #[arg(last = true, required = true, value_name = "CMD")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct MatchArgs {
    #[arg(long)]
    pub json: bool,
    /// Resolve path tables for DIR (its repo, else DIR).
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    #[arg(last = true, required = true, value_name = "SHELL STRING")]
    pub command: String,
}

#[derive(Debug, Args)]
pub struct WrapArgs {
    /// JSON output, the only format (kept explicit for the integrations).
    #[arg(long, required = true)]
    pub json: bool,
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// The harness's timeout; becomes `--run-timeout S`.
    #[arg(long, value_name = "S")]
    pub timeout: Option<f64>,
    #[arg(last = true, required = true, value_name = "SHELL STRING")]
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Harness {
    Claude,
    Codex,
}

#[derive(Debug, Args)]
pub struct HookArgs {
    #[arg(value_enum)]
    pub harness: Harness,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum By {
    Repo,
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    #[arg(long, value_name = "LABEL")]
    pub rule: Option<String>,
    #[arg(long, value_name = "D")]
    pub since: Option<String>,
    #[arg(long, value_name = "D")]
    pub until: Option<String>,
    /// Two periods side by side: `--compare A..B C..D`.
    #[arg(long, num_args = 2, value_names = ["A..B", "C..D"])]
    pub compare: Option<Vec<String>>,
    /// Rows per (rule, repo), each with the slots the current config gives
    /// that repo (0.2 P9). Not with `--compare`: out of scope, so clap
    /// exits 2.
    #[arg(long, value_enum, conflicts_with = "compare")]
    pub by: Option<By>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    #[command(subcommand)]
    pub command: BenchCommand,
}

#[derive(Debug, Subcommand)]
pub enum BenchCommand {
    /// Replay a trace's calls at their offsets.
    Run(BenchRunArgs),
    /// Sample the interactive probe, one JSON line per sample (D35).
    Probe(BenchProbeArgs),
    /// The synthetic CPU burner `bench run` replays (tier 1).
    #[command(hide = true)]
    Burn(BenchBurnArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BenchMode {
    /// Today's practice: role caps, no queue.
    None,
    /// Every call through `muzzle run`, no role caps.
    Muzzle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BenchTier {
    /// Tier 1: CPU burners sized to each call's rule.
    Synthetic,
    /// Tier 2: real commands from bench/real.toml.
    Real,
}

#[derive(Debug, Args)]
pub struct BenchRunArgs {
    #[arg(value_name = "TRACE")]
    pub trace: PathBuf,
    #[arg(long, value_enum)]
    pub mode: Option<BenchMode>,
    #[arg(long, value_enum)]
    pub tier: Option<BenchTier>,
    /// Time compression factor.
    #[arg(long, value_name = "X", default_value_t = 1.0)]
    pub speed: f64,
    /// Replay one window only, such as `busiest:3h`.
    #[arg(long, value_name = "WINDOW")]
    pub window: Option<String>,
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Tier 2's rule commands; default `bench/real.toml`.
    #[arg(long, value_name = "FILE")]
    pub real: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct BenchBurnArgs {
    /// CPU-seconds to burn, summed over the threads.
    #[arg(long, value_name = "S")]
    pub cpu_s: f64,
    /// Busy threads; default `CARGO_BUILD_JOBS`, else 1.
    #[arg(long, value_name = "N")]
    pub threads: Option<u32>,
}

#[derive(Debug, Args)]
pub struct BenchProbeArgs {
    #[arg(long, value_name = "N")]
    pub samples: Option<u32>,
    /// Seconds between samples.
    #[arg(long, value_name = "S")]
    pub interval: Option<f64>,
    /// Append samples to FILE instead of stdout.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }
}
