//! D16 keeper: the handoff of a live group's claim.

use std::time::Instant;

use anyhow::bail;

use super::job::{Ending, Job};
use super::scope::{Sampler, mb, scope_outcome};
use super::spawn::Target;
use super::{Heartbeat, POLL, now_ms};
use crate::proc::{self, Pid};
use crate::state::Killed;

impl Job<'_> {
    /// D16 keeper: fork a detached process that takes over the holder entry
    /// and releases it once the job's group is empty. Returns in the parent
    /// once the keeper owns the entry, so the caller gets the top command's
    /// status at once.
    pub(super) fn hand_off(
        &self,
        target: Target,
        sampler: Option<Sampler>,
        unit: String,
        started: Instant,
        ending: &Ending,
    ) -> anyhow::Result<()> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: plain syscalls on fds this function owns. muzzle runs no
        // other threads, so the forked child may run ordinary Rust.
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                bail!("pipe: {}", std::io::Error::last_os_error());
            }
            match libc::fork() {
                -1 => bail!("fork: {}", std::io::Error::last_os_error()),
                0 => {
                    libc::close(fds[0]);
                    libc::setsid();
                    detach_stdio();
                    // Out of the caller's session, terminal signals don't
                    // apply; SIGTERM kills it, and a dead keeper is pruned
                    // like any holder (D16).
                    for sig in [libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
                        libc::signal(sig, libc::SIG_IGN);
                    }
                    libc::signal(libc::SIGTERM, libc::SIG_DFL);
                    self.keep(target, sampler, &unit, started, ending, fds[1]);
                    libc::_exit(0);
                }
                _ => {
                    libc::close(fds[1]);
                    // One byte once the keeper owns the entry, EOF if it
                    // died first. Either way the entry stays safe: a dead
                    // muzzle's live group holds its slots (admit).
                    let mut b = 0u8;
                    while libc::read(fds[0], (&mut b as *mut u8).cast(), 1) < 0
                        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                    {
                    }
                    libc::close(fds[0]);
                }
            }
        }
        Ok(())
    }

    /// The keeper's body. Writes one byte to `ready` once it holds the
    /// entry, then waits for the group to empty.
    fn keep(
        &self,
        target: Target,
        mut sampler: Option<Sampler>,
        unit: &str,
        started: Instant,
        ending: &Ending,
        ready: libc::c_int,
    ) {
        let kpid = std::process::id();
        let Some(kstart) = proc::start_time(kpid as Pid) else {
            return;
        };
        let owned = self.store.update(now_ms(), |s| {
            Ok(s.holder_mut(self.ticket).map(|h| {
                h.pid = kpid;
                h.start = kstart;
                h.kept = true;
            }))
        });
        // SAFETY: writing one byte to a pipe this process owns.
        unsafe {
            libc::write(ready, b"k".as_ptr().cast(), 1);
            libc::close(ready);
        }
        if !matches!(owned, Ok(Some(()))) {
            return;
        }
        let mut beat = Heartbeat::new(self.store, self.ticket);
        loop {
            beat.beat();
            let others = target.others();
            match others.first() {
                Some(&member) => {
                    if let Some(s) = sampler.as_mut() {
                        // The top command is gone; find the scope through
                        // any member.
                        s.sample(member);
                    }
                }
                None if self.scope_busy() => {}
                None => break,
            }
            std::thread::sleep(POLL);
        }
        let ran_s = started.elapsed().as_secs_f64();
        let mut ending = *ending;
        // D16: figures for a backgrounded run come only from the scope;
        // without one (macOS, advisory Linux) they are null.
        let (mut cpu_s, mut peak) = (None, None);
        if let Some(s) = &sampler {
            let show = scope_outcome(unit);
            if let Some(f) = s.last {
                cpu_s = Some(f.cpu_s.max(show.cpu_s.unwrap_or(0.0)));
                peak = f.mem_peak_bytes.or(show.peak).map(mb);
            }
            if show.result == "oom-kill" || s.last.is_some_and(|f| f.oom_kills > 0) {
                ending.killed = Some(Killed::Oom);
            }
        }
        let _ = self.release(ran_s, &ending, cpu_s, peak, true);
    }
}

/// The keeper keeps no stdio: a caller reading muzzle's output (pi, a
/// test) would otherwise wait for EOF until the keeper exits.
pub(super) fn detach_stdio() {
    // SAFETY: plain syscalls on the standard fds.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
    }
}
