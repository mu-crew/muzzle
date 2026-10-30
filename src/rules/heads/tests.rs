use super::*;
use crate::rules::config::Config;
use std::path::Path;

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

/// The linuxpc rules as command rules (u021_check writes the real file),
/// plus vtmux's path rule made global.
const RULES: &str = r#"
[[rule]]
name = "fast-gate"
command = "contrib/fast-gate"
[[rule]]
name = "gate"
command = "contrib/gate"
[[rule]]
name = "js-test"
command = ["vitest", "npm test", "npm run test*"]
[[rule]]
name = "cargo-test"
command = ["cargo test", "cargo nextest", "cargo clippy", "cargo build"]
[[rule]]
name = "vtmux-check"
command = "scripts/check.sh"
"#;

fn rule_of(c: &Config, text: &str) -> Option<String> {
    c.resolve_target(None)
        .matches(text)
        .map(|m| m.rule.name.clone())
}

#[test]
fn commands_get_their_rule() {
    let c = Config::parse(RULES, Path::new("/t.toml")).unwrap();
    let cases: &[(&str, Option<&str>)] = &[
        // Plain, and C2 rules 3-4.
        ("cargo test", Some("cargo-test")),
        ("/usr/bin/cargo test", Some("cargo-test")),
        ("./contrib/fast-gate", Some("fast-gate")),
        ("/abs/x/contrib/fast-gate --all", Some("fast-gate")),
        ("cargo --locked test", Some("cargo-test")),
        ("cargo +nightly test -p x", Some("cargo-test")),
        ("cargo +nightly fmt", None),
        ("cargo testx", None),
        ("(cd x && cargo test)", Some("cargo-test")),
        ("env -C /x cargo test", Some("cargo-test")),
        ("echo $(cargo build)", Some("cargo-test")),
        // Not command words.
        ("pgrep -f 'contrib/fast-gate'", None),
        ("cat scripts/check.sh", None),
        ("echo \"cargo test\"", None),
        ("git log -- contrib/gate", None),
        ("grep -E vitest", None),
        ("cat <<EOF\ncargo test\nEOF", None),
        ("ls # cargo test", None),
        // Refuter 9 / R4 program option arguments.
        ("cargo -Z x test", Some("cargo-test")),
        ("cargo -Z unstable-options test", Some("cargo-test")),
        ("cargo --config k=v test", Some("cargo-test")),
        ("cargo -C dir build", Some("cargo-test")),
        ("npm --prefix d test", Some("js-test")),
        ("npm -w pkg run test", Some("js-test")),
        // Refuter 2 / R2.
        ("npm run test:fast", Some("js-test")),
        ("npm run test:stress -- --x", Some("js-test")),
        ("npm run build", None),
        // Refuter 1 / R1: script programs.
        (
            "/tmp/fx17-in.sh 'contrib/gate > /tmp/fx17-gate.log 2>&1; echo rc=$?'",
            Some("gate"),
        ),
        ("/tmp/x.sh 'contrib/gate > l'", Some("gate")),
        (
            "/tmp/dbx.sh 2400 \"contrib/gate 2>&1 | tail -25\"",
            Some("gate"),
        ),
        (
            "/tmp/fx17-in.sh 'timeout 300 cargo test --features test-auth'",
            Some("cargo-test"),
        ),
        (
            ".review-private/exec.sh npx vitest run test/x.test.ts",
            Some("js-test"),
        ),
        ("exec.sh npx vitest run", Some("js-test")),
        (
            "distrobox enter swayward-dev -- bash tmp/run.sh cargo build -q",
            Some("cargo-test"),
        ),
        (
            "distrobox enter swayward-dev -- /tmp/fa2-in.sh bash -c 'contrib/gate --fast'",
            Some("gate"),
        ),
        // Refuter 4 / R1.1: the script file is the program.
        ("bash scripts/check.sh", Some("vtmux-check")),
        ("scripts/check.sh", Some("vtmux-check")),
        ("mise exec -- bash scripts/check.sh", Some("vtmux-check")),
        (
            "(time mise exec -- bash scripts/check.sh 2>&1 | tail)",
            Some("vtmux-check"),
        ),
        ("bash -x -o pipefail scripts/check.sh", Some("vtmux-check")),
        // `-n` only parses the file (museum check, u021_check).
        ("bash -n contrib/gate", None),
        ("bash -n contrib/PKGBUILD contrib/gate", None),
        ("sh -en scripts/check.sh", None),
        // Refuter 5 / R3: a variable program.
        (
            "D=\"distrobox enter b --\"; $D bash -c 'contrib/gate'",
            Some("gate"),
        ),
        ("${D} cargo test", Some("cargo-test")),
        // Refuter 6 / R4 descent.
        (
            "distrobox enter b -- bash -lc 'contrib/fast-gate'",
            Some("fast-gate"),
        ),
        (
            "distrobox enter -n b -- contrib/fast-gate",
            Some("fast-gate"),
        ),
        ("distrobox-enter b -- cargo test", Some("cargo-test")),
        (
            "podman exec -w \"$PWD\" b sh -c 'contrib/gate'",
            Some("gate"),
        ),
        (
            "podman exec -w \"$PWD\" swayward-dev sh -c 'timeout 900 contrib/gate --fast'",
            Some("gate"),
        ),
        (
            "podman exec --user 1000:1000 --workdir \"$PWD\" b env A=1 cargo test",
            Some("cargo-test"),
        ),
        ("docker exec -it c cargo test", Some("cargo-test")),
        (
            "toolbox run -c b mise exec -- cargo test",
            Some("cargo-test"),
        ),
        ("toolbox run cargo build", Some("cargo-test")),
        ("mise x -- cargo test", Some("cargo-test")),
        ("flock -w 600 /l contrib/gate", Some("gate")),
        ("flock /l -c 'contrib/gate'", Some("gate")),
        ("flock -E 3 /l cargo test", Some("cargo-test")),
        ("npx vitest run", Some("js-test")),
        ("npx -p vitest vitest run", Some("js-test")),
        ("uv run --with x cargo test", Some("cargo-test")),
        // R7: a bare fd lock runs nothing.
        ("flock 9", None),
        // A container argument isn't its command.
        ("podman exec -w cargo b ls", None),
    ];
    for (text, want) in cases {
        assert_eq!(rule_of(&c, text).as_deref(), *want, "{text:?}");
    }
}

#[test]
fn head_matching_rules() {
    let m = |head: &str, pat: &str| head_matches(&words(head), &words(pat));
    assert!(m("cargo test", "cargo"));
    assert!(m("cargo test -p x extra", "cargo test"));
    assert!(m("x/cargo test", "cargo test"));
    assert!(!m("xcargo test", "cargo test"));
    assert!(!m("cargo", "cargo test"));
    assert!(m("contrib/gate", "gate"));
    assert!(!m("contrib/gatex", "contrib/gate"));
    assert!(m("npm run test", "npm run test*"));
    assert!(!m("npm run tes", "npm run test*"));
    assert!(m("vitest-x run", "vitest*"));
    // An R4 option argument is skipped only for its program.
    assert!(!m("git -C test log", "git log"));
    assert!(m("cargo -C test build", "cargo build"));
    let x = |head: &str, pat: &str| head_matches_exactly(&words(head), &words(pat));
    assert!(x("cargo test", "cargo test"));
    assert!(x("./cargo test", "cargo test"));
    assert!(x("./contrib/gate", "contrib/gate"));
    assert!(!x("/tmp/evil/cargo test", "cargo test"));
    assert!(!x("/x/contrib/gate", "contrib/gate"));
    assert!(!x("../cargo test", "cargo test"));
}

#[test]
fn heads_keep_the_outer_command_and_add_inner_ones() {
    let h = command_heads("timeout 9 distrobox enter b -- bash -lc 'A=1 cargo test'");
    assert_eq!(
        h,
        vec![
            words("distrobox enter b -- bash -lc A=1_cargo_test")
                .into_iter()
                .map(|w| w.replace('_', " "))
                .collect::<Vec<_>>(),
            words("bash -lc A=1_cargo_test")
                .into_iter()
                .map(|w| w.replace('_', " "))
                .collect(),
            words("cargo test"),
        ]
    );
}

#[test]
fn nesting_is_bounded() {
    let mut t = "cargo test".to_string();
    for _ in 0..(MAX_DEPTH + 2) {
        t = format!("bash -c {}", crate::rules::quote(&t));
    }
    let found = |t: &str| {
        command_heads(t)
            .iter()
            .any(|h| head_matches(h, &words("cargo test")))
    };
    let mut shallow = "cargo test".to_string();
    for _ in 0..MAX_DEPTH {
        shallow = format!("bash -c {}", crate::rules::quote(&shallow));
    }
    assert!(found(&shallow));
    // Deeper than MAX_DEPTH: no head for the innermost, and no hang.
    assert!(!found(&t));
}

/// bench/linuxpc-config-021.toml, the shipped command-rule config (C6),
/// with cases from the museum check (docs/eval/2026-10-30-command-rules.md).
#[test]
fn linuxpc_021_config_labels_museum_shapes() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/linuxpc-config-021.toml");
    // The vtmux table moved to a directory that exists everywhere, so its
    // key canonicalises the same in CI.
    let dir = std::env::temp_dir().canonicalize().unwrap();
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("~/hacking/vtmux", &dir.to_string_lossy());
    let c = Config::parse(&text, &path).unwrap();
    let vtmux = Some(dir.as_path());
    let at = |text: &str, target: Option<&Path>| {
        c.resolve_target(target)
            .matches(text)
            .map(|m| (m.rule.name.clone(), m.rule.slots))
    };
    let r = |name: &str, slots: u32| Some((name.to_string(), slots));
    assert_eq!(at("contrib/fast-gate", None), r("fast-gate", 6));
    assert_eq!(
        at("distrobox enter b -- bash -lc 'contrib/gate'", None),
        r("gate", 6)
    );
    assert_eq!(at("npm run test:fast", None), r("js-test", 5));
    assert_eq!(at("npx vitest run t.test.ts", None), r("js-test", 5));
    assert_eq!(at("taskset -c 0-3 cargo test -q", None), r("cargo-test", 3));
    assert_eq!(at("cargo +nightly fmt --check", None), None);
    assert_eq!(at("ps aux | grep -c 'cargo build'", None), None);
    assert_eq!(at("bash -n contrib/gate", None), None);
    // vtmux's table: its own rules first, cargo-test reweighted.
    assert_eq!(
        at("mise exec -- bash scripts/check.sh", vtmux),
        r("vtmux-check", 4)
    );
    assert_eq!(
        at("cargo xtask regress --tmux r", vtmux),
        r("vtmux-harness", 2)
    );
    assert_eq!(at("cargo test -p vtmux-core", vtmux), r("cargo-test", 4));
    assert_eq!(at("scripts/check.sh", None), None);
    // oracle-sweep stays a match rule, first (R7).
    assert_eq!(
        at(
            "L=/x/oracle-sweep.lock; exec 9>>$L; flock 9; cargo test",
            None
        ),
        r("oracle-sweep", 999)
    );
}
