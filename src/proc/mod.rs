//! Unit `proc` (spec §Architecture 3, §Platforms): liveness with start time,
//! group members, ancestry, cores, RAM, rusage, systemd reachability and the
//! scope's figures.
//!
//! One free-function API; `linux.rs` (`/proc`, `/proc/meminfo`, cgroup v2
//! files) and `macos.rs` (libproc `proc_listpgrppids` / `proc_pidinfo`,
//! `hw.logicalcpu`, `hw.memsize`) supply the platform halves. macOS goes
//! through libc's libproc bindings rather than the `libproc` crate: same
//! calls, and libc's bindings need no SDK to cross-check from Linux.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::OnceLock;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("muzzle supports Linux and macOS only (spec §Platforms)");

/// A systemd scope's figures, read from its cgroup v2 directory (D27, D30a).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CgroupFigures {
    /// `cpu.stat` `usage_usec`, in seconds: the systemd `CPUUsageNSec`.
    pub cpu_s: f64,
    /// `memory.peak` in bytes, the systemd `MemoryPeak`; `None` on kernels
    /// before 5.19, which lack the file.
    pub mem_peak_bytes: Option<u64>,
    /// `memory.events` `oom_kill`: non-zero means the scope hit `MemoryMax`
    /// and the run is `killed: "oom"`.
    pub oom_kills: u64,
}

/// A process id, as the kernel hands it out.
pub type Pid = libc::pid_t;

/// An opaque per-process start stamp. Linux: clock ticks since boot
/// (`/proc/<pid>/stat` field 22). macOS: the kernel's per-boot unique
/// process id (`p_uniqueid`), not a time. Only ever compared for equality
/// with a value read on the same host, so the units don't matter.
pub type StartTime = u64;

/// What the platform layer reads about one live process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stat {
    pub ppid: Pid,
    pub pgid: Pid,
    pub start: StartTime,
}

/// One step of a process's parent chain (D17 ancestry check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ancestor {
    pub pid: Pid,
    pub pgid: Pid,
    pub start: StartTime,
}

/// The start time of a live process, `None` when it is gone or a zombie.
pub fn start_time(pid: Pid) -> Option<StartTime> {
    imp::stat(pid).map(|s| s.start)
}

/// The process group of a live process.
pub fn pgid(pid: Pid) -> Option<Pid> {
    imp::stat(pid).map(|s| s.pgid)
}

/// Whether `pid` is alive *and* is the process that was recorded: a recycled
/// pid has a different start time and counts as dead (spec §Architecture 3).
/// Zombies count as dead: they hold no resources and a dead muzzle may sit
/// unreaped under a parent that never waits.
pub fn is_alive(pid: Pid, start: StartTime) -> bool {
    start_time(pid) == Some(start)
}

/// The live (non-zombie) members of process group `pgid` (D16). Empty when
/// the group has no members left.
pub fn group_members(pgid: Pid) -> io::Result<Vec<Pid>> {
    imp::group_members(pgid)
}

/// The parent chain of `pid`, nearest first, excluding `pid` itself and
/// stopping after pid 1 or when a parent can't be read (D17). Ancestry stops
/// at a container boundary; that's what `MUZZLE_HOLD` is for.
pub fn ancestors(pid: Pid) -> Vec<Ancestor> {
    let mut chain = Vec::new();
    let Some(mut cur) = imp::stat(pid) else {
        return chain;
    };
    // The bound only guards against a ppid loop read across a pid recycle.
    while cur.ppid > 0 && chain.len() < 4096 {
        let parent = cur.ppid;
        let Some(stat) = imp::stat(parent) else { break };
        chain.push(Ancestor {
            pid: parent,
            pgid: stat.pgid,
            start: stat.start,
        });
        if parent == 1 {
            break;
        }
        cur = stat;
    }
    chain
}

/// Every live process descended from one of `roots` (not the roots
/// themselves), with its start stamp, from one scan of the process table.
/// A nested run's `--run-timeout` signals these: its job shares the outer
/// claim's group, so the group can't be the target (D17, D18,
/// f-nested-timeout).
pub fn descendants(roots: &[Pid]) -> Vec<(Pid, StartTime)> {
    let table: Vec<(Pid, Stat)> = imp::all_pids()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| imp::stat(p).map(|s| (p, s)))
        .collect();
    let mut found: Vec<(Pid, StartTime)> = Vec::new();
    let mut frontier: Vec<Pid> = roots.to_vec();
    while let Some(parent) = frontier.pop() {
        for &(pid, s) in &table {
            if s.ppid == parent && !roots.contains(&pid) && !found.iter().any(|f| f.0 == pid) {
                found.push((pid, s.start));
                frontier.push(pid);
            }
        }
    }
    found
}

/// The controlling terminal, as seen by a muzzle that runs its job in a
/// group of its own (D16 non-leader case, f-tcsetpgrp). When muzzle's group
/// is the foreground, the job's group is lent the terminal for the run, or
/// a job that reads it stops on SIGTTIN and holds its slots forever; it is
/// given back when the job ends. Only ever given back if it was lent, so a
/// background muzzle never takes the terminal from the shell.
pub struct Terminal {
    tty: std::os::fd::OwnedFd,
    mine: Pid,
    lent: std::cell::Cell<bool>,
}

impl Terminal {
    /// `None` without a controlling terminal.
    pub fn open() -> Option<Terminal> {
        use std::os::fd::FromRawFd;
        // SAFETY: plain syscalls; the fd is owned from here on. CLOEXEC:
        // the job and a keeper must not inherit it.
        unsafe {
            let fd = libc::open(
                c"/dev/tty".as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            );
            if fd < 0 {
                return None;
            }
            Some(Terminal {
                tty: std::os::fd::OwnedFd::from_raw_fd(fd),
                mine: libc::getpgrp(),
                lent: std::cell::Cell::new(false),
            })
        }
    }

    /// The raw fd, for [`Terminal::take_in_child`].
    pub fn fd(&self) -> libc::c_int {
        use std::os::fd::AsRawFd;
        self.tty.as_raw_fd()
    }

    /// Whether muzzle's group is the terminal's foreground.
    pub fn ours(&self) -> bool {
        // SAFETY: plain syscall on an fd this struct owns.
        unsafe { libc::tcgetpgrp(self.fd()) == self.mine }
    }

    /// Make the calling process's group the foreground of `fd`. For a
    /// `pre_exec` hook after `process_group(0)`, so the job owns the
    /// terminal before it execs: only async-signal-safe calls.
    pub fn take_in_child(fd: libc::c_int) -> io::Result<()> {
        // SAFETY: plain syscalls on the fd the parent passed down.
        unsafe { to_fg(fd, libc::getpgrp()) }
    }

    /// Record that the job took the terminal in its `pre_exec`.
    pub fn mark_lent(&self) {
        self.lent.set(true);
    }

    /// Lend the terminal to `pgid` (resuming after `fg`).
    pub fn lend(&self, pgid: Pid) {
        // SAFETY: plain syscalls on an fd this struct owns.
        if unsafe { to_fg(self.fd(), pgid) }.is_ok() {
            self.lent.set(true);
        }
    }

    /// Give the terminal back to muzzle's group, if it was lent.
    pub fn give_back(&self) {
        if self.lent.replace(false) {
            // SAFETY: plain syscalls on an fd this struct owns.
            let _ = unsafe { to_fg(self.fd(), self.mine) };
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.give_back();
    }
}

/// `tcsetpgrp` with SIGTTOU blocked (POSIX: a blocked SIGTTOU lets a
/// background process change the foreground group).
unsafe fn to_fg(fd: libc::c_int, pgid: Pid) -> io::Result<()> {
    // SAFETY: the caller passes a valid fd; the sigsets live on the stack.
    unsafe {
        let mut block: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut block);
        libc::sigaddset(&mut block, libc::SIGTTOU);
        libc::pthread_sigmask(libc::SIG_BLOCK, &block, &mut old);
        let r = libc::tcsetpgrp(fd, pgid);
        let err = io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        if r == 0 { Ok(()) } else { Err(err) }
    }
}

/// Logical cores (D7). Linux: `available_parallelism`, which respects cgroup
/// CPU limits. macOS: `hw.logicalcpu`; E-cores count as full slots.
pub fn logical_cores() -> io::Result<usize> {
    imp::logical_cores()
}

/// 1, 5 and 15 minute load averages, from libc's `getloadavg` on both
/// platforms (bench probe, P12).
pub fn loadavg() -> Option<[f64; 3]> {
    let mut l = [0f64; 3];
    // SAFETY: getloadavg writes at most 3 doubles into `l`.
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    (n == 3).then_some(l)
}

/// Total RAM in bytes (D9). Linux: `/proc/meminfo` MemTotal. macOS:
/// `hw.memsize`.
pub fn total_ram() -> io::Result<u64> {
    imp::total_ram()
}

/// Resource use of a reaped child (D27, the non-scope path).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rusage {
    /// User plus system CPU seconds.
    pub cpu_s: f64,
    /// Peak resident set of the largest single process, in bytes. `wait4`
    /// reports the max over the child and the descendants it waited for, not
    /// their sum.
    pub max_rss_bytes: u64,
}

/// What a non-blocking [`poll_child`] found.
pub enum ChildState {
    Running,
    /// Stopped by this signal (Ctrl-Z, SIGTTIN). Reported once per stop.
    Stopped(libc::c_int),
    Exited(ExitStatus, Rusage),
}

/// [`wait_child`] without blocking, also reporting a stop (`WUNTRACED`):
/// a job stopped in its own group must not leave muzzle waiting on it
/// forever (f-tcsetpgrp).
pub fn poll_child(pid: Pid) -> io::Result<ChildState> {
    let mut status: libc::c_int = 0;
    // SAFETY: a zeroed rusage is a valid value of a plain C struct.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: both out-pointers are valid for the call's duration.
        let r = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG | libc::WUNTRACED, &mut ru) };
        if r == 0 {
            return Ok(ChildState::Running);
        }
        if r == pid {
            if libc::WIFSTOPPED(status) {
                return Ok(ChildState::Stopped(libc::WSTOPSIG(status)));
            }
            return Ok(ChildState::Exited(
                ExitStatus::from_raw(status),
                rusage(&ru),
            ));
        }
        let err = io::Error::last_os_error();
        if r < 0 && err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

fn rusage(ru: &libc::rusage) -> Rusage {
    let secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    let maxrss = u64::try_from(ru.ru_maxrss).unwrap_or(0);
    Rusage {
        cpu_s: secs(ru.ru_utime) + secs(ru.ru_stime),
        max_rss_bytes: maxrss * imp::MAXRSS_UNIT,
    }
}

/// Reap `pid` (a child of this process) with `wait4`, returning its status
/// and rusage. Use this instead of `Child::wait`, which throws the rusage
/// away. The rusage covers the child plus the descendants it reaped; a
/// background process the child left behind isn't in it (D16: those runs get
/// `null` figures on macOS).
pub fn wait_child(pid: Pid) -> io::Result<(ExitStatus, Rusage)> {
    let mut status: libc::c_int = 0;
    // SAFETY: a zeroed rusage is a valid value of a plain C struct.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: both out-pointers are valid for the call's duration.
        let r = unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
        if r == pid {
            break;
        }
        let err = io::Error::last_os_error();
        if r < 0 && err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
    Ok((ExitStatus::from_raw(status), rusage(&ru)))
}

/// Whether a systemd user manager can be reached (D15, §Systemd scope on
/// Linux). Probed once per process with `systemctl --user
/// is-system-running`, then cached. Always false on macOS.
pub fn systemd_user_reachable() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(imp::probe_systemd_user)
}

/// Whether a pid the kernel listed in group `pgid` counts as a member, given
/// the result of reading it (`Err(errno)` when unreadable). EPERM means the
/// process exists but belongs to another user (`kill(pid, 0) == EPERM` reads
/// the same way), so it counts: dropping it would let D16 release a claim
/// while a `sudo` in the group still runs. Any other error means gone.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // Linux /proc is world-readable
fn listed_member(read: Result<Stat, i32>, pgid: Pid) -> bool {
    match read {
        Ok(s) => s.pgid == pgid,
        Err(errno) => errno == libc::EPERM,
    }
}

/// Whether `systemctl --user is-system-running` output means the manager
/// answered. `degraded` exits non-zero but the manager still runs scopes, so
/// the state text decides, not the exit code.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only Linux probes
fn manager_answered(state: &str) -> bool {
    matches!(
        state.trim(),
        "running" | "degraded" | "starting" | "initializing" | "maintenance"
    )
}

/// The cgroup (v2) directory `pid` lives in, under `/sys/fs/cgroup`. On
/// Linux a scope's figures are read from here while the scope is alive.
/// Always `None` on macOS.
pub fn cgroup_of(pid: Pid) -> Option<std::path::PathBuf> {
    imp::cgroup_of(pid)
}

/// The scope's figures (D27, D30a) read from a cgroup v2 directory:
/// `cpu.stat` usage, `memory.peak`, and `memory.events` oom kills. Read them
/// before the scope empties: systemd removes the cgroup with it. Always
/// `None` on macOS.
pub fn cgroup_figures(dir: &std::path::Path) -> Option<CgroupFigures> {
    imp::cgroup_figures(dir)
}

/// The inode of this process's `kind` namespace (`cgroup`, `pid`), which
/// identifies it: two processes in one namespace read the same number.
/// `None` on macOS.
pub fn namespace(kind: &str) -> Option<u64> {
    imp::namespace(kind)
}

/// D16a: whether the cgroup at `rel` (under `/sys/fs/cgroup`, as this
/// process's cgroup namespace sees it) has processes, from `cgroup.events`,
/// which counts descendants too. A missing directory is `Some(false)`:
/// systemd removes an emptied scope. `None` when unanswerable, and always
/// on macOS.
pub fn cgroup_populated(rel: &str) -> Option<bool> {
    imp::cgroup_populated(rel)
}

/// D16a: whether any process visible in `/proc` sits in a cgroup with the
/// path segment `leaf` (a unique `….scope` name). Works across a cgroup
/// namespace boundary, where paths read `/../../app.slice/x.scope`, as long
/// as the pid namespace is shared (distrobox, toolbox). `None` when
/// unanswerable, and always on macOS.
pub fn cgroup_has_process(leaf: &str) -> Option<bool> {
    imp::cgroup_has_process(leaf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    fn spawn(script: &str) -> Child {
        Command::new("sh").args(["-c", script]).spawn().unwrap()
    }

    /// Spawn and hand back only the pid, for tests that reap through
    /// `wait_child` (wait4) rather than `Child::wait`.
    // Reaped by `wait_child`, which clippy can't see.
    #[allow(clippy::zombie_processes)]
    fn spawn_pid(script: &str) -> Pid {
        pid_of(&spawn(script))
    }

    fn pid_of(child: &Child) -> Pid {
        child.id() as Pid
    }

    fn kill(pid: Pid, sig: libc::c_int) {
        // SAFETY: plain syscall; negative pid targets a group.
        unsafe { libc::kill(pid, sig) };
    }

    /// Poll until `f` holds or 5 s pass; process start-up and teardown are
    /// asynchronous.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        f()
    }

    #[test]
    fn start_time_is_stable_and_none_once_reaped() {
        let mut child = spawn("sleep 30");
        let pid = pid_of(&child);
        let start = start_time(pid).expect("live child has a start time");
        assert_eq!(start_time(pid), Some(start));
        kill(pid, libc::SIGKILL);
        child.wait().unwrap();
        assert_eq!(start_time(pid), None);
    }

    #[test]
    fn is_alive_tracks_a_real_child() {
        let mut child = spawn("sleep 30");
        let pid = pid_of(&child);
        let start = start_time(pid).unwrap();
        assert!(is_alive(pid, start));
        kill(pid, libc::SIGKILL);
        child.wait().unwrap();
        assert!(!is_alive(pid, start));
    }

    #[test]
    fn recycled_pid_counts_as_dead() {
        // Same pid, different start time: what a recycled pid looks like.
        let mut child = spawn("sleep 30");
        let pid = pid_of(&child);
        let start = start_time(pid).unwrap();
        assert!(!is_alive(pid, start + 1));
        assert!(!is_alive(pid, start.wrapping_sub(1)));
        kill(pid, libc::SIGKILL);
        child.wait().unwrap();
    }

    #[test]
    fn zombie_counts_as_dead() {
        let mut child = spawn("sleep 30");
        let pid = pid_of(&child);
        let start = start_time(pid).unwrap();
        kill(pid, libc::SIGKILL);
        // Not reaped yet: the pid is a zombie until `wait`.
        assert!(eventually(|| !is_alive(pid, start)));
        child.wait().unwrap();
    }

    #[test]
    fn group_members_sees_background_children_and_empties_on_group_kill() {
        // A new group led by the child, with a backgrounded grandchild: the
        // D16 shape `(gate > log &)`.
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & echo $! ; wait"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = pid_of(&child);
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let grandchild: Pid = line.trim().parse().unwrap();

        assert!(eventually(|| {
            let m = group_members(pgid).unwrap();
            m.contains(&pgid) && m.contains(&grandchild)
        }));
        assert!(
            !group_members(pgid)
                .unwrap()
                .contains(&(std::process::id() as Pid))
        );
        assert_eq!(pgid_of_checked(grandchild), pgid);

        kill(-pgid, libc::SIGKILL);
        child.wait().unwrap();
        // The grandchild is reparented and reaped by init or a subreaper.
        assert!(eventually(|| group_members(pgid).unwrap().is_empty()));
    }

    fn pgid_of_checked(pid: Pid) -> Pid {
        pgid(pid).expect("live process has a pgid")
    }

    #[test]
    fn group_members_of_unused_group_is_empty() {
        let mut child = spawn("exit 0");
        let pid = pid_of(&child);
        child.wait().unwrap();
        assert!(group_members(pid).unwrap().is_empty());
    }

    #[test]
    fn ancestors_walks_up_through_this_process() {
        let me = std::process::id() as Pid;
        // sh → sleep: the sleep's chain is sh, then this test process.
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & echo $! ; wait"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let sh = pid_of(&child);
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let sleep: Pid = line.trim().parse().unwrap();

        let chain = ancestors(sleep);
        let pids: Vec<Pid> = chain.iter().map(|a| a.pid).collect();
        assert_eq!(&pids[..2], &[sh, me], "chain was {pids:?}");
        assert_eq!(chain[1].start, start_time(me).unwrap());
        assert_eq!(chain[1].pgid, pgid(me).unwrap());
        assert!(pids.iter().all(|&p| p > 0));

        kill(sleep, libc::SIGKILL);
        kill(sh, libc::SIGKILL);
        child.wait().unwrap();
        assert!(eventually(|| ancestors(sleep).is_empty()));
    }

    #[test]
    fn cores_and_ram_are_plausible() {
        let cores = logical_cores().unwrap();
        assert!((1..=4096).contains(&cores));
        let ram = total_ram().unwrap();
        // More than 256 MiB, less than 64 TiB.
        assert!(ram > 256 << 20 && ram < 64 << 40, "ram = {ram}");
    }

    #[test]
    fn wait_child_reports_status_and_cpu() {
        // ~0.1 s or more of shell arithmetic on any machine.
        let pid = spawn_pid("i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done; exit 7");
        let (status, ru) = wait_child(pid).unwrap();
        assert_eq!(status.code(), Some(7));
        assert!(ru.cpu_s > 0.0, "cpu_s = {}", ru.cpu_s);
        assert!(ru.max_rss_bytes > 100 << 10, "rss = {}", ru.max_rss_bytes);
    }

    #[test]
    fn wait_child_reports_signal() {
        let pid = spawn_pid("sleep 30");
        kill(pid, libc::SIGTERM);
        let (status, _) = wait_child(pid).unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn wait_child_of_non_child_errors() {
        assert!(wait_child(1).is_err());
    }

    #[test]
    fn systemd_probe_is_cached() {
        let first = systemd_user_reachable();
        assert_eq!(systemd_user_reachable(), first);
        if cfg!(target_os = "macos") {
            assert!(!first);
        }
    }

    #[test]
    fn listed_member_counts_eperm_as_alive() {
        let s = Stat {
            ppid: 1,
            pgid: 9,
            start: 5,
        };
        assert!(listed_member(Ok(s), 9));
        assert!(!listed_member(Ok(s), 8));
        // Another user's process (a setuid sudo) in the listed group.
        assert!(listed_member(Err(libc::EPERM), 9));
        assert!(!listed_member(Err(libc::ESRCH), 9));
        assert!(!listed_member(Err(0), 9));
    }

    #[test]
    fn group_members_of_pgid_zero_or_negative_is_empty() {
        // Linux kernel threads have pgid 0; a default-initialised holder
        // record must not see them as a live group (D16).
        assert!(group_members(0).unwrap().is_empty());
        assert!(group_members(-1).unwrap().is_empty());
    }

    #[test]
    fn manager_state_text_decides_reachability() {
        assert!(manager_answered("running\n"));
        assert!(manager_answered("degraded\n"));
        assert!(!manager_answered("offline\n"));
        assert!(!manager_answered(""));
    }
}
