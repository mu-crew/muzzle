//! 0.2.1 command rules (spec `2026-10-30-muzzle-0.2.1-command-rules.md`):
//! the heads of a shell text, and matching a `command` pattern against one.
//!
//! A head is a simple command from [`shell::commands`] past assignments,
//! keywords and wrappers (C2), plus every command it runs that the text
//! spells out: `sh -c` scripts, the descent table (R4), script programs
//! (R1) and `$VAR` programs (R3). Descent here is its own table: container
//! detection (D16a, `shell::tool_enters`) is unchanged by it.

use std::cell::OnceCell;

use regex::Regex;

use super::shell::{self, SHELLS, base, shell_script, strip_prefix};

/// What a rule matches with (C1): `command` patterns, each a list of
/// words, or a `match` regex over the whole text.
#[derive(Debug, Clone)]
pub enum Matcher {
    Command(Vec<Vec<String>>),
    Regex(Regex),
}

impl Matcher {
    pub fn is_match(&self, text: &str) -> bool {
        self.matches(&Heads::new(text))
    }

    /// Against heads computed once for every rule tried (P4).
    pub fn matches(&self, heads: &Heads) -> bool {
        match self {
            Matcher::Regex(re) => re.is_match(heads.text),
            Matcher::Command(pats) => heads
                .get()
                .iter()
                .any(|h| pats.iter().any(|p| head_matches(h, p))),
        }
    }
}

/// A text and its [`command_heads`], tokenised on first use: a regex-only
/// config never pays for it.
pub struct Heads<'a> {
    text: &'a str,
    heads: OnceCell<Vec<Vec<String>>>,
}

impl<'a> Heads<'a> {
    pub fn new(text: &'a str) -> Self {
        Heads {
            text,
            heads: OnceCell::new(),
        }
    }

    fn get(&self) -> &[Vec<String>] {
        self.heads.get_or_init(|| command_heads(self.text))
    }
}

/// Nesting cut-off for `-c` scripts, descent and script arguments: real
/// shapes nest three deep (`toolbox run … mise exec -- bash -c '…'`).
const MAX_DEPTH: u8 = 8;

/// Every simple command in `text` with assignments, keywords and wrappers
/// stripped, recursing into `-c` scripts, container entry and other
/// descent wrappers (R4), script programs (R1) and variable programs (R3).
/// Outer commands are kept too: `distrobox enter b -- cargo test` gives
/// both the `distrobox` head and the `cargo` one.
pub fn command_heads(text: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    text_heads(text, 0, &mut out);
    out
}

/// C2 rules 3 and 4 with R2 and R4: the program word equals the pattern's
/// program or ends with `/<program>`, and each subcommand word equals the
/// next word that isn't an option (or an argument of one in the program's
/// R4 table). A pattern word ending in `*` matches by prefix (R2).
pub fn head_matches(head: &[String], pattern: &[String]) -> bool {
    let (Some(word), Some(prog)) = (head.first(), pattern.first()) else {
        return false;
    };
    let named = word_matches(word, prog)
        || word
            .match_indices('/')
            .any(|(i, _)| word_matches(&word[i + 1..], prog));
    named && subcommands_match(&head[1..], pattern)
}

/// R5: the hook's exact form. The program word is the pattern's program
/// or `./` plus it, never a path elsewhere (`/tmp/evil/cargo`).
pub fn head_matches_exactly(head: &[String], pattern: &[String]) -> bool {
    let (Some(word), Some(prog)) = (head.first(), pattern.first()) else {
        return false;
    };
    let named = word_matches(word, prog)
        || word
            .strip_prefix("./")
            .is_some_and(|w| word_matches(w, prog));
    named && subcommands_match(&head[1..], pattern)
}

fn word_matches(word: &str, pat: &str) -> bool {
    match pat.strip_suffix('*') {
        Some(prefix) => word.starts_with(prefix),
        None => word == pat,
    }
}

/// R4: program options whose argument is the next word, for subcommand
/// matching. Unlisted options take none.
fn program_opts(prog: &str) -> &'static [&'static str] {
    match prog {
        "cargo" => &["-Z", "-C", "--config", "--color"],
        "npm" => &["--prefix", "-w", "--workspace"],
        _ => &[],
    }
}

fn subcommands_match(mut rest: &[String], pattern: &[String]) -> bool {
    let takes = program_opts(base(&pattern[0]));
    for sub in &pattern[1..] {
        while let Some(w) = rest.first().filter(|w| w.starts_with(['-', '+'])) {
            let n = if takes.contains(&w.as_str()) { 2 } else { 1 };
            rest = &rest[n.min(rest.len())..];
        }
        match rest.split_first() {
            Some((w, tail)) if word_matches(w, sub) => rest = tail,
            _ => return false,
        }
    }
    true
}

fn text_heads(text: &str, depth: u8, out: &mut Vec<Vec<String>>) {
    if depth > MAX_DEPTH {
        return;
    }
    for c in shell::commands(text) {
        heads(&c, depth, out);
    }
}

fn heads(words: &[String], depth: u8, out: &mut Vec<Vec<String>>) {
    if depth > MAX_DEPTH {
        return;
    }
    let words = strip_prefix(words);
    let Some(first) = words.first() else {
        return;
    };
    // R3: `$D bash -c '…'`; what `$D` holds isn't known, so the rest is
    // matched as if it led.
    if is_var(first) {
        return heads(&words[1..], depth + 1, out);
    }
    out.push(words.to_vec());
    if let Some(script) = shell_script(words) {
        return text_heads(script, depth + 1, out);
    }
    if let Some(inner) = descend(words) {
        return match inner {
            Inner::Words(w) => heads(w, depth + 1, out),
            Inner::Text(t) => text_heads(t, depth + 1, out),
        };
    }
    if let Some((script, args)) = script_program(words) {
        // R1.1: the script is the program (already so for `x.sh …`).
        if let Some(file) = script {
            let mut h = vec![file.clone()];
            h.extend_from_slice(args);
            out.push(h);
        }
        // R1.2: `exec.sh [N] npx vitest run` runs its arguments.
        let lead = args
            .iter()
            .take_while(|a| !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit()))
            .count();
        heads(&args[lead..], depth + 1, out);
        // R1.3: `dbx.sh 2400 "contrib/gate | tail"` runs its argument as text.
        for a in args {
            text_heads(a, depth + 1, out);
        }
    }
}

/// `$NAME` or `${NAME}` alone.
fn is_var(w: &str) -> bool {
    let Some(rest) = w.strip_prefix('$') else {
        return false;
    };
    let name = rest
        .strip_prefix('{')
        .and_then(|r| r.strip_suffix('}'))
        .unwrap_or(rest);
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

enum Inner<'a> {
    Words(&'a [String]),
    Text(&'a str),
}

/// Options before the first positional word, with the listed ones taking
/// the next word as their argument. A `--` ends them and is consumed.
fn skip_opts<'a>(mut words: &'a [String], takes: &[&str]) -> &'a [String] {
    while let Some(w) = words.first() {
        if w == "--" {
            return &words[1..];
        }
        if !w.starts_with('-') || w == "-" {
            break;
        }
        let n = if takes.contains(&w.as_str()) { 2 } else { 1 };
        words = &words[n.min(words.len())..];
    }
    words
}

/// The words after the first `--`.
fn after_dashes(words: &[String]) -> Option<&[String]> {
    let at = words.iter().position(|w| w == "--")?;
    Some(&words[at + 1..])
}

/// R4: the command a descent wrapper runs, if `words` is one.
fn descend(words: &[String]) -> Option<Inner<'_>> {
    let (prog, rest) = words.split_first()?;
    let verb = rest.first().map(String::as_str);
    let inner = match (base(prog), verb) {
        ("distrobox", Some("enter")) => after_dashes(&rest[1..])?,
        ("distrobox-enter", _) => after_dashes(rest)?,
        ("mise", Some("exec" | "x")) => after_dashes(&rest[1..])?,
        ("toolbox", Some("run")) => skip_opts(
            &rest[1..],
            &["-c", "--container", "-d", "--distro", "-r", "--release"],
        ),
        ("podman" | "docker", Some("exec")) => {
            let after = skip_opts(
                &rest[1..],
                &[
                    "-w",
                    "--workdir",
                    "-u",
                    "--user",
                    "-e",
                    "--env",
                    "--env-file",
                    "--detach-keys",
                ],
            );
            after.get(1..)?
        }
        ("uv", Some("run")) => skip_opts(
            &rest[1..],
            &[
                "--with",
                "--python",
                "-p",
                "--project",
                "--directory",
                "--extra",
                "--group",
                "--env-file",
                "--package",
            ],
        ),
        ("npx", _) => return positional_command(rest, &["-p", "--package"], &["-c", "--call"], 0),
        ("flock", _) => {
            return positional_command(
                rest,
                &["-w", "--timeout", "-E", "--conflict-exit-code"],
                &["-c", "--command"],
                1,
            );
        }
        _ => return None,
    };
    (!inner.is_empty()).then_some(Inner::Words(inner))
}

/// `npx` and `flock`: options anywhere before the command, `script` options
/// whose argument is shell text, and `skip` positional words (flock's lock
/// or fd, R7) before the command words.
fn positional_command<'a>(
    mut words: &'a [String],
    takes: &[&str],
    script: &[&str],
    mut skip: usize,
) -> Option<Inner<'a>> {
    while let Some(w) = words.first() {
        let w = w.as_str();
        if script.contains(&w) {
            return words.get(1).map(|s| Inner::Text(s));
        }
        if let Some(s) = script
            .iter()
            .filter(|o| o.starts_with("--"))
            .find_map(|o| w.strip_prefix(*o)?.strip_prefix('='))
        {
            return Some(Inner::Text(s));
        }
        if w == "--" {
            words = &words[1..];
        } else if w.starts_with('-') && w != "-" {
            let n = if takes.contains(&w) { 2 } else { 1 };
            words = &words[n.min(words.len())..];
        } else if skip > 0 {
            skip -= 1;
            words = &words[1..];
        } else {
            return Some(Inner::Words(words));
        }
    }
    None
}

/// R1: a script run as the program, with its arguments: `x.sh args…`
/// gives `None` (the script already leads), `bash|sh|zsh [opts] <file>
/// args…` (no `-c`: [`shell_script`] took that) gives the file.
fn script_program(words: &[String]) -> Option<(Option<&String>, &[String])> {
    let (prog, mut rest) = words.split_first()?;
    if base(prog).ends_with(".sh") {
        return Some((None, rest));
    }
    if !SHELLS.contains(&base(prog)) {
        return None;
    }
    while let Some(w) = rest.first().filter(|w| w.starts_with(['-', '+'])) {
        // `bash -n <file>` only parses it: nothing runs. The museum check
        // had 4 `bash -n … contrib/gate` rows that R1.1 claimed for gate.
        if w.starts_with('-') && !w.starts_with("--") && w.contains('n') {
            return None;
        }
        let takes = matches!(
            w.as_str(),
            "-o" | "+o" | "-O" | "+O" | "--rcfile" | "--init-file"
        );
        rest = &rest[if takes { 2 } else { 1 }.min(rest.len())..];
    }
    let (file, args) = rest.split_first()?;
    Some((Some(file), args))
}

#[cfg(test)]
mod tests;
