//! Unit `rules` (spec §Architecture 5, §Interface Config): config loading,
//! `match`, `wrap`, `hook` formats, and the config hash (D28).

pub mod config;
pub mod hook;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::cli::{Class, HookArgs, MatchArgs, WrapArgs};
use crate::exit;
use config::{Config, Source};

/// A rewrite produced by [`wrap`].
#[derive(Debug, Clone, PartialEq)]
pub struct Wrapped {
    pub command: String,
    pub rule: String,
    pub slots: u32,
}

/// D22: rewrite `command` into `muzzle run … -- bash -c '<command>'` when a
/// rule matches. `None` for no match, or when the command already starts
/// with `muzzle ` (a hand-wrapped call, or a second hook pass).
///
/// The class is not passed: `run` resolves it from `--rule` after
/// `MUZZLE_CLASS` (D12), and `--class` here would outrank the environment.
/// A harness timeout becomes `--run-timeout` so it stops counting the
/// queue wait (D24).
pub fn wrap(config: &Config, command: &str, cwd: &Path, timeout_s: Option<f64>) -> Option<Wrapped> {
    if command.trim_start().starts_with("muzzle ") {
        return None;
    }
    let m = config.matches(command, cwd)?;
    let mut out = format!(
        "muzzle run --rule={} --slots {}",
        quote_word(&m.rule.name),
        m.rule.slots
    );
    if let Some(t) = timeout_s.filter(|t| t.is_finite() && *t > 0.0) {
        out.push_str(&format!(" --run-timeout {}", fmt_secs(t)));
    }
    out.push_str(" -- bash -c ");
    out.push_str(&quote(command));
    Some(Wrapped {
        command: out,
        rule: m.rule.name.clone(),
        slots: m.rule.slots,
    })
}

/// POSIX single-quoting: everything inside '…' is literal, including `$`,
/// backslashes and newlines (so heredocs survive). A `'` closes the quote,
/// adds an escaped quote, and reopens: `'\''`.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Quote only when needed, so common labels stay readable.
fn quote_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'));
    if plain { s.to_string() } else { quote(s) }
}

/// `600` for whole seconds (pi and Claude timeouts), else the shortest
/// float form.
fn fmt_secs(t: f64) -> String {
    if t.fract() == 0.0 && t < 1e15 {
        format!("{}", t as u64)
    } else {
        format!("{t}")
    }
}

fn cwd_or_current(cwd: &Option<PathBuf>) -> PathBuf {
    cwd.clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Load the config, or report it and return the exit code: 2 for a bad
/// config (spec §Exit codes), 125 when it can't be read.
fn load() -> Result<Config, ExitCode> {
    Config::load().map_err(|e| {
        eprintln!("muzzle: {e}");
        match e {
            config::ConfigError::Read { .. } => ExitCode::from(exit::INTERNAL),
            _ => ExitCode::from(exit::USAGE),
        }
    })
}

fn class_str(c: Class) -> &'static str {
    match c {
        Class::High => "high",
        Class::Normal => "normal",
    }
}

/// The `match --json` object (spec §CLI).
pub fn match_json(config: &Config, command: &str, cwd: &Path) -> Value {
    match config.matches(command, cwd) {
        None => json!({ "slots": null }),
        Some(m) => {
            let mut v = json!({
                "slots": m.rule.slots,
                "rule": m.rule.name,
                "class": m.rule.class.map(class_str),
                "source": match m.source {
                    Source::Global => "global",
                    Source::Project(_) => "project",
                },
                // Added field: lets the importer and doctor see which
                // config a label came from (D28).
                "cfg": config.hash(),
            });
            if let Source::Project(key) = m.source {
                v["project"] = json!(key);
            }
            v
        }
    }
}

pub fn match_main(args: &MatchArgs) -> anyhow::Result<ExitCode> {
    let config = match load() {
        Ok(c) => c,
        Err(code) => return Ok(code),
    };
    let v = match_json(&config, &args.command, &cwd_or_current(&args.cwd));
    if args.json {
        println!("{v}");
    } else if v["slots"].is_null() {
        println!("no rule matches");
    } else {
        let source = match v.get("project").and_then(Value::as_str) {
            Some(p) => format!("project {p}"),
            None => "global".into(),
        };
        let class = v["class"]
            .as_str()
            .map(|c| format!(", class {c}"))
            .unwrap_or_default();
        println!(
            "{} ({} slots{class}, {source})",
            v["rule"].as_str().unwrap_or(""),
            v["slots"]
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn wrap_main(args: &WrapArgs) -> anyhow::Result<ExitCode> {
    let config = match load() {
        Ok(c) => c,
        Err(code) => return Ok(code),
    };
    let v = match wrap(
        &config,
        &args.command,
        &cwd_or_current(&args.cwd),
        args.timeout,
    ) {
        None => json!({ "command": null }),
        Some(w) => json!({ "command": w.command, "rule": w.rule, "slots": w.slots }),
    };
    println!("{v}");
    Ok(ExitCode::SUCCESS)
}

/// Exit code for a hook that can't answer. Not 2: both Claude Code and
/// Codex read exit 2 as "block this tool call", and a broken muzzle must
/// never block every Bash call. 1 is a non-blocking hook error in both, so
/// the call runs unwrapped and the user sees the notice.
const HOOK_FAILED: u8 = 1;

pub fn hook_main(args: &HookArgs) -> anyhow::Result<ExitCode> {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("muzzle hook: reading stdin: {e}");
        return Ok(ExitCode::from(HOOK_FAILED));
    }
    let payload: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("muzzle hook: payload is not JSON: {e}");
            return Ok(ExitCode::from(HOOK_FAILED));
        }
    };
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("muzzle hook: {e}");
            return Ok(ExitCode::from(HOOK_FAILED));
        }
    };
    println!("{}", hook::reply(args.harness, &payload, &config));
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn config() -> Config {
        Config::parse(config::TEMPLATE, Path::new("t.toml")).unwrap()
    }

    /// What bash hands to `bash -c` after parsing a quoted word.
    fn bash_roundtrip(s: &str) -> String {
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!("printf %s {}", quote(s)))
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn quoting_survives_bash() {
        for s in [
            "plain",
            "it's",
            "''",
            "$HOME ${X:-y} $(id) `id`",
            "a\nb\n\nc",
            "back\\slash \\' \\n",
            "\"double\" and 'single'",
            "tab\there; echo && | > < * ? [a] ~ # !",
            "",
            "ünïcode ✓",
        ] {
            assert_eq!(bash_roundtrip(s), s, "{s:?}");
        }
    }

    #[test]
    fn heredoc_runs_through_the_wrap() {
        let cmd = "cat <<'EOF'\nit's $HOME\nEOF\ncat <<EOF\nx=$((1+2))\nEOF";
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!("bash -c {}", quote(cmd)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "it's $HOME\nx=3\n");
    }

    #[test]
    fn wrap_shape_and_timeout() {
        let c = config();
        let cwd = Path::new("/");
        let w = wrap(&c, "contrib/fast-gate --all", cwd, None).unwrap();
        assert_eq!(
            w,
            Wrapped {
                command: "muzzle run --rule=gate --slots 6 -- bash -c 'contrib/fast-gate --all'"
                    .into(),
                rule: "gate".into(),
                slots: 6,
            }
        );
        let w = wrap(&c, "cargo test", cwd, Some(120.0)).unwrap();
        assert!(
            w.command.contains(" --run-timeout 120 -- "),
            "{}",
            w.command
        );
        let w = wrap(&c, "cargo test", cwd, Some(1.5)).unwrap();
        assert!(
            w.command.contains(" --run-timeout 1.5 -- "),
            "{}",
            w.command
        );
        let w = wrap(&c, "cargo test", cwd, Some(0.0)).unwrap();
        assert!(!w.command.contains("--run-timeout"), "{}", w.command);
        assert!(wrap(&c, "ls", cwd, None).is_none());
    }

    #[test]
    fn wrap_leaves_muzzle_commands_alone() {
        let c = config();
        let cwd = Path::new("/");
        for cmd in [
            "muzzle run --rule gate -- contrib/gate",
            "  muzzle run -- cargo test",
            "muzzle status",
        ] {
            assert!(wrap(&c, cmd, cwd, None).is_none(), "{cmd}");
        }
        // Not a muzzle invocation, just a matching command mentioning it.
        assert!(wrap(&c, "muzzlefoo; cargo test", cwd, None).is_some());
        // Wrapping a wrap is a no-op.
        let w = wrap(&c, "cargo test", cwd, None).unwrap();
        assert!(wrap(&c, &w.command, cwd, None).is_none());
    }

    #[test]
    fn wrapped_command_runs_the_original_string() {
        let c = Config::parse(
            "[[rule]]\nname=\"odd label\"\nmatch='printf'",
            Path::new("t.toml"),
        )
        .unwrap();
        let cmd = "printf '%s|' \"it's\" '$HOME' \"a\nb\"";
        let w = wrap(&c, cmd, Path::new("/"), None).unwrap();
        let prefix = "muzzle run --rule='odd label' --slots 1 -- ";
        let inner = w.command.strip_prefix(prefix).expect(&w.command);
        let run = |s: &str| {
            let out = Command::new("bash").arg("-c").arg(s).output().unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(run(inner), run(cmd));
        assert_eq!(run(inner), "it's|$HOME|a\nb|");
    }

    /// `wrap`'s output split by bash and parsed by clap: the rule name
    /// arrives intact even when it looks like a flag. `Config::parse`
    /// rejects such names; this guards configs built any other way.
    #[test]
    fn wrapped_rule_name_survives_clap() {
        use clap::Parser;
        for name in ["-x", "--slots", "odd label", "it's"] {
            let mut c = config();
            c.rules[0].name = name.into();
            let w = wrap(&c, "contrib/gate", Path::new("/"), None).unwrap();
            let out = Command::new("bash")
                .arg("-c")
                .arg(format!("printf '%s\\0' {}", w.command))
                .output()
                .unwrap();
            let words: Vec<String> = String::from_utf8(out.stdout)
                .unwrap()
                .split_terminator('\0')
                .map(String::from)
                .collect();
            let cli = crate::cli::Cli::try_parse_from(&words)
                .unwrap_or_else(|e| panic!("{name:?}: {}: {e}", w.command));
            let crate::cli::Command::Run(run) = cli.command else {
                panic!("{name:?}: not a run: {}", w.command);
            };
            assert_eq!(run.rule.as_deref(), Some(name), "{}", w.command);
            assert_eq!(run.slots, Some(6));
        }
    }

    #[test]
    fn match_json_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "[[rule]]\nname='gate'\nmatch='gate'\nslots=6\n[[project.\"{}\".rule]]\nname='p'\nmatch='sweep'\nslots=3\nclass='high'",
            dir.path().display()
        );
        let c = Config::parse(&text, Path::new("t.toml")).unwrap();
        let v = match_json(&c, "contrib/gate", Path::new("/"));
        assert_eq!(
            v,
            json!({"slots":6,"rule":"gate","class":null,"source":"global","cfg":c.hash()})
        );
        let v = match_json(&c, "oracle-sweep", dir.path());
        assert_eq!(v["source"], "project");
        assert_eq!(v["class"], "high");
        assert_eq!(v["project"], dir.path().display().to_string());
        assert_eq!(match_json(&c, "ls", Path::new("/")), json!({"slots":null}));
    }
}
