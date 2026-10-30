//! The interactive probe: `git status` p95 in a scratch repo and 10 ms
//! sleep lateness at p99 (D35), plus the load average.
//!
//! One *sample* is a short burst (several `git status` runs, many 10 ms
//! sleeps) so that a single cron invocation already yields a p95/p99.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::Serialize;

use crate::proc::loadavg;
use crate::report::percentile;

/// `git status` runs per burst. `percentile` is nearest-rank, so p95 of 20
/// is the 19th value, the second slowest: one outlier run doesn't set it.
const GIT_RUNS: usize = 20;
/// 10 ms sleeps per burst (~1 s). Nearest-rank p99 of 100 is the 99th
/// value, the second-latest wake.
const SLEEPS: usize = 100;
const SLEEP: Duration = Duration::from_millis(10);
/// Files in the scratch repo: `git status` must stat something.
const FILES: usize = 400;

/// The raw figures of one burst.
#[derive(Debug, Clone, Default)]
pub struct Burst {
    pub git_ms: Vec<f64>,
    pub late_ms: Vec<f64>,
    pub load: Option<[f64; 3]>,
}

/// One `bench probe` line.
#[derive(Debug, Serialize)]
pub struct Sample {
    pub v: u32,
    pub ts: String,
    pub git_status_p95_ms: Option<f64>,
    pub sleep_late_p99_ms: Option<f64>,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
}

impl Burst {
    pub fn sample(&self, now_ms: u64) -> Sample {
        let load = |i: usize| self.load.map(|l| round2(l[i]));
        Sample {
            v: 1,
            ts: crate::admit::rfc3339(now_ms),
            git_status_p95_ms: percentile(&self.git_ms, 95.0).map(round2),
            sleep_late_p99_ms: percentile(&self.late_ms, 99.0).map(round2),
            load1: load(0),
            load5: load(1),
            load15: load(2),
        }
    }
}

pub fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// A scratch git repo with a committed tree, reused when it already exists
/// so repeated cron samples don't pay for setup.
pub struct Probe {
    repo: PathBuf,
}

impl Probe {
    pub fn open(repo: &Path) -> anyhow::Result<Probe> {
        if !repo.join(".git").is_dir() {
            init_repo(repo).with_context(|| format!("making the probe repo {}", repo.display()))?;
        }
        Ok(Probe {
            repo: repo.to_path_buf(),
        })
    }

    pub fn burst(&self) -> anyhow::Result<Burst> {
        let mut b = Burst {
            load: loadavg(),
            ..Burst::default()
        };
        for _ in 0..GIT_RUNS {
            let t = Instant::now();
            let st = git(&self.repo)
                .args(["status", "--porcelain"])
                .stdout(Stdio::null())
                .status()?;
            if !st.success() {
                bail!("git status failed in the probe repo: {st}");
            }
            b.git_ms.push(ms(t.elapsed()));
        }
        for _ in 0..SLEEPS {
            let t = Instant::now();
            std::thread::sleep(SLEEP);
            b.late_ms.push(ms(t.elapsed().saturating_sub(SLEEP)));
        }
        Ok(b)
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// git with no user config, hooks or signing: the probe measures the
/// machine, not the user's setup.
fn git(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    c
}

fn init_repo(repo: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(repo)?;
    for i in 0..FILES {
        let dir = repo.join(format!("d{}", i % 20));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("f{i}.txt")), format!("{i}\n"))?;
    }
    let steps: [&[&str]; 3] = [
        &["init", "-q"],
        &["add", "-A"],
        &[
            "-c",
            "user.name=probe",
            "-c",
            "user.email=probe@invalid",
            "commit",
            "-q",
            "--no-gpg-sign",
            "-m",
            "probe",
        ],
    ];
    for args in steps {
        let st = git(repo).args(args).stdout(Stdio::null()).status()?;
        if !st.success() {
            bail!("git {} failed: {st}", args.join(" "));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_takes_p95_and_p99_of_the_burst() {
        let b = Burst {
            git_ms: (1..=20).map(f64::from).collect(),
            late_ms: (1..=100).map(f64::from).collect(),
            load: Some([1.234, 2.0, 3.0]),
        };
        let s = b.sample(0);
        assert_eq!(s.git_status_p95_ms, Some(19.0));
        assert_eq!(s.sleep_late_p99_ms, Some(99.0));
        assert_eq!(s.load1, Some(1.23));
        assert_eq!(s.ts, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn loadavg_reads() {
        let l = loadavg().unwrap();
        assert!(l.iter().all(|x| *x >= 0.0));
    }
}
