//! `muzzle report` (spec §Interface, `report` and `runs.jsonl`; D29, D30,
//! D30a, D41). Reads `runs.jsonl`, plus the live stored budget in
//! `state.json` and the config's `slots =` to size suggestions as the next
//! run would (D17a, see [`host`]): the arithmetic is pure over parsed
//! records, so tests drive it with fixtures. It never edits the config
//! (D29).

use std::process::ExitCode;

use crate::cli::ReportArgs;

mod by_repo;
mod machine;
mod period;
mod render;
mod rows;

pub use machine::{Machine, machine};
pub use period::{P5090, Period, Run, parse, parse_point, parse_range, percentile};
pub use render::render;
pub use rows::{Delta, NOT_MEASURED_CONTAINER, Row, Suggest, Tag, deltas, row, rows};

// Shared between the submodules (and `by_repo`), not public.
use period::{fmt_ts, p5090};
use render::{hint_text, label, machine_json, machine_text, pair, period, row_json, suggest_text};
use rows::{gb, group};

/// D30: a rule gets a measured or experiment suggestion only after this
/// many clean runs.
pub const MIN_CLEAN_RUNS: usize = 5;
/// D30a: cores used p50 at or above this share of the slots held is
/// saturated.
pub const SATURATED: f64 = 0.9;
/// D30a: an experiment tries this many slots more than the saturated count.
pub const EXPERIMENT_STEP: u32 = 2;
/// D30a: an OOM suggestion covers the largest OOM peak plus 25%.
pub const OOM_HEADROOM: f64 = 1.25;
/// P12: the unclaimed-load line needs this many runs carrying `load1` and
/// `held`; fewer is noise from a lagging load average.
pub const MIN_UNCLAIMED_RUNS: usize = 20;
/// Spec §report: report can't see commands that match no rule, so it names
/// the query that finds them instead of guessing.
pub const UNMATCHED_HINT: &str = "museum-search calls --min-secs 30 --tool bash";

/// What the host contributes: neither is in `runs.jsonl`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Host {
    /// D9 memory per slot, in MB. `None` disables memory suggestions.
    pub mem_per_slot_mb: Option<f64>,
    /// Cap for suggestions when the runs don't carry a budget.
    pub budget: Option<u32>,
}

/// D9 memory per slot and the budget the next run at `now_ms` would use,
/// as state.rs `resolve_budget` picks it (D17a): with a config `slots =`,
/// that slot count and this host's own cores and RAM; otherwise the stored
/// budget, read through `live_budget` as `status` and `doctor` do (an idle
/// state file past `BUDGET_TTL_MS`, which the next run ignores, sizes
/// nothing); else this host's reading with no cap.
fn host(store: &crate::state::Store, now_ms: u64, config_slots: Option<u32>) -> Host {
    let live = store.read().ok().and_then(|s| s.live_budget(now_ms));
    let reading = crate::proc::logical_cores()
        .ok()
        .filter(|&c| c > 0)
        .zip(crate::proc::total_ram().ok());
    resolve_host(live, config_slots, reading)
}

/// The pure part of [`host`]: `reading` is this host's (cores, RAM bytes).
fn resolve_host(
    live: Option<crate::state::Budget>,
    config_slots: Option<u32>,
    reading: Option<(usize, u64)>,
) -> Host {
    const MB: f64 = 1024.0 * 1024.0;
    let measured = reading.map(|(c, r)| r as f64 / c as f64 / MB);
    let stored = live.map(|b| b.ram_per_slot as f64 / MB);
    match (config_slots, live) {
        // As doctor checks.rs: no reading means the run would fail to read
        // too, so the stored RAM beats nothing.
        (Some(slots), _) => Host {
            mem_per_slot_mb: measured.or(stored),
            budget: Some(slots),
        },
        (None, Some(b)) => Host {
            mem_per_slot_mb: stored,
            budget: Some(b.budget),
        },
        (None, None) => Host {
            mem_per_slot_mb: measured,
            budget: None,
        },
    }
}

/// The span `runlog::read` needs for `args`, so archives outside it stay
/// unopened (P11); `None` reads everything. A run counts in the machine
/// figures while it waits or holds, so a line that ends shortly after
/// `until` can still matter: the end gets a day of slack.
fn read_span(args: &ReportArgs, now: f64) -> Option<(f64, f64)> {
    // A bad date reads everything; `render` then reports it.
    let point = |s: &Option<String>, upper| match s {
        Some(s) => parse_point(s, now, upper).map(Some).ok(),
        None => Some(None),
    };
    let periods: Vec<Period> = match &args.compare {
        Some(cmp) => cmp
            .iter()
            .map(|p| parse_range(p, now).ok())
            .collect::<Option<_>>()?,
        None => vec![Period {
            since: point(&args.since, false)?,
            until: point(&args.until, true)?,
        }],
    };
    let since = periods
        .iter()
        .map(|p| p.since.unwrap_or(f64::NEG_INFINITY))
        .fold(f64::INFINITY, f64::min);
    let until = periods
        .iter()
        .map(|p| p.until.map_or(f64::INFINITY, |u| u + 86400.0))
        .fold(f64::NEG_INFINITY, f64::max);
    (since > f64::NEG_INFINITY || until < f64::INFINITY).then_some((since, until))
}

/// `--by repo` needs the current config (B6), which plain `report` never
/// reads; a config error exits as everywhere else: 2 for a bad file, 125
/// for one that can't be read.
fn render_by_repo(
    runs: &[Run],
    args: &ReportArgs,
    now: f64,
    host: Host,
) -> Result<anyhow::Result<String>, ExitCode> {
    use crate::rules::config::{Config, ConfigError};
    let config = Config::load().map_err(|e| {
        eprintln!("muzzle: {e}");
        ExitCode::from(match e {
            ConfigError::Read { .. } => crate::exit::INTERNAL,
            _ => crate::exit::USAGE,
        })
    })?;
    Ok(period(args, now).map(|p| {
        let rows = by_repo::rows(runs, p, args.rule.as_deref(), host, &config);
        by_repo::render(&machine(runs, p), &rows, args.json)
    }))
}

pub fn main(args: &ReportArgs) -> anyhow::Result<ExitCode> {
    let store = crate::state::Store::open_default()?;
    let now_ms = crate::status::now_ms();
    let now = now_ms as f64 / 1000.0;
    let (runs, skipped) = parse(&crate::runlog::read(store.dir(), read_span(args, now))?);
    if skipped > 0 {
        eprintln!("muzzle: report: skipped {skipped} unreadable runs.jsonl lines");
    }
    // Only `slots =` matters here (D17a). Read only an existing file, as
    // doctor does, so report never writes the template (D29); a bad config
    // is `--by repo`'s to report, so plain `report` sizes as if it set none.
    let path = crate::rules::config::path();
    let config_slots = path
        .exists()
        .then(|| crate::rules::config::Config::load_from(&path).ok())
        .flatten()
        .and_then(|c| c.slots);
    let host = host(&store, now_ms, config_slots);
    let out = match args.by {
        Some(crate::cli::By::Repo) => render_by_repo(&runs, args, now, host),
        None => Ok(render(&runs, args, now, host)),
    };
    let out = match out {
        Ok(out) => out,
        Err(code) => return Ok(code),
    };
    match out {
        Ok(out) => {
            print!("{out}");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("muzzle: report: {e:#}");
            Ok(ExitCode::from(crate::exit::USAGE))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admit::Class;
    use crate::state::RunRecord;

    pub(super) const T0: f64 = 1_793_700_000.0; // 2026-11-03-ish, whole seconds.
    pub(super) const HOST: Host = Host {
        mem_per_slot_mb: Some(2048.0),
        budget: None,
    };

    #[test]
    fn host_ignores_an_expired_stored_budget() {
        // D17a: the next run drops a budget stored over an hour ago in an
        // idle file, so report must not size memory from it.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::state::Store::open(dir.path()).unwrap();
        std::fs::write(
            store.state_path(),
            r#"{"v":1,"next_ticket":1,"updated_ms":1000,
               "budget":{"budget":3,"cores":3,"ram_per_slot":1048576}}"#,
        )
        .unwrap();
        let fresh = host(&store, 1000 + crate::state::BUDGET_TTL_MS, None);
        assert_eq!((fresh.budget, fresh.mem_per_slot_mb), (Some(3), Some(1.0)));
        let stale = host(&store, 1001 + crate::state::BUDGET_TTL_MS, None);
        assert_eq!(stale.budget, None, "{stale:?}");
        assert_ne!(stale.mem_per_slot_mb, Some(1.0));
    }

    #[test]
    fn config_slots_sizes_from_this_reading() {
        // D17a, state.rs resolve_budget: with `slots =` the next run keeps
        // the slot count and replaces the stored cores and RAM with its own
        // reading, so a container-written 1 MB/slot must not size anything.
        let stored = crate::state::Budget {
            budget: 6,
            cores: 8,
            ram_per_slot: 1024 * 1024,
        };
        let reading = Some((16, 16 * 2048 * 1024 * 1024));
        let h = resolve_host(Some(stored), Some(5), reading);
        assert_eq!((h.budget, h.mem_per_slot_mb), (Some(5), Some(2048.0)));
        // No stored budget: the cap is still the slots value.
        let h = resolve_host(None, Some(5), reading);
        assert_eq!((h.budget, h.mem_per_slot_mb), (Some(5), Some(2048.0)));
        // Without `slots =` the live stored budget wins (unchanged).
        let h = resolve_host(Some(stored), None, reading);
        assert_eq!((h.budget, h.mem_per_slot_mb), (Some(6), Some(1.0)));
    }

    /// A clean `gate` run on 2 slots under cfg `c1`, budget 8.
    pub(super) fn run(end: f64, ran_s: f64) -> Run {
        Run {
            end,
            rec: RunRecord {
                v: 1,
                end: fmt_ts(end),
                rule: Some("gate".into()),
                slots: 2,
                class: Class::Normal,
                cfg: Some("c1".into()),
                agent: None,
                cwd: "/x".into(),
                repo: None,
                waited_s: 0.0,
                ran_s,
                exit: Some(0),
                killed: None,
                cpu_s: Some(ran_s),
                mem_peak_mb: Some(100),
                enforced: true,
                nested: false,
                kept: false,
                budget: Some(8),
                enforced_reason: None,
                load1: None,
                held: None,
            },
        }
    }

    /// `n` clean runs with these cores used, an hour apart from `start`.
    pub(super) fn with_cores(start: f64, slots: u32, cores: &[f64]) -> Vec<Run> {
        cores
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let mut r = run(start + 3600.0 * i as f64, 100.0);
                r.rec.slots = slots;
                r.rec.cpu_s = Some(c * 100.0);
                r
            })
            .collect()
    }

    pub(super) fn one_row(runs: &[Run]) -> Row {
        let refs: Vec<&Run> = runs.iter().collect();
        row(Some("gate".into()), &refs, HOST)
    }

    pub(super) fn slots_tag(s: &Suggest) -> (u32, Tag, String) {
        match s {
            Suggest::Slots {
                slots,
                tag,
                evidence,
            } => (*slots, *tag, evidence.clone()),
            Suggest::None(r) => panic!("no suggestion: {r}"),
        }
    }

    pub(super) fn args(json: bool) -> ReportArgs {
        ReportArgs {
            rule: None,
            since: None,
            until: None,
            compare: None,
            by: None,
            json,
        }
    }
}
