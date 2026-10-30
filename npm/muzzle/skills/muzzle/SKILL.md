---
name: muzzle
description: Per-host admission for heavy commands (gates, builds, test suites). Use when a bash result carries a `muzzle:` line (queued, admitted, waited/ran, gave up, killed while queued), when wrapping a heavy command by hand with `muzzle run`, when tuning muzzle's weights or benchmarking it, or when hooking muzzle into a harness.
---

# muzzle

muzzle queues each heavy command for a weighted share of this machine's slot
budget (cores minus a reserve), runs it, and logs what it used. Rules in
`~/.config/muzzle/config.toml` decide which commands claim slots and how
many. A command that matches no rule runs at once, unclaimed. There is no
daemon: every waiting muzzle re-checks the queue itself. `muzzle <cmd> --help`
is the reference for flags; this skill covers what the help can't tell you.

In pi, the muzzle extension wraps matching bash calls for you. Elsewhere you
wrap by hand, or a harness hook does it.

## Reading muzzle's lines

| line (stderr, or appended to a pi result) | means | do |
| --- | --- | --- |
| `muzzle: queued 3/5, 14/14 slots held (worker-7 gate 6, …)` | waiting; printed every 30 s. Holders read `agent rule slots` | wait. The command has not started |
| `muzzle: admitted after 312s (6 slots)` | the wait ended, the command starts now | nothing |
| `muzzle: waited 312s for 6 slots, ran 241s` | the run finished; the exit status is the command's own | judge the command by its output, not by the wait |
| `muzzle: nested in an outer claim, ran 4s` | it ran inside another claim and took no slots | nothing |
| `muzzle: gave up after 60s queued (position 2/4, --timeout)` | exit 75: `--timeout` expired before admission, nothing ran | retry later, or drop `--timeout` |
| `muzzle: killed while queued (position 2/4, waited 120s). Put timeout inside …` | something killed muzzle while it waited: usually a harness or `timeout` counting the wait | move the timeout (below); nothing ran |

A long wait is the queue working, not a failure: other gates are using the
machine. Run `muzzle status` to see who holds the slots.

## Wrapping by hand

```sh
muzzle run --rule cargo-test -- cargo test --locked
muzzle run --rule gate --run-timeout 900 -- contrib/fast-gate
```

- `--rule NAME` alone takes the weight from the config, so a later tuning
  applies. `--slots N` pins the weight and overrides the config. A name the
  config doesn't have prints `rule NAME is not in the config; claiming 1
  slot(s)` and runs: fix the name from the `name =` lines in the config.
- **muzzle is the whole command.** Start the call with `muzzle`, and put
  pipes, `cd` and `&&` inside `bash -c`:
  `muzzle run --rule gate -- bash -c 'cd repo && contrib/fast-gate 2>&1 | tail -50'`.
  `CMD` runs as argv, not through a shell, so a bare pipe can't go there.
- Piped or followed by another command (`muzzle run … | tail`), muzzle no
  longer leads the shell's process group. Esc or a pi timeout then kills
  muzzle but leaves the job running, holding its slots.
- `cd repo && muzzle run --class high …` also works: bash execs muzzle as
  the last command, so it leads the group, and no `command` rule matches
  it, so pi leaves it alone and its flags apply. A `match` regex rule that
  hits text inside the call does wrap it again, and the inner run is then
  nested (D17): its `--class`, `--slots` and `--run-timeout` are ignored.
  Starting the call with `muzzle` avoids that.
- **`timeout` goes inside `muzzle run`, never outside it.** An outer
  `timeout` counts the queue wait and kills the gate before it starts. Use
  `--run-timeout S`, which counts from admission and exits 124.
- In pi, a command that already runs muzzle (`muzzle run …`, by any
  path) is left alone by the extension, so pi's own bash `timeout` still counts the wait. Give a
  hand-wrapped call no pi timeout, and put the limit in `--run-timeout`.
- muzzle sets `CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, `MAKEFLAGS` and
  `MUZZLE_SLOTS` from the claim and overwrites any it inherits. Drop
  hand-set caps from the command; an assignment inside the command string
  still wins.
- **Wrap a script as a whole.** muzzle can't see inside a script, so a
  script you write that runs builds or tests (`./check.sh`, `python3
  /tmp/x.py`, a heredoc) matches no rule and runs unclaimed. Wrap the
  whole script under the rule of the heaviest thing it runs:
  `muzzle run --rule cargo-test -- ./check.sh`, or
  `muzzle run --rule cargo-test -- bash -c 'cargo build && cargo test'`.
  Don't move a gate into a script to skip the queue: the machine still
  pays for it, and every other agent's gate slows down.
- A background launch (`(gate > log 2>&1 &)`) keeps its slots: `muzzle run`
  returns at once and a keeper holds the claim until the job exits. Polling
  the log is fine. A poll that repeats the gate's command line would claim a
  second time, so poll with `tail` or `muzzle status`.

## Escape options

- **Unblocking work goes first with `--class high`**: a merge check or fix
  that other agents are waiting on. High waiters go ahead of every normal
  waiter. They never stop a running job. Agents spawned by mu
  (`MU_MANAGED_AGENT=1`) default to `normal`, humans and other callers to
  `high`. `MUZZLE_CLASS=high` in the environment does the same for every
  call in that shell.
  `muzzle run --class high --rule gate -- contrib/fast-gate`
- **Light commands run without muzzle**: status checks, reads, `git`, a
  quick script. Leave them unwrapped when wrapping by hand; a command that
  matches no rule never queues. If muzzle claims slots for a command that
  only names a gate (a `grep`, a heredoc), the rule is a loose `match` regex.
  Fix the config ([tune.md](recipes/tune.md), step 3) rather than working
  around it per call.

## Enforced or advisory

`muzzle status` and `muzzle doctor` say `enforced` or `advisory`.

- **enforced**: Linux with a systemd user manager. The job runs in a scope
  with a memory cap of the claim's share of RAM and a CPU weight. A job
  that exceeds the cap is killed and logged as `killed: "oom"`. Ask for more
  slots, or tune the rule.
- **advisory**: macOS, a container, or no user manager. Admission and the
  queue still work. Nothing caps the job, and CPU and memory figures come
  from rusage.
- A job that enters a container (`distrobox enter`, `podman exec`,
  `toolbox run`, `docker exec`) is admitted but not capped or measured, and
  is logged `enforced: false`. If the caller is killed, the job may keep
  running in the container. Its slots stay held until it exits, so find
  and kill it there.

## The config

`~/.config/muzzle/config.toml` holds global `[[rule]]` entries. A
`[path."<dir>"]` table changes rules for the repos under a directory, and
`include = ["machine.toml"]` merges a per-machine file. Example:

```toml
include = ["machine.toml"]

[[rule]]
name  = "cargo-test"
command = ["cargo test", "cargo nextest", "cargo clippy", "cargo build"]
slots = 3

[path."~/hacking/crew"]
[[path."~/hacking/crew".rule]]
name  = "cargo-test"
slots = 2              # reweight only; command is inherited
```

`command` names a program and its subcommand words, and matches it in
command position anywhere in the text: after `&&` or `|`, inside
`bash -c '…'`, past `env`, `timeout`, `npx`, `mise exec --`, and through
`distrobox enter … --`. A `grep`, a quoted argument or a heredoc never
matches. `match = '<regex>'` is the escape hatch for shapes that aren't a
command word (`docs/usage.md` in the muzzle repo has the full rules).

There is no discovery mode. To set muzzle up on a machine, write the rules
from the heavy commands you know, check them with `muzzle match`, and tune
from the run log: [recipes/tune.md](recipes/tune.md).

## Recipes

- Changing a rule's weight or pattern, reading `muzzle report`:
  [recipes/tune.md](recipes/tune.md).
- Making a trace, replaying it, the interactive probe:
  [recipes/bench.md](recipes/bench.md).
- Installing or checking pi, Claude Code, Codex, opencode, or hand-wrapping;
  harness timeouts; why a hook's `allow` skips the permission prompt:
  [recipes/harnesses.md](recipes/harnesses.md).
