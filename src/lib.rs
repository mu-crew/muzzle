//! muzzle: per-host admission for heavy commands, with a run log that tunes
//! itself. See `docs/specs/2026-10-29-muzzle.md`.
//!
//! One module per unit in the spec's §Architecture. Each command module
//! exposes an entry point `(args) -> anyhow::Result<ExitCode>` that `main.rs`
//! dispatches to: `main` for a module with one subcommand, `<sub>_main` for
//! one with several (`rules::match_main`, `bench::probe_main`). A unit only
//! edits its own files.

pub mod admit;
pub mod bench;
pub mod cli;
pub mod doctor;
pub mod exit;
pub mod proc;
pub mod repo;
pub mod report;
pub mod rules;
pub mod run;
pub mod runlog;
pub mod state;
pub mod status;
