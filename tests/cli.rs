//! Every subcommand from spec §Interface/CLI parses and, until its unit
//! lands, exits 125 ("muzzle itself failed").

use std::process::Command;

fn muzzle(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .args(args)
        .output()
        .expect("spawn muzzle")
}

#[test]
fn every_subcommand_parses() {
    let cases: &[&[&str]] = &[
        &[
            "run",
            "--slots",
            "2",
            "--class",
            "high",
            "--rule",
            "gate",
            "--run-timeout",
            "5",
            "--timeout",
            "3",
            "--report",
            "r.json",
            "--",
            "true",
            "x",
        ],
        &["match", "--json", "--cwd", ".", "--", "cargo test"],
        &[
            "wrap",
            "--json",
            "--cwd",
            ".",
            "--timeout",
            "60",
            "--",
            "cargo test",
        ],
        &["hook", "claude"],
        &["hook", "codex"],
        &["status", "--json"],
        &["doctor", "--json"],
        &[
            "report",
            "--rule",
            "gate",
            "--since",
            "2026-10-17",
            "--until",
            "2026-10-28",
            "--compare",
            "a..b",
            "c..d",
            "--json",
        ],
        &[
            "bench",
            "run",
            "t.jsonl",
            "--mode",
            "none",
            "--tier",
            "synthetic",
            "--speed",
            "10",
            "--window",
            "busiest:3h",
            "--out",
            "out",
        ],
        &[
            "bench",
            "probe",
            "--samples",
            "3",
            "--interval",
            "1",
            "--out",
            "p.jsonl",
        ],
    ];
    for args in cases {
        let out = muzzle(args);
        assert_eq!(out.status.code(), Some(125), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("not implemented"), "{args:?}: {stderr}");
    }
}

#[test]
fn bad_usage_exits_2() {
    assert_eq!(muzzle(&["run"]).status.code(), Some(2));
    assert_eq!(muzzle(&["hook", "cursor"]).status.code(), Some(2));
}
