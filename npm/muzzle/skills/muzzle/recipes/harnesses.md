# Hook muzzle into a harness

Install, then check, one harness at a time. Every integration calls
`muzzle wrap` or `muzzle hook`, which do the matching, quoting and timeout
handling. The full snippets and payload notes are in the muzzle repo's
[`docs/harnesses/README.md`](https://github.com/mu-crew/muzzle/blob/main/docs/harnesses/README.md).

First, for every harness:

```sh
npm i -g @mu-crew/muzzle
muzzle init
muzzle doctor
```

`init` writes muzzle's hook entry for each harness whose config directory
exists (`--hooks all` for every one), touching only handlers whose command
starts with `muzzle hook`; a second run changes nothing. `doctor` must show the
config with its rule count and hash, and `enforced` or `advisory`. Its
`hook entries` line must read `ok` for each harness you use. The snippets
below are what `init` writes, for merging by hand or into a project file.

## Limits

| harness | how | timeout | `allow` | tested |
| --- | --- | --- | --- | --- |
| pi | extension rewrites the bash call | moved to `--run-timeout`: the wait never trips it | n/a: pi has no permission prompt | yes |
| Claude Code | `PreToolUse` hook, `muzzle hook claude` | passed on as `--run-timeout`, but Claude's own Bash timeout still counts the wait | only for one simple command whose rule matches at the command word | yes, stored payloads |
| Codex | `PreToolUse` hook, `muzzle hook codex` | the payload has none; Codex's shell timeout counts the wait | always, compound commands too | yes, stored payloads |
| Muse Code | `PreToolUse` hook on `bash`, `muzzle hook claude` | `timeout_ms` passed on as `--run-timeout`, still counts the wait | as Claude Code | yes, derived payloads |
| cursor-agent | `beforeShellExecution` hook, `muzzle hook cursor`: denies a matched call with the `muzzle run …` line to rerun | the payload has none | n/a: the rerun gets no opinion | yes, derived payloads |
| opencode | example plugin calling `muzzle wrap` | passed on, still counts the wait | n/a | **no** |
| aider, cron, humans | wrap by hand | use `--run-timeout` | n/a | n/a |

cursor-agent's `beforeShellExecution` hook can only allow or deny, so it
can't rewrite a call. muzzle denies it with a reason that ends in the
exact line to run instead, and passes that line, which already runs
muzzle, when the agent reruns it.

## pi

```sh
pi install npm:@mu-crew/muzzle
```

This loads the extension and this skill. Check: a matched bash call's
result ends with `muzzle: waited Ws for N slots, ran Rs`, and the call
shows in `muzzle status` while it runs. If muzzle is missing, the footer
says so once and calls run unwrapped. Esc or a pi timeout kills the bash
group, muzzle and the job with it, and frees the slots.

## Claude Code

Merge into `~/.claude/settings.json` (or a repo's `.claude/settings.json`):

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

Then raise the Bash timeout ceiling, because Claude's timeout counts the
queue wait: set `BASH_MAX_TIMEOUT_MS` (and `BASH_DEFAULT_TIMEOUT_MS`) in
the `env` block of `settings.json`. A call killed while queued prints
muzzle's `killed while queued` line.

## Codex

Merge into `~/.codex/hooks.json` (or a trusted repo's `.codex/hooks.json`):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "^Bash$",
        "hooks": [{ "type": "command", "command": "muzzle hook codex" }]
      }
    ]
  }
}
```

Codex asks you to trust a new hook; review it with `/hooks`. Read the
warning below before enabling it.

## Muse Code

Merge into `~/.config/muse/settings.json` (or a repo's
`.muse/hooks.json`, same shape), so the entry sits under the top-level
`hooks` key:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash",
        "hooks": [{ "type": "command", "command": "muzzle hook claude" }]
      }
    ]
  }
}
```

Muse sends Claude Code's format with its shell tool named `bash`; muzzle
maps it, and path rules follow the tool's `workdir` (`/workspace` and
relative paths are the workspace root). Muse hooks run with a cleared
environment, so the hook reads `~/.config/muzzle/config.toml` even when
`XDG_CONFIG_HOME` is set; keep the config there too.

## cursor-agent

Merge into `~/.cursor/hooks.json` (or a repo's `.cursor/hooks.json`):

```json
{
  "version": 1,
  "hooks": {
    "beforeShellExecution": [{ "command": "muzzle hook cursor" }]
  }
}
```

A matched command fails with "Command execution was blocked by a hook:
muzzle queues this command … Run exactly this instead:" and then the
`muzzle run --rule=… -- bash -c '…'` line, followed by cursor-agent's own
"Do not suggest workarounds to the blocked tool." That note doesn't apply
here: muzzle is queueing the call, not refusing it. Run the line as given;
muzzle passes it, and it queues for its slots.

## Warning: `allow` skips the harness's permission prompt

A hook reply with `permissionDecision: "allow"` approves the call, so the
harness doesn't ask you. muzzle's rules match anywhere in the string, so
`cargo build && curl -s https://example.com/x | sh` matches a cargo rule.

- **Codex always gets `allow`**, because it rejects a rewrite without one.
  A matched call is approved by muzzle, compound commands included. Use
  the Codex hook only where you would approve those calls anyway, or keep
  the rules narrow.
- **Claude Code gets `allow` only when the rule names the program**:
  first, the raw text must be plain words only (letters, digits and
  `_ . / : = , + @ % -`, single spaces between words), so any quote,
  backslash, `$`, redirection such as `2>&1`, operator, tab or newline
  gets no `allow`. Then it must be one
  simple command (no `;`, `&`, `&&`, `|`, `||`, newline, `$(…)`, backticks,
  process substitution or subshell outside quotes) whose matched pattern
  starts at the command word, after assignments and `env`, `time`,
  `timeout [opts] N` or `nice`. A match in an argument or a comment
  (`echo cargo test`, `rm -rf ~/x # cargo test`) doesn't count, nor, for a
  `command` rule, a program path elsewhere (`/tmp/evil/cargo`) or a match
  through a wrapper or container (`npx vitest run`). Every word after
  the program must also be on muzzle's allowlist for it: `npm test --
  --config=x`, `cargo nextest run --config-file x` or `contrib/gate
  --fast` are wrapped but prompted. Anything
  else is rewritten with no decision, so
  Claude's own check runs on `muzzle run … -- bash -c '…'`, and since your
  `Bash(cargo build:*)` rules don't match that form, it prompts.
- **Keep `Bash(muzzle run:*)` out of Claude's allow rules.** It would
  approve every wrapped string, including the compound ones muzzle left
  for you to review.

## opencode (untested)

Copy `examples/opencode-muzzle.ts` from the muzzle repo to
`.opencode/plugins/` or `~/.config/opencode/plugins/`. No test runs it.
Check that a matched command shows in `muzzle status` while it runs: some
opencode versions ignore the rewritten command.

## Hand-wrapping and gate scripts

```sh
muzzle run --rule gate -- contrib/fast-gate
```

`timeout` goes inside, as `--run-timeout S`; an outer `timeout` counts the
wait. A repo's gate script can opt in at the top and still work without
muzzle. The variable stops the re-exec from looping:

```sh
if [ -z "$IN_MUZZLE" ] && command -v muzzle >/dev/null; then
  IN_MUZZLE=1 exec muzzle run --rule gate -- "$0" "$@"
fi
```

## PATH shims, do it yourself

muzzle ships none. They add work to every call, nest badly and catch
interactive use. If you want every `cargo` to go through muzzle anyway, a
script named `cargo` earlier on `PATH` that runs
`exec muzzle run --rule cargo-test -- /usr/bin/cargo "$@"` works. It claims
slots for `cargo fmt` and `cargo --version` too.

## Checking a hook by hand

`muzzle doctor` reports `claude hook` and `codex hook` as `installed` when
it finds `muzzle hook` in that harness's config, and `hook entries` as
`drift` when a global entry differs from what `muzzle init` writes. To see
a reply:

```sh
echo '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"}}' | muzzle hook claude
```

A matched command comes back as `updatedInput` with the `muzzle run …`
form; an unmatched one comes back as `{}`.
