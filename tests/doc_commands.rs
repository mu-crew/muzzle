//! Every `muzzle …` command the skill and the docs show must resolve
//! against the built binary (spec §The skill, D34; IDEAS.md's tuios entry:
//! "skill tests resolve every command"). A doc that names a removed
//! subcommand, a renamed flag or a bad value fails here, not in an agent's
//! hands.
//!
//! Scope, kept narrow: each command is run as `muzzle <args before --> --help`.
//! clap still parses every flag and value before printing help, so an
//! unknown subcommand or flag, a bad enum value or a non-number exits 2.
//! It is not a semantic check: missing required flags are the CLI's job.
//!
//! Commands come from fenced code blocks and from inline code spans, never
//! from prose, so "muzzle queues …" is not a command. `…` marks elided
//! arguments and is dropped. Placeholders such as `N` or `<before>` would
//! fail, so the docs show concrete values.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The skill and its recipes (the DONE WHEN), plus the user docs that show
/// commands. CHANGELOG is history, so it may name removed surface.
fn doc_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let skill = root.join("npm/muzzle/skills/muzzle");
    let mut files = vec![skill.join("SKILL.md")];
    let mut recipes: Vec<PathBuf> = std::fs::read_dir(skill.join("recipes"))
        .expect("recipes dir")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    recipes.sort();
    files.extend(recipes);
    for f in [
        "README.md",
        "docs/why.md",
        "docs/usage.md",
        "docs/harnesses/README.md",
        "bench/README.md",
    ] {
        files.push(root.join(f));
    }
    files
}

#[derive(Debug)]
struct DocCommand {
    file: String,
    line: usize,
    /// The arguments after `muzzle`, up to the end of the command.
    args: Vec<String>,
}

/// Code fragments with the line each starts on: every line of a fenced
/// block, and every inline code span (which may wrap across lines).
fn code_fragments(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut in_fence = false;
    // Prose between fences, with its first line number, scanned for spans.
    let mut prose = String::new();
    let mut prose_start = 1;
    let flush = |prose: &mut String, start: usize, out: &mut Vec<(usize, String)>| {
        let mut line = start;
        let mut rest = prose.as_str();
        while let Some(open) = rest.find('`') {
            line += rest[..open].matches('\n').count();
            let after = &rest[open + 1..];
            let Some(close) = after.find('`') else { break };
            let span = &after[..close];
            out.push((line, span.replace('\n', " ")));
            line += span.matches('\n').count();
            rest = &after[close + 1..];
        }
        prose.clear();
    };
    for (i, l) in text.lines().enumerate() {
        let n = i + 1;
        if l.trim_start().starts_with("```") {
            if !in_fence {
                flush(&mut prose, prose_start, &mut out);
            }
            in_fence = !in_fence;
            prose_start = n + 1;
            continue;
        }
        if in_fence {
            out.push((n, l.to_string()));
        } else {
            prose.push_str(l);
            prose.push('\n');
        }
    }
    flush(&mut prose, prose_start, &mut out);
    out
}

/// Shell-ish words of one command, stopping at the end of the command:
/// an operator, a redirection, a comment, or the quote that opened it.
fn words(s: &str, closing: Option<char>) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if Some(c) == closing => break,
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            None if "|;&<>)`".contains(c) => break,
            None if c == '#' && !in_word => break,
            None => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// `muzzle` in command position in a code fragment, followed by a space:
/// - at the start, or right after a directory (`~/bin/muzzle`) or a quote
///   (`"command": "muzzle hook claude"`);
/// - after `|`, `;`, `&` or a subshell `(` / `$(`, but not after a word's
///   `(` (`Bash(muzzle run:*)` is a permission pattern);
/// - after whitespace, when the word before is an operator, `exec`,
///   `then`, `do`, `else`, `time`, `env`, `nice` or `NAME=value`.
///
/// So `muzzle:` (its stderr lines), `--mode muzzle`, `muzzle-cli` and
/// `skills/muzzle/` are not commands.
fn command_position(before: &str) -> bool {
    let Some(last) = before.chars().last() else {
        return true;
    };
    if last == '(' {
        let prev = before[..before.len() - 1].chars().last();
        return prev.is_none_or(|c| !c.is_alphanumeric() && c != '_');
    }
    if "/'\"|;&".contains(last) {
        return true;
    }
    if !last.is_whitespace() {
        return false;
    }
    let Some(word) = before.split_whitespace().last() else {
        return true;
    };
    let assignment = word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    assignment
        || word.ends_with(['|', ';', '&', '('])
        || ["exec", "then", "do", "else", "time", "env", "nice"].contains(&word)
}

fn extract(file: &str, text: &str) -> Vec<DocCommand> {
    let mut out = Vec::new();
    for (line, frag) in code_fragments(text) {
        let mut from = 0;
        while let Some(i) = frag[from..].find("muzzle") {
            let at = from + i;
            from = at + "muzzle".len();
            let before = frag[..at].chars().last();
            let lead_ok = command_position(&frag[..at]);
            let rest = &frag[from..];
            if !lead_ok || !rest.starts_with(' ') {
                continue;
            }
            let closing = before.filter(|c| *c == '"' || *c == '\'');
            let args: Vec<String> = words(rest, closing)
                .into_iter()
                .filter(|w| w != "…")
                .collect();
            // `command -v muzzle`: a lookup, not a command.
            if args.is_empty() {
                continue;
            }
            out.push(DocCommand {
                file: file.to_string(),
                line,
                args,
            });
        }
    }
    out
}

/// `None` when the command resolves, else why not.
fn check(cmd: &DocCommand) -> Option<String> {
    let mut args: Vec<&str> = cmd
        .args
        .iter()
        .map(String::as_str)
        .take_while(|a| *a != "--")
        .collect();
    args.push("--help");
    let out = Command::new(env!("CARGO_BIN_EXE_muzzle"))
        .args(&args)
        .output()
        .expect("spawn muzzle");
    (!out.status.success()).then(|| {
        let err = String::from_utf8_lossy(&out.stderr);
        format!(
            "{}:{}: `muzzle {}` does not resolve: {}",
            cmd.file,
            cmd.line,
            cmd.args.join(" "),
            err.lines().next().unwrap_or("").trim()
        )
    })
}

fn all_commands() -> Vec<DocCommand> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    doc_files()
        .iter()
        .flat_map(|p| {
            let text = std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            extract(&rel, &text)
        })
        .collect()
}

#[test]
fn every_command_in_the_skill_and_docs_resolves() {
    let cmds = all_commands();
    for c in &cmds {
        println!("{}:{}: muzzle {}", c.file, c.line, c.args.join(" "));
    }
    let problems: Vec<String> = cmds.iter().filter_map(check).collect();
    assert!(
        problems.is_empty(),
        "{} of {} doc commands don't resolve:\n{}",
        problems.len(),
        cmds.len(),
        problems.join("\n")
    );
}

#[test]
fn the_guard_is_looking_at_the_skill() {
    let cmds = all_commands();
    let in_skill = |name: &str| cmds.iter().filter(|c| c.file.ends_with(name)).count();
    // Floors, not targets: a broken extractor finds about 0.
    assert!(in_skill("SKILL.md") >= 5, "{cmds:#?}");
    for r in ["tune.md", "bench.md", "harnesses.md"] {
        assert!(in_skill(r) >= 3, "{r}: {cmds:#?}");
    }
    // Every subcommand the skill teaches shows up at least once.
    for sub in [
        "run", "match", "status", "doctor", "report", "bench", "hook",
    ] {
        assert!(
            cmds.iter()
                .any(|c| c.file.contains("skills/") && c.args.first().is_some_and(|a| a == sub)),
            "the skill never shows `muzzle {sub}`"
        );
    }
}

/// A guard that can't fail is theatre: each drift shape is caught.
#[test]
fn planted_drift_is_caught() {
    for (snippet, why) in [
        ("`muzzle report --comapre 7d..1d 1d..now`", "a renamed flag"),
        (
            "```sh\nmuzzle bench replay t.jsonl\n```",
            "a removed subcommand",
        ),
        ("`muzzle run --class urgent -- true`", "a bad enum value"),
        ("`muzzle run --slots N -- true`", "a placeholder value"),
        (
            "`muzzle report\n--compare 7d..1d`",
            "a span wrapped across lines",
        ),
        (
            "```json\n\"command\": \"muzzle hook cursor\"\n```",
            "a quoted hook command",
        ),
        (
            "```sh\n*/5 * * * * $HOME/bin/muzzle bench probe --bogus\n```",
            "a path-prefixed binary",
        ),
    ] {
        let found = extract("fixture.md", snippet);
        assert_eq!(found.len(), 1, "{why}: {found:?}");
        assert!(check(&found[0]).is_some(), "{why} passed: {found:?}");
    }
}

#[test]
fn prose_and_non_commands_are_not_extracted() {
    let text = "muzzle queues each command.\n\
                `muzzle: waited 0s for 2 slots, ran 1s`\n\
                `cargo install muzzle-cli`, `skills/muzzle/SKILL.md`\n\
                `--mode muzzle --tier real`, `Bash(muzzle run:*)`\n\
                ```sh\ncommand -v muzzle >/dev/null\n```\n";
    let found = extract("fixture.md", text);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn the_command_ends_where_the_shell_says() {
    let found = extract(
        "fixture.md",
        "```sh\nmuzzle run --rule gate -- x | tee log # note\n\
         echo '{}' | muzzle hook claude > out\n```",
    );
    let args: Vec<_> = found.iter().map(|c| c.args.join(" ")).collect();
    assert_eq!(args, ["run --rule gate -- x", "hook claude"]);
    assert!(found.iter().all(|c| check(c).is_none()), "{found:?}");
}
