//! `~/.config/muzzle/config.toml` and its includes: parsing, merging,
//! validation, template and hash. What applies at a cwd is
//! [`super::resolve`].
//!
//! Spec 0.2 (`docs/specs/2026-10-30-muzzle-0.2-paths.md`): P1 path tables,
//! P5 includes, P6 budget keys, P7 hash, B1 key identity, B2 validation,
//! B5 read errors. v1: D8 (slots default 1, capping is admit's job).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::Class;

/// D7: budget = logical cores − reserve, default reserve 2.
pub const DEFAULT_RESERVE: u32 = 2;

/// Errors callers switch on: everything here is "bad config", exit 2
/// (spec §Exit codes), except a file that exists but can't be read (B5).
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    include: Option<Vec<String>>,
    reserve: Option<u32>,
    slots: Option<u32>,
    #[serde(default)]
    rule: Vec<RawRule>,
    // File order, so the B1 message names the spellings as written. toml
    // deserialises through toml_edit, whose tables keep document order.
    #[serde(default, deserialize_with = "in_file_order")]
    path: Vec<(String, RawTable)>,
}

fn in_file_order<'de, D>(d: D) -> Result<Vec<(String, RawTable)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Vec<(String, RawTable)>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a table of directories")
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

/// P6: no budget keys here, so `reserve` in a table is an unknown key.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: String,
    #[serde(rename = "match")]
    pattern: Option<String>,
    slots: Option<u32>,
    class: Option<RawClass>,
    enabled: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawClass {
    High,
    Normal,
}

/// One rule entry with only the fields it sets (P3), merged field by field
/// when a later file names it again (P5).
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub pattern: Option<Regex>,
    pub slots: Option<u32>,
    pub class: Option<Class>,
    pub enabled: Option<bool>,
    /// The first file that defined this entry: B2 messages.
    pub file: PathBuf,
    /// The files that last set `match` and `slots` (P8).
    pub match_from: Option<PathBuf>,
    pub slots_from: Option<PathBuf>,
}

impl Entry {
    /// P3 merge: every field `other` sets replaces this one's.
    fn merge(&mut self, other: Entry) {
        if other.pattern.is_some() {
            self.pattern = other.pattern;
            self.match_from = other.match_from;
        }
        if other.slots.is_some() {
            self.slots = other.slots;
            self.slots_from = other.slots_from;
        }
        if other.class.is_some() {
            self.class = other.class;
        }
        if other.enabled.is_some() {
            self.enabled = other.enabled;
        }
    }
}

/// A `[path."<dir>"]` table, merged across files (B1).
#[derive(Debug, Clone)]
pub struct Table {
    /// As written in the first file that defines the table: what displays
    /// show (B1).
    pub key: String,
    /// Its identity (B1): `~` expanded, canonicalised when it exists, else
    /// normalised lexically (P1).
    pub canonical: PathBuf,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// The root file.
    pub path: PathBuf,
    pub reserve: u32,
    /// An explicit budget, overriding `reserve` (D7, D17a).
    pub slots: Option<u32>,
    /// The global list, root then includes merged in (P5).
    pub global: Vec<Entry>,
    /// Sorted by canonical key, so an ancestor comes before its
    /// descendants (`PathBuf` orders by component).
    pub tables: Vec<Table>,
    includes: Vec<(PathBuf, bool)>,
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

pub(crate) fn home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        home()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// `.` dropped and `..` applied without touching the filesystem.
fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// P1: a table key's identity. `None` for a relative key: there is no
/// directory it could be relative to.
pub(crate) fn canonical_key(key: &str) -> Option<PathBuf> {
    let p = expand_tilde(key);
    if !p.is_absolute() {
        return None;
    }
    Some(canonical_prefix(&normalise(&p)))
}

/// Canonicalises the longest existing ancestor of an already normalised
/// path and appends the rest, so a key's identity follows symlinks
/// (`/home` -> `/var/home`, macOS `/var` -> `/private/var`) whether or not
/// the directory exists yet, and doesn't change when it is created.
fn canonical_prefix(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = cur.canonicalize() {
            return rest.iter().rev().fold(c, |acc, part| acc.join(part));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// One file, parsed and checked on its own.
struct Layer {
    include: Option<Vec<String>>,
    reserve: Option<u32>,
    slots: Option<u32>,
    global: Vec<Entry>,
    tables: Vec<(String, PathBuf, Vec<Entry>)>,
}

fn parse_layer(text: &str, path: &Path) -> Result<Layer, ConfigError> {
    let raw: RawFile = toml::from_str(text).map_err(|e| ConfigError::Parse {
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
    let entries = |raw: Vec<RawRule>, scope: &str| -> Result<Vec<Entry>, ConfigError> {
        // Unique within one table and within one file's global list (P3):
        // the same name elsewhere is the cascade.
        let mut names = HashSet::new();
        raw.into_iter()
            .map(|r| {
                if r.name.trim().is_empty() {
                    return Err(invalid(format!("{scope}: a rule has an empty name")));
                }
                // A leading `-` reads as a flag wherever the name is passed
                // as an argument (`run --rule -x` exits 2).
                if r.name.starts_with('-') {
                    return Err(invalid(format!(
                        "{scope}: rule {:?}: a name can't start with '-'",
                        r.name
                    )));
                }
                if !names.insert(r.name.clone()) {
                    return Err(invalid(format!(
                        "{scope}: duplicate rule name {:?}",
                        r.name
                    )));
                }
                if r.slots == Some(0) {
                    return Err(invalid(format!(
                        "{scope}: rule {:?}: slots must be at least 1",
                        r.name
                    )));
                }
                let pattern =
                    r.pattern.map(|p| Regex::new(&p)).transpose().map_err(|e| {
                        invalid(format!("{scope}: rule {:?}: bad match: {e}", r.name))
                    })?;
                Ok(Entry {
                    name: r.name,
                    match_from: pattern.as_ref().map(|_| path.to_path_buf()),
                    pattern,
                    slots: r.slots,
                    class: r.class.map(|c| match c {
                        RawClass::High => Class::High,
                        RawClass::Normal => Class::Normal,
                    }),
                    enabled: r.enabled,
                    file: path.to_path_buf(),
                    slots_from: r.slots.map(|_| path.to_path_buf()),
                })
            })
            .collect()
    };
    let global = entries(raw.rule, "global")?;
    let mut tables: Vec<(String, PathBuf, Vec<Entry>)> = Vec::new();
    for (key, t) in raw.path {
        let scope = format!("[path.{key:?}]");
        let canonical = canonical_key(&key).ok_or_else(|| {
            invalid(format!(
                "{scope}: a path key must be absolute or start with ~"
            ))
        })?;
        // B1: two spellings of one directory in one file.
        if let Some((other, ..)) = tables.iter().find(|(_, c, _)| *c == canonical) {
            return Err(invalid(format!(
                "[path.{other:?}] and {scope} are the same directory ({})",
                canonical.display()
            )));
        }
        let rules = entries(t.rule, &scope)?;
        tables.push((key, canonical, rules));
    }
    Ok(Layer {
        include: raw.include,
        reserve: raw.reserve,
        slots: raw.slots,
        global,
        tables,
    })
}

/// Merge `entries` into `into` by name; new names are appended.
fn merge_entries(into: &mut Vec<Entry>, entries: Vec<Entry>) {
    for e in entries {
        match into.iter_mut().find(|d| d.name == e.name) {
            Some(d) => d.merge(e),
            None => into.push(e),
        }
    }
}

impl Config {
    /// Load the config at [`path`] with its includes, writing the template
    /// first when the root is missing. A template that can't be written is
    /// a warning: muzzle continues with its defaults.
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

    /// Parse the root file's text, read its includes (relative to `path`'s
    /// directory), merge them in and validate the result (P5, B2).
    pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
        let mut root = parse_layer(text, path)?;
        let mut cfg = Config {
            path: path.to_path_buf(),
            reserve: DEFAULT_RESERVE,
            slots: None,
            global: Vec::new(),
            tables: Vec::new(),
            includes: Vec::new(),
        };
        let includes = root.include.take().unwrap_or_default();
        cfg.layer(root, path)?;
        let base = path.parent().unwrap_or(Path::new("."));
        for inc in includes {
            let p = base.join(expand_tilde(&inc));
            let found = match std::fs::read_to_string(&p) {
                Ok(text) => {
                    let layer = parse_layer(&text, &p)?;
                    if layer.include.is_some() {
                        return Err(ConfigError::Invalid {
                            path: p,
                            message: "an included file can't set include".into(),
                        });
                    }
                    cfg.layer(layer, &p)?;
                    true
                }
                // P5: skipped silently; doctor lists it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(source) => return Err(ConfigError::Read { path: p, source }),
            };
            cfg.includes.push((p, found));
        }
        cfg.tables.sort_by(|a, b| a.canonical.cmp(&b.canonical));
        cfg.validate_tables()?;
        Ok(cfg)
    }

    /// Merge one file into what is loaded so far (P5).
    fn layer(&mut self, layer: Layer, file: &Path) -> Result<(), ConfigError> {
        if let Some(r) = layer.reserve {
            self.reserve = r;
        }
        if layer.slots.is_some() {
            self.slots = layer.slots;
        }
        for e in layer.global {
            // B2: a global entry with no match reweights a rule an earlier
            // file defined.
            if e.pattern.is_none() && !self.global.iter().any(|g| g.name == e.name) {
                return Err(ConfigError::Invalid {
                    path: file.to_path_buf(),
                    message: format!(
                        "global rule {:?} has no match, and no earlier file defines it",
                        e.name
                    ),
                });
            }
            merge_entries(&mut self.global, vec![e]);
        }
        for (key, canonical, entries) in layer.tables {
            match self.tables.iter_mut().find(|t| t.canonical == canonical) {
                Some(t) => merge_entries(&mut t.entries, entries),
                None => self.tables.push(Table {
                    key,
                    canonical,
                    entries,
                }),
            }
        }
        Ok(())
    }

    /// B2: after merging, a table entry with no match names a rule in the
    /// global list or in an ancestor table, so `resolve` can't fail.
    fn validate_tables(&self) -> Result<(), ConfigError> {
        for t in &self.tables {
            for e in t.entries.iter().filter(|e| e.pattern.is_none()) {
                let global = self.global.iter().any(|g| g.name == e.name);
                let ancestor = self.tables.iter().any(|a| {
                    a.canonical != t.canonical
                        && t.canonical.starts_with(&a.canonical)
                        && a.entries.iter().any(|ae| ae.name == e.name)
                });
                if !global && !ancestor {
                    return Err(ConfigError::Invalid {
                        path: e.file.clone(),
                        message: format!(
                            "[path.{:?}] rule {:?} has no match, and no rule of that name \
                             is in effect there",
                            t.key, e.name
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Each include as resolved, and whether it was found (P8, doctor).
    pub fn includes(&self) -> Vec<(PathBuf, bool)> {
        self.includes.clone()
    }

    /// Distinct rule names across the global list and every table: what
    /// doctor counts.
    pub fn rule_count(&self) -> usize {
        self.patterns()
            .map(|(n, _)| n)
            .collect::<HashSet<_>>()
            .len()
    }

    pub fn path_table_count(&self) -> usize {
        self.tables.len()
    }

    /// Every entry that sets a pattern, global and in tables, in file
    /// order: what doctor probes.
    pub fn patterns(&self) -> impl Iterator<Item = (&str, &Regex)> {
        self.global
            .iter()
            .chain(self.tables.iter().flat_map(|t| &t.entries))
            .filter_map(|e| e.pattern.as_ref().map(|p| (e.name.as_str(), p)))
    }

    /// P7 (D28): a short hash of the merged config. Each entry carries only
    /// the fields it sets, so an explicit `slots = 1` is a change (in a
    /// table it overrides an inherited value). Global order matters (first
    /// match wins); tables are sorted by canonical key. `v` = 2: no 0.2
    /// hash equals a 0.1 hash.
    pub fn hash(&self) -> String {
        #[derive(Serialize)]
        struct NormRule<'a> {
            name: &'a str,
            pattern: Option<&'a str>,
            slots: Option<u32>,
            class: Option<&'static str>,
            enabled: Option<bool>,
        }
        #[derive(Serialize)]
        struct Norm<'a> {
            v: u32,
            reserve: u32,
            slots: Option<u32>,
            rules: Vec<NormRule<'a>>,
            paths: Vec<(std::borrow::Cow<'a, str>, Vec<NormRule<'a>>)>,
        }
        fn norm(entries: &[Entry]) -> Vec<NormRule<'_>> {
            entries
                .iter()
                .map(|e| NormRule {
                    name: &e.name,
                    pattern: e.pattern.as_ref().map(Regex::as_str),
                    slots: e.slots,
                    class: e.class.map(|c| match c {
                        Class::High => "high",
                        Class::Normal => "normal",
                    }),
                    enabled: e.enabled,
                })
                .collect()
        }
        let n = Norm {
            v: 2,
            reserve: self.reserve,
            slots: self.slots,
            rules: norm(&self.global),
            paths: self
                .tables
                .iter()
                .map(|t| (t.canonical.to_string_lossy(), norm(&t.entries)))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, Path::new("/nonexistent/config.toml"))
    }

    fn invalid(text: &str, needle: &str) {
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{text}: {err}");
        assert!(err.to_string().contains(needle), "{text}: {err}");
    }

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
    fn unknown_keys_name_the_key() {
        for (text, key) in [
            ("reserv = 2", "reserv"),
            ("[[rule]]\nname='a'\nmatch='a'\nslot=2", "slot"),
            ("[path.\"/x\"]\nrules=[]", "rules"),
            (
                "[[path.\"/x\".rule]]\nname='a'\nmatch='a'\nweight=1",
                "weight",
            ),
            // P6: budget keys belong to the machine, not a path.
            ("[path.\"/x\"]\nreserve=1", "reserve"),
            // P1: no alias, no warning.
            ("[[project.\"/x\".rule]]\nname='a'\nmatch='a'", "project"),
        ] {
            let err = parse(text).unwrap_err();
            assert!(matches!(err, ConfigError::Parse { .. }), "{text}: {err}");
            assert!(err.to_string().contains(key), "{text}: {err}");
        }
    }

    #[test]
    fn names_are_required_and_unique_per_scope() {
        let missing = parse("[[rule]]\nmatch='a'").unwrap_err();
        assert!(missing.to_string().contains("name"), "{missing}");
        invalid(
            "[[rule]]\nname='a'\nmatch='x'\n[[rule]]\nname='a'\nmatch='y'",
            "duplicate",
        );
        invalid(
            "[[path.\"/p\".rule]]\nname='a'\nmatch='x'\n[[path.\"/p\".rule]]\nname='a'",
            "duplicate",
        );
        // The same name in a table and the global list is the cascade.
        assert!(
            parse("[[rule]]\nname='a'\nmatch='x'\n[[path.\"/p\".rule]]\nname='a'\nslots=2").is_ok()
        );
    }

    #[test]
    fn rule_names_cannot_look_like_flags() {
        invalid("[[rule]]\nname='-x'\nmatch='a'", "-x");
        invalid("[[path.\"/p\".rule]]\nname='--slots'\nmatch='a'", "--slots");
        assert!(parse("[[rule]]\nname='x-y'\nmatch='a'").is_ok());
    }

    #[test]
    fn bad_values_are_config_errors() {
        for text in [
            "[[rule]]\nname='a'\nmatch='('",
            "[[rule]]\nname='a'\nmatch='a'\nslots=0",
            "[[rule]]\nname='a'\nmatch='a'\nclass='urgent'",
            "[[rule]]\nname='a'\nmatch='a'\nenabled='no'",
            "slots = 0",
            "reserve = -1",
            "[[path.\"relative/dir\".rule]]\nname='a'\nmatch='a'",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn b1_two_spellings_of_one_directory_in_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let text = format!(
            "[[path.\"{d}\".rule]]\nname='a'\nmatch='a'\n[[path.\"{d}/./x/..\".rule]]\nname='b'\nmatch='b'"
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("\"{d}\"")) && msg.contains("/./x/.."),
            "{msg}"
        );
    }

    #[test]
    fn a_key_follows_symlinks_whether_or_not_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canon_real = real.canonicalize().unwrap();
        // A missing tail under a symlinked prefix resolves through the link.
        let missing = canonical_key(&format!("{}/later", link.display())).unwrap();
        assert_eq!(missing, canon_real.join("later"));
        // Creating the directory doesn't change the key (P7 hash stability).
        std::fs::create_dir(real.join("later")).unwrap();
        let existing = canonical_key(&format!("{}/later", link.display())).unwrap();
        assert_eq!(existing, missing);
        // `x/..` through a missing x is normalised before resolving.
        let dotted = canonical_key(&format!("{}/./nope/..", link.display())).unwrap();
        assert_eq!(dotted, canon_real);
    }

    #[test]
    fn b2_entry_without_match_needs_a_rule_in_effect() {
        // Root global: nothing earlier.
        invalid("[[rule]]\nname='a'\nslots=2", "\"a\"");
        // A table: neither global nor an ancestor table has it.
        invalid("[[path.\"/p\".rule]]\nname='a'\nslots=2", "\"a\"");
        // A sibling or a descendant doesn't count, only an ancestor.
        invalid(
            "[[path.\"/p/x\".rule]]\nname='a'\nmatch='a'\n[[path.\"/p\".rule]]\nname='a'\nslots=2",
            "\"a\"",
        );
        invalid(
            "[[path.\"/p/x\".rule]]\nname='a'\nmatch='a'\n[[path.\"/p/xy\".rule]]\nname='a'\nslots=2",
            "\"a\"",
        );
        let ok = parse(
            "[[path.\"/p\".rule]]\nname='a'\nmatch='a'\n[[path.\"/p/x\".rule]]\nname='a'\nslots=2",
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn tables_sort_ancestors_first() {
        let c = parse(
            "[[path.\"/p/x/y\".rule]]\nname='c'\nmatch='c'\n[[path.\"/p\".rule]]\nname='a'\nmatch='a'\n[[path.\"/p/x\".rule]]\nname='b'\nmatch='b'",
        )
        .unwrap();
        let keys: Vec<_> = c.tables.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, ["/p", "/p/x", "/p/x/y"]);
        assert_eq!(c.path_table_count(), 3);
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
[[path."/p".rule]]
name = "test"
slots = 3
[[path."/p".rule]]
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

[path."/p/"]
[[path."/p/".rule]]
name = "test"
slots = 3
[[path."/p/".rule]]
name = "sweep"   # moved keys, different quotes
slots = 999
match = 'sweep'
"#;
        assert_eq!(hash(reformatted), h);
        let without_reserve = BASE.replacen("reserve = 2\n", "", 1);
        assert_eq!(hash(&without_reserve), h);
    }

    #[test]
    fn hash_changes_with_any_set_field_order_budget_and_reserve() {
        let h = hash(BASE);
        let edits = [
            BASE.replace("slots = 6", "slots = 4"),
            BASE.replace("match = 'gate'", "match = 'gates?'"),
            BASE.replace("name = \"gate\"", "name = \"gate-x\""),
            BASE.replace("slots = 999", "slots = 8"),
            BASE.replace("slots = 3", "slots = 3\nenabled = true"),
            BASE.replace("/p", "/q"),
            BASE.replace("reserve = 2", "reserve = 3"),
            BASE.replace("reserve = 2", "reserve = 2\nslots = 12"),
            BASE.replace("slots = 2\n", "slots = 2\nclass = \"high\"\n"),
            BASE.replace(
                "[[rule]]\nname = \"gate\"\nmatch = 'gate'\nslots = 6\n[[rule]]\nname = \"test\"\nmatch = 'cargo test'\nslots = 2\n",
                "[[rule]]\nname = \"test\"\nmatch = 'cargo test'\nslots = 2\n[[rule]]\nname = \"gate\"\nmatch = 'gate'\nslots = 6\n",
            ),
        ];
        for edit in &edits {
            assert_ne!(edit, BASE, "edit did not apply");
            assert_ne!(hash(edit), h, "{edit}");
        }
        // An explicit default is a set field (spec "Hash detail").
        let explicit = "[[rule]]\nname='a'\nmatch='x'\nslots=1";
        assert_ne!(hash(explicit), hash("[[rule]]\nname='a'\nmatch='x'"));
    }

    /// A root at `<dir>/config.toml` with includes beside it.
    fn files(files: &[(&str, &str)]) -> (tempfile::TempDir, Result<Config, ConfigError>) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let c = Config::load_from(&dir.path().join("config.toml"));
        (dir, c)
    }

    #[test]
    fn includes_merge_after_the_root_in_order() {
        let root = r#"
include = ["machine.toml", "sub/more.toml", "missing.toml"]
reserve = 2
[[rule]]
name = "a"
match = 'a'
slots = 3
enabled = false
[[path."/p".rule]]
name = "a"
slots = 5
"#;
        let machine = r#"
reserve = 4
slots = 10
[[rule]]
name = "a"
slots = 4
enabled = true
[[rule]]
name = "b"
match = 'b'
[[path."/p/".rule]]
name = "pn"
match = 'pn'
[[path."/q".rule]]
name = "a"
slots = 7
"#;
        let more = "reserve = 6
[[rule]]
name='c'
match='c'
[[rule]]
name='b'
slots=2";
        let (dir, c) = files(&[
            ("config.toml", root),
            ("machine.toml", machine),
            ("sub/more.toml", more),
        ]);
        let c = c.unwrap();
        // Budget keys from includes, the last one winning.
        assert_eq!((c.reserve, c.slots), (6, Some(10)));
        // Global: merged by name, new names appended.
        let g: Vec<_> = c
            .global
            .iter()
            .map(|e| (e.name.as_str(), e.slots, e.enabled))
            .collect();
        assert_eq!(
            g,
            [
                ("a", Some(4), Some(true)),
                ("b", Some(2), None),
                ("c", None, None)
            ]
        );
        let machine_path = dir.path().join("machine.toml");
        assert_eq!(
            c.global[0].slots_from.as_deref(),
            Some(machine_path.as_path())
        );
        assert_eq!(
            c.global[0].match_from.as_deref(),
            Some(dir.path().join("config.toml").as_path())
        );
        // Same canonical key: one table, displayed as the root spelled it.
        assert_eq!(c.path_table_count(), 2);
        let p = &c.tables[0];
        assert_eq!(p.key, "/p");
        let names: Vec<_> = p.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a", "pn"]);
        assert_eq!(
            c.includes(),
            [
                (machine_path, true),
                (dir.path().join("sub/more.toml"), true),
                (dir.path().join("missing.toml"), false),
            ]
        );
        // A missing include contributes nothing to the hash.
        let without = root.replace(", \"missing.toml\"", "");
        let (_d2, c2) = files(&[
            ("config.toml", &without),
            ("machine.toml", machine),
            ("sub/more.toml", more),
        ]);
        assert_eq!(c.hash(), c2.unwrap().hash());
    }

    #[test]
    fn include_with_tilde_is_from_home() {
        let Ok(tmp) = tempfile::tempdir_in(home()) else {
            return; // read-only home: nothing to check
        };
        let name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        std::fs::write(tmp.path().join("m.toml"), "reserve = 7").unwrap();
        let (_d, c) = files(&[("config.toml", &format!("include = [\"~/{name}/m.toml\"]"))]);
        let c = c.unwrap();
        assert_eq!(c.reserve, 7);
        assert_eq!(c.includes(), [(home().join(&name).join("m.toml"), true)]);
    }

    #[test]
    fn include_errors_name_the_included_file() {
        for (inc, needle) in [
            // One level only.
            ("include = ['x.toml']", "include"),
            ("bogus = 1", "bogus"),
            ("[[rule]]\nname='zz'\nslots=2", "\"zz\""),
        ] {
            let (_d, c) = files(&[("config.toml", "include = ['m.toml']"), ("m.toml", inc)]);
            let err = c.unwrap_err();
            assert!(!matches!(err, ConfigError::Read { .. }), "{inc}: {err}");
            let msg = err.to_string();
            assert!(
                msg.contains("m.toml") && msg.contains(needle),
                "{inc}: {msg}"
            );
        }
        // B5: exists but can't be read.
        let (_d, c) = files(&[("config.toml", "include = ['m.toml']"), ("m.toml/x", "")]);
        assert!(matches!(c, Err(ConfigError::Read { .. })), "{c:?}");
    }

    #[test]
    fn b2_include_global_reweights_only_earlier_rules() {
        let (_d, c) = files(&[
            (
                "config.toml",
                "include = ['a.toml', 'b.toml']\n[[rule]]\nname='r'\nmatch='r'",
            ),
            ("a.toml", "[[rule]]\nname='new'\nmatch='n'"),
            (
                "b.toml",
                "[[rule]]\nname='new'\nslots=2\n[[rule]]\nname='r'\nslots=3",
            ),
        ]);
        assert!(c.is_ok(), "{c:?}");
        // A later include defines it: too late for an earlier entry.
        let (_d, c) = files(&[
            ("config.toml", "include = ['a.toml', 'b.toml']"),
            ("a.toml", "[[rule]]\nname='new'\nslots=2"),
            ("b.toml", "[[rule]]\nname='new'\nmatch='n'"),
        ]);
        assert!(matches!(c, Err(ConfigError::Invalid { .. })), "{c:?}");
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
