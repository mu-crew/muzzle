//! `muzzle doctor [--json]` (spec §Interface, `doctor`; D15, D17a, D33).
//!
//! Gathers [`Facts`] from the host, then [`check`] turns them into lines and
//! warnings without I/O, so every rule here is unit-tested. Doctor never
//! writes `state.json` or the config template: it may run while the queue
//! is busy, and a check that changes what it checks is no check.
//!
//! Exit 0 means usable, even when only advisory (spec). A bad config exits
//! 2, as `muzzle run` would; an unwritable state directory or an unreadable
//! `state.json` (a newer schema, say) exits 125, since every `run` would
//! fail on it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;

use crate::cli::DoctorArgs;
use crate::rules::config::{self, Config, ConfigError, TEMPLATE};
use crate::state::{Budget, Store};

/// The config at [`config::path`], without writing the template when it is
/// missing: the defaults apply then, as for `run` (spec §Config).
pub(crate) fn read_config() -> (PathBuf, Result<Config, ConfigError>, bool) {
    let path = config::path();
    match std::fs::metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let c = Config::parse(TEMPLATE, &path);
            (path, c, false)
        }
        _ => {
            let c = Config::load_from(&path);
            (path, c, true)
        }
    }
}

/// Everything doctor looks at, read once.
#[derive(Debug, Clone)]
pub struct Facts {
    pub os: &'static str,
    pub arch: &'static str,
    /// This process's own reading (D7, D9), before the reserve.
    pub cores: Option<u32>,
    pub ram: Option<u64>,
    pub config_path: PathBuf,
    pub config_exists: bool,
    pub config: Result<Config, String>,
    /// `state.json`'s stored budget (D17a), when one is stored and not
    /// expired ([`crate::state::State::live_budget`]).
    pub stored: Option<Budget>,
    pub state_dir: PathBuf,
    /// `Err` with the reason when a file can't be created there.
    pub state_writable: Result<(), String>,
    /// `Err` with the reason when `state.json` can't be read, as every
    /// `run` would fail on it.
    pub state_readable: Result<(), String>,
    pub systemd: bool,
    pub runs: Option<RunsLog>,
    /// Rules that `runs.jsonl` shows entering a container (D16a).
    pub container_runs: Vec<String>,
    pub harnesses: Harnesses,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunsLog {
    pub bytes: u64,
    pub lines: usize,
    /// The oldest and newest `end`, RFC 3339 strings compare in time order.
    pub oldest: Option<String>,
    pub newest: Option<String>,
}

/// Where each harness's muzzle hook or extension was found; empty when not.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Harnesses {
    pub pi_extension: Vec<PathBuf>,
    pub claude_hook: Vec<PathBuf>,
    pub codex_hook: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub version: &'static str,
    pub platform: String,
    pub budget: Option<u32>,
    pub cores: Option<u32>,
    pub reserve: Option<u32>,
    pub ram_per_slot_mb: Option<u64>,
    /// `stored` (state.json, D17a), `config` (`slots =`) or `measured`.
    pub budget_source: &'static str,
    pub measured: Option<BudgetView>,
    pub stored: Option<BudgetView>,
    pub enforced: bool,
    pub enforcement: String,
    pub state_dir: PathBuf,
    pub state_writable: bool,
    pub state_readable: bool,
    pub config_path: PathBuf,
    pub config_exists: bool,
    pub config_error: Option<String>,
    pub rules: Option<usize>,
    pub config_hash: Option<String>,
    pub runs: Option<RunsLog>,
    /// Added field (D16a): rules whose jobs enter a container, so muzzle
    /// admits them but can't cap or measure them. Empty when none.
    pub container_rules: Vec<String>,
    /// Added field (D16a): catch-all rules that match a container entry as
    /// well as host commands. Their host runs are enforced.
    pub container_maybe: Vec<String>,
    pub pi_extension: &'static str,
    pub claude_hook: &'static str,
    pub codex_hook: &'static str,
    pub harness_files: Harnesses,
    pub warnings: Vec<String>,
    /// Exit status: 0 usable, 2 bad config, 125 state unwritable or
    /// unreadable.
    #[serde(skip)]
    pub exit: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BudgetView {
    pub budget: u32,
    pub cores: u32,
    pub ram_per_slot_mb: u64,
}

fn mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

fn budget_view(b: Budget) -> BudgetView {
    BudgetView {
        budget: b.budget,
        cores: b.cores,
        ram_per_slot_mb: mb(b.ram_per_slot),
    }
}

/// The enforcement line (D15): a systemd user scope where reachable.
fn enforcement(os: &str, systemd: bool) -> String {
    match (os, systemd) {
        ("linux", true) => "enforced (systemd scope: MemoryMax, CPUWeight)".into(),
        ("linux", false) => "advisory (no systemd user manager reachable)".into(),
        (os, _) => format!("advisory ({os} has no systemd scope)"),
    }
}

/// D16a: what a rule's pattern is probed with. A rule that matches one of
/// these, or that `runs.jsonl` shows entering a container, is flagged.
/// Not a substring test on the pattern: `^docker build` names docker but
/// never enters a container (rv_cg_doctor_flags_by_substring).
const CONTAINER_PROBES: [&str; 6] = [
    "distrobox enter box -- true",
    "podman exec box true",
    "toolbox run true",
    "toolbox enter",
    "docker exec box true",
    "docker compose exec app true",
];

/// A pattern that also matches these is a catch-all: it may wrap a
/// container entry, but most of what it matches runs on the host.
const HOST_PROBES: [&str; 2] = ["make", "cargo test"];

/// Rules that enter a container, by probe match or because `runs.jsonl`
/// recorded such a run for them, split into those that do and catch-alls
/// that may. Each sorted, no duplicates.
fn container_rules(config: Option<&Config>, seen: &[String]) -> (Vec<String>, Vec<String>) {
    let mut sure: std::collections::BTreeSet<String> = seen.iter().cloned().collect();
    let mut maybe = std::collections::BTreeSet::new();
    if let Some(c) = config {
        for (name, pattern) in c.patterns() {
            if !CONTAINER_PROBES.iter().any(|p| pattern.is_match(p)) {
                continue;
            }
            if HOST_PROBES.iter().any(|p| pattern.is_match(p)) {
                maybe.insert(name.to_string());
            } else {
                sure.insert(name.to_string());
            }
        }
    }
    // A catch-all with recorded container runs does enter one.
    maybe.retain(|n| !sure.contains(n));
    (sure.into_iter().collect(), maybe.into_iter().collect())
}

pub fn check(f: &Facts) -> Report {
    let (container_rules, container_maybe) =
        container_rules(f.config.as_ref().ok(), &f.container_runs);
    let mut warnings = Vec::new();
    let (reserve, config_slots, rules, hash, config_error) = match &f.config {
        Ok(c) => (
            Some(c.reserve),
            c.slots,
            Some(c.rule_count()),
            Some(c.hash()),
            None,
        ),
        Err(e) => (None, None, None, None, Some(e.clone())),
    };
    let measured = f.cores.zip(f.ram).map(|(cores, ram)| {
        let cores = cores.max(1);
        Budget {
            budget: config_slots.unwrap_or_else(|| cores.saturating_sub(reserve.unwrap_or(0))),
            cores,
            ram_per_slot: ram / u64::from(cores),
        }
    });
    // D17a: the stored values win until the file goes idle, except that a
    // config `slots =` replaces them on the next run.
    let (effective, budget_source) = match (f.stored, config_slots) {
        (Some(s), Some(slots)) => (Some(Budget { budget: slots, ..s }), "config"),
        (Some(s), None) => (Some(s), "stored"),
        (None, _) => (
            measured,
            if config_slots.is_some() {
                "config"
            } else {
                "measured"
            },
        ),
    };
    if let (Some(s), Some(m)) = (f.stored, measured)
        && (s.cores != m.cores
            || s.ram_per_slot != m.ram_per_slot
            || (config_slots.is_none() && s.budget != m.budget))
    {
        warnings.push(format!(
            "stored budget differs from this reading (D17a): stored {} slots, {} cores, \
             {} MB/slot; measured {} slots, {} cores, {} MB/slot. The stored values apply \
             until state.json is idle for an hour; a container and its host can disagree.",
            s.budget,
            s.cores,
            mb(s.ram_per_slot),
            m.budget,
            m.cores,
            mb(m.ram_per_slot),
        ));
    }
    if effective.is_some_and(|b| b.budget == 0) {
        warnings.push(
            "budget is 0 (cores minus reserve): every job runs alone; lower `reserve`".into(),
        );
    }
    if let Err(e) = &f.state_writable {
        warnings.push(format!("state directory not writable: {e}"));
    }
    if let Err(e) = &f.state_readable {
        warnings.push(format!("state.json unreadable, every run fails: {e}"));
    }
    if let Some(e) = &config_error {
        warnings.push(format!("config: {e}"));
    }
    if !f.config_exists {
        warnings.push(format!(
            "no config at {}: the built-in default rules apply; `muzzle run` writes the template",
            f.config_path.display()
        ));
    }
    let systemd = f.systemd && f.os == "linux";
    let exit = if config_error.is_some() {
        crate::exit::USAGE
    } else if f.state_writable.is_err() || f.state_readable.is_err() {
        crate::exit::INTERNAL
    } else {
        0
    };
    let found = |v: &Vec<PathBuf>, yes, no| if v.is_empty() { no } else { yes };
    Report {
        version: env!("CARGO_PKG_VERSION"),
        platform: format!("{}-{}", f.os, f.arch),
        budget: effective.map(|b| b.budget),
        cores: effective.map(|b| b.cores),
        reserve,
        ram_per_slot_mb: effective.map(|b| mb(b.ram_per_slot)),
        budget_source,
        measured: measured.map(budget_view),
        stored: f.stored.map(budget_view),
        enforced: systemd,
        enforcement: enforcement(f.os, systemd),
        state_dir: f.state_dir.clone(),
        state_writable: f.state_writable.is_ok(),
        state_readable: f.state_readable.is_ok(),
        config_path: f.config_path.clone(),
        config_exists: f.config_exists,
        config_error,
        rules,
        config_hash: hash,
        runs: f.runs.clone(),
        container_rules,
        container_maybe,
        pi_extension: found(&f.harnesses.pi_extension, "loaded", "not installed"),
        claude_hook: found(&f.harnesses.claude_hook, "installed", "not found"),
        codex_hook: found(&f.harnesses.codex_hook, "installed", "not found"),
        harness_files: f.harnesses.clone(),
        warnings,
        exit,
    }
}

pub fn render_text(r: &Report) -> String {
    let opt = |v: Option<u64>| v.map_or("?".to_string(), |v| v.to_string());
    let mut out = String::new();
    let mut line = |k: &str, v: String| out.push_str(&format!("{k:<14} {v}\n"));
    line(
        "budget",
        format!(
            "{} slots ({}; {} cores, reserve {}, {} MB RAM per slot)",
            opt(r.budget.map(u64::from)),
            r.budget_source,
            opt(r.cores.map(u64::from)),
            opt(r.reserve.map(u64::from)),
            opt(r.ram_per_slot_mb),
        ),
    );
    line("enforcement", r.enforcement.clone());
    line(
        "state",
        format!(
            "{} ({})",
            r.state_dir.display(),
            match (r.state_writable, r.state_readable) {
                (false, _) => "NOT writable",
                (true, false) => "writable, state.json unreadable",
                (true, true) => "writable",
            }
        ),
    );
    let config = match (&r.config_error, r.config_exists) {
        (Some(_), _) => "invalid".to_string(),
        (None, exists) => format!(
            "{} rules, hash {}{}",
            opt(r.rules.map(|n| n as u64)),
            r.config_hash.as_deref().unwrap_or("?"),
            if exists { "" } else { ", missing: defaults" }
        ),
    };
    line("config", format!("{} ({config})", r.config_path.display()));
    line(
        "runs.jsonl",
        match &r.runs {
            None => "none yet".into(),
            Some(l) => format!(
                "{} bytes, {} runs, {} .. {}",
                l.bytes,
                l.lines,
                l.oldest.as_deref().unwrap_or("?"),
                l.newest.as_deref().unwrap_or("?")
            ),
        },
    );
    let mut containers = Vec::new();
    if !r.container_rules.is_empty() {
        containers.push(format!(
            "admission only for {} (D16a): the job runs in the container's cgroup, \
             so MemoryMax, CPUWeight and the cpu/mem figures don't reach it",
            r.container_rules.join(", ")
        ));
    }
    if !r.container_maybe.is_empty() {
        containers.push(format!(
            "{} may enter containers; those runs are admission only (D16a)",
            r.container_maybe.join(", ")
        ));
    }
    if !containers.is_empty() {
        line("containers", containers.join("; "));
    }
    line("pi extension", r.pi_extension.into());
    line("claude hook", r.claude_hook.into());
    line("codex hook", r.codex_hook.into());
    line("platform", r.platform.clone());
    for w in &r.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out
}

/// Size, count and age range of `runs.jsonl`; `None` when there is none.
fn runs_log(path: &Path) -> Option<RunsLog> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    #[derive(serde::Deserialize)]
    struct End {
        end: String,
    }
    let ends: Vec<String> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<End>(l).ok().map(|e| e.end))
        .collect();
    Some(RunsLog {
        bytes: bytes.len() as u64,
        lines: text.lines().filter(|l| !l.trim().is_empty()).count(),
        oldest: ends.iter().min().cloned(),
        newest: ends.iter().max().cloned(),
    })
}

/// Rules with a `runs.jsonl` line marked `enforced_reason: "container"`.
fn container_runs(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let (runs, _) = crate::report::parse(&text);
    runs.into_iter()
        .filter(|r| r.rec.enforced_reason.is_some())
        .filter_map(|r| r.rec.rule)
        .collect()
}

/// A file can be created in `dir`. The probe file is removed again.
fn writable(dir: &Path) -> Result<(), String> {
    let probe = dir.join(format!(".doctor-{}", std::process::id()));
    std::fs::write(&probe, b"")
        .map(|()| {
            let _ = std::fs::remove_file(&probe);
        })
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// Any string under `v` names `muzzle hook`.
fn names_hook(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains("muzzle hook"),
        Value::Array(a) => a.iter().any(names_hook),
        Value::Object(o) => o.values().any(names_hook),
        _ => false,
    }
}

/// The `hooks` value of a JSON or TOML harness config names `muzzle hook`.
/// Only `hooks` is searched, so a description or comment elsewhere that
/// mentions muzzle doesn't count as installed.
fn hook_in(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let v: Option<Value> = if path.extension().is_some_and(|e| e == "toml") {
        toml::from_str::<toml::Value>(&text)
            .ok()
            .and_then(|t| serde_json::to_value(t).ok())
    } else {
        serde_json::from_str(&text).ok()
    };
    v.as_ref()
        .and_then(|v| v.get("hooks"))
        .is_some_and(names_hook)
}

/// pi loads muzzle's extension from a package or extension entry in its
/// settings that names muzzle, or a file in its extensions directory.
fn pi_in_settings(path: &Path) -> bool {
    let Some(v) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return false;
    };
    let names = |s: &str| {
        let s = s.trim_end_matches('/');
        s.contains("@mu-crew/muzzle")
            || s.rsplit('/')
                .next()
                .is_some_and(|b| b.starts_with("muzzle"))
    };
    ["packages", "extensions"].iter().any(|k| {
        v.get(k).and_then(Value::as_array).is_some_and(|a| {
            a.iter().any(|e| match e {
                Value::String(s) => names(s),
                // `{"source": "npm:…", …}` package entries.
                Value::Object(o) => o.get("source").and_then(Value::as_str).is_some_and(names),
                _ => false,
            })
        })
    })
}

/// `p` in pi's extensions directory is a muzzle extension pi loads: a
/// `muzzle*.ts`/`.js` file, or a `muzzle*` directory with an `index.ts`,
/// `index.js` or `package.json`. `muzzle.ts.disabled` or `muzzle-notes.md`
/// are not loaded, so they don't count.
fn pi_loads(p: &Path) -> bool {
    if !p
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("muzzle"))
    {
        return false;
    }
    if p.is_dir() {
        ["index.ts", "index.js", "package.json"]
            .iter()
            .any(|f| p.join(f).is_file())
    } else {
        p.is_file() && p.extension().is_some_and(|e| e == "ts" || e == "js")
    }
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Where each harness keeps its config, globally under `home` and for the
/// project at `cwd`. `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and
/// `PI_CODING_AGENT_DIR` move the global ones, as they do for the harness.
pub fn harnesses(home: &Path, cwd: &Path) -> Harnesses {
    let claude = env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"));
    let codex = env_dir("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));
    let pi = env_dir("PI_CODING_AGENT_DIR").unwrap_or_else(|| home.join(".pi/agent"));
    let keep = |paths: Vec<PathBuf>, f: fn(&Path) -> bool| -> Vec<PathBuf> {
        paths.into_iter().filter(|p| f(p)).collect()
    };
    let mut pi_found = keep(
        vec![pi.join("settings.json"), cwd.join(".pi/settings.json")],
        pi_in_settings,
    );
    if let Ok(dir) = std::fs::read_dir(pi.join("extensions")) {
        pi_found.extend(dir.flatten().map(|e| e.path()).filter(|p| pi_loads(p)));
    }
    Harnesses {
        pi_extension: pi_found,
        claude_hook: keep(
            vec![
                claude.join("settings.json"),
                claude.join("settings.local.json"),
                cwd.join(".claude/settings.json"),
                cwd.join(".claude/settings.local.json"),
            ],
            hook_in,
        ),
        codex_hook: keep(
            vec![
                codex.join("hooks.json"),
                codex.join("config.toml"),
                cwd.join(".codex/hooks.json"),
                cwd.join(".codex/config.toml"),
            ],
            hook_in,
        ),
    }
}

fn gather() -> Facts {
    let (config_path, config, config_exists) = read_config();
    let (state_dir, state_writable, state_readable, stored) = match Store::open_default() {
        Ok(store) => {
            let w = writable(store.dir());
            let (readable, stored) = match store.read() {
                Ok(s) => (Ok(()), s.live_budget(crate::status::now_ms())),
                Err(e) => (Err(format!("{e:#}")), None),
            };
            (store.dir().to_path_buf(), w, readable, stored)
        }
        // The directory error already covers the file.
        Err(e) => (PathBuf::from("?"), Err(format!("{e:#}")), Ok(()), None),
    };
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    Facts {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cores: crate::proc::logical_cores().ok().map(|c| c as u32),
        ram: crate::proc::total_ram().ok(),
        config_path,
        config_exists,
        config: config.map_err(|e| e.to_string()),
        stored,
        runs: runs_log(&state_dir.join("runs.jsonl")),
        container_runs: container_runs(&state_dir.join("runs.jsonl")),
        state_dir,
        state_writable,
        state_readable,
        systemd: crate::proc::systemd_user_reachable(),
        harnesses: harnesses(&home, &cwd),
    }
}

pub fn main(args: &DoctorArgs) -> anyhow::Result<ExitCode> {
    let report = check(&gather());
    if args.json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print!("{}", render_text(&report));
    }
    Ok(ExitCode::from(report.exit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn cfg(text: &str) -> Result<Config, String> {
        Config::parse(text, Path::new("/c.toml")).map_err(|e| e.to_string())
    }

    fn facts() -> Facts {
        Facts {
            os: "linux",
            arch: "x86_64",
            cores: Some(16),
            ram: Some(32 * GIB),
            config_path: "/c.toml".into(),
            config_exists: true,
            config: cfg("reserve = 2\n[[rule]]\nname='g'\nmatch='g'\nslots=2\n"),
            stored: None,
            state_dir: "/s".into(),
            state_writable: Ok(()),
            state_readable: Ok(()),
            systemd: true,
            runs: None,
            container_runs: Vec::new(),
            harnesses: Harnesses::default(),
        }
    }

    fn stored(budget: u32, cores: u32, ram_per_slot: u64) -> Option<Budget> {
        Some(Budget {
            budget,
            cores,
            ram_per_slot,
        })
    }

    #[test]
    fn measured_budget_is_cores_minus_reserve() {
        let r = check(&facts());
        assert_eq!(
            (r.budget, r.cores, r.reserve, r.ram_per_slot_mb),
            (Some(14), Some(16), Some(2), Some(2048))
        );
        assert_eq!((r.budget_source, r.rules, r.exit), ("measured", Some(1), 0));
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn matching_stored_budget_does_not_warn() {
        let mut f = facts();
        f.stored = stored(14, 16, 2 * GIB);
        let r = check(&f);
        assert_eq!(r.budget_source, "stored");
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn stored_vs_measured_warns_on_each_field() {
        for s in [
            stored(6, 16, 2 * GIB),
            stored(14, 8, 2 * GIB),
            stored(14, 16, GIB),
        ] {
            let mut f = facts();
            f.stored = s;
            let r = check(&f);
            assert_eq!(r.budget, Some(s.unwrap().budget), "stored wins");
            assert_eq!(r.warnings.len(), 1, "{s:?}: {:?}", r.warnings);
            assert!(r.warnings[0].contains("D17a"), "{}", r.warnings[0]);
        }
    }

    #[test]
    fn config_slots_replaces_the_stored_budget_without_a_warning() {
        let mut f = facts();
        f.config = cfg("slots = 5\n");
        f.stored = stored(14, 16, 2 * GIB);
        let r = check(&f);
        assert_eq!((r.budget, r.budget_source), (Some(5), "config"));
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn enforcement_line() {
        assert_eq!(
            check(&facts()).enforcement,
            "enforced (systemd scope: MemoryMax, CPUWeight)"
        );
        let mut f = facts();
        f.systemd = false;
        let r = check(&f);
        assert!(!r.enforced && r.enforcement.starts_with("advisory ("));
        assert_eq!(r.exit, 0, "advisory is still usable");
        f.os = "macos";
        f.systemd = true;
        assert!(check(&f).enforcement.starts_with("advisory (macos"));
    }

    #[test]
    fn exit_codes() {
        let mut f = facts();
        f.config = cfg("bogus = 1\n");
        let r = check(&f);
        assert_eq!((r.exit, r.rules), (2, None));
        assert!(r.config_error.as_deref().unwrap().contains("bogus"));
        let mut f = facts();
        f.state_writable = Err("read-only".into());
        assert_eq!(check(&f).exit, 125);
        // Reviewer repro (sd_unreadable_state): a newer schema fails every
        // run, so doctor must not call it usable.
        let mut f = facts();
        f.state_readable = Err("schema v99, this muzzle reads up to v1".into());
        let r = check(&f);
        assert_eq!((r.exit, r.state_readable), (125, false));
        assert!(
            r.warnings.iter().any(|w| w.contains("schema v99")),
            "{:?}",
            r.warnings
        );
        assert!(render_text(&r).contains("(writable, state.json unreadable)"));
    }

    #[test]
    fn text_has_every_spec_line() {
        let mut f = facts();
        f.runs = Some(RunsLog {
            bytes: 10,
            lines: 2,
            oldest: Some("2026-10-01T00:00:00Z".into()),
            newest: Some("2026-10-02T00:00:00Z".into()),
        });
        f.harnesses
            .claude_hook
            .push("/h/.claude/settings.json".into());
        let t = render_text(&check(&f));
        for want in [
            "budget         14 slots (measured; 16 cores, reserve 2, 2048 MB RAM per slot)",
            "enforcement    enforced (systemd scope",
            "/s (writable)",
            "1 rules, hash ",
            "10 bytes, 2 runs, 2026-10-01T00:00:00Z .. 2026-10-02T00:00:00Z",
            "pi extension   not installed",
            "claude hook    installed",
            "codex hook     not found",
            "platform       linux-x86_64",
        ] {
            assert!(t.contains(want), "missing {want:?} in\n{t}");
        }
    }

    #[test]
    fn container_rules_are_named_with_the_limit() {
        let mut f = facts();
        let r = check(&f);
        assert!(r.container_rules.is_empty());
        assert!(!render_text(&r).contains("containers"));

        f.config = cfg(concat!(
            "[[rule]]\nname='box'\nmatch='distrobox enter'\n",
            "[[rule]]\nname='any'\nmatch='.'\n",
            "[[rule]]\nname='gate'\nmatch='^contrib/gate'\n",
            "[[rule]]\nname='plain'\nmatch='^make'\n",
            "[[rule]]\nname='dockbuild'\nmatch='^docker build'\n",
            "[[rule]]\nname='boxgate'\nmatch='(^|&&\\s*)distrobox enter \\S+ -- contrib/'\n",
            "[[rule]]\nname='compose'\nmatch='compose exec'\n",
        ));
        f.container_runs = vec!["gate".into(), "gate".into()];
        let r = check(&f);
        // boxgate needs a `contrib/` job, which no probe has: only the log
        // can name it. dockbuild names docker but never enters a container.
        assert_eq!(r.container_rules, ["box", "compose", "gate"]);
        assert_eq!(r.container_maybe, ["any"]);
        let t = render_text(&r);
        assert!(
            t.contains("containers     admission only for box, compose, gate (D16a)"),
            "{t}"
        );
        assert!(t.contains("MemoryMax, CPUWeight"), "{t}");
        assert!(t.contains("; any may enter containers"), "{t}");
        assert!(!t.contains("dockbuild"), "{t}");

        // Seen in the log, a catch-all does enter one.
        f.container_runs = vec!["any".into()];
        let r = check(&f);
        assert_eq!(r.container_rules, ["any", "box", "compose"]);
        assert!(r.container_maybe.is_empty());
    }

    #[test]
    fn container_runs_come_from_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runs.jsonl");
        let line = |rule: &str, reason: &str| {
            format!(
                "{{\"v\":1,\"end\":\"2026-10-29T00:00:00Z\",\"rule\":\"{rule}\",\"slots\":1,\
                 \"class\":\"normal\",\"cfg\":null,\"agent\":null,\"cwd\":\"/\",\"waited_s\":0,\
                 \"ran_s\":1,\"exit\":0,\"killed\":null,\"cpu_s\":null,\"mem_peak_mb\":null,\
                 \"enforced\":false,\"nested\":false,\"kept\":false,\"budget\":4{reason}}}\n"
            )
        };
        let text = line("box", ",\"enforced_reason\":\"container\"") + &line("host", "");
        std::fs::write(&p, text).unwrap();
        assert_eq!(container_runs(&p), ["box"]);
    }

    #[test]
    fn runs_log_reads_size_and_range() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runs.jsonl");
        assert_eq!(runs_log(&p), None);
        std::fs::write(
            &p,
            "{\"end\":\"2026-10-05T00:00:00Z\"}\n{\"end\":\"2026-10-01T00:00:00Z\"}\nbad\n",
        )
        .unwrap();
        let l = runs_log(&p).unwrap();
        assert_eq!((l.lines, l.bytes), (3, 66));
        assert_eq!(l.oldest.as_deref(), Some("2026-10-01T00:00:00Z"));
        assert_eq!(l.newest.as_deref(), Some("2026-10-05T00:00:00Z"));
    }

    #[test]
    fn hook_only_counts_under_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, r#"{"note":"muzzle hook claude","hooks":{}}"#).unwrap();
        assert!(!hook_in(&p));
        std::fs::write(
            &p,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"command":"muzzle hook claude"}]}]}}"#,
        )
        .unwrap();
        assert!(hook_in(&p));
        let t = dir.path().join("config.toml");
        std::fs::write(
            &t,
            "[[hooks.PreToolUse.hooks]]\ncommand = \"muzzle hook codex\"\n",
        )
        .unwrap();
        assert!(hook_in(&t));
        std::fs::write(&t, "[[hooks.PreToolUse.hooks]]\ncommand = \"other\"\n").unwrap();
        assert!(!hook_in(&t));
    }

    #[test]
    fn pi_extension_dir_counts_only_what_pi_loads() {
        // Reviewer repro (sd_pi_ext_prefix).
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for f in ["muzzle.ts.disabled", "muzzle-notes.md", "other.ts"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        std::fs::create_dir(d.join("muzzle-empty")).unwrap();
        for f in [
            "muzzle.ts.disabled",
            "muzzle-notes.md",
            "other.ts",
            "muzzle-empty",
        ] {
            assert!(!pi_loads(&d.join(f)), "{f}");
        }
        std::fs::write(d.join("muzzle.ts"), "").unwrap();
        std::fs::write(d.join("muzzle.js"), "").unwrap();
        for (sub, entry) in [
            ("muzzle-a", "index.ts"),
            ("muzzle-b", "index.js"),
            ("muzzle", "package.json"),
        ] {
            std::fs::create_dir(d.join(sub)).unwrap();
            std::fs::write(d.join(sub).join(entry), "").unwrap();
        }
        for f in ["muzzle.ts", "muzzle.js", "muzzle-a", "muzzle-b", "muzzle"] {
            assert!(pi_loads(&d.join(f)), "{f}");
        }
    }

    #[test]
    fn pi_settings_entries() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        for (body, want) in [
            (r#"{"packages":["npm:@mu-crew/muzzle"]}"#, true),
            (
                r#"{"packages":[{"source":"npm:@mu-crew/muzzle@0.1.0"}]}"#,
                true,
            ),
            (
                r#"{"packages":["../hacking/crew/muzzle/npm/muzzle"]}"#,
                true,
            ),
            (r#"{"extensions":["/x/muzzle.ts"]}"#, true),
            (r#"{"packages":["npm:@mu-crew/mu"]}"#, false),
            (r#"{"defaultModel":"muzzle"}"#, false),
        ] {
            std::fs::write(&p, body).unwrap();
            assert_eq!(pi_in_settings(&p), want, "{body}");
        }
    }
}
