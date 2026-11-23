# muzzle: heredoc/script inspection & retroactive grace accounting

Status: revised draft (2026-11-23, post-refutation wave `spec-034-refute`).
Extends `src/rules/` (command rules) and `src/run/` (D16–D18) with
add-only CLI, config, and JSON additions (AGENTS.md invariants). This
spec wins where it and earlier specs disagree.

## Problem

On a busy multi-agent host, `muzzle report --since 3d` measured
**unclaimed load p50 = 7.3 cores, p90 = 22.0 cores** (on a 16-core host)
while muzzle's own utilisation was only 12%. Auditing 321 recent agent
sessions showed the main leak is **throwaway scripts and heredocs**:

1. **Throwaway `python3` scripts** (`python3 - <<'EOF'`, `python3 -c`,
   `python3 /tmp/*.py`): 157 calls ≥10 s (13,358 s total) invoking
   subprocesses, stress loops, or test harnesses.
2. **Throwaway shell scripts & heredocs** (`bash <<'EOF'`,
   `cat > /tmp/x.sh <<'EOF' … && bash /tmp/x.sh`, `./measure.sh`): 39
   calls ≥10 s (5,332 s total) hiding `cargo`, `vitest`, or build tools
   inside the heredoc or script file.
3. **Throwaway `node` harnesses** (`node --input-type=module <<'EOF'`,
   `node /tmp/*.mjs`): 29 calls ≥10 s (663 s total) running `esbuild` or
   spawning test servers.
4. **Opaque heavy commands**: unfamiliar compilers/tools or pure-Python
   CPU burners that match no static rule at all.

Today `src/rules/shell.rs` discards heredoc bodies (`skip_heredocs`) and
never inspects local script files. Matching every `python3` or `node`
blindly would over-claim 50 ms file-edit scripts.

## Prior art

- **Zed `shell_command_parser` & `kintsugi-core`** (`brush-parser`):
  flatten pipelines, `$(…)`, and shell heredocs (`bash <<EOF`,
  `sh <<< …`) into simple commands. `kintsugi-core` notes that
  `brush-parser`'s heredoc reader has a heap-exhaustion DoS on malformed
  `<<` input and stack-overflow risk on deep `$(…)`.
- **`destructive_command_guard::heredoc`**: extracts commands from
  heredocs/scripts and feeds them back into the main rule evaluator.
- **`processkit`**: cross-platform process-tree accounting/suspension
  (Linux cgroup v2 vs macOS process groups).

muzzle already has a zero-dependency, bounded shell lexer
(`src/rules/shell.rs`, which already parses heredoc delimiters) and
cross-platform `proc::group_cpu`. We extend both directly with zero new
crate dependencies.

## Decisions

### H1 — Executed shell heredocs and here-strings (`src/rules/shell.rs`, `src/rules/heads.rs`)

1. **`collect_heredocs` in `src/rules/shell.rs`:**
   - Replace `skip_heredocs` with `collect_heredocs`, recording for each
     simple command:
     - its **effective fd-0 (stdin) input**: when multiple `<<` / `<<<`
       redirections target fd 0 (implicit or explicit `0<<`), **only the
       last one** wins (matching POSIX/bash semantics). Non-fd-0
       heredocs (e.g. `3<<'EOF'`) are skipped cleanly to their delimiter
       and not treated as stdin.
     - empty delimiters (`<<''` / `<<""`): queue them properly so their
       body (until an empty line) is consumed and never leaked as
       top-level commands.
     - cap stored body text at `16 KiB` per heredoc/here-string (excess
       lines are still skipped cleanly to the delimiter).
     - record per-command output redirect targets (`> path`, `>> path`,
       `1> path`) and simple literal variable assignments (`f=/tmp/x.sh`)
       in the command sequence.
2. **Shell-executed stdin heredocs/here-strings:**
   - When a simple command's stripped program is a shell (`SHELLS`: `sh`,
     `bash`, `zsh`, `dash`, `ksh`, `ash`), **has no `-c` script**, and
     **has no positional script file** (only shell options or `-` / `-s`),
     its effective stdin heredoc or here-string is shell code: feed it
     into `heads::text_heads` at `depth + 1` (bounded by `MAX_DEPTH = 8`).
   - When `bash -c : <<'EOF'` or `bash script.sh <<'EOF'` is given, the
     heredoc is data, not shell code, and is ignored.
3. **Write-then-execute heredocs in the same shell string:**
   - Only commands that actually copy stdin to a file (`cat` with `>`/`>>`
     target, or `tee` with its non-flag file arguments) associate their
     stdin heredoc with those target paths. Bare `> /tmp/x.sh <<'EOF'`
     does not copy stdin and is not treated as a writer.
   - If a later command in the same shell string executes one of those
     written paths (directly `/tmp/x.sh`, via a shell `bash /tmp/x.sh`,
     or via `source` / `.`), feed that heredoc body into
     `heads::text_heads` at `depth + 1` (or into H3 if it has a
     `python`/`node` shebang or `.py`/`.js`/`.mjs` extension).
   - Resolve simple `$VAR` / `"$VAR"` references from earlier literal
     assignments in the same shell string (updating on reassignment;
     stopping if reassigned dynamically).
   - Non-executed heredocs (`git commit -F - <<'EOF'`, writing markdown,
     JSON, or source files) are never parsed as commands.

### H2 — Local shell script files (`src/rules/heads.rs`, `src/rules/resolve.rs`)

1. Add `Heads::new_in(text, cwd: Option<&Path>)` (keeping pure
   `Heads::new(text)` with `cwd = None` for pure callers/tests).
   `match`, `wrap`, and `hook` pass their resolved `cwd`.
2. Track simple `cd <dir>` commands within the same shell string to
   update the effective directory used to resolve subsequent relative
   script paths (`cd sub && ./check.sh`).
3. **Safe, bounded file open and read:**
   - Trigger when `cwd` is `Some` and a command executes a local script
     file: `SHELLS` with a positional script path (`bash /tmp/x.sh`,
     `sh scripts/check.sh`), `source <path>` / `. <path>`, or a command
     path containing `/` (`./measure.sh`, `/tmp/x.sh`, `scripts/gate.sh`).
   - Reject paths under `/bin`, `/usr/bin`, `/usr/local/bin`, `/sbin`,
     `/usr/sbin`, `/opt`, `/dev`, `/proc`, `/sys`, or `node_modules`.
   - Open with `O_RDONLY | O_NONBLOCK | O_NOFOLLOW` (so a FIFO or device
     never blocks and symlinks to arbitrary system files are skipped),
     `fstat` the opened fd to verify `is_file()`, and read at most
     `16 KiB` (`take(16 * 1024)`).
   - Enforce a **per-invocation total budget** of at most `8` files and
     `64 KiB` total bytes read across the entire `Heads` evaluation, plus
     a visited `(dev, ino)` set to prevent self-sourcing loops.
   - Skip binaries (ELF `\x7fELF`, Mach-O magic, or any NUL byte in the
     first 512 bytes).
   - Inspect the first line for a shebang (`#!`): if it names `python`
     or `node`, route the body to H3; otherwise feed it into
     `heads::text_heads` at `depth + 1`.
   - Missing or unreadable files are silently skipped.

### H3 — Throwaway `python3` / `node` script inspection (`src/rules/heads.rs`, `src/rules/config/`)

1. **Input-mode resolution first:** when a command invokes `python`,
   `python3`, or `node` (past wrappers `env`, `timeout`, `time`, `nice`):
   - `python3 -c '<code>'` / `node -e '<code>'` / `node -p '<code>'`:
     inspect `<code>` (heredoc is data, ignored).
   - Positional script file (`python3 /tmp/x.py`, `node /tmp/x.mjs`):
     read the local file safely under H2's budget (heredoc is data,
     ignored).
   - No `-c`/`-e` and no positional file (or `-`, or `node
     --input-type=module`): inspect the command's effective stdin
     heredoc or here-string.
2. **Heavy-signal & token-window rule matching:**
   - Scan the script body (up to `16 KiB`) for **heavy signals**:
     - Python: `subprocess.`, `from subprocess import`, `import subprocess`,
       `os.system(`, `os.popen(`, `multiprocessing`, `pytest`, `cargo`,
       `vitest`, `buck2`, `lake`.
     - Node: `child_process`, `spawnSync(`, `execFileSync(`, `execSync(`,
       `spawn(`, `esbuild`, `vitest`, `tsup`, `tsc`, `cargo`.
   - Pure file-edit scripts (`open(p).read()`, `replace(`, `json.load`,
     `writeFileSync`) carry none of these signals and stay **unclaimed**.
   - When a heavy signal is present:
     - Tokenize the body by splitting on non-path/non-word punctuation
       (whitespace, quotes, commas, brackets, parens — so
       `subprocess.run(["cargo", "test"])` yields adjacent tokens
       `cargo`, `test`!). Check if any configured `command` rule's
       program+subcommand words appear in order within a 3-token window;
       if so, emit a synthetic head for that command so the existing
       tuned rule wins.
     - Otherwise, match the built-in `"script-heavy"` rule.
3. **Built-in `"script-heavy"` rule (consistent across `resolve`, `run`, `report`):**
   - `config.toml` gains optional `script_slots: Option<u32>` (default
     `2`; `0` disables the fallback; excluded from `config::hash`).
   - When `script_slots > 0` and no user rule named `"script-heavy"`
     exists, `Effective` includes a built-in `"script-heavy"` rule at
     the end of the rule list with `slots = script_slots` (and
     `rule_anywhere("script-heavy")` resolves it too), so `muzzle run`
     never warns "unknown rule" and `muzzle report` tracks and tunes it
     normally. A user-defined `[[rule]] name = "script-heavy"` overrides
     its slots/class or disables it (`enabled = false`).
4. **Hook safety (D14 / R5):**
   - `Match` carries `provenance: MatchProvenance` (`Direct` vs
     `Inspected`). Any match produced via H1, H2, or H3 body inspection
     has `MatchProvenance::Inspected`.
   - `src/rules/hook.rs` **never** sets Claude's `permissionDecision:
     "allow"` when `m.provenance == MatchProvenance::Inspected` (even if
     a lower-priority direct rule would also match).

### H4 — Retroactive slot accounting (`muzzle run --grace S`)

For genuinely opaque heavy commands (pure-Python CPU loops, unknown
compilers, custom binaries) that match no static rule even after H1–H3:

1. **CLI, built-in rule, and config:**
   - Add `--grace <S>` (`f64`, seconds, `> 0`, conflicts with
     `--timeout` and `--no-timeout`) and `--grace-cpu <S>` (`f64`,
     CPU-seconds, default `1.0`, `>= 0`) to `RunArgs` (`src/cli.rs`).
   - Add optional `grace_s: Option<f64>` (default `None`), `grace_cpu_s:
     Option<f64>` (default `1.0`), and `grace_max_slots: Option<u32>`
     (default `4`) to `config.toml` (all excluded from `config::hash`).
   - Register `"unmatched-heavy"` as a built-in rule in `Effective` /
     `rule_anywhere` (same pattern as `"script-heavy"`), so `muzzle run
     --rule unmatched-heavy` never emits an "unknown rule" warning and
     `muzzle report` tracks it cleanly.
   - Add `#[serde(default, skip_serializing_if = "std::ops::Not::not")]
     pub grace: bool` on `Holder` (`src/state.rs`, strictly additive).
2. **D17 nesting interaction:**
   - `is_nested` (`src/run/mod.rs`) **skips** any holder with `h.grace ==
     true`. A promoted grace holder never swallows an inner `muzzle run`
     (e.g. `./prep.sh && muzzle run --rule=gate -- contrib/gate` still
     claims its full 6 slots instead of running nested under a 1-slot
     grace claim).
3. **Execution lifecycle (`src/run/grace.rs`):**
   - Check D17 `is_nested` first; if nested inside a real (non-grace)
     holder, run directly as today.
   - **No upfront queue wait, no upfront `state.json` write, no upfront
     `state.lock`.** Spawn the child immediately (same group-leader
     semantics as D16; `--run-timeout` enforced as D18; `enforced: false`
     since systemd scope requires a ticket at spawn).
   - While the child (or any surviving member of its group, `!target.others().is_empty()`)
     is alive, sample `proc::group_cpu(pgid)` every `POLL` (250 ms),
     accumulating monotonic per-member CPU time in a high-water
     `HashMap<(u32, u64), u64>` so total CPU never drops when children
     exit.
   - **Fast / idle exit (no live group members left):** if the group
     empties before promotion, exit with the top command's exit code,
     write **no** `state.json` entry and **no** `runs.jsonl` line, and
     (if `--report` was passed) write a report with `"promoted": false`,
     `"slots": 0`, `"waited_s": 0.0`, `"ran_s": …`, `"killed": …`.
   - **Promotion trigger:** once `elapsed >= grace` **and** either
     `total_cpu_s >= grace_cpu` OR (`group_cpu` returned `None` due to
     `EPERM` and `elapsed >= 2.0 * grace`):
     - Compute `slots = (total_cpu_s / elapsed).ceil() as u32` (or `1`
       on `EPERM`), clamped to `1..=max_slots` (capped to host budget).
     - Acquire `state.lock`, resolve budget (D17a), allocate a ticket,
       stamp `admit_load1` and `admit_held`, and insert a live `Holder`
       with `grace: true`, `waited_s: 0.0`, `enforced: false`, `rule:
       args.rule.or(Some("unmatched-heavy"))`, `slots`, `pgid`,
       `run_start_ms` = spawn time. **Do NOT `SIGSTOP` the child.**
     - If the top command already exited while children remained, hand
       off to a D16 keeper; otherwise continue normal heartbeat, keeper
       handoff on background survivors, and normal `release()` +
       `runs.jsonl` + `--report` (`"promoted": true`) on exit.
   - Note: `state.held()` may temporarily exceed `budget` on promotion;
     `admit.rs:215` (`budget.saturating_sub(state.held())`) already
     handles `held > budget` cleanly.
4. **Opt-in `--with-grace` on `muzzle wrap` (zero impact on hooks or older extensions):**
   - `muzzle wrap --json` keeps its **exact existing behaviour** by
     default (returns `matched: false` for unmatched commands; `hook.rs`
     for Claude, Codex, Cursor, Muse and older pi extensions are 100%
     untouched—no Codex auto-allow regression, no Cursor deny loops, no
     Claude prompt regressions).
   - Add `--with-grace` flag to `WrapArgs` (`src/cli.rs`), used by the
     updated pi extension (`npm/muzzle/extension/muzzle.ts`):
     - When no static rule matched, `grace_s` (from `MUZZLE_GRACE` env
       var > `config.grace_s`, disabled when `0` / unset) is `Some(s)`,
       and the command is **not** purely light:
       - A command is purely light iff **every** stripped head in the
         command (past `env`/`time`/`nice`/assignments) is on the light
         allowlist: `ls`, `cat`, `head`, `tail`, `rg`, `grep`, `git`,
         `mu`, `gh`, `muzzle`, `ps`, `pgrep`, `pkill`, `kill`, `rm`,
         `mv`, `cp`, `mkdir`, `sed`, `awk`, `wc`, `sort`, `uniq`,
         `echo`, `printf`, `date`, `pwd`, `which`, `true`, `false`,
         `sleep`, `cd`, `export`, `unset`, `test`, `[`, `touch`,
         `chmod`, `diff`, `jq`, `find`, `tee`.
       - Otherwise `wrap --json --with-grace` returns `matched: false,
         grace: true, command: "muzzle run --grace <S> --rule=unmatched-heavy --slots <max> [--run-timeout T] -- bash -c '…'"`.
   - **Pi extension (`npm/muzzle/extension/muzzle.ts`):**
     - Passes `--with-grace` to `muzzle wrap --json`.
     - Treats `reply.grace === true` as a wrapped command, and in
       `tool_result` suppresses the `muzzle:` footer **only** when
       `report.promoted === false && report.killed == null` (so a
       `--run-timeout` kill before promotion still prints its footer).

## Schema & compatibility

| surface | additions | compatibility notes |
| --- | --- | --- |
| `config.toml` | `script_slots` (`Option<u32>`, default `2`), `grace_s` (`Option<f64>`, default `None`), `grace_cpu_s` (`Option<f64>`, default `1.0`), `grace_max_slots` (`Option<u32>`, default `4`) — all excluded from `config::hash` | `Layer` uses `deny_unknown_fields`: upgrade muzzle before adding these keys to `config.toml`. `MUZZLE_GRACE` env var is ignored by older binaries. |
| `state.json` (`Holder`) | `grace: bool` (`#[serde(default, skip_serializing_if = "std::ops::Not::not")]`) | Additive. |
| `wrap --json` | `grace: bool` (omitted when `false`, only emitted when `--with-grace` is passed) | Strictly backwards-compatible with older pi extensions and all harness hooks. |
| `--report` JSON | `promoted: Option<bool>` (omitted for normal runs, `Some(false)` for unpromoted grace exits, `Some(true)` for promoted grace runs) | Additive. |
| `runs.jsonl` / `status --json` | None | Promoted grace runs use existing `Holder` and `RunRecord` schemas with `rule: "unmatched-heavy"`. |

## Architecture & affected units

- `src/rules/shell.rs`: `collect_heredocs` (fd-0 last-wins, empty delim,
  16 KiB cap), output redirect targets, here-strings, literal variable
  assignments.
- `src/rules/heads.rs`: H1 shell/write-then-exec heredocs, H2 safe
  `O_NONBLOCK | O_NOFOLLOW` + `fstat` local script reader (`cd` tracking,
  8-file / 64 KiB budget, `(dev, ino)` cycle guard, shebang routing),
  H3 `python`/`node` input-mode + heavy-signal + token-window rule
  matching, `MatchProvenance`.
- `src/rules/resolve.rs`, `src/rules/mod.rs`, `src/rules/hook.rs`: pass
  `cwd` into `Heads`, built-in `"script-heavy"` and `"unmatched-heavy"`
  rules, `MatchProvenance::Inspected` blocks Claude `allow`, `--with-grace`
  + light-command allowlist in `wrap`.
- `src/rules/config/{layer,mod}.rs`: `script_slots`, `grace_s`,
  `grace_cpu_s`, `grace_max_slots` (excluded from `config::hash`).
- `src/state.rs`: additive `Holder.grace: bool`.
- `src/cli.rs`: `--grace`, `--grace-cpu` on `RunArgs`; `--with-grace` on
  `WrapArgs`.
- `src/run/grace.rs` (new unit) & `src/run/{mod,report}.rs`: H4 grace
  runner (monotonic per-member CPU map, background-survivor monitoring,
  retroactive `Holder` promotion with `admit_load1`/`admit_held`,
  `is_nested` skips `h.grace`), `promoted` field in `--report`.
- `npm/muzzle/extension/muzzle.ts` & `npm/muzzle/test/`: pass
  `--with-grace`, suppress footer when `promoted === false && killed ==
  null`.

## Verification plan

1. **Shell & heads unit tests (`src/rules/shell.rs`, `src/rules/heads.rs`):**
   - `bash <<'EOF'\ncargo test\nEOF` matches `cargo-test`; `bash <<'A' <<'B'` uses only `B`; `bash -c : <<'EOF'` ignores heredoc; `<<''` is consumed cleanly.
   - `cat > /tmp/x.sh <<'EOF'\ncargo test\nEOF\nbash /tmp/x.sh` and `f=/tmp/x.sh; cat > "$f" <<'EOF'…; bash "$f"` match `cargo-test`; bare `> /tmp/x.sh <<'EOF'` does not; `git commit -F - <<'EOF'\ncargo test\nEOF` stays unclaimed.
   - Local script file (`bash <tmp>`, `cd sub && ./check.sh`) matches; FIFO/device/symlink/binary/`/usr/bin/*` skipped safely; self-sourcing loop terminates; 8-file/64 KiB cap respected.
   - `python3 - <<'EOF'` doing `open(p).read().replace(...)` stays unclaimed; `python3 - <<'EOF'` calling `subprocess.run(["cargo", "test"])` matches `cargo-test`; `from subprocess import run; run(["my_tool"])` matches `script-heavy`; `python3 -c 'print(1)' <<'EOF'` ignores heredoc.
   - `node --input-type=module <<'EOF'` importing `child_process`/`esbuild` matches `script-heavy`.
   - Claude hook never sets `allow` on `MatchProvenance::Inspected`.
2. **Grace runner & wrap integration tests (`tests/grace.rs`):**
   - `wrap --json` without `--with-grace` returns `matched: false` (hooks untouched); with `--with-grace` and `MUZZLE_GRACE=5`, light commands (`git status`, `cd repo && ls | head`) stay unwrapped while opaque commands (`./mytool`) return `grace: true`.
   - Fast command (`true`) under `--grace 2` writes no `state.json`, no `runs.jsonl`, and `report.promoted == false`.
   - Idle long command (`sleep 2`) under `--grace 0.5 --grace-cpu 0.5` does not promote.
   - CPU burner under `--grace 0.5 --grace-cpu 0.1` promotes into `state.json` with `grace: true`, holds slots until exit, logs to `runs.jsonl`, and writes `report.promoted == true`.
   - Inner `muzzle run` inside a promoted grace holder is NOT swallowed as nested (claims its own slots).
   - Background survivor (`sh -c 'burn &'`) stays monitored past top-command exit and promotes.
3. **Pi extension tests (`npm/muzzle/test/`):**
   - Passes `--with-grace`, handles `grace: true`, suppresses footer on `promoted: false && killed == null`, keeps footer on `promoted: true` or `killed != null`.
4. **Commit gate:** `CARGO_BUILD_JOBS=4 cargo fmt --check &&
   CARGO_BUILD_JOBS=4 cargo clippy --all-targets -- -D warnings &&
   CARGO_BUILD_JOBS=4 cargo test` + `npm test` in `npm/muzzle` + green CI
   on Linux and macOS.
