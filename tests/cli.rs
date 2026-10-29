//! The CLI surface as a whole: every unit has landed and tests itself
//! (match, wrap, hook: `tests/rules.rs`; report: `tests/report.rs`;
//! status, doctor: `tests/status_doctor.rs`; bench: `tests/bench.rs`).

use std::process::Command;

fn muzzle(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .args(args)
        .output()
        .expect("spawn muzzle")
}

#[test]
fn every_subcommand_is_implemented() {
    // `bench run` on a missing trace parses, then fails reading it (125),
    // which is muzzle failing, not a stub.
    let args = [
        "bench",
        "run",
        "/nonexistent/t.jsonl",
        "--mode",
        "none",
        "--tier",
        "synthetic",
        "--speed",
        "10",
        "--window",
        "busiest:3h",
        "--out",
        "/nonexistent/out",
    ];
    let out = muzzle(&args);
    assert_eq!(out.status.code(), Some(125), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("not implemented"), "{stderr}");
    assert!(stderr.contains("reading"), "{stderr}");
}

#[test]
fn bad_usage_exits_2() {
    assert_eq!(muzzle(&["run"]).status.code(), Some(2));
    assert_eq!(muzzle(&["hook", "cursor"]).status.code(), Some(2));
}
