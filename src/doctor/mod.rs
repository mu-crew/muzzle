//! `muzzle doctor [--json]` (spec §Interface, `doctor`; D15, D17a, D33).
//!
//! Gathers [`Facts`] from the host, then [`check`] turns them into lines and
//! warnings without I/O, so every rule here is unit-tested. Doctor never
//! writes `state.json` or the config template: it may run while the queue
//! is busy, and a check that changes what it checks is no check.
//!
//! Exit 0 means usable, even when only advisory (spec). A bad config exits
//! 2, as `muzzle run` would; an unwritable state directory or an unreadable
//! `state.json` (a newer schema, say) exits 125, since every `run` would
//! fail on it.

mod checks;
mod gather;
mod render;

pub use checks::check;
pub use gather::harnesses;
pub use render::render_text;

use std::path::PathBuf;
use std::process::ExitCode;

use serde::Serialize;

use crate::cli::DoctorArgs;
use crate::rules::config::{self, Config, ConfigError, TEMPLATE};
use crate::runlog::Stats;
use crate::state::Budget;

/// The config at [`config::path`], without writing the template when it is
/// missing: the defaults apply then, as for `run` (spec §Config).
pub(crate) fn read_config() -> (PathBuf, Result<Config, ConfigError>, bool) {
    let path = config::path();
    match std::fs::metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let c = Config::parse(TEMPLATE, &path);
            (path, c, false)
        }
        _ => {
            let c = Config::load_from(&path);
            (path, c, true)
        }
    }
}

/// Everything doctor looks at, read once.
#[derive(Debug, Clone)]
pub struct Facts {
    pub os: &'static str,
    pub arch: &'static str,
    /// This process's own reading (D7, D9), before the reserve.
    pub cores: Option<u32>,
    pub ram: Option<u64>,
    pub config_path: PathBuf,
    pub config_exists: bool,
    pub config: Result<Config, String>,
    /// `state.json`'s stored budget (D17a), when one is stored and not
    /// expired ([`crate::state::State::live_budget`]).
    pub stored: Option<Budget>,
    pub state_dir: PathBuf,
    /// `Err` with the reason when a file can't be created there.
    pub state_writable: Result<(), String>,
    /// `Err` with the reason when `state.json` can't be read, as every
    /// `run` would fail on it.
    pub state_readable: Result<(), String>,
    pub systemd: bool,
    pub runs: Option<RunsLog>,
    /// The live file and its monthly archives (P11); `None` when unreadable.
    pub runlog: Option<Stats>,
    /// Rules that `runs.jsonl` shows entering a container (D16a).
    pub container_runs: Vec<String>,
    pub harnesses: Harnesses,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunsLog {
    /// Size of `runs.jsonl` itself.
    pub bytes: u64,
    /// Runs in the live file plus the newest archive (rv020r_doctor_live_only).
    pub lines: usize,
    /// The oldest and newest `end`, RFC 3339 strings compare in time order.
    pub oldest: Option<String>,
    pub newest: Option<String>,
}

/// Where each harness's muzzle hook or extension was found; empty when not.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Harnesses {
    pub pi_extension: Vec<PathBuf>,
    pub claude_hook: Vec<PathBuf>,
    pub codex_hook: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub version: &'static str,
    pub platform: String,
    pub budget: Option<u32>,
    pub cores: Option<u32>,
    pub reserve: Option<u32>,
    pub ram_per_slot_mb: Option<u64>,
    /// `stored` (state.json, D17a), `config` (`slots =`) or `measured`.
    pub budget_source: &'static str,
    pub measured: Option<BudgetView>,
    pub stored: Option<BudgetView>,
    pub enforced: bool,
    pub enforcement: String,
    pub state_dir: PathBuf,
    pub state_writable: bool,
    pub state_readable: bool,
    pub config_path: PathBuf,
    pub config_exists: bool,
    pub config_error: Option<String>,
    pub rules: Option<usize>,
    /// Added field (P8): the root file, then each include in list order,
    /// with whether it was found. Includes are unknown when the config is
    /// invalid, so only the root is listed then.
    pub config_files: Vec<ConfigFile>,
    /// Added field (P8): distinct path tables in the merged config.
    pub path_tables: Option<usize>,
    pub config_hash: Option<String>,
    pub runs: Option<RunsLog>,
    /// Added field (P11): `runs.jsonl`'s size and its monthly archives.
    pub runs_archives: Option<Stats>,
    /// Added field (D16a): rules whose jobs enter a container, so muzzle
    /// admits them but can't cap or measure them. Empty when none.
    pub container_rules: Vec<String>,
    /// Added field (D16a): catch-all rules that match a container entry as
    /// well as host commands. Their host runs are enforced.
    pub container_maybe: Vec<String>,
    pub pi_extension: &'static str,
    pub claude_hook: &'static str,
    pub codex_hook: &'static str,
    pub harness_files: Harnesses,
    pub warnings: Vec<String>,
    /// Exit status: 0 usable, 2 bad config, 125 state unwritable or
    /// unreadable.
    #[serde(skip)]
    pub exit: u8,
}

/// One config file doctor read or looked for (P8).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigFile {
    pub file: PathBuf,
    /// `root` or `include`.
    pub role: &'static str,
    /// A missing include is skipped silently (P5); a missing root means the
    /// built-in defaults apply.
    pub found: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BudgetView {
    pub budget: u32,
    pub cores: u32,
    pub ram_per_slot_mb: u64,
}

pub fn main(args: &DoctorArgs) -> anyhow::Result<ExitCode> {
    let report = check(&gather::gather());
    if args.json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print!("{}", render_text(&report));
    }
    Ok(ExitCode::from(report.exit))
}
