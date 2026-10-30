//! A light shell tokeniser. It started as D16a's container detection
//! ([`text_enters`]); since 0.2.1 it also drives `command` rule matching
//! (`heads.rs`, C2), the hook's command word ([`command_word`], R5) and
//! D22's [`runs_muzzle`]. It finds the words in command position: after
//! `; & | && || ( ) {` and newlines, inside `$( )`, backticks and `sh -c
//! '…'` scripts, and past wrappers such as `env`, `timeout -s KILL 600`,
//! `xargs` and `flatpak-spawn --host`. Quoted text, heredoc bodies,
//! comments and arguments are not command position, so `echo "x; podman
//! exec c"` and `git log -- distrobox enter` don't count. This isn't a
//! shell parser: it errs towards finding an entry, because a miss leaves
//! a container job unheld after a kill, which is the worse error.

/// Split `text` into simple commands, each a list of unquoted words.
/// Command substitutions come out as commands of their own.
pub fn commands(text: &str) -> Vec<Vec<String>> {
    lex(text).out
}

/// [`commands`] with the byte offset in `text` where each word starts
/// (its first character, which may be a quote).
pub fn command_spans(text: &str) -> Vec<Vec<(String, usize)>> {
    let lx = lex(text);
    // The lexer counts chars; callers slice `text` by bytes.
    let bytes: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    lx.out
        .into_iter()
        .zip(lx.starts)
        .map(|(ws, ss)| {
            ws.into_iter()
                .zip(
                    ss.into_iter()
                        .map(|c| bytes.get(c).copied().unwrap_or(text.len())),
                )
                .collect()
        })
        .collect()
}

fn lex(text: &str) -> Lexer {
    let mut lx = Lexer {
        chars: text.chars().collect(),
        at: 0,
        out: Vec::new(),
        starts: Vec::new(),
        words: Vec::new(),
        word_starts: Vec::new(),
        heredocs: Vec::new(),
    };
    lx.run();
    lx
}

struct Lexer {
    chars: Vec<char>,
    at: usize,
    out: Vec<Vec<String>>,
    /// Char index in the text where each word of `out` starts.
    starts: Vec<Vec<usize>>,
    words: Vec<String>,
    word_starts: Vec<usize>,
    /// Heredoc delimiters waiting for the next newline, with `<<-`.
    heredocs: Vec<(String, bool)>,
}

impl Lexer {
    fn peek(&self, off: usize) -> Option<char> {
        self.chars.get(self.at + off).copied()
    }

    fn end_command(&mut self) {
        if !self.words.is_empty() {
            self.out.push(std::mem::take(&mut self.words));
            self.starts.push(std::mem::take(&mut self.word_starts));
        }
    }

    fn run(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' => self.at += 1,
                '\n' => {
                    self.at += 1;
                    self.end_command();
                    self.skip_heredocs();
                }
                ';' | '&' | '|' | '(' | ')' => {
                    self.at += 1;
                    self.end_command();
                }
                '#' => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.at += 1;
                    }
                }
                '<' | '>' => self.redirect(),
                _ => {
                    let start = self.at;
                    let w = self.word();
                    // `2>file`: a word of digits glued to a redirect is its fd.
                    if matches!(self.peek(0), Some('<' | '>'))
                        && w.bytes().all(|b| b.is_ascii_digit())
                    {
                        self.redirect();
                    } else if !w.is_empty() {
                        // Empty after a substitution was lifted out; an
                        // empty `''` word never names a command either.
                        self.words.push(w);
                        self.word_starts.push(start);
                    }
                }
            }
        }
        self.end_command();
    }

    /// `<`, `>`, `>>`, `>&2`, `&>`-style redirects drop their target;
    /// `<<WORD` queues a heredoc; `<<<` takes a word, also dropped.
    fn redirect(&mut self) {
        if self.peek(0) == Some('<') && self.peek(1) == Some('<') && self.peek(2) != Some('<') {
            self.at += 2;
            let strip = self.peek(0) == Some('-');
            if strip {
                self.at += 1;
            }
            self.blanks();
            let delim = self.word();
            if !delim.is_empty() {
                self.heredocs.push((delim, strip));
            }
            return;
        }
        while matches!(self.peek(0), Some('<' | '>' | '|')) {
            self.at += 1;
        }
        if self.peek(0) == Some('&') {
            // `>&2`, `>&-`: the target is glued on.
            self.at += 1;
            while self.peek(0).is_some_and(|c| c.is_ascii_digit() || c == '-') {
                self.at += 1;
            }
            return;
        }
        self.blanks();
        self.word();
    }

    fn blanks(&mut self) {
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.at += 1;
        }
    }

    /// After a newline, skip the bodies of any queued heredocs.
    fn skip_heredocs(&mut self) {
        for (delim, strip) in std::mem::take(&mut self.heredocs) {
            loop {
                let start = self.at;
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.at += 1;
                }
                let line: String = self.chars[start..self.at].iter().collect();
                let at_end = self.peek(0).is_none();
                self.at = (self.at + 1).min(self.chars.len());
                let line = if strip {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                if line == delim || at_end {
                    break;
                }
            }
        }
    }

    /// One word with quotes removed. Command substitutions inside it are
    /// lexed as commands of their own.
    fn word(&mut self) -> String {
        let mut w = String::new();
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => break,
                '\\' => {
                    match self.peek(1) {
                        Some('\n') => {}
                        Some(n) => w.push(n),
                        None => {}
                    }
                    self.at += 2;
                }
                '\'' => {
                    self.at += 1;
                    while let Some(c) = self.peek(0) {
                        self.at += 1;
                        if c == '\'' {
                            break;
                        }
                        w.push(c);
                    }
                }
                '"' => {
                    self.at += 1;
                    while let Some(c) = self.peek(0) {
                        match c {
                            '"' => {
                                self.at += 1;
                                break;
                            }
                            '\\' if matches!(self.peek(1), Some('"' | '\\' | '$' | '`')) => {
                                w.push(self.peek(1).unwrap_or_default());
                                self.at += 2;
                            }
                            '$' if self.peek(1) == Some('(') => self.substitution(),
                            '`' => self.backticks(),
                            _ => {
                                w.push(c);
                                self.at += 1;
                            }
                        }
                    }
                }
                '$' if self.peek(1) == Some('(') => self.substitution(),
                // f-ansi-c: `$'…'` takes backslash escapes, so `\'` stays
                // inside. Other escapes keep their backslash: no command
                // word needs `\n` decoded.
                '$' if self.peek(1) == Some('\'') => {
                    self.at += 2;
                    while let Some(c) = self.peek(0) {
                        self.at += 1;
                        match c {
                            '\'' => break,
                            '\\' => {
                                match self.peek(0) {
                                    Some(e @ ('\'' | '\\')) => w.push(e),
                                    Some(e) => {
                                        w.push('\\');
                                        w.push(e);
                                    }
                                    None => {}
                                }
                                self.at += 1;
                            }
                            _ => w.push(c),
                        }
                    }
                }
                '`' => self.backticks(),
                _ => {
                    w.push(c);
                    self.at += 1;
                }
            }
        }
        w
    }

    /// `$( … )`, matched by depth, skipping quoted parentheses.
    fn substitution(&mut self) {
        self.at += 2;
        let start = self.at;
        let mut depth = 1;
        let mut quote = None;
        while let Some(c) = self.peek(0) {
            match (quote, c) {
                // `$'…'`, marked by `$`: only `'` closes it, `\` escapes.
                (None, '$') if self.peek(1) == Some('\'') => {
                    quote = Some('$');
                    self.at += 1;
                }
                (Some('$'), '\'') => quote = None,
                (Some('$'), '\\') => self.at += 1,
                (Some('$'), _) => {}
                (None, '\'' | '"') => quote = Some(c),
                (Some(q), _) if q == c => quote = None,
                (None, '\\') | (Some('"'), '\\') => self.at += 1,
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            self.at += 1;
        }
        let inner: String = self.chars[start..self.at.min(self.chars.len())]
            .iter()
            .collect();
        self.at += 1;
        self.lift(&inner, start);
    }

    fn backticks(&mut self) {
        self.at += 1;
        let start = self.at;
        while self.peek(0).is_some_and(|c| c != '`') {
            self.at += if self.peek(0) == Some('\\') { 2 } else { 1 };
        }
        let end = self.at.min(self.chars.len());
        let inner: String = self.chars[start..end].iter().collect();
        self.at += 1;
        self.lift(&inner, start);
    }

    /// Add the commands of a substitution whose text starts at `offset`.
    fn lift(&mut self, inner: &str, offset: usize) {
        let lx = lex(inner);
        self.out.extend(lx.out);
        self.starts.extend(
            lx.starts
                .into_iter()
                .map(|s| s.into_iter().map(|i| i + offset).collect()),
        );
    }
}

pub(super) fn base(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

/// Reserved words and openers that leave the next word in command position.
const KEYWORDS: [&str; 11] = [
    "!", "{", "}", "do", "then", "else", "elif", "if", "while", "until", "nohup",
];

/// Wrappers that run their operand as a command, with the short options
/// whose argument is the next word. Long options take `--opt=value`; a
/// wrapper whose long options take a separate argument lists them too.
struct Wrapper {
    name: &'static str,
    short_args: &'static str,
    long_args: &'static [&'static str],
    /// Positional words before the command (timeout's duration).
    positional: usize,
}

const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        name: "env",
        // `-a`/`--argv0` (GNU) and `-P` (BSD) take an argument too.
        short_args: "uCSaP",
        long_args: &["--unset", "--chdir", "--split-string", "--argv0"],
        positional: 0,
    },
    Wrapper {
        name: "nice",
        short_args: "n",
        long_args: &["--adjustment"],
        positional: 0,
    },
    Wrapper {
        name: "time",
        short_args: "fo",
        long_args: &["--format", "--output"],
        positional: 0,
    },
    Wrapper {
        name: "timeout",
        short_args: "sk",
        long_args: &["--signal", "--kill-after"],
        positional: 1,
    },
    Wrapper {
        name: "command",
        short_args: "",
        long_args: &[],
        positional: 0,
    },
    Wrapper {
        name: "exec",
        short_args: "a",
        long_args: &[],
        positional: 0,
    },
    Wrapper {
        name: "builtin",
        short_args: "",
        long_args: &[],
        positional: 0,
    },
    Wrapper {
        name: "sudo",
        short_args: "ugpCDhrtTUR",
        long_args: &["--user", "--group", "--prompt", "--chdir"],
        positional: 0,
    },
    Wrapper {
        name: "doas",
        short_args: "uC",
        long_args: &[],
        positional: 0,
    },
    Wrapper {
        name: "xargs",
        short_args: "aEeIiLlnPsd",
        long_args: &[
            "--arg-file",
            "--delimiter",
            "--max-args",
            "--max-procs",
            "--max-chars",
            "--replace",
            "--eof",
            "--max-lines",
            "--process-slot-var",
        ],
        positional: 0,
    },
    Wrapper {
        name: "flatpak-spawn",
        short_args: "",
        long_args: &[],
        positional: 0,
    },
    Wrapper {
        name: "systemd-inhibit",
        short_args: "",
        long_args: &["--what", "--who", "--why", "--mode"],
        positional: 0,
    },
    Wrapper {
        name: "systemd-run",
        short_args: "pEu",
        long_args: &[
            "--unit",
            "--property",
            "--setenv",
            "--uid",
            "--gid",
            "--working-directory",
            "--slice",
            "--description",
        ],
        positional: 0,
    },
    Wrapper {
        name: "setsid",
        short_args: "",
        long_args: &[],
        positional: 0,
    },
    Wrapper {
        name: "stdbuf",
        short_args: "ioe",
        long_args: &[],
        positional: 0,
    },
    Wrapper {
        name: "ionice",
        short_args: "cnp",
        long_args: &["--class", "--classdata", "--pid"],
        positional: 0,
    },
    Wrapper {
        name: "chrt",
        short_args: "",
        long_args: &[],
        positional: 1,
    },
    Wrapper {
        name: "taskset",
        short_args: "",
        long_args: &[],
        positional: 1,
    },
    Wrapper {
        name: "unbuffer",
        short_args: "",
        long_args: &[],
        positional: 0,
    },
];

/// The words of one simple command from where the real command starts:
/// past assignments, keywords and wrappers.
pub(super) fn strip_prefix(mut words: &[String]) -> &[String] {
    loop {
        let Some(first) = words.first() else {
            return words;
        };
        let w = first.as_str();
        if KEYWORDS.contains(&w) || is_assignment(w) {
            words = &words[1..];
            continue;
        }
        let Some(wr) = WRAPPERS.iter().find(|wr| wr.name == base(w)) else {
            return words;
        };
        words = strip_options(wr, base(w) == "env", &words[1..]);
    }
}

/// The words after wrapper `wr`'s name, past its options, `--`, its
/// positional words and (for `env`) assignments.
fn strip_options<'a>(wr: &Wrapper, env: bool, mut words: &'a [String]) -> &'a [String] {
    let mut positional = wr.positional;
    while let Some(next) = words.first() {
        let n = next.as_str();
        if n == "--" {
            words = &words[1..];
            break;
        }
        if let Some(long) = n.strip_prefix("--") {
            let takes = !long.contains('=') && wr.long_args.contains(&n);
            words = &words[if takes { 2 } else { 1 }.min(words.len())..];
        } else if n.len() > 1 && n.starts_with('-') {
            // `-s KILL`, but `-sKILL` carries its own argument.
            let opts = &n[1..];
            let takes = opts
                .char_indices()
                .find(|&(_, c)| wr.short_args.contains(c))
                .is_some_and(|(i, c)| i + c.len_utf8() == opts.len());
            words = &words[if takes { 2 } else { 1 }.min(words.len())..];
        } else if env && is_assignment(n) {
            words = &words[1..];
        } else if positional > 0 {
            positional -= 1;
            words = &words[1..];
        } else {
            break;
        }
    }
    words
}

/// The options `hook claude` steps over for one prefix word: an
/// allowlist, because here a wrong step approves a call unprompted. An
/// option not listed, in any spelling (GNU's abbreviations `--ch=`, `--sp=`
/// included), stops the allow: `env -S`/`--split-string` runs a command
/// hidden in its argument, `env -C`/`--chdir` moves what a relative word
/// resolves to, `env -a`/`--argv0` makes the next word an argument, and
/// BSD `env -P` swaps the PATH that finds the command (R5).
struct SafeOpts {
    name: &'static str,
    /// Short options that take no argument; they may cluster.
    flags: &'static str,
    /// Short options whose argument is the rest of the word or the next.
    args: &'static str,
    /// Long options (without `--`) that take no argument.
    long_flags: &'static [&'static str],
    /// Long options whose argument is `=value` or the next word.
    long_args: &'static [&'static str],
    /// Operands before the command (timeout's duration).
    positional: usize,
    /// `nice -5`: an adjustment written as the option.
    numeric: bool,
    /// env: assignments, and `-` meaning `-i`.
    env: bool,
}

/// The prefixes `hook claude` steps over before the command word (spec
/// §Integrations "Claude Code and Codex"): a narrower list than
/// [`WRAPPERS`], with only their harmless options.
const ALLOW_PREFIXES: [SafeOpts; 4] = [
    SafeOpts {
        name: "env",
        flags: "iv0",
        args: "u",
        long_flags: &["ignore-environment", "debug", "null"],
        long_args: &["unset"],
        positional: 0,
        numeric: false,
        env: true,
    },
    SafeOpts {
        name: "time",
        // Not `-o`/`--output`, `-a`: they write to a file the user didn't
        // approve.
        flags: "pvq",
        args: "f",
        long_flags: &["portability", "verbose", "quiet"],
        long_args: &["format"],
        positional: 0,
        numeric: false,
        env: false,
    },
    SafeOpts {
        name: "timeout",
        flags: "v",
        args: "sk",
        long_flags: &["preserve-status", "foreground", "verbose"],
        long_args: &["signal", "kill-after"],
        positional: 1,
        numeric: false,
        env: false,
    },
    SafeOpts {
        name: "nice",
        flags: "",
        args: "n",
        long_flags: &[],
        long_args: &["adjustment"],
        positional: 0,
        numeric: true,
        env: false,
    },
];

/// How many of `words` (after the prefix's name) are its options and
/// operands, or `None` at an option not in `o`. Options end at the first
/// operand, as with getopt's `+` mode these tools use: `env A=1 -u X`
/// runs `-u`.
fn skip_safe_opts(o: &SafeOpts, words: &[String]) -> Option<usize> {
    let mut at = 0;
    while let Some(w) = words.get(at).map(String::as_str) {
        if w == "--" {
            at += 1;
            break;
        }
        // GNU env reads a lone `-` as `-i` after its options.
        if o.env && w == "-" {
            at += 1;
            break;
        }
        let Some(opt) = w.strip_prefix('-').filter(|s| !s.is_empty()) else {
            break;
        };
        at += match opt.strip_prefix('-') {
            Some(long) => match long.split_once('=') {
                Some((name, _)) if o.long_args.contains(&name) => 1,
                None if o.long_flags.contains(&long) => 1,
                None if o.long_args.contains(&long) => 2,
                _ => return None,
            },
            None if o.numeric && opt.bytes().all(|b| b.is_ascii_digit()) => 1,
            None => short_cluster(opt, o.flags, o.args)?,
        };
    }
    at += o.positional;
    while o.env && words.get(at).is_some_and(|w| is_assignment(w)) {
        at += 1;
    }
    (at <= words.len()).then_some(at)
}

/// Words taken by short-option cluster `opt` (past its `-`): flags until
/// the first option that takes an argument, which owns the rest of the
/// word (`-uCARGO_HOME`) or, if none is left, the next word. `None` at a
/// letter in neither set.
fn short_cluster(opt: &str, flags: &str, args: &str) -> Option<usize> {
    for (i, c) in opt.char_indices() {
        if args.contains(c) {
            return Some(if i + c.len_utf8() == opt.len() { 2 } else { 1 });
        }
        if !flags.contains(c) {
            return None;
        }
    }
    Some(1)
}

/// Where an [`ALLOW_PREFIXES`] word may live when written as a path
/// (`/usr/bin/time -v cargo clippy`).
const TRUSTED_DIRS: [&str; 2] = ["/usr/bin/", "/bin/"];

/// The byte offset in `text` of the command word, when `text` lexes to one
/// simple command: past assignments and leading [`ALLOW_PREFIXES`] with
/// their allowed options. `None` for anything else, including a prefix
/// option outside the allowlist and a prefix at a path outside
/// [`TRUSTED_DIRS`].
pub fn command_word(text: &str) -> Option<usize> {
    let mut spans = command_spans(text);
    if spans.len() != 1 {
        return None;
    }
    let spans = spans.remove(0);
    let words: Vec<String> = spans.iter().map(|(w, _)| w.clone()).collect();
    let mut at = 0;
    loop {
        let w = words.get(at)?.as_str();
        if is_assignment(w) {
            at += 1;
            continue;
        }
        let Some(o) = ALLOW_PREFIXES.iter().find(|o| o.name == base(w)) else {
            break;
        };
        // R5 (2026-10-30 amendment): a prefix counts by name or from the
        // system directories only; `/tmp/evil/env` or `./time` is some
        // other program, which could run anything.
        if base(w) != w
            && !TRUSTED_DIRS
                .iter()
                .any(|d| w.strip_prefix(d) == Some(base(w)))
        {
            return None;
        }
        at += 1 + skip_safe_opts(o, &words[at + 1..])?;
    }
    // A shell, another wrapper (`exec`, `sudo`, `xargs`) or a `*.sh`
    // script as the command word runs some other program, yet an anchored
    // pattern's own prefix group can match from it (`bash -x evil.sh cargo
    // test`, `evil.sh cargo build` under bench/linuxpc-config.toml).
    let w = base(&words[at]);
    if WRAPPERS.iter().any(|wr| wr.name == w) || SHELLS.contains(&w) || w.ends_with(".sh") {
        return None;
    }
    Some(spans[at].1)
}

/// D22: `text` already runs muzzle: its first simple command, past
/// assignments and wrappers (`env A=1`, `timeout 60`), has a command word
/// whose basename is `muzzle`. Covers `muzzle run …`, an absolute path
/// (`'/x/bin/muzzle' run …`, what the pi extension writes) and `./muzzle`,
/// so a second hook pass or a second loaded extension leaves it alone
/// instead of nesting a run inside a run.
pub fn runs_muzzle(text: &str) -> bool {
    // Substitutions lex as commands of their own, ahead of their parent:
    // the first command in the text is the one that starts first.
    let Some(first) = command_spans(text)
        .into_iter()
        .filter(|c| !c.is_empty())
        .min_by_key(|c| c[0].1)
    else {
        return false;
    };
    let words: Vec<String> = first.into_iter().map(|(w, _)| w).collect();
    strip_prefix(&words)
        .first()
        .is_some_and(|w| base(w) == "muzzle")
}

pub(super) const SHELLS: [&str; 6] = ["bash", "sh", "dash", "zsh", "ksh", "ash"];

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// A shell's words after its name, read the way bash reads its options.
pub(super) struct ShellArgs<'a> {
    /// `-c` was given: the first of `rest` is the script.
    pub c: bool,
    /// `-n` was given: nothing runs.
    pub n: bool,
    /// The words past the options and their arguments.
    pub rest: &'a [String],
}

/// bash's long options, which it takes after one dash or two
/// (`-noprofile`, `--login`). Matched as whole words before the cluster
/// branch: read as a cluster, the `o` in `-login` would take `-c` as its
/// argument and lose the script (D16a container detection missed `bash
/// -login -c 'podman exec …'`).
const BASH_LONG_FLAGS: [&str; 14] = [
    "debug",
    "debugger",
    "dump-po-strings",
    "dump-strings",
    "help",
    "login",
    "noediting",
    "noprofile",
    "norc",
    "posix",
    "pretty-print",
    "restricted",
    "verbose",
    "version",
];

/// The [`BASH_LONG_FLAGS`] counterparts that take the next word.
const BASH_LONG_ARGS: [&str; 2] = ["rcfile", "init-file"];

/// f-shell-opts: `-o`/`+o` take the next word as their argument, and so
/// do `-O`/`+O` in bash (`sh` too: it is bash on Linux and macOS, and
/// dash rejects `-O`); in zsh `-O` is the CORRECT_ALL flag. In a cluster
/// each such letter takes one word in turn, so `bash -co pipefail 'x'`
/// runs `x`. Long options ([`BASH_LONG_FLAGS`], [`BASH_LONG_ARGS`]) match
/// whole words first. zsh (and the other non-bash shells) take an option
/// name glued on, `-opipefail`, `-xonomatch`: the rest of the word after
/// `o` is its argument, not more flags, and the word consumes only itself
/// (zsh 5.9 runs `zsh -f -opipefail -c x`; bash rejects `-opipefail`, so
/// bash keeps taking the next word). `--` or `-` ends options. One reader
/// for both the `-c` script (rules and D16a container detection) and R1.1's script
/// file, so the two can't drift again.
pub(super) fn shell_args<'a>(shell: &str, words: &'a [String]) -> ShellArgs<'a> {
    let arg_letters: &[char] = match base(shell) {
        "bash" | "sh" => &['o', 'O'],
        _ => &['o'],
    };
    let (mut c, mut n, mut at) = (false, false, 0);
    while let Some(w) = words.get(at) {
        let w = w.as_str();
        let long = w.strip_prefix("--").or_else(|| w.strip_prefix('-'));
        at += match w {
            "--" | "-" => {
                at += 1;
                break;
            }
            _ if long.is_some_and(|l| BASH_LONG_ARGS.contains(&l)) => 2,
            _ if long.is_some_and(|l| BASH_LONG_FLAGS.contains(&l)) => 1,
            _ if w.starts_with("--") => 1,
            _ if w.len() > 1 && w.starts_with(['-', '+']) => {
                let mut opts = &w[1..];
                let mut takes = None;
                if arg_letters == ['o']
                    && let Some(i) = opts.find('o')
                    && i + 1 < opts.len()
                {
                    // Review low_shell_args_gives_zsh_glued_o_name_a.
                    opts = &opts[..i];
                    takes = Some(1);
                }
                if w.starts_with('-') {
                    c |= opts.contains('c');
                    n |= opts.contains('n');
                }
                takes.unwrap_or_else(|| 1 + opts.matches(arg_letters).count())
            }
            _ => break,
        };
    }
    ShellArgs {
        c,
        n,
        rest: &words[at.min(words.len())..],
    }
}

/// The shell script of `bash -c '<script>'` (any `-…c…` flag), if `words`
/// is one.
pub(super) fn shell_script(words: &[String]) -> Option<&str> {
    let (sh, rest) = words.split_first()?;
    if !SHELLS.contains(&base(sh)) {
        return None;
    }
    let args = shell_args(sh, rest);
    args.c.then(|| args.rest.first().map(String::as_str))?
}

/// How each container tool's entry verb is spelled after its name. The
/// noun forms (`podman container exec`, `docker compose exec`) step over
/// the middle word.
fn tool_enters(words: &[String]) -> bool {
    let Some((tool, rest)) = words.split_first() else {
        return false;
    };
    let (verbs, middles): (&[&str], &[&str]) = match base(tool) {
        "distrobox-enter" => return true,
        "distrobox" => (&["enter"], &[]),
        "toolbox" => (&["run", "enter"], &[]),
        "podman" | "docker" => (&["exec"], &["container", "compose"]),
        "docker-compose" | "podman-compose" => (&["exec", "run"], &[]),
        _ => return false,
    };
    // Global options may sit in between (`docker -H x exec`, `docker
    // compose -f x.yml exec`): a few words, each an option, an option's
    // argument or a middle noun.
    let mut after_opt = false;
    for w in rest.iter().take(6) {
        let w = w.as_str();
        if verbs.contains(&w) {
            return true;
        }
        if w.starts_with('-') {
            after_opt = !w.contains('=');
        } else if middles.contains(&w) || after_opt {
            after_opt = false;
        } else {
            return false;
        }
    }
    false
}

/// One simple command enters a container, directly, through wrappers, or
/// in an `sh -c` script.
pub fn argv_enters(words: &[String]) -> bool {
    let words = strip_prefix(words);
    match shell_script(words) {
        Some(script) => text_enters(script),
        None => tool_enters(words),
    }
}

/// Any command in `text` enters a container.
pub fn text_enters(text: &str) -> bool {
    commands(text).iter().any(|c| argv_enters(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(ws: &[&str]) -> Vec<String> {
        ws.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn lexing_splits_on_operators_and_drops_quotes_redirects_and_heredocs() {
        assert_eq!(
            commands("a 'b c' \"d;e\" f\\ g; h && i | j 2>&1 >/dev/null k"),
            vec![
                words(&["a", "b c", "d;e", "f g"]),
                words(&["h"]),
                words(&["i"]),
                words(&["j", "k"])
            ]
        );
        assert_eq!(
            commands("cat <<'EOF' | sort\nx; y\nEOF\nz # c; w"),
            vec![words(&["cat"]), words(&["sort"]), words(&["z"])]
        );
        assert_eq!(
            commands("cat <<-E\n\tp\n\tE\nq"),
            vec![words(&["cat"]), words(&["q"])]
        );
        assert_eq!(
            commands("x=$(a b) `c`"),
            vec![words(&["a", "b"]), words(&["c"]), words(&["x="])]
        );
    }

    /// f-shell-opts: `-o`/`-O`/`+o`/`+O`/`--rcfile`/`--init-file` take the
    /// next word, so it isn't the `-c` script; clustered `-co` takes it too.
    #[test]
    fn shell_script_skips_option_arguments() {
        for (cmd, script) in [
            ("bash -c 'cargo test'", Some("cargo test")),
            ("bash -o pipefail -c 'cargo test'", Some("cargo test")),
            ("bash -O extglob -c 'cargo test'", Some("cargo test")),
            ("bash +o posix -c 'cargo test'", Some("cargo test")),
            ("bash -co pipefail 'cargo test'", Some("cargo test")),
            ("bash -oc pipefail 'cargo test'", Some("cargo test")),
            (
                "bash -oO pipefail extglob -c 'cargo test'",
                Some("cargo test"),
            ),
            ("bash --rcfile /r -c 'cargo test'", Some("cargo test")),
            ("bash --norc -ec 'cargo test'", Some("cargo test")),
            ("bash -c -- 'cargo test'", Some("cargo test")),
            ("bash -o pipefail x.sh", None),
            ("bash -co", None),
            // Single-dash long options are whole words, not clusters
            // whose `o` takes `-c` (review medium_shell_args_reads_single_
            // dash_long).
            ("bash -noprofile -c 'cargo test'", Some("cargo test")),
            ("bash -norc -c 'cargo test'", Some("cargo test")),
            ("bash -login -c 'cargo test'", Some("cargo test")),
            ("bash -posix -c 'cargo test'", Some("cargo test")),
            ("bash -verbose -c 'cargo test'", Some("cargo test")),
            ("bash --login -c 'cargo test'", Some("cargo test")),
            ("bash -rcfile /r -c 'cargo test'", Some("cargo test")),
            ("bash -init-file /r -c 'cargo test'", Some("cargo test")),
            ("bash --init-file /r -c 'cargo test'", Some("cargo test")),
            ("bash -rcfile -c 'cargo test'", None),
            // zsh `-O` is CORRECT_ALL, no argument; `-o` still takes one
            // (review low_shell_args_gives_zsh_o_an_argument).
            ("zsh -O -c 'cargo test'", Some("cargo test")),
            ("zsh -xO -c 'cargo test'", Some("cargo test")),
            ("zsh -o pipefail -c 'cargo test'", Some("cargo test")),
            // zsh takes a glued option name; bash takes the next word
            // (review low_shell_args_gives_zsh_glued_o_name_a).
            ("zsh -opipefail -c 'cargo test'", Some("cargo test")),
            ("zsh -xopipefail -c 'cargo test'", Some("cargo test")),
            ("zsh -onomatch -c 'cargo test'", Some("cargo test")),
            ("bash -opipefail -c 'cargo test'", None),
            ("sh -O extglob -c 'cargo test'", Some("cargo test")),
            ("/bin/bash -O extglob -c 'cargo test'", Some("cargo test")),
        ] {
            let c = commands(cmd).remove(0);
            assert_eq!(shell_script(&c), script, "{cmd}");
        }
    }

    /// f-ansi-c: `\'` inside `$'…'` doesn't close it.
    #[test]
    fn ansi_c_quotes_take_escaped_quotes() {
        assert_eq!(
            commands("echo $'it\\'s'; cargo test"),
            vec![words(&["echo", "it's"]), words(&["cargo", "test"])]
        );
        assert_eq!(
            commands("x=$(echo $'a\\'b)'); cargo test"),
            vec![
                words(&["echo", "a'b)"]),
                words(&["x="]),
                words(&["cargo", "test"])
            ]
        );
        assert_eq!(commands("echo \"$'x\""), vec![words(&["echo", "$'x"])]);
    }

    /// D16a: container detection reads the same script and quotes.
    #[test]
    fn container_entry_is_found_past_shell_options_and_ansi_c() {
        for cmd in [
            "bash -o pipefail -c 'podman exec x ls'",
            "bash -co pipefail 'podman exec x ls'",
            "bash -O extglob -c 'podman exec x ls'",
            "echo $'it\\'s'; podman exec x ls",
            "bash -noprofile -c 'podman exec x cargo test'",
            "bash -login -c 'podman exec box true'",
            "zsh -O -c 'podman exec x ls'",
            "zsh -opipefail -c 'podman exec x cargo test'",
        ] {
            assert!(text_enters(cmd), "{cmd}");
        }
        assert!(!text_enters("bash -co 'podman exec x ls' true"));
    }

    #[test]
    fn wrappers_are_stepped_over() {
        for (cmd, rest) in [
            ("timeout -s KILL 600 a b", "a b"),
            ("timeout --signal=KILL -k 5 10 a", "a"),
            ("env -u X A=1 a", "a"),
            ("nice -n 5 sudo -u me a", "a"),
            ("! command a", "a"),
            ("xargs -0 -n 1 a", "a"),
            ("flatpak-spawn --host a", "a"),
            ("systemd-inhibit --why x a", "a"),
            ("time -p a", "a"),
            // env -a/--argv0 (GNU) and -P (BSD) take an argument.
            ("env -a x a", "a"),
            ("env --argv0 x a", "a"),
            ("env -P /p a", "a"),
        ] {
            let c = commands(cmd).remove(0);
            assert_eq!(strip_prefix(&c).join(" "), rest, "{cmd}");
        }
    }

    /// 0.2.1 R5 (2026-10-30 amendment): a prefix counts only from a
    /// trusted place, and `env` may not change directory.
    #[test]
    fn command_word_trusts_only_system_prefixes_and_no_chdir() {
        for (cmd, word) in [
            ("cargo test", "cargo"),
            ("/usr/bin/time -v cargo clippy", "cargo"),
            ("/bin/env A=1 cargo test", "cargo"),
            ("A=1 env B=2 nice -n 5 timeout 9 cargo test", "cargo"),
            ("env -u X cargo test", "cargo"),
            // low_cluster_scan_blocks_pcore_ucargo: `-u` owns the rest.
            ("env -uCARGO_HOME cargo test", "cargo"),
            ("env -iuCARGO_HOME --unset=X - A=1 cargo test", "cargo"),
            ("timeout -s KILL --kill-after=5 600 cargo test", "cargo"),
            ("timeout -sKILL 600 cargo test", "cargo"),
            ("nice -5 time -p cargo test", "cargo"),
            ("env -- cargo test", "cargo"),
        ] {
            let at = command_word(cmd).unwrap_or_else(|| panic!("{cmd}"));
            assert!(cmd[at..].starts_with(word), "{cmd}");
        }
        for cmd in [
            "/tmp/evil/env cargo test",
            "./time cargo test",
            "/tmp/x/timeout 5 contrib/gate",
            "/usr/local/bin/nice cargo test",
            "env -C /e ./contrib/gate",
            "env -C/e cargo test",
            "env -iC /e cargo test",
            "env --chdir=/e contrib/gate",
            "env --chdir /e contrib/gate",
            "env -S 'x' cargo test",
            // high_env_long_opt_abbreviations_bypass_c: GNU abbreviations.
            "env --ch=/tmp/evil contrib/gate",
            "env --c=/e cargo test",
            "env --chd=/e ./contrib/gate",
            "env --sp='sh -c x' cargo test",
            "env --spl='rm -rf ~' cargo test",
            "/usr/bin/env --s=x cargo test",
            "env --unse=X cargo test",
            // high_env_a_argv0_and_p_spoof_the_command.
            "env -a vitest /bin/sh -c 'echo PWNED'",
            "env -avitest /bin/sh",
            "env --argv0 contrib/gate /bin/sh -c x",
            "env --argv0=cargo test",
            "env -P/tmp/evil cargo test",
            "env -P /tmp/evil cargo test",
            "env -iP/tmp/evil cargo test",
            // Unknown or file-writing options of the other prefixes.
            "time -o /home/u/.bashrc cargo test",
            "timeout --foo 5 cargo test",
            "nice --adj=5 cargo test",
        ] {
            assert_eq!(command_word(cmd), None, "{cmd}");
        }
    }

    #[test]
    fn runs_muzzle_reads_the_first_command_word() {
        for cmd in [
            "muzzle run -- cargo test",
            "  muzzle status",
            "/abs/path/muzzle run --rule=x -- bash -c 'cargo test'",
            "'/a b/muzzle' run --report '/t/r.json' --rule=gate -- bash -c 'x'",
            "./muzzle run -- cargo test",
            "env A=1 /x/muzzle run -- cargo test",
            "A=1 timeout 60 muzzle run -- cargo test",
            "muzzle run -- true; cargo test",
        ] {
            assert!(runs_muzzle(cmd), "{cmd:?}");
        }
        for cmd in [
            "cargo test",
            "muzzlefoo; cargo test",
            "/x/muzzle-dogfood run",
            "echo muzzle run",
            "cargo test; muzzle run -- true",
            "echo $(muzzle status) && cargo test",
            "",
        ] {
            assert!(!runs_muzzle(cmd), "{cmd:?}");
        }
    }
}
