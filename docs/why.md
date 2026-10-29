# Why muzzle exists

## Every gate sizes itself for the whole machine

A crew of agents on one machine runs gates, builds and test suites at the
same time. `cargo`, `pytest -n auto` and `vitest` each assume the machine is
theirs and start one job per core. Four of them at once on 16 cores run 64
jobs, so each one is slower, the interactive side stutters, and timing
tests flake.

```
  16 cores, no admission                 16 cores, muzzle (budget 14)
  +-------------------------------+      +-------------------------------+
  | gate A    16 jobs             |      | gate A     6 slots  running   |
  | gate B    16 jobs             |      | test C     2 slots  running   |
  | test C    16 jobs             |      | test D     2 slots  running   |
  | test D    16 jobs             |      | gate B     6 slots  queued    |
  | your shell, the editor        |      | reserve 2: shell, editor      |
  +-------------------------------+      +-------------------------------+
    64 runnable jobs on 16 cores            each admitted job gets its share
```

On linuxpc (16 cores, 31 GB) during an October fan-out, museum's session
archive shows gate-like calls going from a median of ~15 s to 35–89 s, with
p90 from ~100–150 s to 600–830 s, and load averages of 21 to 46 logged in
sessions. The fallout was a lock-screen e2e "timing failure under load
average ~25", a perf audit whose timings were noise, and a TUI dropping
~40% of keypresses at load 21. Sources and dates are in the
[spec](specs/2026-10-29-muzzle.md#problem).

The caps in use before muzzle were prose in briefs: `CARGO_BUILD_JOBS=4`
for workers, 6 for the orchestrator, 2 for reviewers, and "never run two
gates at once". Nothing measured whether those numbers were right, and the
orchestrator's merge check once waited behind the workers' slots until it
was given a reserved one by hand.

## What muzzle does instead

- **A slot budget per host.** Logical cores minus a reserve (2 by
  default), one slot per core. A heavy command claims N slots from a rule
  in the config, waits until they are free, then runs with its parallelism
  set from the claim (`CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`,
  `MAKEFLAGS`). On Linux with systemd it also runs under a memory cap and a
  CPU weight sized to the claim.
- **Two classes.** High waiters (humans, the orchestrator) go ahead of
  normal ones (mu's workers and reviewers), so the work that unblocks
  others goes first. Within a class it is strict first-come, first-served.
- **No daemon.** State is one locked JSON file. Every waiting muzzle
  re-checks liveness and admits whoever fits, so the queue works with no
  muzzle process alive between commands. The one exception is a small
  keeper that holds a background job's claim until the job exits.
- **A run log that tunes itself.** Every admitted run appends what it used
  (CPU seconds, peak memory, wait, run time) to `runs.jsonl`.
  `muzzle report` turns that into a suggested weight per rule, with its
  evidence. You edit the config, and `report --compare` tells you whether
  the change helped. The weights start as guesses; the log is how they stop
  being guesses.
- **A benchmark that replays real days.** `muzzle bench run` replays a
  day's heavy calls from a trace, without muzzle (today's caps) and with
  it, while an interactive probe measures how the machine feels.

## The trade

A queue doesn't make the work smaller. When the machine is oversubscribed,
muzzle turns contention into waiting. The intent is that each job, once
admitted, runs closer to its uncontended time and the machine stays
responsive, while the whole batch may take longer. leasehold, a tool with the same admission model,
measured the same work taking up to 1.5× as long in return for a
responsive machine. Whether muzzle's trade is worth it on a real crew is
measured, not assumed: see the README's "Measured" section.

## What it isn't

- Not a job queue that owns your output: the command runs in your
  terminal, in your process group, and its output is yours.
- Not placement across hosts. That is a later step that reads
  `muzzle status --json` from each host.
- Not load-aware. Admission counts claimed slots, not the load average:
  a command that matches no rule runs unclaimed and unseen.
