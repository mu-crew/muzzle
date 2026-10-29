# Changelog

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
