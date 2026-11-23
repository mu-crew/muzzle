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
muzzle drop --dead                                    # clear orphaned claims from dead/kept holders
muzzle report --since 7d                              # what each rule used, and a suggested weight
```

Rules in `~/.config/muzzle/config.toml` map commands to slots; the first
run writes a template. In pi the extension wraps matching bash calls for
you; Claude Code, Codex, Muse Code and cursor-agent use a hook.

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
`--run-timeout 900`.

**Stuck claims and self-deadlocks:** Nested runs start at once without
claiming. If a killed or backgrounded job leaves stray processes holding
slots, muzzle releases the claim once they go idle (or you can clear them
with `muzzle drop --dead`), and refuses to queue a pane behind its own
orphaned claim instead of hanging. See [docs/usage.md](docs/usage.md) for
timeouts and details.

## Does it help?

**In short:** yes, where it counts. On a normal busy day muzzle costs
nothing. On an overloaded machine it keeps the machine responsive and each
job's run time short and steady, for about 13% fewer jobs per hour at the
default budget of 1.5 slots per core.

We replayed real days of agent work on a 16-core Linux box (linuxpc). The
replay starts each gate, build and test run at the time it really happened,
once without muzzle (the hand-set `CARGO_BUILD_JOBS` caps the agents used
before) and once with it.
[Full write-up](docs/eval/2026-11-muzzle-v1.md), [raw results](docs/bench/0.2.4/).

**A normal busy day:** no difference. Both busiest windows replayed at real
speed finished at the same time, with run times within about a second of
each other. The machine never ran short of slots, so muzzle hardly ever made
anything wait.

**Overload:** the busiest window again, with arrivals packed 12 times as
densely, without muzzle and with it at 1.5 slots per core:

| at 12× | without muzzle | muzzle, 24 slots |
| --- | --- | --- |
| load average, peak (16 cores) | 90 | 25 |
| `git status` in a scratch repo, p95 | 31 ms | 9.7 ms |
| a job's run time, p90 | 329 s | 132 s |
| jobs finished per hour | 408 | 353 |

Load peaks at under a third, `git status` stays under 10 ms, and a job runs
in 40% of the time. Jobs wait in the queue instead, and wait plus run comes
to about 327 s at p90: the same as the run time alone without muzzle, with
the machine responsive throughout. Packed 24 times as densely, the machine
without muzzle peaked at a load of 157 and 103 ms for `git status`; muzzle
held it at 19 and 7 ms.

The default is 1.5 slots per core (`overcommit = 1.5`, 22 slots on this
machine after the reserve of 2). One slot per core, the 0.2.4 default, kept
the machine even quieter but finished 40–49% fewer jobs per hour, because
gates claim more cores than they keep busy. If muzzle queues while your CPU
sits idle, raise `overcommit`; if the machine gets sluggish under a full
queue, lower it (see [tune](npm/muzzle/skills/muzzle/recipes/tune.md)).

**Limits:** one workload, the swayward gates, on one machine. The overload
is simulated by packing a real day's arrivals closer together, and the
default (22 slots here) lies between the two budgets measured, 20 and 24. A
week of real use with muzzle against the weeks before it hasn't been
measured yet.

## More

| Doc | For |
| --- | --- |
| [docs/why.md](docs/why.md) | The overload, what muzzle does about it, the trade |
| [docs/usage.md](docs/usage.md) | Config, running, timeouts, exit codes, `muzzle drop`, orphan/deadlock guards, and the limits: process groups, `setsid`, advisory mode, containers |
| [docs/harnesses/](docs/harnesses/README.md) | `muzzle init`, hook snippets for Claude Code, Codex, Muse Code, cursor-agent, opencode, and the `allow` warning |
| [skills/muzzle](npm/muzzle/skills/muzzle/SKILL.md) | The agent skill, with recipes for tuning, benchmarking and harnesses |
| [bench/](bench/README.md) | Traces, the replay benchmark and the probe |
| [CHANGELOG.md](CHANGELOG.md) | Releases |

## License

MIT
