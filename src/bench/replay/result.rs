//! result.json and summary.md.

use serde_json::{Value, json};

use super::super::probe::{self, Burst};
use super::sim::{Outcome, max_held};
use super::trace::Header;
use crate::cli::{BenchMode, BenchTier};
use crate::report::percentile;

fn p5090(v: &[f64]) -> Value {
    let r = |p| percentile(v, p).map(probe::round2);
    json!({"p50": r(50.0), "p90": r(90.0)})
}

pub(super) struct Figures<'a> {
    pub(super) mode: BenchMode,
    pub(super) tier: BenchTier,
    pub(super) speed: f64,
    pub(super) window: Option<&'a str>,
    pub(super) window_start: Option<f64>,
    pub(super) header: &'a Header,
    pub(super) light: usize,
    pub(super) outcomes: &'a [Outcome],
    pub(super) makespan: f64,
    pub(super) bursts: &'a [Burst],
    pub(super) runs: &'a str,
    pub(super) budget: Option<u32>,
    pub(super) cfg: String,
}

/// result.json: the D35 figures. Times are wall seconds at `speed`. No
/// paths or command text: results are committed to `docs/bench/` (D32).
pub(super) fn result_json(f: &Figures) -> Value {
    let o = f.outcomes;
    // D35 (amended 2026-10-30): `wait_s`/`run_s` cover every call, as in
    // 0.1 results. An unmatched-heavy call never queues (wait 0), so they
    // understate muzzle's wait by the trace's unmatched share; the
    // `matched_` figures leave those calls out. Fields are only added.
    let wait = |c: &Outcome| c.admitted - c.start;
    let run = |c: &Outcome| c.end - c.admitted;
    let waits: Vec<f64> = o.iter().map(wait).collect();
    let runs: Vec<f64> = o.iter().map(run).collect();
    let matched_waits: Vec<f64> = o.iter().filter(|c| c.matched).map(wait).collect();
    let matched_runs: Vec<f64> = o.iter().filter(|c| c.matched).map(run).collect();
    let held: Vec<(f64, f64, u32)> = o
        .iter()
        .filter(|c| c.matched)
        .map(|c| match c.held {
            Some((a, r)) => (a, r, c.slots),
            // Mode none, or no report: the bench's own view.
            None => (c.admitted, c.end, c.slots),
        })
        .collect();
    // D30a: the scope's oom-kill under muzzle; without a scope, a SIGKILL
    // is the kernel OOM killer's signature.
    let oom_lines = f
        .runs
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["killed"] == "oom")
        .count();
    let oom = oom_lines + o.iter().filter(|c| c.sigkilled()).count();
    let git: Vec<f64> = f.bursts.iter().flat_map(|b| b.git_ms.clone()).collect();
    let late: Vec<f64> = f.bursts.iter().flat_map(|b| b.late_ms.clone()).collect();
    let load1: Vec<f64> = f
        .bursts
        .iter()
        .filter_map(|b| b.load.map(|l| l[0]))
        .collect();
    let jobs_per_hour = if f.makespan > 0.0 {
        probe::round2(o.len() as f64 * 3600.0 / f.makespan)
    } else {
        0.0
    };
    json!({
        "v": 1,
        "mode": format!("{:?}", f.mode).to_lowercase(),
        "tier": format!("{:?}", f.tier).to_lowercase(),
        "speed": f.speed,
        "window": f.window,
        "window_start_s": f.window_start,
        "trace": {
            "host": f.header.host,
            "date": f.header.date,
            "cores": f.header.cores,
            "ram_gb": f.header.ram_gb,
            "window_s": f.header.window_s,
            "calls_kept": f.header.calls_kept,
        },
        "machine": {
            "cores": crate::proc::logical_cores().ok(),
            "budget": f.budget,
            "cfg": f.cfg,
        },
        "calls": {
            "replayed": o.len(),
            "matched": o.iter().filter(|c| c.matched).count(),
            "unmatched_heavy": o.iter().filter(|c| !c.matched).count(),
            "light_skipped": f.light,
            "failed": o.iter().filter(|c| c.failed()).count(),
        },
        "makespan_s": probe::round2(f.makespan),
        "jobs_per_hour": jobs_per_hour,
        "wait_s": p5090(&waits),
        "run_s": p5090(&runs),
        "matched_wait_s": p5090(&matched_waits),
        "matched_run_s": p5090(&matched_runs),
        "max_slots_held": max_held(&held),
        "probe": {
            "bursts": f.bursts.len(),
            "git_status_p95_ms": percentile(&git, 95.0).map(probe::round2),
            "sleep_late_p99_ms": percentile(&late, 99.0).map(probe::round2),
        },
        "load": {
            "samples": load1.len(),
            "load1_p50": percentile(&load1, 50.0).map(probe::round2),
            "load1_max": load1.iter().copied().reduce(f64::max).map(probe::round2),
        },
        "oom_kills": oom,
    })
}

pub(super) fn summary(r: &Value) -> String {
    let s = |v: &Value| match v {
        Value::Null => "-".to_string(),
        v => v.to_string(),
    };
    format!(
        "# muzzle bench: mode {mode}, tier {tier}\n\n\
         Trace {date} ({cores} cores), speed {speed}, window {window}. \
         Times are wall seconds at that speed.\n\n\
         | figure | value |\n|---|---|\n\
         | calls replayed (light skipped) | {replayed} ({light}) |\n\
         | failed | {failed} |\n\
         | makespan | {makespan} s |\n\
         | throughput | {jph} jobs/h |\n\
         | wait p50/p90, matched calls | {mw50}/{mw90} s |\n\
         | run p50/p90, matched calls | {mr50}/{mr90} s |\n\
         | wait p50/p90, all calls | {w50}/{w90} s |\n\
         | run p50/p90, all calls | {r50}/{r90} s |\n\
         | most slots held | {held} (budget {budget}) |\n\
         | git status p95 | {git} ms |\n\
         | 10 ms sleep late p99 | {late} ms |\n\
         | load1 p50/max | {l50}/{lmax} |\n\
         | OOM kills | {oom} |\n",
        mode = r["mode"].as_str().unwrap_or("-"),
        tier = r["tier"].as_str().unwrap_or("-"),
        date = r["trace"]["date"].as_str().unwrap_or("-"),
        cores = s(&r["trace"]["cores"]),
        speed = s(&r["speed"]),
        window = r["window"].as_str().unwrap_or("all"),
        replayed = s(&r["calls"]["replayed"]),
        light = s(&r["calls"]["light_skipped"]),
        failed = s(&r["calls"]["failed"]),
        makespan = s(&r["makespan_s"]),
        jph = s(&r["jobs_per_hour"]),
        mw50 = s(&r["matched_wait_s"]["p50"]),
        mw90 = s(&r["matched_wait_s"]["p90"]),
        mr50 = s(&r["matched_run_s"]["p50"]),
        mr90 = s(&r["matched_run_s"]["p90"]),
        w50 = s(&r["wait_s"]["p50"]),
        w90 = s(&r["wait_s"]["p90"]),
        r50 = s(&r["run_s"]["p50"]),
        r90 = s(&r["run_s"]["p90"]),
        held = s(&r["max_slots_held"]),
        budget = s(&r["machine"]["budget"]),
        git = s(&r["probe"]["git_status_p95_ms"]),
        late = s(&r["probe"]["sleep_late_p99_ms"]),
        l50 = s(&r["load"]["load1_p50"]),
        lmax = s(&r["load"]["load1_max"]),
        oom = s(&r["oom_kills"]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under muzzle the held figure uses muzzle's locked times, not the
    /// bench's spawn-relative estimate, which can overlap.
    #[test]
    fn held_slots_prefer_muzzles_own_times() {
        let header = Header {
            host: "linuxpc".into(),
            cores: 16,
            ram_gb: 31,
            date: "2026-10-08".into(),
            window_s: 10.0,
            calls_kept: 2,
        };
        let o = |admitted: f64, end: f64, held| Outcome {
            matched: true,
            slots: 3,
            start: 0.0,
            admitted,
            end,
            held,
            status: None,
        };
        // Estimates overlap by 0.5 s; muzzle's own times hand over at 1.2 s.
        let outcomes = [o(0.0, 1.5, Some((0.1, 1.2))), o(1.0, 2.0, Some((1.2, 2.1)))];
        let r = result_json(&Figures {
            mode: BenchMode::Muzzle,
            tier: BenchTier::Synthetic,
            speed: 1.0,
            window: None,
            window_start: None,
            header: &header,
            light: 0,
            outcomes: &outcomes,
            makespan: 2.1,
            bursts: &[],
            runs: "",
            budget: Some(4),
            cfg: "abc".into(),
        });
        assert_eq!(r["max_slots_held"], 3);
    }

    #[test]
    fn result_has_every_d35_field() {
        let header = Header {
            host: "linuxpc".into(),
            cores: 16,
            ram_gb: 31,
            date: "2026-10-08".into(),
            window_s: 86400.0,
            calls_kept: 2,
        };
        let o = |start: f64, admitted: f64, end: f64, slots| Outcome {
            matched: true,
            slots,
            start,
            admitted,
            end,
            held: None,
            status: None,
        };
        let outcomes = [o(0.0, 0.0, 2.0, 4), o(1.0, 2.0, 3.0, 4)];
        let bursts = [Burst {
            git_ms: vec![1.0, 2.0],
            late_ms: vec![0.1, 0.3],
            load: Some([1.0, 1.0, 1.0]),
        }];
        let r = result_json(&Figures {
            mode: BenchMode::Muzzle,
            tier: BenchTier::Synthetic,
            speed: 10.0,
            window: None,
            window_start: None,
            header: &header,
            light: 1,
            outcomes: &outcomes,
            makespan: 3.0,
            bursts: &bursts,
            runs: "{\"killed\":\"oom\"}\n{\"killed\":null}\n",
            budget: Some(4),
            cfg: "abc".into(),
        });
        assert_eq!(r["makespan_s"], 3.0);
        assert_eq!(r["jobs_per_hour"], 2400.0);
        assert_eq!(r["wait_s"]["p90"], 1.0);
        assert_eq!(r["run_s"]["p50"], 1.0);
        assert_eq!(r["max_slots_held"], 4);
        assert_eq!(r["probe"]["git_status_p95_ms"], 2.0);
        assert_eq!(r["probe"]["sleep_late_p99_ms"], 0.3);
        assert_eq!(r["load"]["load1_max"], 1.0);
        assert_eq!(r["oom_kills"], 1);
        assert_eq!(r["calls"]["failed"], 2);
        assert!(summary(&r).contains("| makespan | 3.0 s |"));
    }

    /// f-replay-wait: an unmatched-heavy call never queues, so its wait is
    /// always 0 and drags the all-call wait toward it. The matched figures
    /// leave it out (D35, amended 2026-10-30).
    #[test]
    fn matched_wait_leaves_out_unmatched_calls() {
        let header = Header {
            host: "linuxpc".into(),
            cores: 16,
            ram_gb: 31,
            date: "2026-10-08".into(),
            window_s: 10.0,
            calls_kept: 3,
        };
        let o = |matched, admitted: f64, end: f64| Outcome {
            matched,
            slots: 1,
            start: 0.0,
            admitted,
            end,
            held: None,
            status: None,
        };
        let outcomes = [o(true, 4.0, 5.0), o(false, 0.0, 9.0), o(false, 0.0, 9.0)];
        let r = result_json(&Figures {
            mode: BenchMode::Muzzle,
            tier: BenchTier::Synthetic,
            speed: 1.0,
            window: None,
            window_start: None,
            header: &header,
            light: 0,
            outcomes: &outcomes,
            makespan: 9.0,
            bursts: &[],
            runs: "",
            budget: Some(4),
            cfg: "abc".into(),
        });
        assert_eq!(r["wait_s"]["p50"], 0.0, "all calls, as before");
        assert_eq!(r["matched_wait_s"]["p50"], 4.0);
        assert_eq!(r["matched_run_s"]["p50"], 1.0);
        let s = summary(&r);
        assert!(
            s.contains("| wait p50/p90, matched calls | 4.0/4.0 s |"),
            "{s}"
        );
        assert!(s.contains("| wait p50/p90, all calls | 0.0/4.0 s |"), "{s}");
    }
}
