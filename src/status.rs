//! `muzzle status [--json]` (spec §Interface, `status --json`; D33).
//!
//! Read-only: it reads `state.json` under the lock and never writes it, so
//! a status poll can't race a waiter's admission. Dead entries a waiter
//! hasn't pruned yet are hidden with the same rule `admit` prunes by, so
//! the numbers match what the next poll will act on.
//!
//! The JSON is the contract mu host integration reads (D2): fields are only
//! ever added, never renamed or removed (AGENTS.md invariants).

use std::collections::HashSet;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::admit::{self, Class, Liveness};
use crate::cli::StatusArgs;
use crate::proc::{self, Pid};
use crate::rules::config::DEFAULT_RESERVE;
use crate::runlog::{self, RuleKey, Typical};
use crate::state::{Budget, EnforcedReason, State, Store};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub version: &'static str,
    pub budget: u32,
    pub cores: u32,
    pub reserve: u32,
    pub held: u32,
    pub free: u32,
    pub enforced: bool,
    pub holders: Vec<HolderView>,
    pub waiting: Vec<WaiterView>,
    /// Added fields (0.2.3 S4): how many holders and waiters are `slow`.
    pub slow_holders: usize,
    pub slow_waiters: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HolderView {
    pub slots: u32,
    pub class: Class,
    pub agent: Option<String>,
    pub rule: Option<String>,
    pub cwd: String,
    /// Added field (D27a): the repo's main checkout, or null.
    pub repo: Option<String>,
    pub run_s: u64,
    pub pid: u32,
    pub pgid: Option<u32>,
    /// Added fields (D16a): false with reason `container` when the job
    /// entered a container and the scope neither caps nor measures it.
    pub enforced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enforced_reason: Option<EnforcedReason>,
    /// Added fields (0.2.3 S2, S4): the p90 of this `(repo, rule)`'s recent
    /// clean runs, or null without enough history; and whether `run_s` is
    /// past it and past [`SLOW_HOLDER_FLOOR_S`].
    pub typical_s: Option<u64>,
    pub slow: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WaiterView {
    pub position: usize,
    pub slots: u32,
    pub class: Class,
    pub agent: Option<String>,
    pub rule: Option<String>,
    pub age_s: u64,
    /// Added field (0.2.3 S3): queued at least [`SLOW_WAITER_S`].
    pub slow: bool,
}

/// 0.2.3 S2: a holder under this is never slow, however short its history.
const SLOW_HOLDER_FLOOR_S: u64 = 60;
/// 0.2.3 S3: when `run` prints its first queued line (D13).
const SLOW_WAITER_S: u64 = 30;

/// The `(repo, rule)` of each holder that has both: what `main` asks
/// `runlog::typical` for. Dead holders may be in it; they cost a lookup.
pub fn holder_keys(state: &State) -> HashSet<RuleKey> {
    state
        .holders
        .iter()
        .filter_map(|h| Some((h.claim.repo.clone()?, h.claim.rule.clone()?)))
        .collect()
}

/// `admit`'s liveness from `/proc` or libproc. Same rule as `run`'s: an
/// unreadable process table keeps the claim.
pub(crate) struct ProcLive;

impl Liveness for ProcLive {
    fn alive(&self, pid: u32, start: u64) -> bool {
        proc::is_alive(pid as Pid, start)
    }
    fn group_alive(&self, pgid: u32) -> bool {
        proc::group_members(pgid as Pid).map_or(true, |m| !m.is_empty())
    }
    fn scope_alive(&self, unit: &str, probe: Option<&crate::state::ScopeProbe>) -> bool {
        crate::run::scope_populated(unit, probe)
    }
}

/// The status of `state` at `now_ms`. The budget is the stored one (D17a)
/// when there is one and it hasn't expired, else what the next run would store: `reading`, with
/// a config `slots =` in place of its budget.
pub fn view(
    state: &State,
    now_ms: u64,
    live: &impl Liveness,
    reading: Budget,
    config_slots: Option<u32>,
    reserve: u32,
    enforced: bool,
) -> Status {
    let mut state = state.clone();
    // Before pruning, as in `Store::update`: the next run expires on the
    // raw file, dead entries included.
    state.budget = state.live_budget(now_ms);
    let stored = state.budget.is_some();
    let b = state.resolve_budget(reading, if stored { None } else { config_slots });
    // Same floor as `admit`: a 0 budget still runs one job alone (D8).
    let budget = b.budget.max(1);
    state.waiters.retain(|w| live.alive(w.pid, w.start));
    state.holders.retain(|h| admit::holds(h, live));
    let held = state.held();
    let secs = |since_ms: u64| now_ms.saturating_sub(since_ms) / 1000;
    let holders: Vec<HolderView> = state
        .holders
        .iter()
        .map(|h| HolderView {
            slots: h.claim.slots,
            class: h.claim.class,
            agent: h.claim.agent.clone(),
            rule: h.claim.rule.clone(),
            cwd: h.claim.cwd.clone(),
            repo: h.claim.repo.clone(),
            run_s: secs(h.run_start_ms),
            pid: h.pid,
            pgid: h.pgid,
            enforced: h.enforced,
            enforced_reason: h.enforced_reason,
            typical_s: None,
            slow: false,
        })
        .collect();
    let waiting: Vec<WaiterView> = admit::queue(&state)
        .into_iter()
        .enumerate()
        .map(|(i, w)| {
            let age_s = secs(w.enqueued_ms);
            WaiterView {
                position: i + 1,
                slots: admit::cap(w.claim.slots, budget),
                class: w.claim.class,
                agent: w.claim.agent.clone(),
                rule: w.claim.rule.clone(),
                age_s,
                slow: age_s >= SLOW_WAITER_S,
            }
        })
        .collect();
    Status {
        version: env!("CARGO_PKG_VERSION"),
        budget,
        cores: b.cores,
        reserve,
        held,
        free: budget.saturating_sub(held),
        enforced,
        slow_holders: 0,
        slow_waiters: waiting.iter().filter(|w| w.slow).count(),
        holders,
        waiting,
    }
}

impl Status {
    /// 0.2.3 S2, S6: fills each holder's `typical_s` from `typical` and marks
    /// it slow past that and [`SLOW_HOLDER_FLOOR_S`]. Pure: `main` reads
    /// the history; `view` stays as it was.
    pub fn mark_slow(mut self, typical: &Typical) -> Status {
        for h in &mut self.holders {
            h.typical_s = match (&h.repo, &h.rule) {
                (Some(repo), Some(rule)) => typical.get(&(repo.clone(), rule.clone())).copied(),
                _ => None,
            };
            h.slow = h
                .typical_s
                .is_some_and(|t| h.run_s > t && h.run_s >= SLOW_HOLDER_FLOOR_S);
        }
        self.slow_holders = self.holders.iter().filter(|h| h.slow).count();
        self
    }
}

fn class_str(c: Class) -> &'static str {
    match c {
        Class::High => "high",
        Class::Normal => "normal",
    }
}

pub fn render_text(s: &Status) -> String {
    let mut out = format!(
        "budget {} slots ({} cores, reserve {}), {} held, {} free, {}\n",
        s.budget,
        s.cores,
        s.reserve,
        s.held,
        s.free,
        if s.enforced { "enforced" } else { "advisory" }
    );
    let or = |v: &Option<String>, d: &'static str| v.clone().unwrap_or_else(|| d.into());
    if s.holders.is_empty() {
        out.push_str("holders: none\n");
    } else {
        out.push_str("holders:\n");
        for h in &s.holders {
            let container = if h.enforced_reason.is_some() {
                "  (container: not enforced)"
            } else {
                ""
            };
            out.push_str(&format!(
                "  {:>3} slots  {:<6}  {}  {}  {}s{}  pid {}  {}{container}\n",
                h.slots,
                class_str(h.class),
                or(&h.agent, "?"),
                or(&h.rule, "-"),
                h.run_s,
                match (h.slow, h.typical_s) {
                    (true, Some(t)) => format!(" (slow, usually \u{2264}{t}s)"),
                    _ => String::new(),
                },
                h.pid,
                h.cwd
            ));
        }
    }
    if s.waiting.is_empty() {
        out.push_str("waiting: none\n");
    } else {
        out.push_str("waiting:\n");
        for w in &s.waiting {
            out.push_str(&format!(
                "  {:>3}. {:>3} slots  {:<6}  {}  {}  {}s{}\n",
                w.position,
                w.slots,
                class_str(w.class),
                or(&w.agent, "?"),
                or(&w.rule, "-"),
                w.age_s,
                if w.slow { " (slow)" } else { "" }
            ));
        }
    }
    out
}

/// This host's own budget reading (D7, D9), as `run` computes it.
pub(crate) fn reading(reserve: u32) -> anyhow::Result<Budget> {
    use anyhow::Context;
    let cores = proc::logical_cores().context("counting cores")?.max(1) as u32;
    let ram = proc::total_ram().context("reading total RAM")?;
    Ok(Budget {
        budget: cores.saturating_sub(reserve),
        cores,
        ram_per_slot: ram / u64::from(cores),
    })
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn main(args: &StatusArgs) -> anyhow::Result<ExitCode> {
    // A broken config must not hide the queue: fall back to the default
    // reserve, say so, and still report what state.json holds.
    let (reserve, config_slots) = match crate::doctor::read_config().1 {
        Ok(c) => (c.reserve, c.slots),
        Err(e) => {
            eprintln!("muzzle: {e} (status uses the default reserve)");
            (DEFAULT_RESERVE, None)
        }
    };
    let store = Store::open_default()?;
    let state = store.read()?;
    // 0.2.3 S5: history is best effort; without it nothing is slow.
    let typical = runlog::typical(store.dir(), &holder_keys(&state)).unwrap_or_else(|e| {
        eprintln!("muzzle: {e:#} (status shows no typical times)");
        Typical::new()
    });
    let status = view(
        &state,
        now_ms(),
        &ProcLive,
        reading(reserve)?,
        config_slots,
        reserve,
        proc::systemd_user_reachable(),
    )
    .mark_slow(&typical);
    if args.json {
        println!("{}", serde_json::to_string(&status)?);
    } else {
        print!("{}", render_text(&status));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::state::{Claim, Holder};

    struct Table {
        alive: HashSet<u32>,
        groups: HashSet<u32>,
    }
    impl Liveness for Table {
        fn alive(&self, pid: u32, _start: u64) -> bool {
            self.alive.contains(&pid)
        }
        fn group_alive(&self, pgid: u32) -> bool {
            self.groups.contains(&pgid)
        }
        fn scope_alive(&self, unit: &str, _probe: Option<&crate::state::ScopeProbe>) -> bool {
            unit == "live.scope"
        }
    }

    const READING: Budget = Budget {
        budget: 6,
        cores: 8,
        ram_per_slot: 1 << 30,
    };

    fn claim(slots: u32, class: Class, agent: &str) -> Claim {
        Claim {
            slots,
            class,
            agent: Some(agent.into()),
            rule: Some("gate".into()),
            cwd: "/w".into(),
            repo: Some("/w".into()),
            cfg: None,
        }
    }

    fn holder(ticket: u64, pid: u32, pgid: Option<u32>, slots: u32, agent: &str) -> Holder {
        Holder {
            ticket,
            pid,
            start: 1,
            pgid,
            run_start_ms: 1_000,
            waited_s: 0.0,
            enforced: true,
            kept: false,
            enforced_reason: None,
            scope: None,
            probe: None,
            admit_load1: None,
            admit_held: None,
            seen_ms: None,
            claim: claim(slots, Class::High, agent),
        }
    }

    fn state() -> State {
        let mut s = State {
            budget: Some(Budget {
                budget: 4,
                cores: 6,
                ram_per_slot: 1 << 30,
            }),
            ..State::default()
        };
        s.holders.push(holder(1, 10, Some(10), 3, "orch"));
        s.holders.push(holder(2, 11, None, 1, "dead-holder"));
        s.next_ticket = 3;
        // Normal first by ticket, high second: high must sort ahead.
        s.enqueue(20, 1, None, claim(9, Class::Normal, "worker-7"), 2_000);
        s.enqueue(21, 1, None, claim(1, Class::High, "human"), 3_000);
        s.enqueue(22, 1, None, claim(1, Class::Normal, "dead-waiter"), 3_000);
        s
    }

    fn live() -> Table {
        // 11's muzzle is dead and it has no group: hidden. 10 is dead but
        // its group lives: still holding (D16). 22 is a dead waiter.
        Table {
            alive: [20, 21].into(),
            groups: [10].into(),
        }
    }

    #[test]
    fn hides_the_dead_and_counts_the_live() {
        let s = view(&state(), 63_000, &live(), READING, None, 2, true);
        assert_eq!((s.budget, s.cores, s.reserve), (4, 6, 2));
        let agents: Vec<_> = s.holders.iter().map(|h| h.agent.clone().unwrap()).collect();
        assert_eq!(agents, ["orch"]);
        assert_eq!((s.held, s.free), (3, 1));
        assert_eq!(s.holders[0].run_s, 62);
        assert_eq!(s.holders[0].pgid, Some(10));
    }

    #[test]
    fn waiting_is_in_queue_order_and_capped() {
        let s = view(&state(), 63_000, &live(), READING, None, 2, false);
        let w: Vec<_> = s
            .waiting
            .iter()
            .map(|w| (w.position, w.agent.clone().unwrap(), w.slots, w.age_s))
            .collect();
        assert_eq!(
            w,
            [
                (1, "human".to_string(), 1, 60),
                (2, "worker-7".to_string(), 4, 61)
            ]
        );
    }

    #[test]
    fn without_a_stored_budget_uses_the_reading_and_config_slots() {
        let empty = State::default();
        let s = view(&empty, 0, &live(), READING, None, 2, false);
        assert_eq!((s.budget, s.cores, s.free), (6, 8, 6));
        let s = view(&empty, 0, &live(), READING, Some(3), 2, false);
        assert_eq!((s.budget, s.free), (3, 3));
        // A stored budget wins over config slots until the next run.
        let s = view(&state(), 0, &live(), READING, Some(3), 2, false);
        assert_eq!(s.budget, 4);
    }

    #[test]
    fn expired_stored_budget_is_not_reported() {
        // Reviewer repro (sd_expired_budget): idle and over an hour old.
        let mut old = State {
            budget: state().budget,
            updated_ms: 1_000,
            ..State::default()
        };
        let later = 1_000 + crate::state::BUDGET_TTL_MS + 1;
        let s = view(&old, later, &live(), READING, None, 2, false);
        assert_eq!((s.budget, s.cores, s.free), (6, 8, 6));
        // Dead entries still count as busy until a run prunes them, as in
        // `Store::update`, which expires before pruning.
        old.enqueue(22, 1, None, claim(1, Class::Normal, "dead-waiter"), 0);
        let s = view(&old, later, &live(), READING, None, 2, false);
        assert_eq!((s.budget, s.cores), (4, 6));
    }

    #[test]
    fn zero_budget_floors_to_one() {
        let zero = Budget {
            budget: 0,
            ..READING
        };
        let s = view(&State::default(), 0, &live(), zero, None, 8, false);
        assert_eq!((s.budget, s.free), (1, 1));
    }

    #[test]
    fn container_holders_show_why_and_stay_while_their_scope_lives() {
        let mut st = state();
        // Muzzle and group gone (as for 11), but its scope still has conmon.
        let mut h = holder(3, 12, Some(12), 2, "box");
        h.enforced = false;
        h.enforced_reason = Some(EnforcedReason::Container);
        h.scope = Some("live.scope".into());
        st.holders.push(h);
        let s = view(&st, 63_000, &live(), READING, None, 2, true);
        let agents: Vec<_> = s.holders.iter().map(|h| h.agent.clone().unwrap()).collect();
        assert_eq!(agents, ["orch", "box"]);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["holders"][0]["enforced"], true);
        assert!(v["holders"][0].get("enforced_reason").is_none());
        assert_eq!(v["holders"][1]["enforced"], false);
        assert_eq!(v["holders"][1]["enforced_reason"], "container");
        assert!(render_text(&s).contains("(container: not enforced)"));
        // Once the scope is gone, so is the holder.
        st.holders[2].scope = Some("gone.scope".into());
        let s = view(&st, 63_000, &live(), READING, None, 2, true);
        assert_eq!(s.holders.len(), 1);
    }

    #[test]
    fn json_has_every_spec_field() {
        let s = view(&state(), 63_000, &live(), READING, None, 2, true);
        let v = serde_json::to_value(&s).unwrap();
        for k in [
            "version",
            "budget",
            "cores",
            "reserve",
            "held",
            "free",
            "enforced",
            "holders",
            "waiting",
            "slow_holders",
            "slow_waiters",
        ] {
            assert!(v.get(k).is_some(), "missing {k}: {v}");
        }
        for k in [
            "slots",
            "class",
            "agent",
            "rule",
            "cwd",
            "repo",
            "run_s",
            "pid",
            "pgid",
            "typical_s",
            "slow",
        ] {
            assert!(v["holders"][0].get(k).is_some(), "holder missing {k}");
        }
        for k in [
            "position", "slots", "class", "agent", "rule", "age_s", "slow",
        ] {
            assert!(v["waiting"][0].get(k).is_some(), "waiter missing {k}");
        }
        assert_eq!(v["waiting"][0]["class"], "high");
        assert_eq!(v["holders"][0]["repo"], "/w", "D27a: copied from the claim");
    }

    /// One live holder of `gate` at `/w` (from `state()`'s orch), started at
    /// 1 s, seen at `now_ms`, with `typical` as its history.
    fn orch_at(now_ms: u64, typical: Option<u64>) -> HolderView {
        let t: Typical = typical
            .map(|t| (("/w".to_string(), "gate".to_string()), t))
            .into_iter()
            .collect();
        let s = view(&state(), now_ms, &live(), READING, None, 2, true).mark_slow(&t);
        s.holders[0].clone()
    }

    #[test]
    fn a_holder_is_slow_past_its_typical_time_and_the_floor() {
        // run_s = (now - 1000) / 1000.
        let at = |run_s: u64| 1_000 + run_s * 1_000;
        let h = orch_at(at(91), Some(90));
        assert_eq!((h.typical_s, h.slow), (Some(90), true));
        assert!(!orch_at(at(90), Some(90)).slow, "at p90 is not past it");
        // Past a short typical time but under the 60 s floor.
        assert!(!orch_at(at(59), Some(5)).slow);
        assert!(orch_at(at(60), Some(5)).slow);
        // No history: never slow, however long.
        let h = orch_at(at(10_000), None);
        assert_eq!((h.typical_s, h.slow), (None, false));
    }

    #[test]
    fn a_holder_without_a_rule_or_repo_has_no_typical_time() {
        let mut st = state();
        st.holders[0].claim.rule = None;
        // Only the dead holder still names (/w, gate).
        st.holders[1].claim.repo = None;
        assert!(holder_keys(&st).is_empty());
        let t: Typical = [(("/w".to_string(), "gate".to_string()), 1)].into();
        let s = view(&st, 1_000_000, &live(), READING, None, 2, true).mark_slow(&t);
        assert_eq!((s.holders[0].typical_s, s.holders[0].slow), (None, false));
    }

    #[test]
    fn waiters_are_slow_from_30s_and_the_counts_add_up() {
        // state(): human enqueued at 3 s, worker-7 at 2 s.
        let s = view(&state(), 32_999, &live(), READING, None, 2, true);
        let w: Vec<_> = s.waiting.iter().map(|w| (w.age_s, w.slow)).collect();
        assert_eq!(w, [(29, false), (30, true)]);
        assert_eq!((s.slow_holders, s.slow_waiters), (0, 1));
        let t: Typical = [(("/w".to_string(), "gate".to_string()), 10)].into();
        let s = view(&state(), 63_000, &live(), READING, None, 2, true).mark_slow(&t);
        assert_eq!((s.slow_holders, s.slow_waiters), (1, 2));
        let text = render_text(&s);
        assert!(text.contains("62s (slow, usually \u{2264}10s)"), "{text}");
        // S4: nothing slow, text as before: a typical time alone adds nothing.
        let t: Typical = [(("/w".to_string(), "gate".to_string()), 1000)].into();
        let s = view(&state(), 20_000, &live(), READING, None, 2, true).mark_slow(&t);
        assert_eq!((s.slow_holders, s.slow_waiters), (0, 0));
        assert!(!render_text(&s).contains("slow"), "{}", render_text(&s));
        assert!(text.contains("60s (slow)"), "{text}");
    }
}
