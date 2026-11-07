# Stored hook payloads

`muzzle hook claude|codex|cursor` is tested against these payloads
(`src/rules/hook.rs`, the CLI tests in `tests/rules.rs`, and the
snippets in `docs/harnesses/` through `tests/harnesses.rs`). The harnesses
document the format and may change it, so a release re-checks it against
the sources below (spec §Integrations, Claude Code and Codex).

Checked 2026-10-29.

## Claude Code

Source: <https://code.claude.com/docs/en/hooks> ("Hook input", "PreToolUse
input" (Bash), "PreToolUse decision control"). See also
<https://code.claude.com/docs/en/agent-sdk/hooks>.

- The `claude-*.json` payloads copy the documented Bash example, with the
  command and tool fields changed.
- Bash `tool_input` is `command`, `description`, `timeout` (milliseconds)
  and `run_in_background`.
- The reply is `hookSpecificOutput` with `hookEventName: "PreToolUse"`
  and `updatedInput`. `updatedInput` replaces the whole input object, so
  muzzle copies the other fields. Without `permissionDecision` the new
  input still applies and goes through Claude's normal permission
  evaluation; `allow` skips the permission prompt. muzzle sends `allow`
  only for one simple command (spec §Interface CLI `hook`).
- `claude-bash-compound.json` (`&&` and `|`) gets no decision;
  `claude-bash-quoted-operator.json` (`'a&&b'` quoted) is still simple,
  so it is wrapped, but it gets no decision either: Claude's `allow` needs
  plain words only, and a quote isn't one (spec 0.2.1 R5).
- Exit 2 blocks the call. Any other non-zero exit is a non-blocking error.
  That is why `muzzle hook` exits 1, never 2, when it can't answer.

## Codex

Sources:

- <https://developers.openai.com/codex/hooks> ("Common input fields",
  "PreToolUse", "Tool coverage").
- The generated schemas,
  <https://github.com/openai/codex/tree/main/codex-rs/hooks/schema/generated>
  (`pre-tool-use.command.{input,output}.schema.json`).
- `codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs` at
  openai/codex `3535c22`.

The spec's third-party guide (aftergradient.com/guides/codex-hooks) agrees
with these sources.

- The required input fields are `session_id`, `turn_id`, `transcript_path`,
  `cwd`, `hook_event_name`, `model`, `permission_mode`, `tool_name`,
  `tool_use_id` and `tool_input`. The `codex-*.json` payloads carry exactly
  these.
- Shell calls arrive as `tool_name: "Bash"` with
  `tool_input: {"command": "<string>"}` and nothing else, so the payload
  has no timeout to pass on (D24). `apply_patch` also uses
  `tool_input.command`, which is why muzzle checks `tool_name`.
- To rewrite a call, reply `permissionDecision: "allow"` with
  `updatedInput.command`. `updatedInput` without `allow` is an error, and
  `ask` is not supported. Exit 2 blocks the call. So Codex always gets
  `allow`, compound commands included (`codex-bash-compound.json`).

## Muse Code

Muse Code sends Claude Code's `PreToolUse` format to `muzzle hook claude`
(every-harness spec D14). Source: the U0 spike logs of Muse Code 1.4.15
(2026-11-05), and the description its `bash` tool gives the model.

- U0 logged no `PreToolUse` (the spike hooked only the event vocabulary),
  so the `muse-*.json` payloads are **derived**: they copy the logged
  `PermissionRequest` payloads, which carry the same `tool_name` and
  `tool_input`, with `hook_event_name` changed and the model fields
  replaced by placeholders.
- The shell tool is `bash`, lowercase; Claude's is `Bash`. Its
  `tool_input` is `command`, `description`, an optional `workdir` (the
  directory the command runs in; the workspace root when absent, a
  relative path is under the root, and `/workspace` is an alias for the
  root), and optionally `sandbox_permissions`, `yield_time_ms` and `timeout_ms`.
  Only `timeout_ms`, the hard kill deadline in milliseconds, is a timeout;
  `yield_time_ms` is how long the call waits for output.
- Other tools carry other fields (`muse-subagent-spawn.json` has
  `command_id`), which is why muzzle checks `tool_name`.
- The reply is Claude's `hookSpecificOutput`, with the same `allow` rule
  as Claude. muzzle always sends `permissionDecision: "allow"` together
  with `updatedInput`, never alone.

## cursor-agent

Source: cursor-agent's own bundle (2026.10.22, re-checked on 2026.11.05) (`dist-package/`, the hook
executor and the shell tool's `beforeShellExecution` call), and
<https://cursor.com/docs/hooks>. The U0 logs have no
`beforeShellExecution`, so the `cursor-*.json` payloads are **derived**:
the common fields copy U0's logged cursor-agent payloads (`user_email`
replaced by a placeholder), and the shell fields follow the bundle.

- The payload is the common fields plus `command`, `cwd` and
  `sandbox`, all at the top level; there is no `tool_name` or
  `tool_input`. `cwd` is `""` when the agent set no working directory, and
  the command then runs in the first of `workspace_roots`; a relative
  `cwd` is under that root, and a leading `~` is the home directory.
- `sandbox` is a boolean, true when the shell call runs sandboxed. muzzle
  doesn't read it.
- The reply is `{"permission": "allow" | "deny" | "ask", "user_message",
  "agent_message"}`. It can't rewrite the command. On `deny` the shell
  path reads only `user_message`: the call fails with "Command execution
  was blocked by a hook: <user_message>", a pointer to cursor's hook
  settings, and "Agent note: Do not suggest workarounds to the blocked
  tool." So `muzzle hook cursor` denies a matched command with a reason
  that says this is a queue and to run the `muzzle run …` rewrite, which
  stands alone on the reason's last line, and replies `{}` to the rerun,
  which already runs muzzle. `agent_message` carries the same text.
- Exit 2 blocks the call. Any other non-zero exit, or empty stdout, is a
  failed hook and the call runs. Stdout that isn't JSON blocks the call,
  so `muzzle hook` prints nothing when it can't answer.
