//! `muzzle init` and doctor's hook entries as processes (mu's every-harness
//! spec D13). Each test gets a private `HOME`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        let home = Home {
            dir: tempfile::tempdir().unwrap(),
        };
        for d in ["home", "state", "config"] {
            std::fs::create_dir_all(home.path(d)).unwrap();
        }
        home
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn write(&self, p: &str, text: &str) {
        let p = self.path(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(&self, p: &str) -> String {
        std::fs::read_to_string(self.path(p)).unwrap()
    }

    fn muzzle(&self, args: &[&str]) -> Output {
        self.muzzle_env(args, &[])
    }

    /// `muzzle` with `env` set on top of the private home.
    fn muzzle_env(&self, args: &[&str], env: &[(&str, PathBuf)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_muzzle"));
        cmd.args(args)
            .current_dir(self.path("home"))
            .env("HOME", self.path("home"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("PI_CODING_AGENT_DIR")
            .stdin(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        self.ok_env(args, &[])
    }

    /// [`Self::ok`] with `env` set, as [`Self::muzzle_env`].
    fn ok_env(&self, args: &[&str], env: &[(&str, PathBuf)]) -> String {
        let out = self.muzzle_env(args, env);
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
}

/// Each harness's settings file, with entries mu, murmur and the user put
/// there, in an order that sorting keys would change.
const FOREIGN: [(&str, &str); 4] = [
    (
        "home/.claude/settings.json",
        r#"{
  "permissions": { "allow": ["Read"] },
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "mu hook stop", "timeout": 600 }] }],
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "./audit.sh" }] }
    ],
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "murmur hook claude" }] }]
  },
  "env": { "Z": "1", "A": "2" }
}
"#,
    ),
    (
        "home/.codex/hooks.json",
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"murmur hook codex"}]}]}}"#,
    ),
    (
        "home/.cursor/hooks.json",
        r#"{"version":1,"hooks":{"beforeShellExecution":[{"command":"./guard.sh"}],"stop":[{"command":"murmur hook cursor-agent"}]}}"#,
    ),
    (
        "home/.config/muse/settings.json",
        r#"{"model":"m","hooks":{"Interrupt":[{"hooks":[{"type":"command","command":"murmur hook muse","async":true}]}]}}"#,
    ),
];

/// `v` without muzzle's handlers, and without the groups and events that
/// held only those.
fn without_muzzle(v: &Value) -> Value {
    let mut v = v.clone();
    let muzzle = |h: &Value| {
        h.get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| c.starts_with("muzzle "))
    };
    if let Some(hooks) = v.get_mut("hooks").and_then(Value::as_object_mut) {
        for entries in hooks.values_mut() {
            let list = entries.as_array_mut().unwrap();
            list.retain(|e| !muzzle(e));
            for g in list.iter_mut() {
                if let Some(inner) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                    inner.retain(|h| !muzzle(h));
                }
            }
            list.retain(|g| {
                g.get("hooks")
                    .and_then(Value::as_array)
                    .is_none_or(|h| !h.is_empty())
            });
        }
        hooks.retain(|_, e| !e.as_array().unwrap().is_empty());
    }
    v
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn init_twice_leaves_foreign_entries_intact() {
    let home = Home::new();
    for (p, text) in FOREIGN {
        home.write(p, text);
    }
    let out = home.ok(&["init"]);
    for h in ["claude", "codex", "cursor-agent", "muse"] {
        assert!(out.contains(&format!("hooks {h}: added ")), "{h}: {out}");
    }
    let first: Vec<String> = FOREIGN.iter().map(|(p, _)| home.read(p)).collect();
    for ((p, before), after) in FOREIGN.iter().zip(&first) {
        let a = json(after);
        assert_eq!(without_muzzle(&a), json(before), "{p}: {after}");
        assert_ne!(without_muzzle(&a), a, "{p}: muzzle's entry is in");
    }
    // Every key keeps its place: sorting would move each of these.
    assert!(
        first[0].find("\"permissions\"") < first[0].find("\"hooks\"")
            && first[0].find("\"hooks\"") < first[0].find("\"env\"")
            && first[0].find("\"Z\"") < first[0].find("\"A\""),
        "{}",
        first[0]
    );
    assert!(first[0].find("\"Stop\"") < first[0].find("\"PreToolUse\""));
    assert!(first[0].find("./audit.sh") < first[0].find("muzzle hook claude"));
    assert!(first[2].find("\"version\"") < first[2].find("\"hooks\""));

    let out = home.ok(&["init"]);
    for h in ["claude", "codex", "cursor-agent", "muse"] {
        assert!(
            out.contains(&format!("hooks {h}: up to date")),
            "{h}: {out}"
        );
    }
    let second: Vec<String> = FOREIGN.iter().map(|(p, _)| home.read(p)).collect();
    assert_eq!(first, second, "a second init is byte-for-byte a no-op");

    let v: Value = serde_json::from_str(&home.ok(&["doctor", "--json"])).unwrap();
    let states: Vec<(&str, &str)> = v["hook_entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["harness"].as_str().unwrap(), c["state"].as_str().unwrap()))
        .collect();
    assert_eq!(
        states,
        [
            ("claude", "ok"),
            ("codex", "ok"),
            ("cursor-agent", "ok"),
            ("muse", "ok")
        ]
    );
    assert_eq!(v["claude_hook"], "installed");
}

#[test]
fn a_fresh_init_writes_the_documented_snippets() {
    let home = Home::new();
    home.ok(&["init", "--hooks", "all"]);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/harnesses");
    for (file, snippet) in [
        ("home/.claude/settings.json", "claude-code.hooks.json"),
        ("home/.codex/hooks.json", "codex.hooks.json"),
        ("home/.cursor/hooks.json", "cursor.hooks.json"),
        ("home/.config/muse/settings.json", "muse.hooks.json"),
    ] {
        let want = std::fs::read_to_string(root.join(snippet)).unwrap();
        assert_eq!(json(&home.read(file)), json(&want), "{file} vs {snippet}");
    }
}

#[test]
fn init_writes_only_set_up_harnesses_unless_asked() {
    let home = Home::new();
    std::fs::create_dir_all(home.path("home/.codex")).unwrap();
    let out = home.ok(&["init"]);
    assert!(out.contains("hooks codex: added PreToolUse"), "{out}");
    assert!(out.contains("trust it once"), "{out}");
    assert!(out.contains("hooks muse: not set up here"), "{out}");
    assert!(!home.path("home/.claude").exists());
    assert!(!home.path("home/.config/muse").exists());
    let v: Value = serde_json::from_str(&home.ok(&["init", "--hooks", "muse", "--json"])).unwrap();
    let states: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["skipped", "skipped", "skipped", "written"]);
    assert_eq!(v[3]["added"][0], "PreToolUse");
    assert!(home.path("home/.config/muse/settings.json").is_file());
    // A typo writes nothing: clap refuses it before init runs.
    let out = home.muzzle(&["init", "--hooks", "claude,claud"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(!home.path("home/.claude").exists());
}

#[test]
fn doctor_reports_drift_and_init_refuses_what_it_cannot_read() {
    let home = Home::new();
    home.write(
        "home/.claude/settings.json",
        r#"{"hooks":{"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"muzzle hook claude"}]}]}}"#,
    );
    home.write("home/.cursor/hooks.json", "[1, 2]");
    let out = home.muzzle(&["doctor"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("hook entries   claude drift, cursor-agent unreadable"),
        "{text}"
    );
    assert!(
        text.contains(
            "missing PreToolUse; stray PostToolUse: muzzle hook claude); run `muzzle init`"
        ),
        "{text}"
    );

    let out = home.muzzle(&["init"]);
    assert_eq!(out.status.code(), Some(125), "{out:?}");
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("hooks cursor-agent: left"), "{err}");
    assert!(err.contains("not a JSON object"), "{err}");
    assert_eq!(home.read("home/.cursor/hooks.json"), "[1, 2]");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("added PreToolUse; removed PostToolUse: muzzle hook claude"),
        "{stdout}"
    );
    let v: Value = serde_json::from_str(&home.ok(&["doctor", "--json"])).unwrap();
    assert_eq!(v["hook_entries"][0]["state"], "ok", "{v}");
}

#[test]
fn init_and_doctor_follow_claude_config_dir_and_codex_home() {
    let home = Home::new();
    let env = [
        ("CLAUDE_CONFIG_DIR", home.path("claude-cfg")),
        ("CODEX_HOME", home.path("codex-cfg")),
    ];
    let out = home.muzzle_env(&["init", "--hooks", "claude,codex"], &env);
    assert!(out.status.success(), "{out:?}");
    assert!(
        home.read("claude-cfg/settings.json")
            .contains("muzzle hook claude")
    );
    assert!(
        home.read("codex-cfg/hooks.json")
            .contains("muzzle hook codex")
    );
    assert!(!home.path("home/.claude").exists());
    assert!(!home.path("home/.codex").exists());
    let out = home.muzzle_env(&["doctor", "--json"], &env);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let entries = v["hook_entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{v}");
    assert_eq!(
        entries[0]["path"],
        home.path("claude-cfg/settings.json").to_str().unwrap()
    );
    assert_eq!(entries[0]["state"], "ok");
    assert_eq!(entries[1]["state"], "ok");
}

#[test]
fn codex_hooked_inline_in_config_toml_is_not_hooked_twice() {
    let home = Home::new();
    home.write(
        "home/.codex/config.toml",
        "[[hooks.PreToolUse]]\nmatcher = \"^Bash$\"\n\n\
         [[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"muzzle hook codex\"\n",
    );
    let text = home.ok(&["doctor"]);
    assert!(
        text.contains("hook entries   codex ok (inline in config.toml)"),
        "{text}"
    );
    let out = home.muzzle(&["init"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("hooks codex: runs from the inline hooks in"),
        "{stdout}"
    );
    assert!(String::from_utf8(out.stderr).unwrap().is_empty());
    assert!(!home.path("home/.codex/hooks.json").exists());
    let v: Value = serde_json::from_str(&home.ok(&["init", "--json"])).unwrap();
    assert_eq!(v[1]["state"], "inline");
    assert!(v[1]["also_in"].as_str().unwrap().ends_with("config.toml"));

    // Both files: init and doctor both say so.
    home.write(
        "home/.codex/hooks.json",
        r#"{"hooks":{"PreToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","command":"muzzle hook codex"}]}]}}"#,
    );
    let err = String::from_utf8(home.muzzle(&["init"]).stderr).unwrap();
    assert!(
        err.contains("also runs `muzzle hook`, so each shell call is matched twice"),
        "{err}"
    );
    let text = home.ok(&["doctor"]);
    assert!(text.contains("hook entries   codex ok"), "{text}");
    assert!(
        text.contains("so each shell call is matched twice"),
        "{text}"
    );
}

#[test]
fn doctor_warns_when_muse_hooks_read_another_config() {
    let home = Home::new();
    let moved = |text: &str| text.contains("Muse Code runs hooks without it");
    // XDG_CONFIG_HOME is set away from ~/.config, but Muse has no entry.
    std::fs::create_dir_all(home.path("home/.claude")).unwrap();
    home.ok(&["init"]);
    assert!(!moved(&home.ok(&["doctor"])));

    home.ok(&["init", "--hooks", "muse"]);
    let text = home.ok(&["doctor"]);
    assert!(moved(&text), "{text}");
    let default = home.path("home/.config/muzzle/config.toml");
    assert!(
        text.contains(&format!("`muzzle hook claude` reads {}", default.display())),
        "{text}"
    );
    let v: Value = serde_json::from_str(&home.ok(&["doctor", "--json"])).unwrap();
    assert!(
        v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| moved(w.as_str().unwrap())),
        "{v}"
    );

    // XDG_CONFIG_HOME at its default.
    let text = home.ok_env(
        &["doctor"],
        &[("XDG_CONFIG_HOME", home.path("home/.config"))],
    );
    assert!(text.contains("hook entries") && !moved(&text), "{text}");

    // The default directory linked to the moved one: the same rules.
    std::fs::create_dir_all(home.path("config/muzzle")).unwrap();
    std::os::unix::fs::symlink(home.path("config/muzzle"), home.path("home/.config/muzzle"))
        .unwrap();
    let text = home.ok(&["doctor"]);
    assert!(text.contains("hook entries") && !moved(&text), "{text}");
}
