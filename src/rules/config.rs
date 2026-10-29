//! `~/.config/muzzle/config.toml`: rules, projects, budget, template, hash.
//!
//! Spec §Interface/Config, D21 (rules per host, project overrides), D8
//! (slots default 1, capping is admit's job), D28 (config hash).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::Class;

/// D7: budget = logical cores − reserve, default reserve 2.
pub const DEFAULT_RESERVE: u32 = 2;

/// Errors callers switch on: everything here is "bad config", exit 2
/// (spec §Exit codes), except a config file that exists but can't be read.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// TOML syntax, a wrong type, or an unknown key; toml's message names
    /// the key and its line.
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

/// The commented template written on first run. Its live rules are the
/// defaults muzzle uses when the file is missing (spec §Config).
pub const TEMPLATE: &str = r#"# muzzle config (spec: github.com/mu-crew/muzzle).
# A command that matches no rule runs without a claim. Patterns are `regex`
# crate syntax, matched against the whole shell string; the first match wins,
# project rules before global ones. `name` is required, unique, and is the
# label in status, reports and runs.jsonl: keep it when the pattern changes.

reserve = 2            # budget = logical cores - reserve
# slots = 12           # explicit budget; overrides reserve

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

# Project rules: keys are globs matched against the command's cwd (and its
# parents), after ~ expansion and canonicalisation.
#
# [project."~/hacking/swayward-wm/*"]
# [[project."~/hacking/swayward-wm/*".rule]]
# name  = "oracle-sweep"
# match = 'oracle-sweep'
# slots = 999          # capped to the budget
# # class = "high"
"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    reserve: Option<u32>,
    slots: Option<u32>,
    #[serde(default)]
    rule: Vec<RawRule>,
    // File order, not key order: overlapping project globs are tried as
    // written, since the first match wins (spec §Config). A map type would
    // sort them (BTreeMap) or need toml's preserve_order feature; toml
    // deserialises through toml_edit, whose tables keep document order, so
    // reading the entries into a Vec is enough. The hash covers this order.
    #[serde(default, deserialize_with = "in_file_order")]
    project: Vec<(String, RawProject)>,
}

fn in_file_order<'de, D>(d: D) -> Result<Vec<(String, RawProject)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Vec<(String, RawProject)>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a table of project globs")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut m: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(entry) = m.next_entry()? {
                out.push(entry);
            }
            Ok(out)
        }
    }
    d.deserialize_map(V)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: String,
    #[serde(rename = "match")]
    pattern: String,
    slots: Option<u32>,
    class: Option<RawClass>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawClass {
    High,
    Normal,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub pattern: Regex,
    pub slots: u32,
    pub class: Option<Class>,
}

#[derive(Debug, Clone)]
pub struct Project {
    /// The key as written, `~` unexpanded: what `match` reports and the hash
    /// covers.
    pub key: String,
    matcher: GlobMatcher,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    pub reserve: u32,
    /// An explicit budget, overriding `reserve` (D7, D17a).
    pub slots: Option<u32>,
    pub rules: Vec<Rule>,
    pub projects: Vec<Project>,
}

/// Where a match came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source<'a> {
    Global,
    Project(&'a str),
}

#[derive(Debug, Clone)]
pub struct Match<'a> {
    pub rule: &'a Rule,
    pub source: Source<'a>,
}

/// `$XDG_CONFIG_HOME/muzzle/config.toml`, else `~/.config/muzzle/config.toml`
/// on every platform: the spec names that path, and `directories` would put
/// it under `~/Library` on macOS.
pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("muzzle").join("config.toml")
}

fn home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

impl Config {
    /// Load the config at [`path`], writing the template first when the
    /// file is missing. A template that can't be written is a warning:
    /// muzzle continues with its defaults.
    pub fn load() -> Result<Config, ConfigError> {
        Self::load_from(&path())
    }

    pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match write_template(path) {
                    Ok(()) => {
                        eprintln!("muzzle: wrote a config template to {}", path.display())
                    }
                    // Another muzzle's first run won the race: it wrote the
                    // template and said so.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => eprintln!("muzzle: could not write {}: {e}", path.display()),
                }
                Self::parse(TEMPLATE, path)
            }
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Parse config text; `path` is only used in messages and kept on the
    /// result for `doctor`.
    pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string().trim_end().to_string(),
        })?;
        let invalid = |message: String| ConfigError::Invalid {
            path: path.to_path_buf(),
            message,
        };
        if raw.slots == Some(0) {
            return Err(invalid("slots must be at least 1".into()));
        }
        let mut names = HashSet::new();
        let mut build = |raw: Vec<RawRule>, scope: &str| -> Result<Vec<Rule>, ConfigError> {
            raw.into_iter()
                .map(|r| {
                    if r.name.trim().is_empty() {
                        return Err(invalid(format!("{scope}: a rule has an empty name")));
                    }
                    // A leading `-` reads as a flag wherever the name is
                    // passed as an argument (`run --rule -x` exits 2).
                    if r.name.starts_with('-') {
                        return Err(invalid(format!(
                            "rule {:?}: a name can't start with '-'",
                            r.name
                        )));
                    }
                    // Unique across global and project rules: the name is
                    // the label history is kept under, and `run --rule`
                    // looks the rule up by it.
                    if !names.insert(r.name.clone()) {
                        return Err(invalid(format!("duplicate rule name {:?}", r.name)));
                    }
                    if r.slots == Some(0) {
                        return Err(invalid(format!(
                            "rule {:?}: slots must be at least 1",
                            r.name
                        )));
                    }
                    let pattern = Regex::new(&r.pattern)
                        .map_err(|e| invalid(format!("rule {:?}: bad match: {e}", r.name)))?;
                    Ok(Rule {
                        name: r.name,
                        pattern,
                        slots: r.slots.unwrap_or(1),
                        class: r.class.map(|c| match c {
                            RawClass::High => Class::High,
                            RawClass::Normal => Class::Normal,
                        }),
                    })
                })
                .collect()
        };
        let rules = build(raw.rule, "global")?;
        let mut projects = Vec::new();
        for (key, p) in raw.project {
            let rules = build(p.rule, &format!("project {key:?}"))?;
            let glob = canonical_glob(&key);
            let matcher = Glob::new(&glob)
                .map_err(|e| invalid(format!("project {key:?}: bad glob: {e}")))?
                .compile_matcher();
            projects.push(Project {
                key,
                matcher,
                rules,
            });
        }
        Ok(Config {
            path: path.to_path_buf(),
            reserve: raw.reserve.unwrap_or(DEFAULT_RESERVE),
            slots: raw.slots,
            rules,
            projects,
        })
    }

    /// The first rule matching `command` run in `cwd`: project rules (for
    /// every project whose glob matches the canonical cwd or one of its
    /// parents) before global rules.
    pub fn matches<'a>(&'a self, command: &str, cwd: &Path) -> Option<Match<'a>> {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let project_rules = self
            .projects
            .iter()
            .filter(|p| cwd.ancestors().any(|dir| p.matcher.is_match(dir)))
            .flat_map(|p| p.rules.iter().map(move |r| (r, Source::Project(&p.key))));
        let global_rules = self.rules.iter().map(|r| (r, Source::Global));
        project_rules
            .chain(global_rules)
            .find(|(r, _)| r.pattern.is_match(command))
            .map(|(rule, source)| Match { rule, source })
    }

    /// A rule by its label, global or project; `run --rule` uses it to find
    /// the rule's class (D12 step 3).
    pub fn rule(&self, name: &str) -> Option<&Rule> {
        self.rules
            .iter()
            .chain(self.projects.iter().flat_map(|p| p.rules.iter()))
            .find(|r| r.name == name)
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len() + self.projects.iter().map(|p| p.rules.len()).sum::<usize>()
    }

    /// D28: a short hash of the normalised rules, budget and reserve.
    /// Normalised means parsed values with defaults filled in, so
    /// whitespace, comments, quoting and spelling out a default don't change
    /// it. Rule order does: the first match wins.
    pub fn hash(&self) -> String {
        #[derive(Serialize)]
        struct NormRule<'a> {
            name: &'a str,
            pattern: &'a str,
            slots: u32,
            class: Option<&'static str>,
        }
        #[derive(Serialize)]
        struct Norm<'a> {
            v: u32,
            reserve: u32,
            slots: Option<u32>,
            rules: Vec<NormRule<'a>>,
            projects: Vec<(&'a str, Vec<NormRule<'a>>)>,
        }
        fn norm(rules: &[Rule]) -> Vec<NormRule<'_>> {
            rules
                .iter()
                .map(|r| NormRule {
                    name: &r.name,
                    pattern: r.pattern.as_str(),
                    slots: r.slots,
                    class: r.class.map(|c| match c {
                        Class::High => "high",
                        Class::Normal => "normal",
                    }),
                })
                .collect()
        }
        let n = Norm {
            v: 1,
            reserve: self.reserve,
            slots: self.slots,
            rules: norm(&self.rules),
            projects: self
                .projects
                .iter()
                .map(|p| (p.key.as_str(), norm(&p.rules)))
                .collect(),
        };
        let json = serde_json::to_vec(&n).expect("serialising plain data");
        let digest = Sha256::digest(&json);
        digest[..3].iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn write_template(path: &Path) -> std::io::Result<()> {
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

/// Expand `~` and canonicalise the literal (glob-free) prefix of a project
/// key, so it compares against a canonical cwd. Without this,
/// `~/hacking/*` never matches on hosts where `$HOME` is a symlink (Fedora
/// Atomic: `/home` → `/var/home`, as on linuxpc).
fn canonical_glob(key: &str) -> String {
    let expanded = if key == "~" {
        home().to_string_lossy().into_owned()
    } else if let Some(rest) = key.strip_prefix("~/") {
        home().join(rest).to_string_lossy().into_owned()
    } else {
        key.to_string()
    };
    let is_glob = |s: &str| s.contains(['*', '?', '[', '{']);
    let parts: Vec<&str> = expanded.split('/').collect();
    let literal = parts.iter().take_while(|p| !is_glob(p)).count();
    if literal == 0 {
        return expanded;
    }
    // Canonicalise the longest existing ancestor of the literal prefix and
    // keep the rest as written.
    for keep in (1..=literal).rev() {
        let prefix = parts[..keep].join("/");
        if prefix.is_empty() {
            continue;
        }
        if let Ok(canon) = Path::new(&prefix).canonicalize() {
            let mut out = canon.to_string_lossy().into_owned();
            for p in &parts[keep..] {
                if !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str(p);
            }
            return out;
        }
    }
    expanded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, Path::new("config.toml"))
    }

    #[test]
    fn template_parses_to_the_spec_defaults() {
        let c = parse(TEMPLATE).unwrap();
        assert_eq!(c.reserve, 2);
        assert_eq!(c.slots, None);
        let names: Vec<_> = c.rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["gate", "cargo-test", "js-test"]);
        assert_eq!(c.rules[0].slots, 6);
        assert!(c.projects.is_empty());
    }

    #[test]
    fn unknown_keys_name_the_key() {
        for (text, key) in [
            ("reserv = 2", "reserv"),
            ("[[rule]]\nname='a'\nmatch='a'\nslot=2", "slot"),
            ("[project.\"/x\"]\nrules=[]", "rules"),
            (
                "[[project.\"/x\".rule]]\nname='a'\nmatch='a'\nweight=1",
                "weight",
            ),
        ] {
            let err = parse(text).unwrap_err();
            assert!(matches!(err, ConfigError::Parse { .. }), "{text}: {err}");
            assert!(err.to_string().contains(key), "{text}: {err}");
        }
    }

    #[test]
    fn rule_names_are_required_and_unique() {
        let missing = parse("[[rule]]\nmatch='a'").unwrap_err();
        assert!(missing.to_string().contains("name"), "{missing}");
        let dup = parse("[[rule]]\nname='a'\nmatch='x'\n[[rule]]\nname='a'\nmatch='y'");
        assert!(matches!(dup, Err(ConfigError::Invalid { .. })));
        // Unique across project and global rules too.
        let dup =
            parse("[[rule]]\nname='a'\nmatch='x'\n[[project.\"/p\".rule]]\nname='a'\nmatch='y'");
        assert!(matches!(dup, Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn rule_names_cannot_look_like_flags() {
        for text in [
            "[[rule]]\nname='-x'\nmatch='a'",
            "[[project.\"/p\".rule]]\nname='--slots'\nmatch='a'",
        ] {
            let err = parse(text).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { .. }), "{text}: {err}");
            assert!(err.to_string().contains("-x") || err.to_string().contains("--slots"));
        }
        // A dash later in the name is fine.
        assert!(parse("[[rule]]\nname='x-y'\nmatch='a'").is_ok());
    }

    #[test]
    fn overlapping_projects_are_tried_in_file_order() {
        let dir = tempfile::tempdir().unwrap();
        let b = dir.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        let specific = format!(
            "[[project.\"{}\".rule]]\nname='b'\nmatch='x'\n",
            b.display()
        );
        let broad = format!(
            "[[project.\"{}/*\".rule]]\nname='a'\nmatch='x'\n",
            dir.path().display()
        );
        let specific_first = parse(&format!("{specific}{broad}")).unwrap();
        let broad_first = parse(&format!("{broad}{specific}")).unwrap();
        // "*" sorts before "b": key order would pick `a` both times.
        assert_eq!(specific_first.matches("x", &b).unwrap().rule.name, "b");
        assert_eq!(broad_first.matches("x", &b).unwrap().rule.name, "a");
        assert_ne!(specific_first.hash(), broad_first.hash());
    }

    #[test]
    fn bad_values_are_config_errors() {
        for text in [
            "[[rule]]\nname='a'\nmatch='('",
            "[[rule]]\nname='a'\nmatch='a'\nslots=0",
            "[[rule]]\nname='a'\nmatch='a'\nclass='urgent'",
            "slots = 0",
            "reserve = -1",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn first_match_wins_projects_before_global() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        let sub = proj.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let text = format!(
            r#"
[[rule]]
name = "gate"
match = 'gate'
slots = 6
[[rule]]
name = "gate2"
match = 'gate'
slots = 1
[[rule]]
name = "test"
match = 'cargo test'
slots = 2
class = "high"
[[project."{}".rule]]
name = "proj-gate"
match = 'gate'
slots = 3
"#,
            proj.display()
        );
        let c = parse(&text).unwrap();
        let m = c.matches("contrib/gate", dir.path()).unwrap();
        assert_eq!((m.rule.name.as_str(), m.source), ("gate", Source::Global));
        for cwd in [&proj, &sub] {
            let m = c.matches("contrib/gate", cwd).unwrap();
            assert_eq!(m.rule.name, "proj-gate");
            assert!(matches!(m.source, Source::Project(_)));
        }
        // A project with no matching rule falls through to global rules.
        let m = c.matches("cargo test -p x", &proj).unwrap();
        assert_eq!(
            (m.rule.name.as_str(), m.rule.class),
            ("test", Some(Class::High))
        );
        assert!(c.matches("ls -la", &proj).is_none());
    }

    #[test]
    fn project_keys_expand_tilde_and_canonicalise() {
        // $HOME is process-wide; build the paths this test needs from the
        // real home rather than mutating the environment.
        let home = home();
        let Ok(tmp) = tempfile::tempdir_in(&home) else {
            return; // read-only home: nothing to check
        };
        let name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let text = format!("[[project.\"~/{name}/*\".rule]]\nname='p'\nmatch='x'");
        let c = parse(&text).unwrap();
        assert_eq!(c.matches("x", &repo).unwrap().rule.name, "p");
        // The non-canonical spelling of the cwd matches too.
        assert_eq!(
            c.matches("x", &repo.join("..").join("repo"))
                .unwrap()
                .rule
                .name,
            "p"
        );
        assert!(c.matches("x", tmp.path().parent().unwrap()).is_none());
    }

    #[test]
    fn symlinked_prefix_matches_canonical_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("repo")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let text = format!(
            "[[project.\"{}/*\".rule]]\nname='p'\nmatch='x'",
            link.display()
        );
        let c = parse(&text).unwrap();
        assert!(c.matches("x", &real.join("repo")).is_some());
        assert!(c.matches("x", &link.join("repo")).is_some());
    }

    #[test]
    fn rule_lookup_by_name() {
        let c = parse(
            "[[rule]]\nname='a'\nmatch='x'\n[[project.\"/p\".rule]]\nname='b'\nmatch='y'\nclass='normal'",
        )
        .unwrap();
        assert_eq!(c.rule("a").unwrap().slots, 1);
        assert_eq!(c.rule("b").unwrap().class, Some(Class::Normal));
        assert!(c.rule("c").is_none());
        assert_eq!(c.rule_count(), 2);
    }

    const BASE: &str = r#"
reserve = 2
[[rule]]
name = "gate"
match = 'gate'
slots = 6
[[rule]]
name = "test"
match = 'cargo test'
slots = 2
[[project."~/p/*".rule]]
name = "sweep"
match = 'sweep'
slots = 999
"#;

    fn hash(text: &str) -> String {
        parse(text).unwrap().hash()
    }

    #[test]
    fn hash_is_short_and_stable_over_formatting() {
        let h = hash(BASE);
        assert_eq!(h.len(), 6);
        let reformatted = r#"
# a comment
reserve=2   # trailing comment

[[rule]]
  name  = "gate"
  match = "gate"
  slots = 6
[[rule]]
name = 'test'
slots = 2
match = 'cargo test'

[project."~/p/*"]
[[project."~/p/*".rule]]
name = "sweep"   # moved keys, different quotes
match = 'sweep'
slots = 999
"#;
        assert_eq!(hash(reformatted), h);
        // Spelling out a default is not a change.
        let without_reserve = BASE.replacen("reserve = 2\n", "", 1);
        assert_eq!(hash(&without_reserve), h);
        let explicit_slots = "[[rule]]\nname='a'\nmatch='x'\nslots=1";
        assert_eq!(hash(explicit_slots), hash("[[rule]]\nname='a'\nmatch='x'"));
    }

    #[test]
    fn hash_changes_with_rules_order_budget_and_reserve() {
        let h = hash(BASE);
        let edits = [
            BASE.replace("slots = 6", "slots = 4"),
            BASE.replace("match = 'gate'", "match = 'gates?'"),
            BASE.replace("name = \"gate\"", "name = \"gate-x\""),
            BASE.replace("slots = 999", "slots = 8"),
            BASE.replace("~/p/*", "~/q/*"),
            BASE.replace("reserve = 2", "reserve = 3"),
            BASE.replace("reserve = 2", "reserve = 2\nslots = 12"),
            BASE.replace("slots = 2\n", "slots = 2\nclass = \"high\"\n"),
            // Reordered rules: the first match wins, so order matters.
            BASE.replace(
                "[[rule]]\nname = \"gate\"\nmatch = 'gate'\nslots = 6\n[[rule]]\nname = \"test\"\nmatch = 'cargo test'\nslots = 2\n",
                "[[rule]]\nname = \"test\"\nmatch = 'cargo test'\nslots = 2\n[[rule]]\nname = \"gate\"\nmatch = 'gate'\nslots = 6\n",
            ),
        ];
        for edit in &edits {
            assert_ne!(edit, BASE, "edit did not apply");
            assert_ne!(hash(edit), h, "{edit}");
        }
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
        assert_eq!(c.rules.len(), 3);
    }
}
