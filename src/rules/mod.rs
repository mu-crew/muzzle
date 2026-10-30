//! Unit `rules` (spec §Architecture 5, §Interface Config): config loading,
//! `match`, `wrap`, `hook` formats, and the config hash (D28).

pub mod config;
pub mod heads;
pub mod hook;
pub mod resolve;
pub mod shell;

use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cli::{Class, HookArgs, MatchArgs, WrapArgs};
use crate::exit;
use config::Config;
use resolve::{Effective, Source};

/// A rewrite produced by [`wrap`].
#[derive(Debug, Clone, PartialEq)]
pub struct Wrapped {
    pub command: String,
    pub rule: String,
    pub slots: u32,
}

/// D22: rewrite `command` into `muzzle run … -- bash -c '<command>'` when a
/// rule matches. `None` for no match, or when the command already runs
/// muzzle ([`shell::runs_muzzle`]: a hand-wrapped call, a second hook pass,
/// or a second copy of the pi extension, whose first rewrite starts with
/// the binary's absolute path).
///
/// The class is not passed: `run` resolves it from `--rule` after
/// `MUZZLE_CLASS` (D12), and `--class` here would outrank the environment.
/// A harness timeout becomes `--run-timeout` so it stops counting the
/// queue wait (D24).
pub fn wrap(config: &Config, command: &str, cwd: &Path, timeout_s: Option<f64>) -> Option<Wrapped> {
    let eff = config.resolve(cwd);
    let m = matches(&eff, command)?;
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

/// [`Effective::matches`], except that a command which already runs muzzle
/// matches nothing (D22): `match`, `wrap` and `hook` all agree.
fn matches<'a>(eff: &'a Effective, command: &str) -> Option<resolve::Match<'a>> {
    if shell::runs_muzzle(command) {
        return None;
    }
    eff.matches(command)
}

/// D16a: `command` (shell text) enters a container (`distrobox enter`,
/// `podman exec`, `toolbox run`, `docker exec` and their long forms) in
/// command position. The job then runs in the container's own cgroup, so
/// muzzle's scope admits it but neither caps nor measures it. See
/// [`shell`] for what counts as command position.
pub fn enters_container(command: &str) -> bool {
    shell::text_enters(command)
}

/// [`enters_container`] for a `muzzle run` argv: `bash -c '<script>'`
/// (what [`wrap`] produces) is read as a script, other argvs word by word,
/// so an argument that merely contains `podman exec` doesn't count.
pub fn argv_enters_container(argv: &[String]) -> bool {
    shell::argv_enters(argv)
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

/// The `match --json` object (spec §CLI, 0.2 P8 and B4).
pub fn match_json(config: &Config, command: &str, cwd: &Path) -> Value {
    let eff = config.resolve(cwd);
    match matches(&eff, command) {
        None => json!({ "slots": null }),
        Some(m) => {
            let mut v = json!({
                "slots": m.rule.slots,
                "rule": m.rule.name,
                "class": m.rule.class.map(class_str),
                "source": match m.source {
                    Source::Global => "global",
                    Source::Path(_) => "path",
                },
                "slots_from": {
                    "file": m.slots_from.file.to_string_lossy(),
                    "path": m.slots_from.path,
                },
                // Added field: lets the importer and doctor see which
                // config a label came from (D28).
                "cfg": config.hash(),
            });
            if let Source::Path(key) = m.source {
                v["path"] = json!(key);
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
        let source = match v.get("path").and_then(Value::as_str) {
            Some(p) => format!("path {p}"),
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
            "/abs/path/muzzle run --rule=x -- bash -c 'cargo test'",
            "'/abs/bin/muzzle' run --report '/t/c.json' --rule=gate --slots 6 -- bash -c 'contrib/gate'",
            "env A=1 /x/muzzle run -- cargo test",
            "./muzzle run -- cargo test",
        ] {
            assert!(wrap(&c, cmd, cwd, None).is_none(), "{cmd}");
            assert_eq!(match_json(&c, cmd, cwd), json!({"slots": null}), "{cmd}");
        }
        assert!(wrap(&c, "cargo test", cwd, None).is_some());
        // The skill's `cd repo && muzzle run …`: the first command is `cd`,
        // but a `command` rule doesn't descend into muzzle's argv, so the
        // inner run's own flags apply (SKILL.md; usage.md §Limits).
        for cmd in [
            "cd repo && muzzle run --class high --rule gate -- contrib/fast-gate",
            "cd repo && muzzle run -- cargo test",
            "cd repo && muzzle run -- bash -c 'cargo test'",
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
            c.global[0].name = name.into();
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
    fn container_entries_in_command_position() {
        for cmd in [
            "distrobox enter swayward-dev -- contrib/fast-gate",
            "cd /w && distrobox enter box -- cargo test",
            "FOO=1 podman exec -it c make",
            "timeout 600 toolbox run cargo build",
            "bash -lc 'docker exec c make'",
            "/usr/bin/distrobox enter x -- true",
            "distrobox-enter x -- true",
            "env A=1 distrobox enter x",
            "true; podman exec c sh",
            // rv_cg_detection_false_negatives: long forms and wrappers.
            "podman container exec c make",
            "docker container exec c make",
            "docker compose exec app make",
            "docker compose -f dev.yml exec app make",
            "docker-compose exec app make",
            "podman-compose exec app make",
            "toolbox enter",
            "toolbox enter fedora-toolbox-44",
            "timeout -s KILL 600 distrobox enter x -- make",
            "timeout --signal=KILL -k 5 600 distrobox enter x -- make",
            "command distrobox enter x",
            "! distrobox enter x -- make",
            "echo c | xargs podman exec c",
            "xargs -n1 podman exec c",
            "flatpak-spawn --host distrobox enter x -- make",
            "systemd-inhibit distrobox enter x -- make",
            "systemd-inhibit --why=build distrobox enter x",
            "sudo -u me podman exec c make",
            "if true; then distrobox enter x -- make; fi",
            "for i in 1; do podman exec c make; done",
            "(cd /w && podman exec c make)",
            "x=$(podman exec c make)",
            "sh -c \"docker exec c make\"",
            "docker -H tcp://h exec c make",
            "cat <<EOF\nbody\nEOF\npodman exec c make",
            "cd x\\\n && distrobox enter x -- make",
        ] {
            assert!(enters_container(cmd), "{cmd:?}");
        }
        for cmd in [
            "grep 'podman exec' log",
            "echo distrobox enter",
            "pgrep -f 'distrobox enter'",
            "podman ps",
            "cat docker-exec.md",
            "distrobox list",
            "contrib/fast-gate",
            "toolbox runner",
            // rv_cg_detection_false_positives: quotes, heredocs, -c, --.
            "echo \"x; podman exec c make\"",
            "echo 'a | podman exec c make'",
            "git commit -m 'fix; distrobox enter box'",
            "cat <<EOF\npodman exec c make\nEOF",
            "cat <<'EOF' > notes\ndistrobox enter box -- make\nEOF\nls",
            "grep -c 'podman exec' log",
            "rg -c \"distrobox enter\" .",
            "git log -- distrobox enter",
            "echo podman exec # distrobox enter",
            "docker build .",
            "podman compose up",
            "printf '%s\\n' \"$(echo 'podman exec')\"",
        ] {
            assert!(!enters_container(cmd), "{cmd:?}");
        }
    }

    #[test]
    fn argv_detection_reads_bash_c_as_a_script_and_others_as_words() {
        let v = |ws: &[&str]| ws.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert!(argv_enters_container(&v(&[
            "bash",
            "-c",
            "distrobox enter x -- make"
        ])));
        assert!(argv_enters_container(&v(&[
            "/bin/sh",
            "-lc",
            "cd w; podman exec c make"
        ])));
        assert!(argv_enters_container(&v(&[
            "distrobox",
            "enter",
            "x",
            "--",
            "make"
        ])));
        assert!(argv_enters_container(&v(&["podman", "exec", "c", "make"])));
        assert!(argv_enters_container(&v(&[
            "timeout", "-s", "KILL", "9", "toolbox", "enter"
        ])));
        assert!(!argv_enters_container(&v(&["echo", "podman exec"])));
        assert!(!argv_enters_container(&v(&[
            "grep",
            "-c",
            "podman exec c",
            "log"
        ])));
        assert!(!argv_enters_container(&v(&[
            "bash",
            "-c",
            "echo 'podman exec c make'"
        ])));
    }

    #[test]
    fn match_json_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().display().to_string();
        let text = format!(
            "[[rule]]\nname='gate'\nmatch='gate'\nslots=6\n[[rule]]\nname='t'\nmatch='cargo'\n\
             [[path.\"{key}\".rule]]\nname='p'\nmatch='sweep'\nslots=3\nclass='high'\n\
             [[path.\"{key}\".rule]]\nname='t'\nclass='high'"
        );
        let c = Config::parse(&text, Path::new("/x/t.toml")).unwrap();
        let v = match_json(&c, "contrib/gate", Path::new("/"));
        assert_eq!(
            v,
            json!({"slots":6,"rule":"gate","class":null,"source":"global",
                   "slots_from":{"file":"/x/t.toml","path":null},"cfg":c.hash()})
        );
        let v = match_json(&c, "oracle-sweep", dir.path());
        assert_eq!(
            v,
            json!({"slots":3,"rule":"p","class":"high","source":"path","path":key,
                   "slots_from":{"file":"/x/t.toml","path":key},"cfg":c.hash()})
        );
        // B4: a table that only touched the class still makes it "path",
        // while slots still come from the global list.
        let v = match_json(&c, "cargo test", dir.path());
        assert_eq!((&v["source"], &v["path"]), (&json!("path"), &json!(key)));
        assert_eq!(v["slots_from"], json!({"file":"/x/t.toml","path":null}));
        assert_eq!(match_json(&c, "ls", Path::new("/")), json!({"slots":null}));
    }
}
