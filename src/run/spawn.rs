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
use super::{KILL_GRACE, TICK, take_signal};
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
    /// A nested run: only the child, never the outer claim's group.
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
}

impl Deadline {
    fn check(&mut self, started: Instant, target: Target) {
        let Some(after) = self.after else { return };
        match self.termed {
            None if started.elapsed() >= after => {
                target.signal(libc::SIGTERM);
                self.termed = Some(Instant::now());
            }
            Some(at) if !self.killed && at.elapsed() >= KILL_GRACE => {
                target.signal(libc::SIGKILL);
                self.killed = true;
            }
            _ => {}
        }
    }

    pub(super) fn fired(&self) -> bool {
        self.termed.is_some()
    }

    /// D18: the top command is gone, but what it left behind gets the rest
    /// of `KILL_GRACE` from the SIGTERM before the SIGKILL. A `bash -c` top
    /// dies on TERM at once, and its children's TERM handlers (a gate
    /// cleaning up worktrees) would otherwise have no time at all.
    pub(super) fn drain(&mut self, target: Target, mut sampler: Option<&mut Sampler>) {
        let Some(termed) = self.termed else { return };
        loop {
            let others = target.others();
            let Some(&member) = others.first() else {
                return;
            };
            if termed.elapsed() >= KILL_GRACE {
                target.signal(libc::SIGKILL);
                self.killed = true;
                return;
            }
            if let Some(sig) = take_signal() {
                target.forward(sig);
            }
            if let Some(s) = sampler.as_deref_mut() {
                s.sample(member);
            }
            std::thread::sleep(TICK);
        }
    }
}

/// Wait for the top command, forwarding signals, enforcing the deadline and
/// sampling the scope. Returns its status and rusage once reaped.
pub(super) fn supervise(
    pid: Pid,
    target: Target,
    deadline: &mut Deadline,
    mut sampler: Option<&mut Sampler>,
) -> anyhow::Result<(ExitStatus, proc::Rusage)> {
    let started = Instant::now();
    while !exited(pid) {
        if let Some(sig) = take_signal() {
            target.forward(sig);
        }
        deadline.check(started, target);
        if let Some(s) = sampler.as_deref_mut() {
            s.sample(pid);
        }
        std::thread::sleep(TICK);
    }
    proc::wait_child(pid).context("waiting for the command")
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
    let mut deadline = Deadline {
        after: run_timeout,
        termed: None,
        killed: false,
    };
    let (status, _) = supervise(pid, Target::Child(pid), &mut deadline, None)?;
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
        write_report(
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
        )?;
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
