//! The checks: [`Facts`] in, a [`Report`] out, without I/O.

use std::path::PathBuf;

use super::{BudgetView, ConfigFile, Facts, Report};
use crate::rules::config::Config;
use crate::runlog::{ARCHIVES_WARN_BYTES, LIVE_MAX_BYTES};
use crate::state::Budget;

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
    let mut config_files = vec![ConfigFile {
        file: f.config_path.clone(),
        role: "root",
        found: f.config_exists,
    }];
    if let Ok(c) = &f.config {
        config_files.extend(c.includes().into_iter().map(|(file, found)| ConfigFile {
            file,
            role: "include",
            found,
        }));
    }
    let path_tables = f.config.as_ref().ok().map(Config::path_table_count);
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
    if let Some(l) = &f.runlog {
        if l.archive_bytes > ARCHIVES_WARN_BYTES {
            warnings.push(format!(
                "runs.jsonl archives total {} MB (over {} MB); they are kept forever, \
                 so remove old runs-YYYY-MM.jsonl.gz files by hand",
                mb(l.archive_bytes),
                mb(ARCHIVES_WARN_BYTES)
            ));
        }
        // Rotation runs before an append, so a healthy live file can sit
        // just over the bound until the next one: warn only at twice it.
        if l.live_bytes > 2 * LIVE_MAX_BYTES {
            warnings.push(format!(
                "runs.jsonl is {} MB (over {} MB): rotation is failing",
                mb(l.live_bytes),
                mb(2 * LIVE_MAX_BYTES)
            ));
        }
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
        config_files,
        path_tables,
        config_hash: hash,
        runs: f.runs.clone(),
        runs_archives: f.runlog.clone(),
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::{Harnesses, RunsLog, render_text};
    use super::*;
    use crate::runlog::Stats;

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
            runlog: None,
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
            "config         /c.toml (found; 1 rules, 0 path tables, hash ",
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
    fn config_files_and_path_tables_are_listed() {
        // P8, P5: root, each include found or not, and the table count.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("config.toml");
        std::fs::write(
            &root,
            concat!(
                "include = ['machine.toml', 'gone.toml']\n",
                "[[rule]]\nname='g'\nmatch='g'\n",
                "[[path.\"/p\".rule]]\nname='g'\nslots=2\n",
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("machine.toml"),
            "[[path.\"/q\".rule]]\nname='g'\nslots=3\n[[path.\"/p/\".rule]]\nname='g'\nslots=4\n",
        )
        .unwrap();
        let mut f = facts();
        f.config_path = root.clone();
        f.config = Config::load_from(&root).map_err(|e| e.to_string());
        let r = check(&f);
        assert_eq!(r.path_tables, Some(2), "/p and /p/ are one table (B1)");
        let machine = dir.path().join("machine.toml");
        let gone = dir.path().join("gone.toml");
        let want = [
            (root.clone(), "root", true),
            (machine.clone(), "include", true),
            (gone.clone(), "include", false),
        ];
        let got: Vec<_> = r
            .config_files
            .iter()
            .map(|c| (c.file.clone(), c.role, c.found))
            .collect();
        assert_eq!(got, want);
        assert!(
            r.warnings.is_empty(),
            "a missing include is silent: {:?}",
            r.warnings
        );

        let t = render_text(&r);
        for want in [
            format!(
                "config         {} (found; 1 rules, 2 path tables, hash ",
                root.display()
            ),
            format!("include        {} (found)\n", machine.display()),
            format!("include        {} (not found)\n", gone.display()),
        ] {
            assert!(t.contains(&want), "missing {want:?} in\n{t}");
        }

        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["path_tables"], 2);
        let files = v["config_files"].as_array().unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[1]["file"], machine.to_str().unwrap());
        assert_eq!(
            (&files[1]["role"], &files[1]["found"]),
            (&"include".into(), &true.into())
        );
        assert_eq!(files[2]["file"], gone.to_str().unwrap());
        assert_eq!(files[2]["found"], false);
        assert_eq!(
            (&files[0]["role"], &files[0]["found"]),
            (&"root".into(), &true.into())
        );
    }

    #[test]
    fn missing_root_and_invalid_config_list_only_the_root() {
        let mut f = facts();
        f.config_exists = false;
        let r = check(&f);
        assert_eq!(r.config_files.len(), 1);
        assert!(!r.config_files[0].found);
        assert!(render_text(&r).contains("(not found: defaults; 1 rules, 0 path tables"));
        f.config = cfg("bogus = 1\n");
        let r = check(&f);
        assert_eq!((r.config_files.len(), r.path_tables), (1, None));
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
    fn run_log_archives_are_shown_and_warned_on() {
        let mut f = facts();
        f.runlog = Some(Stats::default());
        let r = check(&f);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert!(!render_text(&r).contains("archives"));

        // A healthy live file can sit just over the bound until the next
        // append rotates it.
        f.runlog = Some(Stats {
            live_bytes: 2 * LIVE_MAX_BYTES,
            archives: 3,
            archive_bytes: ARCHIVES_WARN_BYTES,
            oldest_month: Some("2026-08".into()),
        });
        let r = check(&f);
        assert!(r.warnings.is_empty(), "at the limits: {:?}", r.warnings);
        let t = render_text(&r);
        assert!(
            t.contains(&format!(
                "archives       3 files, {ARCHIVES_WARN_BYTES} bytes, oldest 2026-08"
            )),
            "{t}"
        );

        f.runlog = Some(Stats {
            live_bytes: 2 * LIVE_MAX_BYTES + 1,
            archive_bytes: ARCHIVES_WARN_BYTES + 1,
            ..f.runlog.unwrap()
        });
        let r = check(&f);
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
        assert!(
            r.warnings[0].contains("archives total 256 MB"),
            "{:?}",
            r.warnings
        );
        assert!(
            r.warnings[1].contains("rotation is failing"),
            "{:?}",
            r.warnings
        );
        assert_eq!(r.exit, 0);
    }
}
