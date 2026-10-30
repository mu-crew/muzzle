//! Enqueue and poll: the admission loop, its stderr lines.

use std::time::{Duration, Instant};

use anyhow::bail;

use super::report::round3;
use super::scope::{reset_failed, scope_loaded, stop_unit, unit_name};
use super::{POLL, PROGRESS, ProcLive, TICK, now_ms, scope_known_empty, take_signal};
use crate::admit;
use crate::exit;
use crate::proc;
use crate::state::{Budget, Holder, State, Store};

pub(super) struct Queue<'a> {
    pub(super) store: &'a Store,
    pub(super) ticket: u64,
    pub(super) reading: Budget,
    pub(super) config_slots: Option<u32>,
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
    },
}

impl Queue<'_> {
    /// One locked `admit` pass: prune, admit, append any `killed: "holder"`
    /// lines, and report where this ticket stands.
    fn poll(&self) -> anyhow::Result<Poll> {
        let mut pruned = Vec::new();
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
            let out = admit::admit(std::mem::take(s), now, &ProcLive);
            *s = out.state;
            pruned = scopes
                .into_iter()
                .filter(|(t, _)| !s.holders.iter().any(|h| h.ticket == *t))
                .map(|(_, unit)| format!("{unit}.scope"))
                .collect();
            let records = out.records;
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
                line: progress_line(s, at + 1, queue.len()),
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

/// Spec §stderr lines: `muzzle: queued 3/5, 14/14 slots held (worker-7 gate
/// 6, orch cargo-test 2, …)`. Each holder reads `<agent> <rule> <slots>`,
/// with `?` and `-` for a missing agent or rule.
pub(super) fn progress_line(s: &State, position: usize, of: usize) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::holder;
    use crate::state::Budget;

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
