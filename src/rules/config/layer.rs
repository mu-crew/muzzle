//! One file: raw TOML types, parsed and checked on its own (P1, P3, P6,
//! B1, B2).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

use super::keys::canonical_key;
use super::{ConfigError, Entry};
use crate::cli::Class;
use crate::rules::heads::Matcher;

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
    command: Option<RawCommand>,
    slots: Option<u32>,
    class: Option<RawClass>,
    enabled: Option<bool>,
}

/// C1: `command = "<words>"` or `command = ["<words>", …]`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawCommand {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawClass {
    High,
    Normal,
}

/// One file, parsed and checked on its own.
pub(super) struct Layer {
    pub(super) include: Option<Vec<String>>,
    pub(super) reserve: Option<u32>,
    pub(super) slots: Option<u32>,
    pub(super) global: Vec<Entry>,
    pub(super) tables: Vec<(String, PathBuf, Vec<Entry>)>,
}

pub(super) fn parse_layer(text: &str, path: &Path) -> Result<Layer, ConfigError> {
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
                let bad = |what: &str| invalid(format!("{scope}: rule {:?}: {what}", r.name));
                // C1: the matcher is one field, set by either key.
                let matcher = match (r.pattern, r.command) {
                    (Some(_), Some(_)) => return Err(bad("set command or match, not both")),
                    (Some(p), None) => Some(Matcher::Regex(
                        Regex::new(&p).map_err(|e| bad(&format!("bad match: {e}")))?,
                    )),
                    (None, Some(c)) => {
                        let list = match c {
                            RawCommand::One(s) => vec![s],
                            RawCommand::Many(v) => v,
                        };
                        let pats: Vec<Vec<String>> = list
                            .iter()
                            .map(|s| s.split_whitespace().map(String::from).collect())
                            .collect();
                        if pats.is_empty() || pats.iter().any(Vec::is_empty) {
                            return Err(bad("a command pattern is empty"));
                        }
                        // R2 prefixes are for subcommand words only: a `*`
                        // program would match any program, and the hook
                        // would auto-allow it (R5).
                        if pats.iter().any(|p| p[0].contains('*')) {
                            return Err(bad("a command's program can't contain '*'"));
                        }
                        Some(Matcher::Command(pats))
                    }
                    (None, None) => None,
                };
                Ok(Entry {
                    name: r.name,
                    match_from: matcher.as_ref().map(|_| path.to_path_buf()),
                    matcher,
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

#[cfg(test)]
mod tests {
    use crate::rules::config::ConfigError;
    use crate::rules::config::tests::{invalid, parse};

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
    fn a_command_program_cannot_be_a_prefix() {
        // rv021_glob_program: `*` only on subcommand words (R2).
        invalid("[[rule]]\nname='a'\ncommand='*'", "can't contain '*'");
        invalid(
            "[[rule]]\nname='a'\ncommand=['cargo test','cargo*']",
            "can't contain '*'",
        );
        assert!(parse("[[rule]]\nname='a'\ncommand='npm run test*'").is_ok());
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
}
