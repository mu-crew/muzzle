//! Run records (`runs.jsonl`) and `--report` output.

use std::fs;
use std::path::Path;

use anyhow::Context;
use serde_json::json;

use super::job::{Ending, Job};
use super::{class_str, now_ms};
use crate::admit::{self, Class};
use crate::cli::RunArgs;
use crate::repo;
use crate::state::{Claim, EnforcedReason, Killed, RunRecord};

/// The timing half of a `--report`.
pub(super) struct Times {
    pub(super) waited_s: f64,
    pub(super) ran_s: f64,
    /// Epoch ms, read under the state lock: one run's `released_ms` is at
    /// most the `admitted_ms` of the run its slots went to, so the bench can
    /// count slots held without the overlap that spawn time plus `waited_s`
    /// gave (20 held of 14 on linuxpc). Null for a nested run, and
    /// `released_ms` is null when a keeper releases later (D16).
    pub(super) admitted_ms: Option<u64>,
    pub(super) released_ms: Option<u64>,
}

#[allow(clippy::too_many_arguments)] // one flat JSON object, spec §CLI
pub(super) fn write_report(
    path: &Path,
    t: &Times,
    slots: Option<u32>,
    class: Option<Class>,
    rule: Option<&str>,
    repo: Option<&str>,
    exit: u8,
    enforced: bool,
    reason: Option<EnforcedReason>,
    kept: bool,
) -> anyhow::Result<()> {
    let mut v = json!({
        "waited_s": round3(t.waited_s),
        "ran_s": round3(t.ran_s),
        "admitted_ms": t.admitted_ms,
        "released_ms": t.released_ms,
        "slots": slots,
        "class": class.map(class_str),
        "rule": rule,
        "repo": repo,
        "exit": exit,
        "enforced": enforced,
        "kept": kept,
    });
    // Added only when set (D16a), as status and runs.jsonl do.
    if let Some(r) = reason {
        v["enforced_reason"] = json!(r);
    }
    fs::write(path, format!("{v}\n")).with_context(|| format!("writing {}", path.display()))
}

/// D27a: the repo `cwd` belongs to, as recorded in `runs.jsonl`, status
/// and `--report`.
pub(super) fn repo_str(cwd: Option<&Path>) -> Option<String> {
    repo::repo_of(cwd?).map(|p| p.to_string_lossy().into_owned())
}

/// What was measured of a run, for its `runs.jsonl` line.
#[derive(Clone, Copy, Default)]
pub(super) struct Measured {
    pub(super) cpu_s: Option<f64>,
    pub(super) mem_peak_mb: Option<u64>,
    /// L1: processes seen in the job's group.
    pub(super) procs: Option<u64>,
}

pub(super) fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// G2: the `runs.jsonl` line of a run refused behind its caller's own
/// orphaned claim. It never waited or ran.
pub(super) fn overlap_record(claim: &Claim, budget: Option<u32>) -> RunRecord {
    RunRecord {
        v: 1,
        end: admit::rfc3339(now_ms()),
        rule: claim.rule.clone(),
        slots: claim.slots,
        class: claim.class,
        cfg: claim.cfg.clone(),
        agent: claim.agent.clone(),
        cwd: claim.cwd.clone(),
        repo: claim.repo.clone(),
        waited_s: 0.0,
        ran_s: 0.0,
        exit: Some(i32::from(crate::exit::SELF_OVERLAP)),
        killed: Some(Killed::SelfOverlap),
        cpu_s: None,
        mem_peak_mb: None,
        enforced: false,
        nested: false,
        kept: false,
        budget,
        enforced_reason: None,
        load1: None,
        held: None,
        procs: None,
    }
}

impl Job<'_> {
    fn record(
        &self,
        ran_s: f64,
        ending: &Ending,
        m: Measured,
        kept: bool,
        budget: Option<u32>,
    ) -> RunRecord {
        let h = &self.holder;
        // D16a: the scope saw only podman exec and conmon, so its figures
        // say nothing about the job (3 s of in-container CPU read 0.21 s on
        // linuxpc). Null, as for a run nobody measured. L1: so is the
        // process count, as the container's processes aren't in the group.
        let m = match h.enforced_reason {
            Some(EnforcedReason::Container) => Measured::default(),
            None => m,
        };
        let Measured {
            cpu_s,
            mem_peak_mb,
            procs,
        } = m;
        RunRecord {
            v: 1,
            end: admit::rfc3339(now_ms()),
            rule: h.claim.rule.clone(),
            slots: h.claim.slots,
            class: h.claim.class,
            cfg: h.claim.cfg.clone(),
            agent: h.claim.agent.clone(),
            cwd: h.claim.cwd.clone(),
            repo: h.claim.repo.clone(),
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
            enforced_reason: h.enforced_reason,
            load1: h.admit_load1,
            held: h.admit_held,
            procs,
        }
    }

    /// Free the slots and append the run line, in one locked update.
    /// Returns when, read under the lock (see `Times::released_ms`). A
    /// failed append only warns, so the job keeps its exit code
    /// (f-append-wedge).
    pub(super) fn release(
        &self,
        ran_s: f64,
        ending: &Ending,
        m: Measured,
        kept: bool,
    ) -> anyhow::Result<u64> {
        let mut released = now_ms();
        self.store.update_logged_if(released, |s| {
            // Capture the release timestamp under the state lock so
            // `--report`'s `released_ms` never predates unlocking.
            released = now_ms();
            let budget = s.budget.map(|b| b.budget.max(1));
            let rec = self.record(ran_s, ending, m, kept, budget);
            // G4: an entry already pruned (a SIGSTOPped keeper) or dropped
            // (`muzzle drop --force`) had its terminal line written then,
            // and the state is left untouched.
            Ok(s.remove(self.ticket).then(|| vec![rec]))
        })?;
        Ok(released)
    }

    /// Spec §Architecture 4.6: `--report`, or the stderr line.
    pub(super) fn finish(
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
                h.claim.repo.as_deref(),
                ending.code,
                enforced,
                h.enforced_reason,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::holder;
    use crate::state::{Budget, Store};

    #[test]
    fn container_records_carry_load1_and_held() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut h = holder(3, 1, 1, None);
        h.enforced_reason = Some(EnforcedReason::Container);
        h.admit_load1 = Some(9.5);
        h.admit_held = Some(4);
        let job = Job {
            store: &store,
            ticket: 3,
            me: 1,
            my_start: 1,
            holder: h,
            orphan: crate::admit::AdmitConfig::default(),
            budget: Budget {
                budget: 8,
                cores: 10,
                ram_per_slot: 1,
            },
            leader: true,
            spawned: false,
            ended: None,
        };
        let ending = Ending {
            code: 0,
            killed: None,
            exit: 0,
        };
        let m = Measured {
            cpu_s: Some(3.0),
            mem_peak_mb: Some(10),
            procs: Some(5),
        };
        let r = job.record(1.0, &ending, m, false, Some(8));
        // D16a nulls the scope's figures, not the admission ones.
        assert_eq!((r.cpu_s, r.mem_peak_mb, r.procs), (None, None, None));
        assert_eq!((r.load1, r.held), (Some(9.5), Some(4)));
    }
}
