# Tier-1 benchmark, muzzle 0.1.0

Recorded results, with no pass/fail threshold (spec D32). The verdict on
whether muzzle helps comes from the proof: the tier-2 replay plus the rollout
week, written up in `docs/eval/`.

- **Traces:** `bench/traces/2026-10-21.jsonl` and `2026-10-08.jsonl`.
- **Window:** `busiest:3h` (10-21 from 06:58Z; 10-08 from 15:38Z).
- **Speed:** 12, so each 3 h window replays in about 15 min.
- **Config:** `bench/linuxpc-config.toml`, hash b7e2bb.
- **Machine:** linuxpc, 16 cores, budget 14, run while it was otherwise
  quiet (load about 1.4 before the first run).

**Tier 1 is synthetic.** Each call is a CPU burner sized to its rule and
burning for `dur_s / speed`:
- `none` uses min(rule slots, role cap) threads;
- `muzzle` uses its claim.

There is no real I/O, no linker and no memory pressure, so these numbers
show admission behaviour, not real gate times.

| figure | 10-21 none | 10-21 muzzle | 10-08 none | 10-08 muzzle |
|---|---|---|---|---|
| calls (light skipped) | 79 (721) | 79 (721) | 121 (620) | 121 (620) |
| makespan | 915 s | 915 s | 928 s | 1029 s (+11%) |
| throughput | 311/h | 311/h | 470/h | 423/h |
| wait p50/p90 | 0/0 s | 0/0 s | 0/0 s | 0/52 s |
| run p50/p90 | 4.3/20.1 s | 4.3/20.2 s | 13.0/75.5 s | 14.0/70.0 s |
| most slots held | 12 | 14 (budget 14) | 24 | 14 (budget 14) |
| git status p95 | 3.5 ms | 5.4 ms | 9.2 ms | 9.0 ms |
| 10 ms sleep late p99 | 0.49 ms | 0.43 ms | 1.46 ms | 1.02 ms |
| load1 p50/max | 2.7/7.0 | 5.1/14.8 | 12.6/22.8 | 12.4/17.4 |

## What this shows, and what it doesn't

- **10-21 (a measured, moderate day):** nothing queued in either mode, and
  makespan and run times are identical. muzzle holds more slots at peak (14
  against 12), because its claims follow the rule weights rather than the
  lower role caps, so load is higher. On a day with no overload, muzzle
  doesn't change makespan or run times. It does raise load (p50 2.7 to 5.1,
  max 7.0 to 14.8) and `git status` p95 (3.5 to 5.4 ms), because it runs those
  gates at their configured 6 slots instead of the 4-thread role cap.
- **10-08 (the overload day; durations estimated, so a workload replay
  only, D43):** at speed 12 the synthetic window held at most 24 slots in
  `none`, 1.7x the budget. muzzle capped it at 14. Some calls queued (wait
  p90 52 s), makespan grew 11%, peak load fell from 22.8 to 17.4, and the
  interactive probe moved a little (sleep-late p99 1.46 to 1.02 ms; git
  status p95 flat).
- **The window was not the real overload.** The real 10-08 had load
  averages of 21-46 during the swayward v3 fan-out, much worse than this
  window reproduced. Two reasons: tier-1 burners do no I/O, linking or
  memory work, and museum's estimated durations under-represent gates that
  ran in the background. The tier-2 replay (real `contrib/fast-gate`,
  per-agent worktrees) is where that gets tested.
- **A confound in the tier-1 model:** in muzzle mode a call burns its full
  claim for `dur_s`, while `none` caps threads at the role cap. So muzzle mode
  does about 22% more CPU-seconds of work in both windows (10-21: 20,834
  against 16,683; 10-08: 112,399 against 88,228). Part of its extra load, and
  part of the 10-08 makespan growth, is that extra work, not admission.
- An earlier synthetic example (40 calls in 90 s) claimed jobs ran "4-6x
  faster once admitted". That was largely a bench artifact, fixed in
  `fix_bench`, so it is not repeated here.
