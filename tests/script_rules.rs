//! H1-H3 and `wrap --with-grace` (spec `2026-11-23-script-and-grace.md`) as
//! processes: heredocs a shell runs, local script files, python/node
//! bodies, and what the Claude hook does with a match it only inferred.

use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

const CONFIG: &str = r#"
[[rule]]
name = "cargo-test"
command = ["cargo test", "cargo build"]
slots = 4
[[rule]]
name = "git-rule"
command = "git gc"
"#;

struct Env {
    home: tempfile::TempDir,
    work: tempfile::TempDir,
}

fn env(config: &str) -> Env {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("muzzle");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), config).unwrap();
    Env {
        home,
        work: tempfile::tempdir().unwrap(),
    }
}

impl Env {
    fn cwd(&self) -> PathBuf {
        self.work.path().canonicalize().unwrap()
    }

    fn run(&self, args: &[&str], stdin: Option<&str>, extra: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_muzzle"));
        cmd.args(args)
            .env("XDG_CONFIG_HOME", self.home.path())
            .env("XDG_STATE_HOME", self.home.path().join("state"))
            .env_remove("MUZZLE_GRACE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let mut pipe = child.stdin.take().unwrap();
        if let Some(s) = stdin {
            pipe.write_all(s.as_bytes()).unwrap();
        }
        drop(pipe);
        child.wait_with_output().unwrap()
    }

    fn json(&self, args: &[&str], stdin: Option<&str>, extra: &[(&str, &str)]) -> Value {
        let out = self.run(args, stdin, extra);
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// The rule `match` names for `cmd` run in `dir`.
    fn rule_in(&self, dir: &Path, cmd: &str) -> Option<String> {
        let cwd = dir.to_str().unwrap();
        let v = self.json(&["match", "--json", "--cwd", cwd, "--", cmd], None, &[]);
        v["rule"].as_str().map(String::from)
    }

    fn rule(&self, cmd: &str) -> Option<String> {
        self.rule_in(&self.cwd(), cmd)
    }

    fn write(&self, name: &str, body: &str) -> PathBuf {
        let p = self.cwd().join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p
    }
}

fn is(r: Option<String>, name: &str) -> bool {
    r.as_deref() == Some(name)
}

#[test]
fn shell_heredoc_is_code_only_when_the_shell_reads_stdin() {
    let e = env(CONFIG);
    assert!(is(e.rule("bash <<'EOF'\ncargo test\nEOF"), "cargo-test"));
    assert!(is(e.rule("sh -s <<'EOF'\ncargo test\nEOF"), "cargo-test"));
    assert!(is(e.rule("bash - <<'EOF'\ncargo test\nEOF"), "cargo-test"));
    assert!(is(e.rule("bash <<<'cargo build'"), "cargo-test"));
    // Last stdin redirect wins; the earlier body is skipped.
    assert!(is(
        e.rule("bash <<'A' <<'B'\ncargo test\nA\ngit gc\nB"),
        "git-rule"
    ));
    assert!(is(
        e.rule("bash <<'A' <<'B'\ngit gc\nA\ncargo test\nB"),
        "cargo-test"
    ));
    // A heredoc on another fd is not stdin, and is skipped cleanly.
    assert_eq!(e.rule("bash 3<<'EOF'\ncargo test\nEOF"), None);
    assert!(is(
        e.rule("bash 0<<'EOF' 3<<'X'\ncargo test\nEOF\nnope\nX"),
        "cargo-test"
    ));
    // Data, not code.
    assert_eq!(e.rule("bash -c : <<'EOF'\ncargo test\nEOF"), None);
    assert_eq!(e.rule("bash script-none.sh <<'EOF'\ncargo test\nEOF"), None);
    assert_eq!(e.rule("git commit -F - <<'EOF'\ncargo test\nEOF"), None);
    assert_eq!(e.rule("cat <<'EOF'\ncargo test\nEOF"), None);
}

#[test]
fn empty_delimiter_body_is_consumed_not_run() {
    let e = env(CONFIG);
    assert_eq!(e.rule("cat <<''\ncargo test\n\nls"), None);
    assert_eq!(e.rule("cat <<\"\"\ncargo test\n\nls"), None);
    // The command after the body is still seen.
    assert!(is(e.rule("cat <<''\nx\n\ncargo test"), "cargo-test"));
}

#[test]
fn write_then_exec_needs_cat_or_tee() {
    let e = env(CONFIG);
    let body = "<<'EOF'\ncargo test\nEOF\n";
    assert!(is(
        e.rule(&format!("cat > /tmp/ws-x.sh {body}bash /tmp/ws-x.sh")),
        "cargo-test"
    ));
    assert!(is(
        e.rule(&format!("cat >> /tmp/ws-x.sh {body}source /tmp/ws-x.sh")),
        "cargo-test"
    ));
    assert!(is(
        e.rule(&format!("tee /tmp/ws-x.sh {body}. /tmp/ws-x.sh")),
        "cargo-test"
    ));
    assert!(is(
        e.rule(&format!("cat > /tmp/ws-x.sh {body}/tmp/ws-x.sh")),
        "cargo-test"
    ));
    // Variables from literal assignments.
    assert!(is(
        e.rule(&format!("f=/tmp/ws-x.sh; cat > \"$f\" {body}bash \"$f\"")),
        "cargo-test"
    ));
    assert!(is(
        e.rule(&format!("f=/tmp/ws-x.sh\ncat > ${{f}} {body}bash $f")),
        "cargo-test"
    ));
    // Reassigned dynamically: unknown, so not the written file.
    assert_eq!(
        e.rule(&format!(
            "f=/tmp/ws-x.sh; cat > \"$f\" {body}f=$(echo y); bash \"$f\""
        )),
        None
    );
    // A bare redirect copies nothing.
    assert_eq!(
        e.rule("> /tmp/ws-x.sh <<'EOF'\ncargo test\nEOF\nbash /tmp/ws-x.sh"),
        None
    );
    // `cat file > out` doesn't write stdin.
    assert_eq!(
        e.rule(&format!("cat a > /tmp/ws-x.sh {body}bash /tmp/ws-x.sh")),
        None
    );
    // Written but never run.
    assert_eq!(e.rule(&format!("cat > /tmp/ws-x.sh {body}ls")), None);
}

#[test]
fn local_script_files_are_read() {
    let e = env(CONFIG);
    let p = e.write("run.sh", "#!/bin/sh\ncargo test\n");
    let cwd = e.cwd();
    assert!(is(e.rule(&format!("bash {}", p.display())), "cargo-test"));
    assert!(is(e.rule("sh run.sh"), "cargo-test"));
    assert!(is(e.rule("source run.sh"), "cargo-test"));
    assert!(is(e.rule(". ./run.sh"), "cargo-test"));
    assert!(is(e.rule("./run.sh"), "cargo-test"));
    assert!(is(e.rule("bash run.sh <<'EOF'\nx\nEOF"), "cargo-test"));
    e.write("sub/check.sh", "cargo build\n");
    assert!(is(e.rule("cd sub && ./check.sh"), "cargo-test"));
    assert!(is(e.rule("cd sub; sh check.sh"), "cargo-test"));
    assert_eq!(e.rule("./check.sh"), None);
    assert_eq!(e.rule("cd sub && cd .. && ./check.sh"), None);
    assert!(is(e.rule_in(&cwd.join("sub"), "./check.sh"), "cargo-test"));
    // Missing and unreadable files are skipped.
    assert_eq!(e.rule("./nope.sh"), None);
}

#[test]
fn unsafe_files_are_skipped() {
    let e = env(CONFIG);
    let cwd = e.cwd();
    // Symlink: not followed.
    let real = e.write("real.sh", "cargo test\n");
    symlink(&real, cwd.join("link.sh")).unwrap();
    assert_eq!(e.rule("./link.sh"), None);
    assert!(is(e.rule("./real.sh"), "cargo-test"));
    // FIFO: opens without blocking, and is not a file.
    let fifo = cwd.join("fifo.sh");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(e.rule("./fifo.sh"), None);
    // Devices and system directories.
    assert_eq!(e.rule("bash /dev/null"), None);
    assert_eq!(e.rule("bash /usr/bin/env"), None);
    assert_eq!(e.rule("/bin/sh"), None);
    // node_modules.
    e.write("node_modules/.bin/tool.sh", "cargo test\n");
    assert_eq!(e.rule("node_modules/.bin/tool.sh"), None);
    assert_eq!(e.rule("cd node_modules/.bin && ./tool.sh"), None);
    // Binaries: ELF magic and NUL bytes.
    std::fs::write(cwd.join("elf.sh"), b"\x7fELF cargo test\n").unwrap();
    std::fs::write(cwd.join("nul.sh"), b"cargo test\0\n").unwrap();
    assert_eq!(e.rule("./elf.sh"), None);
    assert_eq!(e.rule("./nul.sh"), None);
}

#[test]
fn self_sourcing_scripts_end() {
    let e = env(CONFIG);
    e.write("loop.sh", ". ./loop.sh\ncargo test\n");
    assert!(is(e.rule("./loop.sh"), "cargo-test"));
    e.write("a.sh", ". ./b.sh\n");
    e.write("b.sh", ". ./a.sh\ncargo build\n");
    assert!(is(e.rule("./a.sh"), "cargo-test"));
}

#[test]
fn file_budget_is_eight_files() {
    let e = env(CONFIG);
    // Each script sources the next; `cargo test` is in the ninth.
    for i in 0..9 {
        let body = if i == 8 {
            "cargo test\n".to_string()
        } else {
            format!(". ./s{}.sh\n", i + 1)
        };
        e.write(&format!("s{i}.sh"), &body);
    }
    assert_eq!(e.rule("./s0.sh"), None);
    assert!(is(e.rule("./s1.sh"), "cargo-test"));
}

#[test]
fn byte_budget_is_64_kib_and_16_kib_a_file() {
    let e = env(CONFIG);
    // `cargo test` past the first 16 KiB of its file isn't read.
    let pad = "# pad\n".repeat(3000);
    e.write("big.sh", &format!("{pad}cargo test\n"));
    assert_eq!(e.rule("./big.sh"), None);
    // Four full files use the 64 KiB; the fifth is not read.
    let fill = "#".repeat(16 * 1024 - 1);
    for i in 0..4 {
        e.write(&format!("f{i}.sh"), &format!("{fill}\n"));
    }
    e.write("last.sh", "cargo test\n");
    assert_eq!(
        e.rule("./f0.sh; ./f1.sh; ./f2.sh; ./f3.sh; ./last.sh"),
        None
    );
    assert!(is(e.rule("./f0.sh; ./f1.sh; ./last.sh"), "cargo-test"));
}

#[test]
fn python_body_signals_and_rule_words() {
    let e = env(CONFIG);
    // File edits carry no signal: unclaimed.
    assert_eq!(
        e.rule("python3 - <<'EOF'\np = open('a').read().replace('x', 'y')\nEOF"),
        None
    );
    assert_eq!(
        e.rule("python3 - <<'EOF'\nimport json\njson.load(open('a'))\nEOF"),
        None
    );
    // A configured rule's words in the body win.
    assert!(is(
        e.rule("python3 - <<'EOF'\nimport subprocess\nsubprocess.run([\"cargo\", \"test\"])\nEOF"),
        "cargo-test"
    ));
    assert!(is(
        e.rule("python3 <<'EOF'\nimport os\nos.system('cargo build --release')\nEOF"),
        "cargo-test"
    ));
    // Otherwise script-heavy.
    assert!(is(
        e.rule("python3 - <<'EOF'\nfrom subprocess import run\nrun([\"my_tool\"])\nEOF"),
        "script-heavy"
    ));
    assert!(is(
        e.rule("python3 -c 'import multiprocessing as m; m.Pool(4)'"),
        "script-heavy"
    ));
    assert!(is(
        e.rule("python3 -c 'import subprocess; subprocess.run([\"cargo\", \"build\"])'"),
        "cargo-test"
    ));
    // `-c` makes the heredoc data.
    assert_eq!(
        e.rule("python3 -c 'print(1)' <<'EOF'\nimport subprocess\nEOF"),
        None
    );
    // Through wrappers.
    assert!(is(
        e.rule("env A=1 timeout 60 python3 - <<'EOF'\nimport subprocess\nEOF"),
        "script-heavy"
    ));
    // Words far apart are not a window.
    assert!(is(
        e.rule("python3 - <<'EOF'\nimport subprocess\nx = ['cargo', 1, 2, 3, 'test']\nEOF"),
        "script-heavy"
    ));
    // `lake` is a word, not a suffix.
    assert_eq!(
        e.rule("python3 - <<'EOF'\nflake = 1\nprint(flake)\nEOF"),
        None
    );
}

#[test]
fn python_and_node_files_and_input_modes() {
    let e = env(CONFIG);
    e.write(
        "t.py",
        "import subprocess\nsubprocess.run(['cargo', 'test'])\n",
    );
    e.write("edit.py", "open('a').write('b')\n");
    e.write(
        "h.mjs",
        "import { execSync } from 'child_process';\nexecSync('x');\n",
    );
    assert!(is(e.rule("python3 t.py"), "cargo-test"));
    assert_eq!(e.rule("python3 edit.py"), None);
    assert!(is(e.rule("node h.mjs"), "script-heavy"));
    // The positional file wins over a heredoc.
    assert_eq!(
        e.rule("python3 edit.py <<'EOF'\nimport subprocess\nEOF"),
        None
    );
    assert!(is(e.rule("python3 t.py <<'EOF'\nx\nEOF"), "cargo-test"));
    // Shebang routes an executable script to H3.
    e.write("tool", "#!/usr/bin/env python3\nimport subprocess\n");
    assert!(is(e.rule("./tool"), "script-heavy"));
    // node.
    assert!(is(
        e.rule("node --input-type=module <<'EOF'\nimport cp from 'child_process';\nEOF"),
        "script-heavy"
    ));
    assert!(is(
        e.rule("node --input-type=module <<'EOF'\nimport * as esbuild from 'esbuild';\nEOF"),
        "script-heavy"
    ));
    assert!(is(
        e.rule("node -e \"require('child_process').execSync('cargo test')\""),
        "cargo-test"
    ));
    assert_eq!(
        e.rule("node <<'EOF'\nrequire('fs').writeFileSync('a', 'b');\nEOF"),
        None
    );
    assert_eq!(e.rule("node -p '1+1' <<'EOF'\nchild_process\nEOF"), None);
    // Written, then run.
    assert!(is(
        e.rule("cat > /tmp/ws-t.py <<'EOF'\nimport subprocess\nEOF\npython3 /tmp/ws-t.py"),
        "script-heavy"
    ));
}

#[test]
fn script_heavy_slots_and_overrides() {
    let cmd = "python3 - <<'EOF'\nimport subprocess\nEOF";
    let v = |e: &Env| e.json(&["match", "--json", "--cwd", "/", "--", cmd], None, &[]);
    let e = env("");
    assert_eq!(v(&e)["slots"], 2);
    let e = env("script_slots = 5");
    assert_eq!(v(&e)["slots"], 5);
    let e = env("script_slots = 0");
    assert_eq!(v(&e)["slots"], Value::Null);
    let e = env("[[rule]]\nname = 'script-heavy'\nmatch = 'zzz'\nslots = 9\nenabled = false");
    assert_eq!(v(&e)["slots"], Value::Null);
    let e = env("");
    let out = e.run(&["match", "--json", "--", "true"], None, &[]);
    assert!(out.status.success());
}

fn claude(e: &Env, command: &str) -> Value {
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": { "command": command },
        "cwd": e.cwd(),
    });
    let out = e.run(&["hook", "claude"], Some(&payload.to_string()), &[]);
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn claude_hook_never_allows_an_inspected_match() {
    let e = env(CONFIG);
    e.write("run.sh", "cargo test\n");
    // Direct: allowed, as before.
    let r = claude(&e, "cargo test");
    assert_eq!(r["hookSpecificOutput"]["permissionDecision"], "allow");
    // Inspected: wrapped, with no decision.
    for cmd in ["./run.sh", "bash run.sh", "bash <<'EOF'\ncargo test\nEOF"] {
        let r = claude(&e, cmd);
        let out = &r["hookSpecificOutput"];
        assert!(out["updatedInput"]["command"].is_string(), "{cmd}: {r}");
        assert!(out.get("permissionDecision").is_none(), "{cmd}: {r}");
    }
}

#[test]
fn wrap_json_is_unchanged_without_the_flag() {
    let e = env(CONFIG);
    let env_g = [("MUZZLE_GRACE", "5")];
    let w = |args: &[&str]| e.json(args, None, &env_g);
    let v = w(&["wrap", "--json", "--cwd", "/", "--", "./mytool"]);
    assert_eq!(v, json!({"command": null}));
    let v = w(&["wrap", "--json", "--cwd", "/", "--", "cargo test"]);
    assert_eq!(
        v,
        json!({"command": "muzzle run --rule=cargo-test --slots 4 -- bash -c 'cargo test'",
               "rule": "cargo-test", "slots": 4})
    );
}

#[test]
fn wrap_with_grace_wraps_only_what_is_not_light() {
    let e = env(CONFIG);
    let wrap = |cmd: &str, extra: &[(&str, &str)], t: Option<&str>| {
        let mut args = vec!["wrap", "--json", "--with-grace", "--cwd", "/"];
        if let Some(t) = t {
            args.extend(["--timeout", t]);
        }
        args.extend(["--", cmd]);
        e.json(&args, None, extra)
    };
    let g = [("MUZZLE_GRACE", "5")];
    let v = wrap("./mytool --x", &g, None);
    assert_eq!(v["grace"], true);
    assert_eq!(v["matched"], false);
    assert_eq!(
        v["command"],
        "muzzle run --grace 5 --rule=unmatched-heavy --slots 4 -- bash -c './mytool --x'"
    );
    let v = wrap("./mytool", &g, Some("90"));
    assert_eq!(
        v["command"],
        "muzzle run --grace 5 --rule=unmatched-heavy --slots 4 --run-timeout 90 -- bash -c './mytool'"
    );
    // Light: every head is on the allowlist.
    for cmd in [
        "git status",
        "cd repo && ls | head",
        "FOO=1 env git log | wc -l",
        "echo hi; true",
        "/usr/bin/git status",
    ] {
        assert_eq!(wrap(cmd, &g, None), json!({"command": null}), "{cmd}");
    }
    // One heavy head makes the whole command wrapped.
    assert_eq!(wrap("git status && ./mytool", &g, None)["grace"], true);
    // A rule match is the normal reply, with no grace field.
    let v = wrap("cargo test", &g, None);
    assert_eq!(v["rule"], "cargo-test");
    assert!(v.get("grace").is_none());
    // Off: no env and no config, a zero, or already muzzle.
    assert_eq!(wrap("./mytool", &[], None), json!({"command": null}));
    assert_eq!(
        wrap("./mytool", &[("MUZZLE_GRACE", "0")], None),
        json!({"command": null})
    );
    assert_eq!(
        wrap("muzzle run -- ./mytool", &g, None),
        json!({"command": null})
    );
    // The config's grace_s is the fallback; the env var wins.
    let e2 = env("grace_s = 3\ngrace_max_slots = 2");
    let v = e2.json(
        &["wrap", "--json", "--with-grace", "--cwd", "/", "--", "./t"],
        None,
        &[],
    );
    assert_eq!(
        v["command"],
        "muzzle run --grace 3 --rule=unmatched-heavy --slots 2 -- bash -c './t'"
    );
    let v = e2.json(
        &["wrap", "--json", "--with-grace", "--cwd", "/", "--", "./t"],
        None,
        &[("MUZZLE_GRACE", "7.5")],
    );
    assert!(v["command"].as_str().unwrap().contains("--grace 7.5 "));
}

const HEREDOC: &str = "<<'A'\ncargo test\nA\n";

#[test]
fn fd_zero_is_read_as_written_and_dups_take_stdin() {
    let e = env(CONFIG);
    // A quoted `0` is the script argument, not an fd.
    assert_eq!(e.rule("bash '0'<<'A'\ncargo test\nA"), None);
    // `0<&3` makes fd 3 stdin; the heredoc feeds nothing.
    assert_eq!(e.rule("bash 3<light <<'A' 0<&3\ncargo test\nA"), None);
    assert_eq!(e.rule("bash <<'A' <&3\ncargo test\nA"), None);
    // An unquoted fd still counts.
    assert!(is(e.rule("bash 0<<'A'\ncargo test\nA"), "cargo-test"));
}

#[test]
fn heredoc_over_the_cap_drops_every_later_line() {
    let e = env(CONFIG);
    let long = "x".repeat(16 * 1024);
    assert_eq!(
        e.rule(&format!("bash <<'A'\n# {long}\ncargo test\nA")),
        None
    );
    // Under the cap the same shape matches.
    assert!(is(e.rule("bash <<'A'\n# x\ncargo test\nA"), "cargo-test"));
}

#[test]
fn written_files_are_keyed_by_directory_and_stdout() {
    let e = env(CONFIG);
    // The variable program is its file.
    assert!(is(
        e.rule(&format!("f=./new.sh; cat > \"$f\" {HEREDOC}\"$f\"")),
        "cargo-test"
    ));
    // A partly lifted substitution leaves a value we don't know.
    assert_eq!(
        e.rule(&format!(
            "cat > new.sh {HEREDOC}f=./new.sh$(printf .different); bash \"$f\""
        )),
        None
    );
    // A `cd` between the write and the run makes two files.
    assert!(is(
        e.rule(&format!("cat >sub/new.sh {HEREDOC}cd sub && bash ./new.sh")),
        "cargo-test"
    ));
    assert_eq!(
        e.rule(&format!("cat >new.sh {HEREDOC}cd sub && bash ./new.sh")),
        None
    );
    // Only the last `>` gets cat's stdout.
    assert_eq!(
        e.rule(&format!("cat >one.sh >two.sh {HEREDOC}bash ./one.sh")),
        None
    );
    assert!(is(
        e.rule(&format!("cat >one.sh >two.sh {HEREDOC}bash ./two.sh")),
        "cargo-test"
    ));
    // An explicit `-` copies stdin.
    assert!(is(
        e.rule(&format!("cat light - >new.sh {HEREDOC}bash ./new.sh")),
        "cargo-test"
    ));
}

#[test]
fn disk_files_resolve_symlinked_parents_and_skip_all_fat_binaries() {
    let e = env(CONFIG);
    e.write("sub/other.sh", "cargo test\n");
    std::fs::create_dir_all(e.cwd().join("sub/child")).unwrap();
    symlink("sub/child", e.cwd().join("ln")).unwrap();
    // `ln/..` is `sub`, not the directory `ln` sits in.
    assert!(is(e.rule("bash ln/../other.sh"), "cargo-test"));
    for (i, magic) in [
        [0xbe, 0xba, 0xfe, 0xca],
        [0xca, 0xfe, 0xba, 0xbf],
        [0xbf, 0xba, 0xfe, 0xca],
    ]
    .iter()
    .enumerate()
    {
        let mut b = magic.to_vec();
        b.extend_from_slice(b"\ncargo test\n");
        std::fs::write(e.cwd().join(format!("fat{i}.sh")), b).unwrap();
        assert_eq!(e.rule(&format!("bash fat{i}.sh")), None, "fat{i}");
    }
}

#[test]
fn disk_scripts_route_by_interpreter_and_signals_by_language() {
    let e = env(&format!(
        "{CONFIG}[[rule]]\nname = 'make-check'\ncommand = 'make check'\n"
    ));
    // Extension alone doesn't make a disk file python.
    e.write("check.py", "#!/bin/sh\nmake check\n");
    // Neither does `python` in a comment on the shebang line.
    e.write("chk", "#!/bin/sh # python\nmake check\n");
    assert!(is(e.rule("./check.py"), "make-check"));
    assert!(is(e.rule("./chk"), "make-check"));
    // `env -S` names the interpreter.
    e.write("t", "#!/usr/bin/env -S python3 -u\nimport subprocess\n");
    assert!(is(e.rule("./t"), "script-heavy"));
    // Each language has its own signals.
    e.write("p.py", "label = 'child_process'\n");
    e.write("n.js", "const label = 'multiprocessing';\n");
    assert_eq!(e.rule("python3 p.py"), None);
    assert_eq!(e.rule("node n.js"), None);
    // A file this text wrote still falls back to its extension.
    assert!(is(
        e.rule("cat > w.py <<'A'\nimport subprocess\nA\n./w.py"),
        "script-heavy"
    ));
}

#[test]
fn input_modes_cluster_flags_double_dash_and_empty_operands() {
    let e = env(CONFIG);
    assert!(is(
        e.rule("bash -xs arg <<'A'\ncargo test\nA"),
        "cargo-test"
    ));
    assert!(is(
        e.rule("node --input-type module <<'A'\nchild_process\nA"),
        "script-heavy"
    ));
    // After `--` the next word is a file, even named `-c`.
    e.write(
        "-c",
        "import subprocess\nsubprocess.run(['cargo', 'test'])\n",
    );
    assert!(is(e.rule("python3 -- -c"), "cargo-test"));
    // An empty code operand is the code; the next word is an argument.
    assert_eq!(e.rule("python3 -c '' 'import subprocess'"), None);
    assert_eq!(e.rule("node -e '' 'child_process'"), None);
}

#[test]
fn window_is_three_tokens_and_equals_splits() {
    let e = env(&format!(
        "{CONFIG}[[rule]]\nname = 'four'\ncommand = 'cargo check one two'\n"
    ));
    assert!(is(
        e.rule("python3 - <<'A'\nimport subprocess\nsubprocess.run([\"cargo\", \"check\", \"one\", \"two\"])\nA"),
        "script-heavy"
    ));
    assert!(is(
        e.rule("python3 - <<'A'\nimport subprocess\nx = \"cargo=test\"\nA"),
        "cargo-test"
    ));
}

#[test]
fn matcherless_script_heavy_tunes_the_builtin() {
    let cmd = "python3 - <<'EOF'\nimport subprocess\nEOF";
    let v = |e: &Env| e.json(&["match", "--json", "--cwd", "/", "--", cmd], None, &[]);
    let e = env("[[rule]]\nname = 'script-heavy'\nslots = 7\nclass = 'normal'");
    let r = v(&e);
    assert_eq!(r["rule"], "script-heavy");
    assert_eq!(r["slots"], 7);
    assert_eq!(r["class"], "normal");
    let e = env("[[rule]]\nname = 'script-heavy'\nenabled = false");
    assert_eq!(v(&e)["slots"], Value::Null);
}
