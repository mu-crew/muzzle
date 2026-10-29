# Benchmark muzzle on a real day

Make a trace from museum, check it, replay it without and with muzzle, and
read the summary. Run from a clone of the muzzle repo: the trace tools live
in its `bench/` directory, and `bench/README.md` there is the full
reference.

A trace is the one artefact here that gets published, so the sensitivity
rules come first.

## Sensitivity rules

- **linuxpc only.** The importer reads only `linuxpc/` session files and
  refuses other hosts. The checker rejects a trace that names any other.
- **No command text, paths, names or other hosts in a trace.** Rows keep
  only allowlisted fields. Agents become `a1, a2, …` and projects
  `p1, p2, …`. `--keep-project NAME` keeps a name you have already made
  public, and it also needs a `project NAME` line in
  `bench/allowed-labels.txt`.
- **The sidecar stays local.** The mapping from each row back to its command
  goes to `$XDG_STATE_HOME/muzzle/bench/<trace>.private.jsonl`. It is never
  committed, and the pre-commit hook refuses it.
- **`runs.jsonl` is never published**, including the one a replay writes
  under `--out`. `result.json` and `summary.md` carry no paths or command
  text, and are the files that go in `docs/bench/`.

## 1. Make the trace

Install the pre-commit hook once per clone:

```sh
ln -s ../../bench/pre-commit "$(git rev-parse --git-common-dir)/hooks/pre-commit"
```

Then import one UTC day:

```sh
bench/from-museum --date 2026-10-21
```

Rule labels come from your live config, through `muzzle match` run
locally. Waits and polls are labelled `light`, and an unmatched call of
30 s or more is `unmatched-heavy`. A rule name missing from
`allowed-labels.txt` stops the import: add a `rule NAME` line and re-run.

Measured durations exist only from 2026-10-17. Earlier days are
estimated, so use them as a workload to replay and not as a baseline. Each
row's `dur_src` says which it is.

## 2. Check it

```sh
bench/check-trace
```

With no arguments it checks every file under `bench/traces/`. Errors name
the line, field and kind, never the value. Before committing a first batch
of traces from a new source, have one read-only refuter look for
identifying timing or label patterns the schema would let through.

**Done when** `check-trace` passes and nothing under `bench/` but
`traces/*.jsonl` is staged.

## 3. Tier 1: synthetic, anywhere

CPU burners sized to each call's rule. Cheap and repeatable, and runs on
macOS:

```sh
muzzle bench run bench/traces/2026-10-21.jsonl --mode none --tier synthetic --window busiest:3h --speed 10 --out /tmp/b-none
muzzle bench run bench/traces/2026-10-21.jsonl --mode muzzle --tier synthetic --window busiest:3h --speed 10 --out /tmp/b-muzzle
```

- `--mode none` is today's practice: every call starts at its offset with
  the role caps as `CARGO_BUILD_JOBS` (orchestrator 6, worker 4, others 2)
  and no queue.
- `--mode muzzle` runs every matched call through `muzzle run` with your
  current config and a private state directory under `--out`.
- `--window busiest:3h` replays the 3 hours with the most matched
  call-seconds. `--speed 10` compresses time tenfold. Use `--speed 1` for
  figures you intend to quote.
- Run nothing else heavy meanwhile: a replay measures the whole machine.

## 4. Tier 2: real commands, linuxpc

Copy `bench/real.toml.example` to `bench/real.toml` (gitignored) and fill
in the repo, pinned commit, worktree directory, setup command and one
command per rule label. Each replayed agent gets its own worktree, built
once before the replay so both modes start equally warm.

```sh
muzzle bench run bench/traces/2026-10-21.jsonl --mode none --tier real --window busiest:3h --out /tmp/r-none
muzzle bench run bench/traces/2026-10-21.jsonl --mode muzzle --tier real --window busiest:3h --out /tmp/r-muzzle
```

A real-speed busiest window takes about 3 hours per mode. Run it
overnight.

## 5. Read `summary.md`

Each `--out` directory has `summary.md` (and `result.json` with the same
figures). Put the two modes side by side:

- **makespan and throughput**: the cost. A queue can make the whole window
  take longer.
- **wait p50/p90 and run p50/p90**: where the time went. Under muzzle,
  waiting replaces contention.
- **most slots held**: never above the budget under muzzle.
- **git status p95 and 10 ms sleep late p99**: the interactive probe. This
  is what the queue buys.
- **load1 p50/max and OOM kills.**

Report slowdowns as plainly as gains.

## The probe on its own

`muzzle bench probe` samples the same interactive figures without a
replay, one JSON line per sample. For a week of live use, a cron line every
5 minutes:

```crontab
*/5 * * * * $HOME/.local/bin/muzzle bench probe --out $HOME/.local/state/muzzle/bench/probe.jsonl
```

By hand: `muzzle bench probe --samples 12 --interval 5`.
