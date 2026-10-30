//! Unit `run` (spec §Architecture 4, D13-D19, D27, D30a): nesting, enqueue
//! and poll, spawn (in a systemd scope when reachable), signals, the keeper
//! handoff, release, `runs.jsonl` (through [`crate::runlog`]), `--report`,
//! exit codes and stderr lines.

mod job;
mod keeper;
mod queue;
mod report;
mod scope;
mod spawn;

use job::Job;
use queue::{Queue, Turn};
use report::repo_str;
use spawn::{find_program, run_nested, spawn_failed};

use std::process::ExitCode;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;

use crate::admit::{self, Class, Liveness};
use crate::cli::{self, RunArgs};
use crate::exit;
use crate::proc::{self, Pid};
use crate::rules::config::{Config, ConfigError};
use crate::state::{Budget, Claim, ScopeProbe, State, Store};

/// Spec §Architecture 4.2: each waiter polls `admit` every 250 ms.
const POLL: Duration = Duration::from_millis(250);
/// How often a running job is checked for exit, signals and its deadline.
/// Every run's end is late by up to this much, so it stays short.
const TICK: Duration = Duration::from_millis(20);
/// D13: one progress line on stderr every 30 s while queued.
const PROGRESS: Duration = Duration::from_secs(30);
/// ran_s_lag_followup: how often a running muzzle or keeper stamps its
/// holder's `seen_ms`, so a run whose muzzle is SIGKILLed with its job ends
/// within this of the truth when no waiter was polling. One locked, fsynced
/// state write per running job per period; waiters already write every
/// `POLL`, so this is 1/8 of one waiter's I/O.
const HEARTBEAT: Duration = Duration::from_secs(2);
/// D18: SIGTERM, then SIGKILL this much later.
const KILL_GRACE: Duration = Duration::from_secs(10);
/// `--expand-environment=` arrived in systemd 254. From then on systemd-run
/// expands `$VAR` in the command line, so `bash -c 'echo $HOME'` would be
/// rewritten before bash sees it (checked on linuxpc, systemd 259).
const SYSTEMD_EXPAND_FLAG: u32 = 254;

static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    SIGNAL.store(sig, Ordering::SeqCst);
}

/// Catch the signals a caller uses to stop us, so muzzle can drop its ticket
/// (D19) or forward them to the job (§Architecture 4.4) instead of dying
/// and leaving the job unsupervised. Handlers reset to default across exec,
/// so the job starts with default dispositions.
fn install_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: a zeroed sigaction is valid; the handler only stores to an
        // atomic, which is async-signal-safe.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = libc::SA_RESTART;
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

fn take_signal() -> Option<i32> {
    match SIGNAL.swap(0, Ordering::SeqCst) {
        0 => None,
        s => Some(s),
    }
}

/// Stamps the holder's `seen_ms` every [`HEARTBEAT`] (ran_s_lag_followup).
/// Best effort: a failed write only makes a later prune's ran_s shorter.
struct Heartbeat<'a> {
    store: &'a Store,
    ticket: u64,
    next: Instant,
}

impl<'a> Heartbeat<'a> {
    /// Admission stamped `seen_ms`, so the first beat is one period on.
    fn new(store: &'a Store, ticket: u64) -> Self {
        Heartbeat {
            store,
            ticket,
            next: Instant::now() + HEARTBEAT,
        }
    }

    fn beat(&mut self) {
        if Instant::now() < self.next {
            return;
        }
        self.next = Instant::now() + HEARTBEAT;
        let _ = self.store.update(now_ms(), |s| {
            let now = now_ms();
            if let Some(h) = s.holder_mut(self.ticket) {
                h.seen_ms = Some(now);
            }
            Ok(())
        });
    }
}

/// `admit`'s view of processes, answered from `/proc` or libproc.
struct ProcLive;

impl Liveness for ProcLive {
    fn alive(&self, pid: u32, start: u64) -> bool {
        proc::is_alive(pid as Pid, start)
    }
    fn group_alive(&self, pgid: u32) -> bool {
        // An unreadable process table keeps the claim: freeing slots a live
        // job still uses is the worse error.
        proc::group_members(pgid as Pid).map_or(true, |m| !m.is_empty())
    }
    fn scope_alive(&self, unit: &str, probe: Option<&ScopeProbe>) -> bool {
        scope_populated(unit, probe)
    }
}

/// D16a: the scope unit still has processes, answered as `probe` (written
/// at spawn) says. Never from systemctl: inside a container, or on a host
/// without a user bus, it prints nothing and exits 0 or 1, and that read
/// as "empty" and freed a live 12-slot claim (rv_cg_scope_probe_fails_open).
/// An unanswerable question keeps the claim.
pub(crate) fn scope_populated(unit: &str, probe: Option<&ScopeProbe>) -> bool {
    scope_answer(
        unit,
        probe,
        Here {
            cgroup_ns: proc::namespace("cgroup"),
            pid_ns: proc::namespace("pid"),
        },
        proc::cgroup_populated,
        proc::cgroup_has_process,
    )
    .unwrap_or(true)
}

/// The namespaces a waiter reads from.
#[derive(Clone, Copy)]
struct Here {
    cgroup_ns: Option<u64>,
    pid_ns: Option<u64>,
}

/// The decision behind [`scope_populated`], with the reads passed in.
/// - Same cgroup namespace and a recorded path: that path's `cgroup.events`
///   (a missing directory is empty: systemd removes an emptied scope).
/// - Same pid namespace (distrobox and toolbox share the host's): a `/proc`
///   scan for a process in a cgroup named for the unit. Also how a holder
///   is answered before its path is recorded, a few ms after spawn.
/// - A holder written before probes existed: the scan, as best effort.
/// - Otherwise the waiter can't see the scope: `None`, so it keeps the
///   claim. That bounds itself: the scope ends when conmon exits, and the
///   next waiter that can see it (any host run, `muzzle status`) prunes it.
fn scope_answer(
    unit: &str,
    probe: Option<&ScopeProbe>,
    here: Here,
    populated: impl Fn(&str) -> Option<bool>,
    has_process: impl Fn(&str) -> Option<bool>,
) -> Option<bool> {
    let same = |a: Option<u64>, b: Option<u64>| a.is_some() && a == b;
    let Some(p) = probe else {
        return has_process(unit);
    };
    if let Some(path) = p
        .cgroup
        .as_deref()
        .filter(|_| same(p.cgroup_ns, here.cgroup_ns))
    {
        return populated(path);
    }
    if same(p.pid_ns, here.pid_ns) {
        return has_process(unit);
    }
    None
}

/// For stopping a leftover scope: empty only when known to be.
fn scope_known_empty(unit: &str) -> bool {
    proc::cgroup_has_process(unit) == Some(false)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn to_class(c: cli::Class) -> Class {
    match c {
        cli::Class::High => Class::High,
        cli::Class::Normal => Class::Normal,
    }
}

fn class_str(c: Class) -> &'static str {
    match c {
        Class::High => "high",
        Class::Normal => "normal",
    }
}

/// D12: `--class`, then `MUZZLE_CLASS`, then the rule's class, then
/// `normal` for mu-managed agents and `high` for everyone else.
fn resolve_class(
    flag: Option<cli::Class>,
    env: Option<&str>,
    rule: Option<cli::Class>,
    managed: bool,
) -> Result<Class, String> {
    if let Some(c) = flag {
        return Ok(to_class(c));
    }
    match env.map(str::trim).filter(|s| !s.is_empty()) {
        Some("high") => return Ok(Class::High),
        Some("normal") => return Ok(Class::Normal),
        Some(other) => return Err(format!("MUZZLE_CLASS={other:?}: use high or normal")),
        None => {}
    }
    Ok(match (rule, managed) {
        (Some(c), _) => to_class(c),
        (None, true) => Class::Normal,
        (None, false) => Class::High,
    })
}

/// The ticket in `MUZZLE_HOLD=<ticket>:<pid>:<start>`. Tickets are never
/// reused, so the ticket alone names the claim; pid and start are kept for
/// people reading the environment, and stop matching after a keeper
/// handoff anyway.
fn hold_ticket(token: &str) -> Option<u64> {
    let mut parts = token.trim().split(':');
    let ticket = parts.next()?.parse().ok()?;
    let _pid: u32 = parts.next()?.parse().ok()?;
    let _start: u64 = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some(ticket)
}

/// D17: this run is inside a live claim, either through ancestry (an
/// ancestor is a live holder's muzzle or is in its group) or through a
/// `MUZZLE_HOLD` token naming a live holder. Checked against live state, so
/// a stale or leaked token does nothing.
fn is_nested(
    state: &State,
    ancestors: &[proc::Ancestor],
    hold: Option<&str>,
    live: &impl Liveness,
) -> bool {
    let token = hold.and_then(hold_ticket);
    state.holders.iter().any(|h| {
        let named = token == Some(h.ticket)
            || ancestors.iter().any(|a| {
                (a.pid as u32 == h.pid && a.start == h.start) || h.pgid == Some(a.pgid as u32)
            });
        named && admit::holds(h, live)
    })
}

/// D7, D9: this process's own reading of the budget; D17a decides whether
/// it or the stored one is used.
fn reading(config: &Config) -> anyhow::Result<Budget> {
    let cores = proc::logical_cores().context("counting cores")?.max(1) as u32;
    let ram = proc::total_ram().context("reading total RAM")?;
    Ok(Budget {
        budget: cores.saturating_sub(config.reserve),
        cores,
        ram_per_slot: ram / u64::from(cores),
    })
}

fn bad_usage(msg: impl std::fmt::Display) -> anyhow::Result<ExitCode> {
    eprintln!("muzzle: {msg}");
    Ok(ExitCode::from(exit::USAGE))
}

fn seconds(flag: &str, v: Option<f64>) -> Result<Option<Duration>, String> {
    match v {
        None => Ok(None),
        Some(s) if s.is_finite() && s >= 0.0 => Ok(Some(Duration::from_secs_f64(s))),
        Some(s) => Err(format!("{flag} {s}: must be a number of seconds")),
    }
}

pub fn main(args: &RunArgs) -> anyhow::Result<ExitCode> {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("muzzle: {e}");
            return Ok(ExitCode::from(match e {
                ConfigError::Read { .. } => exit::INTERNAL,
                _ => exit::USAGE,
            }));
        }
    };
    // B3: the rule as resolved at this cwd, else its first definition
    // anywhere (disabled here, or in a table that doesn't apply): a
    // pre-wrapped `--rule` is trusted for its class, as in v1.
    let rule = args.rule.as_deref().and_then(|n| {
        let here = std::env::current_dir()
            .ok()
            .and_then(|cwd| config.resolve(&cwd).rule(n).map(|m| m.rule.clone()));
        here.or_else(|| config.rule_anywhere(n))
    });
    // rv_skill_unknown_rule: a renamed rule (rust-gate -> cargo-test) would
    // otherwise quietly under-claim every hand-wrapped call that names it.
    if let (Some(name), None) = (args.rule.as_deref(), &rule) {
        let n = args.slots.unwrap_or(1);
        eprintln!("muzzle: rule {name} is not in the config; claiming {n} slot(s)");
    }
    let class = match resolve_class(
        args.class,
        std::env::var("MUZZLE_CLASS").ok().as_deref(),
        rule.as_ref().and_then(|r| r.class),
        std::env::var("MU_MANAGED_AGENT").is_ok_and(|v| v == "1"),
    ) {
        Ok(c) => c,
        Err(m) => return bad_usage(m),
    };
    let (queue_timeout, run_timeout) = match (
        seconds("--timeout", args.timeout),
        seconds("--run-timeout", args.run_timeout),
    ) {
        (Ok(q), Ok(r)) => (q, r),
        (Err(m), _) | (_, Err(m)) => return bad_usage(m),
    };

    if let Err(e) = find_program(&args.command[0]) {
        return Ok(ExitCode::from(spawn_failed(&args.command[0], &e)));
    }

    let store = Store::open_default()?;
    install_handlers();
    let me = std::process::id();
    let my_start = proc::start_time(me as Pid).context("reading muzzle's own start time")?;

    let hold = std::env::var("MUZZLE_HOLD").ok();
    if is_nested(
        &store.read()?,
        &proc::ancestors(me as Pid),
        hold.as_deref(),
        &ProcLive,
    ) {
        return run_nested(args, run_timeout);
    }

    let reading = reading(&config)?;
    // D16: a group leader runs the job in its own group, so a harness that
    // kills the group (pi on Esc) kills the job too.
    // SAFETY: plain syscall.
    let leader = unsafe { libc::getpgrp() } == me as Pid;
    let cwd = std::env::current_dir().ok();
    let claim = Claim {
        slots: args.slots.or(rule.map(|r| r.slots)).unwrap_or(1),
        class,
        agent: std::env::var("MU_AGENT_NAME")
            .ok()
            .filter(|s| !s.is_empty()),
        rule: args.rule.clone(),
        cwd: cwd
            .as_ref()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default(),
        repo: repo_str(cwd.as_deref()),
        cfg: Some(config.hash()),
    };
    let enqueued = Instant::now();
    let ticket = store.update(now_ms(), |s| {
        s.resolve_budget(reading, config.slots);
        Ok(s.enqueue(me, my_start, leader.then_some(me), claim, now_ms()))
    })?;
    let q = Queue {
        store: &store,
        ticket,
        reading,
        config_slots: config.slots,
    };
    let (holder, budget) = match q.wait(enqueued, queue_timeout)? {
        Turn::Admitted(h, b) => (*h, b),
        Turn::Gave(code) => return Ok(ExitCode::from(code)),
    };

    let mut job = Job {
        store: &store,
        ticket,
        me,
        my_start,
        holder,
        budget,
        leader,
        spawned: false,
    };
    match job.run(args, run_timeout) {
        Ok(code) => Ok(ExitCode::from(code)),
        Err(e) => {
            // Before spawn the slots go back. After it, the entry may be a
            // keeper's or the job may still run: leave it to liveness, so a
            // waiter prunes it once this muzzle and its group are gone (D16,
            // f-keeper-handoff).
            if !job.spawned {
                let _ = store.update(now_ms(), |s| Ok(s.remove(ticket)));
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::state::Holder;

    #[derive(Default)]
    struct Table {
        procs: HashSet<(u32, u64)>,
        groups: HashSet<u32>,
    }

    impl Liveness for Table {
        fn alive(&self, pid: u32, start: u64) -> bool {
            self.procs.contains(&(pid, start))
        }
        fn group_alive(&self, pgid: u32) -> bool {
            self.groups.contains(&pgid)
        }
        fn scope_alive(&self, _unit: &str, _probe: Option<&ScopeProbe>) -> bool {
            false
        }
    }

    pub(super) fn holder(ticket: u64, pid: u32, start: u64, pgid: Option<u32>) -> Holder {
        Holder {
            ticket,
            pid,
            start,
            pgid,
            run_start_ms: 0,
            waited_s: 0.0,
            enforced: false,
            kept: false,
            enforced_reason: None,
            scope: None,
            probe: None,
            admit_load1: None,
            admit_held: None,
            seen_ms: None,
            claim: Claim {
                slots: 2,
                class: Class::Normal,
                agent: Some("worker-7".into()),
                rule: Some("gate".into()),
                cwd: "/w".into(),
                repo: None,
                cfg: None,
            },
        }
    }

    fn ancestor(pid: Pid, pgid: Pid, start: u64) -> proc::Ancestor {
        proc::Ancestor { pid, pgid, start }
    }

    fn one_holder() -> (State, Table) {
        let mut s = State::default();
        s.holders.push(holder(7, 100, 55, Some(200)));
        let mut live = Table::default();
        live.procs.insert((100, 55));
        live.groups.insert(200);
        (s, live)
    }

    fn probe(cgroup: Option<&str>, cgroup_ns: u64, pid_ns: u64) -> ScopeProbe {
        ScopeProbe {
            cgroup: cgroup.map(String::from),
            cgroup_ns: Some(cgroup_ns),
            pid_ns: Some(pid_ns),
        }
    }

    const HOST: Here = Here {
        cgroup_ns: Some(1),
        pid_ns: Some(2),
    };

    /// rv_cg_scope_probe_fails_open: no answer is never "empty".
    #[test]
    fn a_waiter_that_cannot_see_the_scope_keeps_the_claim() {
        let p = probe(Some("/app.slice/u.scope"), 1, 2);
        let nothing = |_: &str| None;
        let empty = |_: &str| Some(false);
        // A container with its own cgroup and pid namespaces: no read.
        let other = Here {
            cgroup_ns: Some(9),
            pid_ns: Some(8),
        };
        assert_eq!(scope_answer("u.scope", Some(&p), other, empty, empty), None);
        // A platform that reads no namespaces at all.
        let blind = Here {
            cgroup_ns: None,
            pid_ns: None,
        };
        assert_eq!(scope_answer("u.scope", Some(&p), blind, empty, empty), None);
        // The reads themselves can't answer (no cgroup v2, /proc unreadable).
        assert_eq!(
            scope_answer("u.scope", Some(&p), HOST, nothing, empty),
            None
        );
        let distrobox = Here {
            cgroup_ns: Some(9),
            pid_ns: Some(2),
        };
        assert_eq!(
            scope_answer("u.scope", Some(&p), distrobox, empty, nothing),
            None
        );
        // And None keeps the claim.
        assert!(scope_answer("u.scope", Some(&p), other, empty, empty).unwrap_or(true));
    }

    #[test]
    fn the_recorded_path_answers_in_its_namespace_and_proc_elsewhere() {
        let p = probe(Some("/app.slice/u.scope"), 1, 2);
        let populated = |path: &str| Some(path == "/app.slice/u.scope");
        let scan = |leaf: &str| Some(leaf == "v.scope");
        // On the host: cgroup.events of the recorded path, not a scan.
        assert_eq!(
            scope_answer("u.scope", Some(&p), HOST, populated, scan),
            Some(true)
        );
        // Inside distrobox (own cgroup ns, host pid ns): the scan.
        let distrobox = Here {
            cgroup_ns: Some(9),
            pid_ns: Some(2),
        };
        assert_eq!(
            scope_answer("u.scope", Some(&p), distrobox, populated, scan),
            Some(false)
        );
        assert_eq!(
            scope_answer("v.scope", Some(&p), distrobox, populated, scan),
            Some(true)
        );
        // Path not recorded yet: the scan, on the host too.
        let early = probe(None, 1, 2);
        assert_eq!(
            scope_answer("v.scope", Some(&early), HOST, populated, scan),
            Some(true)
        );
        // A holder from before probes existed: best effort, the scan.
        assert_eq!(
            scope_answer("v.scope", None, HOST, populated, scan),
            Some(true)
        );
    }

    #[test]
    fn nested_through_the_holders_muzzle() {
        let (s, live) = one_holder();
        assert!(is_nested(&s, &[ancestor(100, 1, 55)], None, &live));
        // Same pid, different start: a recycled pid is not the holder.
        assert!(!is_nested(&s, &[ancestor(100, 1, 56)], None, &live));
    }

    #[test]
    fn nested_through_the_holders_group() {
        let (s, live) = one_holder();
        assert!(is_nested(
            &s,
            &[ancestor(300, 300, 1), ancestor(301, 200, 1)],
            None,
            &live
        ));
        assert!(!is_nested(&s, &[ancestor(301, 201, 1)], None, &live));
    }

    #[test]
    fn nested_through_muzzle_hold_only_when_the_holder_lives() {
        let (s, mut live) = one_holder();
        assert!(is_nested(&s, &[], Some("7:100:55"), &live));
        assert!(!is_nested(&s, &[], Some("8:100:55"), &live));
        assert!(!is_nested(&s, &[], Some("garbage"), &live));
        // Muzzle dead but its group lives: still a live claim (D16).
        live.procs.clear();
        assert!(is_nested(&s, &[], Some("7:100:55"), &live));
        // Both gone: a stale token does nothing.
        live.groups.clear();
        assert!(!is_nested(&s, &[], Some("7:100:55"), &live));
        assert!(!is_nested(&s, &[ancestor(100, 200, 55)], None, &live));
    }

    #[test]
    fn hold_token_shape() {
        assert_eq!(hold_ticket("12:345:678"), Some(12));
        assert_eq!(hold_ticket("12:345"), None);
        assert_eq!(hold_ticket("12:345:678:9"), None);
        assert_eq!(hold_ticket("x:1:2"), None);
    }

    #[test]
    fn class_precedence() {
        use cli::Class as C;
        let r = |f, e, rule, m| resolve_class(f, e, rule, m).unwrap();
        assert_eq!(
            r(Some(C::Normal), Some("high"), Some(C::High), false),
            Class::Normal
        );
        assert_eq!(r(None, Some("high"), Some(C::Normal), true), Class::High);
        assert_eq!(r(None, Some("normal"), None, false), Class::Normal);
        assert_eq!(r(None, Some(""), Some(C::High), true), Class::High);
        assert_eq!(r(None, None, Some(C::Normal), false), Class::Normal);
        assert_eq!(r(None, None, None, true), Class::Normal);
        assert_eq!(r(None, None, None, false), Class::High);
        assert!(resolve_class(None, Some("urgent"), None, false).is_err());
    }
}
