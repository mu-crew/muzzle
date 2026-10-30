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

/// D18: the job is past `--run-timeout`: SIGTERM, and SIGKILL after 10 s.
pub(super) struct Deadline {
    pub(super) after: Option<Duration>,
    pub(super) termed: Option<Instant>,
    pub(super) killed: bool,
    /// A nested run's descendants (pid, start), tracked from the SIGTERM on.
    /// Remembered rather than re-read, because once the top command dies
    /// they are reparented and ancestry no longer finds them; the start
    /// stamp keeps a recycled pid from being signalled (f-nested-timeout).
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
            tree: Vec::new(),
            scanned: None,
        }
    }

    fn check(&mut self, started: Instant, target: Target) {
        let Some(after) = self.after else { return };
        match self.termed {
            None if started.elapsed() >= after => {
                self.hit(target, libc::SIGTERM);
                self.termed = Some(Instant::now());
            }
            Some(at) if !self.killed && at.elapsed() >= KILL_GRACE => {
                self.hit(target, libc::SIGKILL);
                self.killed = true;
            }
            // During the grace, so what the tree forks is still found.
            Some(_) => self.track(target),
            _ => {}
        }
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
        self.tree.retain(|&(p, s)| proc::is_alive(p, s));
        if self.scanned.is_some_and(|t| t.elapsed() < POLL) {
            return;
        }
        self.scanned = Some(Instant::now());
        let mut roots: Vec<Pid> = self.tree.iter().map(|t| t.0).collect();
        roots.push(child);
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
/// sampling the scope, and beating for the holder if there is one. With
/// `tty`, a job in its own group that stops is relayed to the shell (see
/// [`relay_stop`]). Returns its status and rusage once reaped.
pub(super) fn supervise(
    pid: Pid,
    target: Target,
    deadline: &mut Deadline,
    mut sampler: Option<&mut Sampler>,
    mut beat: Option<&mut Heartbeat>,
    tty: Option<&proc::Terminal>,
) -> anyhow::Result<(ExitStatus, proc::Rusage)> {
    let started = Instant::now();
    // A job left stopped waiting for the terminal, resumed once it's ours.
    let mut parked = false;
    loop {
        match proc::poll_child(pid).context("waiting for the command")? {
            proc::ChildState::Exited(status, ru) => return Ok((status, ru)),
            proc::ChildState::Stopped(sig) => {
                if let (Target::Group(pgid), Some(t)) = (target, tty) {
                    parked = !relay_stop(pgid, t, sig);
                }
            }
            proc::ChildState::Running => {}
        }
        if let (true, Target::Group(pgid), Some(t)) = (parked, target, tty)
            && t.ours()
        {
            // `fg` after `bg`: the shell gave muzzle the terminal back.
            t.lend(pgid);
            // SAFETY: plain syscall.
            unsafe { libc::kill(-pgid, libc::SIGCONT) };
            parked = false;
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

/// f-tcsetpgrp: the job, in its own group (D16 non-leader), was stopped
/// (Ctrl-Z, or SIGTTIN reading a terminal it doesn't own). The shell only
/// sees muzzle, so muzzle gives the terminal back and stops too, and the
/// shell reports the job stopped. On `fg` or `bg` the shell continues
/// muzzle, which lends the terminal again if it got it back (`fg`) and
/// continues the job. Without this muzzle waited on a stopped job forever,
/// holding its slots and the shell. Returns whether the job was continued.
fn relay_stop(pgid: Pid, tty: &proc::Terminal, sig: libc::c_int) -> bool {
    tty.give_back();
    // SAFETY: plain syscall. SIGTSTP keeps its default action (muzzle
    // never catches it), so this returns once muzzle is continued, or at
    // once if the kernel discards it for an orphaned group.
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
    if tty.ours() {
        tty.lend(pgid);
    } else if matches!(sig, libc::SIGTTIN | libc::SIGTTOU) {
        // Still in the background (`bg`, or an orphaned group where the
        // stop was discarded): continuing a job that waits for the terminal
        // only stops it again, so it stays stopped, as a background reader
        // does in a plain shell, until the terminal comes back.
        return false;
    }
    // SAFETY: plain syscall.
    unsafe {
        libc::kill(-pgid, libc::SIGCONT);
    }
    true
}

/// D17: a nested run starts at once, claims nothing and is not logged; its
/// CPU counts in the outer run. It keeps the outer claim's environment.
pub(super) fn run_nested(
    args: &RunArgs,
    run_timeout: Option<Duration>,
) -> anyhow::Result<ExitCode> {
    let started = Instant::now();
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
    let (status, _) = supervise(pid, target, &mut deadline, None, None, None)?;
    // D18's grace applies to the nested job's leftovers as to a group.
    deadline.drain(target, None, None);
    let code = if deadline.fired() {
        exit::RUN_TIMEOUT
    } else {
        exit_code(status)
    };
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
