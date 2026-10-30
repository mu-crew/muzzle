# Tune muzzle's weights

The loop that turns `runs.jsonl` into weights. muzzle never edits its
config. You do, one rule at a time, and you keep a change only when the
numbers agree. Every run records the config hash, rule and slots it ran
under, so `report --compare` can split by them later.

Run on the host whose config you are changing. `runs.jsonl` is local machine
state: read it here, and never publish it.

## 0. A new machine: write the config, then trial it

There is no discovery mode. Start from what you know:

1. List the heavy commands agents run here: gates, `cargo test`, `npm test`,
   e2e suites. museum helps (step 2).
2. Write a `command` rule for each, in the shared file, with a weight
   from the command's usual job cap (`CARGO_BUILD_JOBS=4` means 4):
   `command = "contrib/gate"`, `command = ["cargo test", "cargo build"]`.
   Check each pattern with `muzzle match -- 'contrib/gate --full'`.
3. Put this machine's budget and weights in an untracked `machine.toml` next
   to it (layout in step 3), so one shared file serves every machine.
4. Run real work for a day or two, then start the loop at step 1.

## 1. Read the report, machine figures first

```sh
muzzle report --since 7d
```

Read the machine section before the rows:

| figure | high means |
| --- | --- |
| blocked-idle (slot-s) | slots sat free while someone waited: the head of the queue is too big to fit, so everything behind it waits (strict FIFO). Shrink the over-sized rule |
| CPU efficiency (low) | claims are bigger than the jobs use: weights too big |
| utilisation near 100% while the machine's CPU stays low | the budget is saturated but the cores aren't: the reserve is too big |
| queue non-empty | how often anyone waited at all. Near 0% means the weights cost nothing yet |
| unclaimed load (cores) | load that no claim accounts for: `load1 − held` at admission. High means heavy work runs outside muzzle. Find the unmatched heavy commands (step 2) and add rules, or raise `reserve` to leave room for them. It is rough: the load average lags, and on Linux it counts I/O wait |

Then the rows. `suggest` names its evidence and carries a tag:

- `N measured (p90 cores X over K clean runs)`: from at least 5 clean runs
  (exit 0, not killed). Memory can only raise it.
- `try N experiment (saturated at M)`: cores p50 is at the weight, so the
  job may want more. Untested until step 5 confirms it.
- `N oom (…)`: the scope killed runs at the memory cap. This covers their
  peak plus 25%.
- `insufficient data (K runs)`: fewer than 5 clean runs. Wait for more.
- `not measured (container)`: those runs entered a container, so muzzle
  saw no CPU or memory. No suggestion can come from them.

Narrow to one rule with `--rule NAME`, or to a period with `--since D`
and `--until D` (`now`, `7d`, `12h`, `30m`, `YYYY-MM-DD`, or RFC 3339).

### Per repo

One rule can behave very differently per repo: a `cargo test` that takes
1 s in one repo can take 30 s in another. Split the rows by repo:

```sh
muzzle report --by repo --since 7d --rule cargo-test
```

Each row is one (rule, repo); `(none)` is runs outside any repo. The
`now` column is the slots the **current** config gives that repo, and
where they come from: `now 3 (from [path."~/hacking/crew"] in
machine.toml)`. When the runs used a different weight, it adds `; runs used
4`. `not in the current config here` means the rule is off or absent for
that repo now, so there is no suggestion. When suggestions for one rule
differ a lot between repos, give the outliers a path table (step 3).
`--by repo` can't be combined with `--compare`.

**Done when** you have written down, for the rule you will change, its
current slots, run p50/p90, cores p50/p90 and the suggestion.

## 2. Find heavy commands no rule matches

`report` can't see these: an unmatched command never reaches muzzle. Look
in museum:

```sh
museum-search calls --min-secs 30 --tool bash
```

Check a candidate against the live config with
`muzzle match -- 'contrib/gate --full'`. It prints the rule, or `no rule
matches`.

Heavy work inside a script (`./run.sh`, `python3 /tmp/x.py`) matches no
rule, because muzzle only sees the script's name and arguments. A high unclaimed load
with no unmatched commands in museum usually means this. The fix is
wrapping such scripts with `muzzle run --rule cargo-test -- ./run.sh`
(SKILL.md), not a rule for every script name.

## 3. Edit the config

`~/.config/muzzle/config.toml`. Change one rule per round, so the next
report shows what that change did.

Where a change goes:

```toml
# ~/.config/muzzle/config.toml: shared, tracked in dotfiles
include = ["machine.toml"]

[[rule]]
name  = "cargo-test"
command = ["cargo test", "cargo nextest", "cargo clippy", "cargo build"]
slots = 3

[path."~/hacking/swayward-wm"]          # every repo under here, every worktree
[[path."~/hacking/swayward-wm".rule]]
name  = "cargo-test"
slots = 6                               # reweight only; command inherited
```

```toml
# ~/.config/muzzle/machine.toml: this machine only, untracked
reserve = 2
[[rule]]
name  = "cargo-test"
slots = 4                               # this machine's global weight
```

- **A weight for every repo on this machine**: the global rule in
  `machine.toml`.
- **A weight for some repos**: a `[path."<dir>"]` table with just `name`
  and `slots`. The key is a directory; it covers every repo under it and
  all their worktrees. A nearer table wins over a further one.
- **A rule that should not fire in one repo**: `enabled = false` in its
  table.
- After the edit, `muzzle match --json --cwd ~/hacking/swayward-wm/swayward
  -- 'cargo test'` shows the slots there and `slots_from`, the file and
  table they came from.

- **Weights**: set `slots` to the suggestion. For an `experiment`, that
  is the `try N` value, and step 5 decides whether it stays.
- **New rules**: give each a `name`. The name is the label in reports and
  history, so keep it when the pattern changes.
- **False positives**: a `command` rule matches only a command word, so a
  heredoc, a `grep`, a `pgrep -f` poll or a `cat` of the script claims
  nothing. A `match` regex (the escape hatch, for shapes that aren't a
  command word) hits anywhere in the string, and a poll it claims holds
  the gate's slots a second time while the real gate runs. Prefer
  `command`; `bench/linuxpc-config-021.toml` in the muzzle repo has
  worked `command` rules. Test a real call and a poll:

  ```sh
  muzzle match -- 'cd repo && contrib/fast-gate'
  muzzle match -- 'pgrep -f contrib/fast-gate'
  ```

Check the edit loads, and note the new hash:

```sh
muzzle doctor
```

A bad config exits 2 and names the key and the file. `doctor` also lists
each include as `found` or `not found`: a typo in an include path is
skipped silently otherwise.

**Hand-wrapped calls carry their own weight.** A `--slots` on
`muzzle run` overrides the config, so editing the config changes nothing for briefs,
scripts or cron lines that pass `--slots`. Update those too, or change them
to `--rule NAME` alone, which reads the weight from the config. The pi
extension and the harness hooks read the config on every call. Renaming a
rule breaks every `--rule OLD`: those calls warn `rule OLD is not in the
config; claiming 1 slot(s)` and under-claim until they are updated.

## 4. Check it on a replay

Before living with the change, replay a recent trace at tier 1 against the
old and the new config. The trace's busiest window is where weights matter.
See [bench.md](bench.md) for making a trace.

```sh
muzzle bench run bench/traces/2026-10-21.jsonl --mode muzzle --tier synthetic --window busiest:3h --speed 10 --out /tmp/tune-after
```

Run the same command with the old config in place and `--out
/tmp/tune-before`, then compare the two `summary.md` files: makespan, wait
p90, most slots held and the probe. A tier-1 replay burns CPU for each
rule's slots, so it shows queueing and contention, not whether the real
job wants more cores. Step 5 settles that.

## 5. Keep it, then compare live windows

After a few days, or at least 5 clean runs at the new weight:

```sh
muzzle report --compare 2026-11-01..2026-11-03 2026-11-04..now --rule cargo-test
```

For a path-table change, compare per repo instead: run
`muzzle report --by repo --since 2026-11-04 --rule cargo-test` and check
the repo's row against the one you wrote down in step 1.

The output shows both periods, split by config hash when it changed inside
one, then an `A -> C` table of deltas. An experiment reads `confirmed` when
run p50 fell, `rejected` when it didn't.

Rules for reading it (from tuning muzzle's own gate, 2026-10-29):

- **Compare windows only after the last change to the gated code.** A suite
  that grew between the windows moves run time on its own. In that dogfood
  run, run p90 went from 34 s to 58 s while the weight went down, because
  the suite had grown.
- **Prefer cores over wall time.** Cores used (`cpu_s / ran_s`) says what
  the job can use. Wall time also measures the code and the machine.
- **Change a weight only when two windows agree.** Both must come after the
  last suite change. One window's suggestion moves with the code. The same
  rule suggested 2 over one hour and 4 over the next, so a weight tuned
  from one window oscillates.

Revert when the change made p90 or the wait worse and the code didn't move.

## 6. Say why, next to the rule

Add one comment line above or below the edited rule: the evidence and the
date.

```toml
[[rule]]
name  = "cargo-test"
command = "cargo test"
slots = 3
# why: 4 -> 3, 2026-10-29: p90 cores 2.2 over 10 clean runs; run p50 22 -> 21 s at 3.
```

**Done when** the rule's comment names the before and after figures, and
every hand-wrapped `--slots` for that rule matches the config or is gone.
