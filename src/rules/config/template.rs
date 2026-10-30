//! The template written on first run, and writing it without clobbering
//! (spec §Config).

use std::path::{Path, PathBuf};

/// The commented template written on first run. Its live rules are the
/// defaults muzzle uses when the file is missing (spec §Config).
pub const TEMPLATE: &str = r#"# muzzle config (spec: github.com/mu-crew/muzzle).
# A command that matches no rule runs without a claim. Patterns are `regex`
# crate syntax, matched against the whole shell string; the first match wins.
# `name` is required and is the label in status, reports and runs.jsonl:
# keep it when the pattern changes.

reserve = 2            # budget = logical cores - reserve
# slots = 12           # explicit budget; overrides reserve

# Files merged after this one, relative to it (~ works). A missing file is
# skipped: keep per-machine weights in an untracked file.
# include = ["machine.toml"]

[[rule]]
name  = "gate"
match = 'contrib/(fast-)?gate'
slots = 6
[[rule]]
name  = "cargo-test"
match = '\bcargo (test|nextest|clippy|build)\b'
slots = 2
[[rule]]
name  = "js-test"
match = '\b(vitest|npm (run )?test)\b'
slots = 2

# Path tables: a directory and everything under it. A command's target is
# the repo of its cwd (the main checkout, so every worktree of a repo gets
# the same rules), else the cwd. Tables apply from the outermost to the
# nearest. An entry naming a rule in effect changes only the fields it sets;
# a new name is a new rule (it needs `match`), tried before inherited ones.
#
# [path."~/hacking/crew"]
# [[path."~/hacking/crew".rule]]
# name  = "cargo-test"
# slots = 3            # reweight only; match inherited
# [[path."~/hacking/crew".rule]]
# name  = "oracle-sweep"
# match = 'oracle-sweep'
# slots = 999          # capped to the budget
# # class = "high"
# # enabled = false    # takes a rule out of matching under this path
"#;

pub(super) fn write_template(path: &Path) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    // Write a private temp file, then hard-link it into place: link(2)
    // fails with AlreadyExists rather than clobber a config another muzzle
    // (or the user) wrote meanwhile, and a concurrent reader never sees a
    // half-written file, which would parse as a config with no rules.
    let tmp = tempfile_in(dir)?;
    std::fs::write(&tmp, TEMPLATE)?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    linked
}

/// A fresh, exclusively created path next to the config.
fn tempfile_in(dir: &Path) -> std::io::Result<PathBuf> {
    for n in 0u32.. {
        let p = dir.join(format!(".config.toml.{}.{n}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(_) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("u32 exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::config::Config;
    use crate::rules::config::tests::parse;

    #[test]
    fn template_parses_to_the_spec_defaults() {
        let c = parse(TEMPLATE).unwrap();
        assert_eq!(c.reserve, 2);
        assert_eq!(c.slots, None);
        let names: Vec<_> = c.global.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["gate", "cargo-test", "js-test"]);
        assert_eq!(c.global[0].slots, Some(6));
        assert!(c.tables.is_empty());
        assert!(c.includes().is_empty());
    }

    #[test]
    fn missing_file_gets_the_template() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("muzzle").join("config.toml");
        let c = Config::load_from(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TEMPLATE);
        assert_eq!(c.hash(), parse(TEMPLATE).unwrap().hash());
    }

    #[test]
    fn template_never_overwrites_an_existing_file() {
        // The file can appear between load's read and this write.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "reserve = 4\n").unwrap();
        let err = write_template(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "reserve = 4\n");
    }

    #[test]
    fn unwritable_template_still_gives_defaults() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Read-only parent (unless running as root): the write fails, the
        // load doesn't.
        let c = Config::load_from(&ro.join("config.toml")).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(c.global.len(), 3);
    }
}
