//! One admitted run, from spawn to release.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Command;
use std::time::{Duration, Instant};

use super::scope::{
    Sampler, cpu_weight, mb, scope_dir, scope_outcome, systemd_run_expands_env, unit_name,
};
use super::spawn::{Deadline, Target, exit_code, spawn_failed, supervise};
use super::{Heartbeat, now_ms, scope_populated};
use crate::cli::RunArgs;
use crate::exit;
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
        if !self.leader {
            cmd.process_group(0);
        }
        let started = Instant::now();
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.store.update(now_ms(), |s| Ok(s.remove(self.ticket)))?;
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
        // D16a: until the path is known, a waiter in this pid namespace
        // finds the scope by scanning /proc, so the probe goes in at once.
        let mut probe = scope.as_ref().map(|_| ScopeProbe {
            cgroup: None,
            cgroup_ns: proc::namespace("cgroup"),
            pid_ns: proc::namespace("pid"),
        });
        self.set_spawned(pgid, enforced, reason, &scope, &probe)?;
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
                self.set_spawned(pgid, enforced, reason, &scope, &probe)?;
            }
        }
        let mut deadline = Deadline {
            after: run_timeout,
            termed: None,
            killed: false,
        };
        let mut beat = Heartbeat::new(self.store, self.ticket);
        let (status, ru) = supervise(
            pid,
            target,
            &mut deadline,
            sampler.as_mut(),
            Some(&mut beat),
        )?;
        deadline.drain(target, sampler.as_mut(), &mut beat);
        let ran_s = started.elapsed().as_secs_f64();
        let mut ending = Ending {
            code: exit_code(status),
            killed: None,
            exit: i32::from(exit_code(status)),
        };
        if deadline.fired() {
            ending.code = exit::RUN_TIMEOUT;
            ending.exit = i32::from(exit::RUN_TIMEOUT);
            ending.killed = Some(Killed::Timeout);
        } else if status.signal().is_some() {
            ending.killed = Some(Killed::Signal);
        }

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
        self.finish(args, ran_s, released_ms, &ending, enforced, kept)?;
        Ok(ending.code)
    }

    /// Store what spawn learnt in the holder entry and in `self.holder`.
    fn set_spawned(
        &mut self,
        pgid: u32,
        enforced: bool,
        reason: Option<EnforcedReason>,
        scope: &Option<String>,
        probe: &Option<ScopeProbe>,
    ) -> anyhow::Result<()> {
        let apply = |h: &mut Holder| {
            h.pgid = Some(pgid);
            h.enforced = enforced;
            h.enforced_reason = reason;
            h.scope = scope.clone();
            h.probe = probe.clone();
        };
        self.store.update(now_ms(), |s| {
            if let Some(h) = s.holder_mut(self.ticket) {
                apply(h);
            }
            Ok(())
        })?;
        apply(&mut self.holder);
        Ok(())
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
