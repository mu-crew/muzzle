//! `muzzle bench run` and `bench probe` as real processes (spec §Testing,
//! Benchmark tests): the tiny fixture trace in both modes, private state
//! and config.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const CONFIG: &str = r#"
slots = 4

[[rule]]
name = "gate"
match = 'gate'
slots = 3

[[rule]]
name = "js-test"
match = 'vitest'
slots = 2
"#;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn muzzle(dir: &Path, args: &[&str]) -> Output {
    muzzle_env(dir, args, &[])
}

/// Every bench run burns real CPU, and the timing tests assert on wall time,
/// so bench runs in this binary go one at a time (bench_uncontended_test_flaky:
/// in parallel, the other tests' burners stretched an "uncontended" call).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn muzzle_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let config = dir.join("config");
    std::fs::create_dir_all(config.join("muzzle")).unwrap();
    std::fs::write(config.join("muzzle/config.toml"), CONFIG).unwrap();
    Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .args(args)
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env(
            "MUZZLE_UNIT_PREFIX",
            format!("muzzle-b{}", std::process::id()),
        )
        // The gate may run this under muzzle: the bench must not nest.
        .env_remove("MUZZLE_HOLD")
        .env_remove("MUZZLE_CLASS")
        .envs(env.iter().copied())
        .output()
        .unwrap()
}

fn bench(mode: &str, extra: &[&str]) -> (tempfile::TempDir, PathBuf, Value) {
    bench_trace(&fixture("tiny-trace.jsonl"), mode, "20", extra)
}

fn bench_trace(
    trace: &Path,
    mode: &str,
    speed: &str,
    extra: &[&str],
) -> (tempfile::TempDir, PathBuf, Value) {
    bench_trace_env(trace, mode, speed, extra, &[])
}

fn bench_trace_env(
    trace: &Path,
    mode: &str,
    speed: &str,
    extra: &[&str],
    env: &[(&str, &str)],
) -> (tempfile::TempDir, PathBuf, Value) {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let mut args = vec![
        "bench",
        "run",
        trace.to_str().unwrap(),
        "--mode",
        mode,
        "--tier",
        "synthetic",
        "--speed",
        speed,
        "--out",
        out.to_str().unwrap(),
    ];
    args.extend(extra);
    let o = muzzle_env(dir.path(), &args, env);
    assert!(o.status.success(), "{o:?}");
    let result: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("result.json")).unwrap()).unwrap();
    (dir, out, result)
}

/// Every D35 figure, non-null.
fn assert_d35(r: &Value) {
    for path in [
        "/makespan_s",
        "/jobs_per_hour",
        "/wait_s/p50",
        "/wait_s/p90",
        "/run_s/p50",
        "/run_s/p90",
        "/max_slots_held",
        "/probe/git_status_p95_ms",
        "/probe/sleep_late_p99_ms",
        "/load/load1_p50",
        "/load/load1_max",
        "/oom_kills",
    ] {
        let v = r.pointer(path).unwrap_or(&Value::Null);
        assert!(v.is_number(), "{path}: {v} in {r:#}");
    }
}

#[test]
fn muzzle_mode_queues_matched_calls_and_logs_them() {
    let (_dir, out, r) = bench("muzzle", &[]);
    assert_d35(&r);
    assert_eq!(r["mode"], "muzzle");
    // light is skipped; unmatched-heavy runs, but outside muzzle.
    assert_eq!(r["calls"]["replayed"], 5);
    assert_eq!(r["calls"]["light_skipped"], 1);
    assert_eq!(r["calls"]["unmatched_heavy"], 1);
    assert_eq!(r["calls"]["failed"], 0);
    assert_eq!(r["machine"]["budget"], 4);
    // Two 3-slot gates overlap in the trace; the budget of 4 holds one.
    assert_eq!(r["max_slots_held"], 3);
    assert!(r["wait_s"]["p90"].as_f64().unwrap() > 0.0, "{r:#}");
    let runs: Vec<Value> = std::fs::read_to_string(out.join("runs.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let mut rules: Vec<&str> = runs.iter().map(|r| r["rule"].as_str().unwrap()).collect();
    rules.sort();
    assert_eq!(rules, ["gate", "gate", "gate", "js-test"]);
    let summary = std::fs::read_to_string(out.join("summary.md")).unwrap();
    assert!(summary.contains("mode muzzle"), "{summary}");
    assert!(summary.contains("10 ms sleep late p99"), "{summary}");
}

/// The held figure comes from muzzle's own admission and release times,
/// so under overload it never exceeds the budget. Rebuilt from the
/// bench's spawn time plus `waited_s`, 40 calls at once read 20/14 on
/// linuxpc while live `muzzle status` never passed 14. The overlap is a
/// timing race (this trace read 6/4 on 1 of 2 CLI runs, 0 of 4 under
/// `cargo test`), so this guards the bound; the unit test
/// `held_slots_prefer_muzzles_own_times` pins the fix.
#[test]
fn muzzle_mode_held_slots_stay_within_budget_under_overload() {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("overload.jsonl");
    let mut rows = vec![
        r#"{"v":1,"kind":"header","host":"linuxpc","cores":16,"ram_gb":31,"date":"2026-10-08","window_s":10,"source":"museum","calls_total":12,"calls_kept":12,"calls_dropped":0}"#.to_string(),
    ];
    for i in 0..12 {
        rows.push(format!(
            r#"{{"v":1,"kind":"call","t":0.0,"dur_s":1.0,"dur_src":"durationMs","bg":false,"rule":"gate","role":"worker","agent":"a{i}","project":"p1"}}"#
        ));
    }
    std::fs::write(&trace, rows.join("\n") + "\n").unwrap();
    let (_dir, _out, r) = bench_trace(&trace, "muzzle", "1", &[]);
    assert_eq!(r["calls"]["failed"], 0, "{r:#}");
    assert_eq!(r["machine"]["budget"], 4);
    // A gate claims 3 of 4: any overlap would read 6.
    assert_eq!(r["max_slots_held"], 3, "{r:#}");
}

#[test]
fn none_mode_starts_everything_at_once_with_role_caps() {
    let (_dir, out, r) = bench("none", &[]);
    assert_d35(&r);
    assert_eq!(r["mode"], "none");
    assert_eq!(r["calls"]["replayed"], 5);
    assert_eq!(r["calls"]["failed"], 0);
    // No queue: nothing waits. Each burns its rule's slots capped at the
    // C3 role cap: worker gate 3 (cap 4) + orchestrator gate 3 (cap 6) +
    // reviewer js-test 2 (cap 2) overlap at t = 2 s.
    assert_eq!(r["wait_s"]["p90"], 0.0);
    assert_eq!(r["max_slots_held"], 8);
    assert!(!out.join("runs.jsonl").exists());
    assert!(out.join("summary.md").exists());
}

/// A one-call trace, written into `dir`.
fn one_call(dir: &Path, rule: &str, role: &str, dur_s: f64) -> PathBuf {
    let trace = dir.join("one.jsonl");
    std::fs::write(
        &trace,
        format!(
            "{}\n{}\n",
            r#"{"v":1,"kind":"header","host":"linuxpc","cores":16,"ram_gb":31,"date":"2026-10-08","window_s":10,"source":"museum","calls_total":1,"calls_kept":1,"calls_dropped":0}"#,
            format_args!(
                r#"{{"v":1,"kind":"call","t":0.0,"dur_s":{dur_s},"dur_src":"durationMs","bg":false,"rule":"{rule}","role":"{role}","agent":"a1","project":"p1"}}"#
            )
        ),
    )
    .unwrap();
    trace
}

/// bench_none_work_model: uncontended, a call takes about `dur_s / speed`
/// in both modes, even for a role whose cap (reviewer 2) is below the
/// rule's slots (gate 3). It used to burn 3 x dur_s over 2 threads in
/// none mode: 1.5 x dur_s. The burner counts CPU time, so it is never
/// faster than its work over its threads: the bug can't run under 3 s
/// however idle the host. Load only adds wall time (f-flake-bounds: a
/// shared host stretched a 2 s call past 2.7 s), so a mode passes when any
/// of a few tries lands under the bound, which a buggy run never can.
#[test]
fn an_uncontended_call_takes_its_duration_in_both_modes() {
    let dir = tempfile::tempdir().unwrap();
    let trace = one_call(dir.path(), "gate", "reviewer", 2.0);
    for mode in ["none", "muzzle"] {
        let mut runs = Vec::new();
        for _ in 0..3 {
            let (_d, _out, r) = bench_trace(&trace, mode, "1", &[]);
            assert_eq!(r["calls"]["failed"], 0, "{mode}: {r:#}");
            let run = r["run_s"]["p50"].as_f64().unwrap();
            assert!(run >= 1.9, "{mode}: run_s {run}, want ~2");
            runs.push(run);
            if run < 2.8 {
                break;
            }
        }
        let best = runs.iter().copied().fold(f64::MAX, f64::min);
        assert!(best < 2.8, "{mode}: run_s {runs:?}, want ~2");
    }
}

/// bench_unmatched_thread_test: an unmatched-heavy call burns one thread,
/// not the bench's inherited CARGO_BUILD_JOBS (8 here would finish its
/// 1 CPU-second in 0.125 s). The burner counts CPU time, so only a lower
/// bound is needed.
#[test]
fn unmatched_heavy_burns_one_thread_whatever_the_env() {
    let dir = tempfile::tempdir().unwrap();
    let trace = one_call(dir.path(), "unmatched-heavy", "worker", 1.0);
    for mode in ["none", "muzzle"] {
        let (_d, _out, r) = bench_trace_env(&trace, mode, "1", &[], &[("CARGO_BUILD_JOBS", "8")]);
        assert_eq!(r["calls"]["unmatched_heavy"], 1, "{mode}: {r:#}");
        let run = r["run_s"]["p50"].as_f64().unwrap();
        assert!(run >= 0.9, "{mode}: run_s {run}, want ~1");
    }
}

#[test]
fn busiest_window_replays_only_its_calls() {
    let (_dir, _out, r) = bench("none", &["--window", "busiest:10s"]);
    assert_eq!(r["window_start_s"], 0.0);
    // The window [0, 10) holds the first five rows; the gate at 100 s is out.
    assert_eq!(r["calls"]["replayed"], 4);
    assert_eq!(r["calls"]["light_skipped"], 1);
}

#[test]
fn bad_window_and_speed_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let trace = fixture("tiny-trace.jsonl");
    let t = trace.to_str().unwrap();
    for extra in [["--window", "3h"], ["--speed", "0"]] {
        let mut args = vec!["bench", "run", t];
        args.extend(extra);
        assert_eq!(
            muzzle(dir.path(), &args).status.code(),
            Some(2),
            "{extra:?}"
        );
    }
}

#[test]
fn probe_appends_one_line_per_sample() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("probe.jsonl");
    let f = file.to_str().unwrap();
    let args = [
        "bench",
        "probe",
        "--samples",
        "2",
        "--interval",
        "0",
        "--out",
        f,
    ];
    assert!(muzzle(dir.path(), &args).status.success());
    assert!(muzzle(dir.path(), &args[..6]).status.success()); // to stdout
    let o = muzzle(
        dir.path(),
        &["bench", "probe", "--samples", "1", "--out", f],
    );
    assert!(o.status.success(), "{o:?}");
    let lines: Vec<Value> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    for l in &lines {
        for k in [
            "git_status_p95_ms",
            "sleep_late_p99_ms",
            "load1",
            "load5",
            "load15",
        ] {
            assert!(l[k].is_number(), "{k}: {l}");
        }
        assert_eq!(l["v"], 1);
    }
}

#[test]
fn real_tier_runs_rule_commands_in_per_agent_prebuilt_worktrees() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let repo = d.join("repo");
    let git = |args: &[&str]| {
        let st = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    };
    std::fs::create_dir_all(&repo).unwrap();
    git(&["init", "-q"]);
    std::fs::write(repo.join("f"), "x").unwrap();
    git(&["add", "f"]);
    git(&["commit", "-q", "--no-gpg-sign", "-m", "x"]);
    let real = d.join("real.toml");
    std::fs::write(
        &real,
        format!(
            "repo = {:?}\ncommit = \"HEAD\"\nworkdir = {:?}\nsetup = 'touch built && echo \"$CARGO_TARGET_DIR\" >> td.log'\n\
             [rules]\ngate = 'test -f built && echo \"$CARGO_BUILD_JOBS\" >> gate.log && echo \"$CARGO_TARGET_DIR\" >> td.log'\n\
             js-test = 'test -f built && echo js >> js.log'\n",
            repo,
            d.join("wt")
        ),
    )
    .unwrap();
    let trace = fixture("tiny-trace.jsonl");
    for mode in ["none", "muzzle"] {
        let out = d.join(format!("out-{mode}"));
        let args = [
            "bench",
            "run",
            trace.to_str().unwrap(),
            "--mode",
            mode,
            "--tier",
            "real",
            "--real",
            real.to_str().unwrap(),
            "--speed",
            "20",
            "--out",
            out.to_str().unwrap(),
        ];
        // An inherited target dir must not be shared by the agents.
        let o = muzzle_env(d, &args, &[("CARGO_TARGET_DIR", "/tmp/shared-target")]);
        assert!(o.status.success(), "{mode}: {o:?}");
        let r: Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("result.json")).unwrap())
                .unwrap();
        assert_d35(&r);
        assert_eq!(r["calls"]["failed"], 0, "{mode}: {r:#}");
    }
    // a1 is a worker (cap 4 in none); its gates claim 3 slots under muzzle.
    let a1 = std::fs::read_to_string(d.join("wt/a1/gate.log")).unwrap();
    assert_eq!(a1, "4\n4\n3\n3\n");
    assert_eq!(
        std::fs::read_to_string(d.join("wt/a2/gate.log")).unwrap(),
        "6\n3\n"
    );
    assert!(d.join("wt/a3/js.log").exists());
    // bench_real_target_dir: each agent's own target dir, for setup (once
    // per mode) and every replayed call, in both modes.
    for (agent, gates) in [("a1", 4), ("a2", 2)] {
        let want = format!("{}\n", d.join("wt").join(agent).join("target").display());
        assert_eq!(
            std::fs::read_to_string(d.join("wt").join(agent).join("td.log")).unwrap(),
            want.repeat(2 + gates),
            "{agent}"
        );
    }
    // unmatched-heavy (a4) burns a thread and needs no worktree command.
    assert!(d.join("wt/a4/built").exists());
}
