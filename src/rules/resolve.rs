//! What applies at a cwd: the target (P2), the cascade (P3, P4) and where
//! each value came from (P8, B4). Infallible: [`Config::parse`] already
//! checked every entry against the rules it can merge into (B2).

use std::path::{Path, PathBuf};

use super::config::{Config, Entry, Table};
use super::heads::{Heads, Matcher};
use crate::cli::Class;

/// A rule with every field filled in.
#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub matcher: Matcher,
    /// D8: 1 when no file sets it; capping is admit's job.
    pub slots: u32,
    pub class: Option<Class>,
}

/// Where a value came from (P8): a file, and the table key as displayed
/// (B1), or `None` for that file's global list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub file: PathBuf,
    pub path: Option<String>,
}

/// B4: `Path` when any applicable table touched the rule, naming the
/// nearest such table as displayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Global,
    Path(String),
}

/// One rule in effect at a target, enabled or not.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub rule: Rule,
    pub enabled: bool,
    pub source: Source,
    pub match_from: Origin,
    pub slots_from: Origin,
}

#[derive(Debug, Clone)]
pub struct Match<'a> {
    pub rule: &'a Rule,
    pub source: &'a Source,
    pub slots_from: &'a Origin,
}

impl<'a> From<&'a Resolved> for Match<'a> {
    fn from(r: &'a Resolved) -> Self {
        Match {
            rule: &r.rule,
            source: &r.source,
            slots_from: &r.slots_from,
        }
    }
}

/// The rules in effect for one target, in match order (P4).
#[derive(Debug, Clone)]
pub struct Effective {
    /// `None` for the global list alone.
    pub target: Option<PathBuf>,
    pub rules: Vec<Resolved>,
}

impl Effective {
    /// The first enabled rule matching `command` (P3, P4).
    pub fn matches(&self, command: &str) -> Option<Match<'_>> {
        let heads = Heads::new(command);
        self.rules
            .iter()
            .find(|r| r.enabled && r.rule.matcher.matches(&heads))
            .map(Match::from)
    }

    /// An enabled rule by name: `run --rule` (B3) and `report --by repo`.
    pub fn rule(&self, name: &str) -> Option<Match<'_>> {
        self.rules
            .iter()
            .find(|r| r.enabled && r.rule.name == name)
            .map(Match::from)
    }
}

/// P2: the repo of `cwd` (its main checkout, so every worktree gets the
/// same chain), else the canonical cwd.
pub fn target_of(cwd: &Path) -> PathBuf {
    crate::repo::repo_of(cwd)
        .unwrap_or_else(|| cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()))
}

fn origin(file: &Path, table: Option<&Table>) -> Origin {
    Origin {
        file: file.to_path_buf(),
        path: table.map(|t| t.key.clone()),
    }
}

/// A new rule from an entry that sets a matcher (P3), or `None` for one that
/// doesn't (B2 makes that unreachable for a rule not already in effect).
fn create(e: &Entry, table: Option<&Table>) -> Option<Resolved> {
    let created = origin(&e.file, table);
    Some(Resolved {
        rule: Rule {
            name: e.name.clone(),
            matcher: e.matcher.clone()?,
            slots: e.slots.unwrap_or(1),
            class: e.class,
        },
        enabled: e.enabled.unwrap_or(true),
        source: match table {
            Some(t) => Source::Path(t.key.clone()),
            None => Source::Global,
        },
        match_from: e
            .match_from
            .as_deref()
            .map_or_else(|| created.clone(), |f| origin(f, table)),
        slots_from: e
            .slots_from
            .as_deref()
            .map_or(created, |f| origin(f, table)),
    })
}

/// P3: merge a table entry into an inherited rule, field by field.
fn merge(r: &mut Resolved, e: &Entry, t: &Table) {
    // C1: `command` and `match` replace each other.
    if let Some(m) = &e.matcher {
        r.rule.matcher = m.clone();
        r.match_from = origin(e.match_from.as_deref().unwrap_or(&e.file), Some(t));
    }
    if let Some(s) = e.slots {
        r.rule.slots = s;
        r.slots_from = origin(e.slots_from.as_deref().unwrap_or(&e.file), Some(t));
    }
    if e.class.is_some() {
        r.rule.class = e.class;
    }
    if let Some(on) = e.enabled {
        r.enabled = on;
    }
    r.source = Source::Path(t.key.clone());
}

impl Config {
    /// The rules in effect for a command run in `cwd` (P2–P4).
    pub fn resolve(&self, cwd: &Path) -> Effective {
        self.resolve_target(Some(&target_of(cwd)))
    }

    /// The rules in effect at `target`, already a P2 target (a repo path
    /// from runs.jsonl); `None` applies the global list alone (B6).
    pub fn resolve_target(&self, target: Option<&Path>) -> Effective {
        let mut rules: Vec<Resolved> = self.global.iter().filter_map(|e| create(e, None)).collect();
        // `tables` is sorted by canonical key, so the ones that apply come
        // outermost first. `starts_with` compares whole components (P2:
        // `crew` is not an ancestor of `crewmate`).
        let chain = self
            .tables
            .iter()
            .filter(|t| target.is_some_and(|p| p.starts_with(&t.canonical)));
        for t in chain {
            let mut fresh = Vec::new();
            for e in &t.entries {
                match rules.iter_mut().find(|r| r.rule.name == e.name) {
                    Some(r) => merge(r, e, t),
                    None => fresh.extend(create(e, Some(t))),
                }
            }
            // P4: a table's new rules go before everything inherited.
            fresh.append(&mut rules);
            rules = fresh;
        }
        Effective {
            target: target.map(Path::to_path_buf),
            rules,
        }
    }

    /// B3: the first definition of `name` in file order (the global list,
    /// then tables by canonical key), active or not. `run --rule` falls
    /// back to it for the class, and bench replay for slots.
    pub fn rule_anywhere(&self, name: &str) -> Option<Rule> {
        self.global
            .iter()
            .map(|e| (e, None))
            .chain(
                self.tables
                    .iter()
                    .flat_map(|t| t.entries.iter().map(move |e| (e, Some(t)))),
            )
            .filter(|(e, _)| e.name == name)
            .find_map(|(e, t)| create(e, t))
            .map(|r| r.rule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        Config::parse(text, Path::new("/nonexistent/config.toml")).unwrap()
    }

    fn names(e: &Effective) -> Vec<&str> {
        e.rules.iter().map(|r| r.rule.name.as_str()).collect()
    }

    /// global → /c → /c/m, as in the spec's call sites, with plain paths
    /// that don't exist (keys normalise lexically, targets are taken as is).
    const CASCADE: &str = r#"
[[rule]]
name = "cargo-test"
match = 'cargo test'
slots = 3
class = "high"
[[rule]]
name = "js-test"
match = 'vitest'
slots = 2

[[path."/c".rule]]
name = "cargo-test"
slots = 2
[[path."/c".rule]]
name = "c-new"
match = 'make'

[[path."/c/m".rule]]
name = "js-test"
enabled = false
[[path."/c/m".rule]]
name = "m-new"
match = 'sweep'
slots = 5
"#;

    #[test]
    fn cascade_merges_field_by_field_down_the_chain() {
        let c = parse(CASCADE);
        let at = |p: &str| c.resolve_target(Some(Path::new(p)));

        let g = c.resolve_target(None);
        let m = g.matches("cargo test").unwrap();
        assert_eq!((m.rule.slots, m.source), (3, &Source::Global));
        assert_eq!(
            *m.slots_from,
            Origin {
                file: "/nonexistent/config.toml".into(),
                path: None
            }
        );

        let crew = at("/c/mu");
        let m = crew.matches("cargo test").unwrap();
        // slots replaced, match and class inherited.
        assert_eq!((m.rule.slots, m.rule.class), (2, Some(Class::High)));
        assert_eq!(m.source, &Source::Path("/c".into()));
        assert_eq!(m.slots_from.path.as_deref(), Some("/c"));
        let r = crew
            .rules
            .iter()
            .find(|r| r.rule.name == "cargo-test")
            .unwrap();
        assert_eq!(r.match_from.path, None);
        assert_eq!(crew.matches("vitest").unwrap().source, &Source::Global);

        let murmur = at("/c/m");
        assert!(murmur.matches("vitest").is_none(), "disabled under /c/m");
        assert!(murmur.rule("js-test").is_none());
        assert_eq!(murmur.matches("cargo test").unwrap().rule.slots, 2);
        // Nearest table's new rules first, then the parent's, then global.
        assert_eq!(names(&murmur), ["m-new", "c-new", "cargo-test", "js-test"]);
        // A new rule without slots gets 1, from where it was created.
        let m = murmur.matches("make").unwrap();
        assert_eq!(
            (m.rule.slots, m.slots_from.path.as_deref()),
            (1, Some("/c"))
        );
    }

    #[test]
    fn components_not_string_prefixes() {
        let c = parse(CASCADE);
        let mate = c.resolve_target(Some(Path::new("/crewmate")));
        assert_eq!(names(&mate), ["cargo-test", "js-test"]);
        let cm = c.resolve_target(Some(Path::new("/c/mx")));
        assert!(cm.matches("vitest").is_some());
        assert_eq!(cm.matches("cargo test").unwrap().rule.slots, 2);
    }

    #[test]
    fn enabled_toggles_down_the_chain() {
        let c = parse(
            r#"
[[rule]]
name = "x"
match = 'x'
enabled = false
[[path."/a".rule]]
name = "x"
enabled = true
[[path."/a/b".rule]]
name = "x"
enabled = false
[[path."/a/b/c".rule]]
name = "x"
enabled = true
"#,
        );
        for (p, on) in [
            ("/", false),
            ("/a", true),
            ("/a/b", false),
            ("/a/b/c/d", true),
        ] {
            let e = c.resolve_target(Some(Path::new(p)));
            assert_eq!(e.matches("x").is_some(), on, "{p}");
        }
    }

    #[test]
    fn override_keeps_its_position_new_rules_go_first() {
        let c = parse(
            r#"
[[rule]]
name = "a"
match = 'cmd'
[[rule]]
name = "b"
match = 'cmd'
[[path."/p".rule]]
name = "b"
slots = 9
[[path."/p".rule]]
name = "n1"
match = 'other'
[[path."/p".rule]]
name = "n2"
match = 'other'
"#,
        );
        let e = c.resolve_target(Some(Path::new("/p")));
        assert_eq!(names(&e), ["n1", "n2", "a", "b"]);
        assert_eq!(e.matches("cmd").unwrap().rule.name, "a");
        assert_eq!(e.matches("other").unwrap().rule.name, "n1");
    }

    /// 0.2.1 C1: in an override, `command` and `match` replace each other.
    #[test]
    fn command_and_match_replace_each_other_in_overrides() {
        let c = parse(
            r#"
[[rule]]
name = "a"
match = 'regex-only'
[[rule]]
name = "b"
command = "cargo test"
[[path."/p".rule]]
name = "a"
command = "make"
[[path."/p".rule]]
name = "b"
match = 'just-b'
"#,
        );
        let g = c.resolve_target(None);
        let p = c.resolve_target(Some(Path::new("/p")));
        let rule = |e: &Effective, t: &str| e.matches(t).map(|m| m.rule.name.clone());
        assert_eq!(rule(&g, "x regex-only").as_deref(), Some("a"));
        assert_eq!(rule(&g, "cargo test").as_deref(), Some("b"));
        assert_eq!(rule(&p, "x regex-only"), None);
        assert_eq!(rule(&p, "make -j").as_deref(), Some("a"));
        assert_eq!(rule(&p, "cargo test"), None);
        assert_eq!(rule(&p, "echo just-b").as_deref(), Some("b"));
    }

    #[test]
    fn rule_anywhere_finds_disabled_and_table_only_rules() {
        let c = parse(
            "[[rule]]\nname='off'\nmatch='x'\nenabled=false\nslots=4\n\
             [[path.\"/p\".rule]]\nname='only'\nmatch='y'\nslots=7\nclass='normal'\n\
             [[path.\"/p\".rule]]\nname='off'\nslots=9",
        );
        assert_eq!(c.rule_anywhere("off").unwrap().slots, 4);
        let only = c.rule_anywhere("only").unwrap();
        assert_eq!((only.slots, only.class), (7, Some(Class::Normal)));
        assert!(c.rule_anywhere("nope").is_none());
        assert!(c.resolve_target(None).rule("only").is_none());
    }

    #[test]
    fn worktrees_and_subdirectories_target_the_main_repo() {
        use crate::repo::tests::{git, jj, sub, tmp};
        let (_t, p) = tmp();
        let main = sub(&p, "main");
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "x"]);
        // The worktree lives outside the table's directory, as mu's do.
        git(&main, &["worktree", "add", "-q", "../wt"]);
        let table = |dir: &Path| {
            parse(&format!(
                "[[path.\"{}\".rule]]\nname='p'\nmatch='x'\n[[path.\"{}\".rule]]\nname='sub'\nmatch='x'",
                dir.display(),
                dir.join("src").display()
            ))
        };
        let c = table(&main);
        for cwd in [main.clone(), sub(&main, "src"), sub(&p, "wt/src")] {
            let e = c.resolve(&cwd);
            assert_eq!(e.target.as_deref(), Some(main.as_path()), "{cwd:?}");
            // P2: a table can't target a directory inside a repo.
            assert_eq!(e.matches("x").unwrap().rule.name, "p", "{cwd:?}");
        }
        // Outside any repo, the canonical cwd.
        let plain = sub(&p, "plain");
        assert_eq!(c.resolve(&plain.join(".")).target, Some(plain));

        if jj(&p, &["git", "init", "--colocate", "jmain"]) {
            let jmain = p.join("jmain");
            jj(&jmain, &["workspace", "add", "../jws"]);
            let c = table(&jmain);
            let e = c.resolve(&sub(&p, "jws/x"));
            assert_eq!(e.matches("x").unwrap().rule.name, "p");
        }
    }
}
