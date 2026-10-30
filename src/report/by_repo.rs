//! `report --by repo` (0.2 P9, B6): rows per (rule, repo), each set against
//! what the **current** config gives that repo now. runs.jsonl carries no
//! provenance (spec §Scope), so the origin is resolved at report time.

use std::path::Path;

use serde_json::{Value, json};

use super::{
    Host, Machine, Period, Row, Run, Suggest, UNMATCHED_HINT, gb, group, hint_text, label,
    machine_json, machine_text, pair, row, row_json, suggest_text,
};
use crate::rules::config::Config;
use crate::rules::resolve::Origin;

/// B6: the line for a rule not active at that repo; it gets no suggestion.
pub const NOT_HERE: &str = "not in the current config here";

/// What the current config says for a row's rule at its repo.
#[derive(Debug, Clone, PartialEq)]
pub enum Now {
    /// The rule is active there with these slots, from there.
    Slots(u32, Origin),
    /// B6: disabled there, or defined only in a table that doesn't apply.
    NotHere,
    /// Runs without a rule label (`run --slots`): nothing to resolve.
    NoRule,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepoRow {
    /// `None` shows as `(none)`: the run's cwd was in no repo.
    pub repo: Option<String>,
    pub now: Now,
    /// `row.slots` is the latest run's slots: the "runs used" figure.
    pub row: Row,
}

/// Rows for runs ending in the period, grouped by (rule, repo). The
/// 5-clean-run minimum and the suggestion are [`row`]'s, unchanged (P9).
pub fn rows(
    runs: &[Run],
    period: Period,
    rule: Option<&str>,
    host: Host,
    config: &Config,
) -> Vec<RepoRow> {
    let picked = runs.iter().filter(|r| {
        !r.rec.nested
            && period.contains(r.end)
            && rule.is_none_or(|l| r.rec.rule.as_deref() == Some(l))
    });
    group(picked, |r| (r.rec.rule.clone(), r.rec.repo.clone()))
        .into_iter()
        .map(|((name, repo), rs)| {
            // B6: a repo is already a P2 target; `(none)` gets the global
            // list alone.
            let eff = config.resolve_target(repo.as_deref().map(Path::new));
            let now = match name.as_deref() {
                None => Now::NoRule,
                Some(n) => eff.rule(n).map_or(Now::NotHere, |m| {
                    Now::Slots(m.rule.slots, m.slots_from.clone())
                }),
            };
            let mut row = row(name, &rs, host);
            if now == Now::NotHere {
                row.suggest = Suggest::None(NOT_HERE.into());
            }
            RepoRow { repo, now, row }
        })
        .collect()
}

fn file_label(o: &Origin) -> String {
    o.file.file_name().map_or_else(
        || o.file.display().to_string(),
        |f| f.to_string_lossy().into(),
    )
}

/// P9: `[path."~/hacking/crew"] in machine.toml`, or the file's global list.
pub fn origin_text(o: &Origin) -> String {
    match &o.path {
        Some(key) => format!("[path.\"{key}\"] in {}", file_label(o)),
        None => format!("[[rule]] in {}", file_label(o)),
    }
}

/// B6: `now 2 (from …)`, plus `; runs used 4` when the runs differ.
pub fn now_text(r: &RepoRow) -> String {
    match &r.now {
        Now::Slots(n, o) => {
            let mut s = format!("now {n} (from {})", origin_text(o));
            if *n != r.row.slots {
                s.push_str(&format!("; runs used {}", r.row.slots));
            }
            s
        }
        Now::NotHere | Now::NoRule => "-".into(),
    }
}

fn repo_label(repo: &Option<String>) -> &str {
    repo.as_deref().unwrap_or("(none)")
}

fn row_text(r: &RepoRow) -> String {
    let w = &r.row;
    let oom = if w.oom > 0 {
        format!(" ({} oom)", w.oom)
    } else {
        String::new()
    };
    format!(
        "{}\t{}\t{}{oom}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        label(&w.rule),
        repo_label(&r.repo),
        w.runs,
        pair(w.wait_s, " s", 0),
        pair(w.run_s, " s", 0),
        pair(w.cores, "", 1),
        w.mem_peak_mb_p90.map_or("-".into(), gb),
        now_text(r),
        suggest_text(&w.suggest),
    )
}

/// Added fields on top of a rule row (P9): `repo`, `now_slots`,
/// `slots_from` and `runs_slots`.
fn row_json_by_repo(r: &RepoRow) -> Value {
    let mut v = row_json(&r.row);
    let (now_slots, slots_from) = match &r.now {
        Now::Slots(n, o) => (
            json!(n),
            json!({"file": o.file.to_string_lossy(), "path": o.path}),
        ),
        Now::NotHere | Now::NoRule => (Value::Null, Value::Null),
    };
    v["repo"] = json!(r.repo);
    v["now_slots"] = now_slots;
    v["slots_from"] = slots_from;
    v["runs_slots"] = json!(r.row.slots);
    v
}

/// The whole `--by repo` output: the machine section is the same as
/// without `--by`, the rows are per (rule, repo).
pub fn render(m: &Option<Machine>, rows: &[RepoRow], json_out: bool) -> String {
    if json_out {
        return json!({
            "machine": machine_json(m),
            "rules": rows.iter().map(row_json_by_repo).collect::<Vec<_>>(),
            "unmatched_hint": UNMATCHED_HINT,
        })
        .to_string()
            + "\n";
    }
    let mut out = String::new();
    machine_text(m, &mut out);
    out.push('\n');
    out.push_str(
        "rule\trepo\truns\twait p50/p90\trun p50/p90\tcores p50/p90\tmem peak p90\tnow\tsuggest\n",
    );
    rows.iter().for_each(|r| out.push_str(&row_text(r)));
    out.push('\n');
    hint_text(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::{HOST, T0, run};
    use super::*;

    /// Global `gate` at 3 in config.toml; `/r/crew` reweights it to 2 and
    /// `/r/off` disables it. Keys don't exist, so they stay as written.
    fn config() -> Config {
        Config::parse(
            r#"
[[rule]]
name = "gate"
match = 'gate'
slots = 3

[[path."/r/crew".rule]]
name = "gate"
slots = 2

[[path."/r/off".rule]]
name = "gate"
enabled = false
"#,
            Path::new("/cfg/config.toml"),
        )
        .unwrap()
    }

    /// `n` clean `gate` runs at `slots` in `repo`, 2 cores each.
    fn runs_in(repo: Option<&str>, slots: u32, n: usize, at: f64) -> Vec<Run> {
        (0..n)
            .map(|i| {
                let mut r = run(at + 3600.0 * i as f64, 100.0);
                r.rec.repo = repo.map(Into::into);
                r.rec.slots = slots;
                r.rec.cpu_s = Some(200.0);
                r
            })
            .collect()
    }

    fn fixture() -> Vec<Run> {
        let mut runs = runs_in(Some("/r/crew"), 4, 5, T0);
        runs.extend(runs_in(Some("/r/plain"), 3, 5, T0 + 10.0));
        runs.extend(runs_in(Some("/r/off"), 3, 5, T0 + 20.0));
        runs.extend(runs_in(None, 3, 2, T0 + 30.0));
        let mut bare = run(T0 + 40.0, 10.0);
        bare.rec.rule = None;
        runs.push(bare);
        runs.sort_by(|a, b| a.end.total_cmp(&b.end));
        runs
    }

    fn find<'a>(rows: &'a [RepoRow], rule: Option<&str>, repo: Option<&str>) -> &'a RepoRow {
        rows.iter()
            .find(|r| r.row.rule.as_deref() == rule && r.repo.as_deref() == repo)
            .unwrap_or_else(|| panic!("no row {rule:?} {repo:?}"))
    }

    #[test]
    fn rows_resolve_the_current_config_per_repo() {
        let rows = rows(&fixture(), Period::default(), None, HOST, &config());
        assert_eq!(rows.len(), 5);

        let crew = find(&rows, Some("gate"), Some("/r/crew"));
        assert_eq!(
            now_text(crew),
            "now 2 (from [path.\"/r/crew\"] in config.toml); runs used 4"
        );
        // The suggestion is the history's (2 cores p90), as without --by.
        assert!(
            matches!(crew.row.suggest, Suggest::Slots { slots: 2, .. }),
            "{crew:?}"
        );

        let plain = find(&rows, Some("gate"), Some("/r/plain"));
        assert_eq!(now_text(plain), "now 3 (from [[rule]] in config.toml)");

        let off = find(&rows, Some("gate"), Some("/r/off"));
        assert_eq!(off.now, Now::NotHere);
        assert_eq!(off.row.suggest, Suggest::None(NOT_HERE.into()));

        // `(none)` resolves the global list; the clean-run minimum holds.
        let none = find(&rows, Some("gate"), None);
        assert_eq!(now_text(none), "now 3 (from [[rule]] in config.toml)");
        assert_eq!(
            none.row.suggest,
            Suggest::None("insufficient data (2 runs)".into())
        );

        assert_eq!(find(&rows, None, None).now, Now::NoRule);
    }

    #[test]
    fn rule_filter_and_period_apply() {
        let runs = fixture();
        let rows = rows(&runs, Period::default(), Some("gate"), HOST, &config());
        assert!(rows.iter().all(|r| r.row.rule.as_deref() == Some("gate")));
        let late = Period {
            since: Some(T0 + 4.0 * 3600.0),
            until: None,
        };
        let rows = super::rows(&runs, late, None, HOST, &config());
        assert_eq!(find(&rows, Some("gate"), Some("/r/crew")).row.runs, 1);
    }

    #[test]
    fn text_and_json_carry_repo_and_provenance() {
        let rows = rows(&fixture(), Period::default(), None, HOST, &config());
        let text = render(&None, &rows, false);
        assert!(text.contains("\trepo\t"), "{text}");
        assert!(text.contains("gate\t(none)\t2\t"), "{text}");
        assert!(text.contains("(no rule)\t(none)\t"), "{text}");
        assert!(text.contains(NOT_HERE), "{text}");
        assert!(text.contains(UNMATCHED_HINT), "{text}");

        let v: Value = serde_json::from_str(&render(&None, &rows, true)).unwrap();
        let rule = |repo: Value| {
            v["rules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["rule"] == "gate" && r["repo"] == repo)
                .unwrap()
                .clone()
        };
        let crew = rule(json!("/r/crew"));
        assert_eq!(crew["now_slots"], 2);
        assert_eq!(crew["runs_slots"], 4);
        assert_eq!(crew["slots_from"]["path"], "/r/crew");
        assert_eq!(crew["slots_from"]["file"], "/cfg/config.toml");
        let none = rule(Value::Null);
        assert_eq!(none["slots_from"]["path"], Value::Null);
        let off = rule(json!("/r/off"));
        assert_eq!(off["now_slots"], Value::Null);
        assert_eq!(off["suggest"]["reason"], NOT_HERE);
    }
}
