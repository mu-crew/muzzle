#!/usr/bin/env python3
"""Classify museum bash commands under two muzzle configs and list the
differences (spec 0.2.1 C6, R6). Usage:

  scripts/rules-diff.py [--tsv] OLD_DIR NEW_DIR ROWS.json [ROWS.json ...]

OLD_DIR and NEW_DIR each hold a muzzle config as `muzzle/config.toml`
(plus anything it includes, such as `muzzle/machine.toml`); a plain .toml
file is copied into a temp dir of that shape. ROWS files are JSON lists
with `command` and `file` fields (museum-search calls --json output). Rows
are deduplicated on (file, start, command[:200]).

Each row runs `muzzle match --json` (MUZZLE_BIN, default
target/debug/muzzle) with `--cwd` decoded from the row's session folder
(R6: pi keeps sessions per cwd, `/` written as `-`), so `[path]` tables are
exercised. A folder whose directory is gone maps to the repo its workstream
worked on (REPOS) when that repo exists, else /tmp.

Rows exactly 300 characters long were cut by the export: they're their own
bucket, outside the bar. Every difference on a complete row is classed as
a regression, an accepted miss or a fix, with a reason (heuristics: every
regression row is checked by hand). Prints counts, then one TSV line per
difference: bucket, class, reason, old, new, cwd, command (first 200
chars, newlines as ⏎).
"""

import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from collections import Counter
from concurrent.futures import ThreadPoolExecutor

BIN = os.environ.get("MUZZLE_BIN", "target/debug/muzzle")
HOME = os.path.realpath(os.path.expanduser("~"))
TRUNCATED = 300

# Session folders of removed worktrees, by the workstream that made them,
# to the repo the worktree belonged to: `repo_of` would have given that
# repo had the worktree still existed (0.2 P2).
REPOS = [
    (".local-state-mu-next-worktrees-vtmux-", "~/hacking/vtmux"),
    (".local-state-mu-next-worktrees-muzzle-v1-", "~/hacking/crew/muzzle"),
    (".local-state-mu-workspaces-next-build-", "~/hacking/crew/mu"),
    (".local-state-mu-workspaces-swaywardlock-", "~/hacking/swayward-wm/swaywardlock"),
    (".local-state-mu-workspaces-lock-harden-", "~/hacking/swayward-wm/swaywardlock"),
    (".local-state-mu-workspaces-swayward-", "~/hacking/swayward-wm/swayward"),
]


def config_dir(arg: str) -> str:
    if os.path.isdir(arg):
        return arg
    d = tempfile.mkdtemp(prefix="rules-diff-")
    os.makedirs(f"{d}/muzzle")
    shutil.copy(arg, f"{d}/muzzle/config.toml")
    return d


def decode(folder: str, root: str = "/") -> str | None:
    """`--var-home-u-a-b--` back to an existing `/var/home/u/a-b`: each `-`
    is a `/` or a literal `-`, tried against the file system."""
    parts = folder.strip("-").split("-")

    def walk(base: str, i: int) -> str | None:
        if i == len(parts):
            return base
        name = ""
        for j in range(i, len(parts)):
            name = parts[j] if j == i else f"{name}-{parts[j]}"
            p = os.path.join(base, name)
            if os.path.isdir(p):
                found = walk(p, j + 1)
                if found:
                    return found
        return None

    return walk(root, 0) if parts and parts[0] else None


CWDS: dict[str, str] = {}


def cwd_of(row: dict) -> str:
    f = row.get("file") or ""
    folder = f.split("/")[1] if f.count("/") >= 2 else ""
    if folder not in CWDS:
        d = decode(folder)
        if d is None:
            rest = folder.strip("-").removeprefix(HOME.strip("/").replace("/", "-") + "-")
            for prefix, repo in REPOS:
                r = os.path.expanduser(repo)
                if rest.startswith(prefix) and os.path.isdir(r):
                    d = r
                    break
        CWDS[folder] = d or "/tmp"
    return CWDS[folder]


def rule(env: dict, cmd: str, cwd: str) -> str:
    out = subprocess.run(
        [BIN, "match", "--json", "--cwd", cwd, "--", cmd],
        capture_output=True, text=True, env=env, check=False,
    ).stdout
    try:
        return json.loads(out).get("rule") or "-"
    except json.JSONDecodeError:
        return "?"


# The words a rule's program shows up as, for the reasons below.
PROGRAMS = {
    "fast-gate": r"contrib/fast-gate",
    "gate": r"contrib/gate",
    "js-test": r"vitest|npm (run )?test",
    "cargo-test": r"cargo (\+\S+ )?(test|nextest|clippy|build)",
    "vtmux-check": r"scripts/check\.sh",
    "vtmux-harness": r"cargo xtask (regress|oracle)",
}
# Commands whose arguments name a program without running it.
READERS = {
    "grep", "rg", "pgrep", "pkill", "ps", "sed", "cat", "head", "tail", "ls",
    "which", "git", "echo", "printf", "wc", "find", "mu",
}
HEREDOC = re.compile(r"<<-?\s*\\?['\"]?(\w+)['\"]?[^\n]*\n(.*?)(\n\1\b|\Z)", re.S)
QUOTED = re.compile(r"'[^']*('|\Z)|\"(\\.|[^\"\\])*(\"|\Z)", re.S)


def heredoc_class(cmd: str, prog: re.Pattern) -> tuple[str, str] | None:
    """The program shows up only in heredoc bodies (one cut short by the
    export runs to the end)."""
    bodies = list(HEREDOC.finditer(cmd))
    hits = [m for m in bodies if prog.search(m.group(2))]
    if not hits:
        return None
    rest = cmd
    for m in reversed(bodies):
        rest = rest[: m.start(2)] + rest[m.end(2):]
    if prog.search(QUOTED.sub("", rest)):
        return None
    m = hits[0]
    line = cmd[: m.start()].rsplit("\n", 1)[-1]
    body = m.group(2)
    if re.search(r"\bpython3?\s+-(\s|$)", line):
        if re.search(r"subprocess|os\.system|os\.popen", body):
            return "accepted miss", "run by a heredoc-fed interpreter (R6)"
        return "fix", "text inside a heredoc-fed interpreter's source"
    target = re.search(r"\b(?:cat|tee)\b[^|;&<]*?>?\s*([^\s<>|;&]+)\s*$", line[: line.find("<<")])
    if target:
        path = target.group(1)
        after = cmd[m.end():]
        if path.endswith(".sh") or body.lstrip().startswith("#!") or os.path.basename(path) in after:
            return "accepted miss", "inside a script the text writes (and runs) (P12, R6)"
    return "fix", "heredoc text (a note, commit message or file body)"


def read_only_mention(cmd: str, prog: re.Pattern) -> bool:
    """Every simple command that names `prog` is a READERS command."""
    try:
        lex = shlex.shlex(cmd, posix=True, punctuation_chars=True)
        lex.whitespace_split = True
        toks = list(lex)
    except ValueError:
        return False
    segs, cur = [], []
    for t in toks + [";"]:
        if t and set(t) <= set(";&|()"):
            segs.append(cur)
            cur = []
        else:
            cur.append(t)
    hits = [s for s in segs if any(prog.search(w) for w in s)]
    return bool(hits) and all(s[0] in READERS and not prog.search(s[0]) for s in hits)


def classify(old: str, new: str, cmd: str) -> tuple[str, str]:
    """(class, reason) for a complete row whose rule changed (R6)."""
    if old == "-":
        return "fix", f"now claimed as {new}: the old regex missed this command word"
    if new != "-":
        return "regression?", f"rule changed {old} -> {new}"
    prog = re.compile(PROGRAMS.get(old, re.escape(old)))
    if re.search(r"\bmuzzle(\S*)? run\b", cmd):
        return "fix", "already inside a hand-written `muzzle run` (D22: no nested run)"
    if re.search(rf"\bmuzzle\S* (wrap|match)\b[^;&|\n]* -- ['\"]?[^'\"]*({prog.pattern})", cmd):
        return "fix", "an argument to `muzzle wrap|match`: nothing runs"
    if re.search(r"\bssh\b", cmd) and prog.search(cmd[re.search(r"\bssh\b", cmd).start():]):
        return "fix", "runs on another host over ssh"
    if h := heredoc_class(cmd, prog):
        return h
    if not prog.search(QUOTED.sub("", cmd)):
        return "fix", "quoted text (a note, message, pattern or remote command)"
    if read_only_mention(cmd, prog):
        return "fix", "an argument or pattern of a read-only command, not a command word"
    return "regression?", "unclassified: check by hand"


def main() -> int:
    args = sys.argv[1:]
    tsv = "--tsv" in args
    args = [a for a in args if a != "--tsv"]
    if len(args) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    old, new, *files = args
    seen, rows = set(), []
    for f in files:
        for r in json.load(open(f)):
            k = (r.get("file"), r.get("start"), r["command"][:200])
            if k not in seen:
                seen.add(k)
                rows.append(r)
    cwds = [cwd_of(r) for r in rows]
    eo = dict(os.environ, XDG_CONFIG_HOME=config_dir(old))
    en = dict(os.environ, XDG_CONFIG_HOME=config_dir(new))
    with ThreadPoolExecutor(16) as ex:
        a = list(ex.map(lambda rc: rule(eo, rc[0]["command"], rc[1]), zip(rows, cwds)))
        b = list(ex.map(lambda rc: rule(en, rc[0]["command"], rc[1]), zip(rows, cwds)))
    trunc = [len(r["command"]) == TRUNCATED for r in rows]
    print(f"rows {len(rows)}  complete {trunc.count(False)}  truncated {trunc.count(True)}")
    print(f"cwd decoded {sum(c != '/tmp' for c in cwds)} rows, /tmp {cwds.count('/tmp')}")
    print("old", dict(Counter(a)))
    print("new", dict(Counter(b)))
    out = []
    for x, y, r, c, t in zip(a, b, rows, cwds, trunc):
        if x == y:
            continue
        # A truncated row is classed the same way, for the note only: what
        # was cut can change the answer, so it's outside the bar (R6).
        cls, why = classify(x, y, r["command"])
        out.append(("truncated" if t else "complete", cls, why, x, y, c, r["command"]))
    print(f"differ {len(out)}")
    for k, n in sorted(Counter(o[:3] for o in out).items()):
        print(f"  {n:4}  {' / '.join(k)}")
    for k, n in sorted(Counter((o[0], o[3], o[4]) for o in out).items()):
        print(f"  {n:4}  {k[0]}: {k[1]} -> {k[2]}")
    if tsv:
        for o in sorted(out, key=lambda o: o[:5]):
            c = o[6][:200].replace("\n", " ⏎ ")
            cwd = o[5].replace(HOME, "~").replace(os.path.expanduser("~"), "~")
            print("\t".join([*o[:5], cwd, c]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
