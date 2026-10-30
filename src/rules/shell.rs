//! A light shell tokeniser. It started as D16a's container detection
//! ([`text_enters`]); since 0.2.1 it also drives `command` rule matching
//! (`heads.rs`, C2), the hook's command word ([`command_word`], R5) and
//! D22's [`runs_muzzle`]. It finds the words in command position: after
//! `; & | && || ( ) {` and newlines, inside `$( )`, backticks and `sh -c
//! '…'` scripts, and past wrappers such as `env`, `timeout -s KILL 600`,
//! `xargs` and `flatpak-spawn --host`. Quoted text, heredoc bodies,
//! comments and arguments are not command position, so `echo "x; podman
//! exec c"` and `git log -- distrobox enter` don't count. This isn't a shell parser: it errs towards
//! finding an entry, because a miss leaves a container job unheld after a
//! kill, which is the worse error.

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
        short_args: "uCS",
        long_args: &["--unset", "--chdir", "--split-string"],
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

/// The prefixes `hook claude` steps over before the command word (spec
/// §Integrations "Claude Code and Codex"): a narrower list than
/// [`WRAPPERS`], because here a wrong step approves a call unprompted.
const ALLOW_PREFIXES: [&str; 4] = ["env", "time", "timeout", "nice"];

/// Where an [`ALLOW_PREFIXES`] word may live when written as a path
/// (`/usr/bin/time -v cargo clippy`).
const TRUSTED_DIRS: [&str; 2] = ["/usr/bin/", "/bin/"];

/// The byte offset in `text` of the command word, when `text` lexes to one
/// simple command: past assignments and leading `env`, `time`,
/// `timeout [opts] N` and `nice` with their options. `None` for anything
/// else, including `env -S`/`--split-string`, which would run a command
/// hidden in its argument, `env -C`/`--chdir`, and a prefix at a path
/// outside [`TRUSTED_DIRS`].
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
        if !ALLOW_PREFIXES.contains(&base(w)) {
            break;
        }
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
        // strip_prefix steps over exactly this wrapper and stops at the
        // next word (which the loop above then looks at).
        let rest = strip_one(&words[at..]);
        let next = words.len() - rest.len();
        // `env -S` runs a command hidden in its argument; `env -C`/`--chdir`
        // changes what a relative command word (`./contrib/gate`) and the
        // program's own config (`.cargo/config.toml`) resolve to (R5).
        if base(w) == "env"
            && words[at + 1..next].iter().any(|n| {
                ["--split-string", "--chdir"]
                    .iter()
                    .any(|o| n.starts_with(o))
                    || (n.starts_with('-') && !n.starts_with("--") && n.contains(['S', 'C']))
            })
        {
            return None;
        }
        at = next;
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

/// `words` past its first word, a known wrapper, and that wrapper's
/// options and positional words.
fn strip_one(words: &[String]) -> &[String] {
    let Some(wr) = words
        .first()
        .and_then(|w| WRAPPERS.iter().find(|wr| wr.name == base(w)))
    else {
        return words;
    };
    strip_options(wr, base(&words[0]) == "env", &words[1..])
}

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

/// f-shell-opts: `-o`/`-O`/`+o`/`+O`/`--rcfile`/`--init-file` take the
/// next word as their argument; in a cluster each `o`/`O` takes one in
/// turn, so `bash -co pipefail 'x'` runs `x`. `--` or `-` ends options.
/// One reader for both the `-c` script (rules and D16a container
/// detection) and R1.1's script file, so the two can't drift again.
pub(super) fn shell_args(words: &[String]) -> ShellArgs<'_> {
    let (mut c, mut n, mut at) = (false, false, 0);
    while let Some(w) = words.get(at) {
        let w = w.as_str();
        at += match w {
            "--" | "-" => {
                at += 1;
                break;
            }
            "--rcfile" | "--init-file" => 2,
            _ if w.starts_with("--") => 1,
            _ if w.len() > 1 && w.starts_with(['-', '+']) => {
                let opts = &w[1..];
                if w.starts_with('-') {
                    c |= opts.contains('c');
                    n |= opts.contains('n');
                }
                1 + opts.matches(['o', 'O']).count()
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
    let args = shell_args(rest);
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
