# Changelog

## Unreleased

Fixes from a review panel (2026-10-30). Spec notes dated 2026-10-30 in
`docs/specs/2026-10-29-muzzle.md` and R5 amendments in
`docs/specs/2026-10-30-muzzle-0.2.1-command-rules.md`.

Changed:

- **`muzzle hook claude` allows far less.** The raw command text must be
  plain words first: `A-Z a-z 0-9 _ . / : = , + @ % -` separated by single
  spaces. No quotes, `$`, braces, redirections, backslashes, tabs or
  newlines. Then every word must be on an allowlist: prefixes and their
  options, assignment names, the program, and per program every option,
  value, positional and word after `--`. Prefixes must be bare or live in
  `/usr/bin` or `/bin`. `env -C/-S/-a/-P` and abbreviations, glued
  code-naming options (`--config=`, `-C/dir`, `--prefix=`), `cargo +/path`,
  assignments such as `CARGO_TARGET_*_RUNNER=`, `PATH=` and
  `NODE_OPTIONS=`, and any word after `npm test` no longer auto-allow.
  Bare `time` (bash's keyword) takes only `-p`. Commands that used to
  auto-allow and now prompt include `cargo build 2>&1`, anything quoted,
  `vitest run <file>`, `npm run test -- <args>` and `contrib/gate <args>`.
  They are still wrapped. Several of these bypasses were already in
  0.2.1.
- **A non-leader job that stops for the terminal is ended.** muzzle prints
  a `muzzle:` hint, ends the job (SIGTERM, then SIGKILL after 10 s), frees
  the slots and exits 128 + the signal (149 for SIGTTIN, 150 for SIGTTOU).
  Before, it hung and kept the slots. muzzle never takes the terminal.
- **Once the command has ended, muzzle exits with its code** even if a
  later step fails: `--report` write, `runs.jsonl` append, keeper handoff,
  release save. The error goes to stderr. Before, these exited 125.
- `report` under config `slots =` sizes RAM per slot from this host and
  caps suggestions at `slots`. Plain `report` reads `slots =` from an
  existing config, silently, and never writes the template.
- `bench run` results gain `matched_wait_s` and `matched_run_s`, which
  leave out unmatched calls (D35). The existing fields are unchanged.

Fixed:

- An error after the keeper took over no longer frees the claim while the
  job runs, and the keeper's `runs.jsonl` line is no longer lost (D16).
- A failing `runs.jsonl` append no longer wedges the queue: waiters warn,
  prune and admit. The append now comes after the state save, so a
  failed save no longer duplicates the line.
- `nohup muzzle run …` and `trap '' HUP` work: signals the caller ignores
  stay ignored for muzzle and the job.
- A nested `--run-timeout` now reaches the nested job's descendants. On
  Linux that includes orphans in the outer claim's group, but not
  `setsid` daemons. Before, only the direct child was signalled.
- A non-leader job is recorded with its process group as it spawns, so a
  SIGKILLed muzzle can't lose the claim while waiting for the lock.
- `--timeout`/`--run-timeout` values too large for a duration exit 2
  instead of panicking.
- The shell reader finds the script in `bash -o pipefail -c …`, `-O` and
  `--rcfile`, and lexes `$'…'` quotes. Rules and container detection
  missed these in 0.2.1. One-dash long options (`bash -login -c`) and
  zsh's glued `-o<name>` keep working.
- `doctor` under `slots =` shows this host's cores and RAM per slot. Its
  runs-log count counts parseable runs. `report` honours the D17a budget
  expiry.
- Docs: the skill on nested runs and `cd repo && muzzle run` (it leads
  only on bash 4.4 or later, not macOS `/bin/bash`), harness exit codes,
  `linuxpc-config-021.toml` for command rules, the AGENTS.md layout, and
  spec §Architecture 4.4 on the leader's SIGINT/SIGQUIT. Also two
  documented limits: a terminal stop under `timeout` without
  `--foreground` goes unseen, and so does a nested-timeout orphan on
  macOS.
- Tests: a nesting regression fails instead of hanging, new tests cover
  D11 across classes and SIGINT while queued, timing bounds hold under
  load and pin D18 and the heartbeat, the container-scope test no longer
  flakes, and SIGINT tests survive an inherited ignore.

## 0.2.1

Rules that match command words (spec
`docs/specs/2026-10-30-muzzle-0.2.1-command-rules.md`). Additive: every
0.2.0 config still loads.

- **`command` rules.** `command = "cargo test"` or a list of them names a
  program and its subcommand words. muzzle tokenises the shell text and
  matches the program in command position: after `;`, `&&` and `|`, in
  `$( )`, inside `bash -c '…'`, past assignments, `env`, `timeout`, `nice`,
  `nohup`, and through `npx`, `uv run`, `mise exec --`, `flock` and
  container entry (`distrobox enter … --`, `toolbox run`,
  `podman|docker exec`). Option words are skipped (`cargo +nightly test`
  matches `cargo test`), a word ending in `*` matches by prefix, a
  `$VAR` program is stepped over, and a script's arguments are tried as
  commands. Quoted arguments, heredocs and comments never match, so polls
  such as `pgrep -f 'contrib/gate'` claim nothing.
- `match = '<regex>'` stays, as the escape hatch. A rule sets exactly one
  of `command` and `match` (both, or neither on a new rule, exits 2); in a
  path table or include, setting either replaces the inherited one.
- The config hash covers `command` (whitespace normalised, a single string
  as a one-element list). Switching a rule between `command` and `match`
  changes the hash.
- The template, the new `bench/linuxpc-config-021.toml`, the skill,
  `tune.md`, README and usage use `command` rules
  (`bench/linuxpc-config.toml` stays the v1 snapshot).

Changed:

- **`muzzle hook claude` approves fewer calls.** For a `command` rule it
  sends `allow` only when the one simple command's own word is the rule's
  program (or `./` plus it) followed by its subcommand words. A program
  path elsewhere (`/tmp/evil/cargo`) and a match through a wrapper,
  container, script or variable are wrapped but left to Claude's
  permission check. **`npx vitest run` loses auto-allow**, as do
  `mise exec -- …` and `distrobox enter … -- …`. The hook's form also
  skips no option arguments, so `cargo -C <dir> test`,
  `cargo --config '…runner=…' test` and `npm --prefix <dir> test` are
  wrapped but not auto-allowed.
- A `command` pattern's program can't contain `*` (exit 2): a prefix
  program would match, and auto-allow, any program.

## 0.2.0

Per-repo weights, a shared config with a machine file, and a run log that
keeps its history (spec `docs/specs/2026-10-30-muzzle-0.2-paths.md`).

**BREAKING.** There is no migration and no compatibility shim:

- `[project."<glob>"]` tables are removed. They are now an unknown key and
  exit 2. Use `[path."<dir>"]` tables.
- `match --json` no longer has `"source": "project"` or the `"project"`
  field. `source` is `"global"` or `"path"`, with `"path"` (the nearest
  table that touched the rule) and `"slots_from"` (`{"file", "path"}`).
- The config hash is version 2, computed over the merged config. Every
  0.2 hash differs from every 0.1 hash, so `report --compare` splits at
  the upgrade.

Added and changed:

- `[path."<dir>"]` tables apply to the repo a command runs in (its main
  checkout, so all worktrees share them) and cascade from the outermost
  directory to the nearest. An entry with a known name changes only the
  fields it sets; a new name is a new rule. `enabled = false` turns a rule
  off below a directory.
- `include = ["machine.toml"]` in the root file merges per-machine files.
  A missing include is skipped; `doctor` lists each as `found` or
  `not found`, with the number of path tables.
- `report --by repo` gives rows per rule and repo, with the slots the
  current config gives that repo and the file and table they come from.
- `runs.jsonl` is no longer pruned after 30 days. Each month, and past
  16 MB, it rotates into a `runs-YYYY-MM.jsonl.gz` archive kept forever;
  `report` reads the archives, and `doctor` shows them and warns when
  they grow past 256 MB.
- Run lines record `load1` and `held` at admission, and `report` shows
  `unclaimed load: p50, p90` once 20 runs carry them: a rough figure for
  load outside muzzle's claims.
- The skill says to wrap a script that runs builds or tests as a whole,
  and `tune.md` covers path tables, includes, `--by repo` and the
  unclaimed-load figure.

Fixed:

- A holder killed outright (muzzle and keeper gone) used to record `ran_s`
  up to the moment a later run pruned it (75 s for a 20 s job). Holders now
  write a heartbeat, so `ran_s` ends at the last time the job was seen
  alive (within about 2 s). These lines were already left out of report
  figures; the log is now accurate too.
- `bench/check-trace` never echoes an unknown key, its value or a file
  name in its errors, because CI logs are public. Symlinks under
  `bench/traces` are refused, in CI mode and by the pre-commit hook.
- The npm shim prints one stderr line when `execve` fails and it falls
  back to spawn, instead of failing silently: a group kill may leak the job
  in that mode.
- CI runs the npm package and extension tests on macOS as well as Linux.

Internal: the large modules (`run`, `report`, `doctor`, `rules/config`,
`bench/replay`) are split into submodules, with no change in behaviour.

## 0.1.2

- `wrap`, `match` and `hook` leave a command that already runs muzzle
  alone by its first command word's basename, so `/abs/bin/muzzle run …`,
  `./muzzle run …` and `env A=1 muzzle run …` are no longer wrapped again
  (spec D22). Before, only a literal `muzzle ` prefix was.
- The pi extension registers once per pi process. With both the pi package
  and `-e <extension>` loaded, every bash call was wrapped twice and got
  two result lines and two `muzzle` entries.

## 0.1.1

- Runs record `repo`, the canonical path of the repo's main checkout
  (git or jj; every worktree and workspace maps to the same path), or
  `null` outside a repo (spec D27a). It is an added field in `runs.jsonl`,
  `--report` and `status --json` holders. It is found by reading files,
  never by running git or jj. 0.1.1 only records it; per-repo reports come
  later.

## 0.1.0

First release: per-host admission for heavy commands, with a run log that
tunes itself (spec `docs/specs/2026-10-29-muzzle.md`). Linux and macOS.

- `muzzle run` queues a command for a weighted share of the host's slot
  budget (logical cores minus a reserve). Two classes, `high` ahead of
  `normal`, strict first-come, first-served within a class. No daemon: the
  queue is one locked `state.json`, and every waiter prunes dead entries.
- The claim sets `CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, `MAKEFLAGS` and
  `MUZZLE_SLOTS`. On Linux with a systemd user manager the job runs in a
  scope with a memory cap and CPU weight sized to the claim; elsewhere
  admission is advisory.
- A claim lasts as long as the job's process group. A background job keeps
  its slots through a detached keeper. Nested runs claim nothing.
- `--run-timeout` counts from admission (exit 124); `--timeout` gives up
  while queued (exit 75).
- Container gates (`distrobox enter`, `podman exec`, `toolbox run`,
  `docker exec`) are admission only, recorded `enforced: false`, and hold
  their slots until the container job exits.
- Every admitted run appends to `runs.jsonl`. `muzzle report` shows machine
  figures (utilisation, blocked-idle, CPU efficiency), per-rule rows and a
  suggested weight, and `--compare` puts two periods side by side.
- `muzzle match`, `muzzle wrap` and `muzzle hook claude|codex` do the
  matching and rewriting for integrations. `status` and `doctor`, with
  `--json`.
- `muzzle bench run` replays a trace without and with muzzle, synthetic or
  real; `muzzle bench probe` samples interactive latency.
  `bench/from-museum` and `bench/check-trace` make and check traces.
- npm package `@mu-crew/muzzle`: the binary, a pi extension that wraps
  matching bash calls, and the `muzzle` skill with recipes for tuning,
  benchmarking and harnesses. Tested hook snippets for Claude Code and
  Codex; an untested opencode example.
