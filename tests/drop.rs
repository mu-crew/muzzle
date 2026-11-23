//! `muzzle drop` as a process (G4). Each test gets a private `HOME`,
//! `XDG_STATE_HOME` and `XDG_CONFIG_HOME`; holders are seeded into the
//! state file directly.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use muzzle::admit::Class;
use muzzle::proc;
use muzzle::state::{Claim, Holder, Store};

struct Env {
    dir: tempfile::TempDir,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn holder(ticket: u64, agent: &str, pid: u32, start: u64, pgid: Option<u32>, kept: bool) -> Holder {
    Holder {
        ticket,
        pid,
        start,
        pgid,
        run_start_ms: now_ms() - 5000,
        waited_s: 0.0,
        enforced: false,
        kept,
        enforced_reason: None,
        scope: None,
        probe: None,
        admit_load1: None,
        admit_held: None,
        seen_ms: None,
        orphan_since_ms: None,
        orphan_active_ms: None,
        orphan_sample_ms: None,
        orphan_procs: None,
        claim: Claim {
            slots: 2,
            class: Class::Normal,
            agent: Some(agent.into()),
            rule: Some("gate".into()),
            cwd: "/tmp".into(),
            repo: None,
            cfg: None,
            pane: None,
        },
    }
}

/// A holder whose muzzle is long gone.
fn dead(ticket: u64, agent: &str, pgid: Option<u32>) -> Holder {
    holder(ticket, agent, 4_000_000, 1, pgid, false)
}

/// A holder whose muzzle is the live leader of group `pgid` (a child we
/// spawned): the birth-anchored evidence `--kill` needs. Live, so `--force`.
fn led(ticket: u64, pgid: i32) -> Holder {
    let start = proc::start_time(pgid).unwrap();
    holder(ticket, "a", pgid as u32, start, Some(pgid as u32), false)
}

impl Env {
    fn new() -> Env {
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
        };
        for d in ["home", "state", "config/muzzle", "cwd"] {
            std::fs::create_dir_all(env.path(d)).unwrap();
        }
        env
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn store(&self) -> Store {
        Store::open(self.path("state/muzzle")).unwrap()
    }

    fn seed(&self, holders: Vec<Holder>) {
        self.store()
            .update(now_ms(), |s| {
                s.holders = holders;
                Ok(())
            })
            .unwrap();
    }

    fn tickets(&self) -> Vec<u64> {
        let s = self.store().read().unwrap();
        s.holders.iter().map(|h| h.ticket).collect()
    }

    fn drop(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_muzzle"))
            .arg("drop")
            .args(args)
            .current_dir(self.path("cwd"))
            .env("HOME", self.path("home"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env_remove("MUZZLE_HOLD")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn runs(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.path("state/muzzle/runs.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

/// A job leading its own group: a shell and two sleeps.
fn group() -> Child {
    Command::new("sh")
        .args(["-c", "sleep 60 & sleep 60 & wait"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_members(pgid: i32, n: usize) {
    let t = Instant::now();
    while proc::group_members(pgid).unwrap().len() < n {
        assert!(t.elapsed() < Duration::from_secs(5), "group never filled");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

#[test]
fn no_selector_is_usage() {
    let env = Env::new();
    let o = env.drop(&[]);
    assert_eq!(code(&o), 2, "{o:?}");
}

#[test]
fn dead_drops_orphans_and_keeps_live() {
    let env = Env::new();
    let me = std::process::id();
    let start = proc::start_time(me as i32).unwrap();
    env.seed(vec![
        holder(1, "a", me, start, None, false), // live
        dead(2, "a", None),
        holder(3, "b", me, start, None, true), // kept
    ]);
    let o = env.drop(&["--dead"]);
    assert_eq!(code(&o), 0, "{o:?}");
    assert_eq!(env.tickets(), [1]);
    let runs = env.runs();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r["killed"] == "dropped"));
}

#[test]
fn dead_with_nothing_exits_zero() {
    let env = Env::new();
    let o = env.drop(&["--dead"]);
    assert_eq!(code(&o), 0, "{o:?}");
    assert!(String::from_utf8_lossy(&o.stdout).contains("nothing to drop"));
    assert!(env.runs().is_empty());
}

#[test]
fn agent_drops_only_that_agents_orphans() {
    let env = Env::new();
    env.seed(vec![dead(1, "a", None), dead(2, "b", None)]);
    let o = env.drop(&["--agent", "a"]);
    assert_eq!(code(&o), 0, "{o:?}");
    assert_eq!(env.tickets(), [2]);
}

#[test]
fn live_needs_force() {
    let env = Env::new();
    let me = std::process::id();
    let start = proc::start_time(me as i32).unwrap();
    env.seed(vec![holder(1, "a", me, start, None, false)]);
    let o = env.drop(&["1"]);
    assert_eq!(code(&o), 2, "{o:?}");
    assert!(String::from_utf8_lossy(&o.stderr).contains("--force"));
    assert_eq!(env.tickets(), [1]);
    assert!(env.runs().is_empty());
    let o = env.drop(&["--force", "1"]);
    assert_eq!(code(&o), 0, "{o:?}");
    assert!(env.tickets().is_empty());
    assert_eq!(env.runs()[0]["killed"], "dropped");
}

#[test]
fn kill_refuses_before_spawn() {
    let env = Env::new();
    env.seed(vec![dead(1, "a", None)]);
    let o = env.drop(&["--kill", "1"]);
    assert_eq!(code(&o), 2, "{o:?}");
    assert_eq!(env.tickets(), [1]);
}

#[test]
fn kill_terminates_the_group_and_logs() {
    let env = Env::new();
    let mut child = group();
    let pgid = child.id() as i32;
    wait_members(pgid, 3);
    env.seed(vec![led(7, pgid)]);
    let t = Instant::now();
    let o = env.drop(&["--kill", "--force", "7"]);
    assert_eq!(code(&o), 0, "{o:?}");
    assert!(t.elapsed() < Duration::from_secs(5), "SIGTERM was enough");
    let _ = child.wait();
    assert!(proc::group_members(pgid).unwrap().is_empty());
    assert!(env.tickets().is_empty());
    assert_eq!(env.runs()[0]["killed"], "dropped");
}

#[test]
fn kill_never_signals_own_group() {
    // A holder whose pgid is our own group: nothing may be signalled.
    let env = Env::new();
    // SAFETY: getpgrp has no preconditions.
    let own = unsafe { libc::getpgrp() } as u32;
    env.seed(vec![dead(1, "a", Some(own))]);
    let o = env.drop(&["--kill", "1"]);
    assert_eq!(code(&o), 0, "{o:?}");
    assert!(env.tickets().is_empty());
}

#[test]
fn kill_refuses_a_reused_pgid() {
    // The claim's leader pid/pgid now belong to an unrelated process: same
    // numbers, different start stamp. Nothing may be signalled.
    let env = Env::new();
    let mut child = group();
    let pgid = child.id();
    wait_members(pgid as i32, 3);
    let start = proc::start_time(pgid as i32).unwrap();
    env.seed(vec![holder(7, "a", pgid, start + 1, Some(pgid), false)]);
    let o = env.drop(&["--kill", "7"]);
    assert_ne!(code(&o), 0, "{o:?}");
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("reused"),
        "{o:?}"
    );
    assert_eq!(proc::group_members(pgid as i32).unwrap().len(), 3);
    assert_eq!(env.tickets(), [7]);
    assert!(env.runs().is_empty());
    // With no evidence at all (dead muzzle) it refuses too.
    env.seed(vec![dead(8, "a", Some(pgid))]);
    let o = env.drop(&["--kill", "8"]);
    assert_ne!(code(&o), 0, "{o:?}");
    assert_eq!(proc::group_members(pgid as i32).unwrap().len(), 3);
    // The recorded leader is attribution enough.
    env.seed(vec![holder(9, "a", pgid, start, Some(pgid), false)]);
    let o = env.drop(&["--kill", "--force", "9"]);
    assert_eq!(code(&o), 0, "{o:?}");
    let _ = child.wait();
    assert!(proc::group_members(pgid as i32).unwrap().is_empty());
}

/// A job that, on SIGTERM, touches `marker` and takes `secs` to exit. Returns
/// once its trap is installed (`<marker>.ready`), so no TERM can precede it.
fn slow_group(marker: &std::path::Path, secs: u32) -> Child {
    let ready = marker.with_extension("ready");
    let script = format!(
        "trap 'touch {}; sleep {secs}; exit 0' TERM; touch {}; while :; do sleep 1; done",
        marker.display(),
        ready.display()
    );
    let child = slow_spawn(&script);
    wait_for(&ready);
    child
}

fn slow_spawn(script: &str) -> Child {
    Command::new("sh")
        .args(["-c", script])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for(p: &std::path::Path) {
    let t = Instant::now();
    while !p.exists() {
        assert!(t.elapsed() < Duration::from_secs(10), "never saw {p:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_drop(env: &Env, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .arg("drop")
        .args(args)
        .current_dir(env.path("cwd"))
        .env("HOME", env.path("home"))
        .env("XDG_STATE_HOME", env.path("state"))
        .env("XDG_CONFIG_HOME", env.path("config"))
        .env_remove("MUZZLE_HOLD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn kill_holds_the_claim_through_grace() {
    let env = Env::new();
    let marker = env.path("termed");
    let mut child = slow_group(&marker, 1);
    let pgid = child.id() as i32;
    wait_members(pgid, 2);
    env.seed(vec![led(7, pgid)]);
    let mut d = spawn_drop(&env, &["--kill", "--force", "7"]);
    wait_for(&marker);
    // SIGTERM delivered, job still shutting down: slots still held.
    assert_eq!(env.tickets(), [7]);
    assert!(env.runs().is_empty());
    assert!(d.try_wait().unwrap().is_none());
    assert!(d.wait().unwrap().success());
    let _ = child.wait();
    assert!(env.tickets().is_empty());
    assert_eq!(env.runs().len(), 1);
    assert_eq!(env.runs()[0]["killed"], "dropped");
}

#[test]
fn kill_does_not_double_log_a_self_release() {
    let env = Env::new();
    let marker = env.path("termed");
    let mut child = slow_group(&marker, 1);
    let pgid = child.id() as i32;
    wait_members(pgid, 2);
    env.seed(vec![led(7, pgid)]);
    let mut d = spawn_drop(&env, &["--kill", "--force", "7"]);
    wait_for(&marker);
    // The runner releases itself during the grace.
    env.store()
        .update(now_ms(), |s| {
            s.holders.clear();
            Ok(())
        })
        .unwrap();
    assert!(d.wait().unwrap().success());
    let _ = child.wait();
    assert!(env.runs().is_empty(), "{:?}", env.runs());
}

#[test]
fn kill_refuses_scope_only_when_it_cannot_stop_it() {
    let env = Env::new();
    let mut h = dead(1, "a", None);
    h.scope = Some("muzzle-nonexistent-test.scope".into());
    env.seed(vec![h]);
    let o = env.drop(&["--kill", "1"]);
    // No systemd user manager here, or no such unit: either way not stopped.
    assert_ne!(code(&o), 0, "{o:?}");
    assert_eq!(env.tickets(), [1]);
    assert!(env.runs().is_empty());
    // Without --kill the claim alone may go.
    let o = env.drop(&["1"]);
    assert_eq!(code(&o), 0, "{o:?}");
    assert!(env.tickets().is_empty());
}

#[test]
fn usage_errors_do_not_touch_state() {
    let env = Env::new();
    std::fs::create_dir_all(env.path("state/muzzle")).unwrap();
    let state = env.path("state/muzzle/state.json");
    std::fs::write(&state, r#"{"v":999}"#).unwrap();
    for args in [&[][..], &["--kill"], &["--force"]] {
        let o = env.drop(args);
        assert_eq!(code(&o), 2, "{args:?}: {o:?}");
    }
    assert_eq!(std::fs::read_to_string(&state).unwrap(), r#"{"v":999}"#);
}

#[test]
fn kill_ignores_orphan_samples_after_pgid_reuse() {
    // A stale non-leader holder whose pgid now names an unrelated group, and
    // whose G1 sample was taken from that group: the sample is not proof.
    let env = Env::new();
    let mut child = group();
    let pgid = child.id() as i32;
    wait_members(pgid, 3);
    let mut h = dead(7, "a", Some(pgid as u32));
    h.orphan_procs = Some(
        proc::group_members(pgid)
            .unwrap()
            .into_iter()
            .map(|p| muzzle::state::ProcCpu {
                pid: p as u32,
                start: proc::start_time(p).unwrap(),
                cpu_ns: 0,
            })
            .collect(),
    );
    env.seed(vec![h]);
    let o = env.drop(&["--kill", "7"]);
    assert_ne!(code(&o), 0, "{o:?}");
    assert_eq!(proc::group_members(pgid).unwrap().len(), 3);
    assert_eq!(env.tickets(), [7]);
    assert!(env.runs().is_empty());
    // SAFETY: killing our own spawned child's group.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let _ = child.wait();
}
