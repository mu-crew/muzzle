//! Linux half of `proc`: `/proc/<pid>/stat`, `/proc/meminfo`, and cgroup v2
//! files for the scope's figures.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{CgroupFigures, Pid, StartTime, Stat};

/// `ru_maxrss` is in KiB on Linux.
pub(super) const MAXRSS_UNIT: u64 = 1024;

/// Parse `/proc/<pid>/stat`. `None` when the process is gone or a zombie.
pub(super) fn stat(pid: Pid) -> Option<Stat> {
    if pid <= 0 {
        return None;
    }
    parse_stat(&fs::read(format!("/proc/{pid}/stat")).ok()?)
}

/// The fields after comm, split after the last ')' (comm may hold spaces and
/// ')'). Bytes, not `str`: the kernel writes comm raw, so a non-UTF-8 comm
/// must not hide a live process. The numeric fields are ASCII.
fn after_comm(text: &[u8]) -> Option<(usize, Vec<&str>)> {
    let close = text.iter().rposition(|&b| b == b')')?;
    let fields = text[close + 1..]
        .split(u8::is_ascii_whitespace)
        .filter(|f| !f.is_empty())
        .map(|f| std::str::from_utf8(f).ok())
        .collect::<Option<Vec<_>>>()?;
    Some((close, fields))
}

fn parse_stat(text: &[u8]) -> Option<Stat> {
    let (_, fields) = after_comm(text)?;
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

/// Parse `(comm, start, cpu_ns)` from one `/proc/<pid>/stat` read. Fields 14
/// and 15 (utime, stime) are in clock ticks, which `ns_per_tick` converts.
/// `None` for a zombie or a malformed line.
fn parse_cpu(text: &[u8], ns_per_tick: u64) -> Option<(String, StartTime, u64)> {
    let (close, fields) = after_comm(text)?;
    let open = text.iter().position(|&b| b == b'(')?;
    // Lossy: comm is only for diagnostics.
    let comm = String::from_utf8_lossy(text.get(open + 1..close)?).into_owned();
    if matches!(*fields.first()?, "Z" | "X" | "x") {
        return None;
    }
    // fields[0] is field 3, so field n is fields[n - 3].
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((
        comm,
        start,
        utime.saturating_add(stime).saturating_mul(ns_per_tick),
    ))
}

fn ns_per_tick() -> u64 {
    // SAFETY: plain sysconf call.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 {
        1_000_000_000 / hz as u64
    } else {
        10_000_000
    }
}

/// `(start, cpu_ns)` of one live process; `Err(errno)` when unreadable.
/// A process that is gone or a zombie reads as `ESRCH`.
pub(super) fn member_cpu(pid: Pid) -> Result<(StartTime, u64), i32> {
    let text = fs::read(format!("/proc/{pid}/stat"))
        .map_err(|e| e.raw_os_error().unwrap_or(libc::ESRCH))?;
    parse_cpu(&text, ns_per_tick())
        .map(|(_, start, cpu)| (start, cpu))
        .ok_or(libc::ESRCH)
}

/// The `comm` of a live process.
pub(super) fn comm(pid: Pid) -> Option<String> {
    let text = fs::read(format!("/proc/{pid}/stat")).ok()?;
    parse_cpu(&text, 1).map(|(comm, _, _)| comm)
}

pub(super) fn all_pids() -> io::Result<Vec<Pid>> {
    Ok(fs::read_dir("/proc")?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect())
}

pub(super) fn adopt_orphans() {
    // SAFETY: plain syscall. A failure leaves orphans to init, as before.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
}

pub(super) fn reap_adopted(keep: Pid) {
    loop {
        // SAFETY: a zeroed siginfo is valid; WNOWAIT only peeks, so `keep`
        // is never reaped here.
        let pid = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
            if libc::waitid(libc::P_ALL, 0, &mut info, flags) != 0 {
                return;
            }
            info.si_pid()
        };
        if pid == 0 || pid == keep {
            return;
        }
        // SAFETY: plain syscall on a child that has exited.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
    }
}

fn is_member(read: io::Result<Vec<u8>>, pgid: Pid, getpgid: impl FnOnce() -> Pid) -> bool {
    match read {
        Ok(text) => parse_stat(&text).is_some_and(|s| s.pgid == pgid),
        // hidepid or similar: the stat is unreadable but the process is live.
        // getpgid needs no /proc access; list it so group_cpu sees the EPERM
        // and answers None instead of a partial figure.
        Err(e) if matches!(e.raw_os_error(), Some(libc::EPERM | libc::EACCES)) => getpgid() == pgid,
        // A process can exit mid-scan; its stat read just fails.
        Err(_) => false,
    }
}

pub(super) fn group_members(pgid: Pid) -> io::Result<Vec<Pid>> {
    // Kernel threads sit in pgid 0 (269 on linuxpc); no real group is <= 0.
    if pgid <= 0 {
        return Ok(Vec::new());
    }
    let mut members = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        let read = fs::read(format!("/proc/{pid}/stat"));
        // SAFETY: plain syscall.
        let is_member = is_member(read, pgid, || unsafe { libc::getpgid(pid) });
        if is_member {
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

pub(super) fn namespace(kind: &str) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(format!("/proc/self/ns/{kind}"))
        .ok()
        .map(|m| m.ino())
}

pub(super) fn cgroup_populated(rel: &str) -> Option<bool> {
    let root = Path::new("/sys/fs/cgroup");
    // Not a cgroup v2 mount here (a sandbox, or v1): unanswerable.
    if !root.join("cgroup.controllers").exists() {
        return None;
    }
    match fs::read_to_string(root.join(rel.trim_start_matches('/')).join("cgroup.events")) {
        Ok(t) => Some(t.lines().any(|l| l == "populated 1")),
        // systemd removes a scope's cgroup once it empties.
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

pub(super) fn cgroup_has_process(leaf: &str) -> Option<bool> {
    // This process must read as cgroup v2, or "nobody found" means nothing.
    let me = fs::read_to_string("/proc/self/cgroup").ok()?;
    me.lines().find(|l| l.starts_with("0::"))?;
    for entry in fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        // A process can exit mid-scan; its read just fails.
        let Ok(text) = fs::read_to_string(entry.path().join("cgroup")) else {
            continue;
        };
        if text
            .lines()
            .filter_map(|l| l.strip_prefix("0::"))
            .any(|path| path.split('/').any(|seg| seg == leaf))
        {
            return Some(true);
        }
    }
    Some(false)
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
    fn parse_cpu_reads_comm_start_and_utime_plus_stime() {
        let text = "42 (a (b) c) S 7 9 9 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 1 0 123456 1 2";
        // utime 5 + stime 6 = 11 ticks.
        assert_eq!(
            parse_cpu(text.as_bytes(), 10_000_000),
            Some(("a (b) c".to_string(), 123456, 110_000_000))
        );
        assert_eq!(parse_cpu(text.replace(") S", ") Z").as_bytes(), 1), None);
    }

    #[test]
    fn parse_stat_handles_odd_comm() {
        let text = "42 (a) b) c) S 7 9 9 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 1 0 123456 1 2";
        assert_eq!(
            parse_stat(text.as_bytes()),
            Some(Stat {
                ppid: 7,
                pgid: 9,
                start: 123456
            })
        );
    }

    #[test]
    fn non_utf8_comm_still_parses() {
        let text = b"42 (build ) \xff) S 7 9 9 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 1 0 123456 1 2";
        assert_eq!(
            parse_stat(text),
            Some(Stat {
                ppid: 7,
                pgid: 9,
                start: 123456
            })
        );
        let (comm, start, cpu) = parse_cpu(text, 1).unwrap();
        assert_eq!(
            (comm.as_str(), start, cpu),
            ("build ) \u{fffd}", 123456, 11)
        );
    }

    #[test]
    fn unreadable_stat_falls_back_to_getpgid() {
        let eperm = || Err(io::Error::from_raw_os_error(libc::EPERM));
        assert!(is_member(eperm(), 9, || 9));
        assert!(!is_member(eperm(), 9, || 8));
        assert!(is_member(
            Err(io::Error::from_raw_os_error(libc::EACCES)),
            9,
            || 9
        ));
        // Gone mid-scan: not a member, and getpgid is not consulted.
        let gone = Err(io::Error::from_raw_os_error(libc::ENOENT));
        assert!(!is_member(gone, 9, || unreachable!()));
        let ok = |pgid: i32| {
            Ok(format!("1 (x) S 1 {pgid} 1 0 -1 0 1 2 3 4 5 6 7 8 20 0 1 0 5 1 2").into_bytes())
        };
        assert!(is_member(ok(9), 9, || unreachable!()));
        assert!(!is_member(ok(8), 9, || unreachable!()));
    }

    #[test]
    fn parse_stat_treats_zombie_as_gone() {
        let text = "42 (sh) Z 7 9 9 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 1 0 123456 1 2";
        assert_eq!(parse_stat(text.as_bytes()), None);
        assert_eq!(parse_stat(b"garbage"), None);
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
