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
    /// Answer a harness shell hook (PreToolUse, or cursor-agent's
    /// beforeShellExecution): payload on stdin, reply on stdout.
    Hook(HookArgs),
    /// Drop wedged claims from the state (G4).
    Drop(DropArgs),
    /// Show the budget, holders and queue.
    Status(StatusArgs),
    /// Check the budget, enforcement, state, config and harness setup.
    Doctor(DoctorArgs),
    /// Add or update muzzle's hook entry in each harness's settings file,
    /// leaving every other entry alone.
    Init(InitArgs),
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
    /// While queued, keep FILE holding the current OSC 7501 program status
    /// report, for a harness with no terminal to forward; cleared when the
    /// wait ends.
    #[arg(long, value_name = "FILE")]
    pub status: Option<PathBuf>,
    /// Queue even behind this caller's own orphaned claim (G2).
    #[arg(long)]
    pub allow_concurrent: bool,
    /// Wait indefinitely, overriding a configured queue timeout (G5).
    /// `--timeout 0` still means try once.
    #[arg(long, conflicts_with = "timeout")]
    pub no_timeout: bool,
    /// The command and its arguments.
    #[arg(last = true, required = true, value_name = "CMD")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct DropArgs {
    /// Tickets of the holders to drop.
    #[arg(value_name = "TICKET")]
    pub tickets: Vec<u64>,
    /// Drop every dead-muzzle and kept holder.
    #[arg(long, conflicts_with = "tickets")]
    pub dead: bool,
    /// Drop the dead and kept holders of this agent.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["tickets", "dead"])]
    pub agent: Option<String>,
    /// Also SIGTERM, then after 5 s SIGKILL, the surviving members.
    #[arg(long)]
    pub kill: bool,
    /// Allow dropping a healthy live holder.
    #[arg(long)]
    pub force: bool,
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
    /// Claude Code, and Muse Code, whose `PreToolUse` payloads Claude's
    /// format covers once its `bash` tool is mapped (every-harness D14).
    Claude,
    Codex,
    /// cursor-agent's `beforeShellExecution`: deny with the rewrite as the
    /// reason, since it can't rewrite (every-harness D14).
    Cursor,
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

/// A harness whose settings `init` writes (every-harness D13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookTarget {
    Claude,
    Codex,
    CursorAgent,
    Muse,
    All,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Write these harnesses' entries even where they aren't set up yet;
    /// by default only harnesses whose config directory exists.
    #[arg(long, value_enum, value_delimiter = ',', value_name = "LIST")]
    pub hooks: Option<Vec<HookTarget>>,
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

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("muzzle").chain(args.iter().copied()))
    }

    #[test]
    fn run_takes_allow_concurrent_and_no_timeout() {
        let Command::Run(a) = parse(&["run", "--allow-concurrent", "--no-timeout", "--", "x"])
            .unwrap()
            .command
        else {
            panic!("not run");
        };
        assert!(a.allow_concurrent && a.no_timeout);
        let Command::Run(a) = parse(&["run", "--", "x"]).unwrap().command else {
            panic!("not run");
        };
        assert!(!a.allow_concurrent && !a.no_timeout);
        assert!(parse(&["run", "--no-timeout", "--timeout", "0", "--", "x"]).is_err());
    }

    #[test]
    fn drop_takes_tickets_or_a_selector() {
        let Command::Drop(a) = parse(&["drop", "--kill", "--force", "4", "5"])
            .unwrap()
            .command
        else {
            panic!("not drop");
        };
        assert_eq!(a.tickets, [4, 5]);
        assert!(a.kill && a.force && !a.dead && a.agent.is_none());
        let Command::Drop(a) = parse(&["drop", "--dead"]).unwrap().command else {
            panic!("not drop");
        };
        assert!(a.dead && a.tickets.is_empty());
        let Command::Drop(a) = parse(&["drop", "--agent", "w1"]).unwrap().command else {
            panic!("not drop");
        };
        assert_eq!(a.agent.as_deref(), Some("w1"));
        assert!(parse(&["drop", "--dead", "3"]).is_err());
        assert!(parse(&["drop", "x"]).is_err());
    }
}
