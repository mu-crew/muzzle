//! The CLI surface as a whole: every unit has landed and tests itself
//! (match, wrap, hook: `tests/rules.rs`; report: `tests/report.rs`;
//! status, doctor: `tests/status_doctor.rs`; bench: `tests/bench.rs`).

use std::path::Path;
use std::process::Command;

fn muzzle(args: &[&str]) -> std::process::Output {
    let home = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .args(args)
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_STATE_HOME", home.path())
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
    assert_eq!(muzzle(&["hook", "aider"]).status.code(), Some(2));
}

/// AGENTS.md, State directories: no test runs muzzle against the real
/// `~/.local/state/muzzle`. A coarse guard, per file and not per spawn: a
/// test file that spawns the binary sets `XDG_STATE_HOME` somewhere, even
/// for a command that touches no state today. Spawns go through per-file
/// helpers (`under()` in run.rs), so counting sites can't check each one;
/// a new spawn in a file that already passes needs its own review.
#[test]
fn every_test_file_that_spawns_muzzle_sets_xdg_state_home() {
    // Built so this file's own source does not satisfy the check.
    let needle = concat!(".env(", "\"XDG_STATE_HOME\"");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut bare = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        if text.contains("CARGO_BIN_EXE_muzzle") && !text.contains(needle) {
            bare.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    assert!(
        bare.is_empty(),
        "spawn muzzle without XDG_STATE_HOME: {bare:?}"
    );
}
