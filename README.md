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
match = '\bcargo (test|nextest|clippy|build)\b'
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

Not yet. The proof (a replay of a real day without and with muzzle, a
rollout week against a measured baseline, and an interactive probe) is
written up in `docs/eval/` once it has run, and this section will link to
it. Until then muzzle claims nothing about speed.

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
