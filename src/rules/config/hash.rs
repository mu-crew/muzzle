//! P7 (D28): the merged config's hash.

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{Config, Entry};
use crate::cli::Class;
use crate::rules::heads::Matcher;

impl Config {
    /// P7 (D28): a short hash of the merged config. Each entry carries only
    /// the fields it sets, so an explicit `slots = 1` is a change (in a
    /// table it overrides an inherited value). Global order matters (first
    /// match wins); tables are sorted by canonical key. `v` = 2: no 0.2
    /// hash equals a 0.1 hash. 0.2.1 C5/R7: `command` is a list of
    /// patterns with whitespace collapsed, under its own key; absent, it
    /// isn't serialised, so a 0.2.0 config keeps its hash (C8).
    pub fn hash(&self) -> String {
        #[derive(Serialize)]
        struct NormRule<'a> {
            name: &'a str,
            pattern: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            command: Option<Vec<String>>,
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
                    pattern: match &e.matcher {
                        Some(Matcher::Regex(re)) => Some(re.as_str()),
                        _ => None,
                    },
                    command: match &e.matcher {
                        Some(Matcher::Command(pats)) => {
                            Some(pats.iter().map(|p| p.join(" ")).collect())
                        }
                        _ => None,
                    },
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

#[cfg(test)]
mod tests {
    use crate::rules::config::tests::parse;

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

    /// 0.2.1 C5/R7: `command` hashes with whitespace collapsed and a
    /// string the same as a one-item list; switching matcher kind is a
    /// change.
    #[test]
    fn command_hash_normalises_whitespace_and_kind_matters() {
        let one = hash("[[rule]]\nname='t'\ncommand='cargo test'");
        assert_eq!(hash("[[rule]]\nname='t'\ncommand=' cargo  \ttest '"), one);
        assert_eq!(hash("[[rule]]\nname='t'\ncommand=['cargo test']"), one);
        assert_ne!(hash("[[rule]]\nname='t'\nmatch='cargo test'"), one);
        assert_ne!(hash("[[rule]]\nname='t'\ncommand='cargo build'"), one);
        assert_ne!(
            hash("[[rule]]\nname='t'\ncommand=['cargo test', 'cargo build']"),
            one
        );
        assert_ne!(hash("[[rule]]\nname='t'\ncommand='cargo test*'"), one);
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
}
