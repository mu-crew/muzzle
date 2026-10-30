//! R5 (fix-hook-strict): the words `hook claude` lets follow a program
//! word it allows. Every word of the head must be on the program's list:
//! options before the subcommand, the subcommand, its options and their
//! values, positionals, and the words after `--`. Nothing is stepped over
//! for being unknown, so an option a tool grows later (nextest's
//! `--config-file`, `--tool-config-file`) is left to the prompt until it
//! is listed here. The lists are only what the shipped rules and the
//! commit gate use; a missing entry costs a prompt, never an allow.

/// True when every word of `rest` (the head past its program word) is on
/// `prog`'s list, `subs` being how many subcommand words the matched
/// pattern has (the caller checked they match). A subcommand without a
/// list (`cargo bench`, `npm ci`), like a program without one, takes the
/// pattern's words and nothing else.
pub(super) fn allowed(prog: &str, rest: &[String], subs: usize) -> bool {
    match prog {
        "cargo" => cargo(rest, subs),
        "npm" => npm(rest, subs),
        "vitest" => vitest(rest),
        _ => false,
    }
}

/// True for the programs [`allowed`] has a list for.
pub(super) fn listed(prog: &str) -> bool {
    matches!(prog, "cargo" | "npm" | "vitest")
}

fn cargo(rest: &[String], subs: usize) -> bool {
    let at = rest
        .iter()
        .position(|w| !w.starts_with(['-', '+']))
        .unwrap_or(rest.len());
    if !rest[..at].iter().all(|w| cargo_global(w)) {
        return false;
    }
    // A bare `cargo` (or only global options) prints help.
    let Some((sub, mut rest)) = rest[at..].split_first() else {
        return true;
    };
    let spec = match sub.as_str() {
        "test" => &TEST,
        "build" | "check" => &BUILD,
        "clippy" => &CLIPPY,
        "fmt" => &FMT,
        "nextest" => match rest.split_first() {
            None => return true,
            Some((run, tail)) if run == "run" => {
                rest = tail;
                &NEXTEST
            }
            Some(_) => return false,
        },
        // Any other subcommand: only when the pattern names it, and then
        // nothing after it. Under a bare `cargo` pattern, `cargo run` or a
        // plugin (`cargo xtask`) would run code the rule didn't name.
        _ => return subs > 0 && rest.is_empty(),
    };
    spec.check(rest)
}

/// Before the subcommand. `+toolchain` only as a name: rustup takes
/// anything with a `/` as a path (`+/tmp/tc` runs `/tmp/tc/bin/cargo`).
fn cargo_global(w: &str) -> bool {
    if let Some(tc) = w.strip_prefix('+') {
        return !tc.is_empty()
            && tc
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c));
    }
    GLOBAL.check(std::slice::from_ref(&w.to_string()))
}

/// npm hands every word after the script name to the script, where its
/// program reads them as its own options (`npm test -- --import data:…`
/// runs code under `node --test`), so `npm test` and `npm run <script>`
/// take no words after them. npm reads any unambiguous abbreviation
/// (`--prefi=` is `--prefix`) and has options that run code
/// (`--script-shell`, `--node-options`), so before the subcommand only
/// output options pass.
fn npm(rest: &[String], subs: usize) -> bool {
    let at = rest
        .iter()
        .position(|w| !w.starts_with('-'))
        .unwrap_or(rest.len());
    let quiet = |w: &String| matches!(w.as_str(), "--silent" | "--quiet") || short_flags(w, "sqd");
    if !rest[..at].iter().all(quiet) {
        return false;
    }
    // Exactly the pattern's words (`test`, `run test:fast`): a bare `npm`
    // pattern names no script, so it allows none.
    subs > 0 && rest.len() - at == subs
}

/// vitest reads `--config`/`-c`, `--root`/`-r`, `--dir`, `--workspace`
/// and custom `--reporter`/`--environment` modules, all JS from elsewhere;
/// these only change how the run reports.
fn vitest(rest: &[String]) -> bool {
    let rest = match rest.split_first() {
        Some((run, tail)) if run == "run" => tail,
        _ => rest,
    };
    rest.iter().all(|w| {
        matches!(
            w.as_str(),
            "--run" | "--silent" | "--passWithNoTests" | "--no-color"
        )
    })
}

/// `-x`, `-xy`: a cluster of only `flags`.
fn short_flags(w: &str, flags: &str) -> bool {
    w.strip_prefix('-')
        .is_some_and(|o| !o.is_empty() && o.chars().all(|c| flags.contains(c)))
}

/// An option: its long name, its short letter, and what its value may be
/// (`None`: it takes none).
struct Opt(&'static str, Option<char>, Option<fn(&str) -> bool>);

/// A subcommand's words: options from `opts`, positionals that pass
/// `positional`, and what may follow `--`.
struct Spec {
    opts: &'static [&'static [Opt]],
    positional: fn(&str) -> bool,
    after_dashes: fn(&[String]) -> bool,
}

impl Spec {
    fn find(&self, f: impl Fn(&Opt) -> bool) -> Option<&Opt> {
        self.opts.iter().flat_map(|o| o.iter()).find(|o| f(o))
    }

    /// clap reads cargo's and nextest's options: no abbreviations, a value
    /// as `--long=v`, `--long v`, `-xv` or `-x v`, and short flags that
    /// cluster up to the first letter taking a value, which owns the rest
    /// of the word (`-pCore` is a package, not `-C`).
    fn check(&self, mut words: &[String]) -> bool {
        while let Some((w, tail)) = words.split_first() {
            words = tail;
            if w == "--" {
                return (self.after_dashes)(words);
            }
            let (opt, inline) = if let Some(long) = w.strip_prefix("--") {
                let (name, v) = match long.split_once('=') {
                    Some((n, v)) => (n, Some(v)),
                    None => (long, None),
                };
                match self.find(|o| !o.0.is_empty() && o.0 == name) {
                    Some(o) => (o, v),
                    None => return false,
                }
            } else if let Some(cluster) = w.strip_prefix('-').filter(|c| !c.is_empty()) {
                let mut found = None;
                for (i, c) in cluster.char_indices() {
                    let Some(o) = self.find(|o| o.1 == Some(c)) else {
                        return false;
                    };
                    if o.2.is_some() {
                        let v = &cluster[i + c.len_utf8()..];
                        found = Some((o, (!v.is_empty()).then_some(v)));
                        break;
                    }
                }
                match found {
                    Some(f) => f,
                    None => continue,
                }
            } else if (self.positional)(w) {
                continue;
            } else {
                return false;
            };
            match (opt.2, inline) {
                (None, None) => {}
                (None, Some(_)) => return false,
                (Some(ok), Some(v)) if ok(v) => {}
                (Some(_), Some(_)) => return false,
                (Some(ok), None) => match words.split_first() {
                    Some((v, t)) if ok(v) => words = t,
                    _ => return false,
                },
            }
        }
        true
    }
}

/// Starts with a letter, digit or `_` (so never an option) and has only
/// those and `more`.
fn word_of(w: &str, more: &str) -> bool {
    w.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || more.contains(c))
}

/// Target, test, bin, example and profile names.
fn name(w: &str) -> bool {
    word_of(w, "-")
}
/// A package spec by name (`core`, `core@1.2`), never a path or URL.
fn package(w: &str) -> bool {
    word_of(w, "-.@:")
}
/// Features: `-F a,b`, `-F 'a b'`, `-F dep/feat` (a dependency's feature,
/// not a path).
fn feature(w: &str) -> bool {
    word_of(w, "-/, ")
}
fn number(w: &str) -> bool {
    !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit())
}
/// A target triple by name. rustc reads a `--target` ending in `.json` or
/// holding a `/` as a target spec file, which can name a linker.
fn triple(w: &str) -> bool {
    word_of(w, "-")
}
fn color(w: &str) -> bool {
    matches!(w, "auto" | "always" | "never")
}
fn format(w: &str) -> bool {
    word_of(w, "-,")
}
/// A test-name filter. libtest matches it as a substring of test names
/// and reads nothing from it, so any word that isn't an option passes
/// (`cargo test -- 'a&&b'`). A `harness = false` binary may read it
/// otherwise, but that binary is the workspace's own code, which the
/// allow already runs.
fn filter(w: &str) -> bool {
    !w.is_empty() && !w.starts_with('-')
}
fn none(_: &str) -> bool {
    false
}
fn nothing(words: &[String]) -> bool {
    words.is_empty()
}

const GLOBAL: Spec = Spec {
    opts: &[&[
        Opt("locked", None, None),
        Opt("frozen", None, None),
        Opt("offline", None, None),
        Opt("quiet", Some('q'), None),
        Opt("verbose", Some('v'), None),
        Opt("color", None, Some(color)),
    ]],
    positional: none,
    after_dashes: nothing,
};

/// What build, check, test, clippy and nextest share: package and target
/// selection, features, profile, jobs and output. Not `--manifest-path`,
/// `--config`, `-Z`, `--target-dir` or `--target <file>`.
const COMMON: &[Opt] = &[
    Opt("package", Some('p'), Some(package)),
    Opt("workspace", None, None),
    Opt("all", None, None),
    Opt("exclude", None, Some(package)),
    Opt("lib", None, None),
    Opt("bins", None, None),
    Opt("bin", None, Some(name)),
    Opt("examples", None, None),
    Opt("example", None, Some(name)),
    Opt("tests", None, None),
    Opt("test", None, Some(name)),
    Opt("benches", None, None),
    Opt("bench", None, Some(name)),
    Opt("all-targets", None, None),
    Opt("features", Some('F'), Some(feature)),
    Opt("all-features", None, None),
    Opt("no-default-features", None, None),
    Opt("release", Some('r'), None),
    Opt("profile", None, Some(name)),
    Opt("jobs", Some('j'), Some(number)),
    Opt("keep-going", None, None),
    Opt("target", None, Some(triple)),
    Opt("locked", None, None),
    Opt("frozen", None, None),
    Opt("offline", None, None),
    Opt("quiet", Some('q'), None),
    Opt("verbose", Some('v'), None),
    Opt("color", None, Some(color)),
    Opt("message-format", None, Some(format)),
];

const BUILD: Spec = Spec {
    opts: &[COMMON],
    positional: none,
    after_dashes: nothing,
};

/// `cargo test [filter] -- <libtest options>`: the words after `--` go to
/// the test binaries, so only libtest's own options and filters pass.
const TEST: Spec = Spec {
    opts: &[
        COMMON,
        &[
            Opt("no-run", None, None),
            Opt("no-fail-fast", None, None),
            Opt("doc", None, None),
        ],
    ],
    positional: filter,
    after_dashes: libtest,
};

/// nextest takes its options after `run` and filters before or after
/// `--`. Its `--profile` names a profile in the repo's own nextest config;
/// `--config-file`, `--tool-config-file` and `--archive-file` load setup
/// scripts or binaries from elsewhere and are not listed.
const NEXTEST: Spec = Spec {
    opts: &[
        COMMON,
        &[
            Opt("no-fail-fast", None, None),
            Opt("no-capture", None, None),
            Opt("test-threads", None, Some(number)),
            Opt("retries", None, Some(number)),
            Opt("run-ignored", None, Some(name)),
            Opt("cargo-profile", None, Some(name)),
        ],
    ],
    positional: filter,
    after_dashes: libtest,
};

const CLIPPY: Spec = Spec {
    opts: &[COMMON, &[Opt("no-deps", None, None)]],
    positional: none,
    after_dashes: lint_levels,
};

/// `cargo fmt [-- <rustfmt options>]`. rustfmt's file arguments would
/// rewrite files anywhere, and `--config-path` reads another config.
const FMT: Spec = Spec {
    opts: &[&[
        Opt("all", None, None),
        Opt("check", None, None),
        Opt("package", Some('p'), Some(package)),
        Opt("quiet", Some('q'), None),
        Opt("verbose", Some('v'), None),
        Opt("message-format", None, Some(format)),
    ]],
    positional: none,
    after_dashes: rustfmt,
};

fn libtest(words: &[String]) -> bool {
    const LIBTEST: Spec = Spec {
        opts: &[&[
            Opt("nocapture", None, None),
            Opt("no-capture", None, None),
            Opt("show-output", None, None),
            Opt("ignored", None, None),
            Opt("include-ignored", None, None),
            Opt("exact", None, None),
            Opt("quiet", Some('q'), None),
            Opt("list", None, None),
            Opt("test", None, None),
            Opt("bench", None, None),
            Opt("test-threads", None, Some(number)),
            Opt("skip", None, Some(filter)),
            Opt("color", None, Some(color)),
            Opt("", Some('Z'), Some(|v| v == "unstable-options")),
        ]],
        positional: filter,
        after_dashes: nothing,
    };
    LIBTEST.check(words)
}

fn rustfmt(words: &[String]) -> bool {
    const RUSTFMT: Spec = Spec {
        opts: &[&[
            Opt("check", None, None),
            Opt("edition", None, Some(number)),
            Opt("color", None, Some(color)),
            Opt("quiet", Some('q'), None),
            Opt("verbose", Some('v'), None),
        ]],
        positional: none,
        after_dashes: nothing,
    };
    RUSTFMT.check(words)
}

/// After clippy's `--` the words go to clippy-driver, which is rustc:
/// `-Zcodegen-backend=/x.so`, `-C linker=`, `--extern` and `-L` load code
/// from elsewhere. Only lint levels pass: `-D warnings`, `-Wclippy::x`,
/// `--deny=x`.
fn lint_levels(mut rest: &[String]) -> bool {
    let lint = |w: &str| word_of(w, "-:");
    while let Some((w, tail)) = rest.split_first() {
        let level = ["-D", "-W", "-A", "-F"]
            .iter()
            .chain(&["--deny", "--warn", "--allow", "--forbid"])
            .find_map(|o| Some((o, w.strip_prefix(o)?)));
        rest = match level {
            Some((_, "")) => match tail.split_first() {
                Some((l, t)) if lint(l) => t,
                _ => return false,
            },
            Some((o, v)) if !o.starts_with("--") && lint(v) => tail,
            Some((o, v)) if o.starts_with("--") && v.strip_prefix('=').is_some_and(lint) => tail,
            _ => return false,
        };
    }
    true
}
