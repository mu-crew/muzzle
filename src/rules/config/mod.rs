//! `~/.config/muzzle/config.toml` and its includes: parsing, merging,
//! validation, template and hash. What applies at a cwd is
//! [`super::resolve`].
//!
//! Spec 0.2 (`docs/specs/2026-10-30-muzzle-0.2-paths.md`): P1 path tables,
//! P5 includes, P6 budget keys, P7 hash, B1 key identity, B2 validation,
//! B5 read errors. v1: D8 (slots default 1, capping is admit's job).

mod hash;
mod keys;
mod layer;
mod template;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::cli::Class;
use keys::expand_tilde;
use layer::{Layer, parse_layer};
pub use template::TEMPLATE;
use template::write_template;

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
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(super) fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, Path::new("/nonexistent/config.toml"))
    }

    pub(super) fn invalid(text: &str, needle: &str) {
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{text}: {err}");
        assert!(err.to_string().contains(needle), "{text}: {err}");
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
}
