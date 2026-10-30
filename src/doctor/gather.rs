//! Gathering [`Facts`] from the host: the config, the state dir,
//! `runs.jsonl` and each harness's hook or extension.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Facts, Harnesses, RunsLog, read_config};
use crate::runlog;
use crate::state::Store;

/// The recent run lines: the newest archive plus `runs.jsonl`, so a
/// rotation on the 1st doesn't blank the runs line or the D16a container
/// rules (rv020r_doctor_live_only). An unreadable archive falls back to the
/// live file alone: doctor still answers.
fn recent_runs(dir: &Path) -> String {
    runlog::recent(dir)
        .or_else(|_| std::fs::read_to_string(dir.join(runlog::LIVE)))
        .unwrap_or_default()
}

/// The live file's size, and the count and age range of `recent` (see
/// [`recent_runs`]); `None` when there are no runs and no live file.
fn runs_log(dir: &Path, recent: &str) -> Option<RunsLog> {
    let bytes = std::fs::metadata(dir.join(runlog::LIVE)).map(|m| m.len());
    if bytes.is_err() && recent.is_empty() {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct End {
        end: String,
    }
    let ends: Vec<String> = recent
        .lines()
        .filter_map(|l| serde_json::from_str::<End>(l).ok().map(|e| e.end))
        .collect();
    Some(RunsLog {
        bytes: bytes.unwrap_or(0),
        lines: recent.lines().filter(|l| !l.trim().is_empty()).count(),
        oldest: ends.iter().min().cloned(),
        newest: ends.iter().max().cloned(),
    })
}

/// Rules with a run line marked `enforced_reason: "container"`.
fn container_runs(text: &str) -> Vec<String> {
    let (runs, _) = crate::report::parse(text);
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

pub(super) fn gather() -> Facts {
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
    let recent = recent_runs(&state_dir);
    Facts {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cores: crate::proc::logical_cores().ok().map(|c| c as u32),
        ram: crate::proc::total_ram().ok(),
        config_path,
        config_exists,
        config: config.map_err(|e| e.to_string()),
        stored,
        runs: runs_log(&state_dir, &recent),
        runlog: runlog::stats(&state_dir).ok(),
        container_runs: container_runs(&recent),
        state_dir,
        state_writable,
        state_readable,
        systemd: crate::proc::systemd_user_reachable(),
        harnesses: harnesses(&home, &cwd),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_runs_come_from_the_log() {
        let line = |rule: &str, reason: &str| {
            format!(
                "{{\"v\":1,\"end\":\"2026-10-29T00:00:00Z\",\"rule\":\"{rule}\",\"slots\":1,\
                 \"class\":\"normal\",\"cfg\":null,\"agent\":null,\"cwd\":\"/\",\"waited_s\":0,\
                 \"ran_s\":1,\"exit\":0,\"killed\":null,\"cpu_s\":null,\"mem_peak_mb\":null,\
                 \"enforced\":false,\"nested\":false,\"kept\":false,\"budget\":4{reason}}}\n"
            )
        };
        let text = line("box", ",\"enforced_reason\":\"container\"") + &line("host", "");
        assert_eq!(container_runs(&text), ["box"]);
    }

    #[test]
    fn runs_log_reads_size_and_range() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runs.jsonl");
        assert_eq!(runs_log(dir.path(), &recent_runs(dir.path())), None);
        std::fs::write(
            &p,
            "{\"end\":\"2026-10-05T00:00:00Z\"}\n{\"end\":\"2026-10-01T00:00:00Z\"}\nbad\n",
        )
        .unwrap();
        let l = runs_log(dir.path(), &recent_runs(dir.path())).unwrap();
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
