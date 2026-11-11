# Roadmap

What is left after v1. Shipped work is in [CHANGELOG.md](../CHANGELOG.md);
behaviour is in the [spec](specs/2026-10-29-muzzle.md), and the v1 verdict
is in [eval/2026-11-muzzle-v1.md](eval/2026-11-muzzle-v1.md).

The items come from the `muzzle-v1` mu workstream, which closed on
2026-11-06 with 197 tasks closed and 7 parked. Each item names its mu task
(`mu task show -w muzzle-v1 <id>`). None of them blocks a release, and none
is scheduled.

## Measurement and tuning

### Measure the D7a default and the `fast-gate` weight

`budget_vs_weight_rerun_12x_sweep_with` (eval recommendation 2)

D7a (0.2.5) set the default budget to floor(cores × 1.5) − reserve, 22 on
linuxpc. No run measured 22: it lies between the §2c sweep points 20 (316
jobs/h) and 24 (353 jobs/h). Still open:

- rerun the 12× sweep at the new default;
- check whether `fast-gate` at weight 6 rather than 3 costs throughput on its
  own.

The kit is in `~/.cache/muzzle-bench` on linuxpc: `ladder.sh`, `sweep.sh`,
`real-ladder-2026-10-08.toml` and `out/`. The worktrees were removed;
`bench setup` rebuilds them in about an hour. `sweep.sh` reads
`cfg-budget-<N>/muzzle/{config.toml,machine.toml}`.

### Serialise each agent's calls under `--speed`

`bench_serialise_each_agent_s_calls_under` (eval recommendation 3)

Compressed time makes one agent's consecutive calls overlap in its single
worktree, which causes 3 to 11 failures per ladder run in both modes. Fix
`bench run` before the next high-load run, so that failures measure load
flakes.

### Real-use comparison (D44 point 2)

The rollout week (`rollout`, closed wontfix) did not run, so gate p90 and
merged tasks per hour are still unmeasured (eval §3, §4). muzzle stays in
daily use, and `runs.jsonl` keeps collecting data. When there is enough,
compare `muzzle report --compare` against the museum baseline (eval §1).

## Container gates

### Measure container gates from `libpod-<id>.scope` (D16a-b)

`container_measure_followup`

Container gates have no CPU or memory figures (D16a). When muzzle is a
container's only holder, read `cpu.stat` and `memory.peak` from the
container's outer `libpod-<id>.scope`, which the user owns. The human chose
option (b), to be done after v1, on 2026-10-29.

### D16a edge cases

`d16a_followup` (from `review_container_gates_2`, c360b0d). All four only
read state.

1. `cgroup_has_process` (`src/proc/linux.rs`) takes "saw no match in
   `/proc`" as "empty". With `hidepid=1/2`, or an unreadable proc file, it
   fails open for a waiter that shares the pid namespace but not the cgroup
   namespace (distrobox, toolbox). Return `Some(false)` only when the scan
   could read at least one of the holder uid's processes, such as the
   recorded holder pid. Otherwise return `None`.
2. `scope_known_empty` scans only `/proc` and ignores the recorded cgroup
   path. On the host, reading `cgroup.events` would be cheaper and exact.
3. A waiter in a separate pid namespace (`docker`/`podman exec` into a
   container without host pids) keeps a dead container claim until a host
   waiter or `muzzle status` runs. This fails closed by design, but the
   README should say so.
4. `scope_dir()` polls for up to 2 s at spawn. A signal in that window stays
   pending. Add a test that SIGTERM in that window still keeps the hold.

## Hardening

### `state.json`

`state_hardening_followup` (from `review_state_admit_3`). Triage it after
rollout, or sooner if dogfooding hits one of these.

- The `state::load` probe reads `v` as a `u32`. A newer file whose `v` is
  not a `u32` (`"2"`, `2.0`, negative, or above `u32::MAX`) takes the
  corrupt branch and is overwritten. Probe `v` as a `serde_json::Value`
  instead, and refuse any `v` that is present but not a `u32`.
- `newer_schema_with_an_incompatible_shape_is_refused_and_left_alone`
  asserts that the directory holds exactly 2 entries. It should assert that
  no `*.corrupt-*` entry exists.

### Command rules

`rules_followup` (from `review_rules_2`, 4b5f4f7). Some hand mutants of
`is_simple_command` survive the suite:

1. Dropping the `$`/`<`/`>` + `(` arm survives, because the `(` arm already
   rejects those inputs. Remove it, or document it as defence in depth.
2. Dropping `\r` as an operator survives. Bash does not split on CR, so the
   check is only conservative and has no test of its own.
3. Dropping the `$'…'` guard survives. Pin it with
   `r"cargo test $'\\'' ; curl x ; echo \\'"`, which fails without the guard.
4. Spec §Interface/CLI: the hook's shape line shows
   `permissionDecision: allow` unconditionally. For claude the field is
   conditional.

### `status` / `doctor`

`sd_followup` (from `review_status_doctor_2`)

- `pi_loads` counts any `muzzle*` directory that has a `package.json`, even
  one with no pi extension entry. Check how pi resolves a package: a `pi`
  field, or `main`.
- `pi_loads` compares extensions case-sensitively and ignores `.mjs`/`.cjs`.
  Check which extensions pi's loader accepts (pi `docs/extensions.md`).
- `gather()`: when `Store::open_default` fails, only the directory error is
  reported. That is intended, but no test pins it.
- The `status::view` doc comment runs past the wrap width (cosmetic).

## Not planned

- **PATH shims for `cargo`/`npm`/… that claim work hidden in scripts.** The
  human rejected them on 2026-10-30 as too heavy-handed: muzzle is opt-in.
  Unclaimed load is measured instead (0.2 P12).
- **The rollout week as a gated proof step.** Closed on the replay evidence
  on 2026-11-06. Real-use data still accrues; see *Real-use comparison*
  above.
