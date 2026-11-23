//! 0.3.3 deadlock guards (G1, G2, G3, G4, G5) as real processes: each test
//! gets a private `XDG_STATE_HOME` and `XDG_CONFIG_HOME` and a stored budget
//! of 4 slots (D17a).

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use muzzle::state::{Budget, State, Store};
use serde_json::Value;

const BUDGET: u32 = 4;

struct Env {
    _dir: tempfile::TempDir,
    state: PathBuf,
    config: PathBuf,
    units: String,
}

impl Env {
    /// `extra` is appended to `config.toml` (top-level keys only).
    fn new(extra: &str) -> Env {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = tempfile::tempdir().unwrap();
        let env = Env {
            state: dir.path().join("state"),
            config: dir.path().join("config"),
            units: format!(
                "muzzle-g{}x{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            _dir: dir,
        };
        std::fs::create_dir_all(env.config.join("muzzle")).unwrap();
        std::fs::write(env.config.join("muzzle/config.toml"), extra).unwrap();
        env.store()
            .update(now_ms(), |s| {
                s.budget = Some(Budget {
                    budget: BUDGET,
                    cores: BUDGET + 2,
                    ram_per_slot: 64 << 20,
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

    /// `muzzle run ARGS…` as a group leader (D16), from `pane` if given.
    fn run(&self, pane: Option<&str>, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_muzzle"));
        c.arg("run")
            .args(args)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("MUZZLE_UNIT_PREFIX", &self.units)
            .env_remove("MUZZLE_HOLD")
            .env_remove("MUZZLE_CLASS")
            .env_remove("MU_MANAGED_AGENT")
            .env_remove("MU_AGENT_NAME")
            .env_remove("MUZZLE_QUEUE_TIMEOUT")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(p) = pane {
            c.env("TMUX_PANE", p);
        }
        c
    }

    /// A job that backgrounds `sleep` and exits, so a keeper takes the
    /// claim (D16): 4 slots, orphaned from the start. Returns the group.
    fn orphan(&self, pane: Option<&str>, agent: Option<&str>) -> i32 {
        let mut c = self.run(
            pane,
            &["--slots", "4", "--", "sh", "-c", "sleep 60 & exit 0"],
        );
        if let Some(a) = agent {
            c.env("MU_AGENT_NAME", a);
        }
        let mut child = c
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        assert!(child.wait().unwrap().success());
        let s = self.state();
        assert_eq!(s.holders.len(), 1);
        assert!(s.holders[0].kept, "a keeper holds the claim");
        s.holders[0].pgid.unwrap() as i32
    }

    /// A live holder: muzzle itself sleeping 60 s, 4 slots.
    fn live(&self, pane: Option<&str>) -> Child {
        let child = self
            .run(pane, &["--slots", "4", "--", "sleep", "60"])
            .spawn()
            .unwrap();
        assert!(eventually(Duration::from_secs(5), || {
            self.state().holders.len() == 1
        }));
        // Held only means admitted: wait until the job has exec'd.
        assert!(eventually(Duration::from_secs(5), || {
            muzzle::proc::group_members(child.id() as i32)
                .is_ok_and(|m| m.iter().any(|&p| p != child.id() as i32))
        }));
        child
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

fn kill(pid: i32, sig: i32) {
    // SAFETY: plain syscall; a negative pid targets a group.
    unsafe { libc::kill(pid, sig) };
}

fn gone(pid: i32) -> bool {
    // SAFETY: signal 0 only probes.
    unsafe { libc::kill(pid, 0) != 0 }
}

struct Out {
    code: Option<i32>,
    stderr: String,
    took: Duration,
}

fn finish(mut c: Command) -> Out {
    let t = Instant::now();
    let o = c.spawn().unwrap().wait_with_output().unwrap();
    Out {
        code: o.status.code(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        took: t.elapsed(),
    }
}

/// Free everything a test left behind: the leaked group, then the keeper
/// (which releases once the group is empty).
fn reap(env: &Env, pgid: i32) {
    kill(-pgid, libc::SIGKILL);
    assert!(eventually(Duration::from_secs(8), || env.state().held() == 0));
}

#[test]
fn keeper_idle_release_frees_slots_without_killing_the_job() {
    let env = Env::new("orphan_idle_s = 1\norphan_ttl = 0\n");
    let pgid = env.orphan(None, None);
    let keeper = env.state().holders[0].pid as i32;
    // Heartbeat samples at ~2 s and ~4 s; the second sees no activity.
    assert!(
        eventually(Duration::from_secs(10), || env.state().held() == 0),
        "idle orphan was never released"
    );
    assert!(!gone(-pgid), "sleep must survive: only the claim is freed");
    assert!(eventually(Duration::from_secs(5), || gone(keeper)));
    // Exactly one line, written by the pruning pass; the keeper adds none.
    std::thread::sleep(Duration::from_millis(500));
    let runs = env.runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["killed"], "orphan-idle");
    kill(-pgid, libc::SIGKILL);
}

#[test]
fn same_pane_blocked_by_own_orphan_exits_76_at_once() {
    let env = Env::new("orphan_idle_s = 0\norphan_ttl = 0\n");
    let pgid = env.orphan(Some("%9"), Some("rev-2"));
    let report = env._dir.path().join("report.json");
    let mut c = env.run(Some("%9"), &["--slots", "1", "--report"]);
    c.arg(&report).args(["--", "true"]);
    let out = finish(c);
    assert_eq!(out.code, Some(76), "{}", out.stderr);
    assert!(out.took < Duration::from_secs(3), "{:?}", out.took);
    assert!(
        out.stderr.contains("warning: same pane (%9"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("refusing to queue behind own orphaned claim")
    );
    assert!(out.stderr.contains("muzzle drop"));
    let rep: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(rep["exit"], 76);
    let runs = env.runs();
    let last = runs.last().unwrap();
    assert_eq!(last["killed"], "self-overlap", "{runs:?}");
    assert_eq!(last["waited_s"], 0.0);
    assert_eq!(last["ran_s"], 0.0);
    assert!(env.state().waiters.is_empty(), "the refused ticket is gone");
    reap(&env, pgid);
}

#[test]
fn allow_concurrent_queues_behind_own_orphan() {
    for (config, flag) in [
        ("orphan_idle_s = 0\norphan_ttl = 0\n", true),
        (
            "orphan_idle_s = 0\norphan_ttl = 0\nallow_concurrent = true\n",
            false,
        ),
    ] {
        let env = Env::new(config);
        let pgid = env.orphan(Some("%9"), None);
        let mut args = vec!["--slots", "1", "--timeout", "1"];
        if flag {
            args.push("--allow-concurrent");
        }
        args.extend(["--", "true"]);
        let out = finish(env.run(Some("%9"), &args));
        assert_eq!(out.code, Some(75), "{}", out.stderr);
        assert!(
            out.stderr.contains("warning: same pane (%9"),
            "{}",
            out.stderr
        );
        assert!(!out.stderr.contains("refusing"), "{}", out.stderr);
        // G5: the timeout names the holder and tags it.
        assert!(out.stderr.contains("[kept]"), "{}", out.stderr);
        reap(&env, pgid);
    }
}

#[test]
fn two_live_same_pane_claims_warn_and_queue() {
    let env = Env::new("");
    let mut hog = env.live(Some("%9"));
    let out = finish(env.run(
        Some("%9"),
        &["--slots", "1", "--timeout", "1", "--", "true"],
    ));
    assert_eq!(out.code, Some(75), "{}", out.stderr);
    assert!(
        out.stderr.contains("warning: same pane (%9"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("live"), "{}", out.stderr);
    assert!(!out.stderr.contains("refusing"), "{}", out.stderr);
    kill(-(hog.id() as i32), libc::SIGKILL);
    hog.wait().unwrap();
}

#[test]
fn different_panes_with_one_agent_name_do_not_overlap() {
    let env = Env::new("orphan_idle_s = 0\norphan_ttl = 0\n");
    let pgid = env.orphan(Some("%1"), Some("same-name"));
    let mut c = env.run(
        Some("%2"),
        &["--slots", "1", "--timeout", "1", "--", "true"],
    );
    c.env("MU_AGENT_NAME", "same-name");
    let out = finish(c);
    assert_eq!(out.code, Some(75), "{}", out.stderr);
    assert!(!out.stderr.contains("same pane"), "{}", out.stderr);
    assert!(!out.stderr.contains("refusing"), "{}", out.stderr);
    reap(&env, pgid);
}

#[test]
fn queue_timeout_precedence() {
    let env = Env::new("queue_timeout = 1\n");
    let mut hog = env.live(None);
    let quick = |out: Out| {
        assert_eq!(out.code, Some(75), "{}", out.stderr);
        assert!(out.took < Duration::from_secs(4), "{:?}", out.took);
        out
    };
    // config alone
    let out = quick(finish(env.run(None, &["--slots", "1", "--", "true"])));
    assert!(out.stderr.contains("held by"), "{}", out.stderr);
    // env over config (config would wait 1 s; env says 0 → try once)
    let mut c = env.run(None, &["--slots", "1", "--", "true"]);
    c.env("MUZZLE_QUEUE_TIMEOUT", "0");
    assert!(quick(finish(c)).took < Duration::from_millis(900));
    // flag over env
    let mut c = env.run(None, &["--slots", "1", "--timeout", "0", "--", "true"]);
    c.env("MUZZLE_QUEUE_TIMEOUT", "30");
    assert!(quick(finish(c)).took < Duration::from_millis(900));

    // Each of these waits past the configured 1 s.
    let waits = |mut c: Command| {
        let mut child = c.spawn().unwrap();
        std::thread::sleep(Duration::from_millis(2500));
        let waiting = child.try_wait().unwrap().is_none();
        kill(-(child.id() as i32), libc::SIGKILL);
        child.wait().unwrap();
        waiting
    };
    assert!(waits(
        env.run(None, &["--slots", "1", "--no-timeout", "--", "true"])
    ));
    let mut c = env.run(None, &["--slots", "1", "--", "true"]);
    c.env("MUZZLE_QUEUE_TIMEOUT", "none");
    assert!(waits(c));
    let mut c = env.run(None, &["--slots", "1", "--", "true"]);
    c.env("MUZZLE_QUEUE_TIMEOUT", "off");
    assert!(waits(c));
    assert!(
        env.state().waiters.is_empty() || {
            // SIGKILLed waiters are pruned by the next pass.
            eventually(Duration::from_secs(3), || {
                let _ = env.store().update(now_ms(), |s| {
                    s.waiters
                        .retain(|w| muzzle::proc::is_alive(w.pid as _, w.start));
                    Ok(())
                });
                env.state().waiters.is_empty()
            })
        }
    );
    kill(-(hog.id() as i32), libc::SIGKILL);
    hog.wait().unwrap();
}

#[test]
fn keeper_exits_at_once_and_writes_nothing_when_its_ticket_is_removed() {
    // Both G1 thresholds off: only the external removal can end the keeper.
    let env = Env::new("orphan_idle_s = 0\norphan_ttl = 0\n");
    let pgid = env.orphan(None, None);
    let keeper = env.state().holders[0].pid as i32;
    let ticket = env.state().holders[0].ticket;
    env.store()
        .update(now_ms(), |s| {
            s.remove(ticket);
            Ok(())
        })
        .unwrap();
    let path = env.state.join("muzzle/state.json");
    let before = std::fs::read(&path).unwrap();
    // Well under the 2 s heartbeat: the next 250 ms poll notices.
    assert!(
        eventually(Duration::from_millis(1500), || gone(keeper)),
        "keeper outlived its ticket"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "state.json rewritten"
    );
    assert!(env.runs().is_empty(), "{:?}", env.runs());
    assert!(!gone(-pgid), "the job group is not the keeper's to kill");
    kill(-pgid, libc::SIGKILL);
}

#[test]
fn overlap_warning_names_each_tickets_slots() {
    let env = Env::new("");
    let spawn = |slots: &str| {
        let n = env.state().holders.len();
        let child = env
            .run(Some("%998"), &["--slots", slots, "--", "sleep", "60"])
            .spawn()
            .unwrap();
        assert!(eventually(Duration::from_secs(5), || {
            env.state().holders.len() > n
        }));
        child
    };
    let a = spawn("1");
    let b = spawn("3");
    let out = finish(env.run(
        Some("%998"),
        &["--slots", "1", "--timeout", "0", "--", "true"],
    ));
    assert_eq!(out.code, Some(75), "{}", out.stderr);
    assert!(
        out.stderr.contains("ticket 1, 1 slots,") && out.stderr.contains("ticket 2, 3 slots,"),
        "{}",
        out.stderr
    );
    for mut c in [a, b] {
        kill(-(c.id() as i32), libc::SIGKILL);
        let _ = c.wait();
    }
}
