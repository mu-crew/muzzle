# Stored `PreToolUse` payloads

`muzzle hook claude|codex` is tested against these payloads
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
  `claude-bash-quoted-operator.json` (`'a&&b'` quoted) is still simple.
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
