//! `bench run TRACE`: tiers, modes, `--window`, result.json, summary.md
//! (spec §Benchmark runner, D31, D35, C3).
//!
//! The replay is plain processes: `--mode muzzle` runs each matched call
//! through `muzzle run` (this binary) with a private state dir, so admission
//! is only ever touched through `run` (§Architecture 6).

mod result;
mod sim;
mod trace;

pub use sim::{Real, burn_main, max_held, role_cap, synthetic_threads, work_s};
pub use trace::{
    Call, Header, LIGHT, UNMATCHED, busiest_start, in_window, parse_trace, parse_window,
    window_load,
};

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};

use super::probe::{Burst, Probe};
use crate::cli::{BenchMode, BenchRunArgs, BenchTier};
use crate::rules::config::Config;
use result::{Figures, result_json, summary};
use sim::{Plan, prepare_real};

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
                .filter(|c| in_window(c, s, len))
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
        .filter(|c| c.matched() && config.rule_anywhere(&c.rule).is_none())
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
    let t0_ms = now_ms();
    let mut handles = Vec::new();
    for (i, call) in calls.iter().cloned().enumerate() {
        let due = Duration::from_secs_f64(call.t / args.speed);
        if let Some(wait) = due.checked_sub(t0.elapsed()) {
            std::thread::sleep(wait);
        }
        let plan = plan.clone();
        handles.push(std::thread::spawn(move || {
            plan.run_call(i, &call, t0, t0_ms)
        }));
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

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
