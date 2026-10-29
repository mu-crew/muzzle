// UNTESTED (spec D23): an example opencode plugin, not run by any test.
// Copy to .opencode/plugins/ or ~/.config/opencode/plugins/. opencode issue
// #42409 reports that some versions ignore a changed `output.args.command`;
// check `muzzle status` shows the claim before relying on it.
import type { Plugin } from "@opencode-ai/plugin"

export const Muzzle: Plugin = async ({ $, directory }) => ({
  "tool.execute.before": async (input, output) => {
    const cmd = output.args?.command
    if (input.tool !== "bash" || typeof cmd !== "string") return
    // opencode's bash `timeout` is in ms; it still counts the wait (D24).
    const t = output.args.timeout ? ["--timeout", String(output.args.timeout / 1000)] : []
    try {
      const r = await $`muzzle wrap --json --cwd ${output.args.workdir ?? directory} ${t} -- ${cmd}`.quiet().json()
      if (r.command) output.args.command = r.command
    } catch {} // muzzle missing or failing: the call runs as it is
  },
})
