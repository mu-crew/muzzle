# bench

Benchmark traces and the tools that make and check them (spec D36–D40,
§Interface "Trace format"). `muzzle bench run` replays a trace.

| file | what |
| --- | --- |
| `from-museum` | builds `traces/<date>.jsonl` from `museum-search calls --json` |
| `check-trace` | validates traces; CI and the pre-commit hook run it |
| `pre-commit` | git hook: checks anything staged under `traces/` and the labels, refuses private sidecars and data files elsewhere |
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
checks every file under `traces/`, recursively. Errors name the line, field
and kind, never the value.

Git publishes file names too, so every path under `traces/` must be
`YYYY-MM-DD.jsonl` or `YYYY-MM-DD-<n>.jsonl` (a real date, n from 1 to 99;
no free-text suffix) or the top-level `.gitkeep`, with no subdirectories.
CI mode, the hook (`check-trace --paths`) and `from-museum --out` all refuse
any other name, and the error never echoes it.

Elsewhere under `bench/`, CI mode and the hook refuse any `*.json`,
`*.jsonl` or `.gitattributes`, and any name containing `.private.`. They
also run the email, home-dir, token and hostname patterns over every line of
`allowed-labels.txt`, comments included, without echoing the match.

## Threat model

The checks guard generated data: traces, and anything shaped like one, can
never carry command text, paths, names or other hosts (D36–D40), because
they are built by a tool from private sources and too large to read. Prose
written by a person (this README, `allowed-labels.txt` comments, notes,
scripts) is reviewed like any code and is not scanned beyond the labels
file. A `project` line in `allowed-labels.txt` is published on review, by
design (D39).

Tests: `tests/bench_tools.rs` (run by `cargo test`) drives the scripts and
`bench/test_tools.py`.
