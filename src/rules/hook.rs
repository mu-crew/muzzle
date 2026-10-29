//! Claude Code and Codex `PreToolUse` payloads and replies (D22, D23, D24).
//!
//! Both harnesses send `{"tool_name":"Bash","tool_input":{"command":…},
//! "cwd":…}` and accept the same rewrite reply:
//! `hookSpecificOutput.permissionDecision = "allow"` plus `updatedInput`.
//! Sources and stored payloads: `tests/fixtures/hooks/README.md`.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::config::Config;
use crate::cli::Harness;

/// The reply for one payload. `{}` means "no opinion": the harness runs
/// the call as it is.
pub fn reply(harness: Harness, payload: &Value, config: &Config) -> Value {
    let skip = json!({});
    if payload.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return skip;
    }
    let Some(input) = payload.get("tool_input").and_then(Value::as_object) else {
        return skip;
    };
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return skip;
    };
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let Some(wrapped) = super::wrap(config, command, &cwd, timeout_s(harness, input)) else {
        return skip;
    };
    // Claude replaces the whole input with `updatedInput`, so every other
    // field (description, timeout, run_in_background) is carried over. The
    // harness timeout stays: muzzle can't lift it (D24), only pass it on as
    // --run-timeout. Codex only reads `command` but tolerates the copy.
    let mut updated = input.clone();
    updated.insert("command".into(), Value::String(wrapped.command));
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            // `allow` is required for the harness to accept updatedInput,
            // and also skips its permission prompt (spec §CLI, my call).
            "permissionDecision": "allow",
            "updatedInput": updated,
        }
    })
}

/// The payload's timeout in seconds. Claude's Bash `timeout` is in ms.
/// Zero, negative and non-finite values pass through: `wrap` drops them.
/// Codex's Bash `tool_input` carries only `command` (checked against
/// openai/codex `exec_command.rs` at 3535c22), so it has none.
fn timeout_s(harness: Harness, input: &serde_json::Map<String, Value>) -> Option<f64> {
    match harness {
        Harness::Claude => input
            .get("timeout")
            .and_then(Value::as_f64)
            .map(|ms| ms / 1000.0),
        Harness::Codex => None,
    }
}

/// Load the fixture payload at `path` (tests only).
#[cfg(test)]
fn fixture(path: &str) -> Value {
    let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks")
        .join(path);
    serde_json::from_str(&std::fs::read_to_string(&full).unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn config() -> Config {
        Config::parse(super::super::config::TEMPLATE, Path::new("t.toml")).unwrap()
    }

    fn updated(reply: &Value) -> &serde_json::Map<String, Value> {
        let out = &reply["hookSpecificOutput"];
        assert_eq!(out["hookEventName"], "PreToolUse");
        assert_eq!(out["permissionDecision"], "allow");
        out["updatedInput"].as_object().unwrap()
    }

    #[test]
    fn claude_bash_match_is_rewritten_keeping_other_fields() {
        let r = reply(
            Harness::Claude,
            &fixture("claude-bash-match.json"),
            &config(),
        );
        let u = updated(&r);
        assert_eq!(
            u["command"],
            "muzzle run --rule=gate --slots 6 --run-timeout 600 -- bash -c 'contrib/fast-gate'"
        );
        assert_eq!(u["timeout"], 600000);
        assert_eq!(u["description"], "Run the gate");
        assert_eq!(u["run_in_background"], false);
    }

    #[test]
    fn claude_without_timeout_has_no_run_timeout() {
        let r = reply(
            Harness::Claude,
            &fixture("claude-bash-no-timeout.json"),
            &config(),
        );
        assert_eq!(
            updated(&r)["command"],
            "muzzle run --rule=cargo-test --slots 2 -- bash -c 'cargo test --workspace'"
        );
    }

    #[test]
    fn claude_zero_or_negative_timeout_has_no_run_timeout() {
        for t in [json!(0), json!(-5000)] {
            let mut payload = fixture("claude-bash-match.json");
            payload["tool_input"]["timeout"] = t.clone();
            let r = reply(Harness::Claude, &payload, &config());
            let u = updated(&r);
            assert_eq!(
                u["command"], "muzzle run --rule=gate --slots 6 -- bash -c 'contrib/fast-gate'",
                "timeout {t}"
            );
            assert_eq!(u["timeout"], t, "the harness timeout is carried over");
        }
    }

    #[test]
    fn codex_bash_match_is_rewritten() {
        let r = reply(Harness::Codex, &fixture("codex-bash-match.json"), &config());
        let u = updated(&r);
        assert_eq!(
            u["command"],
            "muzzle run --rule=js-test --slots 2 -- bash -c 'npm run test -- --run'"
        );
        assert_eq!(u.len(), 1, "codex Bash input is only `command`");
    }

    #[test]
    fn no_match_other_tools_and_wrapped_commands_get_empty_replies() {
        let c = config();
        for (h, f) in [
            (Harness::Claude, "claude-bash-nomatch.json"),
            (Harness::Claude, "claude-write.json"),
            (Harness::Claude, "claude-bash-already-wrapped.json"),
            (Harness::Codex, "codex-bash-nomatch.json"),
            (Harness::Codex, "codex-apply-patch.json"),
        ] {
            assert_eq!(reply(h, &fixture(f), &c), json!({}), "{f}");
        }
    }

    #[test]
    fn project_rules_use_the_payload_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "[[project.\"{}\".rule]]\nname='p'\nmatch='make'\nslots=3",
            dir.path().display()
        );
        let c = Config::parse(&text, Path::new("t.toml")).unwrap();
        let mut payload = fixture("codex-bash-nomatch.json");
        payload["tool_input"]["command"] = json!("make -j");
        assert_eq!(reply(Harness::Codex, &payload, &c), json!({}));
        payload["cwd"] = json!(dir.path());
        let r = reply(Harness::Codex, &payload, &c);
        assert!(
            updated(&r)["command"]
                .as_str()
                .unwrap()
                .contains("--rule=p --slots 3")
        );
    }
}
