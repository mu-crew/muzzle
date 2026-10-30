//! Per-rule rows, their suggestions (D30, D30a) and `--compare` deltas.

use std::collections::BTreeMap;

use super::{
    EXPERIMENT_STEP, Host, MIN_CLEAN_RUNS, OOM_HEADROOM, P5090, Period, Run, SATURATED, p5090,
    percentile,
};
use crate::state::Killed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Measured,
    Experiment,
    Oom,
}

impl Tag {
    pub(super) fn as_str(self) -> &'static str {
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

pub(super) fn gb(mb: f64) -> String {
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

pub(super) fn group<'a, K: Ord>(
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

#[cfg(test)]
mod tests {
    use super::super::tests::{HOST, T0, one_row, run, slots_tag, with_cores};
    use super::*;
    use crate::report::render::{row_json, row_text};

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
}
