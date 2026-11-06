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

## Does it help?

**In short:** when the machine has room, muzzle costs nothing. On an
overloaded machine it keeps things usable and each job's run time steady,
but at the default budget it finishes fewer jobs per hour. A larger budget
wins most of that back.

We replayed real days of agent work on a 16-core Linux box (linuxpc). The
replay starts each gate, build and test run at the time it really happened,
once without muzzle (the hand-set `CARGO_BUILD_JOBS` caps the agents used
before) and once with it.
[Full write-up](docs/eval/2026-11-muzzle-v1.md), [raw results](docs/bench/0.2.4/).

**A normal busy day:** no difference. Both busiest windows replayed at real
speed finished at the same time, with run times within about a second of
each other. The machine never ran short of slots, so muzzle hardly ever made
anything wait.

**Overload:** the busiest window again, with arrivals packed 12 and 24 times
as densely:

| at 24× | without muzzle | with muzzle |
| --- | --- | --- |
| load average, peak (16 cores) | 157 | 19 |
| `git status` in a scratch repo, p95 | 103 ms | 7 ms |
| a job's run time, p90 | 561 s | 96 s |
| jobs finished per hour | 486 | 248 |

muzzle keeps the machine usable for whoever is working in it, but at the
default budget (cores − 2) it finishes 40–49% fewer jobs per hour. Gates
claim more cores than they keep busy, so the budget fills while cores sit
idle. In a 12× run with a budget of 1.5× cores (`slots = 24` on this
machine), the loss fell from 40% to 13% and `git status` stayed under
10 ms. If you see muzzle queueing
while your CPU sits idle, raise `slots` (see
[tune](npm/muzzle/skills/muzzle/recipes/tune.md)).

**Limits:** one workload, the swayward gates, on one machine. The overload
is simulated by packing a real day's arrivals closer together. We haven't
yet measured a week of real use with muzzle against the weeks before it.

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
