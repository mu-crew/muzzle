//! Linux half of `proc`: `/proc/<pid>/stat`, `/proc/meminfo`, and cgroup v2
//! files for the scope's figures.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{CgroupFigures, Pid, Stat};

/// `ru_maxrss` is in KiB on Linux.
pub(super) const MAXRSS_UNIT: u64 = 1024;

/// Parse `/proc/<pid>/stat`. `None` when the process is gone or a zombie.
pub(super) fn stat(pid: Pid) -> Option<Stat> {
    if pid <= 0 {
        return None;
    }
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn parse_stat(text: &str) -> Option<Stat> {
    // comm (field 2) is in parentheses and may itself hold spaces and ')',
    // so split after the last ')'.
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    // fields[0] is field 3 (state), so field n is fields[n - 3].
    if matches!(*fields.first()?, "Z" | "X" | "x") {
        return None;
    }
    Some(Stat {
        ppid: fields.get(1)?.parse().ok()?,
        pgid: fields.get(2)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
    })
}

pub(super) fn group_members(pgid: Pid) -> io::Result<Vec<Pid>> {
    let mut members = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        // A process can exit mid-scan; its stat read just fails.
        if stat(pid).is_some_and(|s| s.pgid == pgid) {
            members.push(pid);
        }
    }
    Ok(members)
}

pub(super) fn logical_cores() -> io::Result<usize> {
    std::thread::available_parallelism().map(usize::from)
}

pub(super) fn total_ram() -> io::Result<u64> {
    parse_meminfo(&fs::read_to_string("/proc/meminfo")?)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no MemTotal in /proc/meminfo"))
}

fn parse_meminfo(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_ascii_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

pub(super) fn probe_systemd_user() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|out| super::manager_answered(&String::from_utf8_lossy(&out.stdout)))
}

pub(super) fn cgroup_of(pid: Pid) -> Option<PathBuf> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    // cgroup v2 only: the unified line is `0::<path>`.
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// Read the cgroup files directly rather than `systemctl --user show`:
/// systemd collects a finished scope at once, and on systemd 259 `show`
/// then reports `CPUUsageNSec=[not set]` (checked on linuxpc).
pub(super) fn cgroup_figures(dir: &Path) -> Option<CgroupFigures> {
    let read = |name: &str| fs::read_to_string(dir.join(name)).ok();
    let usage_usec = keyed(&read("cpu.stat")?, "usage_usec")?;
    Some(CgroupFigures {
        cpu_s: usage_usec as f64 / 1e6,
        mem_peak_bytes: read("memory.peak").and_then(|s| s.trim().parse().ok()),
        oom_kills: read("memory.events")
            .and_then(|s| keyed(&s, "oom_kill"))
            .unwrap_or(0),
    })
}

/// The value of `key` in a cgroup flat-keyed file (`key value` lines).
fn keyed(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Child;

    #[test]
    fn parse_stat_handles_odd_comm() {
        let text = "42 (a) b) c) S 7 9 9 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 1 0 123456 1 2";
        assert_eq!(
            parse_stat(text),
            Some(Stat {
                ppid: 7,
                pgid: 9,
                start: 123456
            })
        );
    }

    #[test]
    fn parse_stat_treats_zombie_as_gone() {
        let text = "42 (sh) Z 7 9 9 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 1 0 123456 1 2";
        assert_eq!(parse_stat(text), None);
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn parse_meminfo_reads_memtotal() {
        let text = "MemTotal:       65536 kB\nMemFree:  1 kB\n";
        assert_eq!(parse_meminfo(text), Some(65536 * 1024));
        assert_eq!(parse_meminfo("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn keyed_reads_flat_files() {
        let events = "low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\noom_group_kill 0\n";
        assert_eq!(keyed(events, "oom_kill"), Some(1));
        assert_eq!(keyed(events, "oom"), Some(1));
        assert_eq!(keyed(events, "missing"), None);
    }

    #[test]
    fn cgroup_figures_from_fixture_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cpu.stat"),
            "usage_usec 2500000\nuser_usec 1\n",
        )
        .unwrap();
        fs::write(dir.path().join("memory.peak"), "1048576\n").unwrap();
        fs::write(dir.path().join("memory.events"), "oom 0\noom_kill 2\n").unwrap();
        assert_eq!(
            cgroup_figures(dir.path()),
            Some(CgroupFigures {
                cpu_s: 2.5,
                mem_peak_bytes: Some(1 << 20),
                oom_kills: 2
            })
        );
        fs::remove_file(dir.path().join("memory.peak")).unwrap();
        assert_eq!(cgroup_figures(dir.path()).unwrap().mem_peak_bytes, None);
        assert_eq!(cgroup_figures(&dir.path().join("nope")), None);
    }

    #[test]
    fn cgroup_of_this_process_is_readable() {
        // Containers without cgroup v2 have no `0::` line; skip there.
        let Some(dir) = cgroup_of(std::process::id() as Pid) else {
            return;
        };
        assert!(dir.starts_with("/sys/fs/cgroup"));
    }

    /// A real scope, when this machine has a user manager: the figures are
    /// read from inside the scope while it is alive, as `run` does.
    #[test]
    fn real_scope_figures_when_systemd_reachable() {
        if !super::super::systemd_user_reachable() {
            eprintln!("skipped: no systemd user manager");
            return;
        }
        let mut child: Child = Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "sleep", "30"])
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as Pid;
        // systemd-run execs the command in place once the scope exists.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let dir = loop {
            if let Some(d) = cgroup_of(pid).filter(|d| d.to_string_lossy().ends_with(".scope")) {
                break d;
            }
            assert!(std::time::Instant::now() < deadline, "scope never appeared");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let figs = cgroup_figures(&dir).expect("live scope has cpu.stat");
        assert!(figs.cpu_s >= 0.0);
        assert_eq!(figs.oom_kills, 0);
        // SAFETY: plain syscall.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        child.wait().unwrap();
    }
}
