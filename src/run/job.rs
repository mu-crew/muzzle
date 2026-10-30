//! One admitted run, from spawn to release.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Command;
use std::time::{Duration, Instant};

use super::scope::{
    Sampler, cpu_weight, mb, scope_dir, scope_outcome, systemd_run_expands_env, unit_name,
};
use super::spawn::{Deadline, Fired, Target, exit_code, spawn_failed, supervise};
use super::{Heartbeat, now_ms, scope_populated};
use crate::cli::RunArgs;
use crate::proc::{self, Pid, StartTime};
use crate::state::{Budget, EnforcedReason, Holder, Killed, ScopeProbe, Store};

/// One admitted run, from spawn to release.
pub(super) struct Job<'a> {
    pub(super) store: &'a Store,
    pub(super) ticket: u64,
    pub(super) me: u32,
    pub(super) my_start: StartTime,
    pub(super) holder: Holder,
    pub(super) budget: Budget,
    pub(super) leader: bool,
    /// Set once the command is spawned. From then on only [`Job::release`]
    /// or a pruning waiter, judging liveness, may free the entry: a keeper
    /// may own it, or the job may still run (D16, f-keeper-handoff).
    pub(super) spawned: bool,
    /// The top command's code, set once it has ended. An error after that
    /// (a keeper handoff or the release save) is muzzle's, said on stderr,
    /// and the exit is still this code (spec §Exit codes, 2026-10-30 note).
    pub(super) ended: Option<u8>,
}

/// How the top command ended, before anything was released.
#[derive(Clone, Copy)]
pub(super) struct Ending {
    pub(super) code: u8,
    pub(super) killed: Option<Killed>,
    pub(super) exit: i32,
}

impl Job<'_> {
    pub(super) fn run(
        &mut self,
        args: &RunArgs,
        run_timeout: Option<Duration>,
    ) -> anyhow::Result<u8> {
        let slots = self.holder.claim.slots;
        let scoped = proc::systemd_user_reachable();
        // D16a: a container job runs in the container's cgroup, which the
        // scope's MemoryMax and CPUWeight don't reach. Still run it under the
        // scope: it holds podman exec and conmon, and is how a pruner sees
        // the job outlive a group kill.
        let container = crate::rules::argv_enters_container(&args.command);
        let enforced = scoped && !container;
        let reason = container.then_some(EnforcedReason::Container);
        let unit = unit_name(self.me, self.my_start, self.ticket);
        let scope = (scoped && container).then(|| format!("{unit}.scope"));
        let mut cmd = if scoped {
            let mut c = Command::new("systemd-run");
            c.args(["--user", "--scope", "--quiet"]);
            if systemd_run_expands_env() {
                c.arg("--expand-environment=no");
            }
            c.arg(format!("--unit={unit}"))
                .arg("-p")
                .arg(format!(
                    "MemoryMax={}",
                    self.budget.ram_per_slot.saturating_mul(u64::from(slots))
                ))
                // Without this the job swaps past MemoryMax instead of being
                // killed: 300 MB under MemoryMax=64M ran to completion on
                // linuxpc (32 GB swap). D9 means a hard cap, and D30a's OOM
                // signal needs the kill.
                .args(["-p", "MemorySwapMax=0"])
                .arg("-p")
                .arg(format!("CPUWeight={}", cpu_weight(slots)))
                .arg("--")
                .args(&args.command);
            c
        } else {
            let mut c = Command::new(&args.command[0]);
            c.args(&args.command[1..]);
            c
        };
        // D14: the claim decides the parallelism, overwriting inherited
        // values. D17: MUZZLE_HOLD marks everything below as nested.
        let n = slots.to_string();
        cmd.env("MUZZLE_SLOTS", &n)
            .env("CARGO_BUILD_JOBS", &n)
            .env("RUST_TEST_THREADS", &n)
            .env("MAKEFLAGS", format!("-j{n}"))
            .env(
                "MUZZLE_HOLD",
                format!("{}:{}:{}", self.ticket, self.me, self.my_start),
            );
        // D16: the new group is never given the terminal; a job that
        // needs it is ended by `supervise` (see `terminal_stop`).
        if !self.leader {
            cmd.process_group(0);
        }
        // D16a: until the path is known, a waiter in this pid namespace
        // finds the scope by scanning /proc, so the probe goes in at once.
        let mut probe = scope.as_ref().map(|_| ScopeProbe {
            cgroup: None,
            cgroup_ns: proc::namespace("cgroup"),
            pid_ns: proc::namespace("pid"),
        });
        let started = Instant::now();
        let child = match self.spawn_recorded(&mut cmd, enforced, reason, &scope, &probe)? {
            Ok(c) => c,
            Err(e) => {
                return Ok(spawn_failed(&args.command[0], &e));
            }
        };
        let pid = child.id() as Pid;
        let target = if self.leader {
            Target::GroupButMe {
                pgid: self.me as Pid,
                me: self.me as Pid,
            }
        } else {
            Target::Group(pid)
        };
        let pgid = if self.leader { self.me } else { pid as u32 };
        let mut sampler = scoped.then(|| Sampler {
            unit: unit.clone(),
            dir: None,
            last: None,
            next: Instant::now(),
        });
        if let (Some(p), Some(s)) = (probe.as_mut(), sampler.as_mut()) {
            // systemd-run moves itself into the scope before it execs the
            // command, within a few ms on linuxpc; until then the child
            // sits in muzzle's cgroup. A caught signal stays pending for
            // `supervise`.
            s.dir = scope_dir(pid, &unit, Duration::from_secs(2));
            p.cgroup = s.dir.as_deref().and_then(|d| {
                d.strip_prefix("/sys/fs/cgroup")
                    .ok()
                    .map(|r| format!("/{}", r.display()))
            });
            if p.cgroup.is_some() {
                self.set_spawned(pgid, enforced, reason, &scope, &probe);
            }
        }
        let mut deadline = Deadline::new(run_timeout);
        let mut beat = Heartbeat::new(self.store, self.ticket);
        let (status, ru) = supervise(
            pid,
            target,
            &mut deadline,
            sampler.as_mut(),
            Some(&mut beat),
        )?;
        deadline.drain(target, sampler.as_mut(), Some(&mut beat));
        let ran_s = started.elapsed().as_secs_f64();
        let mut ending = Ending {
            code: exit_code(status),
            killed: None,
            exit: i32::from(exit_code(status)),
        };
        if let Some(why) = deadline.why {
            ending.code = why.code();
            ending.exit = i32::from(ending.code);
            ending.killed = Some(match why {
                Fired::Timeout => Killed::Timeout,
                Fired::Terminal(_) => Killed::Signal,
            });
        } else if status.signal().is_some() {
            ending.killed = Some(Killed::Signal);
        }
        self.ended = Some(ending.code);

        // D16a: a container job outlives its group on every exit path, not
        // only a group SIGKILL: Ctrl-C, SIGTERM to muzzle and --run-timeout
        // all end the top command and leave conmon and the job running in
        // the scope (rv_cg_graceful_stop_frees_container). Hand off rather
        // than release, so a leaked build still holds its slots.
        let kept = (!deadline.fired() && !target.others().is_empty()) || self.scope_busy();
        let mut released_ms = None;
        if kept {
            self.hand_off(target, sampler, unit, started, &ending)?;
        } else {
            let mut cpu_s = ru.cpu_s;
            let mut peak = ru.max_rss_bytes;
            if let Some(s) = &sampler {
                // Both are lower bounds: rusage misses what the child didn't
                // reap, the last sample can be up to 250 ms old.
                if let Some(f) = s.last {
                    cpu_s = cpu_s.max(f.cpu_s);
                    peak = peak.max(f.mem_peak_bytes.unwrap_or(0));
                }
                let oom_sampled = s.last.is_some_and(|f| f.oom_kills > 0);
                // Always ask the unit: OOMPolicy=stop SIGTERMs the top
                // command, and one that handles TERM can exit with any code,
                // so the exit status says nothing. A clean scope reads
                // `inactive` on the first call. This also resets a failed
                // unit, so none is left loaded.
                let show = scope_outcome(&unit);
                cpu_s = cpu_s.max(show.cpu_s.unwrap_or(0.0));
                peak = peak.max(show.peak.unwrap_or(0));
                if oom_sampled || show.result == "oom-kill" {
                    ending.killed = Some(Killed::Oom);
                }
            }
            released_ms = Some(self.release(ran_s, &ending, Some(cpu_s), Some(mb(peak)), false)?);
        }
        // The job ran and its slots are settled, so its code stands: a
        // `--report` that can't be written is muzzle's error, said on stderr,
        // not the command's (spec §Exit codes, 2026-10-30 note).
        if let Err(e) = self.finish(args, ran_s, released_ms, &ending, enforced, kept) {
            eprintln!("muzzle: {e:#}");
        }
        Ok(ending.code)
    }

    /// Spawn `cmd` with the state lock held, and record the job's group in
    /// the same write (f-nonleader-pgid). A non-leader's group is the
    /// child's pid, unknown until spawn, and the holder sat at `pgid: None`
    /// until a later write that waited for the lock: a muzzle SIGKILLed in
    /// that wait read as a dead holder with no group, and its slots were
    /// handed out while the job ran. Under the lock no pruner can run
    /// between spawn and the write, so the window is down to spawn plus one
    /// save (milliseconds, whatever the contention); `spawn` returns once
    /// the child has exec'd. The lock fd is CLOEXEC, so the job never holds
    /// it. The outer `Err` is the state write failing; the inner one is a
    /// spawn failure, which frees the entry.
    fn spawn_recorded(
        &mut self,
        cmd: &mut Command,
        enforced: bool,
        reason: Option<EnforcedReason>,
        scope: &Option<String>,
        probe: &Option<ScopeProbe>,
    ) -> anyhow::Result<std::io::Result<std::process::Child>> {
        let mut spawned = None;
        let recorded = self.store.update(now_ms(), |s| {
            let c = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    s.remove(self.ticket);
                    spawned = Some(Err(e));
                    return Ok(());
                }
            };
            let pgid = if self.leader { self.me } else { c.id() };
            spawned = Some(Ok(c));
            if let Some(h) = s.holder_mut(self.ticket) {
                apply_spawned(h, pgid, enforced, reason, scope, probe);
            }
            Ok(())
        });
        match spawned {
            Some(Ok(c)) => {
                self.spawned = true;
                let pgid = if self.leader { self.me } else { c.id() };
                match recorded {
                    Ok(()) => apply_spawned(&mut self.holder, pgid, enforced, reason, scope, probe),
                    // The job runs, so supervise it: retry the write, and
                    // until it lands a waiter sees this muzzle alive.
                    Err(e) => {
                        eprintln!("muzzle: recording the spawned job: {e:#}");
                        self.set_spawned(pgid, enforced, reason, scope, probe);
                    }
                }
                Ok(Ok(c))
            }
            // A failed spawn removed the entry. If that write failed too,
            // the caller's error path frees it.
            Some(Err(e)) => recorded.map(|()| Err(e)),
            // The lock or the read failed: nothing ran, nothing was written.
            None => Err(recorded
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("spawn skipped"))),
        }
    }

    /// Store what spawn learnt in the holder entry and in `self.holder`.
    /// Best effort: the job is already running, and giving up here would
    /// leave it unsupervised while muzzle's exit freed its slots. Without
    /// the pgid a waiter still sees this muzzle alive until the job ends.
    fn set_spawned(
        &mut self,
        pgid: u32,
        enforced: bool,
        reason: Option<EnforcedReason>,
        scope: &Option<String>,
        probe: &Option<ScopeProbe>,
    ) {
        let apply = |h: &mut Holder| apply_spawned(h, pgid, enforced, reason, scope, probe);
        if let Err(e) = self.store.update(now_ms(), |s| {
            if let Some(h) = s.holder_mut(self.ticket) {
                apply(h);
            }
            Ok(())
        }) {
            eprintln!("muzzle: recording the spawned job: {e:#}");
        }
        apply(&mut self.holder);
    }

    /// D16a: a container run's scope still has processes (conmon and the
    /// job). False for a host run, which the group already covers.
    pub(super) fn scope_busy(&self) -> bool {
        self.holder
            .scope
            .as_deref()
            .is_some_and(|u| scope_populated(u, self.holder.probe.as_ref()))
    }
}

fn apply_spawned(
    h: &mut Holder,
    pgid: u32,
    enforced: bool,
    reason: Option<EnforcedReason>,
    scope: &Option<String>,
    probe: &Option<ScopeProbe>,
) {
    h.pgid = Some(pgid);
    h.enforced = enforced;
    h.enforced_reason = reason;
    h.scope = scope.clone();
    h.probe = probe.clone();
}
