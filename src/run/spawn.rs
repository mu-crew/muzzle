//! Spawn and process groups: signal targets, the run deadline, the
//! nested run, program lookup and exit codes.

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::Context;

use super::report::{Times, repo_str, write_report};
use super::scope::Sampler;
use super::{Heartbeat, KILL_GRACE, POLL, TICK, take_signal};
use crate::cli::RunArgs;
use crate::exit;
use crate::proc::{self, Pid};

/// Who gets a signal meant for the job.
#[derive(Clone, Copy)]
pub(super) enum Target {
    /// The job leads its own group.
    Group(Pid),
    /// The job shares muzzle's group (D16 leader case): every member but
    /// muzzle itself.
    GroupButMe { pgid: Pid, me: Pid },
    /// A nested run: only the child, never the outer claim's group. Its
    /// descendants share that group, so on `--run-timeout` [`Deadline`]
    /// reaches them through ancestry instead (D18, f-nested-timeout).
    Child(Pid),
}

impl Target {
    fn signal(self, sig: libc::c_int) {
        // SAFETY: plain syscalls.
        unsafe {
            match self {
                Target::Group(pgid) => {
                    libc::kill(-pgid, sig);
                }
                Target::GroupButMe { pgid, me } => {
                    for pid in proc::group_members(pgid).unwrap_or_default() {
                        if pid != me {
                            libc::kill(pid, sig);
                        }
                    }
                }
                Target::Child(pid) => {
                    libc::kill(pid, sig);
                }
            }
        }
    }

    /// §Architecture 4.4: forward a signal muzzle caught. In the leader
    /// case SIGINT and SIGQUIT are not forwarded: they come from the
    /// terminal, which already sent them to the whole group, and a second
    /// SIGINT means "force quit" to cargo and friends.
    fn forward(self, sig: libc::c_int) {
        let tty = matches!(sig, libc::SIGINT | libc::SIGQUIT);
        if !(tty && matches!(self, Target::GroupButMe { .. })) {
            self.signal(sig);
        }
    }

    /// Live members other than muzzle; empty for a nested child.
    pub(super) fn others(self) -> Vec<Pid> {
        match self {
            Target::Group(pgid) => proc::group_members(pgid).unwrap_or_default(),
            Target::GroupButMe { pgid, me } => proc::group_members(pgid)
                .unwrap_or_default()
                .into_iter()
                .filter(|&p| p != me)
                .collect(),
            Target::Child(_) => Vec::new(),
        }
    }
}

/// Non-blocking: the child has exited. `proc` reads a zombie as gone, so
/// this leaves the zombie for [`proc::wait_child`], which collects the
/// rusage `Child::wait` would throw away.
pub(super) fn exited(pid: Pid) -> bool {
    proc::start_time(pid).is_none()
}

pub(super) fn exit_code(status: ExitStatus) -> u8 {
    match (status.code(), status.signal()) {
        (Some(c), _) => c as u8,
        (None, Some(sig)) => 128u8.saturating_add(sig as u8),
        (None, None) => exit::INTERNAL,
    }
}

/// Why the [`Deadline`] ended the job.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Fired {
    /// D18: `--run-timeout`.
    Timeout,
    /// D16: a non-leader's job stopped for the terminal (SIGTTIN or
    /// SIGTTOU); see [`terminal_stop`].
    Terminal(libc::c_int),
}

impl Fired {
    /// muzzle's exit: 124 for the timeout (D18); for the terminal, 128 +
    /// the stop signal, the code a shell gives a job stopped that way.
    pub(super) fn code(self) -> u8 {
        match self {
            Fired::Timeout => exit::RUN_TIMEOUT,
            Fired::Terminal(sig) => 128u8.saturating_add(sig as u8),
        }
    }
}

/// D18: the job is past `--run-timeout`, or stuck waiting for the terminal
/// (D16): SIGTERM, and SIGKILL after 10 s.
pub(super) struct Deadline {
    pub(super) after: Option<Duration>,
    pub(super) termed: Option<Instant>,
    pub(super) killed: bool,
    pub(super) why: Option<Fired>,
    /// A nested run's descendants (pid, start), tracked from the SIGTERM on.
    /// Remembered rather than re-read, because once the top command dies
    /// they are reparented and ancestry no longer finds them; the start
    /// stamp keeps a recycled pid from being signalled (f-nested-timeout).
    /// Those orphaned before the SIGTERM are found because a nested run
    /// with a timeout adopts orphans on Linux ([`proc::adopt_orphans`]).
    pub(super) tree: Vec<(Pid, proc::StartTime)>,
    /// When the tree was last rescanned: a scan reads every process on the
    /// host, so it runs every `POLL`, not every `TICK`, plus right before
    /// each signal.
    scanned: Option<Instant>,
}

impl Deadline {
    pub(super) fn new(after: Option<Duration>) -> Deadline {
        Deadline {
            after,
            termed: None,
            killed: false,
            why: None,
            tree: Vec::new(),
            scanned: None,
        }
    }

    fn check(&mut self, started: Instant, target: Target) {
        let due = self.after.is_some_and(|after| started.elapsed() >= after);
        match self.termed {
            None if due => self.fire(target, Fired::Timeout),
            Some(at) if !self.killed && at.elapsed() >= KILL_GRACE => {
                self.hit(target, libc::SIGKILL);
                self.killed = true;
            }
            // During the grace, so what the tree forks is still found.
            Some(_) => self.track(target),
            _ => {}
        }
    }

    /// Start ending the job now: SIGTERM, and the grace runs from here.
    fn fire(&mut self, target: Target, why: Fired) {
        self.why = Some(why);
        self.hit(target, libc::SIGTERM);
        // A stopped process keeps a SIGTERM pending until it is continued,
        // so it would only see the SIGKILL; continue it as a shell's `kill`
        // does for a stopped job.
        self.hit(target, libc::SIGCONT);
        self.termed = Some(Instant::now());
    }

    /// The deadline's signal: the target, and for a nested run its tracked
    /// descendants too.
    fn hit(&mut self, target: Target, sig: libc::c_int) {
        self.scanned = None;
        self.track(target);
        for &(pid, _) in &self.tree {
            // SAFETY: plain syscall; `track` just checked the start stamp.
            unsafe { libc::kill(pid, sig) };
        }
        target.signal(sig);
    }

    /// Drop the dead from a nested run's tree and add what the child and
    /// the tree have forked since. A no-op for a group target.
    fn track(&mut self, target: Target) {
        let Target::Child(child) = target else { return };
        proc::reap_adopted(child);
        self.tree.retain(|&(p, s)| proc::is_alive(p, s));
        if self.scanned.is_some_and(|t| t.elapsed() < POLL) {
            return;
        }
        self.scanned = Some(Instant::now());
        let mut roots: Vec<Pid> = self.tree.iter().map(|t| t.0).collect();
        // muzzle too: what it adopted (`adopt_orphans`) hangs off it, not
        // off the child.
        roots.extend([child, std::process::id() as Pid]);
        for d in proc::descendants(&roots) {
            if !self.tree.iter().any(|t| t.0 == d.0) {
                self.tree.push(d);
            }
        }
    }

    /// What the deadline still waits for once the top command is gone.
    fn remaining(&mut self, target: Target) -> Vec<Pid> {
        match target {
            Target::Child(_) => {
                self.track(target);
                self.tree.iter().map(|t| t.0).collect()
            }
            _ => target.others(),
        }
    }

    pub(super) fn fired(&self) -> bool {
        self.termed.is_some()
    }

    /// D18: the top command is gone, but what it left behind gets the rest
    /// of `KILL_GRACE` from the SIGTERM before the SIGKILL. A `bash -c` top
    /// dies on TERM at once, and its children's TERM handlers (a gate
    /// cleaning up worktrees) would otherwise have no time at all.
    pub(super) fn drain(
        &mut self,
        target: Target,
        mut sampler: Option<&mut Sampler>,
        mut beat: Option<&mut Heartbeat>,
    ) {
        let Some(termed) = self.termed else { return };
        loop {
            let others = self.remaining(target);
            let Some(&member) = others.first() else {
                return;
            };
            if termed.elapsed() >= KILL_GRACE {
                self.hit(target, libc::SIGKILL);
                self.killed = true;
                return;
            }
            if let Some(sig) = take_signal() {
                target.forward(sig);
            }
            if let Some(s) = sampler.as_deref_mut() {
                s.sample(member);
            }
            if let Some(b) = beat.as_deref_mut() {
                b.beat();
            }
            std::thread::sleep(TICK);
        }
    }
}

/// Wait for the top command, forwarding signals, enforcing the deadline and
/// sampling the scope, and beating for the holder if there is one. Returns
/// its status and rusage once reaped.
pub(super) fn supervise(
    pid: Pid,
    target: Target,
    deadline: &mut Deadline,
    mut sampler: Option<&mut Sampler>,
    mut beat: Option<&mut Heartbeat>,
) -> anyhow::Result<(ExitStatus, proc::Rusage)> {
    let started = Instant::now();
    loop {
        match proc::poll_child(pid).context("waiting for the command")? {
            proc::ChildState::Exited(status, ru) => return Ok((status, ru)),
            proc::ChildState::Stopped(sig) => {
                if let Some(why) = terminal_stop(target, sig, deadline) {
                    deadline.fire(target, why);
                }
            }
            proc::ChildState::Running => {}
        }
        if let Target::Child(child) = target {
            proc::reap_adopted(child);
        }
        if let Some(sig) = take_signal() {
            target.forward(sig);
        }
        deadline.check(started, target);
        if let Some(s) = sampler.as_deref_mut() {
            s.sample(pid);
        }
        if let Some(b) = beat.as_deref_mut() {
            b.beat();
        }
        std::thread::sleep(TICK);
    }
}

/// D16, f-tcsetpgrp: a non-leader runs its job in a group of its own,
/// which is never the terminal's foreground: muzzle does not lend it the
/// terminal. Handing it over and taking it back stopped callers that share
/// muzzle's group, took the terminal from a shell that had reclaimed it, and
/// sent Ctrl-C and Ctrl-Z to the job instead of the caller (review of
/// a0e59fd). A job that reads (SIGTTIN) or sets up (SIGTTOU) the terminal
/// therefore stops, and used to wait so forever, holding its slots. It
/// can't go on without the terminal, so muzzle says why once and ends it:
/// SIGTERM, then SIGKILL after D18's grace, exit 128 + the stop signal,
/// what a shell reports for the stopped job. A plain SIGSTOP is left alone,
/// as at 0036aa7: whoever sent it can continue the job. Ctrl-Z never lands
/// here: it goes to the foreground group, the caller's, not the job's.
fn terminal_stop(target: Target, sig: libc::c_int, deadline: &Deadline) -> Option<Fired> {
    let ttin = matches!(sig, libc::SIGTTIN | libc::SIGTTOU);
    if !ttin || !matches!(target, Target::Group(_)) || deadline.why.is_some() {
        return None;
    }
    eprintln!(
        "muzzle: the command stopped waiting for the terminal: muzzle doesn't lead its \
         process group, so the command runs in the background (D16). Run muzzle as \
         the first command, or give it no terminal (< /dev/null). Ending it."
    );
    Some(Fired::Terminal(sig))
}

/// D17: a nested run starts at once, claims nothing and is not logged; its
/// CPU counts in the outer run. It keeps the outer claim's environment.
pub(super) fn run_nested(
    args: &RunArgs,
    run_timeout: Option<Duration>,
) -> anyhow::Result<ExitCode> {
    let started = Instant::now();
    if run_timeout.is_some() {
        // A descendant orphaned before the deadline (`(cmd &)`) reparents
        // to init and ancestry loses it, so the timeout missed it
        // (low_nested_run_timeout_misses). As a subreaper muzzle adopts it
        // instead. Linux only; on macOS it still escapes this timeout,
        // though not the outer claim's, whose group it stays in (D18 note).
        proc::adopt_orphans();
    }
    let child = match Command::new(&args.command[0])
        .args(&args.command[1..])
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Ok(ExitCode::from(spawn_failed(&args.command[0], &e))),
    };
    let pid = child.id() as Pid;
    let target = Target::Child(pid);
    let mut deadline = Deadline::new(run_timeout);
    let (status, _) = supervise(pid, target, &mut deadline, None, None)?;
    // D18's grace applies to the nested job's leftovers as to a group.
    deadline.drain(target, None, None);
    proc::reap_adopted(pid);
    let code = deadline.why.map_or(exit_code(status), Fired::code);
    if let Some(path) = &args.report {
        let ran = started.elapsed().as_secs_f64();
        let t = Times {
            waited_s: 0.0,
            ran_s: ran,
            admitted_ms: None,
            released_ms: None,
        };
        let repo = repo_str(std::env::current_dir().ok().as_deref());
        if let Err(e) = write_report(
            path,
            &t,
            None,
            None,
            None,
            repo.as_deref(),
            code,
            false,
            None,
            false,
        ) {
            // As for an admitted run: the command's code stands.
            eprintln!("muzzle: {e:#}");
        }
    }
    Ok(ExitCode::from(code))
}

/// The program `CMD` names, looked up on `PATH` as exec would. Checked
/// before queueing, so a typo fails at once rather than after the wait, and
/// so the scope path reports it the same way: systemd-run exits 1 for a
/// missing program.
pub(super) fn find_program(name: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let runnable = |p: &Path| -> std::io::Result<()> {
        let meta = fs::metadata(p)?;
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
            Ok(())
        } else {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }
    };
    if name.contains('/') {
        let p = PathBuf::from(name);
        return runnable(&p).map(|()| p);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut denied = None;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        match runnable(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = Some(e),
            Err(_) => {}
        }
    }
    Err(denied.unwrap_or_else(|| std::io::ErrorKind::NotFound.into()))
}

/// `env`'s convention: 127 when the command isn't found, 126 when it can't
/// be run. 125 stays "muzzle itself failed".
pub(super) fn spawn_failed(program: &str, e: &std::io::Error) -> u8 {
    eprintln!("muzzle: {program}: {e}");
    match e.kind() {
        std::io::ErrorKind::NotFound => 127,
        _ => 126,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_pass_through_and_signals_add_128() {
        assert_eq!(exit_code(ExitStatus::from_raw(7 << 8)), 7);
        assert_eq!(exit_code(ExitStatus::from_raw(libc::SIGTERM)), 143);
        assert_eq!(exit_code(ExitStatus::from_raw(libc::SIGKILL)), 137);
    }
}
