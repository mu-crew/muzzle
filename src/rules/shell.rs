//! D16a: a light shell tokeniser for container detection. It finds the
//! words in command position: after `; & | && || ( ) {` and newlines,
//! inside `$( )`, backticks and `sh -c '…'` scripts, and past wrappers
//! such as `env`, `timeout -s KILL 600`, `xargs` and `flatpak-spawn
//! --host`. Quoted text, heredoc bodies, comments and arguments are not
//! command position, so `echo "x; podman exec c"` and `git log --
//! distrobox enter` don't count. This isn't a shell parser: it errs towards
//! finding an entry, because a miss leaves a container job unheld after a
//! kill, which is the worse error.

/// Split `text` into simple commands, each a list of unquoted words.
/// Command substitutions come out as commands of their own.
pub fn commands(text: &str) -> Vec<Vec<String>> {
    let mut lx = Lexer {
        chars: text.chars().collect(),
        at: 0,
        out: Vec::new(),
        words: Vec::new(),
        heredocs: Vec::new(),
    };
    lx.run();
    lx.out
}

struct Lexer {
    chars: Vec<char>,
    at: usize,
    out: Vec<Vec<String>>,
    words: Vec<String>,
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
        self.out.extend(commands(&inner));
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
        self.out.extend(commands(&inner));
    }
}

fn base(w: &str) -> &str {
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
fn strip_prefix(mut words: &[String]) -> &[String] {
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
        words = &words[1..];
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
            } else if base(w) == "env" && is_assignment(n) {
                words = &words[1..];
            } else if positional > 0 {
                positional -= 1;
                words = &words[1..];
            } else {
                break;
            }
        }
    }
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// The shell script of `bash -c '<script>'` (any `-…c…` flag), if `words`
/// is one.
fn shell_script(words: &[String]) -> Option<&str> {
    let (sh, rest) = words.split_first()?;
    if !matches!(base(sh), "bash" | "sh" | "dash" | "zsh" | "ksh" | "ash") {
        return None;
    }
    let mut c = false;
    for w in rest {
        if w.starts_with('-') && !w.starts_with("--") {
            c |= w.contains('c');
        } else if w.starts_with("--") {
            continue;
        } else {
            return c.then_some(w.as_str());
        }
    }
    None
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
}
