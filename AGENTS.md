# AGENTS.md — muzzle

`muzzle` is per-host admission for heavy commands: `muzzle run` queues a
command for a weighted share of the machine's slot budget, runs it, and logs
what it used. No daemon.

Read [`docs/specs/2026-10-29-muzzle.md`](docs/specs/2026-10-29-muzzle.md)
before changing behaviour. Decisions are numbered (Dn); cite them in commits
and comments. The spec wins over any brief.

## The commit gate

Every commit must be clean on all three:

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

- `cargo fmt --check`, not `cargo fmt`: the gate verifies, it doesn't mutate.
- `-D warnings` is not negotiable. An intended `#[allow(...)]` carries a
  comment saying why.
- No `#[ignore]` to get a commit through. Never `--no-verify`.
- Build with `CARGO_BUILD_JOBS=4` and never run two full test suites at once
  on a shared machine (until muzzle itself runs there).
- CI (`.github/workflows/check.yml`) runs the gate on ubuntu-latest and
  macos-latest. Only Linux runs locally, so a change to a macOS code path
  needs a green CI run before review.

## Layout

One module per unit in spec §Architecture. Every module, subcommand and
dependency is pre-declared, so a unit edits only its own files.

| file | unit |
| --- | --- |
| `src/cli.rs` | the clap surface, spec §Interface/CLI |
| `src/main.rs` | dispatch only |
| `src/exit.rs` | exit codes, spec §Exit codes |
| `src/admit.rs` | pure admission: `(state, now, liveness) → (state, admitted)` |
| `src/state.rs` | locked `state.json` read-modify-write |
| `src/proc/` | platform layer: `mod.rs`, `linux.rs`, `macos.rs` |
| `src/run/` | `muzzle run`: `queue.rs` (enqueue, poll), `spawn.rs` (groups, signals), `scope.rs` (systemd), `job.rs`, `keeper.rs` (D16), `report.rs` (run records, `--report`) |
| `src/runlog.rs` | `runs.jsonl` append, rotation and monthly archives (P11) |
| `src/repo.rs` | which repo a run belongs to (D27a) |
| `src/rules/` | `config/` (load, merge, template, hash), `resolve.rs` (what applies at a cwd), `shell.rs`, `heads.rs` (command rules), `hook.rs`; `match`, `wrap` |
| `src/status.rs` | `status` |
| `src/doctor/` | `doctor`: `gather.rs` (I/O), `checks.rs` (pure), `render.rs` |
| `src/report/` | `report`: `period.rs`, `rows.rs`, `by_repo.rs`, `machine.rs`, `render.rs` |
| `src/bench/` | `bench run` and `bench burn` (`replay/`), `bench probe` (`probe.rs`) |
| `npm/muzzle/` | npm wrapper, `extension/` (pi), `skills/muzzle/` |
| `npm/muzzle-<platform>/` | prebuilt binaries, filled by release CI |

A unit that must touch `Cargo.toml`, `src/cli.rs`, `src/lib.rs` or
`src/main.rs` rebases onto main before closing and says so in its evidence.

## Invariants

- **No daemon, and no process between invocations** except a keeper holding
  a live group's claim (D16). Every waiter re-runs pruning, so the queue
  works with no muzzle alive.
- **`admit` does no I/O.** All queue rules live there and are unit-tested.
- **`status --json` fields are only ever added**, never renamed or removed:
  mu host integration reads them.
- **Traces never carry command text, paths, names or other hosts**
  (D36–D40). `bench/check-trace` enforces this; `runs.jsonl` is never
  published.
- **Never put `timeout` outside `muzzle run`**: it counts the queue wait. Use
  `--run-timeout`.

## Style

Rust edition 2024, `clap` derive, `anyhow` at the boundary with `thiserror`
for errors callers switch on, `serde` with `toml` for config. Comments explain
why, not what; where a decision was measured, put the number in the comment.
