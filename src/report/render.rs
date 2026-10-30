//! Report output: text and JSON, for one period or a `--compare`.

use anyhow::bail;
use serde_json::{Value, json};

use super::{
    Host, Machine, P5090, Period, Row, Run, Suggest, Tag, UNMATCHED_HINT, deltas, fmt_ts, gb,
    machine, parse_point, parse_range, rows,
};
use crate::cli::ReportArgs;

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

pub(super) fn row_json(r: &Row) -> Value {
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

pub(super) fn machine_json(m: &Option<Machine>) -> Value {
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

pub(super) fn pair(p: Option<P5090>, unit: &str, prec: usize) -> String {
    p.map_or("-".into(), |p| {
        format!("{:.prec$}/{:.prec$}{unit}", p.p50, p.p90)
    })
}

pub(super) fn label(rule: &Option<String>) -> &str {
    rule.as_deref().unwrap_or("(no rule)")
}

pub(super) fn suggest_text(s: &Suggest) -> String {
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

pub(super) fn machine_text(m: &Option<Machine>, out: &mut String) {
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

pub(super) fn row_text(r: &Row) -> String {
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

pub(super) fn hint_text(out: &mut String) {
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

/// `--since`/`--until` as a period.
pub(super) fn period(args: &ReportArgs, now: f64) -> anyhow::Result<Period> {
    let point =
        |s: &Option<String>, upper| s.as_deref().map(|s| parse_point(s, now, upper)).transpose();
    Ok(Period {
        since: point(&args.since, false)?,
        until: point(&args.until, true)?,
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
    let period = period(args, now)?;
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

#[cfg(test)]
mod tests {
    use super::super::tests::{HOST, T0, args, with_cores};
    use super::*;

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
}
