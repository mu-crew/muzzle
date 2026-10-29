//! `muzzle run` as real processes (spec §Testing, Integration tests): each
//! test gets a private `XDG_STATE_HOME` and `XDG_CONFIG_HOME`, and a stored
//! budget of 4 slots (D17a) seeded into `state.json`.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use muzzle::state::{Budget, State, Store};
use serde_json::Value;

const BUDGET: u32 = 4;
/// Small enough that the OOM test needs only a 300 MB allocation; the
/// other jobs are `sleep` and `sh`.
const RAM_PER_SLOT: u64 = 64 << 20;

struct Env {
    _dir: tempfile::TempDir,
    state: PathBuf,
    config: PathBuf,
    scratch: PathBuf,
    /// `MUZZLE_UNIT_PREFIX`: this test's scopes, told apart from other
    /// suites' on a shared host.
    units: String,
}

const CONFIG: &str = r#"
[[rule]]
name = "gate"
match = 'gate'
slots = 2
"#;

impl Env {
    fn new() -> Env {
        Env::with_config(CONFIG)
    }

    fn with_config(config: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let env = Env {
            state: dir.path().join("state"),
            config: dir.path().join("config"),
            scratch: dir.path().join("scratch"),
            units: format!("muzzle-t{}x{}", std::process::id(), next_env()),
            _dir: dir,
        };
        for d in [&env.config.join("muzzle"), &env.scratch] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(env.config.join("muzzle/config.toml"), config).unwrap();
        let store = env.store();
        store
            .update(now_ms(), |s| {
                s.budget = Some(Budget {
                    budget: BUDGET,
                    cores: BUDGET + 2,
                    ram_per_slot: RAM_PER_SLOT,
                });
                Ok(())
            })
            .unwrap();
        env
    }

    fn store(&self) -> Store {
        Store::open(self.state.join("muzzle")).unwrap()
    }

    fn state(&self) -> State {
        self.store().read().unwrap()
    }

    fn runs(&self) -> Vec<Value> {
        std::fs::read_to_string(self.state.join("muzzle/runs.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }

    /// `muzzle run ARGS…`, in the caller's group (muzzle is not a leader).
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_muzzle"));
        c.arg("run")
            .args(args)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("MUZZLE_UNIT_PREFIX", &self.units)
            // A test run under muzzle must not read as nested.
            .env_remove("MUZZLE_HOLD")
            .env_remove("MUZZLE_CLASS")
            .env_remove("MU_MANAGED_AGENT")
            .env_remove("MU_AGENT_NAME")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    /// As pi runs it: muzzle leads its own group (D16).
    fn leader(&self, args: &[&str]) -> Command {
        let mut c = self.cmd(args);
        c.process_group(0);
        c
    }

    /// Hold every slot with `sleep SECS` until killed or done.
    fn hog(&self, secs: &str) -> Child {
        let child = self
            .leader(&["--slots", "4", "--", "sleep", secs])
            .spawn()
            .unwrap();
        job_started(&child, "sleep");
        child
    }
}

/// No run may leave its scope loaded, `failed` or otherwise: such units pile
/// up on the host until someone resets or stops them (75 empty `active`
/// scopes once did). A keeper or pruner may still be finishing, so allow a
/// few seconds.
impl Drop for Env {
    fn drop(&mut self) {
        if std::thread::panicking() || !muzzle::proc::systemd_user_reachable() {
            return;
        }
        let left = eventually(Duration::from_secs(5), || {
            loaded_units(&self.units).is_empty()
        });
        assert!(left, "scopes left: {:?}", loaded_units(&self.units));
    }
}

fn loaded_units(prefix: &str) -> Vec<String> {
    let out = Command::new("systemctl")
        .args(["--user", "list-units", "--all", "--plain"])
        .args(["--no-legend", &format!("{prefix}-*")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

fn next_env() -> u32 {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn eventually(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

fn kill(pid: i32, sig: i32) {
    // SAFETY: plain syscall; a negative pid targets a group.
    unsafe { libc::kill(pid, sig) };
}

fn wait_for(child: &mut Child, within: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return Some(s);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stderr_of(child: Child) -> String {
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Wait until a leader muzzle's group runs `program` itself. Held slots
/// only mean admitted, and until the job execs, the member is a forked
/// muzzle or `systemd-run` setting up the scope: a SIGKILL to muzzle then
/// takes the job down before it starts (seen 6/10 on linuxpc), which
/// empties the group and rightly frees the slots.
fn job_started(muzzle: &Child, program: &str) {
    let pgid = muzzle.id() as i32;
    let running = || {
        muzzle::proc::group_members(pgid)
            .unwrap()
            .iter()
            .any(|&p| p != pgid && ps_comm(p).is_some_and(|c| c == program))
    };
    assert!(
        eventually(Duration::from_secs(5), running),
        "{program} never started"
    );
}

fn ps_comm(pid: i32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let comm = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // macOS ps prints the full path.
    Some(comm.rsplit('/').next().unwrap_or("").to_string())
}

fn waiters(env: &Env) -> usize {
    env.state().waiters.len()
}

fn group_empty(pgid: i32) -> bool {
    // `/proc` or `ps`: a portable check that no process has this pgid.
    let out = Command::new("ps")
        .args(["-A", "-o", "pgid="])
        .output()
        .unwrap();
    !String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.trim().parse() == Ok(pgid))
}

#[test]
fn twenty_concurrent_mixed_claims_never_exceed_the_budget() {
    let env = Env::new();
    let mut children: Vec<Child> = (0..20)
        .map(|i| {
            let slots = (i % 4 + 1).to_string();
            env.cmd(&["--slots", &slots, "--", "sleep", "0.2"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut max_held = 0;
    let deadline = Instant::now() + Duration::from_secs(60);
    while children.iter_mut().any(|c| c.try_wait().unwrap().is_none()) {
        assert!(Instant::now() < deadline, "20 claims didn't finish in 60 s");
        let held = env.state().held();
        assert!(held <= BUDGET, "held {held} > budget {BUDGET}");
        max_held = max_held.max(held);
        std::thread::sleep(Duration::from_millis(20));
    }
    for c in &mut children {
        assert!(c.wait().unwrap().success());
    }
    assert!(max_held > 1, "never ran two at once (max {max_held})");
    assert_eq!(env.runs().len(), 20);
    let s = env.state();
    assert!(s.holders.is_empty() && s.waiters.is_empty());
}

#[test]
fn sigkill_of_a_waiter_advances_the_queue() {
    let env = Env::new();
    let mut hog = env
        .leader(&["--slots", "3", "--", "sleep", "30"])
        .spawn()
        .unwrap();
    job_started(&hog, "sleep");
    // A 4-slot head blocks a 1-slot job behind it (D11).
    let mut head = env.cmd(&["--slots", "4", "--", "true"]).spawn().unwrap();
    assert!(eventually(Duration::from_secs(5), || waiters(&env) == 1));
    let mut small = env.cmd(&["--slots", "1", "--", "true"]).spawn().unwrap();
    assert!(eventually(Duration::from_secs(5), || waiters(&env) == 2));
    std::thread::sleep(Duration::from_millis(600));
    assert!(small.try_wait().unwrap().is_none(), "jumped the head");

    kill(head.id() as i32, libc::SIGKILL);
    let killed = Instant::now();
    assert!(
        wait_for(&mut small, Duration::from_secs(3))
            .unwrap()
            .success()
    );
    assert!(
        killed.elapsed() < Duration::from_millis(1500),
        "{:?}",
        killed.elapsed()
    );
    head.wait().unwrap();
    kill(-(hog.id() as i32), libc::SIGKILL);
    hog.wait().unwrap();
}

#[test]
fn sigkill_of_the_whole_group_kills_the_job_and_frees_the_slots() {
    let env = Env::new();
    let mut hog = env.hog("30");
    let pgid = hog.id() as i32;
    let mut next = env.cmd(&["--slots", "1", "--", "true"]).spawn().unwrap();
    assert!(eventually(Duration::from_secs(5), || waiters(&env) == 1));

    // pi on Esc: SIGKILL to the bash group, which muzzle leads.
    kill(-pgid, libc::SIGKILL);
    let killed = Instant::now();
    hog.wait().unwrap();
    assert!(
        wait_for(&mut next, Duration::from_secs(3))
            .unwrap()
            .success()
    );
    assert!(
        killed.elapsed() < Duration::from_millis(1500),
        "{:?}",
        killed.elapsed()
    );
    assert!(eventually(Duration::from_secs(2), || group_empty(pgid)));
    let runs = env.runs();
    assert_eq!(runs[0]["killed"], "holder");
    assert_eq!(runs[0]["slots"], 4);
}

#[test]
fn sigkill_of_only_the_muzzle_keeps_the_slots_until_the_group_exits() {
    let env = Env::new();
    let mut hog = env
        .leader(&["--slots", "4", "--", "sleep", "2"])
        .spawn()
        .unwrap();
    job_started(&hog, "sleep");
    let started = Instant::now();
    let mut next = env.cmd(&["--slots", "1", "--", "true"]).spawn().unwrap();
    kill(hog.id() as i32, libc::SIGKILL);
    hog.wait().unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    assert!(
        next.try_wait().unwrap().is_none(),
        "admitted while the job ran: {:?}",
        env.runs()
    );
    assert!(
        wait_for(&mut next, Duration::from_secs(4))
            .unwrap()
            .success()
    );
    assert!(started.elapsed() >= Duration::from_millis(1800));
    assert_eq!(env.runs()[0]["killed"], "holder");
}

#[test]
fn background_child_hands_off_to_a_keeper() {
    let env = Env::new();
    let started = Instant::now();
    let mut caller = env
        .leader(&["--slots", "2", "--", "sh", "-c", "sleep 2 & exit 0"])
        // The background sleep inherits muzzle's stdio, as it would pi's.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status = wait_for(&mut caller, Duration::from_secs(1)).expect("returned at once");
    assert!(status.success());
    let s = env.state();
    assert_eq!(s.held(), 2, "keeper holds the slots");
    assert!(s.holders[0].kept);
    assert_ne!(s.holders[0].pid, caller.id(), "the keeper took the entry");
    assert!(env.runs().is_empty());

    assert!(eventually(Duration::from_secs(5), || env.state().held() == 0));
    assert!(started.elapsed() >= Duration::from_millis(1800));
    let runs = env.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["kept"], true);
    assert_eq!(runs[0]["exit"], 0);
    assert!(runs[0]["ran_s"].as_f64().unwrap() >= 1.8, "{}", runs[0]);
}

#[test]
fn keeper_survives_a_group_kill_and_releases_when_the_group_empties() {
    let env = Env::new();
    let mut caller = env
        .leader(&["--slots", "2", "--", "sh", "-c", "sleep 30 & exit 0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        wait_for(&mut caller, Duration::from_secs(1))
            .unwrap()
            .success()
    );
    assert!(env.state().holders[0].kept);
    let keeper = env.state().holders[0].pid;

    kill(-(caller.id() as i32), libc::SIGKILL);
    let killed = Instant::now();
    assert!(eventually(Duration::from_secs(3), || env.state().held() == 0));
    assert!(
        killed.elapsed() < Duration::from_millis(1500),
        "{:?}",
        killed.elapsed()
    );
    let runs = env.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["kept"], true);
    assert_eq!(
        runs[0]["killed"],
        Value::Null,
        "the keeper wrote it, not a pruner"
    );
    assert!(eventually(Duration::from_secs(2), || {
        muzzle::proc::start_time(keeper as i32).is_none()
    }));
}

#[test]
fn oom_kill_is_recorded_on_linux() {
    if cfg!(not(target_os = "linux")) || !muzzle::proc::systemd_user_reachable() {
        eprintln!("skipped: needs a systemd user manager (D15)");
        return;
    }
    let env = Env::new();
    let out = env
        .cmd(&[
            "--slots",
            "1",
            "--",
            "perl",
            "-e",
            r#"$x = "x" x (300 * 1024 * 1024); print length $x"#,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(137), "{out:?}");
    let runs = env.runs();
    assert_eq!(runs[0]["killed"], "oom", "{}", runs[0]);
    assert_eq!(runs[0]["enforced"], true);
    assert!(
        runs[0]["mem_peak_mb"].as_u64().unwrap() >= 60,
        "{}",
        runs[0]
    );
}

/// OOMPolicy=stop SIGTERMs the top command; one that handles TERM exits
/// with whatever code it likes, and the unit's Result still says oom-kill.
#[test]
fn oom_kill_is_recorded_when_the_top_survives_term() {
    if cfg!(not(target_os = "linux")) || !muzzle::proc::systemd_user_reachable() {
        eprintln!("skipped: needs a systemd user manager (D15)");
        return;
    }
    let env = Env::new();
    let out = env
        .cmd(&[
            "--slots",
            "1",
            "--",
            "sh",
            "-c",
            r#"trap "" TERM; perl -e '$x = "x" x (300 * 1024 * 1024)'; exit 1"#,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(env.runs()[0]["killed"], "oom", "{}", env.runs()[0]);
}

/// pi on Esc can SIGKILL the group while systemd-run is still setting up
/// the scope, which leaves the unit `failed` (Result=resources). The next
/// muzzle to prune the holder resets it; `Env`'s drop checks none is left.
#[test]
fn a_scope_killed_during_setup_is_not_left_failed() {
    let env = Env::new();
    for i in 0..10u64 {
        let mut hog = env
            .leader(&["--slots", "4", "--", "sleep", "30"])
            .spawn()
            .unwrap();
        // Wait for admission, then kill at a spread of points in the setup.
        assert!(eventually(Duration::from_secs(5), || env.state().held() == 4));
        std::thread::sleep(Duration::from_millis(i * 3));
        kill(-(hog.id() as i32), libc::SIGKILL);
        hog.wait().unwrap();
        // The next run prunes the dead holder.
        let out = env.cmd(&["--", "true"]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
    }
}

#[test]
fn nested_run_through_ancestry_starts_at_once() {
    let env = Env::new();
    let me = env!("CARGO_BIN_EXE_muzzle");
    let inner = format!("env -u MUZZLE_HOLD {me} run --slots 4 -- echo inner");
    let out = env
        .cmd(&["--slots", "4", "--timeout", "5", "--", "sh", "-c", &inner])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "inner\n");
    assert_eq!(env.runs().len(), 1, "nested runs are not logged");
}

/// The inner muzzle is started by a double fork plus setsid(2), so by the
/// time it looks, its parent chain is init (or a subreaper) and its group
/// is its own: only `MUZZLE_HOLD` can tell it is nested.
fn detached_inner(env: &Env, hold: &str) -> String {
    let me = env!("CARGO_BIN_EXE_muzzle");
    let mark = env.file("inner-ran");
    format!(
        // macOS has no setsid(1); perl's POSIX::setsid is on both platforms.
        "{hold} (perl -MPOSIX -e 'POSIX::setsid() or die; exec @ARGV' sh -c 'sleep 0.3; exec {me} run --slots 4 --timeout 2 -- touch {m}' &); \
         for i in $(seq 40); do [ -e {m} ] && exit 0; sleep 0.1; done; exit 9",
        m = mark.display()
    )
}

#[test]
fn nested_run_through_muzzle_hold_with_ancestry_broken() {
    let env = Env::new();
    let script = detached_inner(&env, "");
    let out = env
        .cmd(&["--slots", "4", "--", "sh", "-c", &script])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn without_muzzle_hold_the_detached_run_queues() {
    // The control for the test above: same shape, token removed.
    let env = Env::new();
    let script = detached_inner(&env, "unset MUZZLE_HOLD;");
    let out = env
        .cmd(&["--slots", "4", "--", "sh", "-c", &script])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(9), "{out:?}");
}

#[test]
fn stale_muzzle_hold_queues_normally() {
    let env = Env::new();
    let report = env.file("first.json");
    let out = env
        .cmd(&["--report", report.to_str().unwrap(), "--", "true"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let mut hog = env.hog("30");
    // Ticket 1 finished: its token names a dead holder.
    let out = env
        .cmd(&["--slots", "1", "--timeout", "1", "--", "true"])
        .env("MUZZLE_HOLD", format!("1:{}:1", std::process::id()))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(75), "{out:?}");
    kill(-(hog.id() as i32), libc::SIGKILL);
    hog.wait().unwrap();
}

#[test]
fn run_timeout_counts_from_admission() {
    let env = Env::new();
    let mut hog = env.hog("2");
    let report = env.file("r.json");
    let started = Instant::now();
    let out = env
        .cmd(&[
            "--run-timeout",
            "1",
            "--report",
            report.to_str().unwrap(),
            "--",
            "sleep",
            "10",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(124), "{out:?}");
    assert!(started.elapsed() >= Duration::from_millis(2500));
    assert!(started.elapsed() < Duration::from_secs(6));
    let r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    let ran = r["ran_s"].as_f64().unwrap();
    assert!((0.9..2.0).contains(&ran), "{r}");
    assert!(r["waited_s"].as_f64().unwrap() >= 1.0, "{r}");
    assert_eq!(r["exit"], 124);
    assert_eq!(env.runs().last().unwrap()["killed"], "timeout");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("muzzle: admitted after "), "{err}");
    hog.wait().unwrap();
}

/// D18: the SIGKILL comes 10 s after the SIGTERM, even when the top
/// command dies at once: a `bash -c` top's children get their TERM grace.
#[test]
fn run_timeout_gives_children_the_term_grace() {
    let env = Env::new();
    let cleaned = env.file("cleaned");
    let inner = format!(
        "trap 'sleep 2; touch {}; exit 0' TERM; while :; do sleep 0.1; done",
        cleaned.display()
    );
    let started = Instant::now();
    let out = env
        .cmd(&[
            "--run-timeout",
            "1",
            "--",
            "bash",
            "-c",
            r#"bash -c "$0" & wait"#,
            &inner,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(124), "{out:?}");
    assert!(cleaned.exists(), "the child's TERM handler was cut short");
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(env.runs()[0]["killed"], "timeout");
}

#[test]
fn run_timeout_escalates_to_sigkill_after_the_grace() {
    let env = Env::new();
    let started = Instant::now();
    let out = env
        .cmd(&[
            "--run-timeout",
            "0.5",
            "--",
            "sh",
            "-c",
            r#"trap "" TERM; sleep 30"#,
        ])
        .output()
        .unwrap();
    let took = started.elapsed();
    assert_eq!(out.status.code(), Some(124), "{out:?}");
    assert!(
        (Duration::from_secs(10)..Duration::from_secs(12)).contains(&took),
        "{took:?}"
    );
    assert_eq!(env.runs()[0]["killed"], "timeout");
}

#[test]
fn queue_timeout_exits_75_and_drops_the_ticket() {
    let env = Env::new();
    let mut hog = env.hog("30");
    let out = env.cmd(&["--timeout", "1", "--", "true"]).output().unwrap();
    assert_eq!(out.status.code(), Some(75), "{out:?}");
    assert_eq!(waiters(&env), 0);
    kill(-(hog.id() as i32), libc::SIGKILL);
    hog.wait().unwrap();
}

#[test]
fn sigterm_while_queued_prints_d19_and_drops_the_ticket() {
    let env = Env::new();
    let mut hog = env.hog("30");
    let mut waiter = env.cmd(&["--", "true"]).spawn().unwrap();
    assert!(eventually(Duration::from_secs(5), || waiters(&env) == 1));
    kill(waiter.id() as i32, libc::SIGTERM);
    let status = wait_for(&mut waiter, Duration::from_secs(2)).unwrap();
    assert_eq!(status.code(), Some(143));
    let err = stderr_of(waiter);
    assert!(
        err.contains("muzzle: killed while queued (position 1/1, waited ")
            && err.contains("Put timeout inside the command: muzzle run -- timeout N …"),
        "{err}"
    );
    assert_eq!(waiters(&env), 0);
    kill(-(hog.id() as i32), libc::SIGKILL);
    hog.wait().unwrap();
}

/// rv_skill_unknown_rule: a name the config lacks warns, and otherwise
/// runs as before (1 slot, or `--slots`, exit passed through).
#[test]
fn an_unknown_rule_warns_and_still_runs() {
    let env = Env::new();
    for (args, n) in [
        (
            &["--rule", "no-such-rule", "--", "sh", "-c", "exit 3"][..],
            1,
        ),
        (
            &["--rule", "nope", "--slots", "2", "--", "sh", "-c", "exit 3"][..],
            2,
        ),
    ] {
        let out = env.cmd(args).output().unwrap();
        assert_eq!(out.status.code(), Some(3), "{out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        let name = args[1];
        assert!(
            err.contains(&format!(
                "muzzle: rule {name} is not in the config; claiming {n} slot(s)"
            )),
            "{err}"
        );
        assert_eq!(env.runs().pop().unwrap()["slots"], n);
    }
    let out = env.cmd(&["--rule", "gate", "--", "true"]).output().unwrap();
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("not in the config"),
        "{out:?}"
    );
}

#[test]
fn a_run_appends_its_line_to_runs_jsonl() {
    let env = Env::new();
    let busy = "i=0; while [ $i -lt 300000 ]; do i=$((i+1)); done";
    let out = env
        .cmd(&[
            "--rule", "gate", "--class", "normal", "--", "sh", "-c", busy,
        ])
        .env("MU_AGENT_NAME", "worker-7")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let runs = env.runs();
    assert_eq!(runs.len(), 1);
    let r = &runs[0];
    for key in [
        "v",
        "end",
        "rule",
        "slots",
        "class",
        "cfg",
        "agent",
        "cwd",
        "waited_s",
        "ran_s",
        "exit",
        "killed",
        "cpu_s",
        "mem_peak_mb",
        "enforced",
        "nested",
        "kept",
        "budget",
    ] {
        assert!(r.get(key).is_some(), "missing {key}: {r}");
    }
    assert_eq!(r["rule"], "gate");
    assert_eq!(r["slots"], 2, "the rule's slots");
    assert_eq!(r["class"], "normal");
    assert_eq!(r["agent"], "worker-7");
    assert_eq!(r["budget"], BUDGET);
    assert_eq!(r["exit"], 0);
    assert_eq!(r["kept"], false);
    assert!(r["cpu_s"].as_f64().unwrap() > 0.0, "{r}");
    assert!(r["mem_peak_mb"].as_u64().unwrap() > 0, "{r}");
    assert_eq!(r["cfg"].as_str().unwrap().len(), 6);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("muzzle: waited 0s for 2 slots, ran "),
        "{err}"
    );
    assert!(
        !err.contains("admitted after"),
        "no admitted line without a wait"
    );
}

#[test]
fn exit_codes() {
    let env = Env::new();
    let code = |args: &[&str]| env.cmd(args).output().unwrap().status;
    assert_eq!(code(&["--", "sh", "-c", "exit 7"]).code(), Some(7));
    assert_eq!(code(&["--", "sh", "-c", "kill -TERM $$"]).code(), Some(143));
    assert_eq!(code(&["--", "sh", "-c", "kill -KILL $$"]).code(), Some(137));
    assert_eq!(code(&["--", "/nonexistent/cmd"]).code(), Some(127));
    let bad = Env::with_config("bogus = 1\n");
    let out = bad.cmd(&["--", "true"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("bogus"));
    let out = env
        .cmd(&["--timeout", "-1", "--", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    // Signals from the caller reach a job in its own group.
    let mut c = env.cmd(&["--", "sleep", "30"]).spawn().unwrap();
    assert!(eventually(Duration::from_secs(5), || env.state().held() == 1));
    kill(c.id() as i32, libc::SIGTERM);
    let s = wait_for(&mut c, Duration::from_secs(3)).unwrap();
    assert_eq!((s.code(), s.signal()), (Some(143), None));
    let last = env.runs().pop().unwrap();
    assert_eq!(
        (&last["killed"], &last["exit"]),
        (&"signal".into(), &143.into())
    );
}

#[test]
fn the_claim_sets_the_job_counts_and_shell_text_reaches_bash_intact() {
    let env = Env::new();
    let out = env
        .cmd(&[
            "--slots",
            "3",
            "--",
            "sh",
            "-c",
            r#"echo "$MUZZLE_SLOTS $CARGO_BUILD_JOBS $RUST_TEST_THREADS $MAKEFLAGS"; echo '$HOME'; echo "${MUZZLE_HOLD%%:*}""#,
        ])
        .env("CARGO_BUILD_JOBS", "99")
        .env("MAKEFLAGS", "-j99")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    // systemd-run expands `$VAR` unless told not to (systemd ≥ 254).
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "3 3 3 -j3\n$HOME\n1\n"
    );
}

#[test]
fn report_file_has_the_spec_fields() {
    let env = Env::new();
    let report = env.file("r.json");
    let out = env
        .cmd(&[
            "--rule",
            "gate",
            "--report",
            report.to_str().unwrap(),
            "--",
            "true",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty(), "--report replaces the stderr line");
    let r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(r["slots"], 2);
    assert_eq!(r["class"], "high", "unmanaged agents default to high (D12)");
    assert_eq!(r["rule"], "gate");
    assert_eq!(r["exit"], 0);
    assert_eq!(r["kept"], false);
    assert!(r["enforced"].is_boolean());
    assert!(r["waited_s"].is_f64() && r["ran_s"].is_f64());
}

/// `admitted_ms`/`released_ms` (additive, epoch ms) are read under the
/// state lock, so slots that pass from one run to the next never overlap:
/// the bench counts slots held from them.
#[test]
fn report_times_hand_slots_over_without_overlap() {
    let env = Env::new();
    let (a, b) = (env.file("a.json"), env.file("b.json"));
    let first = env
        .leader(&[
            "--slots",
            "4",
            "--report",
            a.to_str().unwrap(),
            "--",
            "sleep",
            "1",
        ])
        .spawn()
        .unwrap();
    job_started(&first, "sleep");
    let out = env
        .cmd(&[
            "--slots",
            "4",
            "--report",
            b.to_str().unwrap(),
            "--",
            "true",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(first.wait_with_output().unwrap().status.success());
    let read =
        |p: &PathBuf| -> Value { serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap() };
    let (a, b) = (read(&a), read(&b));
    let ms = |r: &Value, k: &str| r[k].as_u64().unwrap_or_else(|| panic!("{k}: {r}"));
    assert!(ms(&a, "admitted_ms") <= ms(&a, "released_ms"), "{a}");
    assert!(ms(&a, "released_ms") <= ms(&b, "admitted_ms"), "{a} {b}");
    assert!(ms(&b, "admitted_ms") <= ms(&b, "released_ms"), "{b}");
    // B queued behind A's whole second of sleep.
    assert!(b["waited_s"].as_f64().unwrap() >= 0.5, "{b}");
}

/// D16a: a fake `podman` on `PATH`. `podman exec <box> CMD…` runs CMD the
/// way conmon does: in a session of its own, so a group kill misses it,
/// but still a descendant, so it stays inside muzzle's scope.
fn fake_podman(env: &Env) -> String {
    let bin = env.scratch.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("podman");
    std::fs::write(
        &script,
        format!(
            // `entered` only after the fork: a kill between the two would
            // leave the scope empty, and a pruner rightly frees it.
            "#!/bin/sh\n[ \"$1\" = exec ] || exit 2\nshift 2\nsetsid \"$@\" &\ntouch {}\nwait\n",
            env.file("entered").display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[test]
fn a_container_run_is_recorded_as_not_enforced() {
    let env = Env::new();
    let path = fake_podman(&env);
    let report = env.file("r.json");
    let out = env
        .cmd(&[
            "--rule",
            "gate",
            "--report",
            report.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            "podman exec box true",
        ])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let r = &env.runs()[0];
    assert_eq!(r["enforced"], false, "{r}");
    assert_eq!(r["enforced_reason"], "container", "{r}");
    // The scope saw only podman and conmon: no figures rather than wrong ones.
    assert!(r["cpu_s"].is_null() && r["mem_peak_mb"].is_null(), "{r}");
    let rep: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(rep["enforced"], false);
    assert_eq!(rep["enforced_reason"], "container");
    // Both additive sets survive together: D35's lock times and D16a's reason.
    assert!(
        rep["admitted_ms"].is_u64() && rep["released_ms"].is_u64(),
        "{rep}"
    );

    // A host run gets no reason field in either place.
    let out = env
        .cmd(&["--report", report.to_str().unwrap(), "--", "true"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(env.runs()[1].get("enforced_reason").unwrap().is_null());
    let rep: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert!(rep.get("enforced_reason").is_none(), "{rep}");
}

#[test]
fn group_kill_of_a_container_gate_holds_the_slots_until_its_scope_empties() {
    if cfg!(not(target_os = "linux")) || !muzzle::proc::systemd_user_reachable() {
        eprintln!("skipped: needs a systemd user manager (D15, D16a)");
        return;
    }
    let env = Env::new();
    let path = fake_podman(&env);
    let mut gate = env
        .leader(&["--slots", "4", "--", "podman", "exec", "box", "sleep", "3"])
        .env("PATH", &path)
        .spawn()
        .unwrap();
    assert!(eventually(Duration::from_secs(5), || env
        .file("entered")
        .exists()));
    let started = Instant::now();
    let h = &env.state().holders[0];
    assert_eq!(h.enforced_reason.map(|_| ()), Some(()));
    assert!(
        h.scope
            .as_deref()
            .is_some_and(|u| u.starts_with(&env.units))
    );

    let mut next = env.cmd(&["--slots", "1", "--", "true"]).spawn().unwrap();
    // pi on Esc: muzzle and its group die, the "in-container" sleep doesn't.
    kill(-(gate.id() as i32), libc::SIGKILL);
    gate.wait().unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    assert!(
        next.try_wait().unwrap().is_none(),
        "admitted while the container job ran: {:?}",
        env.runs()
    );
    let status = muzzle_status(&env);
    assert_eq!(status["held"], 4, "{status}");
    assert_eq!(status["holders"][0]["enforced_reason"], "container");

    assert!(
        wait_for(&mut next, Duration::from_secs(6))
            .unwrap()
            .success()
    );
    assert!(started.elapsed() >= Duration::from_millis(2800));
    let r = &env.runs()[0];
    assert_eq!(r["killed"], "holder", "{r}");
    assert_eq!(r["enforced_reason"], "container", "{r}");
}

fn muzzle_status(env: &Env) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .args(["status", "--json"])
        .env("XDG_STATE_HOME", &env.state)
        .env("XDG_CONFIG_HOME", &env.config)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

/// rv_cg_graceful_stop_frees_container: muzzle exits through its own
/// release path (SIGTERM, Ctrl-C, --run-timeout), but conmon's job lives on
/// in the scope. The slots stay held until it ends.
fn container_job_outlives_a_graceful_stop(stop: &str) {
    if cfg!(not(target_os = "linux")) || !muzzle::proc::systemd_user_reachable() {
        eprintln!("skipped: needs a systemd user manager (D15, D16a)");
        return;
    }
    let env = Env::new();
    let path = fake_podman(&env);
    let report = env.file("r.json");
    let mut args = vec!["--slots", "4", "--report", report.to_str().unwrap()];
    if stop == "timeout" {
        args.extend(["--run-timeout", "0.5"]);
    }
    args.extend(["--", "podman", "exec", "box", "sleep", "3"]);
    let mut gate = env.leader(&args).env("PATH", &path).spawn().unwrap();
    assert!(eventually(Duration::from_secs(5), || env
        .file("entered")
        .exists()));
    let started = Instant::now();
    let h = &env.state().holders[0];
    // The probe is recorded at spawn: path, and the namespaces it's valid in.
    let probe = h.probe.as_ref().expect("probe recorded");
    assert!(
        probe
            .cgroup
            .as_deref()
            .is_some_and(|c| c.ends_with(".scope")),
        "{probe:?}"
    );
    match stop {
        "term" => kill(gate.id() as i32, libc::SIGTERM),
        // Ctrl-C in a terminal: the whole group.
        "int" => kill(-(gate.id() as i32), libc::SIGINT),
        _ => {}
    }
    let code = wait_for(&mut gate, Duration::from_secs(5)).expect("muzzle exits");
    let want = match stop {
        "timeout" => Some(124),
        "term" => Some(143),
        _ => Some(130),
    };
    assert_eq!(code.code(), want, "{code:?}");

    let mut next = env.cmd(&["--slots", "1", "--", "true"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        next.try_wait().unwrap().is_none(),
        "admitted while the container job ran: {:?}",
        env.runs()
    );
    let status = muzzle_status(&env);
    assert_eq!(status["held"], 4, "{status}");
    let rep: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(rep["kept"], true, "{rep}");

    assert!(
        wait_for(&mut next, Duration::from_secs(6))
            .unwrap()
            .success()
    );
    assert!(started.elapsed() >= Duration::from_millis(2000));
    let r = &env.runs()[0];
    assert_eq!(r["enforced_reason"], "container", "{r}");
    assert_eq!(r["kept"], true, "{r}");
    if stop == "timeout" {
        assert_eq!(r["killed"], "timeout", "{r}");
    }
}

#[test]
fn sigterm_to_a_container_gate_keeps_the_slots_until_its_scope_empties() {
    container_job_outlives_a_graceful_stop("term");
}

#[test]
fn ctrl_c_to_a_container_gate_keeps_the_slots_until_its_scope_empties() {
    container_job_outlives_a_graceful_stop("int");
}

#[test]
fn run_timeout_of_a_container_gate_keeps_the_slots_until_its_scope_empties() {
    container_job_outlives_a_graceful_stop("timeout");
}
