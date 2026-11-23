//! H4: `muzzle run --grace S`, retroactive slot accounting. The command
//! starts at once with no claim and no state write; a run that is still
//! alive after S seconds and has used `--grace-cpu` CPU-seconds is then
//! entered into the state as a live holder, sized by the CPU it has shown.
//! Most commands end first and leave no trace in `state.json` or
//! `runs.jsonl`.

use std::collections::HashMap;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use super::job::{Ending, Job};
use super::report::{Measured, Times, repo_str, write_report_as};
use super::spawn::{Deadline, Fired, Launches, Target, exit_code, spawn_failed, supervise};
use super::{Heartbeat, POLL, ProcLive, TICK, now_ms, reading_budget_cap, take_signal};
use crate::admit;
use crate::cli::RunArgs;
use crate::proc::{self, Pid, StartTime};
use crate::rules::config::Config;
use crate::state::{Budget, Claim, Holder, Killed, Store};

/// Everything `main` knows by the time it dispatches a grace run.
pub(super) struct Ctx<'a> {
    pub(super) args: &'a RunArgs,
    pub(super) config: &'a Config,
    pub(super) store: &'a Store,
    pub(super) claim: Claim,
    pub(super) reading: Budget,
    pub(super) leader: bool,
    pub(super) me: u32,
    pub(super) my_start: StartTime,
    pub(super) grace_s: f64,
    pub(super) run_timeout: Option<Duration>,
}

/// The claim a run took when it was promoted.
pub(super) struct Promotion {
    pub(super) holder: Holder,
    pub(super) budget: Budget,
    /// When the claim was taken (under the lock): `--report`'s
    /// `admitted_ms`. The holder's `run_start_ms` is the spawn, earlier.
    pub(super) admitted_ms: u64,
}

/// Samples the job's group and promotes it. Driven from
/// [`Heartbeat::beat`], which `supervise` calls every `TICK`.
pub(super) struct Watch {
    target: Target,
    pgid: u32,
    me: u32,
    my_start: StartTime,
    leader: bool,
    spawned: Instant,
    spawned_ms: u64,
    grace: f64,
    grace_cpu: f64,
    max_slots: u32,
    config_slots: Option<u32>,
    reading: Budget,
    claim: Claim,
    rule: Option<String>,
    /// High-water CPU (ns) per (pid, start): a member that exits keeps its
    /// share, so the total never drops.
    cpu: HashMap<(u32, u64), u64>,
    next: Instant,
    pub(super) launches: Launches,
    pub(super) promoted: Option<Promotion>,
}

impl Watch {
    pub(super) fn total_cpu_s(&self) -> f64 {
        self.cpu.values().sum::<u64>() as f64 / 1e9
    }

    pub(super) fn tick(&mut self, store: &Store) {
        self.launches.tick(self.target);
        if self.promoted.is_some() || Instant::now() < self.next {
            return;
        }
        self.next = Instant::now() + POLL;
        let sample = proc::group_cpu(self.pgid as Pid);
        if let Some(members) = &sample {
            for m in members {
                // The leader's own group includes muzzle; its sampling is
                // not the job's CPU.
                if self.leader && m.pid == self.me {
                    continue;
                }
                let e = self.cpu.entry((m.pid, m.start)).or_insert(0);
                *e = (*e).max(m.cpu_ns);
            }
        }
        let elapsed = self.spawned.elapsed().as_secs_f64();
        let total = self.total_cpu_s();
        // EPERM: members couldn't be read, so CPU is unknown; wait twice
        // the grace and claim one slot.
        let blind = sample.is_none() && elapsed >= 2.0 * self.grace;
        if elapsed < self.grace || !(total >= self.grace_cpu || blind) {
            return;
        }
        let slots = if sample.is_none() {
            1
        } else {
            (total / elapsed).ceil() as u32
        };
        match self.promote(store, slots) {
            Ok(p) => self.promoted = Some(p),
            // The job runs on, unclaimed; the next POLL retries.
            Err(e) => eprintln!("muzzle: claiming slots for a grace run: {e:#}"),
        }
    }

    fn promote(&self, store: &Store, wanted: u32) -> anyhow::Result<Promotion> {
        let mut out = None;
        store.update(now_ms(), |s| {
            let budget = s.resolve_budget(self.reading, self.config_slots);
            let slots = reading_budget_cap(wanted, self.max_slots, budget.budget);
            let ticket = s.next_ticket;
            s.next_ticket += 1;
            // Slots the others hold, read before this claim is added: it
            // may push `held` past the budget, which `admit` copes with.
            // Only the live ones: a SIGKILLed run's slots stay in the file
            // until a prune, and the normal path never counts them.
            let held = s
                .holders
                .iter()
                .filter(|h| admit::holds(h, &ProcLive))
                .map(|h| h.claim.slots)
                .sum();
            let now = now_ms();
            let mut claim = self.claim.clone();
            claim.slots = slots;
            claim.rule = self.rule.clone();
            let holder = Holder {
                ticket,
                pid: self.me,
                start: self.my_start,
                pgid: Some(self.pgid),
                run_start_ms: self.spawned_ms,
                waited_s: 0.0,
                enforced: false,
                kept: false,
                enforced_reason: None,
                scope: None,
                probe: None,
                admit_load1: proc::loadavg().map(|l| super::report::round3(l[0])),
                admit_held: Some(held),
                seen_ms: Some(now),
                orphan_since_ms: None,
                orphan_active_ms: None,
                orphan_sample_ms: None,
                orphan_procs: None,
                grace: true,
                claim,
            };
            s.holders.push(holder.clone());
            out = Some(Promotion {
                holder,
                budget,
                admitted_ms: now,
            });
            Ok(())
        })?;
        Ok(out.expect("set under the lock"))
    }
}

pub(super) fn run(cx: &Ctx) -> anyhow::Result<ExitCode> {
    let args = cx.args;
    let mut cmd = Command::new(&args.command[0]);
    cmd.args(&args.command[1..]);
    // D16: a non-leader's job gets a group of its own.
    if !cx.leader {
        cmd.process_group(0);
    }
    let started = Instant::now();
    let spawned_ms = now_ms();
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Ok(ExitCode::from(spawn_failed(&args.command[0], &e))),
    };
    let pid = child.id() as Pid;
    let (target, pgid) = if cx.leader {
        (
            Target::GroupButMe {
                pgid: cx.me as Pid,
                me: cx.me as Pid,
            },
            cx.me,
        )
    } else {
        (Target::Group(pid), pid as u32)
    };
    let watch = Watch {
        target,
        pgid,
        me: cx.me,
        my_start: cx.my_start,
        leader: cx.leader,
        spawned: started,
        spawned_ms,
        grace: cx.grace_s,
        grace_cpu: args.grace_cpu.unwrap_or(cx.config.grace_cpu_s),
        max_slots: cx.config.grace_max_slots,
        config_slots: cx.config.slots,
        reading: cx.reading,
        claim: cx.claim.clone(),
        rule: args.rule.clone().or(Some("unmatched-heavy".into())),
        cpu: HashMap::new(),
        next: Instant::now() + POLL,
        launches: Launches::new(pid),
        promoted: None,
    };
    let mut beat = Heartbeat::watching(cx.store, watch);
    let mut deadline = Deadline::new(cx.run_timeout);
    let (status, ru) = supervise(pid, target, &mut deadline, None, Some(&mut beat), None)?;
    deadline.drain(target, None, Some(&mut beat), None);
    // Survivors of the top command still count: keep sampling until the
    // group empties or earns a claim. With a `--run-timeout` the watch
    // outlives the promotion: a keeper enforces no timeout (D16), so
    // handing off would let a survivor outrun it.
    let promoted = |b: &Heartbeat| b.watch.as_ref().is_some_and(|w| w.promoted.is_some());
    let timed = deadline.after.is_some();
    while !deadline.fired() && (timed || !promoted(&beat)) && !target.others().is_empty() {
        deadline.check(started, target);
        if deadline.fired() {
            break;
        }
        if let Some(sig) = take_signal() {
            forward(target, sig);
        }
        beat.beat();
        std::thread::sleep(TICK);
    }
    // A no-op unless the deadline fired in the loop above.
    deadline.drain(target, None, Some(&mut beat), None);
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

    let Heartbeat { watch, .. } = beat;
    let watch = watch.expect("set above");
    let Some(promotion) = &watch.promoted else {
        // Fast or idle: nothing was claimed, nothing is logged.
        if let Some(path) = &args.report {
            let t = Times {
                waited_s: 0.0,
                ran_s,
                admitted_ms: None,
                released_ms: None,
            };
            let c = &cx.claim;
            if let Err(e) = write_report_as(
                path,
                &t,
                Some(0),
                None,
                args.rule.as_deref(),
                repo_str(std::env::current_dir().ok().as_deref())
                    .or_else(|| c.repo.clone())
                    .as_deref(),
                ending.code,
                false,
                None,
                false,
                Some((false, ending.killed)),
            ) {
                eprintln!("muzzle: {e:#}");
            }
        }
        return Ok(ExitCode::from(ending.code));
    };

    let job = Job {
        store: cx.store,
        ticket: promotion.holder.ticket,
        me: cx.me,
        my_start: cx.my_start,
        holder: promotion.holder.clone(),
        orphan: crate::admit::AdmitConfig {
            orphan_idle_s: cx.config.orphan_idle_s,
            orphan_ttl: cx.config.orphan_ttl,
        },
        budget: promotion.budget,
        leader: cx.leader,
        spawned: true,
        ended: Some(ending.code),
    };
    let kept = !deadline.fired() && !target.others().is_empty();
    let mut released_ms = None;
    let total_cpu = watch.total_cpu_s();
    if kept {
        let r = job.hand_off(
            target,
            None,
            watch.launches,
            String::new(),
            started,
            &ending,
        );
        if let Err(e) = r {
            eprintln!("muzzle: {e:#}");
        }
    } else {
        let m = Measured {
            cpu_s: Some(ru.cpu_s.max(total_cpu)),
            mem_peak_mb: Some(super::scope::mb(ru.max_rss_bytes)),
            procs: Some(watch.launches.count),
        };
        match job.release(ran_s, &ending, m, false) {
            Ok(ms) => released_ms = Some(ms),
            Err(e) => eprintln!("muzzle: {e:#}"),
        }
    }
    if let Err(e) = job.finish_as(
        args,
        ran_s,
        released_ms,
        &ending,
        false,
        kept,
        Some(promotion.admitted_ms),
    ) {
        eprintln!("muzzle: {e:#}");
    }
    Ok(ExitCode::from(ending.code))
}

/// §Architecture 4.4 for the survivors of a top command that has exited:
/// what `Target::forward` does, which `spawn` keeps private. A leader's
/// terminal signals already reached its group from the terminal.
fn forward(target: Target, sig: libc::c_int) {
    let tty = matches!(sig, libc::SIGINT | libc::SIGQUIT);
    if tty && matches!(target, Target::GroupButMe { .. }) {
        return;
    }
    for pid in target.others() {
        // SAFETY: plain syscall.
        unsafe { libc::kill(pid, sig) };
    }
}
