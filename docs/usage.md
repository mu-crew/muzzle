# Using muzzle

`muzzle --help` and `muzzle <command> --help` are the flag reference. This
page covers the behaviour around them, and the limits. The design and its
numbered decisions (Dn) are in the [spec](specs/2026-10-29-muzzle.md).

## Config

`~/.config/muzzle/config.toml` (or `$XDG_CONFIG_HOME/muzzle/config.toml`).
The first run writes a commented template and uses its rules. A command
that matches no rule runs at once, without a claim.

```toml
reserve = 2            # budget = logical cores - reserve
# slots = 12           # an explicit budget instead

[[rule]]
name  = "cargo-test"
command = ["cargo test", "cargo nextest", "cargo clippy", "cargo build"]
slots = 2
```

- `name` is the label in `status`, reports and `runs.jsonl`. Keep it when
  you change the pattern, so history stays comparable.
- The first rule that matches wins. `command` is described below; `match`
  is the regex escape hatch.
- A claim above the budget is capped to the whole budget, so the job runs
  alone. That replaces exclusive lock files.
- An unknown key, a bad pattern, or a rule with both or neither of
  `command` and `match` exits 2 and names it, and the file it is
  in.
- `muzzle doctor` shows the config's rule count, path tables, includes and
  hash. Each run records the hash, so reports can split by config.

### Command rules

`command = "<words>"`, or a list of them, names a program and optionally
its subcommand words. muzzle tokenises the shell text and tries every
simple command in it:

- Commands after `;`, `&&`, `|`, in `$( )` and backticks, and inside
  `bash -c '…'` (any shell, any `-lc` style flags) all count.
- Assignments (`A=1`), keywords and wrappers are skipped first: `env`,
  `time`, `timeout [opts] N`, `nice`, `nohup`, `exec`, `npx [opts]`,
  `uv run [opts]`, `mise exec [opts] --`, and `flock [opts] <lock>` (or
  its `-c '…'` script).
- Container entry descends: the command after `distrobox enter … --`,
  `toolbox run [-c box]` or `podman|docker exec [opts] <ctr>` is matched
  too. The job still counts as a container job.
- A `$VAR` as the program is stepped over. A script program (`x.sh`, or
  `bash file` with no `-c`) matches as itself, and its arguments are tried
  as a command and as shell text: `dbx.sh 2400 "contrib/gate | tail"`
  matches `contrib/gate`.
- The program matches a word that equals it or ends in `/<program>`:
  `contrib/gate` matches `./contrib/gate` and `/abs/repo/contrib/gate`;
  `cargo` matches `/usr/bin/cargo`.
- Each subcommand word must equal the next word that doesn't start with
  `-` or `+`, so `cargo +nightly test` and `cargo --locked test` match
  `cargo test`. Extra words after the pattern are fine. `cargo -C dir`,
  `npm --prefix dir` and `npm -w pkg` skip the option's argument too.
- A pattern word ending in `*` matches by prefix: `npm run test*` matches
  `npm run test:fast`. There is no other glob syntax.

Quoted arguments, heredoc bodies and comments are never command words, so
`pgrep -f 'contrib/gate'`, `cat contrib/gate` and `echo "cargo test"`
claim nothing.

**The escape hatch:** `match = '<regex>'` instead of `command`. It is
`regex` crate syntax over the whole shell string, with no shell parsing,
so it also hits commands that only mention the word. Use it for shapes
that aren't a command word, such as `exec 9>>$L; flock 9`.
[`bench/linuxpc-config.toml`](../bench/linuxpc-config.toml) has one.
A rule sets exactly one of `command` and `match`. In a path table or an
include, setting either replaces the inherited one.

### Weights per repo: path tables

One rule can need different weights in different repos. A `[path."<dir>"]`
table changes rules for a directory and everything under it:

```toml
[[rule]]
name  = "cargo-test"
command = ["cargo test", "cargo nextest", "cargo clippy", "cargo build"]
slots = 3

[path."~/hacking/crew"]
[[path."~/hacking/crew".rule]]
name  = "cargo-test"
slots = 2              # only the weight changes; command is inherited

[path."~/hacking/crew/murmur"]
[[path."~/hacking/crew/murmur".rule]]
name    = "cargo-test"
enabled = false        # no claim for cargo in this repo
[[path."~/hacking/crew/murmur".rule]]
name  = "js-test"      # a new rule, only here
command = ["vitest", "npm test", "npm run test*"]
slots = 2
```

- A table applies to a command when its key is the command's **target** or
  a parent of it. The target is the repo the cwd is in (its main checkout,
  so every git worktree and jj workspace of a repo gets the same rules),
  or the cwd itself outside a repo. Keys are compared by whole path
  components: `~/hacking/crew` does not cover `~/hacking/crewmate`. A
  table can't target a subdirectory inside a repo.
- Tables apply from the outermost key to the nearest. An entry whose
  `name` is already in effect changes only the fields it sets (`command`
  or `match`, `slots`, `class`, `enabled`). A new name is a new rule: it
  must set `command` (or `match`), and it is tried before the inherited rules.
- `enabled = false` turns a rule off for that subtree. A deeper table can
  set `enabled = true` again.
- `reserve` and `slots` (the budget) can't go in a path table.
- `muzzle match --json --cwd DIR -- 'cargo test'` shows the result for a
  directory: `"source": "path"` with the nearest table that touched the
  rule in `"path"`, and where the slots came from in `"slots_from"`.

### Includes: a shared file plus a machine file

The root file can pull in other files, so one config can live in dotfiles
while each machine keeps its own weights in an untracked file:

```toml
# ~/.config/muzzle/config.toml (tracked)
include = ["machine.toml"]   # relative to this file; ~ works

[[rule]]
name  = "cargo-test"
command = ["cargo test", "cargo nextest", "cargo clippy", "cargo build"]
slots = 2
```

```toml
# ~/.config/muzzle/machine.toml (untracked)
reserve = 4
[[rule]]
name  = "cargo-test"
slots = 4              # this machine's global weight
```

- Includes are merged after the root, in list order. `reserve` and `slots`
  replace the root's. A global rule with a name already in effect changes
  only the fields it sets; a new name is appended to the global list and
  must set `command` (or `match`). Path
  tables with the same directory merge by rule name.
- A missing include is skipped, and `doctor` lists it as `not found`. Any
  other read, parse or validation error exits and names the file.
- An included file can't set `include` itself.

Check a rule with `muzzle match -- '<command>'` on both a real call and a
poll of it. [`bench/linuxpc-config-021.toml`](../bench/linuxpc-config-021.toml)
has worked `command` rules, and
[tune.md](../npm/muzzle/skills/muzzle/recipes/tune.md) covers the loop for
changing weights.

## Running a command

```sh
muzzle run --rule cargo-test -- cargo test --locked
```

- `CMD ARGS…` runs as argv. For shell syntax, wrap a shell:
  `muzzle run --rule gate -- bash -c 'cd repo && contrib/gate'`.
- `--rule NAME` takes its weight from the config. `--slots N` overrides
  it, and then a config change no longer reaches that call.
- The class is `--class`, else `MUZZLE_CLASS`, else the rule's `class`,
  else `normal` when `MU_MANAGED_AGENT=1` and `high` otherwise. High
  waiters go ahead of normal ones and never pre-empt a running job.
- muzzle sets `MUZZLE_SLOTS`, `CARGO_BUILD_JOBS`, `RUST_TEST_THREADS` and
  `MAKEFLAGS=-jN` from the claim, overwriting inherited values. An
  assignment inside the command string still wins.
- A `muzzle run` inside a claimed job (a gate script calling `cargo test`
  through muzzle) is nested: it starts at once and claims nothing. Nesting
  is found through process ancestry, or through the `MUZZLE_HOLD` variable
  muzzle sets for its job, which also crosses into containers.

### Timeouts

**Never put `timeout` outside `muzzle run`.** It counts the queue wait and
can kill the command before it starts. Use the flags:

| flag | counts | on expiry |
| --- | --- | --- |
| `--run-timeout S` | from admission | the job's group (a nested run: its own process tree; on macOS not a descendant orphaned before the deadline, which the outer claim still covers) gets SIGTERM, then SIGKILL after 10 s; exit 124 |
| `--timeout S` | while queued | muzzle gives up, nothing runs; exit 75 |

Waiting has no limit by default, and a progress line prints every 30 s.
A waiter killed by SIGTERM or SIGINT prints `muzzle: killed while queued
(…)`, which names the usual cause: a harness or outer `timeout` counting
the wait.

Signals muzzle catches (HUP, INT, QUIT, TERM) go on to the job. A signal
the caller ignores stays ignored for muzzle and the job, so `nohup muzzle
run …` survives a hangup.

### Exit codes

| code | meaning |
| --- | --- |
| the command's own | passed through |
| 128 + signal | the command was killed by a signal |
| 124 | `--run-timeout` fired |
| 75 | `--timeout` fired while queued |
| 2 | bad usage or config |
| 125 | muzzle itself failed |

## Seeing what's happening

- `muzzle status` shows the budget, the holders (agent, rule, slots, run
  time, pid) and the queue. `--json` is a stable interface: fields are only
  ever added.
- `muzzle doctor` checks the budget, enforcement, the state directory, the
  config, the run log and each harness's setup. Exit 0 means usable, even
  when only advisory.
- `muzzle report` summarises the run log: machine figures, per-rule rows
  and a suggested weight per rule. `--compare A..B C..D` puts two periods
  side by side. `--by repo` gives a row per rule and repo, each with the
  slots the current config gives that repo and where they come from
  (`now 2 (from [path."~/hacking/crew"] in config.toml)`). A repo of
  `(none)` means runs outside any repo. `--by repo` and `--compare` can't
  be combined.

### The run log

Every admitted run appends one line to
`~/.local/state/muzzle/runs.jsonl` (or `$XDG_STATE_HOME/muzzle/`).

- Nothing is deleted. When a new month starts (UTC), or the file passes
  16 MB, its lines move into a gzip archive beside it,
  `runs-YYYY-MM.jsonl.gz`. The live file holds at most about a month,
  roughly 3–8 MB at a busy machine's rate, and the archives grow by a few
  MB a year. `zcat` and `jq` read them.
- `report` reads the archives as well as the live file, so its default
  period is the whole history. Use `--since` to narrow it.
- `doctor` counts the runs in the live file and the newest archive, shows
  the archives, and warns when the live file passes 32 MB (rotation is
  failing) or the archives pass 256 MB. A rotation that fails prints one
  `muzzle: runs.jsonl rotation failed` line and the run still logs.
- The log is local state and records working directories, so don't
  publish it.

### Unclaimed load

Each run line also records `load1`, the 1-minute load average at
admission, and `held`, the slots other jobs held at that moment. From
runs that carry both, `report` prints
`unclaimed load: p50 X, p90 Y cores` (once at least 20 runs have them):
`load1 − held`, never below 0. It estimates how much of the machine's load
comes from work muzzle doesn't see, such as heavy commands that match no
rule or builds hidden inside scripts.

It is a rough figure. The load average lags by about a minute, and on
Linux it also counts processes waiting on disk I/O, so a busy disk raises
it without any CPU use. Read it as a trend, not a measurement.

## Harnesses

pi, Claude Code, Codex, opencode and hand-wrapping:
[harnesses.md](../npm/muzzle/skills/muzzle/recipes/harnesses.md), with the
snippets in [docs/harnesses/](harnesses/README.md).

**A hook's `allow` skips the harness's permission prompt.** Codex hooks
need `permissionDecision: "allow"` to rewrite a command, so `muzzle hook
codex` approves every matched call, compound commands included:
`cargo build && curl … | sh` matches a cargo rule. `muzzle hook claude`
sends `allow` only when the raw command text is plain words: letters,
digits and `_ . / : = , + @ % -`, separated by single spaces. Any quote,
backslash, `$`, brace, glob, redirection (`2>&1`), operator, tab or
newline means no `allow`, because muzzle's lexer can't be proven to read
quoting as bash does (spec 0.2.1 R5). Then it must be one simple
command whose own command word (after assignments and `env`, `time`,
`timeout [opts] N` or `nice`) is the rule's program, or `./` plus it, followed by its subcommand words, and
only when every word after that is on the program's allowlist (spec 0.2.1
R5): `cargo test -p x -- --nocapture` and `cargo clippy --all-targets --
-D warnings` are allowed, but `npm test` takes no extra words, `cargo
nextest run --config-file x` and `cargo build --target x.json` are not
listed, and a program without a list (`contrib/gate`) takes only the
pattern's words. An unknown word means no `allow`; the command is still
wrapped. A path elsewhere doesn't count (`/tmp/evil/cargo test`), and neither does
a match through a wrapper, a container, a script or a variable
(`npx vitest run`, `mise exec -- cargo test`): those are wrapped, and the
decision is left to Claude. For a `match` rule, the regex must match at
the command word, and the words after the program go through the same
allowlist. A compound command, or a pattern found only in an
argument or a comment (`echo cargo test`, `rm -rf ~/x # cargo test`),
goes to Claude's own permission check. Keep `Bash(muzzle run:*)` out of Claude's
allow rules, which would approve every wrapped string.

## Limits

### Claims follow the process group

A claim belongs to the process group the job runs in, and lasts until that
group has no members left.

- **muzzle leads its group** when it is the only, or the last, simple
  command bash execs: pi's `bash -c 'muzzle run …'` execs muzzle on
  any bash, and `bash -c 'cd repo && muzzle run …'` does on bash 4.4
  or later. The job then runs in muzzle's own group, so a harness that kills the group (pi
  on Esc or timeout) kills the job too, and the next waiter frees the
  slots.
- **muzzle is not the leader** when it is piped (`muzzle run … | tail`),
  followed by another command (`muzzle run …; true`), the last command
  of an `&&` list under bash older than 4.4 (macOS `/bin/bash` is 3.2,
  and forks it), typed into an
  interactive shell after another command, or spawned as a child by a
  wrapper. Put the pipe inside instead:
  `muzzle run --rule gate -- bash -c 'contrib/fast-gate 2>&1 | tail -50'`. It then starts the job in a new group of its own. **Killing the
  caller may leave the job running**, still holding its slots until it
  exits. The job runs in the terminal's background: Ctrl-C and Ctrl-Z
  reach the caller, and muzzle forwards Ctrl-C to the job. A job that
  reads the terminal or changes its settings can't run like this, so
  muzzle prints `muzzle: the command stopped waiting for the terminal…`,
  ends the job (SIGTERM, SIGKILL after 10 s), frees its slots and exits
  128 + the stop signal (149 for SIGTTIN). Run muzzle as the first
  command, or give the job no terminal (`< /dev/null`). muzzle only
  sees this when its top command stops too: a top command that ignores
  the terminal stop signals, like `timeout` in `muzzle run -- timeout N
  …`, keeps running while the job under it waits, so there is no
  `muzzle:` line and the slots stay held until `timeout` ends it. The npm `bin/muzzle` shim execs the binary for this reason (Node
  22.15 or later), and the pi extension calls the platform binary
  directly.
- **Background children stay in the group.** `(gate > log 2>&1 &)` keeps
  its slots: `muzzle run` returns at once with the top command's status,
  and a detached keeper holds the claim until the group is empty, then
  writes the run line. On macOS that run's CPU and memory are `null`.

### `setsid` and daemons leave the group

A job that calls `setsid` or daemonizes leaves its group, and muzzle stops
counting it: the slots are freed while it still runs. muzzle doesn't track
such jobs.

### Advisory mode

`status` and `doctor` say `enforced` or `advisory`.

- **Enforced** needs Linux with a reachable systemd user manager. The job
  runs in `systemd-run --user --scope` with `MemoryMax` of the claim's share
  of RAM (RAM / cores × slots, no swap) and a `CPUWeight` scaled to the
  claim. A job killed at the cap is logged `killed: "oom"`, and `report`
  raises that rule's suggestion.
- **Advisory** is macOS, a container, or Linux without a user manager.
  Admission, classes and the queue work the same. Nothing caps memory or
  CPU, and the figures come from rusage.

### Containers

A job started through `distrobox enter`, `podman exec`, `toolbox run` or
`docker exec` runs in the container's own cgroup, outside muzzle's scope.

- **Admission still works:** slots, queue, classes and nesting. Host and
  container share one queue when both see the same `~/.local/state`.
  distrobox resets `XDG_*` inside the container, so a custom
  `XDG_STATE_HOME` on the host splits the queue.
- **Enforcement does not:** no memory cap or CPU weight reaches the job,
  and its CPU and memory aren't measured. muzzle detects a container entry
  in the command and records `enforced: false` with reason `container` in
  `runs.jsonl`, `--report` and `status`. `doctor` names the rules
  affected, and `report` marks them `not measured (container)` and
  suggests nothing from them.
- **Cleanup on a kill does not:** when the caller's group is killed, the
  in-container job keeps running, because the container runtime starts it
  in its own session. muzzle keeps the slots held until the job's scope
  has no processes left, so the leaked job waits its turn instead of
  overloading the machine. Kill it in the container to free them.
- Run muzzle on the host, around `distrobox enter …`, not inside the
  container: inside, there is usually no systemd user manager, and its
  core and RAM reading can differ from the host's. The first reading is
  stored for the shared queue, and `doctor` warns when its own differs.
