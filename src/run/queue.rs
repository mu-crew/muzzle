//! Enqueue and poll: the admission loop, its stderr lines.

use std::time::{Duration, Instant};

use anyhow::bail;

use super::osc::{Sink, queued_msg};
use super::report::{overlap_record, round3};
use super::scope::{reset_failed, scope_loaded, stop_unit, unit_name};
use super::{POLL, PROGRESS, ProcLive, TICK, now_ms, same_caller, scope_known_empty, take_signal};
use crate::admit::{self, AdmitConfig, Liveness};
use crate::exit;
use crate::proc::{self, Pid};
use crate::state::{Budget, Claim, Holder, State, Store};

pub(super) struct Queue<'a> {
    pub(super) store: &'a Store,
    pub(super) ticket: u64,
    pub(super) reading: Budget,
    pub(super) config_slots: Option<u32>,
    /// G1: the orphan thresholds handed to `admit_with`.
    pub(super) cfg: AdmitConfig,
    /// This run's own claim, for G2's same-caller test and its run record.
    pub(super) claim: Claim,
    /// G2 opt-out: `--allow-concurrent` or `allow_concurrent = true`.
    pub(super) allow_concurrent: bool,
    /// Program status while queued (OSC 7501), cleared when the wait ends.
    pub(super) status: std::cell::RefCell<Sink>,
}

pub(super) enum Turn {
    Admitted(Box<Holder>, Budget),
    /// Gave up while queued; exit with this code.
    Gave(u8),
}

pub(super) enum Poll {
    Admitted(Box<Holder>, Budget),
    Waiting {
        position: usize,
        of: usize,
        line: String,
        /// The program status message, from the same locked read.
        msg: String,
    },
    /// G2: blocked by the caller's own orphaned claim; the waiter is gone.
    Refused,
}

impl Queue<'_> {
    /// One locked `admit` pass: prune, admit, append any `killed: "holder"`
    /// lines, and report where this ticket stands.
    ///
    /// `first` is the t=0 pass, where G2 warns about and may refuse on the
    /// caller's own claims.
    fn poll(&self, first: bool) -> anyhow::Result<Poll> {
        let mut pruned = Vec::new();
        let mut notes = Vec::new();
        let mut refused = None;
        // f-append-wedge, f-dup-prune-line: the pruned holders' lines are
        // appended after the save, and a failed append only warns.
        let poll = self.store.update_logged(now_ms(), |s| {
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
            let out = admit::admit_with(std::mem::take(s), now, &ProcLive, self.cfg);
            *s = out.state;
            pruned = scopes
                .into_iter()
                .filter(|(t, _)| !s.holders.iter().any(|h| h.ticket == *t))
                .map(|(_, unit)| format!("{unit}.scope"))
                .collect();
            let mut records = out.records;
            let b = budget.budget.max(1);
            if first {
                let (warning, refusal) = self.same_caller_check(s, now, b);
                notes.extend(warning);
                if let Some(msg) = refusal {
                    s.remove(self.ticket);
                    records.push(overlap_record(&self.claim, Some(b)));
                    refused = Some(msg);
                    return Ok((Poll::Refused, records));
                }
            }
            if let Some(h) = stamp_admission(s, self.ticket, || proc::loadavg().map(|l| l[0])) {
                return Ok((Poll::Admitted(Box::new(h.clone()), budget), records));
            }
            let queue = admit::queue(s);
            let Some(at) = queue.iter().position(|w| w.ticket == self.ticket) else {
                // Only a waiter whose pid reads as dead is pruned, and this
                // one is running; a hand-edited state file is the only way.
                bail!("ticket {} vanished from the queue", self.ticket);
            };
            let waiting = Poll::Waiting {
                position: at + 1,
                of: queue.len(),
                line: progress_line(s, at + 1, queue.len(), &ProcLive),
                msg: queued_msg(
                    at + 1,
                    queue.len(),
                    admit::cap(queue[at].claim.slots, b),
                    s.held(),
                    b,
                ),
            };
            Ok((waiting, records))
        });
        // A group SIGKILLed while systemd-run was still setting up the scope
        // (pi on Esc) leaves it `failed` with Result=resources, and nobody
        // but the pruner is left to reset it (2/10 on linuxpc, systemd 259).
        // The same race can instead leave it `active running` with an empty
        // cgroup: its only PID died before systemd watched it, so no empty
        // notification ever comes (3-4/10 on linuxpc, systemd 259; 75 such
        // units had piled up). Stop it, but only if empty: a populated scope
        // holds a job that outlived its group.
        for note in &notes {
            eprintln!("{note}");
        }
        if let Some(msg) = &refused {
            self.status.borrow_mut().clear();
            eprintln!("{msg}");
        }
        for unit in &pruned {
            if scope_loaded(unit) && scope_known_empty(unit) {
                stop_unit(unit);
            }
            reset_failed(unit);
        }
        poll
    }

    pub(super) fn wait(
        &self,
        enqueued: Instant,
        timeout: Option<Duration>,
    ) -> anyhow::Result<Turn> {
        let mut polls = 0u32;
        let mut next_progress = enqueued + PROGRESS;
        // Reported only when it changes: a poll every 250 ms must not mean a
        // terminal write every 250 ms.
        let mut last_msg = String::new();
        loop {
            polls += 1;
            let (position, of) = match self.poll(polls == 1)? {
                Poll::Admitted(h, budget) => {
                    self.status.borrow_mut().clear();
                    if polls > 1 {
                        eprintln!(
                            "muzzle: admitted after {:.0}s ({} slots)",
                            enqueued.elapsed().as_secs_f64(),
                            h.claim.slots
                        );
                    }
                    return Ok(Turn::Admitted(h, budget));
                }
                Poll::Refused => return Ok(Turn::Gave(exit::SELF_OVERLAP)),
                Poll::Waiting {
                    position,
                    of,
                    line,
                    msg,
                } => {
                    if msg != last_msg {
                        self.status.borrow_mut().queued(&msg);
                        last_msg = msg;
                    }
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
                    // G5: say who holds the slots, read before the ticket goes.
                    let holders = self
                        .store
                        .read()
                        .ok()
                        .map(|s| holder_list(&s, usize::MAX, &ProcLive));
                    self.leave()?;
                    eprintln!(
                        "muzzle: gave up after {:.0}s queued (position {position}/{of}, --timeout)",
                        enqueued.elapsed().as_secs_f64()
                    );
                    if let Some(h) = holders.filter(|h| !h.is_empty()) {
                        eprintln!("muzzle: held by {h}");
                    }
                    return Ok(Turn::Gave(exit::QUEUE_TIMEOUT));
                }
                std::thread::sleep(TICK);
            }
        }
    }

    /// G2, under the lock after `admit`: the t=0 warning when the same
    /// caller already holds claims, and the refusal text when this run was
    /// not admitted and the caller's own orphaned claims are part of what
    /// blocks it (`need > free && need <= free + own_orphan_held`).
    fn same_caller_check(
        &self,
        s: &State,
        now: u64,
        budget: u32,
    ) -> (Option<String>, Option<String>) {
        let mine: Vec<&Holder> = s
            .holders
            .iter()
            .filter(|h| h.ticket != self.ticket && same_caller(&h.claim, &self.claim))
            .collect();
        if mine.is_empty() {
            return (None, None);
        }
        let slots: u32 = mine.iter().map(|h| h.claim.slots).sum();
        let who = match (&self.claim.pane, &self.claim.agent) {
            (Some(p), Some(a)) => format!("same pane ({p} / {a})"),
            (Some(p), None) => format!("same pane ({p})"),
            (None, a) => format!("same agent ({})", a.as_deref().unwrap_or("?")),
        };
        let details: Vec<String> = mine
            .iter()
            .map(|h| {
                format!(
                    "ticket {}, {} slots, {}",
                    h.ticket,
                    h.claim.slots,
                    holder_state(h, now, &ProcLive)
                )
            })
            .collect();
        let warning = format!(
            "muzzle: warning: {who} already holds {slots} slots ({})",
            details.join("; ")
        );
        let admitted = s.holders.iter().any(|h| h.ticket == self.ticket);
        let orphans: Vec<&&Holder> = mine
            .iter()
            .filter(|h| admit::is_orphaned(h, &ProcLive))
            .collect();
        let own: u32 = orphans.iter().map(|h| h.claim.slots).sum();
        let need = admit::cap(self.claim.slots, budget);
        let free = budget.saturating_sub(s.held());
        if admitted || self.allow_concurrent || own == 0 || need <= free || need > free + own {
            return (Some(warning), None);
        }
        let named: Vec<String> = orphans
            .iter()
            .map(|h| {
                format!(
                    "ticket {}, {} slots, {}",
                    h.ticket,
                    h.claim.slots,
                    holder_state(h, now, &ProcLive)
                )
            })
            .collect();
        let tickets: Vec<String> = orphans.iter().map(|h| h.ticket.to_string()).collect();
        let refusal = format!(
            "muzzle: refusing to queue behind own orphaned claim ({}); run `muzzle drop {}` \
             (or `muzzle drop --kill {}`) or pass --allow-concurrent",
            named.join("; "),
            tickets.join(" "),
            tickets.join(" ")
        );
        (Some(warning), Some(refusal))
    }

    /// Drop the ticket, whether still waiting or admitted since the last
    /// poll: either way this muzzle won't run the job.
    fn leave(&self) -> anyhow::Result<()> {
        self.status.borrow_mut().clear();
        self.store.update(now_ms(), |s| Ok(s.remove(self.ticket)))?;
        Ok(())
    }
}

/// P12: the first poll that finds `ticket` admitted stamps `held` (slots
/// the other holders have, in the state under the lock) and `load1` into
/// its entry. Another waiter's pass may have admitted it, so this is done
/// by the admitted muzzle, not in `admit` (which does no I/O). In the entry,
/// a pruned holder's or a keeper's line carries them too.
pub(super) fn stamp_admission(
    s: &mut State,
    ticket: u64,
    load1: impl FnOnce() -> Option<f64>,
) -> Option<&Holder> {
    let all = s.held();
    let h = s.holder_mut(ticket)?;
    if h.admit_held.is_none() {
        h.admit_held = Some(all - h.claim.slots);
        h.admit_load1 = load1().map(round3);
    }
    Some(h)
}

/// D19's message. Its ticket is not held for it.
pub(super) fn killed_while_queued(position: usize, of: usize, waited: Duration) -> String {
    format!(
        "muzzle: killed while queued (position {position}/{of}, waited {:.0}s). \
         Put timeout inside the command: muzzle run -- timeout N …",
        waited.as_secs_f64()
    )
}

/// G2, G3: `live`, `kept` or `dead-muzzle`, how long it has been so, and for
/// an orphan up to 3 surviving group members as `comm(pid)`.
fn holder_state(h: &Holder, now: u64, live: &impl Liveness) -> String {
    let orphan = admit::is_orphaned(h, live);
    let tag = match (h.kept, orphan) {
        (true, _) => "kept",
        (false, true) => "dead-muzzle",
        (false, false) => "live",
    };
    let since = if orphan {
        h.orphan_since_ms.unwrap_or(h.run_start_ms)
    } else {
        h.run_start_ms
    };
    let mut out = format!("{tag} {}s", now.saturating_sub(since) / 1000);
    if let (true, Some(g)) = (orphan, h.pgid) {
        let procs: Vec<String> = proc::group_procs(g as Pid)
            .into_iter()
            .take(3)
            .map(|(p, c)| format!("{c}({p})"))
            .collect();
        out.push_str(&format!(", pgid {g}"));
        if !procs.is_empty() {
            out.push_str(&format!(": {}", procs.join(", ")));
        }
    }
    out
}

/// `<agent> <rule> <slots>` per holder, `?` and `-` for a missing agent or
/// rule, tagged `[kept]` or `[dead]` when orphaned (G3). Shows `shown`, then `…`.
fn holder_list(s: &State, shown: usize, live: &impl Liveness) -> String {
    let mut held: Vec<String> = s
        .holders
        .iter()
        .take(shown)
        .map(|h| {
            let tag = match (h.kept, admit::is_orphaned(h, live)) {
                (true, _) => " [kept]",
                (false, true) => " [dead]",
                (false, false) => "",
            };
            format!(
                "{} {} {}{tag}",
                h.claim.agent.as_deref().unwrap_or("?"),
                h.claim.rule.as_deref().unwrap_or("-"),
                h.claim.slots
            )
        })
        .collect();
    if s.holders.len() > shown {
        held.push("…".into());
    }
    held.join(", ")
}

/// Spec §stderr lines: `muzzle: queued 3/5, 14/14 slots held (worker-7 gate
/// 6, orch cargo-test 2, …)`, with G3's `[dead]`/`[kept]` tags.
pub(super) fn progress_line(s: &State, position: usize, of: usize, live: &impl Liveness) -> String {
    let budget = s.budget.map_or(0, |b| b.budget.max(1));
    format!(
        "muzzle: queued {position}/{of}, {}/{budget} slots held ({})",
        s.held(),
        holder_list(s, 4, live)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::holder;
    use crate::state::Budget;

    struct Alive;
    impl Liveness for Alive {
        fn alive(&self, _: u32, _: u64) -> bool {
            true
        }
        fn group_alive(&self, _: u32) -> bool {
            true
        }
        fn scope_alive(&self, _: &str, _: Option<&crate::state::ScopeProbe>) -> bool {
            true
        }
    }

    #[test]
    fn progress_line_tags_orphans() {
        let mut s = State::default();
        let mut a = holder(1, 1, 1, None);
        a.kept = true;
        s.holders = vec![a, holder(2, 2, 2, None)];
        assert_eq!(
            progress_line(&s, 1, 1, &Alive),
            "muzzle: queued 1/1, 4/0 slots held (worker-7 gate 2 [kept], worker-7 gate 2)"
        );
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
            progress_line(&s, 3, 5, &Alive),
            "muzzle: queued 3/5, 8/14 slots held (worker-7 gate 6, ? cargo-test 2)"
        );
        for t in 3..8 {
            s.holders.push(holder(t, t as u32, 1, None));
        }
        assert!(progress_line(&s, 1, 1, &Alive).ends_with(", …)"));
    }

    #[test]
    fn admission_stamps_held_by_others_and_load_once() {
        let mut s = State::default();
        let mut other = holder(1, 1, 1, None);
        other.claim.slots = 5;
        s.holders = vec![other, holder(2, 2, 2, None)];
        let h = stamp_admission(&mut s, 2, || Some(7.123_45)).unwrap();
        assert_eq!((h.admit_held, h.admit_load1), (Some(5), Some(7.123)));
        // A later poll keeps the admission-time figures.
        s.holders[0].claim.slots = 1;
        let h = stamp_admission(&mut s, 2, || Some(1.0)).unwrap();
        assert_eq!((h.admit_held, h.admit_load1), (Some(5), Some(7.123)));
        // No load average: held is still recorded.
        let h = stamp_admission(&mut s, 1, || None).unwrap();
        assert_eq!((h.admit_held, h.admit_load1), (Some(2), None));
        assert!(stamp_admission(&mut s, 9, || Some(1.0)).is_none());
    }

    #[test]
    fn killed_while_queued_text() {
        assert_eq!(
            killed_while_queued(2, 4, Duration::from_millis(12_400)),
            "muzzle: killed while queued (position 2/4, waited 12s). \
             Put timeout inside the command: muzzle run -- timeout N …"
        );
    }
}
