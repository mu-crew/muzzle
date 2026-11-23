---
name: muzzle
description: Per-host admission for heavy commands (gates, builds, test suites). Use when a bash result carries a `muzzle:` line (queued, admitted, waited/ran, gave up, killed while queued), when wrapping a heavy command by hand with `muzzle run`, when tuning muzzle's weights or benchmarking it, or when hooking muzzle into a harness.
---

# muzzle

muzzle queues each heavy command for a weighted share of this machine's slot
budget (1.5 slots per core, minus a reserve), runs it, and logs what it used. Rules in
`~/.config/muzzle/config.toml` decide which commands claim slots and how
many. A command that matches no rule runs at once, unclaimed. There is no
daemon: every waiting muzzle re-checks the queue itself. `muzzle <cmd> --help`
is the reference for flags; this skill covers what the help can't tell you.

In pi, the muzzle extension wraps matching bash calls for you. Elsewhere you
wrap by hand, or a harness hook does it.

## Reading muzzle's lines

| line (stderr, or appended to a pi result) | means | do |
| --- | --- | --- |
| `muzzle: queued 3/5, 14/14 slots held (worker-7 gate 6, reviewer-1 js-test 4 [dead], worker-3 gate 4)` | waiting; printed every 30 s. Holders read `agent rule slots`; `[dead]` or `[kept]` marks an orphaned claim (below) | wait. The command has not started. If the `[dead]`/`[kept]` holder is yours, `muzzle drop` it |
| `muzzle: admitted after 312s (6 slots)` | the wait ended, the command starts now | nothing |
| `muzzle: waited 312s for 6 slots, ran 241s` | the run finished; the exit status is the command's own | judge the command by its output, not by the wait |
| `muzzle: nested in an outer claim, ran 4s` | it ran inside another claim and took no slots | nothing |
| `muzzle: gave up after 60s queued (position 2/4, --timeout)` | exit 75: `--timeout`, `MUZZLE_QUEUE_TIMEOUT` or `queue_timeout` expired before admission, nothing ran; the holders are listed | retry later, or `--no-timeout` |
| `muzzle: warning: same pane (%12 / reviewer-2) already holds 4 slots (ticket 42, …)` | you already hold a claim from this caller (same `TMUX_PANE`; without panes, same `MU_AGENT_NAME` and cwd or repo). The warning prints for non-nested calls, even if admitted at once; it is not a refusal | don't start a second heavy command behind your own claim |
| `muzzle: refusing to queue behind own orphaned claim (ticket 42, …)` | exit 76: on the first pass the call needs more than is free, but would fit if your own `[dead]`/`[kept]` slots were freed; nothing ran. A live claim of yours alone never causes 76 | `muzzle drop 42` (or `muzzle drop --kill 42`), then retry; `--allow-concurrent` (or `allow_concurrent = true`) skips the refusal, not the warning |
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
- `cd repo && muzzle run …` leads the group only on bash 4.4 or later,
  which execs the last command of an `&&` list. Older bash, including
  macOS `/bin/bash` 3.2, forks it, so Esc or a pi timeout leaves the job
  running. No `command` rule matches this form, so pi leaves it alone and
  its flags apply. A `match` regex rule that hits text inside the call
  wraps it again, and the inner run is then nested (D17): it claims
  nothing, so its `--class` and `--slots` do nothing, but its
  `--run-timeout` still fires. Starting the call with `muzzle` avoids
  both.
- **`timeout` goes inside `muzzle run`, never outside it.** An outer
  `timeout` counts the queue wait and kills the gate before it starts. Use
  `--run-timeout S`, which counts from admission and exits 124.
- In pi, a command that already runs muzzle (`muzzle run …` by any
  path, or `cd repo && muzzle run …`) is left alone by the extension, so pi's own bash `timeout` still counts the wait. Give a
  hand-wrapped call no pi timeout, and put the limit in `--run-timeout`.
- muzzle sets `CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, `MAKEFLAGS` and
  `MUZZLE_SLOTS` from the claim and overwrites any it inherits. Drop
  hand-set caps from the command; an assignment inside the command string
  still wins.
- **Heredocs and throwaway scripts are inspected automatically.** Executed
  shell heredocs (`bash <<'EOF'`, `cat > /tmp/x.sh <<'EOF' … && bash
  /tmp/x.sh`), local shell scripts (`./check.sh`), and Python/Node scripts
  that spawn subprocesses or test harnesses match their rule (or the
  built-in `script-heavy` fallback, default 2 slots); plain file-edit
  scripts stay unclaimed. For opaque CPU burners that match no rule, set
  `grace_s` in the config (or `MUZZLE_GRACE=5` in pi) so anything burning
  CPU past the grace window retroactively claims slots as `unmatched-heavy`.
  Hand-wrapping (`muzzle run --rule cargo-test -- ./check.sh`) is still
  useful when you want a specific rule or weight up front.
- **Prefer foreground `muzzle run --run-timeout 900` over `&` or `nohup`.** A
  background launch (`(gate > log 2>&1 &)`) keeps its slots: a keeper holds
  the claim while the job runs, until it exits or the orphan idle/TTL release
  frees it. Starting a second heavy command while it
  runs may be admitted at once if slots are free; if short, it risks waiting behind your own claim or exit 76. Poll the
  log with `tail` or `muzzle status`; a poll that repeats the gate's command
  line claims a second time. A second non-nested heavy call from the same caller warns (a nested run does not); it may be admitted at once, waits if slots are short, and exits 76 only when your own orphaned claim is what blocks it.
- **Never wrap `muzzle run` in a subshell or pipe** (`(muzzle run … | tail)`):
  muzzle then isn't the group leader, and a timeout can SIGKILL it and
  leave the job holding slots. Put the pipe inside
  `bash -c '… | tail'` so muzzle leads and tears the job down on timeout.
- A queue wait can be bounded by `MUZZLE_QUEUE_TIMEOUT` (seconds, or `none`)
  or `queue_timeout` in the config; the env var wins over the config, and
  `MUZZLE_QUEUE_TIMEOUT=none` or `--no-timeout` waits indefinitely despite a
  configured limit. `--timeout S` overrides both. Expiry exits 75.
  `--timeout 0` tries once: if admission succeeds the command runs; exit 75
  means that immediate admission failed. No limit is built in; 900 is the
  recommended setting for managed agents.

## Stuck claims

`muzzle status` shows `#ticket`, and `[dead-muzzle …]` or `[kept …]` with the
surviving processes for an orphaned claim (muzzle died, or a keeper holds a
leaked group). Muzzle may release the claim itself, **even while the
processes still run**: after 120 s idle (`orphan_idle_s`, readable CPU
samples under 1% of one core and no new members) or 600 s (`orphan_ttl`,
whatever the CPU, busy or unreadable; with unreadable CPU only the TTL applies). Both are defaults, 0 disables each,
and they are checked when someone admits or the keeper polls. Don't wait on
it:

```sh
muzzle drop 42            # one orphaned claim; a live one exits 2 without --force
muzzle drop --dead        # every dead/kept claim
muzzle drop --agent NAME  # dead/kept claims of NAME
muzzle drop --kill 42     # also SIGTERM, up to 5 s, SIGKILL verified survivors
```

Recover with `drop`, never by editing `state.json` or skipping muzzle. Plain
`drop` frees the slots but leaves the processes running. `--kill` signals
only current-user group members verified by pid and start time (not drop's
own group); for a container scope it stops the scope if reachable, otherwise
it releases the claim and warns the workload could not be stopped.

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
  running in the container. Its slots stay held until it exits or an orphan
  idle/TTL release (above) frees them, so find and kill it there.

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
