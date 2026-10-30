//! D41 machine figures over a period.

use super::{MIN_UNCLAIMED_RUNS, P5090, Period, Run, p5090};

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

#[cfg(test)]
mod tests {
    use super::super::tests::{HOST, T0, args, run};
    use super::*;
    use crate::report::render;
    use serde_json::{Value, json};

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
}
