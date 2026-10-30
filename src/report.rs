//! `muzzle report` (spec §Interface, `report` and `runs.jsonl`; D29, D30,
//! D30a, D41). Reads `runs.jsonl` and nothing else of muzzle's: the
//! arithmetic is pure over parsed records, so tests drive it with fixtures.
//! It never edits the config (D29).

use std::collections::BTreeMap;
use std::process::ExitCode;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::cli::ReportArgs;
use crate::state::{Killed, RunRecord};

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

/// A half-open period `[since, until)` in Unix seconds; `None` is open.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Period {
    pub since: Option<f64>,
    pub until: Option<f64>,
}

/// A parsed record with its times resolved, so nothing re-parses `end`.
#[derive(Debug, Clone)]
pub struct Run {
    pub rec: RunRecord,
    /// Unix seconds.
    pub end: f64,
}

impl Run {
    fn run_start(&self) -> f64 {
        self.end - self.rec.ran_s
    }

    fn wait_start(&self) -> f64 {
        self.run_start() - self.rec.waited_s
    }

    /// D30: exit 0 and not killed. OOM runs are never clean (D30a).
    pub fn is_clean(&self) -> bool {
        self.rec.exit == Some(0) && self.rec.killed.is_none()
    }

    /// Spec §report: cores used = `cpu_s / ran_s`.
    pub fn cores(&self) -> Option<f64> {
        let cpu = self.rec.cpu_s?;
        (self.rec.ran_s > 0.0).then(|| cpu / self.rec.ran_s)
    }
}

/// Parses `runs.jsonl` text. Lines that don't parse or have a newer schema
/// are skipped and counted, so one torn line doesn't hide a month of runs.
pub fn parse(text: &str) -> (Vec<Run>, usize) {
    let mut runs = Vec::new();
    let mut skipped = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let parsed = serde_json::from_str::<RunRecord>(line)
            .ok()
            .filter(|r| r.v == 1)
            .and_then(|rec| Some((parse_ts(&rec.end)?, rec)));
        match parsed {
            Some((end, rec)) => runs.push(Run { rec, end }),
            None => skipped += 1,
        }
    }
    runs.sort_by(|a, b| a.end.total_cmp(&b.end));
    (runs, skipped)
}

fn parse_ts(s: &str) -> Option<f64> {
    let ts: jiff::Timestamp = s.parse().ok()?;
    Some(ts.as_millisecond() as f64 / 1000.0)
}

fn fmt_ts(secs: f64) -> String {
    jiff::Timestamp::from_second(secs.floor() as i64)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|_| format!("{secs}"))
}

/// A `D` for `--since`/`--until` or one side of `A..B`: `now`, a relative
/// `30m`/`12h`/`7d` back from now, a `YYYY-MM-DD` UTC date, or an RFC 3339
/// timestamp. A date as an upper bound includes that whole day.
pub fn parse_point(s: &str, now: f64, upper: bool) -> anyhow::Result<f64> {
    let s = s.trim();
    if s == "now" {
        return Ok(now);
    }
    if let Some(unit) = s.chars().last().and_then(|c| match c {
        'm' => Some(60.0),
        'h' => Some(3600.0),
        'd' => Some(86400.0),
        _ => None,
    }) && let Ok(n) = s[..s.len() - 1].parse::<f64>()
    {
        return Ok(now - n * unit);
    }
    if let Ok(date) = s.parse::<jiff::civil::Date>() {
        let start = date
            .to_zoned(jiff::tz::TimeZone::UTC)
            .context("date out of range")?
            .timestamp()
            .as_second() as f64;
        return Ok(if upper { start + 86400.0 } else { start });
    }
    parse_ts(s).with_context(|| {
        format!("bad date {s:?}: use now, 7d, 12h, 30m, YYYY-MM-DD or an RFC 3339 time")
    })
}

/// `A..B`, either side may be empty.
pub fn parse_range(s: &str, now: f64) -> anyhow::Result<Period> {
    let Some((a, b)) = s.split_once("..") else {
        bail!("bad period {s:?}: expected A..B");
    };
    let side = |x: &str, upper| {
        (!x.trim().is_empty())
            .then(|| parse_point(x, now, upper))
            .transpose()
    };
    Ok(Period {
        since: side(a, false)?,
        until: side(b, true)?,
    })
}

impl Period {
    /// Rule rows count a run in the period its `end` falls in.
    fn contains(&self, t: f64) -> bool {
        self.since.is_none_or(|s| t >= s) && self.until.is_none_or(|u| t < u)
    }

    /// Closes open ends with the span of the runs, from the first wait to
    /// the last end.
    fn resolve(&self, runs: &[Run]) -> Option<(f64, f64)> {
        let lo = self
            .since
            .or_else(|| runs.iter().map(Run::wait_start).min_by(f64::total_cmp))?;
        let hi = self
            .until
            .or_else(|| runs.iter().map(|r| r.end).max_by(f64::total_cmp))?;
        (hi > lo).then_some((lo, hi))
    }
}

/// Nearest-rank percentile: the smallest value with at least `p`% of the
/// sample at or below it. `None` for an empty sample.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    Some(v[rank.clamp(1, v.len()) - 1])
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct P5090 {
    pub p50: f64,
    pub p90: f64,
}

fn p5090(values: &[f64]) -> Option<P5090> {
    Some(P5090 {
        p50: percentile(values, 50.0)?,
        p90: percentile(values, 90.0)?,
    })
}

/// D41 machine figures over a period.
#[derive(Debug, Clone, PartialEq)]
pub struct Machine {
    pub since: f64,
    pub until: f64,
    pub budget: Option<u32>,
    pub runs: usize,
    pub waited_s: f64,
    pub ran_s: f64,
    pub held_slot_s: f64,
    /// Slot-seconds held ÷ budget-seconds.
    pub utilisation: Option<f64>,
    /// Slot-seconds left free while someone waited.
    pub blocked_idle_slot_s: Option<f64>,
    /// Share of the period with at least one waiter.
    pub queue_nonempty: f64,
    /// CPU seconds ÷ slot-seconds held, over runs with CPU figures.
    pub cpu_efficiency: Option<f64>,
    /// P12: `max(0, load1 - held)` at admission, `None` under
    /// `MIN_UNCLAIMED_RUNS`; and how many runs carry both fields.
    pub unclaimed_load: Option<P5090>,
    pub unclaimed_n: usize,
}

/// P12: load muzzle didn't admit, over runs recording both fields.
fn unclaimed_load(runs: &[&Run]) -> (Option<P5090>, usize) {
    let v: Vec<f64> = runs
        .iter()
        .filter_map(|r| Some((r.rec.load1? - f64::from(r.rec.held?)).max(0.0)))
        .collect();
    let p = (v.len() >= MIN_UNCLAIMED_RUNS).then(|| p5090(&v)).flatten();
    (p, v.len())
}

/// D41 by a sweep over each run's wait and hold intervals, clipped to the
/// period. Only admitted runs are logged, so a waiter that gave up
/// (`--timeout`, killed while queued) is not in the queue figures.
pub fn machine(runs: &[Run], period: Period) -> Option<Machine> {
    let (lo, hi) = period.resolve(runs)?;
    // Delta events: (time, held slots, waiters).
    let mut events: Vec<(f64, i64, i64)> = Vec::new();
    let mut budget: Option<u32> = None;
    for r in runs.iter().filter(|r| !r.rec.nested) {
        let (w0, w1, e) = (r.wait_start().max(lo), r.run_start().min(hi), r.end);
        if w1 > w0 {
            events.push((w0, 0, 1));
            events.push((w1, 0, -1));
        }
        let (h0, h1) = (r.run_start().max(lo), e.min(hi));
        if h1 > h0 {
            let slots = i64::from(r.rec.slots);
            events.push((h0, slots, 0));
            events.push((h1, -slots, 0));
        }
        if (w1 > w0 || h1 > h0)
            && let Some(b) = r.rec.budget
        {
            // A budget change inside the period (D17a) is rare; the
            // largest one keeps utilisation from going over 100%.
            budget = Some(budget.map_or(b, |x| x.max(b)));
        }
    }
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut held, mut waiting) = (0i64, 0i64);
    let (mut held_slot_s, mut idle, mut queued_s) = (0.0, 0.0, 0.0);
    let mut t = lo;
    for (at, dh, dw) in events {
        let dt = at - t;
        held_slot_s += held as f64 * dt;
        if waiting > 0 {
            queued_s += dt;
            if let Some(b) = budget {
                idle += (i64::from(b) - held).max(0) as f64 * dt;
            }
        }
        t = at;
        held += dh;
        waiting += dw;
    }
    let ended: Vec<&Run> = runs
        .iter()
        .filter(|r| !r.rec.nested && period.contains(r.end))
        .collect();
    let (cpu, cpu_slot_s) = ended
        .iter()
        .filter_map(|r| Some((r.rec.cpu_s?, f64::from(r.rec.slots) * r.rec.ran_s)))
        .fold((0.0, 0.0), |(a, b), (c, s)| (a + c, b + s));
    let span = hi - lo;
    let (unclaimed_load, unclaimed_n) = unclaimed_load(&ended);
    Some(Machine {
        since: lo,
        until: hi,
        budget,
        runs: ended.len(),
        waited_s: ended.iter().map(|r| r.rec.waited_s).sum(),
        ran_s: ended.iter().map(|r| r.rec.ran_s).sum(),
        held_slot_s,
        utilisation: budget.map(|b| held_slot_s / (f64::from(b) * span)),
        blocked_idle_slot_s: budget.map(|_| idle),
        queue_nonempty: queued_s / span,
        cpu_efficiency: (cpu_slot_s > 0.0).then(|| cpu / cpu_slot_s),
        unclaimed_load,
        unclaimed_n,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Measured,
    Experiment,
    Oom,
}

impl Tag {
    fn as_str(self) -> &'static str {
        match self {
            Tag::Measured => "measured",
            Tag::Experiment => "experiment",
            Tag::Oom => "oom",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Suggest {
    Slots {
        slots: u32,
        tag: Tag,
        evidence: String,
    },
    /// No suggestion, with the reason (`insufficient data (3 runs)`).
    None(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub rule: Option<String>,
    /// Set on a sub-row split by config hash.
    pub cfg: Option<String>,
    pub runs: usize,
    pub clean: usize,
    pub oom: usize,
    /// The slots of the latest run: the setting now in force.
    pub slots: u32,
    pub wait_s: Option<P5090>,
    pub run_s: Option<P5090>,
    pub cores: Option<P5090>,
    pub mem_peak_mb_p90: Option<f64>,
    pub suggest: Suggest,
}

/// One rule's row. Wait covers every run (queueing doesn't depend on how
/// the job ended); run time, cores and memory cover clean runs only, as the
/// suggestion does (D30).
pub fn row(rule: Option<String>, runs: &[&Run], host: Host) -> Row {
    let clean: Vec<&Run> = runs.iter().copied().filter(|r| r.is_clean()).collect();
    let ooms: Vec<&Run> = runs
        .iter()
        .copied()
        .filter(|r| r.rec.killed == Some(Killed::Oom))
        .collect();
    let latest = runs.last().expect("a row has runs");
    let slots = latest.rec.slots;
    let budget = latest.rec.budget.or(host.budget).unwrap_or(u32::MAX).max(1);
    let waits: Vec<f64> = runs.iter().map(|r| r.rec.waited_s).collect();
    let rans: Vec<f64> = clean.iter().map(|r| r.rec.ran_s).collect();
    let cores: Vec<f64> = clean.iter().filter_map(|r| r.cores()).collect();
    let mems: Vec<f64> = clean
        .iter()
        .filter_map(|r| r.rec.mem_peak_mb.map(|m| m as f64))
        .collect();
    let mem_p90 = percentile(&mems, 90.0);
    // D16a: a container run's scope saw only podman exec and conmon, so its
    // figures say nothing about the job; suggesting from them would shrink
    // the rule to nothing. In a mixed rule only the host runs count, so
    // container runs don't make up D30's minimum of clean runs.
    let suggest = if runs.iter().all(|r| r.rec.enforced_reason.is_some()) {
        Suggest::None(NOT_MEASURED_CONTAINER.into())
    } else {
        let (clean, ooms) = (measured(&clean), measured(&ooms));
        let cores: Vec<f64> = clean.iter().filter_map(|r| r.cores()).collect();
        let mems: Vec<f64> = clean
            .iter()
            .filter_map(|r| r.rec.mem_peak_mb.map(|m| m as f64))
            .collect();
        suggest(SuggestIn {
            clean: &clean,
            ooms: &ooms,
            slots,
            budget,
            cores_p90: percentile(&cores, 90.0),
            mem_p90: percentile(&mems, 90.0),
            mem_per_slot_mb: host.mem_per_slot_mb,
        })
    };
    Row {
        rule,
        cfg: None,
        runs: runs.len(),
        clean: clean.len(),
        oom: ooms.len(),
        slots,
        wait_s: p5090(&waits),
        run_s: p5090(&rans),
        cores: p5090(&cores),
        mem_peak_mb_p90: mem_p90,
        suggest,
    }
}

/// D16a: the runs whose figures describe the job: not container runs.
fn measured<'a>(rs: &[&'a Run]) -> Vec<&'a Run> {
    rs.iter()
        .copied()
        .filter(|r| r.rec.enforced_reason.is_none())
        .collect()
}

/// D16a: the reason given for a rule whose runs all entered a container.
pub const NOT_MEASURED_CONTAINER: &str = "not measured (container)";

struct SuggestIn<'a> {
    clean: &'a [&'a Run],
    ooms: &'a [&'a Run],
    slots: u32,
    budget: u32,
    cores_p90: Option<f64>,
    mem_p90: Option<f64>,
    mem_per_slot_mb: Option<f64>,
}

fn gb(mb: f64) -> String {
    format!("{:.1} GB", mb / 1024.0)
}

/// Spec §report suggestions. Candidates, capped at the budget:
/// - measured: `max(ceil(p90 cores), ceil(p90 mem / mem-per-slot))` over
///   clean runs, after [`MIN_CLEAN_RUNS`] (D30). Memory can only raise it.
/// - experiment: `slots + 2` when cores p50 at the current slots is at
///   least 90% of them (D30a), over at least [`MIN_CLEAN_RUNS`] clean runs
///   at those slots: right after a slot change, the runs at the old
///   setting say nothing about saturation at the new one (D30).
/// - oom: `ceil(1.25 × max oom peak / mem-per-slot)` (D30a). It needs no
///   clean runs: a rule that OOMs every time would otherwise never get 5
///   clean runs, and the kill itself is the evidence.
///
/// The largest wins; on a tie the raising signal is named, since it says
/// more about why.
fn suggest(i: SuggestIn) -> Suggest {
    let n = i.clean.len();
    let cap = |s: u32| s.clamp(1, i.budget);
    let mut out: Vec<(u32, Tag, String)> = Vec::new();
    if n >= MIN_CLEAN_RUNS {
        let by_cores = i.cores_p90.map(|c| c.ceil() as u32);
        let by_mem = i
            .mem_p90
            .zip(i.mem_per_slot_mb)
            .map(|(m, per)| (m / per).ceil() as u32);
        match (by_cores, by_mem) {
            (Some(c), m) if m.is_none_or(|m| c >= m) => out.push((
                cap(c),
                Tag::Measured,
                format!(
                    "p90 cores {:.1} over {n} clean runs",
                    i.cores_p90.unwrap_or_default()
                ),
            )),
            (_, Some(m)) => out.push((
                cap(m),
                Tag::Measured,
                format!(
                    "p90 mem {} over {n} clean runs, {} per slot",
                    gb(i.mem_p90.unwrap_or_default()),
                    gb(i.mem_per_slot_mb.unwrap_or_default())
                ),
            )),
            _ => {}
        }
        let at_slots: Vec<f64> = i
            .clean
            .iter()
            .filter(|r| r.rec.slots == i.slots)
            .filter_map(|r| r.cores())
            .collect();
        if at_slots.len() >= MIN_CLEAN_RUNS
            && let Some(p50) = percentile(&at_slots, 50.0)
            && p50 >= SATURATED * f64::from(i.slots)
        {
            let to = cap(i.slots + EXPERIMENT_STEP);
            // At the budget already there is nothing to try.
            if to > i.slots {
                out.push((
                    to,
                    Tag::Experiment,
                    format!(
                        "saturated at {}: cores p50 {p50:.1} over {} clean runs",
                        i.slots,
                        at_slots.len()
                    ),
                ));
            }
        }
    }
    let oom_peak = i
        .ooms
        .iter()
        .filter_map(|r| r.rec.mem_peak_mb)
        .max()
        .map(|m| m as f64);
    if let (Some(peak), Some(per)) = (oom_peak, i.mem_per_slot_mb) {
        let s = cap((OOM_HEADROOM * peak / per).ceil() as u32);
        out.push((
            s,
            Tag::Oom,
            format!(
                "{} oom runs, max peak {} +25% at {} per slot",
                i.ooms.len(),
                gb(peak),
                gb(per)
            ),
        ));
    }
    let rank = |t: Tag| match t {
        Tag::Measured => 0,
        Tag::Experiment => 1,
        Tag::Oom => 2,
    };
    match out.into_iter().max_by_key(|(s, t, _)| (*s, rank(*t))) {
        Some((slots, tag, evidence)) => Suggest::Slots {
            slots,
            tag,
            evidence,
        },
        None if n < MIN_CLEAN_RUNS => Suggest::None(format!("insufficient data ({n} runs)")),
        None => Suggest::None(format!("no CPU or memory figures ({n} runs)")),
    }
}

fn group<'a, K: Ord>(
    runs: impl Iterator<Item = &'a Run>,
    key: impl Fn(&Run) -> K,
) -> BTreeMap<K, Vec<&'a Run>> {
    let mut by: BTreeMap<K, Vec<&Run>> = BTreeMap::new();
    for r in runs {
        by.entry(key(r)).or_default().push(r);
    }
    by
}

/// Rule rows for runs ending in the period. With `split`, a rule whose
/// runs carry more than one config hash also gets one sub-row per hash
/// (D28, `--compare`).
pub fn rows(
    runs: &[Run],
    period: Period,
    rule: Option<&str>,
    host: Host,
    split: bool,
) -> Vec<(Row, Vec<Row>)> {
    let picked = runs.iter().filter(|r| {
        !r.rec.nested
            && period.contains(r.end)
            && rule.is_none_or(|l| r.rec.rule.as_deref() == Some(l))
    });
    group(picked, |r| r.rec.rule.clone())
        .into_iter()
        .map(|(label, rs)| {
            let subs = group(rs.iter().copied(), |r| r.rec.cfg.clone());
            let subs = if split && subs.len() > 1 {
                subs.into_iter()
                    .map(|(cfg, crs)| Row {
                        cfg: Some(cfg.unwrap_or_default()),
                        ..row(label.clone(), &crs, host)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            (row(label, &rs, host), subs)
        })
        .collect()
}

/// `--compare`: how a rule moved from period A to period C. The experiment
/// verdict (D30a) applies when C ran it with more slots than A: confirmed
/// when the clean run p50 dropped, rejected when it didn't.
#[derive(Debug, Clone, PartialEq)]
pub struct Delta {
    pub rule: Option<String>,
    pub slots: (u32, u32),
    pub run_p50: Option<(f64, f64)>,
    pub wait_p50: Option<(f64, f64)>,
    pub cores_p50: Option<(f64, f64)>,
    pub experiment: Option<&'static str>,
}

pub fn deltas(a: &[(Row, Vec<Row>)], c: &[(Row, Vec<Row>)]) -> Vec<Delta> {
    let both = |x: Option<P5090>, y: Option<P5090>| Some((x?.p50, y?.p50));
    a.iter()
        .filter_map(|(ra, _)| {
            let (rc, _) = c.iter().find(|(rc, _)| rc.rule == ra.rule)?;
            let run_p50 = both(ra.run_s, rc.run_s);
            let experiment = (rc.slots > ra.slots)
                .then_some(run_p50)
                .flatten()
                .map(|(x, y)| if y < x { "confirmed" } else { "rejected" });
            Some(Delta {
                rule: ra.rule.clone(),
                slots: (ra.slots, rc.slots),
                run_p50,
                wait_p50: both(ra.wait_s, rc.wait_s),
                cores_p50: both(ra.cores, rc.cores),
                experiment,
            })
        })
        .collect()
}

// --- output ---------------------------------------------------------------

fn pj(p: Option<P5090>) -> Value {
    p.map_or(Value::Null, |p| json!({"p50": p.p50, "p90": p.p90}))
}

fn suggest_json(s: &Suggest) -> Value {
    match s {
        Suggest::Slots {
            slots,
            tag,
            evidence,
        } => json!({"slots": slots, "tag": tag.as_str(), "evidence": evidence}),
        Suggest::None(reason) => json!({"slots": null, "reason": reason}),
    }
}

fn row_json(r: &Row) -> Value {
    let mut v = json!({
        "rule": r.rule, "runs": r.runs, "clean": r.clean, "oom": r.oom,
        "slots": r.slots, "wait_s": pj(r.wait_s), "run_s": pj(r.run_s),
        "cores": pj(r.cores), "mem_peak_mb_p90": r.mem_peak_mb_p90,
        "suggest": suggest_json(&r.suggest),
    });
    if let Some(cfg) = &r.cfg {
        v["cfg"] = json!(cfg);
    }
    v
}

fn machine_json(m: &Option<Machine>) -> Value {
    let Some(m) = m else { return Value::Null };
    json!({
        "since": fmt_ts(m.since), "until": fmt_ts(m.until), "budget": m.budget,
        "runs": m.runs, "waited_s": m.waited_s, "ran_s": m.ran_s,
        "held_slot_s": m.held_slot_s, "utilisation": m.utilisation,
        "blocked_idle_slot_s": m.blocked_idle_slot_s,
        "queue_nonempty": m.queue_nonempty, "cpu_efficiency": m.cpu_efficiency,
        "unclaimed_load": {
            "p50": m.unclaimed_load.map(|p| p.p50), "p90": m.unclaimed_load.map(|p| p.p90),
            "n": m.unclaimed_n,
        },
    })
}

fn rows_json(rows: &[(Row, Vec<Row>)]) -> Value {
    rows.iter()
        .map(|(r, subs)| {
            let mut v = row_json(r);
            if !subs.is_empty() {
                v["by_cfg"] = subs.iter().map(row_json).collect();
            }
            v
        })
        .collect()
}

fn pct(x: Option<f64>) -> String {
    x.map_or("-".into(), |x| format!("{:.0}%", x * 100.0))
}

fn pair(p: Option<P5090>, unit: &str, prec: usize) -> String {
    p.map_or("-".into(), |p| {
        format!("{:.prec$}/{:.prec$}{unit}", p.p50, p.p90)
    })
}

fn label(rule: &Option<String>) -> &str {
    rule.as_deref().unwrap_or("(no rule)")
}

fn suggest_text(s: &Suggest) -> String {
    match s {
        Suggest::Slots {
            slots,
            tag,
            evidence,
        } if *tag == Tag::Experiment => format!("try {slots} experiment ({evidence})"),
        Suggest::Slots {
            slots,
            tag,
            evidence,
        } => format!("{slots} {} ({evidence})", tag.as_str()),
        Suggest::None(reason) => reason.clone(),
    }
}

fn machine_text(m: &Option<Machine>, out: &mut String) {
    let Some(m) = m else {
        out.push_str("no runs in the period\n");
        return;
    };
    let budget = m.budget.map_or("-".into(), |b| b.to_string());
    out.push_str(&format!(
        "period {} .. {} ({:.1} h)\n\
         runs {}, waited {:.0} s, ran {:.0} s\n\
         utilisation {} of {budget} slots\n\
         blocked-idle {} slot-s\n\
         queue non-empty {} of the time\n\
         CPU efficiency {}\n",
        fmt_ts(m.since),
        fmt_ts(m.until),
        (m.until - m.since) / 3600.0,
        m.runs,
        m.waited_s,
        m.ran_s,
        pct(m.utilisation),
        m.blocked_idle_slot_s
            .map_or("-".into(), |x| format!("{x:.0}")),
        pct(Some(m.queue_nonempty)),
        pct(m.cpu_efficiency),
    ));
    if let Some(p) = m.unclaimed_load {
        out.push_str(&format!(
            "unclaimed load: p50 {:.1}, p90 {:.1} cores\n",
            p.p50, p.p90
        ));
    }
}

fn row_text(r: &Row) -> String {
    let name = match &r.cfg {
        Some(cfg) => format!("  {} cfg {cfg}", label(&r.rule)),
        None => label(&r.rule).to_string(),
    };
    let oom = if r.oom > 0 {
        format!(" ({} oom)", r.oom)
    } else {
        String::new()
    };
    format!(
        "{name}\t{}{oom}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        r.runs,
        r.slots,
        pair(r.wait_s, " s", 0),
        pair(r.run_s, " s", 0),
        pair(r.cores, "", 1),
        r.mem_peak_mb_p90.map_or("-".into(), gb),
        suggest_text(&r.suggest),
    )
}

fn rows_text(rows: &[(Row, Vec<Row>)], out: &mut String) {
    out.push_str(
        "rule\truns\tslots\twait p50/p90\trun p50/p90\tcores p50/p90\tmem peak p90\tsuggest\n",
    );
    for (r, subs) in rows {
        out.push_str(&row_text(r));
        subs.iter().for_each(|s| out.push_str(&row_text(s)));
    }
}

fn hint_text(out: &mut String) {
    out.push_str(&format!(
        "unmatched heavy commands never reach muzzle; find them with: {UNMATCHED_HINT}\n"
    ));
}

fn moved(x: Option<(f64, f64)>, prec: usize) -> String {
    x.map_or("-".into(), |(a, c)| {
        let d = if a > 0.0 {
            format!(" ({:+.0}%)", (c - a) / a * 100.0)
        } else {
            String::new()
        };
        format!("{a:.prec$} -> {c:.prec$}{d}")
    })
}

/// Everything `main` prints, from parsed runs; split out so tests check
/// the text and JSON a user sees.
pub fn render(runs: &[Run], args: &ReportArgs, now: f64, host: Host) -> anyhow::Result<String> {
    let rule = args.rule.as_deref();
    if let Some(cmp) = &args.compare {
        let [a, c] = cmp.as_slice() else {
            bail!("--compare takes two periods")
        };
        let (a, c) = (parse_range(a, now)?, parse_range(c, now)?);
        let (ra, rc) = (
            rows(runs, a, rule, host, true),
            rows(runs, c, rule, host, true),
        );
        let (ma, mc) = (machine(runs, a), machine(runs, c));
        let ds = deltas(&ra, &rc);
        if args.json {
            let dj: Vec<Value> = ds
                .iter()
                .map(|d| {
                    let pr = |x: Option<(f64, f64)>| x.map_or(Value::Null, |(a, c)| json!([a, c]));
                    json!({"rule": d.rule, "slots": [d.slots.0, d.slots.1],
                           "run_p50": pr(d.run_p50), "wait_p50": pr(d.wait_p50),
                           "cores_p50": pr(d.cores_p50), "experiment": d.experiment})
                })
                .collect();
            return Ok(json!({
                "periods": [
                    {"machine": machine_json(&ma), "rules": rows_json(&ra)},
                    {"machine": machine_json(&mc), "rules": rows_json(&rc)},
                ],
                "deltas": dj,
                "unmatched_hint": UNMATCHED_HINT,
            })
            .to_string()
                + "\n");
        }
        let mut out = String::new();
        for (name, m, r) in [("A", &ma, &ra), ("C", &mc, &rc)] {
            out.push_str(&format!("== period {name}\n"));
            machine_text(m, &mut out);
            out.push('\n');
            rows_text(r, &mut out);
            out.push('\n');
        }
        out.push_str("== A -> C\nrule\tslots\trun p50\twait p50\tcores p50\texperiment\n");
        for d in &ds {
            out.push_str(&format!(
                "{}\t{} -> {}\t{}\t{}\t{}\t{}\n",
                label(&d.rule),
                d.slots.0,
                d.slots.1,
                moved(d.run_p50, 0),
                moved(d.wait_p50, 0),
                moved(d.cores_p50, 1),
                d.experiment.unwrap_or("-"),
            ));
        }
        out.push('\n');
        hint_text(&mut out);
        return Ok(out);
    }
    let period = Period {
        since: args
            .since
            .as_deref()
            .map(|s| parse_point(s, now, false))
            .transpose()?,
        until: args
            .until
            .as_deref()
            .map(|s| parse_point(s, now, true))
            .transpose()?,
    };
    // Machine figures are the whole machine's even under --rule: the
    // budget is shared, so one rule's share of it says little alone.
    let m = machine(runs, period);
    let r = rows(runs, period, rule, host, false);
    if args.json {
        return Ok(json!({
            "machine": machine_json(&m),
            "rules": rows_json(&r),
            "unmatched_hint": UNMATCHED_HINT,
        })
        .to_string()
            + "\n");
    }
    let mut out = String::new();
    machine_text(&m, &mut out);
    out.push('\n');
    rows_text(&r, &mut out);
    out.push('\n');
    hint_text(&mut out);
    Ok(out)
}

/// D9 memory per slot and the budget, from the stored state (D17a) when
/// there is one, else this host's reading.
fn host(store: &crate::state::Store) -> Host {
    if let Ok(Some(b)) = store.read().map(|s| s.budget) {
        return Host {
            mem_per_slot_mb: Some(b.ram_per_slot as f64 / (1024.0 * 1024.0)),
            budget: Some(b.budget),
        };
    }
    let cores = crate::proc::logical_cores().ok().filter(|&c| c > 0);
    let ram = crate::proc::total_ram().ok();
    Host {
        mem_per_slot_mb: cores
            .zip(ram)
            .map(|(c, r)| r as f64 / c as f64 / (1024.0 * 1024.0)),
        budget: None,
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

pub fn main(args: &ReportArgs) -> anyhow::Result<ExitCode> {
    let store = crate::state::Store::open_default()?;
    let now = jiff::Timestamp::now().as_millisecond() as f64 / 1000.0;
    let (runs, skipped) = parse(&crate::runlog::read(store.dir(), read_span(args, now))?);
    if skipped > 0 {
        eprintln!("muzzle: report: skipped {skipped} unreadable runs.jsonl lines");
    }
    match render(&runs, args, now, host(&store)) {
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

    const T0: f64 = 1_793_700_000.0; // 2026-11-03-ish, whole seconds.
    const HOST: Host = Host {
        mem_per_slot_mb: Some(2048.0),
        budget: None,
    };

    /// A clean `gate` run on 2 slots under cfg `c1`, budget 8.
    fn run(end: f64, ran_s: f64) -> Run {
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
    fn with_cores(start: f64, slots: u32, cores: &[f64]) -> Vec<Run> {
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

    fn one_row(runs: &[Run]) -> Row {
        let refs: Vec<&Run> = runs.iter().collect();
        row(Some("gate".into()), &refs, HOST)
    }

    fn slots_tag(s: &Suggest) -> (u32, Tag, String) {
        match s {
            Suggest::Slots {
                slots,
                tag,
                evidence,
            } => (*slots, *tag, evidence.clone()),
            Suggest::None(r) => panic!("no suggestion: {r}"),
        }
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let v: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), Some(5.0));
        assert_eq!(percentile(&v, 90.0), Some(9.0));
        assert_eq!(percentile(&v, 100.0), Some(10.0));
        assert_eq!(percentile(&[7.0], 90.0), Some(7.0));
        assert_eq!(percentile(&[], 50.0), None);
        // Unsorted input.
        assert_eq!(percentile(&[3.0, 1.0, 2.0], 50.0), Some(2.0));
    }

    #[test]
    fn cores_used_is_cpu_over_ran() {
        let mut r = run(T0, 200.0);
        r.rec.cpu_s = Some(500.0);
        assert_eq!(r.cores(), Some(2.5));
        r.rec.cpu_s = None;
        assert_eq!(r.cores(), None);
        let mut z = run(T0, 0.0);
        z.rec.cpu_s = Some(1.0);
        assert_eq!(z.cores(), None);
    }

    #[test]
    fn rows_report_percentiles_over_clean_runs() {
        let mut runs = with_cores(T0, 4, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]);
        runs.iter_mut().enumerate().for_each(|(i, r)| {
            r.rec.waited_s = i as f64 * 10.0;
        });
        // A failed run counts for wait but not for run time or cores.
        let mut failed = run(T0 + 99_999.0, 5000.0);
        failed.rec.exit = Some(1);
        failed.rec.waited_s = 1000.0;
        failed.rec.slots = 4;
        runs.push(failed);
        let r = one_row(&runs);
        assert_eq!((r.runs, r.clean), (11, 10));
        assert_eq!(r.cores, Some(P5090 { p50: 5.0, p90: 9.0 }));
        assert_eq!(
            r.run_s,
            Some(P5090 {
                p50: 100.0,
                p90: 100.0
            })
        );
        assert_eq!(
            r.wait_s,
            Some(P5090 {
                p50: 50.0,
                p90: 90.0
            })
        );
    }

    #[test]
    fn four_clean_runs_are_insufficient_five_suggest() {
        let mut runs = with_cores(T0, 4, &[2.0, 2.1, 2.2, 2.4]);
        // Unclean runs don't count toward the threshold.
        let mut failed = run(T0 - 10.0, 100.0);
        failed.rec.exit = Some(1);
        runs.insert(0, failed);
        assert_eq!(
            one_row(&runs).suggest,
            Suggest::None("insufficient data (4 runs)".into())
        );
        runs.extend(with_cores(T0 + 1e6, 4, &[2.3]));
        let (slots, tag, evidence) = slots_tag(&one_row(&runs).suggest);
        assert_eq!((slots, tag), (3, Tag::Measured));
        assert_eq!(evidence, "p90 cores 2.4 over 5 clean runs");
    }

    #[test]
    fn memory_raises_the_suggestion_but_never_lowers_it() {
        let mut runs = with_cores(T0, 4, &[1.0; 5]);
        runs.iter_mut().for_each(|r| r.rec.mem_peak_mb = Some(9000));
        let r = one_row(&runs);
        let (slots, tag, evidence) = slots_tag(&r.suggest);
        // ceil(9000 / 2048) = 5 beats ceil(1.0) = 1.
        assert_eq!((slots, tag), (5, Tag::Measured));
        assert!(evidence.contains("p90 mem 8.8 GB"), "{evidence}");
        // Low memory leaves the cores figure alone.
        let mut runs = with_cores(T0, 4, &[3.0; 5]);
        runs.iter_mut().for_each(|r| r.rec.mem_peak_mb = Some(10));
        assert_eq!(slots_tag(&one_row(&runs).suggest).0, 3);
    }

    #[test]
    fn suggestions_are_capped_at_the_budget() {
        let mut runs = with_cores(T0, 4, &[3.0; 5]);
        runs.iter_mut()
            .for_each(|r| r.rec.mem_peak_mb = Some(100_000));
        assert_eq!(slots_tag(&one_row(&runs).suggest).0, 8);
    }

    #[test]
    fn saturated_rule_gets_an_experiment() {
        // 6 slots, cores p50 5.5 >= 0.9 * 6 = 5.4.
        let runs = with_cores(T0, 6, &[5.5; 5]);
        let (slots, tag, evidence) = slots_tag(&one_row(&runs).suggest);
        assert_eq!((slots, tag), (8, Tag::Experiment));
        assert!(
            evidence.starts_with("saturated at 6: cores p50 5.5"),
            "{evidence}"
        );
        // Just under 90% is measured, not an experiment.
        let runs = with_cores(T0, 6, &[5.3; 5]);
        assert_eq!(slots_tag(&one_row(&runs).suggest).1, Tag::Measured);
        // Below the threshold there is no experiment either.
        let runs = with_cores(T0, 6, &[5.5; 4]);
        assert!(matches!(one_row(&runs).suggest, Suggest::None(_)));
    }

    #[test]
    fn container_rules_are_not_measured_and_get_no_suggestion() {
        use crate::state::EnforcedReason;
        // Saturated and OOMing: every signal would suggest something.
        let mut runs = with_cores(T0, 6, &[5.9; 6]);
        let mut oom = run(T0 + 99_999.0, 10.0);
        oom.rec.killed = Some(Killed::Oom);
        oom.rec.mem_peak_mb = Some(9_000);
        runs.push(oom);
        assert!(matches!(one_row(&runs).suggest, Suggest::Slots { .. }));
        runs.iter_mut()
            .for_each(|r| r.rec.enforced_reason = Some(EnforcedReason::Container));
        let r = one_row(&runs);
        assert_eq!(r.suggest, Suggest::None(NOT_MEASURED_CONTAINER.into()));
        assert!(row_text(&r).contains("not measured (container)"));
        assert_eq!(
            row_json(&r)["suggest"]["reason"],
            "not measured (container)"
        );
        // Host runs in a mixed rule suggest again, counted alone: four
        // host runs are short of the five D30 needs.
        for r in &mut runs[..4] {
            r.rec.enforced_reason = None;
        }
        assert_eq!(
            one_row(&runs).suggest,
            Suggest::None("insufficient data (4 runs)".into())
        );
        runs[4].rec.enforced_reason = None;
        assert!(matches!(one_row(&runs).suggest, Suggest::Slots { .. }));
    }

    #[test]
    fn saturated_includes_exactly_ninety_percent() {
        // D30a says p50 >= 90%: the boundary itself is saturated.
        let mut runs = with_cores(T0, 6, &[1.0; 5]);
        runs.iter_mut().for_each(|r| {
            r.rec.ran_s = 1.0;
            r.rec.cpu_s = Some(SATURATED * 6.0);
        });
        assert_eq!(runs[0].cores(), Some(SATURATED * 6.0));
        assert_eq!(slots_tag(&one_row(&runs).suggest).1, Tag::Experiment);
    }

    #[test]
    fn experiment_needs_five_clean_runs_at_the_current_slots() {
        // The reviewer's repro: 4 runs at 4 slots, then 1 saturated run at
        // 6. Five clean runs in all, but one at 6 slots is no evidence of
        // saturation there (D30).
        let mut runs = with_cores(T0, 4, &[1.0; 4]);
        runs.extend(with_cores(T0 + 1e6, 6, &[5.9]));
        let r = one_row(&runs);
        assert_eq!(r.slots, 6);
        let (slots, tag, evidence) = slots_tag(&r.suggest);
        assert_eq!((slots, tag), (6, Tag::Measured), "{evidence}");
    }

    #[test]
    fn saturation_counts_only_runs_at_the_latest_slots() {
        // 6 runs at 2 slots using 1 core, then 5 at 4 slots using 3.8.
        // At 4 slots p50 3.8 >= 3.6 is saturated. Over all 11 runs p50 is
        // 1.0, and at the first run's 2 slots it is 50%: neither saturates.
        let mut runs = with_cores(T0, 2, &[1.0; 6]);
        runs.extend(with_cores(T0 + 1e6, 4, &[3.8; 5]));
        let r = one_row(&runs);
        assert_eq!(r.slots, 4, "the latest run's slots");
        let (slots, tag, evidence) = slots_tag(&r.suggest);
        assert_eq!((slots, tag), (6, Tag::Experiment));
        assert_eq!(evidence, "saturated at 4: cores p50 3.8 over 5 clean runs");
    }

    #[test]
    fn experiment_is_capped_and_dropped_at_the_budget() {
        let mut runs = with_cores(T0, 7, &[7.0; 5]);
        runs.iter_mut().for_each(|r| r.rec.budget = Some(8));
        assert_eq!(slots_tag(&one_row(&runs).suggest).0, 8);
        assert_eq!(slots_tag(&one_row(&runs).suggest).1, Tag::Experiment);
        runs.iter_mut().for_each(|r| r.rec.budget = Some(7));
        assert_eq!(slots_tag(&one_row(&runs).suggest).1, Tag::Measured);
    }

    #[test]
    fn oom_runs_raise_the_suggestion_and_are_never_clean() {
        let mut runs = with_cores(T0, 2, &[1.0; 5]);
        let mut oom = run(T0 + 1e6, 50.0);
        oom.rec.killed = Some(Killed::Oom);
        oom.rec.exit = Some(0); // even with exit 0, an oom kill is not clean
        oom.rec.mem_peak_mb = Some(8192);
        assert!(!oom.is_clean());
        runs.push(oom);
        let r = one_row(&runs);
        assert_eq!((r.runs, r.clean, r.oom), (6, 5, 1));
        let (slots, tag, evidence) = slots_tag(&r.suggest);
        // ceil(1.25 * 8192 / 2048) = 5.
        assert_eq!((slots, tag), (5, Tag::Oom));
        assert!(
            evidence.contains("1 oom runs, max peak 8.0 GB"),
            "{evidence}"
        );
    }

    #[test]
    fn oom_needs_no_clean_runs() {
        let mut oom = run(T0, 50.0);
        oom.rec.killed = Some(Killed::Oom);
        oom.rec.exit = None;
        oom.rec.mem_peak_mb = Some(4096);
        let r = one_row(&[oom]);
        assert_eq!(r.clean, 0);
        assert_eq!(slots_tag(&r.suggest).0, 3);
    }

    #[test]
    fn other_kills_are_not_clean() {
        for k in [Killed::Timeout, Killed::Signal, Killed::Holder] {
            let mut r = run(T0, 1.0);
            r.rec.killed = Some(k);
            assert!(!r.is_clean(), "{k:?}");
        }
        let mut r = run(T0, 1.0);
        r.rec.exit = None;
        assert!(!r.is_clean());
        assert!(run(T0, 1.0).is_clean());
    }

    #[test]
    fn machine_figures_from_a_timeline() {
        // Budget 4. A: 3 slots, runs 0..100. B: 2 slots, waits 50..100
        // (only 1 slot free), runs 100..200.
        let mut a = run(T0 + 100.0, 100.0);
        a.rec.slots = 3;
        a.rec.budget = Some(4);
        a.rec.cpu_s = Some(150.0);
        let mut b = run(T0 + 200.0, 100.0);
        b.rec.waited_s = 50.0;
        b.rec.budget = Some(4);
        b.rec.cpu_s = Some(200.0);
        // Nested runs are counted in their outer run, never twice.
        let mut nested = run(T0 + 150.0, 50.0);
        nested.rec.nested = true;
        let runs = vec![a, nested, b];
        let m = machine(&runs, Period::default()).unwrap();
        assert_eq!((m.since, m.until), (T0, T0 + 200.0));
        assert_eq!(m.runs, 2);
        assert_eq!(m.held_slot_s, 500.0);
        assert_eq!(m.utilisation, Some(500.0 / 800.0));
        assert_eq!(m.blocked_idle_slot_s, Some(50.0));
        assert_eq!(m.queue_nonempty, 0.25);
        assert_eq!(m.cpu_efficiency, Some(350.0 / 500.0));
        assert_eq!((m.waited_s, m.ran_s), (50.0, 200.0));
    }

    #[test]
    fn machine_figures_clip_to_the_period() {
        let mut a = run(T0 + 100.0, 100.0);
        a.rec.slots = 4;
        a.rec.budget = Some(4);
        let p = Period {
            since: Some(T0 + 50.0),
            until: Some(T0 + 150.0),
        };
        let m = machine(&[a], p).unwrap();
        assert_eq!(m.held_slot_s, 200.0);
        assert_eq!(m.utilisation, Some(0.5));
    }

    #[test]
    fn machine_figures_clip_waits_to_the_period() {
        // Budget 4, nothing else held. Waits T0-50..T0+50, then holds 4
        // slots T0+50..T0+110.
        let mut a = run(T0 + 110.0, 60.0);
        a.rec.slots = 4;
        a.rec.budget = Some(4);
        a.rec.waited_s = 100.0;
        let runs = [a];
        // The wait crosses `since`: 50 s of it fall in the 110 s period.
        let since = Period {
            since: Some(T0),
            until: None,
        };
        let m = machine(&runs, since).unwrap();
        assert_eq!(m.held_slot_s, 240.0);
        assert_eq!(m.queue_nonempty, 50.0 / 110.0);
        assert_eq!(m.blocked_idle_slot_s, Some(200.0));
        // The wait crosses `until`: 50 s of it fall in the 100 s period.
        let until = Period {
            since: Some(T0 - 100.0),
            until: Some(T0),
        };
        let m = machine(&runs, until).unwrap();
        assert_eq!(m.held_slot_s, 0.0);
        assert_eq!(m.queue_nonempty, 0.5);
        assert_eq!(m.blocked_idle_slot_s, Some(200.0));
    }

    #[test]
    fn compare_splits_by_config_hash_and_judges_the_experiment() {
        let a = with_cores(T0, 6, &[5.5; 5]);
        let mut c = with_cores(T0 + 1e6, 8, &[6.0; 6]);
        c.iter_mut().for_each(|r| r.rec.ran_s = 80.0);
        c[3..]
            .iter_mut()
            .for_each(|r| r.rec.cfg = Some("c2".into()));
        let runs: Vec<Run> = a.into_iter().chain(c).collect();
        let pa = Period {
            since: None,
            until: Some(T0 + 5e5),
        };
        let pc = Period {
            since: Some(T0 + 5e5),
            until: None,
        };
        let ra = rows(&runs, pa, None, HOST, true);
        let rc = rows(&runs, pc, None, HOST, true);
        assert!(ra[0].1.is_empty(), "one hash in A: no split");
        let cfgs: Vec<_> = rc[0]
            .1
            .iter()
            .map(|r| (r.cfg.clone().unwrap(), r.runs))
            .collect();
        assert_eq!(cfgs, [("c1".to_string(), 3), ("c2".to_string(), 3)]);
        let d = &deltas(&ra, &rc)[0];
        assert_eq!(d.slots, (6, 8));
        assert_eq!(d.run_p50, Some((100.0, 80.0)));
        assert_eq!(d.experiment, Some("confirmed"));
        // No faster: rejected.
        let mut slow = runs.clone();
        slow.iter_mut()
            .filter(|r| r.rec.slots == 8)
            .for_each(|r| r.rec.ran_s = 100.0);
        let rc = rows(&slow, pc, None, HOST, true);
        assert_eq!(deltas(&ra, &rc)[0].experiment, Some("rejected"));
    }

    fn args(json: bool) -> ReportArgs {
        ReportArgs {
            rule: None,
            since: None,
            until: None,
            compare: None,
            json,
        }
    }

    #[test]
    fn text_names_evidence_insufficient_data_and_the_hint() {
        let mut runs = with_cores(T0, 4, &[2.0, 2.4, 2.4, 2.0, 2.0]);
        let mut few = with_cores(T0, 2, &[1.0; 3]);
        few.iter_mut()
            .for_each(|r| r.rec.rule = Some("cargo-test".into()));
        runs.extend(few);
        runs.sort_by(|a, b| a.end.total_cmp(&b.end));
        let out = render(&runs, &args(false), T0 + 1e6, HOST).unwrap();
        assert!(
            out.contains("3 measured (p90 cores 2.4 over 5 clean runs)"),
            "{out}"
        );
        assert!(out.contains("insufficient data (3 runs)"), "{out}");
        assert!(out.contains(UNMATCHED_HINT), "{out}");
        assert!(out.contains("utilisation "), "{out}");
    }

    /// P12 fixture: run `i` has load `i + 2`. Runs 1..=15 have 30 slots
    /// held by others, more than the load, so they clamp to 0; later runs
    /// leave `i` cores unclaimed. Plus one 0.1 line without the fields,
    /// which doesn't count.
    fn loaded(n: usize) -> Vec<Run> {
        let mut runs: Vec<Run> = (1..=n)
            .map(|i| {
                let mut r = run(T0 + 100.0 * i as f64, 10.0);
                r.rec.load1 = Some(i as f64 + 2.0);
                r.rec.held = Some(if i <= 15 { 30 } else { 2 });
                r
            })
            .collect();
        runs.push(run(T0 + 100.0 * (n + 1) as f64, 10.0));
        runs
    }

    #[test]
    fn unclaimed_load_over_twenty_five_runs() {
        let runs = loaded(25);
        let m = machine(&runs, Period::default()).unwrap();
        // Fifteen 0s, then 16..=25: nearest rank 13 and 23.
        assert_eq!(
            m.unclaimed_load,
            Some(P5090 {
                p50: 0.0,
                p90: 23.0
            })
        );
        assert_eq!(m.unclaimed_n, 25);
        let text = render(&runs, &args(false), T0 + 1e6, HOST).unwrap();
        assert!(
            text.contains("unclaimed load: p50 0.0, p90 23.0 cores\n"),
            "{text}"
        );
        let v: Value =
            serde_json::from_str(&render(&runs, &args(true), T0 + 1e6, HOST).unwrap()).unwrap();
        assert_eq!(
            v["machine"]["unclaimed_load"],
            json!({"p50": 0.0, "p90": 23.0, "n": 25})
        );
    }

    #[test]
    fn unclaimed_load_needs_twenty_runs() {
        let runs = loaded(19);
        let m = machine(&runs, Period::default()).unwrap();
        assert_eq!((m.unclaimed_load, m.unclaimed_n), (None, 19));
        let text = render(&runs, &args(false), T0 + 1e6, HOST).unwrap();
        assert!(!text.contains("unclaimed"), "{text}");
        let v: Value =
            serde_json::from_str(&render(&runs, &args(true), T0 + 1e6, HOST).unwrap()).unwrap();
        assert_eq!(
            v["machine"]["unclaimed_load"],
            json!({"p50": null, "p90": null, "n": 19})
        );
        let twenty = machine(&loaded(20), Period::default()).unwrap();
        assert_eq!(twenty.unclaimed_n, 20);
        assert!(twenty.unclaimed_load.is_some());
    }

    #[test]
    fn json_tags_every_suggestion() {
        let runs = with_cores(T0, 6, &[5.5; 5]);
        let out = render(&runs, &args(true), T0 + 1e6, HOST).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let s = &v["rules"][0]["suggest"];
        assert_eq!(s["tag"], "experiment");
        assert_eq!(s["slots"], 8);
        assert!(s["evidence"].as_str().unwrap().contains("saturated at 6"));
        assert_eq!(v["unmatched_hint"], UNMATCHED_HINT);
        assert!(v["machine"]["utilisation"].is_number());
    }

    #[test]
    fn rule_filter_and_since_until() {
        let mut runs = with_cores(T0, 4, &[1.0; 3]);
        runs[0].rec.rule = Some("other".into());
        let mut a = args(true);
        a.rule = Some("gate".into());
        a.since = Some(fmt_ts(T0 + 3600.0));
        a.until = Some(fmt_ts(T0 + 3601.0));
        let v: Value = serde_json::from_str(&render(&runs, &a, T0 + 1e6, HOST).unwrap()).unwrap();
        assert_eq!(v["rules"].as_array().unwrap().len(), 1);
        assert_eq!(v["rules"][0]["runs"], 1);
    }

    #[test]
    fn parse_skips_torn_and_newer_lines() {
        let good = serde_json::to_string(&run(T0, 5.0).rec).unwrap();
        let newer = good.replacen("\"v\":1", "\"v\":2", 1);
        let text = format!("{good}\n{{\"v\":1,\"end\":\n\n{newer}\n{good}\n");
        let (runs, skipped) = parse(&text);
        assert_eq!((runs.len(), skipped), (2, 2));
        assert_eq!(runs[0].end, T0);
    }

    #[test]
    fn points_and_ranges() {
        let now = T0;
        assert_eq!(parse_point("now", now, false).unwrap(), now);
        assert_eq!(parse_point("7d", now, false).unwrap(), now - 7.0 * 86400.0);
        assert_eq!(parse_point("12h", now, false).unwrap(), now - 12.0 * 3600.0);
        let day = parse_point("2026-11-03", now, false).unwrap();
        assert_eq!(fmt_ts(day), "2026-11-03T00:00:00Z");
        assert_eq!(parse_point("2026-11-03", now, true).unwrap(), day + 86400.0);
        assert_eq!(
            parse_point("2026-11-03T10:12:44Z", now, false).unwrap(),
            day + 36764.0
        );
        assert!(parse_point("yesterday", now, false).is_err());
        let p = parse_range("2026-11-01..2026-11-03", now).unwrap();
        assert_eq!(p.until, Some(day + 86400.0));
        assert_eq!(parse_range("..now", now).unwrap().since, None);
        assert!(parse_range("2026-11-01", now).is_err());
    }
}
