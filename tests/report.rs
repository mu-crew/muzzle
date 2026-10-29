//! `muzzle report` as a process over a fixture `runs.jsonl`, with a private
//! `XDG_STATE_HOME` (spec §Interface, `report`; D29, D30, D30a, D41).

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

/// Runs `muzzle report ARGS` over `tests/fixtures/runs/basic.jsonl`. The
/// state.json budget fixes memory per slot at 2 GiB, so the result doesn't
/// depend on the test machine.
fn report(args: &[&str]) -> Output {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("muzzle");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runs/basic.jsonl"),
        dir.join("runs.jsonl"),
    )
    .unwrap();
    std::fs::write(
        dir.join("state.json"),
        r#"{"v":1,"next_ticket":1,"budget":{"budget":8,"cores":8,"ram_per_slot":2147483648}}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .arg("report")
        .args(args)
        .env("XDG_STATE_HOME", home.path())
        .output()
        .unwrap()
}

fn json(args: &[&str]) -> Value {
    let out = report(args);
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

fn rule<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule"] == name)
        .unwrap_or_else(|| panic!("no {name} in {v}"))
}

#[test]
fn one_day_tags_each_suggestion() {
    let v = json(&["--json", "--since", "2026-11-01", "--until", "2026-11-01"]);
    let gate = rule(&v, "gate");
    assert_eq!(gate["runs"], 5);
    assert_eq!(gate["suggest"]["tag"], "experiment");
    assert_eq!(gate["suggest"]["slots"], 8);
    assert_eq!(
        rule(&v, "cargo-test")["suggest"]["reason"],
        "insufficient data (3 runs)"
    );
    let js = rule(&v, "js-test");
    assert_eq!(
        (js["clean"].as_u64(), js["oom"].as_u64()),
        (Some(0), Some(1))
    );
    // ceil(1.25 * 6000 MB / 2048 MB) = 4.
    assert_eq!(js["suggest"]["tag"], "oom");
    assert_eq!(js["suggest"]["slots"], 4);
    assert!(v["machine"]["cpu_efficiency"].is_number());
    assert_eq!(v["machine"]["budget"], 8);
}

#[test]
fn compare_splits_by_hash_and_confirms_the_experiment() {
    let v = json(&[
        "--json",
        "--compare",
        "2026-11-01..2026-11-01",
        "2026-11-02..2026-11-02",
    ]);
    let d = v["deltas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["rule"] == "gate")
        .unwrap();
    assert_eq!(d["slots"], serde_json::json!([6, 8]));
    assert_eq!(d["experiment"], "confirmed");
    // The whole log under one period holds both hashes: split.
    let all = json(&["--json", "--compare", "..2026-11-01", "..2026-11-02"]);
    let by: Vec<&str> = rule(&all["periods"][1], "gate")["by_cfg"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["cfg"].as_str().unwrap())
        .collect();
    assert_eq!(by, ["c1", "c2"]);
}

#[test]
fn text_has_the_hint_line_and_bad_dates_exit_2() {
    let out = report(&["--rule", "gate"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("museum-search calls --min-secs 30 --tool bash"),
        "{text}"
    );
    assert!(!text.contains("cargo-test"), "{text}");
    assert_eq!(report(&["--since", "last tuesday"]).status.code(), Some(2));
}

#[test]
fn missing_log_is_an_empty_report() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .args(["report", "--json"])
        .env("XDG_STATE_HOME", home.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["machine"].is_null());
    assert_eq!(v["rules"], serde_json::json!([]));
}
