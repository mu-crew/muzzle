//! Every muzzle config example in the docs must load (0.2 P1–P6, B2): a doc
//! that still shows `[project]`, an entry with no `match` for a rule not in
//! effect, or a budget key in a path table fails here, not in an agent's
//! hands. Commands in the same docs are checked by `doc_commands`.
//!
//! A block whose first line names `machine.toml` is an include: it is
//! written as `machine.toml` beside the block before it in the same file,
//! which then loads with it. `docs/harnesses/` is left out: its TOML is
//! Codex's config, not muzzle's.

use std::path::Path;

use muzzle::rules::config::Config;

const FILES: &[&str] = &[
    "README.md",
    "docs/usage.md",
    "npm/muzzle/skills/muzzle/SKILL.md",
    "npm/muzzle/skills/muzzle/recipes/tune.md",
    "docs/specs/2026-10-29-muzzle.md",
    "docs/specs/2026-10-30-muzzle-0.2-paths.md",
];

/// Each ```toml block with the line it starts on.
fn toml_blocks(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur: Option<(usize, String)> = None;
    for (i, l) in text.lines().enumerate() {
        match &mut cur {
            None if l.trim_start() == "```toml" => cur = Some((i + 1, String::new())),
            Some(_) if l.trim_start() == "```" => out.push(cur.take().unwrap()),
            Some((_, body)) => {
                body.push_str(l);
                body.push('\n');
            }
            None => {}
        }
    }
    out
}

fn is_machine_file(block: &str) -> bool {
    block
        .lines()
        .next()
        .is_some_and(|l| l.contains("machine.toml"))
        && !block.contains("include")
}

#[test]
fn every_config_example_loads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0;
    for f in FILES {
        let text = std::fs::read_to_string(root.join(f)).unwrap();
        let blocks = toml_blocks(&text);
        for (i, (line, block)) in blocks.iter().enumerate() {
            if is_machine_file(block) {
                continue; // loaded with the block before it
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            std::fs::write(&path, block).unwrap();
            if let Some((_, next)) = blocks.get(i + 1).filter(|(_, b)| is_machine_file(b)) {
                std::fs::write(dir.path().join("machine.toml"), next).unwrap();
            }
            let cfg = Config::load_from(&path)
                .unwrap_or_else(|e| panic!("{f}:{line}: example doesn't load: {e}"));
            // A machine file shown next to the root must actually be found.
            if blocks.get(i + 1).is_some_and(|(_, b)| is_machine_file(b)) {
                assert!(
                    cfg.includes().iter().all(|(_, found)| *found),
                    "{f}:{line}: machine.toml not included"
                );
            }
            checked += 1;
        }
    }
    // README, usage (3), SKILL, tune (2), v1 spec, 0.2 spec: a doc losing
    // its example, or the extractor breaking, shows up here.
    assert!(checked >= 9, "only {checked} examples found");
}
