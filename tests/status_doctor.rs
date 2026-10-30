//! `muzzle status` and `muzzle doctor` as processes (spec §Interface,
//! `status --json`, `doctor`; D17a, D33). Each test gets a private `HOME`,
//! `XDG_STATE_HOME` and `XDG_CONFIG_HOME`.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use muzzle::state::{Budget, Store};
use serde_json::Value;

const CONFIG: &str = r#"
reserve = 2
[[rule]]
name = "gate"
match = 'gate'
slots = 2
"#;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
        };
        for d in ["home", "state", "config/muzzle", "cwd"] {
            std::fs::create_dir_all(env.path(d)).unwrap();
        }
        std::fs::write(env.path("config/muzzle/config.toml"), CONFIG).unwrap();
        env
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn store(&self) -> Store {
        Store::open(self.path("state/muzzle")).unwrap()
    }

    fn seed_budget(&self, budget: Budget) {
        self.store()
            .update(now_ms(), |s| {
                s.budget = Some(budget);
                Ok(())
            })
            .unwrap();
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_muzzle"));
        c.args(args)
            .current_dir(self.path("cwd"))
            .env("HOME", self.path("home"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env(
                "MUZZLE_UNIT_PREFIX",
                format!("muzzle-sd{}", std::process::id()),
            )
            .env_remove("MUZZLE_HOLD")
            .env_remove("MUZZLE_CLASS")
            .env_remove("MU_MANAGED_AGENT")
            .env_remove("MU_AGENT_NAME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("PI_CODING_AGENT_DIR")
            .stdin(Stdio::null());
        c
    }

    fn output(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.output(args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// `muzzle run` as agent `agent`, leading its own group as pi runs it.
    fn run(&self, agent: &str, class: &str, slots: &str, rule: &str) -> Child {
        self.cmd(&[
            "run", "--slots", slots, "--class", class, "--rule", rule, "--", "sleep", "30",
        ])
        .env("MU_AGENT_NAME", agent)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap()
    }

    fn copy(&self, from: &str, to: &str) {
        let to = self.path(to);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join(from), to).unwrap();
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

fn kill_group(child: &mut Child) {
    // SAFETY: plain syscall; a negative pid targets the group.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    child.wait().unwrap();
}

#[test]
fn status_json_shows_live_holders_and_waiters() {
    let env = Env::new();
    env.seed_budget(Budget {
        budget: 4,
        cores: 6,
        ram_per_slot: 1 << 30,
    });
    let mut orch = env.run("orch", "high", "3", "gate");
    assert!(eventually(Duration::from_secs(5), || {
        env.store().read().unwrap().holders.len() == 1
    }));
    // 1 slot free. A normal 2-slot waiter, then a high 2-slot one: neither
    // fits, and the later high one is listed first (D10).
    let mut worker = env.run("worker-7", "normal", "2", "gate");
    let mut human = env.run("human", "high", "2", "cargo-test");
    assert!(eventually(Duration::from_secs(5), || {
        env.store().read().unwrap().waiters.len() == 2
    }));

    let v = env.json(&["status", "--json"]);
    for k in [
        "version", "budget", "cores", "reserve", "held", "free", "enforced", "holders", "waiting",
    ] {
        assert!(v.get(k).is_some(), "missing {k}: {v}");
    }
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        (v["budget"].as_u64(), v["cores"].as_u64()),
        (Some(4), Some(6))
    );
    assert_eq!(v["reserve"], 2);
    assert_eq!((v["held"].as_u64(), v["free"].as_u64()), (Some(3), Some(1)));
    assert!(v["enforced"].is_boolean());

    let h = &v["holders"][0];
    assert_eq!(v["holders"].as_array().unwrap().len(), 1);
    assert_eq!(
        (h["slots"].as_u64(), &h["class"]),
        (Some(3), &Value::from("high"))
    );
    assert_eq!(
        (&h["agent"], &h["rule"]),
        (&Value::from("orch"), &Value::from("gate"))
    );
    assert_eq!(h["pid"].as_u64(), Some(u64::from(orch.id())));
    assert_eq!(h["pgid"].as_u64(), Some(u64::from(orch.id())));
    assert!(h["run_s"].is_u64());
    assert!(h["cwd"].as_str().unwrap().ends_with("cwd"), "{h}");
    // D27a: the tempdir cwd is in no repo.
    assert_eq!(h.get("repo"), Some(&Value::Null), "{h}");

    let w: Vec<(u64, &str, &str, u64)> = v["waiting"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            assert!(w["age_s"].is_u64() && w.get("rule").is_some(), "{w}");
            (
                w["position"].as_u64().unwrap(),
                w["agent"].as_str().unwrap(),
                w["class"].as_str().unwrap(),
                w["slots"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(w, [(1, "human", "high", 2), (2, "worker-7", "normal", 2)]);

    // Text form names the same entries.
    let text = String::from_utf8(env.output(&["status"]).stdout).unwrap();
    assert!(text.contains("3 held, 1 free"), "{text}");
    for want in ["orch", "worker-7", "human"] {
        assert!(text.contains(want), "{want} missing:\n{text}");
    }

    // Kill the holder's group: status stops counting it at once, before
    // any waiter has pruned it, and the queue drains.
    kill_group(&mut orch);
    let v = env.json(&["status", "--json"]);
    let agents: Vec<&str> = v["holders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["agent"].as_str().unwrap())
        .collect();
    assert!(!agents.contains(&"orch"), "{v}");
    kill_group(&mut worker);
    kill_group(&mut human);
}

#[test]
fn status_with_no_state_reports_this_hosts_budget() {
    let env = Env::new();
    let v = env.json(&["status", "--json"]);
    assert_eq!(v["held"], 0);
    assert_eq!(v["holders"], Value::Array(vec![]));
    assert_eq!(v["waiting"], Value::Array(vec![]));
    let cores = v["cores"].as_u64().unwrap();
    assert_eq!(
        v["budget"].as_u64().unwrap(),
        cores.saturating_sub(2).max(1)
    );
    // Read-only: no template written, no state.json created.
    assert!(!env.path("state/muzzle/state.json").exists());
}

#[test]
fn doctor_reports_every_line_with_no_harness() {
    let env = Env::new();
    let out = env.output(&["doctor"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let t = String::from_utf8(out.stdout).unwrap();
    for want in [
        "budget ",
        "enforcement ",
        "(writable)",
        "1 rules, 0 path tables, hash ",
        "runs.jsonl     none yet",
        "pi extension   not installed",
        "claude hook    not found",
        "codex hook     not found",
        "platform ",
    ] {
        assert!(t.contains(want), "missing {want:?} in\n{t}");
    }
    let v = env.json(&["doctor", "--json"]);
    assert_eq!(v["rules"], 1);
    assert_eq!(v["reserve"], 2);
    assert_eq!(v["state_writable"], true);
    assert_eq!(
        v["platform"],
        format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
    );
    let e = v["enforcement"].as_str().unwrap();
    assert!(
        e.starts_with("enforced (") || e.starts_with("advisory ("),
        "{e}"
    );
    if cfg!(target_os = "macos") {
        assert!(e.starts_with("advisory ("), "{e}");
    }
}

#[test]
fn doctor_finds_harness_hooks_in_fixture_configs() {
    let env = Env::new();
    env.copy(
        "tests/fixtures/doctor/pi-settings.json",
        "home/.pi/agent/settings.json",
    );
    env.copy(
        "docs/harnesses/claude-code.hooks.json",
        "home/.claude/settings.json",
    );
    env.copy(
        "tests/fixtures/doctor/codex-config.toml",
        "home/.codex/config.toml",
    );
    let v = env.json(&["doctor", "--json"]);
    assert_eq!(v["pi_extension"], "loaded", "{v}");
    assert_eq!(v["claude_hook"], "installed", "{v}");
    assert_eq!(v["codex_hook"], "installed", "{v}");
}

#[test]
fn doctor_finds_project_hooks_and_ignores_other_hooks() {
    let env = Env::new();
    env.copy(
        "tests/fixtures/doctor/claude-settings-other-hook.json",
        "home/.claude/settings.json",
    );
    env.copy("docs/harnesses/codex.hooks.json", "cwd/.codex/hooks.json");
    let v = env.json(&["doctor", "--json"]);
    assert_eq!(v["claude_hook"], "not found", "{v}");
    assert_eq!(v["codex_hook"], "installed", "{v}");
    assert_eq!(v["pi_extension"], "not installed", "{v}");
    // CODEX_HOME moves the global config, as it does for codex.
    env.copy(
        "tests/fixtures/doctor/codex-config.toml",
        "elsewhere/config.toml",
    );
    let out = env
        .cmd(&["doctor", "--json"])
        .current_dir(env.path("home"))
        .env("CODEX_HOME", env.path("elsewhere"))
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["codex_hook"], "installed", "{v}");
}

#[test]
fn doctor_warns_when_stored_budget_differs_from_its_reading() {
    let env = Env::new();
    // No host has 1000 cores: the stored values can't match the reading.
    env.seed_budget(Budget {
        budget: 998,
        cores: 1000,
        ram_per_slot: 1 << 20,
    });
    let v = env.json(&["doctor", "--json"]);
    assert_eq!(
        (v["budget"].as_u64(), v["budget_source"].as_str()),
        (Some(998), Some("stored"))
    );
    let warnings: Vec<&str> = v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect();
    assert!(
        warnings.iter().any(|w| w.contains("stored budget differs")),
        "{warnings:?}"
    );
    let t = String::from_utf8(env.output(&["doctor"]).stdout).unwrap();
    assert!(t.contains("warning: stored budget differs"), "{t}");
}

#[test]
fn doctor_reads_the_runs_log_and_exits_2_on_bad_config() {
    let env = Env::new();
    std::fs::create_dir_all(env.path("state/muzzle")).unwrap();
    std::fs::write(
        env.path("state/muzzle/runs.jsonl"),
        "{\"end\":\"2026-10-28T10:00:00Z\"}\n{\"end\":\"2026-10-20T10:00:00Z\"}\n",
    )
    .unwrap();
    // P11: doctor sizes the archives without opening them.
    std::fs::write(env.path("state/muzzle/runs-2026-08.jsonl.gz"), "12345").unwrap();
    std::fs::write(env.path("state/muzzle/runs-2026-09.jsonl.gz"), "123").unwrap();
    let v = env.json(&["doctor", "--json"]);
    assert_eq!(v["runs"]["lines"], 2);
    assert_eq!(v["runs"]["oldest"], "2026-10-20T10:00:00Z");
    assert_eq!(v["runs"]["newest"], "2026-10-28T10:00:00Z");
    assert_eq!(v["runs_archives"]["archives"], 2);
    assert_eq!(v["runs_archives"]["archive_bytes"], 8);
    assert_eq!(v["runs_archives"]["oldest_month"], "2026-08");
    let text = String::from_utf8_lossy(&env.output(&["doctor"]).stdout).into_owned();
    assert!(
        text.contains("archives       2 files, 8 bytes, oldest 2026-08"),
        "{text}"
    );

    std::fs::write(env.path("config/muzzle/config.toml"), "bogus = 1\n").unwrap();
    let out = env.output(&["doctor"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("bogus"));
    // status still answers on a bad config.
    assert!(env.output(&["status", "--json"]).status.success());
}

#[test]
fn expired_stored_budget_is_not_reported() {
    // Reviewer repro (sd_expired_budget): idle and written long ago, so the
    // next run would drop it and use this host's reading.
    let env = Env::new();
    std::fs::create_dir_all(env.path("state/muzzle")).unwrap();
    std::fs::write(
        env.path("state/muzzle/state.json"),
        r#"{"v":1,"next_ticket":1,"budget":{"budget":998,"cores":1000,"ram_per_slot":1048576},"waiters":[],"holders":[],"updated_ms":1000}"#,
    )
    .unwrap();
    let s = env.json(&["status", "--json"]);
    let cores = s["cores"].as_u64().unwrap();
    assert_ne!(cores, 1000, "{s}");
    assert_eq!(
        s["budget"].as_u64().unwrap(),
        cores.saturating_sub(2).max(1)
    );
    let d = env.json(&["doctor", "--json"]);
    assert_eq!(d["budget_source"], "measured", "{d}");
    assert_eq!(d["stored"], Value::Null, "{d}");
    assert_eq!(d["warnings"], Value::Array(vec![]), "{d}");
}

#[test]
fn doctor_exits_125_on_an_unreadable_state_file() {
    // Reviewer repro (sd_unreadable_state): every run exits 125 here.
    let env = Env::new();
    std::fs::create_dir_all(env.path("state/muzzle")).unwrap();
    std::fs::write(env.path("state/muzzle/state.json"), r#"{"v":99}"#).unwrap();
    let out = env.output(&["run", "--slots", "1", "--", "true"]);
    assert_eq!(out.status.code(), Some(125), "{out:?}");
    let out = env.output(&["doctor"]);
    assert_eq!(out.status.code(), Some(125), "{out:?}");
    let t = String::from_utf8(out.stdout).unwrap();
    assert!(t.contains("warning: state.json unreadable"), "{t}");
    assert!(t.contains("schema v99"), "{t}");
}

#[test]
fn doctor_ignores_files_pi_does_not_load() {
    // Reviewer repro (sd_pi_ext_prefix).
    let env = Env::new();
    let ext = env.path("home/.pi/agent/extensions");
    std::fs::create_dir_all(&ext).unwrap();
    std::fs::write(ext.join("muzzle.ts.disabled"), "").unwrap();
    std::fs::write(ext.join("muzzle-notes.md"), "").unwrap();
    let v = env.json(&["doctor", "--json"]);
    assert_eq!(v["pi_extension"], "not installed", "{v}");
    std::fs::write(ext.join("muzzle.ts"), "").unwrap();
    let v = env.json(&["doctor", "--json"]);
    assert_eq!(v["pi_extension"], "loaded", "{v}");
}
