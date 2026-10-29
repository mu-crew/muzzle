//! Unit `run` (spec §Architecture 4, D13-D19, D27, D30a): nesting, enqueue
//! and poll, spawn (in a systemd scope when reachable), signals, the keeper
//! handoff, release, `runs.jsonl`, `--report`, exit codes and stderr lines.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use serde_json::json;

use crate::admit::{self, Class, Liveness};
use crate::cli::{self, RunArgs};
use crate::exit;
use crate::proc::{self, CgroupFigures, Pid, StartTime};
use crate::rules::config::{Config, ConfigError};
use crate::state::{Budget, Claim, Holder, Killed, RunRecord, State, Store};

/// Spec §Architecture 4.2: each waiter polls `admit` every 250 ms.
const POLL: Duration = Duration::from_millis(250);
/// How often a running job is checked for exit, signals and its deadline.
/// Every run's end is late by up to this much, so it stays short.
const TICK: Duration = Duration::from_millis(20);
/// D13: one progress line on stderr every 30 s while queued.
const PROGRESS: Duration = Duration::from_secs(30);
/// D18: SIGTERM, then SIGKILL this much later.
const KILL_GRACE: Duration = Duration::from_secs(10);
/// D27: `runs.jsonl` lines older than this are pruned.
const RUNS_TTL_MS: u64 = 30 * 24 * 3600 * 1000;
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
}

fn holder_live(h: &Holder, live: &impl Liveness) -> bool {
    live.alive(h.pid, h.start) || h.pgid.is_some_and(|g| live.group_alive(g))
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
        named && holder_live(h, live)
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
    let rule = args.rule.as_deref().and_then(|n| config.rule(n));
    let class = match resolve_class(
        args.class,
        std::env::var("MUZZLE_CLASS").ok().as_deref(),
        rule.and_then(|r| r.class),
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
    let claim = Claim {
        slots: args.slots.or(rule.map(|r| r.slots)).unwrap_or(1),
        class,
        agent: std::env::var("MU_AGENT_NAME")
            .ok()
            .filter(|s| !s.is_empty()),
        rule: args.rule.clone(),
        cwd: std::env::current_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default(),
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
    };
    match job.run(args, run_timeout) {
        Ok(code) => Ok(ExitCode::from(code)),
        Err(e) => {
            // Whatever failed, the slots go back.
            let _ = store.update(now_ms(), |s| Ok(s.remove(ticket)));
            Err(e)
        }
    }
}

struct Queue<'a> {
    store: &'a Store,
    ticket: u64,
    reading: Budget,
    config_slots: Option<u32>,
}

enum Turn {
    Admitted(Box<Holder>, Budget),
    /// Gave up while queued; exit with this code.
    Gave(u8),
}

enum Poll {
    Admitted(Box<Holder>, Budget),
    Waiting {
        position: usize,
        of: usize,
        line: String,
    },
}

impl Queue<'_> {
    /// One locked `admit` pass: prune, admit, append any `killed: "holder"`
    /// lines, and report where this ticket stands.
    fn poll(&self) -> anyhow::Result<Poll> {
        let mut pruned = Vec::new();
        let poll = self.store.update(now_ms(), |s| {
            // Read under the lock: an admission's `run_start_ms` then never
            // precedes the release that freed its slots, and `--report`'s
            // `admitted_ms` can be compared with another run's `released_ms`.
            let now = now_ms();
            let budget = s.resolve_budget(self.reading, self.config_slots);
            // A holder's scope is named for its muzzle; after a handoff the
            // keeper resets it itself. `enforced` isn't checked: it is set
            // just after spawn, and a kill in that window is the case here.
            let scopes: Vec<(u64, String)> = s
                .holders
                .iter()
                .filter(|h| !h.kept && proc::systemd_user_reachable())
                .map(|h| (h.ticket, unit_name(h.pid, h.start, h.ticket)))
                .collect();
            let out = admit::admit(std::mem::take(s), now, &ProcLive);
            *s = out.state;
            pruned = scopes
                .into_iter()
                .filter(|(t, _)| !s.holders.iter().any(|h| h.ticket == *t))
                .map(|(_, unit)| format!("{unit}.scope"))
                .collect();
            append_runs(self.store.dir(), &out.records, now)?;
            if let Some(h) = s.holders.iter().find(|h| h.ticket == self.ticket) {
                return Ok(Poll::Admitted(Box::new(h.clone()), budget));
            }
            let queue = admit::queue(s);
            let Some(at) = queue.iter().position(|w| w.ticket == self.ticket) else {
                // Only a waiter whose pid reads as dead is pruned, and this
                // one is running; a hand-edited state file is the only way.
                bail!("ticket {} vanished from the queue", self.ticket);
            };
            Ok(Poll::Waiting {
                position: at + 1,
                of: queue.len(),
                line: progress_line(s, at + 1, queue.len()),
            })
        });
        // A group SIGKILLed while systemd-run was still setting up the scope
        // (pi on Esc) leaves it `failed` with Result=resources, and nobody
        // but the pruner is left to reset it (2/10 on linuxpc, systemd 259).
        for unit in &pruned {
            reset_failed(unit);
        }
        poll
    }

    fn wait(&self, enqueued: Instant, timeout: Option<Duration>) -> anyhow::Result<Turn> {
        let mut polls = 0u32;
        let mut next_progress = enqueued + PROGRESS;
        loop {
            polls += 1;
            let (position, of) = match self.poll()? {
                Poll::Admitted(h, budget) => {
                    if polls > 1 {
                        eprintln!(
                            "muzzle: admitted after {:.0}s ({} slots)",
                            enqueued.elapsed().as_secs_f64(),
                            h.claim.slots
                        );
                    }
                    return Ok(Turn::Admitted(h, budget));
                }
                Poll::Waiting { position, of, line } => {
                    if Instant::now() >= next_progress {
                        eprintln!("{line}");
                        next_progress += PROGRESS;
                    }
                    (position, of)
                }
            };
            let next_poll = Instant::now() + POLL;
            while Instant::now() < next_poll {
                if let Some(sig) = take_signal() {
                    self.leave()?;
                    eprintln!("{}", killed_while_queued(position, of, enqueued.elapsed()));
                    return Ok(Turn::Gave(128u8.saturating_add(sig as u8)));
                }
                if timeout.is_some_and(|t| enqueued.elapsed() >= t) {
                    self.leave()?;
                    eprintln!(
                        "muzzle: gave up after {:.0}s queued (position {position}/{of}, --timeout)",
                        enqueued.elapsed().as_secs_f64()
                    );
                    return Ok(Turn::Gave(exit::QUEUE_TIMEOUT));
                }
                std::thread::sleep(TICK);
            }
        }
    }

    /// Drop the ticket, whether still waiting or admitted since the last
    /// poll: either way this muzzle won't run the job.
    fn leave(&self) -> anyhow::Result<()> {
        self.store.update(now_ms(), |s| Ok(s.remove(self.ticket)))?;
        Ok(())
    }
}

/// D19's message. Its ticket is not held for it.
fn killed_while_queued(position: usize, of: usize, waited: Duration) -> String {
    format!(
        "muzzle: killed while queued (position {position}/{of}, waited {:.0}s). \
         Put timeout inside the command: muzzle run -- timeout N …",
        waited.as_secs_f64()
    )
}

/// Spec §stderr lines: `muzzle: queued 3/5, 14/14 slots held (worker-7 gate
/// 6, orch cargo-test 2, …)`. Each holder reads `<agent> <rule> <slots>`,
/// with `?` and `-` for a missing agent or rule.
fn progress_line(s: &State, position: usize, of: usize) -> String {
    const SHOWN: usize = 4;
    let budget = s.budget.map_or(0, |b| b.budget.max(1));
    let mut held: Vec<String> = s
        .holders
        .iter()
        .take(SHOWN)
        .map(|h| {
            format!(
                "{} {} {}",
                h.claim.agent.as_deref().unwrap_or("?"),
                h.claim.rule.as_deref().unwrap_or("-"),
                h.claim.slots
            )
        })
        .collect();
    if s.holders.len() > SHOWN {
        held.push("…".into());
    }
    format!(
        "muzzle: queued {position}/{of}, {}/{budget} slots held ({})",
        s.held(),
        held.join(", ")
    )
}

/// Who gets a signal meant for the job.
#[derive(Clone, Copy)]
enum Target {
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
    fn others(self) -> Vec<Pid> {
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
fn exited(pid: Pid) -> bool {
    proc::start_time(pid).is_none()
}

/// The job's scope unit. `MUZZLE_UNIT_PREFIX` exists for the integration
/// tests: each test names its own scopes, so it can check that it left none
/// `failed` among other suites' units on a shared host.
fn unit_name(pid: u32, start: StartTime, ticket: u64) -> String {
    let prefix = std::env::var("MUZZLE_UNIT_PREFIX")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "muzzle".to_string());
    format!("{prefix}-{pid}-{start}-{ticket}")
}

/// Drop a scope's `failed` state; failed units stay loaded until reset.
/// Harmless on a unit that is gone or never existed.
fn reset_failed(name: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "reset-failed"])
        .arg(name)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The last figures read from the job's scope (Linux, enforced). The scope
/// is removed the moment it empties, so figures are sampled while it lives
/// (FROM proc: systemd 259 collects a finished scope at once).
struct Sampler {
    unit: String,
    dir: Option<PathBuf>,
    last: Option<CgroupFigures>,
    next: Instant,
}

impl Sampler {
    fn sample(&mut self, pid: Pid) {
        if Instant::now() < self.next {
            return;
        }
        self.next = Instant::now() + POLL;
        if self.dir.is_none() {
            // Until systemd-run has moved itself into the scope, the child
            // sits in muzzle's own cgroup, whose figures would be wrong.
            let scope = format!("{}.scope", self.unit);
            self.dir = proc::cgroup_of(pid).filter(|d| d.file_name().is_some_and(|n| *n == *scope));
        }
        if let Some(f) = self.dir.as_deref().and_then(proc::cgroup_figures) {
            self.last = Some(f);
        }
    }
}

/// `systemctl --user show` fields for a scope that's gone or going.
#[derive(Debug, Default, PartialEq)]
struct Show {
    active: String,
    result: String,
    cpu_s: Option<f64>,
    peak: Option<u64>,
}

fn parse_show(text: &str) -> Show {
    let mut s = Show::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "ActiveState" => s.active = v.to_string(),
            "Result" => s.result = v.to_string(),
            "CPUUsageNSec" => s.cpu_s = v.parse::<u64>().ok().map(|n| n as f64 / 1e9),
            "MemoryPeak" => s.peak = v.parse().ok(),
            _ => {}
        }
    }
    s
}

/// D30a: whether the scope was killed at `MemoryMax`. systemd's default
/// `OOMPolicy=stop` kills the whole scope, which removes the cgroup before a
/// sample can see `oom_kill`, but leaves the unit `failed` with
/// `Result=oom-kill` (checked on linuxpc, systemd 259). systemd marks it
/// asynchronously, so wait up to 1 s for it to settle. A clean scope is
/// collected at once and reads as `inactive`.
fn scope_outcome(unit: &str) -> Show {
    let name = format!("{unit}.scope");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let show = Command::new("systemctl")
            .args(["--user", "show", "-p"])
            .arg("ActiveState,Result,CPUUsageNSec,MemoryPeak")
            .arg(&name)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| parse_show(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default();
        let settled = !matches!(
            show.active.as_str(),
            "active" | "activating" | "deactivating" | "reloading"
        );
        if settled || Instant::now() >= deadline {
            // Don't leave one failed unit per OOM behind. A scope still
            // settling at the deadline may fail after this read, so reset it
            // too: anything not cleanly `inactive`.
            if show.active != "inactive" {
                reset_failed(&name);
            }
            return show;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn systemd_version(text: &str) -> Option<u32> {
    text.split_whitespace().nth(1)?.parse().ok()
}

fn systemd_run_expands_env() -> bool {
    Command::new("systemd-run")
        .arg("--version")
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| systemd_version(&String::from_utf8_lossy(&o.stdout)))
        .is_some_and(|v| v >= SYSTEMD_EXPAND_FLAG)
}

/// D15: CPU weight in proportion to the claim; systemd's default is 100.
fn cpu_weight(slots: u32) -> u32 {
    (100 * slots).clamp(1, 10_000)
}

fn mb(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 20)
}

fn exit_code(status: ExitStatus) -> u8 {
    match (status.code(), status.signal()) {
        (Some(c), _) => c as u8,
        (None, Some(sig)) => 128u8.saturating_add(sig as u8),
        (None, None) => exit::INTERNAL,
    }
}

/// D18: the job is past `--run-timeout`: SIGTERM, and SIGKILL after 10 s.
struct Deadline {
    after: Option<Duration>,
    termed: Option<Instant>,
    killed: bool,
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

    fn fired(&self) -> bool {
        self.termed.is_some()
    }

    /// D18: the top command is gone, but what it left behind gets the rest
    /// of `KILL_GRACE` from the SIGTERM before the SIGKILL. A `bash -c` top
    /// dies on TERM at once, and its children's TERM handlers (a gate
    /// cleaning up worktrees) would otherwise have no time at all.
    fn drain(&mut self, target: Target, mut sampler: Option<&mut Sampler>) {
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
fn supervise(
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
fn run_nested(args: &RunArgs, run_timeout: Option<Duration>) -> anyhow::Result<ExitCode> {
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
        write_report(path, &t, None, None, None, code, false, false)?;
    }
    Ok(ExitCode::from(code))
}

/// The program `CMD` names, looked up on `PATH` as exec would. Checked
/// before queueing, so a typo fails at once rather than after the wait, and
/// so the scope path reports it the same way: systemd-run exits 1 for a
/// missing program.
fn find_program(name: &str) -> std::io::Result<PathBuf> {
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
fn spawn_failed(program: &str, e: &std::io::Error) -> u8 {
    eprintln!("muzzle: {program}: {e}");
    match e.kind() {
        std::io::ErrorKind::NotFound => 127,
        _ => 126,
    }
}

/// The timing half of a `--report`.
struct Times {
    waited_s: f64,
    ran_s: f64,
    /// Epoch ms, read under the state lock: one run's `released_ms` is at
    /// most the `admitted_ms` of the run its slots went to, so the bench can
    /// count slots held without the overlap that spawn time plus `waited_s`
    /// gave (20 held of 14 on linuxpc). Null for a nested run, and
    /// `released_ms` is null when a keeper releases later (D16).
    admitted_ms: Option<u64>,
    released_ms: Option<u64>,
}

#[allow(clippy::too_many_arguments)] // one flat JSON object, spec §CLI
fn write_report(
    path: &Path,
    t: &Times,
    slots: Option<u32>,
    class: Option<Class>,
    rule: Option<&str>,
    exit: u8,
    enforced: bool,
    kept: bool,
) -> anyhow::Result<()> {
    let v = json!({
        "waited_s": round3(t.waited_s),
        "ran_s": round3(t.ran_s),
        "admitted_ms": t.admitted_ms,
        "released_ms": t.released_ms,
        "slots": slots,
        "class": class.map(class_str),
        "rule": rule,
        "exit": exit,
        "enforced": enforced,
        "kept": kept,
    });
    fs::write(path, format!("{v}\n")).with_context(|| format!("writing {}", path.display()))
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// One admitted run, from spawn to release.
struct Job<'a> {
    store: &'a Store,
    ticket: u64,
    me: u32,
    my_start: StartTime,
    holder: Holder,
    budget: Budget,
    leader: bool,
}

/// How the top command ended, before anything was released.
#[derive(Clone, Copy)]
struct Ending {
    code: u8,
    killed: Option<Killed>,
    exit: i32,
}

impl Job<'_> {
    fn run(&mut self, args: &RunArgs, run_timeout: Option<Duration>) -> anyhow::Result<u8> {
        let slots = self.holder.claim.slots;
        let enforced = proc::systemd_user_reachable();
        let unit = unit_name(self.me, self.my_start, self.ticket);
        let mut cmd = if enforced {
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
        self.store.update(now_ms(), |s| {
            if let Some(h) = s.holder_mut(self.ticket) {
                h.pgid = Some(pgid);
                h.enforced = enforced;
            }
            Ok(())
        })?;
        self.holder.pgid = Some(pgid);
        self.holder.enforced = enforced;

        let mut sampler = enforced.then(|| Sampler {
            unit: unit.clone(),
            dir: None,
            last: None,
            next: Instant::now(),
        });
        let mut deadline = Deadline {
            after: run_timeout,
            termed: None,
            killed: false,
        };
        let (status, ru) = supervise(pid, target, &mut deadline, sampler.as_mut())?;
        deadline.drain(target, sampler.as_mut());
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

        let kept = !deadline.fired() && !target.others().is_empty();
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

    fn record(
        &self,
        ran_s: f64,
        ending: &Ending,
        cpu_s: Option<f64>,
        mem_peak_mb: Option<u64>,
        kept: bool,
        budget: Option<u32>,
    ) -> RunRecord {
        let h = &self.holder;
        RunRecord {
            v: 1,
            end: admit::rfc3339(now_ms()),
            rule: h.claim.rule.clone(),
            slots: h.claim.slots,
            class: h.claim.class,
            cfg: h.claim.cfg.clone(),
            agent: h.claim.agent.clone(),
            cwd: h.claim.cwd.clone(),
            waited_s: h.waited_s,
            ran_s: round3(ran_s),
            exit: Some(ending.exit),
            killed: ending.killed,
            cpu_s: cpu_s.map(round3),
            mem_peak_mb,
            enforced: h.enforced,
            nested: false,
            kept,
            budget,
        }
    }

    /// Free the slots and append the run line, in one locked update.
    /// Returns when, read under the lock (see `Times::released_ms`).
    fn release(
        &self,
        ran_s: f64,
        ending: &Ending,
        cpu_s: Option<f64>,
        mem_peak_mb: Option<u64>,
        kept: bool,
    ) -> anyhow::Result<u64> {
        self.store.update(now_ms(), |s| {
            let now = now_ms();
            let budget = s.budget.map(|b| b.budget.max(1));
            let rec = self.record(ran_s, ending, cpu_s, mem_peak_mb, kept, budget);
            // A keeper whose entry was pruned (it was SIGSTOPped, say) has
            // already had its line written by the pruning waiter.
            if s.remove(self.ticket) || !kept {
                append_runs(self.store.dir(), &[rec], now)?;
            }
            Ok(now)
        })
    }

    /// Spec §Architecture 4.6: `--report`, or the stderr line.
    fn finish(
        &self,
        args: &RunArgs,
        ran_s: f64,
        released_ms: Option<u64>,
        ending: &Ending,
        enforced: bool,
        kept: bool,
    ) -> anyhow::Result<()> {
        let h = &self.holder;
        match &args.report {
            Some(path) => write_report(
                path,
                &Times {
                    waited_s: h.waited_s,
                    ran_s,
                    admitted_ms: Some(h.run_start_ms),
                    released_ms,
                },
                Some(h.claim.slots),
                Some(h.claim.class),
                h.claim.rule.as_deref(),
                ending.code,
                enforced,
                kept,
            ),
            None => {
                eprintln!(
                    "muzzle: waited {:.0}s for {} slots, ran {:.0}s",
                    h.waited_s, h.claim.slots, ran_s
                );
                Ok(())
            }
        }
    }

    /// D16 keeper: fork a detached process that takes over the holder entry
    /// and releases it once the job's group is empty. Returns in the parent
    /// once the keeper owns the entry, so the caller gets the top command's
    /// status at once.
    fn hand_off(
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
        loop {
            let others = target.others();
            let Some(&member) = others.first() else { break };
            if let Some(s) = sampler.as_mut() {
                // The top command is gone; find the scope through any member.
                s.sample(member);
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
fn detach_stdio() {
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

/// D27: append run lines to `runs.jsonl`, pruning lines older than 30 days
/// first. Callers hold the state lock, so the rewrite can't race an append.
pub fn append_runs(dir: &Path, records: &[RunRecord], now_ms: u64) -> anyhow::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let path = dir.join("runs.jsonl");
    let cutoff = admit::rfc3339(now_ms.saturating_sub(RUNS_TTL_MS));
    prune_runs(&path, &cutoff)?;
    let mut buf = Vec::new();
    for r in records {
        serde_json::to_writer(&mut buf, r)?;
        buf.push(b'\n');
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(&buf))
        .with_context(|| format!("appending to {}", path.display()))
}

/// A line survives pruning when its `end` is at or after `cutoff`. Both are
/// `YYYY-MM-DDTHH:MM:SSZ`, so string order is time order. A line without a
/// readable `end` is kept: pruning is for age, not for repair.
fn keep_line(line: &str, cutoff: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct End {
        end: String,
    }
    serde_json::from_str::<End>(line).map_or(true, |e| e.end.as_str() >= cutoff)
}

/// Lines are appended in time order, so only the first needs checking
/// before deciding to rewrite.
fn prune_runs(path: &Path, cutoff: &str) -> anyhow::Result<()> {
    let Ok(file) = fs::File::open(path) else {
        return Ok(());
    };
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first)?;
    if keep_line(first.trim_end(), cutoff) {
        return Ok(());
    }
    let text = fs::read_to_string(path)?;
    let kept: String = text
        .lines()
        .filter(|l| keep_line(l, cutoff))
        .flat_map(|l| [l, "\n"])
        .collect();
    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, kept)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

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
    }

    fn holder(ticket: u64, pid: u32, start: u64, pgid: Option<u32>) -> Holder {
        Holder {
            ticket,
            pid,
            start,
            pgid,
            run_start_ms: 0,
            waited_s: 0.0,
            enforced: false,
            kept: false,
            claim: Claim {
                slots: 2,
                class: Class::Normal,
                agent: Some("worker-7".into()),
                rule: Some("gate".into()),
                cwd: "/w".into(),
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

    #[test]
    fn progress_line_matches_the_spec_shape() {
        let mut s = State {
            budget: Some(Budget {
                budget: 14,
                cores: 16,
                ram_per_slot: 1,
            }),
            ..State::default()
        };
        let mut a = holder(1, 1, 1, None);
        a.claim.slots = 6;
        let mut b = holder(2, 2, 2, None);
        b.claim.agent = None;
        b.claim.rule = Some("cargo-test".into());
        s.holders = vec![a, b];
        assert_eq!(
            progress_line(&s, 3, 5),
            "muzzle: queued 3/5, 8/14 slots held (worker-7 gate 6, ? cargo-test 2)"
        );
        for t in 3..8 {
            s.holders.push(holder(t, t as u32, 1, None));
        }
        assert!(progress_line(&s, 1, 1).ends_with(", …)"));
    }

    #[test]
    fn killed_while_queued_text() {
        assert_eq!(
            killed_while_queued(2, 4, Duration::from_millis(12_400)),
            "muzzle: killed while queued (position 2/4, waited 12s). \
             Put timeout inside the command: muzzle run -- timeout N …"
        );
    }

    #[test]
    fn show_parsing() {
        let s = parse_show(
            "ActiveState=failed\nResult=oom-kill\nCPUUsageNSec=38927000\nMemoryPeak=67108864\n",
        );
        assert_eq!(s.result, "oom-kill");
        assert_eq!(s.active, "failed");
        assert_eq!(s.peak, Some(64 << 20));
        assert!((s.cpu_s.unwrap() - 0.038927).abs() < 1e-9);
        let gone = parse_show("ActiveState=inactive\nResult=success\nCPUUsageNSec=[not set]\n");
        assert_eq!((gone.cpu_s, gone.peak), (None, None));
    }

    #[test]
    fn systemd_version_gates_the_expand_flag() {
        assert_eq!(
            systemd_version("systemd 259 (259.9-1.fc44)\n+PAM +AUDIT"),
            Some(259)
        );
        assert_eq!(systemd_version("nonsense"), None);
    }

    #[test]
    fn exit_codes_pass_through_and_signals_add_128() {
        assert_eq!(exit_code(ExitStatus::from_raw(7 << 8)), 7);
        assert_eq!(exit_code(ExitStatus::from_raw(libc::SIGTERM)), 143);
        assert_eq!(exit_code(ExitStatus::from_raw(libc::SIGKILL)), 137);
    }

    #[test]
    fn weights_and_sizes() {
        assert_eq!(cpu_weight(1), 100);
        assert_eq!(cpu_weight(6), 600);
        assert_eq!(cpu_weight(1000), 10_000);
        assert_eq!(mb(1), 1);
        assert_eq!(mb(1 << 20), 1);
        assert_eq!(mb((1 << 20) + 1), 2);
    }

    #[test]
    fn old_run_lines_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.jsonl");
        let now = 100 * 24 * 3600 * 1000u64;
        let line = |end: &str| format!("{{\"v\":1,\"end\":\"{end}\"}}\n");
        let old = admit::rfc3339(now - RUNS_TTL_MS - 1000);
        let fresh = admit::rfc3339(now - RUNS_TTL_MS + 1000);
        fs::write(&path, line(&old) + &line(&fresh)).unwrap();
        let mut rec: RunRecord = serde_json::from_value(json!({
            "v":1,"end":"x","rule":null,"slots":1,"class":"high","cfg":null,"agent":null,
            "cwd":"/","waited_s":0.0,"ran_s":0.0,"exit":0,"killed":null,"cpu_s":null,
            "mem_peak_mb":null,"enforced":false,"nested":false,"kept":false,"budget":4
        }))
        .unwrap();
        rec.end = admit::rfc3339(now);
        append_runs(dir.path(), &[rec], now).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let ends: Vec<String> = text
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["end"].to_string())
            .collect();
        assert_eq!(ends.len(), 2, "{text}");
        assert!(!text.contains(&old));
        assert!(text.contains(&fresh));
        assert!(keep_line("not json", &fresh));
    }
}
