//! 0.2.1 command rules (spec `2026-10-30-command-rules.md`):
//! the heads of a shell text, and matching a `command` pattern against one.
//!
//! A head is a simple command from [`shell::commands`] past assignments,
//! keywords and wrappers (C2), plus every command it runs that the text
//! spells out: `sh -c` scripts, the descent table (R4), script programs
//! (R1) and `$VAR` programs (R3). Descent here is its own table: container
//! detection (D16a, `shell::tool_enters`) is unchanged by it.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use regex::Regex;

use super::shell::{
    self, BODY_CAP, Cmd, SHELLS, base, is_assignment, shell_args, shell_script, strip_prefix,
};

/// R5: where a match came from. `Inspected` is a match found in a heredoc,
/// a script file or a script body (H1-H3), never in the words of the
/// command itself; the hook must not approve what it only inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchProvenance {
    Direct,
    Inspected,
}

/// The head H3 emits for a python/node body with a heavy signal, which the
/// built-in `script-heavy` rule matches. The `\u{1}` keeps any real
/// command from spelling it.
pub const SCRIPT_HEAVY_HEAD: &str = "\u{1}script-heavy";
/// Nothing emits this: `unmatched-heavy` is only ever named, by `wrap
/// --with-grace` and `run --rule` (H4).
pub const UNMATCHED_HEAVY_HEAD: &str = "\u{1}unmatched-heavy";

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
        self.provenance(heads).is_some()
    }

    /// How this matcher matches `heads`, if it does: `Direct` when any
    /// head spelt out by the command matches, else `Inspected`.
    pub fn provenance(&self, heads: &Heads) -> Option<MatchProvenance> {
        match self {
            Matcher::Regex(re) => re.is_match(heads.text).then_some(MatchProvenance::Direct),
            Matcher::Command(pats) => {
                let a = heads.get();
                let mut inspected = false;
                for (h, insp) in a.heads.iter().zip(&a.inspected) {
                    if pats.iter().any(|p| head_matches(h, p)) {
                        if !insp {
                            return Some(MatchProvenance::Direct);
                        }
                        inspected = true;
                    }
                }
                (inspected || a.bodies.iter().any(|b| body_matches(b, pats)))
                    .then_some(MatchProvenance::Inspected)
            }
        }
    }
}

/// What [`Heads`] found in a text.
struct Analysis {
    heads: Vec<Vec<String>>,
    /// In step with `heads`: found by inspecting a body (H1-H3).
    inspected: Vec<bool>,
    /// python/node bodies with a heavy signal (H3), for rule words.
    bodies: Vec<String>,
}

/// A text and its [`command_heads`], tokenised on first use: a regex-only
/// config never pays for it.
pub struct Heads<'a> {
    text: &'a str,
    cwd: Option<PathBuf>,
    analysis: OnceCell<Analysis>,
}

impl<'a> Heads<'a> {
    pub fn new(text: &'a str) -> Self {
        Self::new_in(text, None)
    }

    /// H2: with a `cwd`, local script files the text runs are read and
    /// their commands count too.
    pub fn new_in(text: &'a str, cwd: Option<&Path>) -> Self {
        Heads {
            text,
            cwd: cwd.map(Path::to_path_buf),
            analysis: OnceCell::new(),
        }
    }

    fn get(&self) -> &Analysis {
        self.analysis
            .get_or_init(|| analyse(self.text, self.cwd.clone()))
    }
}

/// Nesting cut-off for `-c` scripts, descent and script arguments: real
/// shapes nest three deep (`toolbox run … mise exec -- bash -c '…'`).
const MAX_DEPTH: u8 = 8;

/// Every simple command in `text` with assignments, keywords and wrappers
/// stripped, recursing into `-c` scripts, container entry and other
/// descent wrappers (R4), script programs (R1) and variable programs (R3),
/// and heredocs a shell runs (H1). No files are read.
/// Outer commands are kept too: `distrobox enter b -- cargo test` gives
/// both the `distrobox` head and the `cargo` one.
pub fn command_heads(text: &str) -> Vec<Vec<String>> {
    analyse(text, None).heads
}

fn analyse(text: &str, cwd: Option<PathBuf>) -> Analysis {
    let mut w = Walk {
        out: Vec::new(),
        insp: Vec::new(),
        bodies: Vec::new(),
        inspected: false,
        cwd,
        rel: Some(PathBuf::new()),
        vars: HashMap::new(),
        written: Vec::new(),
        files: 0,
        bytes: 0,
        seen: HashSet::new(),
    };
    w.text_heads(text, 0);
    Analysis {
        heads: w.out,
        inspected: w.insp,
        bodies: w.bodies,
    }
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
/// or `./` plus it, never a path elsewhere (`/tmp/evil/cargo`), the
/// pattern's subcommand words follow it, and (fix-hook-strict) every
/// word after the program is on its list in [`allow`]: a program without
/// a list takes exactly the pattern's words. Anything unknown is wrapped
/// but left for the user to approve.
pub fn head_matches_exactly(head: &[String], pattern: &[String]) -> bool {
    let (Some(word), Some(prog)) = (head.first(), pattern.first()) else {
        return false;
    };
    if !(word == prog || word.strip_prefix("./") == Some(prog.as_str())) {
        return false;
    }
    let rest = &head[1..];
    if !allow::listed(prog) {
        return rest.len() == pattern.len() - 1
            && rest
                .iter()
                .zip(&pattern[1..])
                .all(|(w, p)| word_matches(w, p));
    }
    // The subcommand words are the next words that aren't options: no R4
    // option arguments here, and [`allow::allowed`] refuses any option
    // that takes one before the subcommand (`cargo -C /x test`).
    let mut words = rest;
    for sub in &pattern[1..] {
        while words.first().is_some_and(|w| w.starts_with(['-', '+'])) {
            words = &words[1..];
        }
        match words.split_first() {
            Some((w, tail)) if word_matches(w, sub) => words = tail,
            _ => return false,
        }
    }
    allow::allowed(prog, rest, pattern.len() - 1)
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

/// H2: most files, and bytes, one [`Heads`] evaluation reads.
const MAX_FILES: u32 = 8;
const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Python,
    Node,
}

/// One evaluation: the heads found so far and the H1-H3 state.
struct Walk {
    out: Vec<Vec<String>>,
    insp: Vec<bool>,
    bodies: Vec<String>,
    /// Heads pushed now come from an inspected body.
    inspected: bool,
    /// Where relative script paths resolve; `None` reads no files.
    cwd: Option<PathBuf>,
    /// The effective directory relative to where the text started, folded
    /// (`None` once a `cd` can't be told): written files are keyed by it,
    /// so `cat > f; cd sub; bash f` are two files (H1.3).
    rel: Option<PathBuf>,
    /// Literal assignments of the shell text being walked (H1.3).
    vars: HashMap<String, String>,
    /// `(path, body)` of heredocs `cat`/`tee` wrote in this text (H1.3).
    written: Vec<(String, String)>,
    files: u32,
    bytes: usize,
    /// `(dev, ino)` of files read: a script that sources itself ends here.
    seen: HashSet<(u64, u64)>,
}

impl Walk {
    fn push(&mut self, words: &[String]) {
        self.out.push(words.to_vec());
        self.insp.push(self.inspected);
    }

    fn text_heads(&mut self, text: &str, depth: u8) {
        if depth > MAX_DEPTH {
            return;
        }
        // Assignments, written files and `cd` belong to one shell string.
        let cwd = self.cwd.clone();
        let rel = self.rel.clone();
        let vars = std::mem::take(&mut self.vars);
        let written = std::mem::take(&mut self.written);
        for c in shell::commands_full(text) {
            self.command(&c, depth);
        }
        self.cwd = cwd;
        self.rel = rel;
        self.vars = vars;
        self.written = written;
    }

    /// Text found in a heredoc or file: its heads are inspected ones.
    fn inspect_text(&mut self, text: &str, depth: u8) {
        let prev = std::mem::replace(&mut self.inspected, true);
        self.text_heads(text, depth + 1);
        self.inspected = prev;
    }

    fn command(&mut self, c: &Cmd, depth: u8) {
        let stripped = strip_prefix(&c.words);
        let Some(first) = stripped.first() else {
            // Only assignments: `f=/tmp/x.sh`. A value with `$` or a glob
            // isn't known, so it forgets the name.
            for (i, w) in c.words.iter().enumerate() {
                if let Some((name, value)) = w.split_once('=')
                    && is_assignment(w)
                {
                    // A lifted `$(…)` leaves part of the value: unknown.
                    let value = (!c.dynamic.contains(&i)).then(|| self.expand(value));
                    match value.flatten() {
                        Some(v) if !v.contains(['$', '*', '?', '`', '~']) => {
                            self.vars.insert(name.to_string(), v);
                        }
                        _ => {
                            self.vars.remove(name);
                        }
                    }
                }
            }
            return;
        };
        if first == "cd" {
            self.cd(&stripped[1..]);
        }
        self.register_writes(c, stripped);
        self.heads(&c.words, c.stdin.as_deref(), depth);
    }

    /// H2.2: the directory after `cd <dir>`; unknown when it can't be told.
    fn cd(&mut self, args: &[String]) {
        let dir = args.iter().find(|a| !a.starts_with('-') || *a == "-");
        let dir = dir
            .and_then(|d| self.expand(d))
            .filter(|d| d != "-" && !d.starts_with('~'));
        self.cwd = match (&dir, &self.cwd) {
            (Some(d), Some(cwd)) => Some(cwd.join(d)),
            _ => None,
        };
        self.rel = match (&dir, &self.rel) {
            (Some(d), Some(rel)) => Some(fold(&rel.join(d))),
            _ => None,
        };
    }

    /// `$NAME` and `${NAME}` in `w` from the literal assignments seen; a
    /// reference to anything else leaves it unknown.
    fn expand(&self, w: &str) -> Option<String> {
        let mut out = String::new();
        let mut rest = w;
        while let Some(i) = rest.find('$') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            let (name, tail) = match after.strip_prefix('{') {
                Some(r) => {
                    let end = r.find('}')?;
                    (&r[..end], &r[end + 1..])
                }
                None => {
                    let end = after
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(after.len());
                    (&after[..end], &after[end..])
                }
            };
            out.push_str(self.vars.get(name)?);
            rest = tail;
        }
        out.push_str(rest);
        Some(out)
    }

    /// H1.3: only `cat` and `tee` copy stdin to a file. `cat > f <<EOF`
    /// and `tee f <<EOF` remember the body under `f`; a bare `> f <<EOF`
    /// writes nothing and is not remembered. `cat` copies stdin when it
    /// has no file operand or one is `-`, and its stdout goes to the last
    /// `>` only (bash opens every one, but the others stay empty).
    fn register_writes(&mut self, c: &Cmd, stripped: &[String]) {
        let Some(body) = c.stdin.as_deref() else {
            return;
        };
        let args: Vec<&String> = stripped[1..]
            .iter()
            .filter(|a| !a.starts_with('-') || *a == "-")
            .collect();
        let targets: Vec<String> = match base(&stripped[0]) {
            "cat" if args.is_empty() || args.iter().any(|a| *a == "-") => {
                c.writes.last().cloned().into_iter().collect()
            }
            "tee" => args.into_iter().filter(|a| *a != "-").cloned().collect(),
            _ => return,
        };
        for t in targets {
            if let Some(key) = self.expand(&t).and_then(|p| self.key(&p)) {
                self.written.retain(|(p, _)| *p != key);
                self.written.push((key, body.to_string()));
            }
        }
    }

    /// Where `path` is, as a key for [`Self::written`]: folded and
    /// relative to the directory the text started in.
    fn key(&self, path: &str) -> Option<String> {
        let full = fold(&self.rel.as_ref()?.join(path));
        Some(full.to_string_lossy().into_owned())
    }

    fn heads(&mut self, words: &[String], stdin: Option<&str>, depth: u8) {
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
            // A variable of a literal assignment is the program it names
            // (`f=./new.sh; "$f"`); one word, as bash would see it.
            let known = self
                .expand(first)
                .filter(|v| !v.is_empty() && !v.contains(char::is_whitespace));
            return match known {
                Some(v) => {
                    let mut w = words.to_vec();
                    w[0] = v;
                    self.heads(&w, stdin, depth + 1)
                }
                None => self.heads(&words[1..], stdin, depth + 1),
            };
        }
        self.push(words);
        if let Some(script) = shell_script(words) {
            // The heredoc of `bash -c : <<EOF` is data (H1.2).
            return self.text_heads(script, depth + 1);
        }
        if let Some(inner) = descend(words) {
            return match inner {
                Inner::Words(w) => self.heads(w, stdin, depth + 1),
                Inner::Text(t) => self.text_heads(t, depth + 1),
            };
        }
        self.inspect(words, stdin, depth);
        if let Some((script, args)) = script_program(words) {
            // R1.1: the script is the program (already so for `x.sh …`).
            if let Some(file) = script {
                let mut h = vec![file.clone()];
                h.extend_from_slice(args);
                self.push(&h);
            }
            // R1.2: `exec.sh [N] npx vitest run` runs its arguments.
            let lead = args
                .iter()
                .take_while(|a| !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit()))
                .count();
            self.heads(&args[lead..], None, depth + 1);
            // R1.3: `dbx.sh 2400 "contrib/gate | tail"` runs its argument as text.
            for a in args {
                self.text_heads(a, depth + 1);
            }
        }
    }

    /// H1-H3: what `words` runs that the text doesn't spell out: its
    /// stdin heredoc, a script file or a python/node body.
    fn inspect(&mut self, words: &[String], stdin: Option<&str>, depth: u8) {
        let prog = base(&words[0]);
        if SHELLS.contains(&prog) {
            let a = shell_args(&words[0], &words[1..]);
            if a.n || a.c {
                return;
            }
            let opts = &words[1..words.len() - a.rest.len()];
            // `-s`, alone or in a cluster (`-xs`), and `-` read stdin.
            let reads_stdin = opts.iter().any(|w| {
                w == "-" || (w.starts_with('-') && !w.starts_with("--") && w.contains('s'))
            });
            match a.rest.first() {
                Some(file) if !reads_stdin => self.run_file(file, depth),
                // H1.2: no script, so stdin is the shell's code.
                _ => {
                    if let Some(text) = stdin {
                        self.inspect_text(text, depth);
                    }
                }
            }
        } else if prog == "source" || prog == "." {
            if let Some(file) = words[1..].iter().find(|w| !w.starts_with('-')) {
                self.run_file(file, depth);
            }
        } else if let Some(lang) = lang_of(prog) {
            self.script_lang(lang, &words[1..], stdin);
        } else if words[0].contains('/') {
            self.run_file(&words[0], depth);
        }
    }

    /// A script file the text runs: written earlier in it (H1.3) or on
    /// disk (H2). A disk file is routed by its shebang's interpreter alone
    /// (H2.3); a written one falls back to its extension (H1.3).
    fn run_file(&mut self, file: &str, depth: u8) {
        let Some(path) = self.expand(file) else {
            return;
        };
        let Some((body, written)) = self.source_of(&path) else {
            return;
        };
        let lang = shebang_lang(&body).or_else(|| {
            if !written {
                return None;
            }
            match Path::new(&path).extension().and_then(|e| e.to_str()) {
                Some("py") => Some(Lang::Python),
                Some("js" | "mjs" | "cjs") => Some(Lang::Node),
                _ => None,
            }
        });
        match lang {
            Some(l) => self.heavy_body(body, l),
            None => self.inspect_text(&body, depth),
        }
    }

    /// The body of `path`: a heredoc written to it earlier (`true`), else
    /// the file.
    fn source_of(&mut self, path: &str) -> Option<(String, bool)> {
        if let Some(key) = self.key(path)
            && let Some((_, b)) = self.written.iter().rev().find(|(p, _)| *p == key)
        {
            return Some((b.clone(), true));
        }
        self.read_script(path).map(|b| (b, false))
    }

    /// H3.1: input-mode resolution first, then the body it names.
    fn script_lang(&mut self, lang: Lang, args: &[String], stdin: Option<&str>) {
        let mut i = 0;
        let mode = loop {
            let Some(a) = args.get(i).map(String::as_str) else {
                break Mode::Stdin;
            };
            match (lang, a) {
                (_, "-") => break Mode::Stdin,
                // `--` ends the options: the next word is the file.
                (_, "--") => break args.get(i + 1).map_or(Mode::Stdin, |f| Mode::File(f)),
                (Lang::Python, "-c") => break Mode::Code(args.get(i + 1)),
                (Lang::Python, "-m") => break Mode::None,
                (Lang::Python, "-W" | "-X") => i += 2,
                (Lang::Python, _) if a.starts_with("-c") => {
                    break Mode::Glued(a[2..].to_string());
                }
                (Lang::Node, "-e" | "-p" | "--eval" | "--print" | "-pe") => {
                    break Mode::Code(args.get(i + 1));
                }
                (Lang::Node, _) if a.starts_with("--eval=") || a.starts_with("--print=") => {
                    break Mode::Glued(a.split_once('=').map_or("", |(_, c)| c).to_string());
                }
                (Lang::Node, "-r" | "--require" | "--import" | "--loader" | "--input-type") => {
                    i += 2
                }
                (_, _) if a.starts_with('-') => i += 1,
                _ => break Mode::File(a),
            }
        };
        let body = match mode {
            Mode::Code(c) => c.cloned(),
            Mode::Glued(c) => Some(c),
            Mode::File(f) => self
                .expand(f)
                .and_then(|p| self.source_of(&p).map(|(b, _)| b)),
            Mode::Stdin => stdin.map(String::from),
            Mode::None => None,
        };
        if let Some(b) = body {
            self.heavy_body(b, lang);
        }
    }

    /// H3.2: a python/node body with a heavy signal is a head of its own,
    /// for the built-in `script-heavy` rule, and keeps its text for the
    /// configured rules' words.
    fn heavy_body(&mut self, mut body: String, lang: Lang) {
        if body.len() > BODY_CAP {
            let mut end = BODY_CAP;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
        }
        if !has_signal(&body, lang) {
            return;
        }
        let prev = std::mem::replace(&mut self.inspected, true);
        self.push(&[SCRIPT_HEAVY_HEAD.to_string()]);
        self.inspected = prev;
        self.bodies.push(body);
    }

    /// H2.3: read a local script safely, within the evaluation's budget.
    fn read_script(&mut self, path: &str) -> Option<String> {
        let cwd = self.cwd.as_ref()?;
        if self.files >= MAX_FILES || self.bytes >= MAX_BYTES {
            return None;
        }
        // The final component is not followed (below); the directory above
        // it is resolved by the file system, so `ln/..` is the parent of
        // what `ln` points to, not of where it sits.
        let full = cwd.join(path);
        let real = full.parent()?.canonicalize().ok()?.join(full.file_name()?);
        if blocked(&real) {
            return None;
        }
        // NONBLOCK: a FIFO with no writer opens at once instead of
        // hanging; NOFOLLOW: a symlink to some system file is skipped.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&real)
            .ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() || !self.seen.insert((meta.dev(), meta.ino())) {
            return None;
        }
        self.files += 1;
        let limit = (16 * 1024).min(MAX_BYTES - self.bytes);
        let mut buf = Vec::new();
        file.take(limit as u64).read_to_end(&mut buf).ok()?;
        self.bytes += buf.len();
        if is_binary(&buf) {
            return None;
        }
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

enum Mode<'a> {
    Code(Option<&'a String>),
    Glued(String),
    File(&'a str),
    Stdin,
    /// `python -m mod`: no body to read.
    None,
}

fn lang_of(prog: &str) -> Option<Lang> {
    if prog == "node" || prog == "nodejs" {
        return Some(Lang::Node);
    }
    let v = prog.strip_prefix("python")?;
    v.bytes()
        .all(|b| b.is_ascii_digit() || b == b'.')
        .then_some(Lang::Python)
}

/// `.` and `..` folded away without touching the file system: a key for
/// paths, never what is opened. A `..` with nothing to pop stays.
fn fold(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {}
                _ => out.push(".."),
            },
            c => out.push(c),
        }
    }
    out
}

/// The language a script's shebang names as its interpreter, past `env`
/// and its options: `#!/usr/bin/env -S python3 -u`, not `#!/bin/sh # python`.
fn shebang_lang(body: &str) -> Option<Lang> {
    let line = body.lines().next()?.strip_prefix("#!")?;
    let mut words = line.split_whitespace();
    let mut prog = base(words.next()?);
    if prog == "env" {
        prog = base(words.find(|w| !w.starts_with('-') && !w.contains('='))?);
    }
    lang_of(prog)
}

/// H2.3: where a script is never read: system and tool directories, and
/// anything under `node_modules`.
fn blocked(p: &Path) -> bool {
    const DIRS: [&str; 9] = [
        "/bin",
        "/usr/bin",
        "/usr/local/bin",
        "/sbin",
        "/usr/sbin",
        "/opt",
        "/dev",
        "/proc",
        "/sys",
    ];
    DIRS.iter().any(|d| p.starts_with(d)) || p.components().any(|c| c.as_os_str() == "node_modules")
}

/// ELF, Mach-O (thin and fat, either byte order) or a NUL in the first
/// 512 bytes.
fn is_binary(buf: &[u8]) -> bool {
    const MAGICS: [[u8; 4]; 9] = [
        *b"\x7fELF",
        [0xfe, 0xed, 0xfa, 0xce],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
        // Fat with the other byte order, and both fat64.
        [0xbe, 0xba, 0xfe, 0xca],
        [0xca, 0xfe, 0xba, 0xbf],
        [0xbf, 0xba, 0xfe, 0xca],
    ];
    MAGICS.iter().any(|m| buf.starts_with(m)) || buf.iter().take(512).any(|&b| b == 0)
}

/// H3.2: the heavy signals of a script body in `lang`; each language has
/// its own, so a string `'child_process'` in Python is only a string.
fn has_signal(body: &str, lang: Lang) -> bool {
    const BOTH: [&str; 2] = ["cargo", "vitest"];
    let (phrases, words): (&[&str], &[&str]) = match lang {
        Lang::Python => (
            &[
                "subprocess.",
                "from subprocess import",
                "import subprocess",
                "os.system(",
                "os.popen(",
            ],
            &["multiprocessing", "pytest", "buck2", "lake"],
        ),
        Lang::Node => (
            &[
                "child_process",
                "spawnSync(",
                "execFileSync(",
                "execSync(",
                "spawn(",
                "esbuild",
            ],
            &["tsup", "tsc"],
        ),
    };
    phrases.iter().any(|p| body.contains(p)) || words.iter().chain(&BOTH).any(|w| has_word(body, w))
}

/// `word` in `body` not glued to letters, digits or `_` (`lake` is not
/// `flake`).
fn has_word(body: &str, word: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    body.match_indices(word).any(|(i, _)| {
        !body[..i].chars().next_back().is_some_and(ident)
            && !body[i + word.len()..].chars().next().is_some_and(ident)
    })
}

/// H3.2: a configured `command` pattern's words, in order, within a
/// 3-token window of a body split on whitespace, quotes, commas, brackets
/// and parens: `subprocess.run(["cargo", "test"])` has `cargo test`.
fn body_matches(body: &str, pats: &[Vec<String>]) -> bool {
    let toks: Vec<&str> = body
        .split(|c: char| c.is_whitespace() || "'\"`,[](){};=".contains(c))
        .filter(|t| !t.is_empty())
        .collect();
    pats.iter().any(|p| window_matches(&toks, p))
}

fn window_matches(toks: &[&str], pat: &[String]) -> bool {
    let Some((prog, subs)) = pat.split_first() else {
        return false;
    };
    // Fixed: a pattern of more than 3 words never matches a body.
    let window = 3;
    toks.iter().enumerate().any(|(i, t)| {
        let named = word_matches(t, prog)
            || t.match_indices('/')
                .any(|(j, _)| word_matches(&t[j + 1..], prog));
        if !named {
            return false;
        }
        let end = (i + window).min(toks.len());
        let mut at = i + 1;
        subs.iter().all(
            |s| match toks[at..end].iter().position(|t| word_matches(t, s)) {
                Some(k) => {
                    at += k + 1;
                    true
                }
                None => false,
            },
        )
    })
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
    let (prog, rest) = words.split_first()?;
    if base(prog).ends_with(".sh") {
        return Some((None, rest));
    }
    if !SHELLS.contains(&base(prog)) {
        return None;
    }
    let opts = shell::shell_args(prog, rest);
    // `bash -n <file>` only parses it: nothing runs. The museum check had
    // 4 `bash -n … contrib/gate` rows that R1.1 claimed for gate.
    if opts.n {
        return None;
    }
    let (file, args) = opts.rest.split_first()?;
    Some((Some(file), args))
}

/// R5 (fix-hook-strict): a `match` rule's head, which names no program
/// or subcommand words: every word after the program word must be on the
/// program's list (with no subcommand words named, so `npm` takes none),
/// and an unlisted program takes no words at all.
pub fn words_allowed(head: &[String]) -> bool {
    let Some((prog, rest)) = head.split_first() else {
        return false;
    };
    if allow::listed(prog) {
        allow::allowed(prog, rest, 0)
    } else {
        rest.is_empty()
    }
}

mod allow;

#[cfg(test)]
mod tests;
