//! `muzzle doctor [--json]` (spec §Interface, `doctor`; D15, D17a, D33).
//!
//! Gathers [`Facts`] from the host, then [`check`] turns them into lines and
//! warnings without I/O, so every rule here is unit-tested. Doctor never
//! writes `state.json` or the config template: it may run while the queue
//! is busy, and a check that changes what it checks is no check.
//!
//! Exit 0 means usable, even when only advisory (spec). A bad config exits
//! 2, as `muzzle run` would; an unwritable state directory exits 125, since
//! every `run` would fail on it.

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
    /// `state.json`'s stored budget (D17a), when one is stored.
    pub stored: Option<Budget>,
    pub state_dir: PathBuf,
    /// `Err` with the reason when a file can't be created there.
    pub state_writable: Result<(), String>,
    pub systemd: bool,
    pub runs: Option<RunsLog>,
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
    pub config_path: PathBuf,
    pub config_exists: bool,
    pub config_error: Option<String>,
    pub rules: Option<usize>,
    pub config_hash: Option<String>,
    pub runs: Option<RunsLog>,
    pub pi_extension: &'static str,
    pub claude_hook: &'static str,
    pub codex_hook: &'static str,
    pub harness_files: Harnesses,
    pub warnings: Vec<String>,
    /// Exit status: 0 usable, 2 bad config, 125 state unwritable.
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

pub fn check(f: &Facts) -> Report {
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
    } else if f.state_writable.is_err() {
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
        config_path: f.config_path.clone(),
        config_exists: f.config_exists,
        config_error,
        rules,
        config_hash: hash,
        runs: f.runs.clone(),
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
            if r.state_writable {
                "writable"
            } else {
                "NOT writable"
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
        pi_found.extend(dir.flatten().map(|e| e.path()).filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("muzzle"))
        }));
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
    let (state_dir, state_writable, stored) = match Store::open_default() {
        Ok(store) => {
            let w = writable(store.dir());
            // Unreadable state (newer schema, lock trouble) is a warning
            // through `stored: None`, not a doctor failure.
            let stored = store.read().ok().and_then(|s| s.budget);
            (store.dir().to_path_buf(), w, stored)
        }
        Err(e) => (PathBuf::from("?"), Err(format!("{e:#}")), None),
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
        state_dir,
        state_writable,
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
            systemd: true,
            runs: None,
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
