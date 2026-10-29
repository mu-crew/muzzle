//! `bench/linuxpc-config.toml` (linuxpc_config; spec §Interface Config, D8,
//! D14, D17b, D21, D28) through `muzzle match`, with a private
//! `XDG_CONFIG_HOME`. The commands are synthetic copies of the shapes seen
//! in museum; the real sample stays in the private sidecar (D38).

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn config_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("muzzle")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/linuxpc-config.toml"),
        home.path().join("muzzle/config.toml"),
    )
    .unwrap();
    home
}

fn matched(home: &Path, command: &str) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .args(["match", "--json", "--", command])
        .env("XDG_CONFIG_HOME", home)
        .output()
        .expect("spawn muzzle");
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn gate_commands_get_their_rule() {
    let home = config_home();
    let cases: &[(&str, &str, u64)] = &[
        // D17b: the container hop is typed by the agent, inside the string.
        (
            "cd w1 && distrobox enter dev -- bash -lc 'cd $PWD && ./contrib/fetch-oracle 2>&1 | tail -2 && ./contrib/fast-gate > s/gate.log 2>&1'",
            "fast-gate",
            6,
        ),
        (
            "distrobox enter dev -- bash -lc 'cd w1 && FAST_GATE_BASE=main ./contrib/fast-gate'",
            "fast-gate",
            6,
        ),
        ("./contrib/fast-gate", "fast-gate", 6),
        (
            "timeout 3000 distrobox enter dev -- bash -c \"cd $PWD && timeout 1500 contrib/gate\"",
            "gate",
            6,
        ),
        (
            "cd w1 && unset A B && timeout 10m contrib/gate --fast",
            "gate",
            6,
        ),
        (
            "/usr/bin/time -f \"WALL=%e\" timeout 300 contrib/gate --fast > g.log",
            "gate",
            6,
        ),
        ("x.sh 2400 \"contrib/gate 2>&1 | tail -25\"", "gate", 6),
        (
            "time flock s/oracle-sweep.lock podman run --rm img",
            "oracle-sweep",
            999,
        ),
        (
            "flock -w 1800 s/oracle-sweep.lock distrobox enter dev -- bash -lc 'x'",
            "oracle-sweep",
            999,
        ),
        (
            "L=s/oracle-sweep.lock exec 9>>$L; flock -w 7200 9 || exit 2; run",
            "oracle-sweep",
            999,
        ),
        (
            "distrobox enter dev -- bash -lc \"cd $PWD && ../s/oracle-sweep a b\"",
            "oracle-sweep",
            999,
        ),
        (
            "(npm run typecheck && npm run lint && npm run test:fast) 2>&1 | tail",
            "js-test",
            2,
        ),
        (
            "npx biome check src; npx vitest run test/a.test.ts",
            "js-test",
            2,
        ),
        (
            "export TMPDIR=t; npm run test > full.log 2>&1",
            "js-test",
            2,
        ),
        (
            "distrobox enter dev -- bash -lc 'cd w1 && cargo build --release 2>&1 | tail -2'",
            "cargo-test",
            4,
        ),
        ("for p in a b; do cargo test -p $p; done", "cargo-test", 4),
        (
            "CARGO_BUILD_JOBS=4 cargo clippy --all-targets -- -D warnings",
            "cargo-test",
            4,
        ),
        // First match wins: the gate, not the cargo inside the same string.
        (
            "cargo build --release && ./contrib/fast-gate",
            "fast-gate",
            6,
        ),
    ];
    for (command, rule, slots) in cases {
        let v = matched(home.path(), command);
        assert_eq!(v["rule"], *rule, "{command}");
        assert_eq!(v["slots"], *slots, "{command}");
        assert_eq!(v["source"], "global", "{command}");
    }
}

/// A poll that names a gate must not claim: wrapped, it would hold the
/// gate's slots a second time while the gate itself runs.
#[test]
fn polls_and_reads_stay_unclaimed() {
    let home = config_home();
    for command in [
        "while pgrep -f 'contrib/fast-gate' >/dev/null; do sleep 30; done; tail -25 g.log",
        "sleep 900; pgrep -f \"contrib/fast-gate\" >/dev/null && echo running",
        "sleep 1500; cat s/oracle-sweep.lock",
        "for i in $(seq 1 100); do flock -n s/oracle-sweep.lock true && break; sleep 30; done",
        "grep -n 'PAM' ci.yml tree/contrib/gate",
        "cat contrib/gate | head -40",
        "pgrep -fa \"vitest\" | head -3",
        "cargo +nightly fmt --all",
        "sed -i 's/cargo test --release/x/' run.sh",
    ] {
        assert_eq!(
            matched(home.path(), command)["slots"],
            Value::Null,
            "{command}"
        );
    }
}

/// D28: later tasks that change this file record the new hash. This pins
/// the one in linuxpc_config's evidence, so a change can't go unnoticed.
#[test]
fn config_hash_is_recorded() {
    let home = config_home();
    assert_eq!(matched(home.path(), "cargo test")["cfg"], "b7e2bb");
}
