//! `muzzle run --grace` as real processes (H4, spec 2026-11-23, Verification
//! plan §2): each test gets a private `XDG_STATE_HOME` and `XDG_CONFIG_HOME`,
//! and a stored budget of 4 slots (D17a) seeded into `state.json`.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use muzzle::state::{Budget, State, Store};
use serde_json::Value;

struct Env {
    _dir: tempfile::TempDir,
    state: PathBuf,
    config: PathBuf,
    scratch: PathBuf,
}

/// A pure CPU burn in a bash builtin loop, no forks: `$SECONDS` is bash's.
fn burn(secs: u32) -> String {
    format!("while [ $SECONDS -lt {secs} ]; do :; done")
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let env = Env {
            state: dir.path().join("state"),
            config: dir.path().join("config"),
            scratch: dir.path().join("scratch"),
            _dir: dir,
        };
        std::fs::create_dir_all(env.config.join("muzzle")).unwrap();
        std::fs::create_dir_all(&env.scratch).unwrap();
        env.store()
            .update(now_ms(), |s| {
                s.budget = Some(Budget {
                    budget: 4,
                    cores: 6,
                    ram_per_slot: 1 << 30,
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

    fn state_bytes(&self) -> Vec<u8> {
        std::fs::read(self.state.join("muzzle/state.json")).unwrap()
    }

    fn runs(&self) -> Vec<Value> {
        std::fs::read_to_string(self.state.join("muzzle/runs.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn report(&self, name: &str) -> Value {
        let text = std::fs::read_to_string(self.scratch.join(name)).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// `muzzle ARGS…`, in the caller's group.
    fn muzzle(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_muzzle"));
        c.args(args)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("MUZZLE", env!("CARGO_BIN_EXE_muzzle"))
            .env("MUZZLE_UNIT_PREFIX", "muzzle-grace-test")
            .env_remove("MUZZLE_HOLD")
            .env_remove("MUZZLE_CLASS")
            .env_remove("MU_MANAGED_AGENT")
            .env_remove("MU_AGENT_NAME")
            .env_remove("TMUX_PANE")
            .env_remove("MUZZLE_QUEUE_TIMEOUT")
            .env_remove("MUZZLE_GRACE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    /// `muzzle run --grace … --report <name> -- bash -c SCRIPT`.
    fn grace(&self, flags: &[&str], report: &str, script: &str) -> Command {
        let path = self.scratch.join(report);
        let mut c = self.muzzle(&["run"]);
        c.args(flags)
            .arg("--report")
            .arg(path)
            .args(["--", "bash", "-c", script]);
        c
    }
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

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn wrap_without_with_grace_leaves_unmatched_commands_alone() {
    let env = Env::new();
    let out = env
        .muzzle(&["wrap", "--json", "--", "./mytool"])
        .env("MUZZLE_GRACE", "5")
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["command"].is_null(), "{}", text(&out));
    assert!(v.get("grace").is_none());
}

#[test]
fn a_fast_command_leaves_no_trace_and_reports_unpromoted() {
    let env = Env::new();
    let before = env.state_bytes();
    let out = env
        .grace(&["--grace", "2"], "r.json", "exit 3")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert_eq!(env.state_bytes(), before, "state.json was written");
    assert!(env.runs().is_empty());
    let r = env.report("r.json");
    assert_eq!(r["promoted"], false);
    assert_eq!(r["slots"], 0);
    assert_eq!(r["waited_s"], 0.0);
    assert_eq!(r["exit"], 3);
    assert!(r["killed"].is_null());
    assert!(r["ran_s"].as_f64().unwrap() < 2.0);
}

#[test]
fn a_fast_command_never_creates_state() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .args(["run", "--grace", "2", "--", "true"])
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env_remove("MUZZLE_HOLD")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dir.path().join("state/muzzle/state.json").exists());
    assert!(!dir.path().join("state/muzzle/runs.jsonl").exists());
}

#[test]
fn an_idle_long_command_does_not_promote() {
    let env = Env::new();
    let before = env.state_bytes();
    let out = env
        .grace(
            &["--grace", "0.5", "--grace-cpu", "0.5"],
            "r.json",
            "sleep 2",
        )
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(env.state_bytes(), before);
    assert!(env.runs().is_empty());
    let r = env.report("r.json");
    assert_eq!(r["promoted"], false);
    assert!(r["ran_s"].as_f64().unwrap() >= 2.0);
}

#[test]
fn run_timeout_before_promotion_reports_the_kill() {
    let env = Env::new();
    let out = env
        .grace(
            &["--grace", "5", "--run-timeout", "0.5"],
            "r.json",
            "sleep 30",
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(124), "{}", text(&out));
    let r = env.report("r.json");
    assert_eq!(r["promoted"], false);
    assert_eq!(r["killed"], "timeout");
    assert!(env.runs().is_empty());
}

#[test]
fn a_cpu_burner_promotes_holds_slots_and_is_logged() {
    let env = Env::new();
    let child = env
        .grace(
            &["--grace", "0.5", "--grace-cpu", "0.1"],
            "r.json",
            &burn(3),
        )
        .spawn()
        .unwrap();
    let held = eventually(Duration::from_secs(10), || {
        env.state().holders.iter().any(|h| h.grace)
    });
    assert!(held, "never promoted");
    let s = env.state();
    let h = s.holders.iter().find(|h| h.grace).unwrap();
    assert_eq!(h.claim.rule.as_deref(), Some("unmatched-heavy"));
    assert!((1..=4).contains(&h.claim.slots));
    assert!(!h.enforced);
    assert_eq!(h.waited_s, 0.0);
    assert!(h.admit_held.is_some());
    assert!(h.pgid.is_some());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(env.state().holders.is_empty(), "slots not released");
    let runs = env.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["rule"], "unmatched-heavy");
    assert!(runs[0]["ran_s"].as_f64().unwrap() >= 1.0);
    assert!(runs[0]["cpu_s"].as_f64().unwrap() > 0.5);
    let r = env.report("r.json");
    assert_eq!(r["promoted"], true);
    assert_eq!(r["rule"], "unmatched-heavy");
    assert!(r["killed"].is_null());
    assert!(r["waited_s"].as_f64().unwrap() == 0.0);
}

#[test]
fn an_inner_run_is_not_swallowed_by_a_grace_holder() {
    let env = Env::new();
    let inner = env.scratch.join("inner.json");
    // Burn past promotion, then run a muzzle of its own.
    let script = format!(
        "{}; \"$MUZZLE\" run --slots 2 --report {} -- true",
        burn(2),
        inner.display()
    );
    let out = env
        .grace(&["--grace", "0.3", "--grace-cpu", "0.1"], "r.json", &script)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let r = env.report("inner.json");
    assert_eq!(r["slots"], 2, "inner run was swallowed as nested");
    assert!(!r["admitted_ms"].is_null());
    let runs = env.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(runs.iter().any(|r| r["slots"] == 2 && r["rule"].is_null()));
    assert!(runs.iter().any(|r| r["rule"] == "unmatched-heavy"));
    assert_eq!(env.report("r.json")["promoted"], true);
}

#[test]
fn a_background_survivor_is_monitored_and_promotes() {
    let env = Env::new();
    // The top command exits at once; its child burns on.
    let script = format!("bash -c '{}' >/dev/null 2>&1 & exit 0", burn(4));
    let out = env
        .grace(&["--grace", "0.5", "--grace-cpu", "0.1"], "r.json", &script)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let r = env.report("r.json");
    assert_eq!(r["promoted"], true, "{r}");
    assert_eq!(r["kept"], true);
    // A keeper holds the claim until the survivor is done, then logs it.
    assert!(
        eventually(Duration::from_secs(10), || env.runs().len() == 1),
        "no run line"
    );
    let runs = env.runs();
    assert_eq!(runs[0]["rule"], "unmatched-heavy");
    assert_eq!(runs[0]["kept"], true);
    assert!(env.state().holders.is_empty());
}

#[test]
fn an_unpromoted_run_takes_no_lock_and_leaves_a_corrupt_state_alone() {
    let dir = tempfile::tempdir().unwrap();
    let muzzle = dir.path().join("state/muzzle");
    std::fs::create_dir_all(&muzzle).unwrap();
    std::fs::write(muzzle.join("state.json"), b"{ torn").unwrap();
    // Someone else holds the writer lock for the whole run.
    let lock = std::fs::File::create(muzzle.join("state.lock")).unwrap();
    lock.lock().unwrap();
    let marker = dir.path().join("ran");
    let out = Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .args(["run", "--grace", "2", "--", "touch"])
        .arg(&marker)
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env_remove("MUZZLE_HOLD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let ran = eventually(Duration::from_secs(5), || marker.exists());
    let out = out.wait_with_output().unwrap();
    assert!(ran, "blocked on state.lock: {}", text(&out));
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read(muzzle.join("state.json")).unwrap(), b"{ torn");
    assert!(!text(&out).contains("unreadable"), "{}", text(&out));
}

#[test]
fn run_timeout_kills_an_unpromoted_survivor() {
    let env = Env::new();
    let t = Instant::now();
    let out = env
        .grace(
            &[
                "--grace",
                "5",
                "--grace-cpu",
                "0.01",
                "--run-timeout",
                "0.5",
            ],
            "r.json",
            "sleep 30 </dev/null >/dev/null 2>&1 & exit 0",
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(124), "{}", text(&out));
    assert!(t.elapsed() < Duration::from_secs(15));
    let r = env.report("r.json");
    assert_eq!(r["promoted"], false);
    assert_eq!(r["killed"], "timeout");
}

#[test]
fn run_timeout_kills_a_promoted_survivor_instead_of_handing_off() {
    let env = Env::new();
    let script = format!("bash -c '{}' >/dev/null 2>&1 & exit 0", burn(30));
    let t = Instant::now();
    let out = env
        .grace(
            &[
                "--grace",
                "0.3",
                "--grace-cpu",
                "0.05",
                "--run-timeout",
                "1.5",
            ],
            "r.json",
            &script,
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(124), "{}", text(&out));
    assert!(t.elapsed() < Duration::from_secs(15));
    let r = env.report("r.json");
    assert_eq!(r["promoted"], true, "{r}");
    assert_eq!(r["killed"], "timeout");
    assert_eq!(r["kept"], false);
    assert!(env.state().holders.is_empty());
}

#[test]
fn admitted_ms_is_the_claim_not_the_spawn_and_dead_holders_are_not_held() {
    let env = Env::new();
    // A holder whose muzzle and group are long gone.
    env.store()
        .update(now_ms(), |s| {
            let h = serde_json::from_value(serde_json::json!({
                "ticket": 1, "pid": 999_999_999u32, "start": 1, "pgid": null,
                "run_start_ms": 0, "waited_s": 0.0, "slots": 3,
                "class": "normal", "agent": null, "rule": null, "cwd": "/",
                "cfg": null
            }))
            .unwrap();
            s.next_ticket = 2;
            s.holders.push(h);
            Ok(())
        })
        .unwrap();
    let t0 = now_ms();
    let out = env
        .grace(
            &["--grace", "0.8", "--grace-cpu", "0.1"],
            "r.json",
            &burn(3),
        )
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let r = env.report("r.json");
    assert_eq!(r["promoted"], true, "{r}");
    let admitted = r["admitted_ms"].as_u64().unwrap();
    assert!(
        admitted >= t0 + 700,
        "admitted_ms is the spawn: {admitted} vs {t0}"
    );
    let runs = env.runs();
    let line = runs
        .iter()
        .find(|l| l["rule"] == "unmatched-heavy")
        .unwrap();
    assert_eq!(line["held"], 0, "{line}");
}
