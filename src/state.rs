//! Unit `state` (spec §Architecture 2): `$XDG_STATE_HOME/muzzle/state.json`,
//! read-modify-write under an exclusive lock on `state.lock`.
//!
//! Every change is one short [`Store::update`]: lock, read, mutate, write to
//! a temp file, rename, unlock. The lock is `std::fs::File::lock` (flock on
//! Unix), so it is released by the kernel when a holder is SIGKILLed and no
//! stale lock file can wedge the queue.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::admit::Class;

/// The `state.json` schema version. A newer file is refused rather than
/// rewritten, so an older muzzle can't drop fields it doesn't know.
pub const SCHEMA_VERSION: u32 = 1;

/// D17a: stored budget values are cleared once the file has been idle (no
/// holders, no waiters) for longer than this.
pub const BUDGET_TTL_MS: u64 = 60 * 60 * 1000;

/// The whole of `state.json`. Times are Unix milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub v: u32,
    /// Monotonic; never reused, so a ticket names one request forever.
    pub next_ticket: u64,
    /// D17a: the budget the first run stored. `None` until then, or after
    /// the idle expiry.
    #[serde(default)]
    pub budget: Option<Budget>,
    /// In arrival (ticket) order. Admission order is [`crate::admit::queue`].
    #[serde(default)]
    pub waiters: Vec<Waiter>,
    #[serde(default)]
    pub holders: Vec<Holder>,
    /// Time of the last write, for the D17a expiry.
    #[serde(default)]
    pub updated_ms: u64,
}

impl Default for State {
    fn default() -> Self {
        State {
            v: SCHEMA_VERSION,
            next_ticket: 1,
            budget: None,
            waiters: Vec::new(),
            holders: Vec::new(),
            updated_ms: 0,
        }
    }
}

/// D17a: one budget per state file, shared by host and container runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub budget: u32,
    pub cores: u32,
    /// Bytes of RAM per slot (D9).
    pub ram_per_slot: u64,
}

/// What a request asks for, carried from waiter to holder unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// As requested; [`crate::admit::admit`] caps it to the budget (D8).
    pub slots: u32,
    pub class: Class,
    pub agent: Option<String>,
    pub rule: Option<String>,
    pub cwd: String,
    /// The config hash (D28), copied into the run record.
    pub cfg: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiter {
    pub ticket: u64,
    pub pid: u32,
    /// Process start time as `proc` reports it; with `pid` it identifies
    /// the process, so a recycled pid counts as dead.
    pub start: u64,
    /// The group the job will run in, when already known (muzzle leads its
    /// group, D16). `None` when the job gets a new group at spawn.
    pub pgid: Option<u32>,
    pub enqueued_ms: u64,
    #[serde(flatten)]
    pub claim: Claim,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Holder {
    pub ticket: u64,
    /// The muzzle, or after a handoff the keeper (D16).
    pub pid: u32,
    pub start: u64,
    /// `None` between admission and spawn of a job that gets a new group.
    pub pgid: Option<u32>,
    pub run_start_ms: u64,
    pub waited_s: f64,
    /// Set at spawn: running under a systemd scope (D15).
    #[serde(default)]
    pub enforced: bool,
    /// A keeper took over the entry (D16).
    #[serde(default)]
    pub kept: bool,
    /// Capped to the budget at admission (D8).
    #[serde(flatten)]
    pub claim: Claim,
}

/// One `runs.jsonl` line (spec §Interface, `runs.jsonl`). Defined here so
/// `admit` can build the `killed: "holder"` record without I/O and `run`
/// can write it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub v: u32,
    /// RFC 3339, UTC, whole seconds.
    pub end: String,
    pub rule: Option<String>,
    pub slots: u32,
    pub class: Class,
    pub cfg: Option<String>,
    pub agent: Option<String>,
    pub cwd: String,
    pub waited_s: f64,
    pub ran_s: f64,
    pub exit: Option<i32>,
    pub killed: Option<Killed>,
    pub cpu_s: Option<f64>,
    pub mem_peak_mb: Option<u64>,
    pub enforced: bool,
    pub nested: bool,
    pub kept: bool,
    pub budget: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Killed {
    Timeout,
    Oom,
    Signal,
    Holder,
}

impl State {
    /// Appends a waiter and returns its ticket.
    pub fn enqueue(
        &mut self,
        pid: u32,
        start: u64,
        pgid: Option<u32>,
        claim: Claim,
        now_ms: u64,
    ) -> u64 {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.waiters.push(Waiter {
            ticket,
            pid,
            start,
            pgid,
            enqueued_ms: now_ms,
            claim,
        });
        ticket
    }

    /// Drops the waiter or holder with this ticket (queue timeout, signal,
    /// release). Returns whether anything was removed.
    pub fn remove(&mut self, ticket: u64) -> bool {
        let before = self.waiters.len() + self.holders.len();
        self.waiters.retain(|w| w.ticket != ticket);
        self.holders.retain(|h| h.ticket != ticket);
        before != self.waiters.len() + self.holders.len()
    }

    pub fn holder_mut(&mut self, ticket: u64) -> Option<&mut Holder> {
        self.holders.iter_mut().find(|h| h.ticket == ticket)
    }

    pub fn held(&self) -> u32 {
        self.holders.iter().map(|h| h.claim.slots).sum()
    }

    /// D17a: the budget this run uses. A config `slots =` replaces the
    /// stored values; otherwise stored values win over this process's own
    /// reading, so a container and its host agree on one budget.
    pub fn resolve_budget(&mut self, reading: Budget, config_slots: Option<u32>) -> Budget {
        let budget = match (config_slots, self.budget) {
            (Some(slots), _) => Budget {
                budget: slots,
                ..reading
            },
            (None, Some(stored)) => stored,
            (None, None) => reading,
        };
        self.budget = Some(budget);
        budget
    }

    /// D17a: forget the stored budget once the file is idle and old.
    fn expire_budget(&mut self, now_ms: u64) {
        if self.waiters.is_empty()
            && self.holders.is_empty()
            && now_ms.saturating_sub(self.updated_ms) > BUDGET_TTL_MS
        {
            self.budget = None;
        }
    }
}

/// The state directory and its two files.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// `$XDG_STATE_HOME/muzzle`, defaulting to `~/.local/state/muzzle` on
    /// every platform (the spec names that path; `directories` has no state
    /// dir on macOS). Created if missing.
    pub fn open_default() -> anyhow::Result<Store> {
        let base = match std::env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
            Some(dir) if dir.is_absolute() => dir,
            _ => directories::BaseDirs::new()
                .context("cannot find the home directory")?
                .home_dir()
                .join(".local/state"),
        };
        Store::open(base.join("muzzle"))
    }

    pub fn open(dir: impl Into<PathBuf>) -> anyhow::Result<Store> {
        let dir = dir.into();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Store { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    fn lock(&self) -> anyhow::Result<File> {
        let path = self.dir.join("state.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
    }

    /// Reads the state under the lock, without writing (`status`).
    pub fn read(&self) -> anyhow::Result<State> {
        let _lock = self.lock()?;
        self.load()
    }

    /// One locked read-modify-write. `f` runs with the lock held; when it
    /// fails nothing is written.
    pub fn update<T>(
        &self,
        now_ms: u64,
        f: impl FnOnce(&mut State) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let _lock = self.lock()?;
        let mut state = self.load()?;
        state.expire_budget(now_ms);
        let out = f(&mut state)?;
        state.updated_ms = now_ms;
        self.save(&state)?;
        Ok(out)
    }

    fn load(&self) -> anyhow::Result<State> {
        let path = self.state_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        if text.trim().is_empty() {
            return Ok(State::default());
        }
        let state: State =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if state.v > SCHEMA_VERSION {
            bail!(
                "{} has schema v{}, this muzzle reads up to v{SCHEMA_VERSION}",
                path.display(),
                state.v
            );
        }
        Ok(state)
    }

    /// Temp file plus rename, so a crashed writer leaves the old file. No
    /// fsync: every waiter writes on each 250 ms poll, and only a host crash
    /// can lose the write, after which every pid in the file is dead anyway
    /// (an empty file reads as default). The 8-thread test at up to 400
    /// waiters took 2.3 s with fsync and 1.1 s without, on btrfs.
    fn save(&self, state: &State) -> anyhow::Result<()> {
        let path = self.state_path();
        let tmp = self.dir.join("state.json.tmp");
        // One write: `to_writer` on a bare File is a syscall per token, which
        // cost 47 ms per update at 400 waiters on btrfs.
        let mut bytes = serde_json::to_vec(state)?;
        bytes.push(b'\n');
        let mut file = File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(&bytes)?;
        fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(slots: u32) -> Claim {
        Claim {
            slots,
            class: Class::Normal,
            agent: Some("worker-7".into()),
            rule: Some("gate".into()),
            cwd: "/tmp".into(),
            cfg: Some("a1b2c3".into()),
        }
    }

    #[test]
    fn missing_file_is_default_and_update_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("muzzle")).unwrap();
        assert_eq!(store.read().unwrap(), State::default());
        let ticket = store
            .update(1_000, |s| Ok(s.enqueue(10, 20, Some(10), claim(2), 1_000)))
            .unwrap();
        assert_eq!(ticket, 1);
        let state = store.read().unwrap();
        assert_eq!(state.next_ticket, 2);
        assert_eq!(state.waiters[0].claim, claim(2));
        assert_eq!(state.updated_ms, 1_000);
    }

    #[test]
    fn failed_update_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let err = store.update(1, |s| -> anyhow::Result<()> {
            s.next_ticket = 99;
            bail!("no")
        });
        assert!(err.is_err());
        assert_eq!(store.read().unwrap().next_ticket, 1);
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        fs::write(store.state_path(), r#"{"v":2,"next_ticket":1}"#).unwrap();
        let err = store.read().unwrap_err().to_string();
        assert!(err.contains("schema v2"), "{err}");
    }

    #[test]
    fn concurrent_updates_lose_nothing() {
        const THREADS: u64 = 8;
        const EACH: u64 = 50;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let store = store.clone();
                scope.spawn(move || {
                    for i in 0..EACH {
                        store
                            .update(t * 1000 + i, |s| {
                                s.enqueue(t as u32, i, None, claim(1), 0);
                                Ok(())
                            })
                            .unwrap();
                    }
                });
            }
        });
        let state = store.read().unwrap();
        assert_eq!(state.next_ticket, THREADS * EACH + 1);
        let mut tickets: Vec<u64> = state.waiters.iter().map(|w| w.ticket).collect();
        tickets.sort();
        assert_eq!(tickets, (1..=THREADS * EACH).collect::<Vec<_>>());
    }

    const READING: Budget = Budget {
        budget: 14,
        cores: 16,
        ram_per_slot: 2 << 30,
    };

    #[test]
    fn first_run_stores_budget_and_later_runs_use_it() {
        let mut state = State::default();
        assert_eq!(state.resolve_budget(READING, None), READING);
        let container = Budget {
            budget: 6,
            cores: 8,
            ram_per_slot: 1 << 30,
        };
        assert_eq!(state.resolve_budget(container, None), READING);
    }

    #[test]
    fn config_slots_replace_stored_budget() {
        let mut state = State::default();
        state.resolve_budget(READING, None);
        let got = state.resolve_budget(READING, Some(12));
        assert_eq!(got.budget, 12);
        assert_eq!(state.budget, Some(got));
    }

    #[test]
    fn stored_budget_expires_when_idle_and_old() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .update(0, |s| {
                s.resolve_budget(READING, None);
                Ok(())
            })
            .unwrap();
        // Within the hour: kept.
        let kept = store.update(BUDGET_TTL_MS, |s| Ok(s.budget)).unwrap();
        assert_eq!(kept, Some(READING));
        // A waiter keeps it alive past the hour.
        store
            .update(BUDGET_TTL_MS, |s| Ok(s.enqueue(1, 1, None, claim(1), 0)))
            .unwrap();
        let busy = store.update(3 * BUDGET_TTL_MS, |s| Ok(s.budget)).unwrap();
        assert_eq!(busy, Some(READING));
        // Idle and over an hour since the last write: cleared.
        store
            .update(3 * BUDGET_TTL_MS, |s| Ok(s.remove(1)))
            .unwrap();
        let gone = store.update(5 * BUDGET_TTL_MS, |s| Ok(s.budget)).unwrap();
        assert_eq!(gone, None);
    }

    #[test]
    fn run_record_serialises_to_spec_shape() {
        let record = RunRecord {
            v: 1,
            end: "2026-11-03T10:12:44Z".into(),
            rule: Some("gate".into()),
            slots: 6,
            class: Class::Normal,
            cfg: Some("a1b2c3".into()),
            agent: None,
            cwd: "/x".into(),
            waited_s: 1.5,
            ran_s: 2.0,
            exit: None,
            killed: Some(Killed::Holder),
            cpu_s: None,
            mem_peak_mb: None,
            enforced: false,
            nested: false,
            kept: false,
            budget: Some(14),
        };
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["killed"], "holder");
        assert_eq!(json["class"], "normal");
        assert!(json["cpu_s"].is_null());
    }
}
