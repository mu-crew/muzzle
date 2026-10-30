#!/usr/bin/env python3
"""Classify museum bash commands under two muzzle configs and list the
differences (spec 0.2.1 C6). Usage:

  scripts/rules-diff.py OLD.toml NEW.toml ROWS.json [ROWS.json ...]

ROWS files are JSON lists with a `command` field (museum-search calls
--json output). Rows are deduplicated on (file, start, command[:200]).
Runs `muzzle match --json` (MUZZLE_BIN, default target/debug/muzzle) with
XDG_CONFIG_HOME pointed at a temp dir per config. Prints counts and every
command whose rule differs, as TSV: old, new, secs, command (first 200
chars, newlines shown as ⏎).
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
from collections import Counter
from concurrent.futures import ThreadPoolExecutor

BIN = os.environ.get("MUZZLE_BIN", "target/debug/muzzle")


def env_for(cfg: str) -> dict:
    d = tempfile.mkdtemp(prefix="rules-diff-")
    os.makedirs(f"{d}/muzzle")
    shutil.copy(cfg, f"{d}/muzzle/config.toml")
    return dict(os.environ, XDG_CONFIG_HOME=d)


def rule(env: dict, cmd: str) -> str:
    out = subprocess.run(
        [BIN, "match", "--json", "--cwd", "/tmp", "--", cmd],
        capture_output=True, text=True, env=env, check=False,
    ).stdout
    try:
        return json.loads(out).get("rule") or "-"
    except json.JSONDecodeError:
        return "?"


def main() -> int:
    old, new, *files = sys.argv[1:]
    if not files:
        print(__doc__, file=sys.stderr)
        return 2
    seen, rows = set(), []
    for f in files:
        for r in json.load(open(f)):
            k = (r.get("file"), r.get("start"), r["command"][:200])
            if k not in seen:
                seen.add(k)
                rows.append(r)
    eo, en = env_for(old), env_for(new)
    with ThreadPoolExecutor(16) as ex:
        a = list(ex.map(lambda r: rule(eo, r["command"]), rows))
        b = list(ex.map(lambda r: rule(en, r["command"]), rows))
    print(f"rows {len(rows)}")
    print("old", dict(Counter(a)))
    print("new", dict(Counter(b)))
    diffs = [(x, y, r) for x, y, r in zip(a, b, rows) if x != y]
    print(f"differ {len(diffs)}  by (old -> new): {dict(Counter((x, y) for x, y, _ in diffs))}")
    for x, y, r in sorted(diffs, key=lambda d: (d[0], d[1], -(d[2].get('secs') or 0))):
        c = r["command"][:200].replace("\n", " ⏎ ")
        print(f"{x}\t{y}\t{round(r.get('secs') or 0)}\t{c}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
