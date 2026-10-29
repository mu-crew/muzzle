# muzzle

A crew of agents on one machine runs gates, builds and test suites at once,
each sized for the whole machine, and together they overload it. `muzzle run`
queues each heavy command for a weighted share of a per-host slot budget, so
the machine stays usable and the gates that unblock others go first. Every
admitted run is logged, and `muzzle report` suggests weights from that log.

**Status: not implemented yet.** The design is in
[`docs/specs/2026-10-29-muzzle.md`](docs/specs/2026-10-29-muzzle.md).

## Install

```sh
npm i -g @mu-crew/muzzle            # the binary
pi install npm:@mu-crew/muzzle      # the pi extension and skill
```

Prebuilt for Linux x64, Linux arm64 and macOS arm64. Or `cargo install muzzle-cli`.

## Usage

```sh
muzzle run    [--slots N] [--class high|normal] [--rule LABEL] [--run-timeout S]
              [--timeout S] [--report FILE] -- CMD [ARGS…]
muzzle match  [--json] [--cwd DIR] -- 'SHELL STRING'
muzzle wrap   --json [--cwd DIR] [--timeout S] -- 'SHELL STRING'
muzzle hook   claude|codex
muzzle status [--json]
muzzle doctor [--json]
muzzle report [--rule LABEL] [--since D] [--until D] [--compare A..B C..D] [--json]
muzzle bench  run TRACE [--mode none|muzzle] [--tier synthetic|real] [--speed X] [--out DIR]
muzzle bench  probe [--samples N] [--interval S] [--out FILE]
```

## License

MIT
