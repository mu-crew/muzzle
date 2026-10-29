//! `bench/check-trace`, `bench/from-museum` and `bench/pre-commit` as
//! processes (spec D36-D40, §Interface "Trace format", §Testing
//! "Benchmark tests"). The scripts are Python, so `cargo test` covers them
//! on both CI platforms.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture(name: &str) -> PathBuf {
    root().join("tests/fixtures").join(name)
}

/// A script with a private HOME (whose name the user-name check sees),
/// XDG dirs, and the built muzzle for rule labels.
fn script(home: &Path, name: &str, args: &[&str]) -> Output {
    let home = home.join("muzzleuser");
    fs::create_dir_all(&home).unwrap();
    Command::new(root().join("bench").join(name))
        .args(args)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("spawn bench/{name}: {e}"))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn bad_fixtures() -> Vec<PathBuf> {
    let mut v: Vec<_> = fs::read_dir(root().join("tests/fixtures"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with("bad-") && n.ends_with(".jsonl")
        })
        .collect();
    v.sort();
    v
}

#[test]
fn python_unit_tests_pass() {
    let out = Command::new("python3")
        .args([
            "-m",
            "unittest",
            "discover",
            "-s",
            "bench",
            "-p",
            "test_*.py",
        ])
        .current_dir(root())
        .output()
        .expect("python3");
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn good_fixture_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let good = fixture("good.jsonl");
    let out = script(tmp.path(), "check-trace", &[good.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn committed_traces_pass() {
    // What CI runs: no arguments checks every bench/traces/*.jsonl.
    let tmp = tempfile::tempdir().unwrap();
    let out = script(tmp.path(), "check-trace", &[]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn every_bad_fixture_fails_for_its_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let reasons = [
        ("bad-agent-name", "free text in 'agent'"),
        (
            "bad-big-number",
            "'dur_s' must be a number in [0, window_s]",
        ),
        ("bad-cores", "'cores' must be an integer in [1, 1024]"),
        ("bad-count", "calls_kept says 3, file has 2"),
        ("bad-duplicate-host", "duplicate key 'host'"),
        ("bad-duplicate-key", "duplicate key 'project'"),
        ("bad-email", "email in 'project'"),
        ("bad-extra-field", "field 'task' not allowed"),
        ("bad-foreign-host", "foreign host in 'host'"),
        ("bad-free-text", "free text in 'rule'"),
        ("bad-home-dir", "home dir in 'project'"),
        ("bad-hostname", "hostname in 'project'"),
        ("bad-not-canonical", "not canonical"),
        ("bad-path", "path in 'project'"),
        (
            "bad-precision",
            "'t' must be a number in [0, window_s) with at most 3 decimals",
        ),
        ("bad-token", "token in 'project'"),
        ("bad-unhashable", "free text in 'rule'"),
        ("bad-user-name", "home dir in 'project'"),
        ("bad-window", "'window_s' must be an integer in [1, 604800]"),
    ];
    let bad = bad_fixtures();
    assert_eq!(
        bad.len(),
        reasons.len(),
        "a bad fixture with no expected reason"
    );
    for (name, reason) in reasons {
        let path = fixture(&format!("{name}.jsonl"));
        let out = script(tmp.path(), "check-trace", &[path.to_str().unwrap()]);
        assert_eq!(out.status.code(), Some(1), "{name} passed");
        assert!(stderr(&out).contains(reason), "{name}: {}", stderr(&out));
        // The value under a duplicate key never reaches the log either.
        assert!(!stderr(&out).contains("secret"), "{name}: {}", stderr(&out));
    }
}

#[test]
fn a_traceback_never_replaces_a_named_error() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture("bad-unhashable.jsonl");
    let out = script(tmp.path(), "check-trace", &[path.to_str().unwrap()]);
    assert!(!stderr(&out).contains("Traceback"), "{}", stderr(&out));
}

/// Runs a copy of check-trace with no arguments over `files` in its
/// bench/traces/, as CI does.
fn check_default(files: &[(&str, &Path)]) -> Output {
    let tmp = tempfile::tempdir().unwrap();
    let repo = hook_repo(tmp.path());
    for (name, src) in files {
        let dst = repo.join("bench/traces").join(name);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::copy(src, dst).unwrap();
    }
    Command::new(repo.join("bench/check-trace"))
        .env("HOME", tmp.path())
        .output()
        .unwrap()
}

/// Path names that would publish an identity through git (D40, the
/// trace_filename_leak repro): an email, a token and a foreign host's
/// directory. Obviously fake; the token is built so no scanner flags it.
fn leaky_names() -> Vec<String> {
    vec![
        "jane.doe@example.com.jsonl".into(),
        format!("ghp_{}.jsonl", "x".repeat(36)),
        "otherhost.example/.gitkeep".into(),
    ]
}

/// What a refused name must never leak into a log.
fn assert_name_withheld(err: &str) {
    for part in ["jane", "example", "ghp_", "otherhost"] {
        assert!(!err.contains(part), "{part} echoed: {err}");
    }
}

#[test]
fn default_mode_checks_traces_and_refuses_other_paths() {
    let good = fixture("good.jsonl");
    let bad = fixture("bad-path.jsonl");
    let out = check_default(&[
        ("2026-10-08.jsonl", &good),
        ("2026-10-08-1.jsonl", &good),
        (".gitkeep", &good),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = check_default(&[("2026-10-08.jsonl", &bad)]);
    assert_eq!(out.status.code(), Some(1), "bad trace passed");
    assert!(
        stderr(&out).contains("2026-10-08.jsonl:4: path"),
        "{}",
        stderr(&out)
    );

    // A good trace under a bad name, a nested dir or a misnamed file.
    for name in [
        "x.json",
        "a.jsonl",
        "2026/2026-10-08.jsonl",
        "2026-13-08.jsonl",
        "2026-10-08-Tier2.jsonl",
        "2026-10-08-tier2.jsonl",
        "2026-10-08-100.jsonl",
        "sub/.gitkeep",
    ] {
        let out = check_default(&[("2026-10-08.jsonl", &good), (name, &good)]);
        assert_eq!(out.status.code(), Some(1), "{name} passed");
        assert!(
            stderr(&out).contains("names withheld"),
            "{name}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn default_mode_refuses_identifying_names_without_echoing_them() {
    let good = fixture("good.jsonl");
    for name in leaky_names() {
        let out = check_default(&[(&name, &good)]);
        assert_eq!(out.status.code(), Some(1), "{name} passed CI mode");
        let err = stderr(&out);
        assert!(err.contains("path(s) not named"), "{name}: {err}");
        assert_name_withheld(&err);
    }
}

#[test]
fn errors_never_echo_the_offending_value() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture("bad-token.jsonl");
    let out = script(tmp.path(), "check-trace", &[path.to_str().unwrap()]);
    assert!(!stderr(&out).contains("ghp_"), "{}", stderr(&out));
}

#[test]
fn a_label_is_allowed_only_from_the_labels_file() {
    let tmp = tempfile::tempdir().unwrap();
    let trace = tmp.path().join("t.jsonl");
    let good = fs::read_to_string(fixture("good.jsonl")).unwrap();
    fs::write(
        &trace,
        good.replace(r#""rule":"gate""#, r#""rule":"oracle""#),
    )
    .unwrap();
    let labels = tmp.path().join("labels.txt");
    fs::write(&labels, "rule gate\n").unwrap();
    let args = |l: &Path| {
        vec![
            "--labels".into(),
            l.to_str().unwrap().to_string(),
            trace.to_str().unwrap().to_string(),
        ]
    };
    let run = |l: &Path| {
        let a = args(l);
        script(
            tmp.path(),
            "check-trace",
            &a.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    };
    assert_eq!(run(&labels).status.code(), Some(1));
    fs::write(&labels, "rule gate\nrule oracle\n").unwrap();
    assert!(run(&labels).status.success());
    // A malformed labels line fails rather than allowing anything.
    fs::write(&labels, "rule oracle\nrule /tmp/x\n").unwrap();
    assert_eq!(run(&labels).status.code(), Some(1));
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn import(tmp: &Path, extra: &[&str]) -> (Output, PathBuf) {
    let out_path = tmp.join("traces/2026-10-21.jsonl");
    let calls = fixture("museum-calls.json");
    let mut args = vec![
        "--date",
        "2026-10-21",
        "--calls",
        calls.to_str().unwrap(),
        "--out",
        out_path.to_str().unwrap(),
        "--muzzle",
        env!("CARGO_BIN_EXE_muzzle"),
        "--cores",
        "16",
        "--ram-gb",
        "31",
    ];
    args.extend_from_slice(extra);
    (script(tmp, "from-museum", &args), out_path)
}

#[test]
fn from_museum_writes_an_allowlisted_trace_and_a_private_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let (out, trace) = import(tmp.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));

    let rows = lines(&trace);
    let header = &rows[0];
    assert_eq!(header["host"], "linuxpc");
    assert_eq!(header["date"], "2026-10-21");
    // 7 linuxpc calls on the day (the 10-20 one is outside it, otherhost's is
    // refused before counting); the `read` call is dropped.
    assert_eq!(header["calls_total"], 7);
    assert_eq!(header["calls_kept"], 6);
    assert_eq!(header["calls_dropped"], 1);

    let calls: Vec<(String, String, String, String, bool)> = rows[1..]
        .iter()
        .map(|c| {
            let s = |k: &str| c[k].as_str().unwrap().to_string();
            (
                s("rule"),
                s("role"),
                s("agent"),
                s("project"),
                c["bg"].as_bool().unwrap(),
            )
        })
        .collect();
    let want = [
        ("gate", "worker", "a1", "p1", false),
        ("light", "orchestrator", "a2", "p1", false), // mu agent wait, 1500 s
        ("light", "worker", "a1", "p1", false),       // sleep; tail gate.log
        ("cargo-test", "reviewer", "a3", "p2", true),
        ("unmatched-heavy", "worker", "a4", "p1", false),
        ("light", "worker", "a4", "p1", false),
    ];
    let want: Vec<_> = want
        .iter()
        .map(|(r, o, a, p, b)| {
            (
                r.to_string(),
                o.to_string(),
                a.to_string(),
                p.to_string(),
                *b,
            )
        })
        .collect();
    assert_eq!(calls, want);
    assert_eq!(rows[1]["t"], 28810.0);
    assert_eq!(rows[1]["dur_src"], "durationMs");
    assert_eq!(rows[3]["dur_src"], "estimated");

    // Nothing identifying reaches the trace, and it passes the checker.
    let text = fs::read_to_string(&trace).unwrap();
    for leak in [
        "contrib",
        "worker-17",
        "lock-audit",
        "someone",
        "otherhost",
        "fixture",
        "s1",
    ] {
        assert!(!text.contains(leak), "{leak} in the trace");
    }
    let check = script(tmp.path(), "check-trace", &[trace.to_str().unwrap()]);
    assert!(check.status.success(), "{}", stderr(&check));

    // The sidecar maps each line back, privately.
    let side = tmp
        .path()
        .join("muzzleuser/state/muzzle/bench/2026-10-21.private.jsonl");
    let side_rows = lines(&side);
    assert_eq!(side_rows.len(), 7);
    assert_eq!(side_rows[0]["cfg"].as_str().unwrap().len(), 6);
    assert_eq!(side_rows[1]["line"], 2);
    assert_eq!(
        side_rows[1]["command"],
        "cd /home/someone/hacking/lock && contrib/gate"
    );
    assert_eq!(side_rows[1]["agent_was"], "lock-audit/worker-17");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&side).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn from_museum_refuses_other_hosts() {
    let tmp = tempfile::tempdir().unwrap();
    let trace = tmp.path().join("2026-10-21.jsonl");
    let calls = fixture("museum-other-host.json");
    let out = script(
        tmp.path(),
        "from-museum",
        &[
            "--date",
            "2026-10-21",
            "--calls",
            calls.to_str().unwrap(),
            "--out",
            trace.to_str().unwrap(),
            "--muzzle",
            env!("CARGO_BIN_EXE_muzzle"),
        ],
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    // otherhost, linuxpc-old, linuxpc/../otherhost, /linuxpc and a
    // linuxpc path with no session folder (D37).
    assert!(err.contains("refused 5 call(s) from non-linuxpc"), "{err}");
    assert!(
        !err.contains("otherhost") && !err.contains("secret"),
        "{err}"
    );
    assert!(!trace.exists());
    assert!(!tmp.path().join("muzzleuser/state/muzzle/bench").exists());
}

#[test]
fn from_museum_refuses_an_identifying_out_name() {
    let mut names = leaky_names();
    names.push("t.jsonl".into());
    for name in names {
        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("traces").join(&name);
        let calls = fixture("museum-calls.json");
        let out = script(
            tmp.path(),
            "from-museum",
            &[
                "--date",
                "2026-10-21",
                "--calls",
                calls.to_str().unwrap(),
                "--out",
                out_path.to_str().unwrap(),
                "--muzzle",
                env!("CARGO_BIN_EXE_muzzle"),
            ],
        );
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("--out must be named"), "{name}: {err}");
        assert_name_withheld(&err);
        assert!(!tmp.path().join("traces").exists(), "{name} written");
        assert!(
            !tmp.path().join("muzzleuser/state/muzzle/bench").exists(),
            "{name}: sidecar written"
        );
    }
}

#[test]
fn keep_project_needs_a_reviewed_label() {
    let tmp = tempfile::tempdir().unwrap();
    let (out, trace) = import(tmp.path(), &["--keep-project", "lock-audit"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("free text in 'project'"),
        "{}",
        stderr(&out)
    );
    assert!(!trace.exists());

    let labels = tmp.path().join("labels.txt");
    fs::write(&labels, "rule gate\nrule cargo-test\nproject lock-audit\n").unwrap();
    let (out, trace) = import(
        tmp.path(),
        &[
            "--keep-project",
            "lock-audit",
            "--labels",
            labels.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let rows = lines(&trace);
    assert_eq!(rows[1]["project"], "lock-audit");
    assert_eq!(rows[4]["project"], "p1"); // other-ws is still renamed
}

#[test]
fn an_unlisted_rule_stops_the_import() {
    let tmp = tempfile::tempdir().unwrap();
    let labels = tmp.path().join("labels.txt");
    fs::write(&labels, "rule gate\n").unwrap(); // no cargo-test
    let (out, trace) = import(tmp.path(), &["--labels", labels.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("allowed-labels.txt"),
        "{}",
        stderr(&out)
    );
    assert!(!trace.exists());
}

/// A scratch repo with the bench scripts, the hook run as git would.
fn hook_repo(tmp: &Path) -> PathBuf {
    let repo = tmp.join("repo");
    fs::create_dir_all(repo.join("bench/traces")).unwrap();
    for f in ["check-trace", "allowed-labels.txt", "pre-commit"] {
        fs::copy(root().join("bench").join(f), repo.join("bench").join(f)).unwrap();
    }
    git(&repo, &["init", "-q"]);
    repo
}

fn git(repo: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

fn hook(repo: &Path) -> Output {
    Command::new(repo.join("bench/pre-commit"))
        .current_dir(repo)
        .env("HOME", repo)
        .output()
        .unwrap()
}

#[test]
fn pre_commit_checks_staged_traces_and_refuses_sidecars() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = hook_repo(tmp.path());
    let trace = repo.join("bench/traces/2026-10-08.jsonl");

    fs::copy(fixture("bad-path.jsonl"), &trace).unwrap();
    git(&repo, &["add", "bench/traces/2026-10-08.jsonl"]);
    let out = hook(&repo);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("bench/traces/2026-10-08.jsonl:4: path"),
        "{}",
        stderr(&out)
    );

    // The staged blob is checked, not the working tree.
    fs::copy(fixture("good.jsonl"), &trace).unwrap();
    assert_eq!(hook(&repo).status.code(), Some(1));
    git(&repo, &["add", "bench/traces/2026-10-08.jsonl"]);
    let out = hook(&repo);
    assert!(out.status.success(), "{}", stderr(&out));

    for name in ["x.private.jsonl", "x.private.jsonl.bak"] {
        fs::write(repo.join(name), "{}\n").unwrap();
        git(&repo, &["add", name]);
        let out = hook(&repo);
        assert_eq!(out.status.code(), Some(1), "{name}");
        assert!(stderr(&out).contains("private sidecar"), "{}", stderr(&out));
        git(&repo, &["rm", "-q", "--cached", name]);
    }
}

#[test]
fn pre_commit_refuses_any_path_under_traces_not_named_as_a_trace() {
    // A space in the name, a nested dir, a non-.jsonl name and the
    // identifying ones: each is refused on its name alone, and a refused
    // name is never echoed.
    let mut names = leaky_names();
    names.extend(["a b.jsonl", "2026/2026-10-08.jsonl", "x.json"].map(String::from));
    for name in names {
        let tmp = tempfile::tempdir().unwrap();
        let repo = hook_repo(tmp.path());
        let path = repo.join("bench/traces").join(&name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(fixture("good.jsonl"), &path).unwrap();
        git(&repo, &["add", "."]);
        let out = hook(&repo);
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("names withheld"), "{name}: {err}");
        assert_name_withheld(&err);
    }
}

#[test]
fn pre_commit_allows_dated_traces_and_the_top_level_gitkeep() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = hook_repo(tmp.path());
    for name in ["2026-10-08.jsonl", "2026-10-21-2.jsonl"] {
        fs::copy(fixture("good.jsonl"), repo.join("bench/traces").join(name)).unwrap();
    }
    fs::write(repo.join("bench/traces/.gitkeep"), "").unwrap();
    git(&repo, &["add", "."]);
    let out = hook(&repo);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn pre_commit_rechecks_traces_when_labels_change() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = hook_repo(tmp.path());
    fs::copy(
        fixture("good.jsonl"),
        repo.join("bench/traces/2026-10-08.jsonl"),
    )
    .unwrap();
    git(&repo, &["add", "."]);
    assert!(hook(&repo).status.success());
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "t",
        ],
    );

    // Dropping `gate` from the list strands the committed trace's label.
    fs::copy(
        fixture("good.jsonl"),
        repo.join("bench/traces/2026-10-21.jsonl"),
    )
    .unwrap();
    git(&repo, &["add", "."]);
    fs::write(repo.join("bench/allowed-labels.txt"), "rule cargo-test\n").unwrap();
    git(&repo, &["add", "bench/allowed-labels.txt"]);
    let out = hook(&repo);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("bench/traces/2026-10-21.jsonl:2: free text in 'rule'"),
        "{}",
        stderr(&out)
    );
}

fn commit(repo: &Path, msg: &str) {
    git(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            msg,
        ],
    );
}

/// A symlink target that names someone: git would publish it as the blob.
#[cfg(unix)]
fn leaky_target(tmp: &Path) -> PathBuf {
    let dir = tmp.join("jane.doe@example.com");
    fs::create_dir_all(&dir).unwrap();
    fs::copy(fixture("good.jsonl"), dir.join("good.jsonl")).unwrap();
    dir
}

/// The trace_symlink_typechange repro: a committed trace replaced by a
/// symlink stages as T, which the hook's filter used to leave out.
#[cfg(unix)]
#[test]
fn pre_commit_refuses_a_trace_typechanged_to_a_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = hook_repo(tmp.path());
    let target = leaky_target(tmp.path());
    let trace = repo.join("bench/traces/2026-01-01.jsonl");
    fs::copy(fixture("good.jsonl"), &trace).unwrap();
    git(&repo, &["add", "."]);
    commit(&repo, "base");

    fs::remove_file(&trace).unwrap();
    std::os::unix::fs::symlink(target.join("good.jsonl"), &trace).unwrap();
    git(&repo, &["add", "bench/traces/2026-01-01.jsonl"]);
    let out = hook(&repo);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("not a regular file"), "{err}");
    assert_name_withheld(&err);
    assert!(!err.contains("2026-01-01"), "{err}");
}

/// A new symlink (A) and one already committed, re-checked when the
/// labels change: both are refused on mode, not on the blob's content.
#[cfg(unix)]
#[test]
fn pre_commit_refuses_new_and_committed_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = hook_repo(tmp.path());
    let target = leaky_target(tmp.path());
    let link = repo.join("bench/traces/2026-01-02.jsonl");
    std::os::unix::fs::symlink(target.join("good.jsonl"), &link).unwrap();
    git(&repo, &["add", "."]);
    let out = hook(&repo);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("not a regular file"),
        "{}",
        stderr(&out)
    );
    assert_name_withheld(&stderr(&out));

    commit(&repo, "link");
    fs::write(repo.join("bench/allowed-labels.txt"), "rule gate\n").unwrap();
    git(&repo, &["add", "bench/allowed-labels.txt"]);
    let out = hook(&repo);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("not a regular file"),
        "{}",
        stderr(&out)
    );
    assert_name_withheld(&stderr(&out));
}

/// CI mode refuses a symlinked file and a symlinked dir under
/// bench/traces/, even when the name and the target's content are good.
#[cfg(unix)]
#[test]
fn default_mode_refuses_symlinks_without_echoing_them() {
    let tmp = tempfile::tempdir().unwrap();
    let target = leaky_target(tmp.path());
    for (name, to) in [
        ("2026-01-01.jsonl", target.join("good.jsonl")),
        ("2026-01-02", target.clone()),
    ] {
        let repo_tmp = tempfile::tempdir().unwrap();
        let repo = hook_repo(repo_tmp.path());
        std::os::unix::fs::symlink(&to, repo.join("bench/traces").join(name)).unwrap();
        let out = Command::new(repo.join("bench/check-trace"))
            .env("HOME", repo_tmp.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("1 symlink(s)"), "{name}: {err}");
        assert_name_withheld(&err);
        assert!(!err.contains(name), "{name}: {err}");
    }
}

/// A rename's raw record carries two paths; the hook must check the
/// destination, or `git mv x.jsonl bench/traces/<leak>` slips past.
#[test]
fn pre_commit_checks_the_destination_of_a_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = hook_repo(tmp.path());
    fs::copy(fixture("good.jsonl"), repo.join("x.jsonl")).unwrap();
    git(&repo, &["add", "."]);
    commit(&repo, "base");
    let name = "bench/traces/jane.doe@example.com.jsonl";
    git(&repo, &["mv", "x.jsonl", name]);
    let out = hook(&repo);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("names withheld"), "{}", stderr(&out));
    assert_name_withheld(&stderr(&out));
}

/// Fake identifiers the trace_leak_refute repros smuggled outside a trace.
const LEAKY_PROSE: &str = "owner: jane.doe@example.com host: jdoe-laptop.corp.example /home/jdoe\n";

/// Repro 1: a data file or git attributes elsewhere under bench/ is refused
/// by the hook and CI mode, on its name alone and without echoing it.
#[test]
fn data_files_outside_traces_are_refused() {
    for name in [
        "bench/NOTES.jsonl",
        "bench/sub/x.json",
        "bench/.gitattributes",
        "bench/notes.private.txt",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let repo = hook_repo(tmp.path());
        let path = repo.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, LEAKY_PROSE).unwrap();
        git(&repo, &["add", "."]);
        let out = hook(&repo);
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("names withheld"),
            "{name}: {}",
            stderr(&out)
        );
        assert_name_withheld(&stderr(&out));

        let out = Command::new(repo.join("bench/check-trace"))
            .env("HOME", tmp.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("1 file(s) outside traces/"),
            "{name}: {}",
            stderr(&out)
        );
        assert_name_withheld(&stderr(&out));
    }
}

/// Human-written prose under bench/ is reviewed like code, not scanned
/// (bench/README.md "Threat model"): the hook and CI mode pass it.
#[test]
fn prose_outside_traces_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = hook_repo(tmp.path());
    fs::write(repo.join("bench/README.md"), LEAKY_PROSE).unwrap();
    fs::write(repo.join("bench/NOTES.txt"), "see the spec, D40\n").unwrap();
    git(&repo, &["add", "."]);
    let out = hook(&repo);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = Command::new(repo.join("bench/check-trace"))
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
}

/// Repro 2: a comment in allowed-labels.txt is published too, so the hook
/// and CI mode scan it, and never echo the match.
#[test]
fn identifiers_in_label_comments_are_refused() {
    let token = format!("ghp_{}", "x".repeat(36));
    for comment in [
        "# contact jane.doe@example.com".to_string(),
        "# see /home/jdoe".into(),
        "# host otherhost.example".into(),
        format!("# {token}"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let repo = hook_repo(tmp.path());
        let labels = repo.join("bench/allowed-labels.txt");
        let mut text = fs::read_to_string(&labels).unwrap();
        text.push_str(&comment);
        text.push('\n');
        fs::write(&labels, text).unwrap();
        git(&repo, &["add", "."]);
        let out = hook(&repo);
        assert_eq!(out.status.code(), Some(1), "{comment}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("in the labels file"), "{comment}: {err}");
        assert_name_withheld(&err);
        assert!(!err.contains("jdoe"), "{comment}: {err}");

        let out = Command::new(repo.join("bench/check-trace"))
            .env("HOME", tmp.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{comment}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("in the labels file"), "{comment}: {err}");
        assert_name_withheld(&err);
        assert!(!err.contains("jdoe"), "{comment}: {err}");
    }
}

/// Repro 3: a name or alias as a trace filename suffix. Only a counter may
/// follow the date, so the hook, CI mode and from-museum --out refuse it
/// whoever the local user is.
#[test]
fn a_free_text_trace_suffix_is_refused_everywhere() {
    let good = fixture("good.jsonl");
    for name in ["2026-01-01-jdoe.jsonl", "2026-01-01-janedoe.jsonl"] {
        let out = check_default(&[(name, &good)]);
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        assert!(!stderr(&out).contains("doe"), "{}", stderr(&out));

        let tmp = tempfile::tempdir().unwrap();
        let repo = hook_repo(tmp.path());
        fs::copy(&good, repo.join("bench/traces").join(name)).unwrap();
        git(&repo, &["add", "."]);
        let out = hook(&repo);
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        assert!(!stderr(&out).contains("doe"), "{}", stderr(&out));

        let out_path = tmp.path().join("traces").join(name);
        let calls = fixture("museum-calls.json");
        let out = script(
            tmp.path(),
            "from-museum",
            &[
                "--date",
                "2026-10-21",
                "--calls",
                calls.to_str().unwrap(),
                "--out",
                out_path.to_str().unwrap(),
                "--muzzle",
                env!("CARGO_BIN_EXE_muzzle"),
            ],
        );
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("--out must be named"),
            "{}",
            stderr(&out)
        );
        assert!(!out_path.exists());
    }
}
