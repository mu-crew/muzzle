# 0.2.1 command rules: the museum check

Spec: `docs/specs/2026-10-30-muzzle-0.2.1-command-rules.md`, C6 and R6.
Result: **zero regressions** on complete rows after one matcher change
(`bash -n`, below).

## Method

```sh
mkdir -p old/muzzle && cp ~/.config/muzzle/{config,machine}.toml old/muzzle/   # live 0.2.0 config, hash 5c5aa6
MUZZLE_BIN=target/debug/muzzle scripts/rules-diff.py --tsv \
  old bench/linuxpc-config-021.toml /tmp/pb/mu.json /tmp/pb/swayward.json
```

- **Old:** the live 0.2.0 config (regex rules, vtmux and crew/muzzle
  `[path]` tables), cfg `5c5aa6`. **New:**
  `bench/linuxpc-config-021.toml` (command rules; `oracle-sweep` stays a
  `match` rule, first), cfg `6aa3ee`. Both are run by the same binary, built
  at this commit.
- **Rows:** the museum `mu` and `swayward` bash calls cached in `/tmp/pb`,
  deduplicated on (file, start, command[:200]): **38,139**. That replaces
  C6's 37,990.
- **cwd (R6):** each row's session folder is decoded back to a directory
  (pi writes the cwd with `/` as `-`; each `-` is tried as `/` and as `-`
  against the file system). A removed worktree maps to the repo its
  workstream worked on (`REPOS` in the script), which is the repo
  `repo_of` would have returned. 32,241 rows got a real cwd and 5,898 fell
  back to `/tmp`: scratch dirs under `/tmp`, and other hosts' paths.
- **Truncated:** 18,017 rows are exactly 300 characters long, cut by the
  export. They're a bucket of their own, outside the bar. 20,122 rows are
  complete.
- **Classes:** the script gives each difference a heuristic class and
  reason. All 26 complete-row differences were then read by hand. The
  heuristics and the hand check agree on every row.

## Counts

| | rows | old claimed | new claimed | differ |
| --- | ---: | ---: | ---: | ---: |
| complete | 20,122 | | | 26 |
| truncated | 18,017 | | | 125 |
| all | 38,139 | 2,284 | 2,161 | 151 |

Per rule, old → new: js-test 1,547 → 1,513, cargo-test 525 → 467, gate
144 → 128, vtmux-check 31 → 23, fast-gate 17 → 13, vtmux-harness 15 → 12,
oracle-sweep 5 → 5.

### Complete rows (the bar)

| class | rows | reason |
| --- | ---: | --- |
| **regression** | **0** | |
| accepted miss | 1 | inside a script the text writes, then runs (P12) |
| fix | 15 | quoted text: a note, message, grep pattern or remote command |
| fix | 3 | runs on another host over ssh |
| fix | 3 | an argument of a read-only command (`ps`, `grep`, `git diff --`) |
| fix | 1 | already inside a hand-written `muzzle run` (D22) |
| fix, newly claimed | 3 | a command word the old regex missed |

### Truncated rows (reported, outside the bar)

| class | rows |
| --- | ---: |
| accepted miss: inside a script the text writes | 33 |
| accepted miss: run by a heredoc-fed interpreter | 1 |
| fix: quoted text | 40 |
| fix: heredoc text (a note, commit message, file body) | 14 |
| fix: text in a heredoc-fed interpreter's source | 8 |
| fix: already inside `muzzle run` | 9 |
| fix: over ssh | 4 |
| fix: read-only command argument | 4 |
| fix: an argument to `muzzle wrap` | 1 |
| fix, newly claimed | 11 |

The heuristic classes no truncated row as a regression. Most of the losses
are `cat > x.sh <<EOF … cargo build …` bodies, where the export cut the
text before the script runs.

## What changed to reach zero

The first run classed 4 truncated rows as newly claimed by `gate`:
`bash -n contrib/PKGBUILD contrib/build-deb … contrib/gate`. That's a
syntax check that runs nothing, but R1.1 treats `bash <file>` as running
the file. `script_program` (`src/rules/heads.rs`) now returns nothing when
a short option contains `n`, and `heads/tests.rs` covers it. No other
matcher or config change was needed.

## Examples

Paths are trimmed and examples cut short.

**Accepted miss: a written-then-run script (P12).**
- `cat > …/h39/gate.sh <<'E' ⏎ cd …/fixer-14 ⏎ contrib/gate 2>&1 | tail -40 ⏎ E`
  (complete; the script runs in a later call).
- `cat > /tmp/u.sh <<'E' ⏎ … cargo test "$@" … ⏎ E ⏎ distrobox enter swayward-dev -- bash /tmp/u.sh pam_children`
- `python3 - <<'EOF' ⏎ import subprocess … subprocess.run("npx vitest run …")`
  (a heredoc-fed interpreter).

**Fix: quoted text.**
- `ps -eo pid,ppid,args | grep -E 'contrib/gate|cargo clippy|cargo test' | grep -v grep`
- `cat package.json | grep -n '"test\|vitest'`
- `mu task close … --evidence "… mise exec -- bash scripts/check.sh: exit 0 …"`

**Fix: another host.**
- `timeout 600 ssh -o BatchMode=yes macmini '… cd /tmp/…; cargo test -q --locked --test bench …'`
- `ssh macmini 'cd /tmp/tt-trace_tools && … cargo test --test bench_tools …'`

**Fix: a read-only command's argument.**
- `git diff 46f87cc HEAD -- contrib/gate .github/ | head -30`
- `ps aux | grep -c "cargo build"`

**Fix: already inside muzzle (D22).**
- `cd npm/muzzle && sed -i … && CARGO_BUILD_JOBS=4 ~/…/muzzle run --rule rust-gate --slots 3 -- cargo build --locked`:
  the old regex claimed the `cargo build` after `muzzle run`'s `--`.
  `runs_muzzle` doesn't fire, because the first simple command is `cd`.
  `muzzle` isn't a descent wrapper, though, so its inner command isn't a
  head, and the text isn't wrapped a second time (D22's intent).

**Fix: newly claimed (the old regex missed it).**
- `for i in 1 2; do taskset -c 0-3 cargo test -q --locked --test bench …; done`
  (`taskset` is a wrapper).
- `env -u TMUX -u TMUX_PANE -u HERDR_ENV npm run test:fast …` (the old
  prefix took one `env` option, not three).
- `timeout 300 distrobox enter swayward-dev -- bash -lc 'cd … && /usr/bin/time -p timeout 280 contrib/gate --fast'`.
