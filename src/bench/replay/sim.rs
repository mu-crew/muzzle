//! The scheduler and simulation: what each replayed call runs, under
//! which mode, and the synthetic burner (`bench burn`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::time::Instant;

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::Value;

use super::trace::Call;
use crate::cli::{BenchBurnArgs, BenchMode};
use crate::rules::config::Config;

/// C3 role caps: today's `CARGO_BUILD_JOBS` per role in the briefs.
pub fn role_cap(role: &str) -> u32 {
    match role {
        "worker" => 4,
        "orchestrator" => 6,
        _ => 2,
    }
}

/// The threads a synthetic call burns. `unmatched-heavy` never reaches
/// muzzle: one thread in both modes. Under muzzle a matched call burns
/// its claim (`None`: the burner reads `CARGO_BUILD_JOBS`, which `muzzle
/// run` sets from the claim, D14). In mode none it burns the rule's slots
/// capped at the C3 role cap: the trace's `dur_s` was measured under those
/// caps, so an uncontended call takes about `dur_s` in both modes
/// (bench_none_work_model).
pub fn synthetic_threads(call: &Call, mode: BenchMode, rule_slots: u32) -> Option<u32> {
    match (call.matched(), mode) {
        (false, _) => Some(1),
        (true, BenchMode::Muzzle) => None,
        (true, BenchMode::None) => Some(rule_slots.min(role_cap(&call.role))),
    }
}

/// The CPU-seconds a synthetic call burns: its threads busy for `dur_s`,
/// compressed by `speed`, with the claim taken as the rule's slots. The
/// work is fixed per call, so contention or a claim cut to the budget
/// stretches the wall time as it would for a real build.
pub fn work_s(call: &Call, mode: BenchMode, rule_slots: u32, speed: f64) -> f64 {
    let threads = synthetic_threads(call, mode, rule_slots).unwrap_or(rule_slots);
    f64::from(threads) * call.dur_s / speed
}

/// The most parallelism held at once over `(from, to, slots)` intervals.
/// An interval ending at `t` frees its slots before one starting at `t`.
pub fn max_held(intervals: &[(f64, f64, u32)]) -> u32 {
    let mut events: Vec<(f64, i64)> = intervals
        .iter()
        .flat_map(|&(a, b, n)| [(a, i64::from(n)), (b, -i64::from(n))])
        .collect();
    events.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
    let (mut cur, mut max) = (0i64, 0i64);
    for (_, d) in events {
        cur += d;
        max = max.max(cur);
    }
    max as u32
}

/// `bench/real.toml` (tier 2; never committed). Each replayed agent gets
/// `workdir/<agent>`, a worktree of `repo` at `commit`, where `setup` runs
/// once before the replay so both modes start equally warm.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Real {
    pub repo: String,
    pub commit: String,
    pub workdir: String,
    #[serde(default)]
    pub setup: Option<String>,
    pub rules: BTreeMap<String, String>,
}

fn expand(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

/// How one call ran.
#[derive(Debug, Clone)]
pub(super) struct Outcome {
    pub(super) matched: bool,
    pub(super) slots: u32,
    /// Wall seconds from replay start.
    pub(super) start: f64,
    pub(super) admitted: f64,
    pub(super) end: f64,
    /// Under muzzle, the slots as muzzle held them, in seconds from replay
    /// start: the report's epoch ms, read under the state lock, so a
    /// release never reads later than the admission it allows. `start + waited_s` can't stand in: it
    /// is late by the spawn and the first lock, and overlapping intervals
    /// read 20 slots held of 14.
    pub(super) held: Option<(f64, f64)>,
    pub(super) status: Option<ExitStatus>,
}

impl Outcome {
    pub(super) fn failed(&self) -> bool {
        !self.status.is_some_and(|s| s.success())
    }
    pub(super) fn sigkilled(&self) -> bool {
        use std::os::unix::process::ExitStatusExt;
        self.status.and_then(|s| s.signal()) == Some(libc::SIGKILL)
    }
}

/// Everything a call needs to run, shared across the call threads.
pub(super) struct Plan {
    pub(super) mode: BenchMode,
    pub(super) exe: PathBuf,
    pub(super) out: PathBuf,
    pub(super) speed: f64,
    pub(super) config: Config,
    /// Per-agent worktrees and rule commands, tier 2 only.
    pub(super) real: Option<(Real, PathBuf)>,
}

impl Plan {
    fn rule_slots(&self, call: &Call) -> u32 {
        // B3: a trace may name a table-only or disabled rule.
        self.config.rule_anywhere(&call.rule).map_or(1, |r| r.slots)
    }

    fn command(&self, i: usize, call: &Call) -> anyhow::Result<(Command, Option<PathBuf>)> {
        let rule_slots = self.rule_slots(call);
        let argv = match &self.real {
            Some((real, _)) if call.matched() => {
                vec!["bash".into(), "-c".into(), real.rules[&call.rule].clone()]
            }
            _ => {
                let mut c = vec![
                    self.exe.to_string_lossy().into_owned(),
                    "bench".into(),
                    "burn".into(),
                    "--cpu-s".into(),
                    format!("{}", work_s(call, self.mode, rule_slots, self.speed)),
                ];
                if let Some(t) = synthetic_threads(call, self.mode, rule_slots) {
                    c.extend(["--threads".into(), t.to_string()]);
                }
                c
            }
        };
        let mut report = None;
        let mut cmd = if call.matched() && self.mode == BenchMode::Muzzle {
            let file = self.out.join("reports").join(format!("{i}.json"));
            let mut c = Command::new(&self.exe);
            c.arg("run")
                .arg("--rule")
                .arg(&call.rule)
                .arg("--report")
                .arg(&file)
                .arg("--")
                .args(&argv);
            report = Some(file);
            c
        } else {
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        };
        if call.matched() && self.mode == BenchMode::None {
            let n = role_cap(&call.role).to_string();
            cmd.env("CARGO_BUILD_JOBS", &n)
                .env("RUST_TEST_THREADS", &n)
                .env("MAKEFLAGS", format!("-j{n}"));
        }
        let cwd = match &self.real {
            Some((_, dir)) => {
                let wt = dir.join(&call.agent);
                own_target_dir(&mut cmd, &wt);
                wt
            }
            None => self.out.clone(),
        };
        cmd.current_dir(cwd)
            .env("XDG_STATE_HOME", self.out.join("state"))
            .env("MU_AGENT_NAME", &call.agent)
            // D12: mu's orchestrator runs high, everyone it spawns normal.
            .env(
                "MUZZLE_CLASS",
                if call.role == "orchestrator" {
                    "high"
                } else {
                    "normal"
                },
            )
            // The bench may itself run under muzzle: its calls must queue
            // in the private state, not read as nested.
            .env_remove("MUZZLE_HOLD")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Ok((cmd, report))
    }

    pub(super) fn run_call(
        &self,
        i: usize,
        call: &Call,
        t0: Instant,
        t0_ms: u64,
    ) -> anyhow::Result<Outcome> {
        let (mut cmd, report) = self.command(i, call)?;
        let start = t0.elapsed().as_secs_f64();
        let status = cmd.status().context("spawning a replayed call")?;
        let end = t0.elapsed().as_secs_f64();
        let mut o = Outcome {
            matched: call.matched(),
            slots: match (self.mode, &self.real) {
                (BenchMode::Muzzle, _) => self.rule_slots(call),
                // Real commands get the role cap through the env above.
                (BenchMode::None, Some(_)) => role_cap(&call.role),
                (BenchMode::None, None) => {
                    synthetic_threads(call, self.mode, self.rule_slots(call))
                        .unwrap_or_else(|| role_cap(&call.role))
                }
            },
            start,
            admitted: start,
            end,
            held: None,
            status: Some(status),
        };
        if let Some(file) = report {
            match std::fs::read_to_string(&file)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            {
                Some(r) => {
                    o.admitted = start + r["waited_s"].as_f64().unwrap_or(0.0);
                    o.end = o.admitted + r["ran_s"].as_f64().unwrap_or(end - start);
                    o.slots = r["slots"].as_u64().map_or(o.slots, |n| n as u32);
                    // released_ms is null for a run a keeper released later
                    // (D16); the bench's own end is an upper bound for that.
                    let rel = |ms: u64| ms.saturating_sub(t0_ms) as f64 / 1000.0;
                    o.held = r["admitted_ms"].as_u64().map(|a| {
                        let r = r["released_ms"].as_u64().map_or(end, rel);
                        (rel(a), r)
                    });
                }
                // No report: muzzle itself failed before admission.
                None => o.status = None,
            }
        }
        Ok(o)
    }
}

/// Spec §Benchmark runner: each replayed agent has its own target
/// directory, in setup and replay alike. An inherited `CARGO_TARGET_DIR`
/// would point every agent at one shared dir, contending on cargo's lock
/// and sharing the cache the pre-build equalises (bench_real_target_dir).
fn own_target_dir(cmd: &mut Command, worktree: &Path) {
    let target = worktree.join("target");
    cmd.env("CARGO_TARGET_DIR", &target)
        .env("CARGO_BUILD_TARGET_DIR", &target);
}

/// Set up tier 2's per-agent worktrees and pre-build each once.
pub(super) fn prepare_real(real: &Real, agents: &BTreeSet<&str>) -> anyhow::Result<PathBuf> {
    let repo = expand(&real.repo);
    let dir = expand(&real.workdir);
    std::fs::create_dir_all(&dir)?;
    let sh = |cwd: &Path, script: &str| -> anyhow::Result<()> {
        let mut c = Command::new("bash");
        own_target_dir(&mut c, cwd);
        let st = c
            .arg("-c")
            .arg(script)
            .current_dir(cwd)
            .env_remove("MUZZLE_HOLD")
            .status()?;
        if !st.success() {
            bail!("bench real: {script:?} failed in {}: {st}", cwd.display());
        }
        Ok(())
    };
    for agent in agents {
        let wt = dir.join(agent);
        if !wt.exists() {
            let st = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["worktree", "add", "-q", "--detach"])
                .arg(&wt)
                .arg(&real.commit)
                .status()?;
            if !st.success() {
                bail!("bench real: git worktree add {} failed", wt.display());
            }
        }
        sh(&wt, &format!("git checkout -q --detach {}", real.commit))?;
        if let Some(setup) = &real.setup {
            eprintln!("muzzle: bench: pre-building {agent}");
            sh(&wt, setup)?;
        }
    }
    Ok(dir)
}

/// The hidden `bench burn`: spin `threads` threads until together they
/// have used `cpu_s` CPU-seconds. Threads come from `--threads`, else
/// `CARGO_BUILD_JOBS` (set by `muzzle run` or the role cap), else 1.
pub fn burn_main(args: &BenchBurnArgs) -> anyhow::Result<ExitCode> {
    let threads = args
        .threads
        .or_else(|| std::env::var("CARGO_BUILD_JOBS").ok()?.parse().ok())
        .unwrap_or(1)
        .max(1);
    if !(args.cpu_s.is_finite() && args.cpu_s >= 0.0) {
        bail!("--cpu-s {}: must be a non-negative number", args.cpu_s);
    }
    let each = args.cpu_s / f64::from(threads);
    let workers: Vec<_> = (0..threads)
        .map(|_| std::thread::spawn(move || spin(each)))
        .collect();
    for w in workers {
        w.join().expect("burner thread panicked");
    }
    Ok(ExitCode::SUCCESS)
}

/// Busy-loop until this thread's own CPU clock reaches `secs`: CPU time,
/// not wall time, so a starved burner takes longer, as a build would.
fn spin(secs: f64) {
    let cpu = || {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: writes one timespec.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
    };
    let start = cpu();
    let mut x = 0u64;
    while cpu() - start < secs {
        for i in 0..10_000u64 {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(i));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::trace::UNMATCHED;
    use super::super::trace::tests::call;
    use super::*;

    #[test]
    fn role_caps_follow_c3() {
        assert_eq!(role_cap("worker"), 4);
        assert_eq!(role_cap("orchestrator"), 6);
        assert_eq!(role_cap("reviewer"), 2);
        assert_eq!(role_cap("other"), 2);
    }

    #[test]
    fn synthetic_work_is_threads_times_duration_over_speed() {
        let gate = call(0.0, 10.0, "gate"); // a worker: cap 4
        let un = call(0.0, 10.0, UNMATCHED);
        // Muzzle: the claim (the burner reads it from CARGO_BUILD_JOBS).
        assert_eq!(synthetic_threads(&gate, BenchMode::Muzzle, 6), None);
        assert_eq!(work_s(&gate, BenchMode::Muzzle, 6, 2.0), 30.0);
        // None: the rule's slots capped at the role cap, so dur_s holds.
        assert_eq!(synthetic_threads(&gate, BenchMode::None, 6), Some(4));
        assert_eq!(work_s(&gate, BenchMode::None, 6, 2.0), 20.0);
        assert_eq!(synthetic_threads(&gate, BenchMode::None, 3), Some(3));
        assert_eq!(work_s(&gate, BenchMode::None, 3, 2.0), 15.0);
        // unmatched-heavy burns one explicit thread whatever the slots.
        for mode in [BenchMode::None, BenchMode::Muzzle] {
            assert_eq!(synthetic_threads(&un, mode, 6), Some(1));
            assert_eq!(work_s(&un, mode, 6, 2.0), 5.0);
        }
    }

    #[test]
    fn max_held_sweeps_overlaps() {
        assert_eq!(max_held(&[]), 0);
        assert_eq!(max_held(&[(0.0, 2.0, 2), (1.0, 3.0, 3), (2.5, 4.0, 1)]), 5);
        // Back to back: the first frees before the second starts.
        assert_eq!(max_held(&[(0.0, 1.0, 4), (1.0, 2.0, 4)]), 4);
    }
}
