//! Unit `admit` (spec §Architecture 1, D8, D10, D11): the pure function
//! `(state, now, liveness) -> (pruned state, admitted tickets)`. No I/O.
//!
//! Every live waiter calls this under the state lock on each poll, so the
//! queue keeps moving with no muzzle alive between invocations (D16): the
//! pruning here is the only garbage collection there is.

use serde::{Deserialize, Serialize};

use crate::state::{Holder, Killed, RunRecord, ScopeProbe, State, Waiter};

/// D10. Declaration order is queue order: `High` sorts first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    High,
    Normal,
}

/// What `admit` needs to know about processes, answered by `proc` in
/// production and by a table in tests.
pub trait Liveness {
    /// The process with this pid and start time is running. A recycled pid
    /// has a different start time and so counts as dead.
    fn alive(&self, pid: u32, start: u64) -> bool;
    /// The group has at least one live, non-zombie member.
    fn group_alive(&self, pgid: u32) -> bool;
    /// The systemd scope unit (`muzzle-….scope`) still has processes, as
    /// `probe` (recorded at spawn) says to look. Unanswerable is `true`:
    /// freeing slots a live container job uses is the worse error (D16a).
    fn scope_alive(&self, unit: &str, probe: Option<&ScopeProbe>) -> bool;
}

/// The holder's claim is still in use: its muzzle (or keeper) lives, its
/// group has members (D16), or, for a container run, its scope still holds
/// processes (D16a). A container job runs under conmon in its own session,
/// so a group kill leaves it running with only the scope to show for it.
pub fn holds(h: &Holder, live: &impl Liveness) -> bool {
    live.alive(h.pid, h.start)
        || h.pgid.is_some_and(|g| live.group_alive(g))
        || h.scope
            .as_deref()
            .is_some_and(|u| live.scope_alive(u, h.probe.as_ref()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub state: State,
    /// Tickets moved from waiter to holder by this call, in admission order.
    pub admitted: Vec<u64>,
    /// `killed: "holder"` lines for holders pruned here; the caller appends
    /// them to `runs.jsonl` (spec §Architecture, Pruning).
    pub records: Vec<RunRecord>,
}

/// D8: a request is at least one slot and at most the whole budget, so an
/// oversized job runs alone instead of waiting forever.
pub fn cap(slots: u32, budget: u32) -> u32 {
    slots.clamp(1, budget.max(1))
}

/// Waiters in admission order: high before normal (D10), then by ticket
/// (D11). Position `i` here is queue position `i + 1` for stderr/status.
pub fn queue(state: &State) -> Vec<&Waiter> {
    let mut waiters: Vec<&Waiter> = state.waiters.iter().collect();
    waiters.sort_by_key(|w| (w.claim.class, w.ticket));
    waiters
}

/// Prunes dead entries, then admits from the head of the queue while it
/// fits. Uses the budget stored in the state (D17a); with none stored yet it
/// prunes but admits nothing, since the caller resolves the budget first.
pub fn admit(mut state: State, now_ms: u64, live: &impl Liveness) -> Outcome {
    // Pruning: a waiter whose process is gone, recycled pid included.
    state.waiters.retain(|w| live.alive(w.pid, w.start));

    // Pruning: a holder goes once its muzzle (or keeper) is dead *and* its
    // group is empty (and, for a container run, its scope too: D16a). A
    // dead muzzle with a live group keeps its slots: the job is still using
    // the machine (D16).
    // D7 gives cores − reserve, which is 0 on a 2-core host or a small
    // container (D17a). Floor it once here so `cap` and `free` agree and a
    // job still runs alone (D8) instead of every waiter blocking forever.
    let budget = state.budget.map(|b| b.budget.max(1));
    let (kept, dead): (Vec<Holder>, Vec<Holder>) = std::mem::take(&mut state.holders)
        .into_iter()
        .partition(|h| holds(h, live));
    state.holders = kept;
    // ran_s_lag_followup: a holder still in use now is seen now, so the
    // record of a later prune ends here rather than at that prune.
    for h in &mut state.holders {
        h.seen_ms = Some(now_ms);
    }
    let records = dead
        .into_iter()
        .map(|h| holder_record(h, now_ms, budget))
        .collect();

    let mut admitted = Vec::new();
    if let Some(budget) = budget {
        // A budget lowered while slots are held can leave held > budget;
        // nothing is admitted until enough is released.
        let mut free = budget.saturating_sub(state.held());
        // D11: strict FIFO. Stop at the first head that doesn't fit, so a
        // big job is never starved by a stream of small ones.
        for waiter in queue(&state) {
            let slots = cap(waiter.claim.slots, budget);
            if slots > free {
                break;
            }
            free -= slots;
            admitted.push(waiter.ticket);
        }
        for &ticket in &admitted {
            let at = state.waiters.iter().position(|w| w.ticket == ticket);
            let waiter = state.waiters.remove(at.expect("admitted from waiters"));
            state.holders.push(into_holder(waiter, now_ms, budget));
        }
    }

    Outcome {
        state,
        admitted,
        records,
    }
}

fn into_holder(w: Waiter, now_ms: u64, budget: u32) -> Holder {
    let mut claim = w.claim;
    claim.slots = cap(claim.slots, budget);
    Holder {
        ticket: w.ticket,
        pid: w.pid,
        start: w.start,
        pgid: w.pgid,
        run_start_ms: now_ms,
        waited_s: now_ms.saturating_sub(w.enqueued_ms) as f64 / 1000.0,
        enforced: false,
        kept: false,
        enforced_reason: None,
        scope: None,
        probe: None,
        admit_load1: None,
        admit_held: None,
        seen_ms: Some(now_ms),
        claim,
    }
}

/// The muzzle or keeper was killed and a waiter pruned the entry: no exit
/// status and no CPU figures, since nobody was left to collect them.
///
/// The run ends when the claim was last seen in use (`seen_ms`), never
/// after this prune (ran_s_lag_followup: smoke_final recorded 75 s for a
/// 20 s job when the end was the prune). The true end lies between the two,
/// so ran_s is short by at most the gap between sightings: one poll (250
/// ms) while anyone waits, else the owner's heartbeat (`run::HEARTBEAT`).
/// A muzzle SIGKILLed with its job left running has no heartbeat, so with
/// no waiter either the gap runs to the prune. A state file from before
/// `seen_ms` ends at the prune, as before. These lines carry `killed:
/// "holder"`, so `report` never counts them as clean (D30): they feed the
/// machine figures (D41) but not run p50/p90 or suggestions.
fn holder_record(h: Holder, now_ms: u64, budget: Option<u32>) -> RunRecord {
    let end_ms = h
        .seen_ms
        .map_or(now_ms, |s| s.clamp(h.run_start_ms, now_ms));
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
        ran_s: end_ms.saturating_sub(h.run_start_ms) as f64 / 1000.0,
        exit: None,
        killed: Some(Killed::Holder),
        cpu_s: None,
        mem_peak_mb: None,
        enforced: h.enforced,
        nested: false,
        kept: h.kept,
        budget,
        enforced_reason: h.enforced_reason,
        load1: h.admit_load1,
        held: h.admit_held,
    }
}

/// `runs.jsonl`'s `end`: UTC, whole seconds.
pub fn rfc3339(ms: u64) -> String {
    let secs = i64::try_from(ms / 1000).unwrap_or(i64::MAX);
    jiff::Timestamp::from_second(secs)
        .unwrap_or(jiff::Timestamp::MAX)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::state::{Budget, Claim};

    /// Live (pid, start) pairs and groups with live members.
    #[derive(Default)]
    struct Table {
        procs: HashSet<(u32, u64)>,
        groups: HashSet<u32>,
        scopes: HashSet<String>,
    }

    impl Liveness for Table {
        fn alive(&self, pid: u32, start: u64) -> bool {
            self.procs.contains(&(pid, start))
        }
        fn group_alive(&self, pgid: u32) -> bool {
            self.groups.contains(&pgid)
        }
        fn scope_alive(&self, unit: &str, _probe: Option<&ScopeProbe>) -> bool {
            self.scopes.contains(unit)
        }
    }

    /// Start time of every process `push` creates. Not 1, so code that
    /// passes a constant start instead of the recorded one fails the tests.
    const START: u64 = 4_242;

    /// Every pid in `state` alive with its recorded start, and its group too.
    fn all_alive(state: &State) -> Table {
        let mut t = Table::default();
        for w in &state.waiters {
            t.procs.insert((w.pid, w.start));
        }
        for h in &state.holders {
            t.procs.insert((h.pid, h.start));
            t.groups.extend(h.pgid);
        }
        t
    }

    fn state(budget: u32) -> State {
        State {
            budget: Some(Budget {
                budget,
                cores: budget + 2,
                ram_per_slot: 1 << 30,
            }),
            ..State::default()
        }
    }

    fn claim(slots: u32, class: Class) -> Claim {
        Claim {
            slots,
            class,
            agent: None,
            rule: Some("gate".into()),
            cwd: "/w".into(),
            repo: None,
            cfg: None,
        }
    }

    /// Enqueue with pid = ticket-to-be, so tests can name processes.
    fn push(s: &mut State, slots: u32, class: Class, now_ms: u64) -> u64 {
        let pid = s.next_ticket as u32 + 100;
        s.enqueue(pid, START, Some(pid), claim(slots, class), now_ms)
    }

    fn run(s: State, now_ms: u64) -> Outcome {
        let live = all_alive(&s);
        admit(s, now_ms, &live)
    }

    fn holder_tickets(s: &State) -> Vec<u64> {
        s.holders.iter().map(|h| h.ticket).collect()
    }

    #[test]
    fn fifo_order_within_a_class() {
        let mut s = state(2);
        let a = push(&mut s, 1, Class::Normal, 0);
        let b = push(&mut s, 1, Class::Normal, 0);
        let c = push(&mut s, 1, Class::Normal, 0);
        let out = run(s, 10);
        assert_eq!(out.admitted, vec![a, b]);
        assert_eq!(queue(&out.state)[0].ticket, c);
    }

    #[test]
    fn high_goes_ahead_of_normal() {
        let mut s = state(2);
        let n = push(&mut s, 2, Class::Normal, 0);
        let h = push(&mut s, 2, Class::High, 0);
        let order: Vec<u64> = queue(&s).iter().map(|w| w.ticket).collect();
        assert_eq!(order, vec![h, n]);
        let out = run(s, 10);
        assert_eq!(out.admitted, vec![h]);
    }

    #[test]
    fn high_never_preempts_a_running_job() {
        let mut s = state(2);
        let n = push(&mut s, 2, Class::Normal, 0);
        let s = run(s, 1).state;
        let mut s = s;
        push(&mut s, 1, Class::High, 2);
        let out = run(s, 3);
        assert!(out.admitted.is_empty());
        assert_eq!(holder_tickets(&out.state), vec![n]);
    }

    #[test]
    fn nothing_jumps_ahead_of_a_head_that_does_not_fit() {
        let mut s = state(4);
        let running = push(&mut s, 2, Class::Normal, 0);
        let mut s = run(s, 1).state;
        let big = push(&mut s, 4, Class::Normal, 2);
        push(&mut s, 1, Class::Normal, 3);
        push(&mut s, 1, Class::Normal, 4);
        // Two slots are free and the small ones would fit; they wait anyway.
        let out = run(s, 5);
        assert!(out.admitted.is_empty());
        // Once the runner leaves, the big head goes first.
        let mut s = out.state;
        s.remove(running);
        let out = run(s, 6);
        assert_eq!(out.admitted, vec![big]);
    }

    #[test]
    fn requests_above_the_budget_are_capped_and_run_alone() {
        let mut s = state(4);
        let huge = push(&mut s, 999, Class::Normal, 0);
        push(&mut s, 1, Class::Normal, 0);
        let out = run(s, 1);
        assert_eq!(out.admitted, vec![huge]);
        assert_eq!(out.state.holders[0].claim.slots, 4);
        assert_eq!(out.state.held(), 4);
        assert_eq!(cap(0, 4), 1);
    }

    #[test]
    fn budget_zero_or_one_runs_one_job_at_a_time() {
        for b in [0, 1] {
            let mut s = state(b);
            let first = push(&mut s, 1, Class::Normal, 0);
            let second = push(&mut s, 3, Class::Normal, 0);
            let out = run(s, 1);
            assert_eq!(out.admitted, vec![first], "budget {b}");
            assert_eq!(out.state.held(), 1, "budget {b}");
            let mut s = out.state;
            s.holders.clear();
            let out = run(s, 2);
            assert_eq!(out.admitted, vec![second], "budget {b}");
            assert_eq!(out.state.holders[0].claim.slots, 1, "budget {b}");
        }
    }

    #[test]
    fn admitted_holder_records_wait_and_start() {
        let mut s = state(4);
        push(&mut s, 2, Class::Normal, 1_000);
        let out = run(s, 3_500);
        let h = &out.state.holders[0];
        assert_eq!(h.run_start_ms, 3_500);
        assert_eq!(h.waited_s, 2.5);
        assert!(out.state.waiters.is_empty());
    }

    #[test]
    fn dead_waiters_are_pruned_and_the_queue_advances() {
        let mut s = state(2);
        let head = push(&mut s, 2, Class::Normal, 0);
        let mut s = run(s, 1).state;
        let dead = push(&mut s, 2, Class::Normal, 2);
        let next = push(&mut s, 2, Class::Normal, 3);
        s.remove(head);
        let mut live = all_alive(&s);
        live.procs.remove(&(dead as u32 + 100, START));
        let out = admit(s, 4, &live);
        assert_eq!(out.admitted, vec![next]);
        assert!(out.state.waiters.is_empty());
        assert!(out.records.is_empty(), "waiters never get run records");
    }

    #[test]
    fn a_recycled_pid_counts_as_dead() {
        let mut s = state(1);
        let t = push(&mut s, 1, Class::Normal, 0);
        let pid = t as u32 + 100;
        let mut live = Table::default();
        live.procs.insert((pid, START + 1)); // same pid, different start time
        let out = admit(s, 1, &live);
        assert!(out.state.waiters.is_empty());
        assert!(out.admitted.is_empty());
    }

    #[test]
    fn a_holder_with_a_recycled_pid_and_empty_group_is_pruned() {
        let mut s = state(2);
        let t = push(&mut s, 2, Class::Normal, 0);
        let s = run(s, 1).state;
        let pid = t as u32 + 100;
        let mut live = Table::default();
        live.procs.insert((pid, START + 1)); // the muzzle's pid, reused
        let out = admit(s, 2, &live);
        assert!(out.state.holders.is_empty());
        assert_eq!(out.records.len(), 1);
        assert_eq!(out.records[0].killed, Some(Killed::Holder));
    }

    #[test]
    fn dead_holder_with_empty_group_is_pruned_with_a_holder_record() {
        let mut s = state(2);
        let t = push(&mut s, 2, Class::High, 0);
        let mut s = run(s, 1_000).state;
        let w = push(&mut s, 2, Class::Normal, 2_000);
        // Its heartbeat saw it in use until just before the prune.
        s.holders[0].seen_ms = Some(61_000);
        // pi's group kill: the muzzle and the whole group are gone.
        let mut live = all_alive(&s);
        live.procs.remove(&(t as u32 + 100, START));
        live.groups.remove(&(t as u32 + 100));
        let out = admit(s, 61_000, &live);
        assert_eq!(out.admitted, vec![w]);
        assert_eq!(out.records.len(), 1);
        let r = &out.records[0];
        assert_eq!(r.killed, Some(Killed::Holder));
        assert_eq!((r.slots, r.class), (2, Class::High));
        assert_eq!(r.ran_s, 60.0);
        assert_eq!(r.exit, None);
        assert_eq!((r.cpu_s, r.mem_peak_mb), (None, None));
        assert_eq!(r.budget, Some(2));
        assert_eq!(r.end, "1970-01-01T00:01:01Z");
        assert_eq!((r.load1, r.held), (None, None));
    }

    #[test]
    fn a_pruned_holder_ends_when_it_was_last_seen_not_at_the_prune() {
        // ran_s_lag_followup: admitted at 1 s, last seen in use at 21 s,
        // pruned by a waiter that turned up at 76 s.
        let mut s = state(2);
        let t = push(&mut s, 2, Class::Normal, 0);
        let s = run(s, 1_000).state;
        assert_eq!(s.holders[0].seen_ms, Some(1_000), "admission is a sighting");
        let mut s = run(s, 21_000).state;
        assert_eq!(
            s.holders[0].seen_ms,
            Some(21_000),
            "a pass that keeps it sees it"
        );
        // The heartbeat goes past the last pass.
        s.holders[0].seen_ms = Some(21_500);
        let mut live = all_alive(&s);
        live.procs.remove(&(t as u32 + 100, START));
        live.groups.clear();
        let out = admit(s, 76_000, &live);
        let r = &out.records[0];
        assert_eq!(r.ran_s, 20.5);
        assert_eq!(r.end, "1970-01-01T00:00:21Z");
        assert_eq!(r.killed, Some(Killed::Holder));
    }

    #[test]
    fn a_holder_from_an_older_state_file_still_prunes_at_the_prune_time() {
        let json = r#"{"v":1,"next_ticket":2,"holders":[{"ticket":1,"pid":5,
            "start":6,"pgid":5,"run_start_ms":1000,"waited_s":0.0,"slots":2,
            "class":"normal","cwd":"/w"}]}"#;
        let s: State = serde_json::from_str(json).unwrap();
        assert_eq!(s.holders[0].seen_ms, None);
        let out = admit(s, 61_000, &Table::default());
        assert!(out.state.holders.is_empty());
        assert_eq!(out.records[0].ran_s, 60.0);
    }

    #[test]
    fn a_seen_time_outside_the_run_is_clamped() {
        let mut s = state(2);
        let t = push(&mut s, 2, Class::Normal, 0);
        let mut s = run(s, 10_000).state;
        let mut live = all_alive(&s);
        live.procs.remove(&(t as u32 + 100, START));
        live.groups.clear();
        // A clock stepped back, or a hand edit: never a negative run.
        s.holders[0].seen_ms = Some(5_000);
        assert_eq!(admit(s.clone(), 20_000, &live).records[0].ran_s, 0.0);
        // Never past the prune.
        s.holders[0].seen_ms = Some(99_000);
        assert_eq!(admit(s, 20_000, &live).records[0].ran_s, 10.0);
    }

    #[test]
    fn a_pruned_holder_keeps_its_admission_load() {
        let mut s = state(2);
        let t = push(&mut s, 2, Class::High, 0);
        let mut s = run(s, 1_000).state;
        s.holders[0].admit_load1 = Some(3.5);
        s.holders[0].admit_held = Some(0);
        let mut live = all_alive(&s);
        live.procs.remove(&(t as u32 + 100, START));
        live.groups.remove(&(t as u32 + 100));
        let out = admit(s, 61_000, &live);
        assert_eq!(
            (out.records[0].load1, out.records[0].held),
            (Some(3.5), Some(0))
        );
    }

    #[test]
    fn dead_muzzle_with_live_group_keeps_its_slots_until_the_group_empties() {
        let mut s = state(2);
        let t = push(&mut s, 2, Class::Normal, 0);
        let mut s = run(s, 1).state;
        let w = push(&mut s, 1, Class::Normal, 2);
        let mut live = all_alive(&s);
        live.procs.remove(&(t as u32 + 100, START)); // muzzle SIGKILLed, job lives
        let out = admit(s, 3, &live);
        assert!(out.admitted.is_empty());
        assert!(out.records.is_empty());
        assert_eq!(holder_tickets(&out.state), vec![t]);
        // The group exits: pruned, recorded, next admitted.
        live.groups.remove(&(t as u32 + 100));
        let out = admit(out.state, 4, &live);
        assert_eq!(out.admitted, vec![w]);
        assert_eq!(out.records.len(), 1);
    }

    #[test]
    fn container_run_keeps_its_slots_until_the_scope_empties() {
        // D16a: pi's group kill took muzzle and its group, but conmon and
        // the in-container job live on in the scope.
        let mut s = state(2);
        let t = push(&mut s, 2, Class::Normal, 0);
        let mut s = run(s, 1).state;
        s.holders[0].scope = Some("muzzle-1-2-3.scope".into());
        s.holders[0].enforced_reason = Some(crate::state::EnforcedReason::Container);
        let w = push(&mut s, 1, Class::Normal, 2);
        let mut live = all_alive(&s);
        live.procs.remove(&(t as u32 + 100, START));
        live.groups.clear();
        live.scopes.insert("muzzle-1-2-3.scope".into());
        let out = admit(s, 3, &live);
        assert!(out.admitted.is_empty());
        assert!(out.records.is_empty());
        assert_eq!(holder_tickets(&out.state), vec![t]);
        // The job ends, conmon exits, the scope empties: pruned like a dead
        // keeper, with the reason carried into the record.
        live.scopes.clear();
        let out = admit(out.state, 4, &live);
        assert_eq!(out.admitted, vec![w]);
        assert_eq!(out.records.len(), 1);
        assert_eq!(out.records[0].killed, Some(Killed::Holder));
        assert_eq!(
            out.records[0].enforced_reason,
            Some(crate::state::EnforcedReason::Container)
        );
    }

    #[test]
    fn a_live_scope_does_not_hold_a_non_container_run() {
        // Only a holder that names its scope is held by it.
        let mut s = state(2);
        let t = push(&mut s, 2, Class::Normal, 0);
        let s = run(s, 1).state;
        let mut live = Table::default();
        live.scopes.insert(format!("muzzle-{}.scope", t));
        let out = admit(s, 2, &live);
        assert!(out.state.holders.is_empty());
    }

    #[test]
    fn live_muzzle_keeps_its_slots_with_an_empty_group() {
        // Between admission and spawn there is no group yet.
        let mut s = state(2);
        let t = push(&mut s, 2, Class::Normal, 0);
        let mut s = run(s, 1).state;
        s.holders[0].pgid = None;
        let out = run(s, 2);
        assert_eq!(holder_tickets(&out.state), vec![t]);
    }

    #[test]
    fn live_keeper_keeps_its_slots_after_the_group_empties() {
        // D16: the keeper sits outside the job's group and releases the
        // slots itself (writing the real run record), so an empty group with
        // a live keeper must not be pruned or recorded here.
        let mut s = state(2);
        let t = push(&mut s, 2, Class::Normal, 0);
        let mut s = run(s, 1).state;
        let w = push(&mut s, 1, Class::Normal, 2);
        s.holders[0].pid = 9_999;
        s.holders[0].start = 7;
        s.holders[0].kept = true;
        assert!(s.holders[0].pgid.is_some());
        let mut live = all_alive(&s);
        live.groups.clear();
        let out = admit(s, 3, &live);
        assert_eq!(holder_tickets(&out.state), vec![t]);
        assert!(out.records.is_empty());
        assert!(out.admitted.is_empty());
        assert_eq!(queue(&out.state)[0].ticket, w);
    }

    #[test]
    fn dead_keeper_is_pruned_like_any_holder() {
        let mut s = state(2);
        push(&mut s, 2, Class::Normal, 0);
        let mut s = run(s, 1).state;
        s.holders[0].pid = 9_999;
        s.holders[0].start = 7;
        s.holders[0].kept = true;
        let mut live = all_alive(&s);
        live.procs.remove(&(9_999, 7));
        live.groups.clear();
        let out = admit(s, 2, &live);
        assert!(out.state.holders.is_empty());
        assert!(out.records[0].kept);
    }

    #[test]
    fn a_budget_change_while_held() {
        let mut s = state(4);
        let a = push(&mut s, 3, Class::Normal, 0);
        let mut s = run(s, 1).state;
        // Lowered to 2 while 3 are held: the holder keeps its 3, nothing
        // else is admitted until it leaves.
        s.budget.as_mut().unwrap().budget = 2;
        let b = push(&mut s, 1, Class::Normal, 2);
        let out = run(s, 3);
        assert!(out.admitted.is_empty());
        assert_eq!(out.state.holders[0].claim.slots, 3);
        let mut s = out.state;
        s.remove(a);
        let out = run(s, 4);
        assert_eq!(out.admitted, vec![b]);
        // Raised: more fits at once.
        let mut s = out.state;
        s.budget.as_mut().unwrap().budget = 8;
        let c = push(&mut s, 7, Class::Normal, 5);
        assert_eq!(run(s, 6).admitted, vec![c]);
    }

    #[test]
    fn no_stored_budget_admits_nothing_but_still_prunes() {
        let mut s = State::default();
        push(&mut s, 1, Class::Normal, 0);
        let alive = push(&mut s, 1, Class::Normal, 0);
        let mut live = all_alive(&s);
        live.procs.remove(&(101, START));
        let out = admit(s, 1, &live);
        assert!(out.admitted.is_empty());
        let left: Vec<u64> = out.state.waiters.iter().map(|w| w.ticket).collect();
        assert_eq!(left, vec![alive]);
    }

    #[test]
    fn held_never_exceeds_budget_over_a_mixed_run() {
        // A deterministic churn: random-ish sizes and classes, holders
        // leaving in ticket order, checked after every step.
        let mut s = state(4);
        let mut seed = 7u64;
        let mut done = 0;
        for step in 0..200u64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            if step < 60 {
                let class = if seed >> 63 == 0 {
                    Class::High
                } else {
                    Class::Normal
                };
                push(&mut s, (seed >> 33) as u32 % 6 + 1, class, step);
            }
            if step % 3 == 0 && !s.holders.is_empty() {
                let first = s.holders[0].ticket;
                s.remove(first);
                done += 1;
            }
            s = run(s, step).state;
            assert!(s.held() <= 4, "held {} at step {step}", s.held());
        }
        assert_eq!(done + s.holders.len() as u64, 60);
        assert!(s.waiters.is_empty());
    }
}
