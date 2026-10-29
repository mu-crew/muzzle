use std::process::ExitCode;

use clap::Parser;
use muzzle::cli::{BenchCommand, Cli, Command};
use muzzle::{bench, doctor, exit, report, rules, run, status};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Run(args) => run::main(args),
        Command::Match(args) => rules::match_main(args),
        Command::Wrap(args) => rules::wrap_main(args),
        Command::Hook(args) => rules::hook_main(args),
        Command::Status(args) => status::main(args),
        Command::Doctor(args) => doctor::main(args),
        Command::Report(args) => report::main(args),
        Command::Bench(args) => match &args.command {
            BenchCommand::Run(args) => bench::run_main(args),
            BenchCommand::Probe(args) => bench::probe_main(args),
        },
    };
    result.unwrap_or_else(|error| {
        eprintln!("muzzle: {error:#}");
        ExitCode::from(exit::INTERNAL)
    })
}
