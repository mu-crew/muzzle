//! The systemd scope: unit names, sampling, outcome and cleanup.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::spawn::exited;
use super::{POLL, SYSTEMD_EXPAND_FLAG, scope_known_empty};
use crate::proc::{self, CgroupFigures, Pid, StartTime};

/// The job's scope unit. `MUZZLE_UNIT_PREFIX` exists for the integration
/// tests: each test names its own scopes, so it can check that it left none
/// `failed` among other suites' units on a shared host.
pub(super) fn unit_name(pid: u32, start: StartTime, ticket: u64) -> String {
    let prefix = std::env::var("MUZZLE_UNIT_PREFIX")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "muzzle".to_string());
    format!("{prefix}-{pid}-{start}-{ticket}")
}

/// Whether systemd has the unit loaded at all; `false` if unanswerable.
pub(super) fn scope_loaded(name: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "show", "-p", "LoadState", "--value"])
        .arg(name)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "loaded")
}

pub(super) fn stop_unit(name: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "stop"])
        .arg(name)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Drop a scope's `failed` state; failed units stay loaded until reset.
/// Harmless on a unit that is gone or never existed.
pub(super) fn reset_failed(name: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "reset-failed"])
        .arg(name)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The last figures read from the job's scope (Linux, enforced). The scope
/// is removed the moment it empties, so figures are sampled while it lives
/// (FROM proc: systemd 259 collects a finished scope at once).
pub(super) struct Sampler {
    pub(super) unit: String,
    pub(super) dir: Option<PathBuf>,
    pub(super) last: Option<CgroupFigures>,
    pub(super) next: Instant,
}

impl Sampler {
    pub(super) fn sample(&mut self, pid: Pid) {
        if Instant::now() < self.next {
            return;
        }
        self.next = Instant::now() + POLL;
        if self.dir.is_none() {
            // Until systemd-run has moved itself into the scope, the child
            // sits in muzzle's own cgroup, whose figures would be wrong.
            let scope = format!("{}.scope", self.unit);
            self.dir = proc::cgroup_of(pid).filter(|d| d.file_name().is_some_and(|n| *n == *scope));
        }
        if let Some(f) = self.dir.as_deref().and_then(proc::cgroup_figures) {
            self.last = Some(f);
        }
    }
}

/// The scope's cgroup directory once `pid` (systemd-run, then the command)
/// has moved into it; `None` if it exits first or `within` passes.
pub(super) fn scope_dir(pid: Pid, unit: &str, within: Duration) -> Option<PathBuf> {
    let scope = format!("{unit}.scope");
    let deadline = Instant::now() + within;
    loop {
        let dir = proc::cgroup_of(pid).filter(|d| d.file_name().is_some_and(|n| *n == *scope));
        if dir.is_some() || exited(pid) || Instant::now() >= deadline {
            return dir;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `systemctl --user show` fields for a scope that's gone or going.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Show {
    pub(super) active: String,
    pub(super) result: String,
    pub(super) cpu_s: Option<f64>,
    pub(super) peak: Option<u64>,
}

pub(super) fn parse_show(text: &str) -> Show {
    let mut s = Show::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "ActiveState" => s.active = v.to_string(),
            "Result" => s.result = v.to_string(),
            "CPUUsageNSec" => s.cpu_s = v.parse::<u64>().ok().map(|n| n as f64 / 1e9),
            "MemoryPeak" => s.peak = v.parse().ok(),
            _ => {}
        }
    }
    s
}

/// D30a: whether the scope was killed at `MemoryMax`. systemd's default
/// `OOMPolicy=stop` kills the whole scope, which removes the cgroup before a
/// sample can see `oom_kill`, but leaves the unit `failed` with
/// `Result=oom-kill` (checked on linuxpc, systemd 259). systemd marks it
/// asynchronously, so wait up to 1 s for it to settle. A clean scope is
/// collected at once and reads as `inactive`.
pub(super) fn scope_outcome(unit: &str) -> Show {
    let name = format!("{unit}.scope");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let show = Command::new("systemctl")
            .args(["--user", "show", "-p"])
            .arg("ActiveState,Result,CPUUsageNSec,MemoryPeak")
            .arg(&name)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| parse_show(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default();
        let settled = !matches!(
            show.active.as_str(),
            "active" | "activating" | "deactivating" | "reloading"
        );
        if settled || Instant::now() >= deadline {
            // Don't leave one failed unit per OOM behind. A scope still
            // settling at the deadline may fail after this read, so reset it
            // too: anything not cleanly `inactive`. A scope whose systemd-run
            // was signalled mid-setup stays `active` with an empty cgroup
            // forever (see `poll`); stop it, only here at the deadline so an
            // OOM-killed scope has had time to record `oom-kill`.
            if !settled && scope_loaded(&name) && scope_known_empty(&name) {
                stop_unit(&name);
            }
            if show.active != "inactive" {
                reset_failed(&name);
            }
            return show;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(super) fn systemd_version(text: &str) -> Option<u32> {
    text.split_whitespace().nth(1)?.parse().ok()
}

pub(super) fn systemd_run_expands_env() -> bool {
    Command::new("systemd-run")
        .arg("--version")
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| systemd_version(&String::from_utf8_lossy(&o.stdout)))
        .is_some_and(|v| v >= SYSTEMD_EXPAND_FLAG)
}

/// D15: CPU weight in proportion to the claim; systemd's default is 100.
pub(super) fn cpu_weight(slots: u32) -> u32 {
    (100 * slots).clamp(1, 10_000)
}

pub(super) fn mb(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 20)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_parsing() {
        let s = parse_show(
            "ActiveState=failed\nResult=oom-kill\nCPUUsageNSec=38927000\nMemoryPeak=67108864\n",
        );
        assert_eq!(s.result, "oom-kill");
        assert_eq!(s.active, "failed");
        assert_eq!(s.peak, Some(64 << 20));
        assert!((s.cpu_s.unwrap() - 0.038927).abs() < 1e-9);
        let gone = parse_show("ActiveState=inactive\nResult=success\nCPUUsageNSec=[not set]\n");
        assert_eq!((gone.cpu_s, gone.peak), (None, None));
    }

    #[test]
    fn systemd_version_gates_the_expand_flag() {
        assert_eq!(
            systemd_version("systemd 259 (259.9-1.fc44)\n+PAM +AUDIT"),
            Some(259)
        );
        assert_eq!(systemd_version("nonsense"), None);
    }

    #[test]
    fn weights_and_sizes() {
        assert_eq!(cpu_weight(1), 100);
        assert_eq!(cpu_weight(6), 600);
        assert_eq!(cpu_weight(1000), 10_000);
        assert_eq!(mb(1), 1);
        assert_eq!(mb(1 << 20), 1);
        assert_eq!(mb((1 << 20) + 1), 2);
    }
}
