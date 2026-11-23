//! Unit `drop` (G4): `muzzle drop` clears wedged claims without editing
//! `state.json`.
//!
//! Three phases, so the state lock is never held while waiting on a job:
//! 1. validate the selectors (before any state access), read the state, pick
//!    the holders, and with `--kill` attribute each group to its claim and
//!    stop scope-only jobs (a refusal changes nothing);
//! 2. remove the holders and append their `killed: "dropped"` lines;
//! 3. with `--kill`, signal the `(pid, start, pgid)`-verified members, no
//!    lock.
//!
//! The claim goes *before* the signals, not after as G4 first wrote it: a
//! live muzzle outside the group reaps the signalled job and releases its
//! ticket within the grace poll, and whoever removes the holder first owns
//! the terminal record (u4_drop_terminal_record). The price is that the
//! slots free up to `GRACE` before the job is gone.

use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use crate::admit::rfc3339;
use crate::cli::DropArgs;
use crate::exit;
use crate::proc;
use crate::state::{Holder, Killed, RunRecord, State, Store};
use crate::status::now_ms;

/// SIGTERM to SIGKILL grace, polled every [`POLL`].
const GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);

/// A group member as snapshotted: signalled only while `(pid, start)` still
/// matches and the pid is still in the group, so a recycled pid is never hit.
type Member = (proc::Pid, proc::StartTime, proc::Pid);

/// The selector check, which needs no state: run before the store is opened
/// so a bad invocation never reads (or quarantines) `state.json`.
fn usage(args: &DropArgs) -> Result<(), String> {
    if args.tickets.is_empty() && !args.dead && args.agent.is_none() {
        return Err("name a ticket, or pass --dead or --agent NAME".into());
    }
    Ok(())
}

/// Picks the holders `args` names. `live` says whether a holder is healthy
/// (its muzzle lives and it is not kept). `Err` is a usage error for stderr.
fn select(
    state: &State,
    args: &DropArgs,
    live: impl Fn(&Holder) -> bool,
) -> Result<Vec<Holder>, String> {
    usage(args)?;
    if !args.tickets.is_empty() {
        let mut out: Vec<Holder> = Vec::new();
        for t in &args.tickets {
            let Some(h) = state.holders.iter().find(|h| h.ticket == *t) else {
                return Err(format!("no holder with ticket {t}"));
            };
            if live(h) && !args.force {
                return Err(format!(
                    "ticket {t} is live (its muzzle is running); pass --force to drop it"
                ));
            }
            if args.kill && h.pgid.is_none() && h.scope.is_none() {
                return Err(format!(
                    "ticket {t} has no process group yet, nothing to --kill; drop it without --kill"
                ));
            }
            if !out.iter().any(|o| o.ticket == *t) {
                out.push(h.clone());
            }
        }
        return Ok(out);
    }
    let out: Vec<Holder> = state
        .holders
        .iter()
        .filter(|h| !live(h))
        .filter(|h| {
            args.agent
                .as_deref()
                .is_none_or(|a| h.claim.agent.as_deref() == Some(a))
        })
        .filter(|h| !args.kill || h.pgid.is_some() || h.scope.is_some())
        .cloned()
        .collect();
    Ok(out)
}

fn is_live(h: &Holder) -> bool {
    !h.kept && proc::is_alive(h.pid as proc::Pid, h.start)
}

/// The `runs.jsonl` line for a dropped holder; the run ends now.
fn record(h: Holder, now_ms: u64, budget: Option<u32>) -> RunRecord {
    let end_ms = now_ms.max(h.run_start_ms);
    RunRecord {
        v: 1,
        end: rfc3339(end_ms),
        rule: h.claim.rule,
        slots: h.claim.slots,
        class: h.claim.class,
        cfg: h.claim.cfg,
        agent: h.claim.agent,
        cwd: h.claim.cwd,
        repo: h.claim.repo,
        waited_s: h.waited_s,
        ran_s: (end_ms - h.run_start_ms) as f64 / 1000.0,
        exit: None,
        killed: Some(Killed::Dropped),
        cpu_s: None,
        mem_peak_mb: None,
        enforced: h.enforced,
        nested: false,
        kept: h.kept,
        budget,
        enforced_reason: h.enforced_reason,
        load1: h.admit_load1,
        held: h.admit_held,
        procs: None,
    }
}

/// The effective uid of `pid`, from the process credentials. `None` when
/// unreadable: the caller fails closed. (Not `/proc/PID` ownership, which the
/// kernel presents as root for non-dumpable processes, and not `kill(pid, 0)`,
/// which root passes for every uid.)
fn euid_of(pid: proc::Pid) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        // "Uid:\treal\teffective\tsaved\tfs"
        let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }
    #[cfg(target_os = "macos")]
    {
        // PROC_PIDTBSDINFO is EPERM for another euid unless we are root,
        // which is the fail-closed answer for a foreign process anyway.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the buffer is `size` bytes.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        (n == size).then_some(info.pbi_uid)
    }
}

/// Whether `(pid, start)` is ours to signal: its effective uid is ours, read
/// between two start-stamp reads so the uid belongs to the sampled process.
fn mine(pid: proc::Pid, start: proc::StartTime) -> bool {
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    proc::is_alive(pid, start) && euid_of(pid) == Some(me) && proc::is_alive(pid, start)
}

/// Whether the live members `all` of `h`'s group are `h`'s own, and not a
/// replacement group that reused the pgid (G4: no PGID-reuse). Evidence, any:
/// - the group leader is `h`'s muzzle (`pgid == pid`) and is in the group
///   with the recorded start; a leader present with another start is a
///   reuse and rules everything else out;
/// - `h`'s muzzle is alive and a member descends from it.
///
/// G1's `orphan_procs` is not evidence: it samples by bare pgid, so after a
/// reuse it attests to the replacement group.
///
/// An empty group has nothing to signal and needs no evidence.
fn attributed(
    h: &Holder,
    all: &[(proc::Pid, proc::StartTime)],
    muzzle_alive: bool,
    descends: impl Fn(proc::Pid) -> bool,
) -> bool {
    if all.is_empty() {
        return true;
    }
    if h.pgid == Some(h.pid)
        && let Some(&(_, start)) = all.iter().find(|(p, _)| *p as u32 == h.pid)
    {
        return start == h.start;
    }
    muzzle_alive && all.iter().any(|&(p, _)| descends(p))
}

/// The members of `h`'s group we may signal, with their start times,
/// excluding our own process group and ourselves. `Err` when the group
/// can't be attributed to the claim: nothing may be signalled then.
fn snapshot(h: &Holder, pgid: proc::Pid) -> Result<Vec<Member>, String> {
    // SAFETY: getpgrp/getpid have no preconditions.
    let (own_pg, own_pid) = unsafe { (libc::getpgrp(), libc::getpid()) };
    if pgid == own_pg || pgid <= 1 {
        return Ok(Vec::new());
    }
    // A member's stamp is read, then its group re-checked, so a pid that
    // left and was recycled between listing and reading is dropped.
    let all: Vec<(proc::Pid, proc::StartTime)> = proc::group_members(pgid)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| {
            let s = proc::start_time(p)?;
            (proc::pgid(p) == Some(pgid)).then_some((p, s))
        })
        .collect();
    let (hp, hs) = (h.pid as proc::Pid, h.start);
    let descends = |p| {
        proc::ancestors(p)
            .iter()
            .any(|a| a.pid == hp && a.start == hs)
    };
    if !attributed(h, &all, proc::is_alive(hp, hs), descends) {
        return Err(format!(
            "ticket {}: process group {pgid} can't be tied to this claim (its pgid may have been reused); not signalling. Drop it without --kill",
            h.ticket
        ));
    }
    Ok(all
        .into_iter()
        .filter(|&(p, s)| p != own_pid && mine(p, s))
        .map(|(p, s)| (p, s, pgid))
        .collect())
}

fn signal(members: &[Member], sig: libc::c_int) {
    for &(pid, start, pgid) in members {
        if proc::is_alive(pid, start) && proc::pgid(pid) == Some(pgid) {
            // SAFETY: pid was verified against its start time just above.
            unsafe { libc::kill(pid, sig) };
        }
    }
}

fn any_alive(members: &[Member]) -> bool {
    members.iter().any(|&(p, s, _)| proc::is_alive(p, s))
}

/// SIGTERM, up to [`GRACE`] for them to exit, then SIGKILL the survivors.
fn terminate(members: &[Member]) {
    signal(members, libc::SIGTERM);
    let deadline = Instant::now() + GRACE;
    while any_alive(members) && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    signal(members, libc::SIGKILL);
}

/// D16a: a container job lives outside the group, in its scope. `Err` says
/// why it was not stopped: the manager is unreachable or `systemctl stop`
/// failed.
fn stop_scope(unit: &str) -> Result<(), String> {
    if !proc::systemd_user_reachable() {
        return Err(format!(
            "systemd user manager not reachable; could not stop {unit}"
        ));
    }
    match Command::new("systemctl")
        .args(["--user", "stop", unit])
        .output()
    {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "systemctl --user stop {unit} failed ({}): {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("systemctl --user stop {unit}: {e}")),
    }
}

pub fn main(args: &DropArgs) -> anyhow::Result<ExitCode> {
    if let Err(msg) = usage(args) {
        eprintln!("muzzle: drop: {msg}");
        return Ok(ExitCode::from(exit::USAGE));
    }
    let store = Store::open_default()?;
    let state = store.read()?;
    let picked = match select(&state, args, is_live) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("muzzle: drop: {msg}");
            return Ok(ExitCode::from(exit::USAGE));
        }
    };
    if picked.is_empty() {
        println!("muzzle: nothing to drop");
        return Ok(ExitCode::SUCCESS);
    }
    // Everything that can refuse happens here, before any side effect on
    // the claims; scope-only jobs are stopped last since that one is not
    // undoable, and a failure leaves the claims for a retry.
    let mut plans: Vec<(u64, Vec<Member>)> = Vec::new();
    if args.kill {
        for h in &picked {
            match h.pgid.map(|g| snapshot(h, g as proc::Pid)) {
                Some(Ok(m)) => plans.push((h.ticket, m)),
                Some(Err(msg)) => {
                    eprintln!("muzzle: drop: {msg}");
                    return Ok(ExitCode::from(exit::INTERNAL));
                }
                None => {}
            }
        }
        for h in picked.iter().filter(|h| h.pgid.is_none()) {
            if let Some(unit) = &h.scope
                && let Err(e) = stop_scope(unit)
            {
                eprintln!(
                    "muzzle: drop: ticket {}: no process group to signal and {e}; claim kept",
                    h.ticket
                );
                return Ok(ExitCode::from(exit::INTERNAL));
            }
        }
    }
    // G4: with --kill the claim stays in state.json through the whole
    // shutdown (SIGTERM, up to GRACE, SIGKILL), so no other run is admitted
    // onto slots still in use, and the state lock is never held across the
    // grace. Only then is the holder removed; a runner that exited and
    // released itself meanwhile has already logged its own line.
    if args.kill {
        for h in picked.iter().filter(|h| h.pgid.is_some()) {
            if let Some(unit) = &h.scope
                && let Err(e) = stop_scope(unit)
            {
                eprintln!(
                    "muzzle: warning: ticket {}: could not stop scope: {e}",
                    h.ticket
                );
            }
        }
        let all: Vec<Member> = plans.into_iter().flat_map(|(_, m)| m).collect();
        terminate(&all);
    }
    let tickets: Vec<u64> = picked.iter().map(|h| h.ticket).collect();
    let now = now_ms();
    let dropped = store.update_logged(now, |s| {
        let budget = s.budget.map(|b| b.budget);
        let mut done = Vec::new();
        let mut records = Vec::new();
        for t in &tickets {
            // Already released or pruned meanwhile: its own line is written.
            if let Some(i) = s.holders.iter().position(|h| h.ticket == *t) {
                let h = s.holders.remove(i);
                done.push(*t);
                records.push(record(h, now, budget));
            }
        }
        Ok((done, records))
    })?;
    let list: Vec<String> = dropped.iter().map(u64::to_string).collect();
    println!(
        "muzzle: dropped {} holder{}: {}",
        dropped.len(),
        if dropped.len() == 1 { "" } else { "s" },
        list.join(" ")
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admit::Class;
    use crate::state::Claim;

    fn holder(ticket: u64, agent: &str, kept: bool, pgid: Option<u32>) -> Holder {
        Holder {
            ticket,
            pid: ticket as u32,
            start: 1,
            pgid,
            run_start_ms: 1000,
            waited_s: 0.5,
            enforced: false,
            kept,
            enforced_reason: None,
            scope: None,
            probe: None,
            admit_load1: None,
            admit_held: None,
            seen_ms: None,
            orphan_since_ms: None,
            orphan_active_ms: None,
            orphan_sample_ms: None,
            orphan_procs: None,
            grace: false,
            claim: Claim {
                slots: 2,
                class: Class::Normal,
                agent: Some(agent.into()),
                rule: Some("gate".into()),
                cwd: "/tmp".into(),
                repo: None,
                cfg: None,
                pane: None,
            },
        }
    }

    fn args(tickets: &[u64], dead: bool, agent: Option<&str>, kill: bool, force: bool) -> DropArgs {
        DropArgs {
            tickets: tickets.to_vec(),
            dead,
            agent: agent.map(String::from),
            kill,
            force,
        }
    }

    fn state() -> State {
        State {
            holders: vec![
                holder(1, "a", false, Some(10)), // live (odd tickets)
                holder(2, "a", false, Some(20)), // dead muzzle
                holder(3, "b", true, Some(30)),  // kept
                holder(4, "b", false, None),     // dead, not spawned
            ],
            ..State::default()
        }
    }

    fn live(h: &Holder) -> bool {
        h.ticket == 1
    }

    fn tickets(v: Result<Vec<Holder>, String>) -> Vec<u64> {
        v.unwrap().iter().map(|h| h.ticket).collect()
    }

    #[test]
    fn needs_a_selector() {
        assert!(select(&state(), &args(&[], false, None, false, false), live).is_err());
    }

    #[test]
    fn dead_skips_live() {
        let got = select(&state(), &args(&[], true, None, false, false), live);
        assert_eq!(tickets(got), [2, 3, 4]);
    }

    #[test]
    fn agent_picks_that_agents_orphans() {
        let got = select(&state(), &args(&[], false, Some("a"), false, false), live);
        assert_eq!(tickets(got), [2]);
        let got = select(&state(), &args(&[], false, Some("zz"), false, false), live);
        assert!(tickets(got).is_empty());
    }

    #[test]
    fn live_needs_force() {
        let e = select(&state(), &args(&[1], false, None, false, false), live).unwrap_err();
        assert!(e.contains("--force"), "{e}");
        let got = select(&state(), &args(&[1], false, None, false, true), live);
        assert_eq!(tickets(got), [1]);
    }

    #[test]
    fn unknown_ticket_is_refused() {
        assert!(select(&state(), &args(&[99], false, None, false, false), live).is_err());
    }

    #[test]
    fn kill_refuses_unspawned_holder() {
        let e = select(&state(), &args(&[4], false, None, true, false), live).unwrap_err();
        assert!(e.contains("no process group"), "{e}");
        assert!(select(&state(), &args(&[4], false, None, false, false), live).is_ok());
        // A scope is stoppable without a group.
        let mut st = state();
        st.holders[3].scope = Some("muzzle-x.scope".into());
        assert!(select(&st, &args(&[4], false, None, true, false), live).is_ok());
    }

    #[test]
    fn record_is_dropped_and_ends_now() {
        let r = record(holder(2, "a", false, Some(20)), 4000, Some(8));
        assert_eq!(r.killed, Some(Killed::Dropped));
        assert_eq!(r.ran_s, 3.0);
        assert_eq!(r.budget, Some(8));
        assert_eq!(r.exit, None);
    }

    #[test]
    fn snapshot_excludes_own_group() {
        // SAFETY: getpgrp has no preconditions.
        let h = holder(1, "a", false, None);
        assert!(snapshot(&h, unsafe { libc::getpgrp() }).unwrap().is_empty());
        assert!(snapshot(&h, 1).unwrap().is_empty());
    }

    #[test]
    fn euid_is_the_process_credential() {
        let me = std::process::id() as proc::Pid;
        // SAFETY: geteuid has no preconditions.
        assert_eq!(euid_of(me), Some(unsafe { libc::geteuid() }));
        assert!(mine(me, proc::start_time(me).unwrap()));
        // Unreadable credentials fail closed.
        assert_eq!(euid_of(4_000_000), None);
        assert!(!mine(4_000_000, 1));
        // A stale start stamp is not ours either.
        assert!(!mine(me, proc::start_time(me).unwrap() + 1));
    }

    #[test]
    fn attribution_needs_evidence() {
        use crate::state::ProcCpu;
        let mut h = holder(50, "a", false, Some(50));
        let none = |_| false;
        assert!(attributed(&h, &[], false, none));
        // Leader present with the recorded start: ours. Another start: a
        // reused pgid, whatever else looks right.
        assert!(attributed(&h, &[(50, 1), (51, 9)], false, none));
        assert!(!attributed(&h, &[(50, 2), (51, 9)], true, |_| true));
        // No leader, no evidence: ambiguous.
        assert!(!attributed(&h, &[(51, 9)], false, none));
        // A sampled member is not evidence, whatever G1 recorded.
        h.orphan_procs = Some(vec![ProcCpu {
            pid: 51,
            start: 9,
            cpu_ns: 0,
        }]);
        assert!(!attributed(&h, &[(51, 9)], false, none));
        // A descendant of a live muzzle counts only while it lives.
        h.orphan_procs = None;
        h.pgid = Some(60);
        assert!(attributed(&h, &[(61, 9)], true, |_| true));
        assert!(!attributed(&h, &[(61, 9)], false, |_| true));
    }

    #[test]
    fn selectors_are_checked_without_state() {
        assert!(usage(&args(&[], false, None, true, true)).is_err());
        assert!(usage(&args(&[], true, None, false, false)).is_ok());
    }
}
