//! `bench run TRACE`: tiers, modes, `--window`, result.json, summary.md
//! (spec §Benchmark runner, D31, D35, C3).
//!
//! The replay is plain processes: `--mode muzzle` runs each matched call
//! through `muzzle run` (this binary) with a private state dir, so admission
//! is only ever touched through `run` (§Architecture 6).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::probe::{self, Burst, Probe};
use crate::cli::{BenchBurnArgs, BenchMode, BenchRunArgs, BenchTier};
use crate::report::percentile;
use crate::rules::config::Config;

pub const LIGHT: &str = "light";
pub const UNMATCHED: &str = "unmatched-heavy";

#[derive(Debug, Clone, Deserialize)]
pub struct Header {
    pub host: String,
    pub cores: u32,
    pub ram_gb: u32,
    pub date: String,
    pub window_s: f64,
    pub calls_kept: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Call {
    pub t: f64,
    pub dur_s: f64,
    pub rule: String,
    pub role: String,
    pub agent: String,
}

impl Call {
    /// Has a rule label: claims slots under muzzle.
    pub fn matched(&self) -> bool {
        self.rule != LIGHT && self.rule != UNMATCHED
    }
}

pub fn parse_trace(text: &str) -> anyhow::Result<(Header, Vec<Call>)> {
    let mut header = None;
    let mut calls = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line).with_context(|| format!("line {}", i + 1))?;
        if row["v"] != 1 {
            bail!("line {}: unknown trace version", i + 1);
        }
        match (row["kind"].as_str(), &header) {
            (Some("header"), None) => header = Some(serde_json::from_value(row)?),
            (Some("call"), Some(_)) => calls.push(
                serde_json::from_value::<Call>(row).with_context(|| format!("line {}", i + 1))?,
            ),
            _ => bail!("line {}: expected one header, then calls", i + 1),
        }
    }
    let header = header.context("the trace has no header")?;
    calls.sort_by(|a, b| a.t.total_cmp(&b.t));
    Ok((header, calls))
}

/// `busiest:3h` → 10800 s. Units s, m, h; a bare number is seconds.
pub fn parse_window(s: &str) -> anyhow::Result<f64> {
    let len = s
        .strip_prefix("busiest:")
        .with_context(|| format!("--window {s:?}: expected busiest:<duration>, e.g. busiest:3h"))?;
    let (num, mult) = match len.char_indices().last() {
        Some((i, 'h')) => (&len[..i], 3600.0),
        Some((i, 'm')) => (&len[..i], 60.0),
        Some((i, 's')) => (&len[..i], 1.0),
        _ => (len, 1.0),
    };
    match num.parse::<f64>() {
        Ok(n) if n.is_finite() && n > 0.0 => Ok(n * mult),
        _ => bail!("--window {s:?}: bad duration"),
    }
}

/// Matched call-seconds inside `[start, start + len)`, calls clipped.
pub fn window_load(calls: &[Call], start: f64, len: f64) -> f64 {
    calls
        .iter()
        .filter(|c| c.matched())
        .map(|c| ((c.t + c.dur_s).min(start + len) - c.t.max(start)).max(0.0))
        .sum()
}

/// The earliest start of the `len`-second window with the most matched
/// call-seconds. The load is piecewise linear in the start, so its maximum
/// is at a breakpoint: a call's start, or where a call's end meets the
/// window's end.
pub fn busiest_start(calls: &[Call], len: f64) -> f64 {
    let mut best = (0.0, window_load(calls, 0.0, len));
    for c in calls.iter().filter(|c| c.matched()) {
        for s in [c.t, (c.t + c.dur_s - len).max(0.0)] {
            let v = window_load(calls, s, len);
            if v > best.1 || (v == best.1 && s < best.0) {
                best = (s, v);
            }
        }
    }
    best.0
}

/// C3 role caps: today's `CARGO_BUILD_JOBS` per role in the briefs.
pub fn role_cap(role: &str) -> u32 {
    match role {
        "worker" => 4,
        "orchestrator" => 6,
        _ => 2,
    }
}

/// The CPU-seconds a synthetic call burns: the rule's slots busy for
/// `dur_s` (one thread for `unmatched-heavy`), compressed by `speed`. The
/// work is fixed per call and split over however many threads the mode
/// allows, so contention or a smaller cap stretches the wall time as it
/// would for a real build.
pub fn work_s(call: &Call, rule_slots: u32, speed: f64) -> f64 {
    let threads = if call.matched() { rule_slots } else { 1 };
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
struct Outcome {
    matched: bool,
    slots: u32,
    /// Wall seconds from replay start.
    start: f64,
    admitted: f64,
    end: f64,
    status: Option<ExitStatus>,
}

impl Outcome {
    fn failed(&self) -> bool {
        !self.status.is_some_and(|s| s.success())
    }
    fn sigkilled(&self) -> bool {
        use std::os::unix::process::ExitStatusExt;
        self.status.and_then(|s| s.signal()) == Some(libc::SIGKILL)
    }
}

/// Everything a call needs to run, shared across the call threads.
struct Plan {
    mode: BenchMode,
    exe: PathBuf,
    out: PathBuf,
    speed: f64,
    config: Config,
    /// Per-agent worktrees and rule commands, tier 2 only.
    real: Option<(Real, PathBuf)>,
}

impl Plan {
    fn command(&self, i: usize, call: &Call) -> anyhow::Result<(Command, Option<PathBuf>)> {
        let rule_slots = self.config.rule(&call.rule).map_or(1, |r| r.slots);
        let burn = |threads: Option<u32>| {
            let mut c = vec![
                self.exe.to_string_lossy().into_owned(),
                "bench".into(),
                "burn".into(),
                "--cpu-s".into(),
                format!("{}", work_s(call, rule_slots, self.speed)),
            ];
            if let Some(t) = threads {
                c.extend(["--threads".into(), t.to_string()]);
            }
            c
        };
        // Unclaimed commands never reach muzzle: one thread, in both modes.
        let argv = if !call.matched() {
            burn(Some(1))
        } else if let Some((real, _)) = &self.real {
            vec!["bash".into(), "-c".into(), real.rules[&call.rule].clone()]
        } else {
            // The burner reads its threads from CARGO_BUILD_JOBS, which
            // `muzzle run` sets from the claim (D14) and mode none sets
            // from the role cap.
            burn(None)
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
            Some((_, dir)) => dir.join(&call.agent),
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

    fn run_call(&self, i: usize, call: &Call, t0: Instant) -> anyhow::Result<Outcome> {
        let (mut cmd, report) = self.command(i, call)?;
        let start = t0.elapsed().as_secs_f64();
        let status = cmd.status().context("spawning a replayed call")?;
        let end = t0.elapsed().as_secs_f64();
        let mut o = Outcome {
            matched: call.matched(),
            slots: if self.mode == BenchMode::None {
                role_cap(&call.role)
            } else {
                self.config.rule(&call.rule).map_or(1, |r| r.slots)
            },
            start,
            admitted: start,
            end,
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
                }
                // No report: muzzle itself failed before admission.
                None => o.status = None,
            }
        }
        Ok(o)
    }
}

/// Set up tier 2's per-agent worktrees and pre-build each once.
fn prepare_real(real: &Real, agents: &BTreeSet<&str>) -> anyhow::Result<PathBuf> {
    let repo = expand(&real.repo);
    let dir = expand(&real.workdir);
    std::fs::create_dir_all(&dir)?;
    let sh = |cwd: &Path, script: &str| -> anyhow::Result<()> {
        let st = Command::new("bash")
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

fn p5090(v: &[f64]) -> Value {
    let r = |p| percentile(v, p).map(probe::round2);
    json!({"p50": r(50.0), "p90": r(90.0)})
}

pub fn run_main(args: &BenchRunArgs) -> anyhow::Result<ExitCode> {
    let mode = args.mode.unwrap_or(BenchMode::Muzzle);
    let tier = args.tier.unwrap_or(BenchTier::Synthetic);
    if !(args.speed.is_finite() && args.speed > 0.0) {
        eprintln!("muzzle: --speed {}: must be a positive number", args.speed);
        return Ok(ExitCode::from(crate::exit::USAGE));
    }
    let window = match args.window.as_deref().map(parse_window).transpose() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("muzzle: {e}");
            return Ok(ExitCode::from(crate::exit::USAGE));
        }
    };
    let text = std::fs::read_to_string(&args.trace)
        .with_context(|| format!("reading {}", args.trace.display()))?;
    let (header, all) = parse_trace(&text).context("bad trace")?;
    let (window_start, mut calls) = match window {
        Some(len) => {
            let s = busiest_start(&all, len);
            let mut kept: Vec<Call> = all
                .iter()
                .filter(|c| c.t >= s && c.t < s + len)
                .cloned()
                .collect();
            kept.iter_mut().for_each(|c| c.t -= s);
            (Some(s), kept)
        }
        None => (None, all),
    };
    let light = calls.iter().filter(|c| c.rule == LIGHT).count();
    calls.retain(|c| c.rule != LIGHT);

    let config = Config::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let missing: BTreeSet<&str> = calls
        .iter()
        .filter(|c| c.matched() && config.rule(&c.rule).is_none())
        .map(|c| c.rule.as_str())
        .collect();
    if !missing.is_empty() {
        eprintln!("muzzle: bench: rules not in the config: {missing:?}");
        return Ok(ExitCode::from(crate::exit::USAGE));
    }

    let out = args.out.clone().unwrap_or_else(|| {
        let ts = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ");
        PathBuf::from(format!("muzzle-bench-{ts}-{mode:?}-{tier:?}").to_lowercase())
    });
    std::fs::create_dir_all(&out)?;
    let out = out.canonicalize()?;
    // A fresh queue and log every time: the bench measures one replay.
    for stale in ["state", "reports", "runs.jsonl"] {
        let p = out.join(stale);
        let _ = std::fs::remove_dir_all(&p).or_else(|_| std::fs::remove_file(&p));
    }
    std::fs::create_dir_all(out.join("reports"))?;

    let real = match tier {
        BenchTier::Synthetic => None,
        BenchTier::Real => {
            let path = args
                .real
                .clone()
                .unwrap_or_else(|| "bench/real.toml".into());
            let real: Real = toml::from_str(
                &std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
            )
            .with_context(|| format!("parsing {}", path.display()))?;
            let missing: BTreeSet<&str> = calls
                .iter()
                .filter(|c| c.matched() && !real.rules.contains_key(&c.rule))
                .map(|c| c.rule.as_str())
                .collect();
            if !missing.is_empty() {
                bail!("{} has no command for {missing:?}", path.display());
            }
            let agents = calls.iter().map(|c| c.agent.as_str()).collect();
            let dir = prepare_real(&real, &agents)?;
            Some((real, dir))
        }
    };

    let plan = Arc::new(Plan {
        mode,
        exe: std::env::current_exe().context("finding the muzzle binary")?,
        out: out.clone(),
        speed: args.speed,
        config,
        real,
    });

    // D35: the probe samples alongside the whole replay.
    let probe = Probe::open(&out.join("probe-repo"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let bursts = Arc::new(Mutex::new(Vec::<Burst>::new()));
    let prober = {
        let (stop, bursts) = (stop.clone(), bursts.clone());
        std::thread::spawn(move || -> anyhow::Result<()> {
            // At least one burst, even for a replay shorter than one.
            loop {
                let b = probe.burst()?;
                bursts.lock().unwrap().push(b);
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
            }
        })
    };

    eprintln!(
        "muzzle: bench: replaying {} calls ({light} light skipped), mode {mode:?}, tier {tier:?}, speed {}",
        calls.len(),
        args.speed
    );
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for (i, call) in calls.iter().cloned().enumerate() {
        let due = Duration::from_secs_f64(call.t / args.speed);
        if let Some(wait) = due.checked_sub(t0.elapsed()) {
            std::thread::sleep(wait);
        }
        let plan = plan.clone();
        handles.push(std::thread::spawn(move || plan.run_call(i, &call, t0)));
    }
    let outcomes = handles
        .into_iter()
        .map(|h| h.join().expect("call thread panicked"))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let makespan = t0.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    prober.join().expect("probe thread panicked")?;
    let _ = std::fs::remove_dir_all(out.join("probe-repo"));

    // runs.jsonl stays next to the result; it is local state (never
    // published), unlike result.json and summary.md.
    let runs_src = out.join("state/muzzle/runs.jsonl");
    let runs = std::fs::read_to_string(&runs_src).unwrap_or_default();
    if mode == BenchMode::Muzzle {
        std::fs::write(out.join("runs.jsonl"), &runs)?;
    }
    let budget = crate::state::Store::open(out.join("state/muzzle"))
        .and_then(|s| s.read())
        .ok()
        .and_then(|s| s.budget)
        .map(|b| b.budget);

    let bursts = bursts.lock().unwrap();
    let result = result_json(&Figures {
        mode,
        tier,
        speed: args.speed,
        window: args.window.as_deref(),
        window_start,
        header: &header,
        light,
        outcomes: &outcomes,
        makespan,
        bursts: &bursts,
        runs: &runs,
        budget,
        cfg: plan.config.hash(),
    });
    std::fs::write(
        out.join("result.json"),
        serde_json::to_string_pretty(&result)? + "\n",
    )?;
    std::fs::write(out.join("summary.md"), summary(&result))?;
    eprintln!("muzzle: bench: wrote {}", out.join("result.json").display());
    Ok(ExitCode::SUCCESS)
}

struct Figures<'a> {
    mode: BenchMode,
    tier: BenchTier,
    speed: f64,
    window: Option<&'a str>,
    window_start: Option<f64>,
    header: &'a Header,
    light: usize,
    outcomes: &'a [Outcome],
    makespan: f64,
    bursts: &'a [Burst],
    runs: &'a str,
    budget: Option<u32>,
    cfg: String,
}

/// result.json: the D35 figures. Times are wall seconds at `speed`. No
/// paths or command text: results are committed to `docs/bench/` (D32).
fn result_json(f: &Figures) -> Value {
    let o = f.outcomes;
    let waits: Vec<f64> = o.iter().map(|c| c.admitted - c.start).collect();
    let runs: Vec<f64> = o.iter().map(|c| c.end - c.admitted).collect();
    let held: Vec<(f64, f64, u32)> = o
        .iter()
        .filter(|c| c.matched)
        .map(|c| (c.admitted, c.end, c.slots))
        .collect();
    // D30a: the scope's oom-kill under muzzle; without a scope, a SIGKILL
    // is the kernel OOM killer's signature.
    let oom_lines = f
        .runs
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["killed"] == "oom")
        .count();
    let oom = oom_lines + o.iter().filter(|c| c.sigkilled()).count();
    let git: Vec<f64> = f.bursts.iter().flat_map(|b| b.git_ms.clone()).collect();
    let late: Vec<f64> = f.bursts.iter().flat_map(|b| b.late_ms.clone()).collect();
    let load1: Vec<f64> = f
        .bursts
        .iter()
        .filter_map(|b| b.load.map(|l| l[0]))
        .collect();
    let jobs_per_hour = if f.makespan > 0.0 {
        probe::round2(o.len() as f64 * 3600.0 / f.makespan)
    } else {
        0.0
    };
    json!({
        "v": 1,
        "mode": format!("{:?}", f.mode).to_lowercase(),
        "tier": format!("{:?}", f.tier).to_lowercase(),
        "speed": f.speed,
        "window": f.window,
        "window_start_s": f.window_start,
        "trace": {
            "host": f.header.host,
            "date": f.header.date,
            "cores": f.header.cores,
            "ram_gb": f.header.ram_gb,
            "window_s": f.header.window_s,
            "calls_kept": f.header.calls_kept,
        },
        "machine": {
            "cores": crate::proc::logical_cores().ok(),
            "budget": f.budget,
            "cfg": f.cfg,
        },
        "calls": {
            "replayed": o.len(),
            "matched": o.iter().filter(|c| c.matched).count(),
            "unmatched_heavy": o.iter().filter(|c| !c.matched).count(),
            "light_skipped": f.light,
            "failed": o.iter().filter(|c| c.failed()).count(),
        },
        "makespan_s": probe::round2(f.makespan),
        "jobs_per_hour": jobs_per_hour,
        "wait_s": p5090(&waits),
        "run_s": p5090(&runs),
        "max_slots_held": max_held(&held),
        "probe": {
            "bursts": f.bursts.len(),
            "git_status_p95_ms": percentile(&git, 95.0).map(probe::round2),
            "sleep_late_p99_ms": percentile(&late, 99.0).map(probe::round2),
        },
        "load": {
            "samples": load1.len(),
            "load1_p50": percentile(&load1, 50.0).map(probe::round2),
            "load1_max": load1.iter().copied().reduce(f64::max).map(probe::round2),
        },
        "oom_kills": oom,
    })
}

fn summary(r: &Value) -> String {
    let s = |v: &Value| match v {
        Value::Null => "-".to_string(),
        v => v.to_string(),
    };
    format!(
        "# muzzle bench: mode {mode}, tier {tier}\n\n\
         Trace {date} ({cores} cores), speed {speed}, window {window}. \
         Times are wall seconds at that speed.\n\n\
         | figure | value |\n|---|---|\n\
         | calls replayed (light skipped) | {replayed} ({light}) |\n\
         | failed | {failed} |\n\
         | makespan | {makespan} s |\n\
         | throughput | {jph} jobs/h |\n\
         | wait p50/p90 | {w50}/{w90} s |\n\
         | run p50/p90 | {r50}/{r90} s |\n\
         | most slots held | {held} (budget {budget}) |\n\
         | git status p95 | {git} ms |\n\
         | 10 ms sleep late p99 | {late} ms |\n\
         | load1 p50/max | {l50}/{lmax} |\n\
         | OOM kills | {oom} |\n",
        mode = r["mode"].as_str().unwrap_or("-"),
        tier = r["tier"].as_str().unwrap_or("-"),
        date = r["trace"]["date"].as_str().unwrap_or("-"),
        cores = s(&r["trace"]["cores"]),
        speed = s(&r["speed"]),
        window = r["window"].as_str().unwrap_or("all"),
        replayed = s(&r["calls"]["replayed"]),
        light = s(&r["calls"]["light_skipped"]),
        failed = s(&r["calls"]["failed"]),
        makespan = s(&r["makespan_s"]),
        jph = s(&r["jobs_per_hour"]),
        w50 = s(&r["wait_s"]["p50"]),
        w90 = s(&r["wait_s"]["p90"]),
        r50 = s(&r["run_s"]["p50"]),
        r90 = s(&r["run_s"]["p90"]),
        held = s(&r["max_slots_held"]),
        budget = s(&r["machine"]["budget"]),
        git = s(&r["probe"]["git_status_p95_ms"]),
        late = s(&r["probe"]["sleep_late_p99_ms"]),
        l50 = s(&r["load"]["load1_p50"]),
        lmax = s(&r["load"]["load1_max"]),
        oom = s(&r["oom_kills"]),
    )
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

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(t: f64, dur_s: f64, rule: &str) -> Call {
        Call {
            t,
            dur_s,
            rule: rule.into(),
            role: "worker".into(),
            agent: "a1".into(),
        }
    }

    #[test]
    fn window_parses_units() {
        assert_eq!(parse_window("busiest:3h").unwrap(), 10800.0);
        assert_eq!(parse_window("busiest:90m").unwrap(), 5400.0);
        assert_eq!(parse_window("busiest:30s").unwrap(), 30.0);
        assert_eq!(parse_window("busiest:45").unwrap(), 45.0);
        for bad in ["3h", "busiest:", "busiest:0h", "busiest:xh", "busiest:-1"] {
            assert!(parse_window(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn busiest_window_counts_matched_call_seconds_only() {
        // A long light call and an unmatched-heavy one early must not pull
        // the window: the busiest 600 s hold both late gates whole.
        let calls = [
            call(0.0, 5000.0, LIGHT),
            call(10.0, 900.0, UNMATCHED),
            call(100.0, 50.0, "gate"),
            call(1000.0, 300.0, "gate"),
            call(1100.0, 300.0, "gate"),
        ];
        let s = busiest_start(&calls, 600.0);
        assert_eq!(window_load(&calls, s, 600.0), 600.0);
        assert_eq!(s, 800.0, "earliest of the tied starts");
    }

    #[test]
    fn busiest_window_beats_a_brute_force_scan() {
        let calls = [
            call(0.0, 100.0, "gate"),
            call(250.0, 40.0, "gate"),
            call(1000.0, 1000.0, "gate"),
            call(1900.0, 500.0, "gate"),
            call(2600.0, 30.0, "gate"),
        ];
        for len in [50.0, 300.0, 600.0, 1500.0] {
            let best = (0..3000)
                .map(|s| window_load(&calls, f64::from(s), len))
                .fold(0.0, f64::max);
            let s = busiest_start(&calls, len);
            assert_eq!(window_load(&calls, s, len), best, "len {len}");
        }
    }

    #[test]
    fn role_caps_follow_c3() {
        assert_eq!(role_cap("worker"), 4);
        assert_eq!(role_cap("orchestrator"), 6);
        assert_eq!(role_cap("reviewer"), 2);
        assert_eq!(role_cap("other"), 2);
    }

    #[test]
    fn synthetic_work_is_slots_times_duration_over_speed() {
        assert_eq!(work_s(&call(0.0, 10.0, "gate"), 6, 2.0), 30.0);
        // unmatched-heavy burns one thread whatever the slots.
        assert_eq!(work_s(&call(0.0, 10.0, UNMATCHED), 6, 2.0), 5.0);
    }

    #[test]
    fn max_held_sweeps_overlaps() {
        assert_eq!(max_held(&[]), 0);
        assert_eq!(max_held(&[(0.0, 2.0, 2), (1.0, 3.0, 3), (2.5, 4.0, 1)]), 5);
        // Back to back: the first frees before the second starts.
        assert_eq!(max_held(&[(0.0, 1.0, 4), (1.0, 2.0, 4)]), 4);
    }

    #[test]
    fn trace_needs_a_header_first() {
        let header = r#"{"v":1,"kind":"header","host":"linuxpc","cores":16,"ram_gb":31,"date":"2026-10-08","window_s":86400,"source":"museum","calls_total":1,"calls_kept":1,"calls_dropped":0}"#;
        let c = r#"{"v":1,"kind":"call","t":5.0,"dur_s":1,"dur_src":"durationMs","bg":false,"rule":"gate","role":"worker","agent":"a1","project":"p1"}"#;
        let (h, calls) = parse_trace(&format!("{header}\n{c}\n")).unwrap();
        assert_eq!((h.cores, calls.len()), (16, 1));
        assert!(parse_trace(c).is_err());
        assert!(parse_trace(&format!("{header}\n{header}")).is_err());
        assert!(parse_trace(&header.replace("\"v\":1", "\"v\":2")).is_err());
    }

    #[test]
    fn result_has_every_d35_field() {
        let header = Header {
            host: "linuxpc".into(),
            cores: 16,
            ram_gb: 31,
            date: "2026-10-08".into(),
            window_s: 86400.0,
            calls_kept: 2,
        };
        let o = |start: f64, admitted: f64, end: f64, slots| Outcome {
            matched: true,
            slots,
            start,
            admitted,
            end,
            status: None,
        };
        let outcomes = [o(0.0, 0.0, 2.0, 4), o(1.0, 2.0, 3.0, 4)];
        let bursts = [Burst {
            git_ms: vec![1.0, 2.0],
            late_ms: vec![0.1, 0.3],
            load: Some([1.0, 1.0, 1.0]),
        }];
        let r = result_json(&Figures {
            mode: BenchMode::Muzzle,
            tier: BenchTier::Synthetic,
            speed: 10.0,
            window: None,
            window_start: None,
            header: &header,
            light: 1,
            outcomes: &outcomes,
            makespan: 3.0,
            bursts: &bursts,
            runs: "{\"killed\":\"oom\"}\n{\"killed\":null}\n",
            budget: Some(4),
            cfg: "abc".into(),
        });
        assert_eq!(r["makespan_s"], 3.0);
        assert_eq!(r["jobs_per_hour"], 2400.0);
        assert_eq!(r["wait_s"]["p90"], 1.0);
        assert_eq!(r["run_s"]["p50"], 1.0);
        assert_eq!(r["max_slots_held"], 4);
        assert_eq!(r["probe"]["git_status_p95_ms"], 2.0);
        assert_eq!(r["probe"]["sleep_late_p99_ms"], 0.3);
        assert_eq!(r["load"]["load1_max"], 1.0);
        assert_eq!(r["oom_kills"], 1);
        assert_eq!(r["calls"]["failed"], 2);
        assert!(summary(&r).contains("| makespan | 3.0 s |"));
    }
}
