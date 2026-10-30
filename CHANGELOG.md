# Changelog

## 0.2.0 (unreleased)

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
