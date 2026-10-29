# bench

Benchmark traces and the tools that make and check them (spec D36–D40,
§Interface "Trace format"). `muzzle bench run` replays a trace.

| file | what |
| --- | --- |
| `from-museum` | builds `traces/<date>.jsonl` from `museum-search calls --json` |
| `check-trace` | validates traces; CI and the pre-commit hook run it |
| `pre-commit` | git hook: checks anything staged under `traces/`, refuses private sidecars |
| `allowed-labels.txt` | the rule and kept-project labels a trace may carry |
| `traces/` | committed traces (linuxpc only) |
| `real.toml` | tier-2 commands per rule label; local, never committed |

Install the hook once per clone:

```sh
ln -s ../../bench/pre-commit "$(git rev-parse --git-common-dir)/hooks/pre-commit"
```

## Making a trace

```sh
bench/from-museum --date 2026-10-21 --museum-search "$S"   # or --calls FILE
bench/check-trace
```

- Only calls from `linuxpc/` session files are read; others are refused and
  not counted (D37).
- Rules come from your config through `muzzle match --cwd`, run locally, so
  command text never leaves the machine. Waits and polls (`mu wait`,
  `mu agent wait`, `mu task wait`, `sleep`, `tail -f`, `museum-search`, and
  segments that only read logs) are `light` however long they take. An
  unmatched call of 30 s or more is `unmatched-heavy`.
- Agents become `a1, a2, …` and projects `p1, p2, …` by first appearance.
  `--keep-project NAME` keeps a name you have made public; it must also have
  a `project NAME` line in `allowed-labels.txt` (D39).
- The mapping from each trace line back to its command, cwd and session goes
  to `$XDG_STATE_HOME/muzzle/bench/<trace>.private.jsonl` (mode 600). Never
  commit it (D38).
- The importer runs the checker before writing, so a rule name missing from
  `allowed-labels.txt` stops the import. Add a `rule NAME` line and re-run.
- Before committing the first traces, run one read-only refuter pass for
  identifying timing or label patterns (D40).

## What check-trace rejects

Any field outside the schema; any string that isn't a fixed label, a listed
label, `a<digits>` or `p<digits>`; anything path- or home-like (including
your user name); a host other than `linuxpc`; emails; token-like strings;
header counts that don't add up; a duplicate key, or a line that isn't
its own canonical `json.dumps(row, separators=(",", ":"))`; numbers out of
bounds (cores 1–1024, ram_gb 1–4096, window_s ≤ 7 days, counts ≤ 10^7,
`dur_s` ≤ window_s, `t`/`dur_s` at most 3 decimals). With no arguments it
checks every file under `traces/`, recursively, and fails on any that isn't
`.jsonl` (`.gitkeep` aside). Errors name the line, field and kind, never
the value.

Tests: `tests/bench_tools.rs` (run by `cargo test`) drives the scripts and
`bench/test_tools.py`.
