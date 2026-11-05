//! What bringing one settings file in line with muzzle's hook entry takes,
//! without I/O: `init` writes the plan, `doctor` reports it as drift.

use serde::Serialize;

use super::json::{Json, Object};

/// How a settings file nests its handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Claude Code's `hooks.<Event>[].hooks[]`, each group with a
    /// `matcher`. Codex and Muse Code's settings use it too.
    Grouped,
    /// cursor-agent's `hooks.<event>[]`, handlers directly, beside a
    /// top-level `version`.
    Flat,
}

/// One harness's muzzle entry: the snippets in `docs/harnesses/`, which
/// `tests/init.rs` checks a fresh `init` against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// The harness name, as murmur's and mu's `init` spell it.
    pub harness: &'static str,
    pub shape: Shape,
    pub event: &'static str,
    /// The group's matcher (grouped shape only): the shell tool's name.
    pub matcher: Option<&'static str>,
    pub command: &'static str,
    /// Fields a new handler gets beside `type` and `command`. Cosmetic, so
    /// never required of an existing one.
    pub extra: &'static [(&'static str, &'static str)],
}

/// Every hook harness's entry. Muse Code sends Claude's `PreToolUse`
/// format, so it runs `hook claude`; its shell tool is `bash`.
pub const TARGETS: [Target; 4] = [
    Target {
        harness: "claude",
        shape: Shape::Grouped,
        event: "PreToolUse",
        matcher: Some("Bash"),
        command: "muzzle hook claude",
        extra: &[],
    },
    Target {
        harness: "codex",
        shape: Shape::Grouped,
        event: "PreToolUse",
        matcher: Some("^Bash$"),
        command: "muzzle hook codex",
        extra: &[("statusMessage", "muzzle: matching the command")],
    },
    Target {
        harness: "cursor-agent",
        shape: Shape::Flat,
        event: "beforeShellExecution",
        matcher: None,
        command: "muzzle hook cursor",
        extra: &[],
    },
    Target {
        harness: "muse",
        shape: Shape::Grouped,
        event: "PreToolUse",
        matcher: Some("bash"),
        command: "muzzle hook claude",
        extra: &[],
    },
];

/// What one file needs. Each entry reads `<event>: <what>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// muzzle-owned (`muzzle hook`) handlers found before any change.
    #[serde(skip)]
    pub owned: usize,
    /// The event had no usable muzzle handler, and gets one.
    pub added: Vec<String>,
    /// muzzle's handler kept but corrected in place.
    pub fixed: Vec<String>,
    /// muzzle handlers removed, as `<event>: <command>`.
    pub removed: Vec<String>,
}

impl Plan {
    pub fn changed(&self) -> bool {
        !(self.added.is_empty() && self.fixed.is_empty() && self.removed.is_empty())
    }
}

/// A handler command is muzzle's to manage: its first two words are
/// `muzzle hook` (D13). muzzle is also a general wrapper, so a handler that
/// runs a gate through `muzzle run` or `muzzle wrap` is the user's, as is
/// one that runs muzzle by absolute path or through a wrapper: all are
/// left alone.
pub fn is_muzzle_hook(command: &str) -> bool {
    let mut words = command.split_whitespace();
    words.next() == Some("muzzle") && words.next() == Some("hook")
}

/// Remove from `list` every muzzle handler but the one `init` would keep,
/// and fix that one in place. `matcher` is the group's (grouped shape);
/// `None` for the flat shape.
fn sweep(
    t: &Target,
    event: &str,
    matcher: Option<Option<&str>>,
    list: &mut Vec<Json>,
    kept: &mut bool,
    plan: &mut Plan,
) {
    let mut i = 0;
    while i < list.len() {
        let Json::Object(handler) = &mut list[i] else {
            i += 1;
            continue;
        };
        let Some(command) = handler
            .get("command")
            .and_then(Json::as_str)
            .filter(|c| is_muzzle_hook(c))
            .map(str::to_string)
        else {
            i += 1;
            continue;
        };
        plan.owned += 1;
        let typed = match handler.get("type") {
            None => true,
            Some(t) => t.as_str() == Some("command"),
        };
        let usable = event == t.event
            && !*kept
            && command == t.command
            && matcher.is_none_or(|m| m == t.matcher)
            && (t.shape == Shape::Flat || typed);
        if !usable {
            plan.removed.push(format!("{event}: {command}"));
            list.remove(i);
            continue;
        }
        *kept = true;
        if t.shape == Shape::Grouped && handler.get("type").is_none() {
            handler.insert("type", Json::str("command"));
            plan.fixed.push(format!("{event}: type"));
        }
        // An async handler runs in the background, so its rewrite never
        // reaches the call.
        if handler
            .get("async")
            .is_some_and(|a| *a != Json::Bool(false))
        {
            handler.remove("async");
            plan.fixed.push(format!("{event}: async"));
        }
        i += 1;
    }
}

/// The handler (and, grouped, the group) `init` appends.
fn new_entry(t: &Target) -> Json {
    match t.shape {
        Shape::Flat => Json::Object(Object::from_pairs(vec![("command", Json::str(t.command))])),
        Shape::Grouped => {
            let mut handler = Object::from_pairs(vec![
                ("type", Json::str("command")),
                ("command", Json::str(t.command)),
            ]);
            for (k, v) in t.extra {
                handler.insert(k, Json::str(v));
            }
            let mut group = Object::new();
            if let Some(m) = t.matcher {
                group.insert("matcher", Json::str(m));
            }
            group.insert("hooks", Json::Array(vec![Json::Object(handler)]));
            Json::Object(group)
        }
    }
}

/// Bring `root`, one parsed settings file, in line with muzzle's entry, in
/// place, and say what that took. `Err` names what muzzle won't guess its
/// way through; the file is then left alone.
///
/// The first muzzle handler that is exactly the one muzzle writes, on its
/// event and under its matcher, is kept wherever it sits, with any field
/// the user added; a missing `type` is set and an `async` dropped. Every
/// other muzzle handler is removed: a duplicate, an older form, one under
/// another matcher or on another event. A group or event list that held
/// only muzzle handlers goes with them. Without a kept handler a new group
/// (or entry) is appended to the event's list. Nothing else is touched.
pub fn plan(t: &Target, root: &mut Object) -> Result<Plan, String> {
    let mut plan = Plan::default();
    let mut hooks = match root.get("hooks") {
        None => Object::new(),
        Some(Json::Object(h)) => h.clone(),
        Some(_) => return Err("its `hooks` is not an object".into()),
    };
    let mut kept = false;
    for event in hooks.keys() {
        let Some(entries) = hooks.get_mut(&event) else {
            continue;
        };
        let Json::Array(entries) = entries else {
            if event == t.event {
                return Err(format!("its `hooks.{event}` is not a list"));
            }
            continue;
        };
        let before = plan.removed.len();
        match t.shape {
            Shape::Flat => sweep(t, &event, None, entries, &mut kept, &mut plan),
            Shape::Grouped => {
                let mut i = 0;
                while i < entries.len() {
                    let mut emptied = false;
                    if let Json::Object(group) = &mut entries[i] {
                        // A missing matcher matches every tool, so it
                        // is never muzzle's: `None` equals no target's.
                        let matcher = group
                            .get("matcher")
                            .and_then(Json::as_str)
                            .map(String::from);
                        if let Some(Json::Array(list)) = group.get_mut("hooks") {
                            let had = list.len();
                            let m = Some(matcher.as_deref());
                            sweep(t, &event, m, list, &mut kept, &mut plan);
                            // One muzzle emptied goes too; one already
                            // empty is not muzzle's.
                            emptied = had > 0 && list.is_empty();
                        }
                    }
                    if emptied {
                        entries.remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
        }
        if entries.is_empty() && plan.removed.len() > before {
            hooks.remove(&event);
        }
    }
    if !kept {
        match hooks.get_mut(t.event) {
            Some(Json::Array(entries)) => entries.push(new_entry(t)),
            _ => hooks.insert(t.event, Json::Array(vec![new_entry(t)])),
        }
        plan.added.push(t.event.to_string());
    }
    if plan.changed() {
        // cursor-agent reads no hook file without its schema version; a new
        // file gets it first, as the snippet has it.
        if t.shape == Shape::Flat && !root.contains_key("version") {
            let mut fresh = Object::from_pairs(vec![("version", Json::Number(1.into()))]);
            for k in root.keys() {
                if let Some(v) = root.get(&k) {
                    fresh.insert(&k, v.clone());
                }
            }
            *root = fresh;
        }
        root.insert("hooks", Json::Object(hooks));
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(harness: &str) -> &'static Target {
        TARGETS.iter().find(|t| t.harness == harness).unwrap()
    }

    fn parse(text: &str) -> Object {
        match serde_json::from_str::<Json>(text).unwrap() {
            Json::Object(o) => o,
            other => panic!("{other:?}"),
        }
    }

    fn text(o: &Object) -> String {
        serde_json::to_string(&Json::Object(o.clone())).unwrap()
    }

    /// Plan `text` for `harness`, then plan the result again: the second
    /// pass must change nothing.
    fn settle(harness: &str, text_in: &str) -> (Plan, String) {
        let t = target(harness);
        let mut root = parse(text_in);
        let first = plan(t, &mut root).unwrap();
        let out = text(&root);
        let mut again = root.clone();
        let second = plan(t, &mut again).unwrap();
        assert!(!second.changed(), "second pass changed {second:?} in {out}");
        assert_eq!(text(&again), out);
        (first, out)
    }

    #[test]
    fn muzzle_hook_decides_ownership() {
        for c in ["muzzle hook claude", "  muzzle  hook", "muzzle\thook codex"] {
            assert!(is_muzzle_hook(c), "{c}");
        }
        for c in [
            "/usr/local/bin/muzzle hook claude",
            "muzzled hook claude",
            "muzzle hooks",
            "muzzle",
            "muzzle run --rule gate -- make check",
            "muzzle wrap -- ./lint.sh",
            "env muzzle hook claude",
            "murmur hook claude",
        ] {
            assert!(!is_muzzle_hook(c), "{c}");
        }
    }

    #[test]
    fn the_users_muzzle_run_and_wrap_handlers_survive() {
        // muzzle's docs suggest wrapping gates and linters by hand; a hook
        // that does so is the user's, on any event.
        let before = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"muzzle run --rule gate -- make check"}]}],"PostToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"muzzle wrap -- ./lint.sh"}]}]}}"#;
        let (p, out) = settle("claude", before);
        assert_eq!(
            (p.added.as_slice(), p.removed.len(), p.owned),
            (["PreToolUse".to_string()].as_slice(), 0, 0)
        );
        assert_eq!(
            out,
            before.replace(
                r#"}]}]}}"#,
                r#"}]}],"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"muzzle hook claude"}]}]}}"#
            )
        );
    }

    #[test]
    fn an_empty_file_gets_exactly_the_snippet() {
        let (p, out) = settle("claude", "{}");
        assert_eq!(p.added, ["PreToolUse"]);
        assert_eq!(
            out,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"muzzle hook claude"}]}]}}"#
        );
        let (_, out) = settle("codex", "{}");
        assert!(
            out.contains(r#""statusMessage":"muzzle: matching the command""#),
            "{out}"
        );
        let (_, out) = settle("cursor-agent", "{}");
        assert_eq!(
            out,
            r#"{"version":1,"hooks":{"beforeShellExecution":[{"command":"muzzle hook cursor"}]}}"#
        );
        let (_, out) = settle("muse", r#"{"model":"m"}"#);
        assert_eq!(
            out,
            r#"{"model":"m","hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"muzzle hook claude"}]}]}}"#
        );
    }

    #[test]
    fn foreign_entries_stay_where_they_were() {
        // murmur's, mu's and the user's handlers, another tool's matcher,
        // and keys muzzle knows nothing of, in an order sorting would break.
        let before = r#"{"permissions":{"allow":["Read"]},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"mu hook stop","timeout":600}]}],"PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"./lint.sh"}]},{"matcher":"Bash","hooks":[{"type":"command","command":"murmur hook claude"}]}],"SessionStart":[{"hooks":[{"type":"command","command":"murmur hook claude"}]}]},"env":{"B":"1","A":"2"}}"#;
        let (p, out) = settle("claude", before);
        assert_eq!(
            (p.added.as_slice(), p.removed.len(), p.owned),
            (["PreToolUse".to_string()].as_slice(), 0, 0)
        );
        let want = before.replace(
            r#""command":"murmur hook claude"}]}],"SessionStart""#,
            r#""command":"murmur hook claude"}]},{"matcher":"Bash","hooks":[{"type":"command","command":"muzzle hook claude"}]}],"SessionStart""#,
        );
        assert_eq!(out, want);
    }

    #[test]
    fn its_own_handler_is_kept_in_place_with_the_users_fields() {
        let before = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"murmur x"},{"type":"command","command":"muzzle hook claude","timeout":30}]}]}}"#;
        let (p, out) = settle("claude", before);
        assert!(!p.changed(), "{p:?}");
        assert_eq!(p.owned, 1);
        assert_eq!(out, before);
    }

    #[test]
    fn stale_and_duplicate_handlers_go_and_their_empty_groups_with_them() {
        let before = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"muzzle hook codex"}]},{"matcher":"Bash","hooks":[{"type":"command","command":"muzzle hook claude"},{"type":"command","command":"muzzle hook claude"},{"type":"command","command":"/opt/bin/muzzle hook claude"}]},{"hooks":[{"type":"command","command":"muzzle hook claude"}]}],"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"muzzle hook claude"}]}]}}"#;
        let (p, out) = settle("claude", before);
        assert!(p.added.is_empty(), "{p:?}");
        assert_eq!(
            p.removed,
            [
                "PreToolUse: muzzle hook codex",
                "PreToolUse: muzzle hook claude",
                "PreToolUse: muzzle hook claude",
                "PostToolUse: muzzle hook claude",
            ]
        );
        assert_eq!(p.owned, 5);
        assert_eq!(
            out,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"muzzle hook claude"},{"type":"command","command":"/opt/bin/muzzle hook claude"}]}]}}"#
        );
    }

    #[test]
    fn a_missing_type_is_set_and_async_dropped() {
        let (p, out) = settle(
            "muse",
            r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"command":"muzzle hook claude","async":true}]}]}}"#,
        );
        assert_eq!(p.fixed, ["PreToolUse: type", "PreToolUse: async"]);
        assert_eq!(
            out,
            r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"command":"muzzle hook claude","type":"command"}]}]}}"#
        );
        // Another type is not muzzle's handler: replaced.
        let (p, _) = settle(
            "claude",
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"prompt","command":"muzzle hook claude"}]}]}}"#,
        );
        assert_eq!(
            (p.removed.len(), p.added.as_slice()),
            (1, ["PreToolUse".to_string()].as_slice())
        );
    }

    #[test]
    fn cursor_keeps_its_version_and_other_events() {
        let before = r#"{"version":2,"hooks":{"stop":[{"command":"murmur hook cursor-agent"}],"beforeShellExecution":[{"command":"./guard.sh"}]}}"#;
        let (p, out) = settle("cursor-agent", before);
        assert_eq!(p.added, ["beforeShellExecution"]);
        assert_eq!(
            out,
            before.replace(
                r#"{"command":"./guard.sh"}"#,
                r#"{"command":"./guard.sh"},{"command":"muzzle hook cursor"}"#
            )
        );
        // An emptied event goes; the version stays.
        let (p, out) = settle(
            "cursor-agent",
            r#"{"version":1,"hooks":{"stop":[{"command":"muzzle hook cursor"}]}}"#,
        );
        assert_eq!(p.removed, ["stop: muzzle hook cursor"]);
        assert_eq!(
            out,
            r#"{"version":1,"hooks":{"beforeShellExecution":[{"command":"muzzle hook cursor"}]}}"#
        );
    }

    #[test]
    fn shapes_it_cannot_read_are_refused() {
        let t = target("claude");
        for bad in [
            r#"{"hooks":[]}"#,
            r#"{"hooks":{"PreToolUse":{"matcher":"Bash"}}}"#,
        ] {
            let mut root = parse(bad);
            assert!(plan(t, &mut root).is_err(), "{bad}");
            assert_eq!(text(&root), bad, "untouched");
        }
        // Another event in a shape muzzle doesn't know is someone else's.
        let (p, _) = settle("claude", r#"{"hooks":{"Stop":"x"}}"#);
        assert_eq!(p.added, ["PreToolUse"]);
    }
}
