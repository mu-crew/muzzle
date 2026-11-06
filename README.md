# muzzle

A crew of agents on one machine runs gates, builds and test suites at once,
each sized for the whole machine, and together they overload it. `muzzle run`
queues each heavy command for a weighted share of a per-host slot budget, so
the machine stays usable and the gates that unblock others go first. Every
admitted run is logged, and `muzzle report` suggests weights from that log.
No daemon. [Why, and what it trades](docs/why.md).

## Install

```sh
npm i -g @mu-crew/muzzle            # the binary
pi install npm:@mu-crew/muzzle      # the pi extension and skill
```

Prebuilt for Linux x64, Linux arm64 and macOS arm64. Or `cargo install muzzle-cli`.

## Use

```sh
muzzle run --rule cargo-test -- cargo test --locked   # ends with: muzzle: waited Ws for N slots, ran Rs
muzzle status                                         # who holds the slots, who waits
muzzle report --since 7d                              # what each rule used, and a suggested weight
```

Rules in `~/.config/muzzle/config.toml` map commands to slots; the first
run writes a template. In pi the extension wraps matching bash calls for
you; Claude Code and Codex use a hook.

```toml
include = ["machine.toml"]     # per-machine file, merged after this one if it exists

[[rule]]
name  = "cargo-test"
command = ["cargo test", "cargo nextest", "cargo clippy", "cargo build"]
slots = 3

[path."~/hacking/crew"]         # this directory's repos and their worktrees
[[path."~/hacking/crew".rule]]
name  = "cargo-test"
slots = 2                      # reweight only
```

`muzzle report --by repo` shows which repos need their own weight. The
run log keeps everything: each month moves into a gzip archive (about
3–8 MB a month before compression). [docs/usage.md](docs/usage.md#config)
has the details.

**Never put `timeout` outside `muzzle run`**: it counts the queue wait. Use
`--run-timeout S`.

## Measured

On linuxpc (16 cores), the real swayward gates replayed from busy days,
without and with muzzle ([eval](docs/eval/2026-11-muzzle-v1.md),
[results](docs/bench/0.2.4/)):

- **The busiest real windows at real speed show no measurable cost**: no
  longer makespan, run times within about 1 s, probe unchanged. Those
  windows never filled the budget, so muzzle rarely queued.
- **Under overload (the busiest window at 12× and 24×), muzzle keeps the
  machine responsive**: load at or under 20 instead of 157, `git status` p95
  about 7 ms instead of 103 ms, and the run p90 of rule-matched calls steady at
  ~95 s instead of up to 561 s.
- **The cost is throughput**: 40–49% fewer jobs per hour at the default
  budget (cores − 2), because gates claim more slots than they keep busy.
  A budget of 1.5× cores (`slots = 24`) cut the loss to 13% while keeping
  `git status` p95 under 10 ms.

One workload on one machine, and no rollout week, so gate p90 and merged
tasks per hour in real use are not measured; the eval lists the limits.

## More

| Doc | For |
| --- | --- |
| [docs/why.md](docs/why.md) | The overload, what muzzle does about it, the trade |
| [docs/usage.md](docs/usage.md) | Config, running, timeouts, exit codes, and the limits: process groups, `setsid`, advisory mode, containers |
| [docs/harnesses/](docs/harnesses/README.md) | Hook snippets for Claude Code, Codex, opencode, and the `allow` warning |
| [skills/muzzle](npm/muzzle/skills/muzzle/SKILL.md) | The agent skill, with recipes for tuning, benchmarking and harnesses |
| [bench/](bench/README.md) | Traces, the replay benchmark and the probe |
| [spec](docs/specs/2026-10-29-muzzle.md) | Design and its numbered decisions; [0.2 amendments](docs/specs/2026-10-30-muzzle-0.2-paths.md) for path tables, includes and the run log |
| [CHANGELOG.md](CHANGELOG.md) | Releases |

## License

MIT
