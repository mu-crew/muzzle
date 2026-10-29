Harness configs for `tests/status_doctor.rs`. `doctor` reports a harness as
installed when its config names `muzzle hook` under `hooks` (pi: a package or
extension entry naming muzzle). The Claude and Codex JSON snippets are read
from `docs/harnesses/` directly, so the documented ones are what is tested.

- `pi-settings.json`: `~/.pi/agent/settings.json` with the npm package.
- `codex-config.toml`: the inline `config.toml` form from `docs/harnesses`.
- `claude-settings-other-hook.json`: mentions muzzle outside `hooks` and has
  another hook: must read as `not found`.
