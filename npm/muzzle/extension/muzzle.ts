// muzzle pi extension (spec §Integrations "pi extension"; D22, D24, D26).
//
// tool_call on bash: ask `muzzle wrap` for the rewrite (D22: matching,
// quoting and the timeout move live in the CLI). On a match, run the wrapped
// command with `--report <tmp>/<id>.json` and drop pi's own timeout, so the
// queue wait can't trip it (D24: it is now `--run-timeout`).
// tool_result: read the report, append `muzzle: waited Ws for N slots, ran
// Rs` and a `muzzle` custom entry with the same numbers (D26).
//
// Nothing here may block a call: a missing binary or a failing wrap leaves
// the call as it is. A throwing tool_call handler blocks the tool in pi, so
// every path is caught.
import { execFile } from "node:child_process";
import { accessSync, closeSync, constants, mkdtempSync, openSync, readFileSync, readSync, rmSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const PLATFORM_PACKAGES: Record<string, string> = {
  "linux-x64": "@mu-crew/muzzle-linux-x64",
  "linux-arm64": "@mu-crew/muzzle-linux-arm64",
  "darwin-arm64": "@mu-crew/muzzle-darwin-arm64",
};

// `muzzle wrap` only reads config and matches a regex; it answers in a few
// ms. A wedged one must not hold the call for long.
const WRAP_TIMEOUT_MS = 5000;

export interface FindOptions {
  /** Resolve a package-relative path, as `require.resolve` does. */
  resolve?: (id: string) => string;
  platform?: string;
  path?: string;
}

function isExecutable(file: string): boolean {
  try {
    if (!statSync(file).isFile()) return false;
    accessSync(file, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * A script, not the binary: bin/muzzle from a global `npm i -g` is a node
 * shim. On a Node without process.execve it runs muzzle as a child, so
 * muzzle isn't the group leader and pi's group SIGKILL leaks the job and its
 * slots (D16).
 */
function isScript(file: string): boolean {
  let fd: number | undefined;
  try {
    fd = openSync(file, "r");
    const head = Buffer.alloc(2);
    return readSync(fd, head, 0, 2, 0) === 2 && head.toString("latin1") === "#!";
  } catch {
    return false;
  } finally {
    if (fd !== undefined) closeSync(fd);
  }
}

/**
 * The binary from this package's platform dependency first, then `muzzle` on
 * PATH; null when neither exists. The platform binary is used directly,
 * not through bin/muzzle, so a wrapped call doesn't pay for a node start
 * and muzzle leads the bash group (D16). PATH entries that are scripts are
 * skipped for the same reason.
 */
export function findMuzzle(opts: FindOptions = {}): string | null {
  const resolve = opts.resolve ?? createRequire(import.meta.url).resolve;
  const pkg = PLATFORM_PACKAGES[opts.platform ?? `${process.platform}-${process.arch}`];
  if (pkg) {
    try {
      const own = resolve(`${pkg}/bin/muzzle`);
      if (isExecutable(own)) return own;
    } catch {
      // not installed for this platform; fall through to PATH
    }
  }
  for (const dir of (opts.path ?? process.env.PATH ?? "").split(delimiter)) {
    if (!dir) continue;
    const candidate = join(dir, "muzzle");
    if (isExecutable(candidate) && !isScript(candidate)) return candidate;
  }
  return null;
}

/** POSIX single quoting, as `muzzle wrap` does it. */
export function shellQuote(s: string): string {
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

/** A file name for a toolCallId. Nested ids carry `/` (`<parent>/<n>`). */
export function reportName(toolCallId: string): string {
  return `${toolCallId.replace(/[^A-Za-z0-9_-]/g, (c) => `%${c.charCodeAt(0).toString(16)}`)}.json`;
}

export interface Report {
  waited_s: number;
  ran_s: number;
  slots: number | null;
  class: string | null;
  rule: string | null;
  exit: number;
  enforced: boolean;
  kept?: boolean;
}

/** D26, the same text `muzzle run` prints when `--report` is unset. */
export function resultLine(r: Report): string {
  // D17: a nested run claims nothing; its report has no slots.
  if (r.slots === null) return `muzzle: nested in an outer claim, ran ${Math.round(r.ran_s)}s`;
  return `muzzle: waited ${Math.round(r.waited_s)}s for ${r.slots} slots, ran ${Math.round(r.ran_s)}s`;
}

function wrap(bin: string, command: string, cwd: string, timeout?: number): Promise<string | null> {
  const args = ["wrap", "--json", "--cwd", cwd];
  if (typeof timeout === "number") args.push("--timeout", String(timeout));
  args.push("--", command);
  return new Promise((resolve, reject) => {
    execFile(bin, args, { timeout: WRAP_TIMEOUT_MS }, (err, stdout, stderr) => {
      if (err) return reject(new Error(stderr.trim() || err.message));
      try {
        const reply = JSON.parse(stdout);
        resolve(typeof reply.command === "string" ? reply.command : null);
      } catch (e) {
        reject(e);
      }
    });
  });
}

export interface Deps {
  findBinary?: () => string | null;
  tmpRoot?: string;
}

export function createExtension(deps: Deps = {}) {
  const findBinary = deps.findBinary ?? (() => findMuzzle());
  return function (pi: ExtensionAPI) {
    let bin: string | null | undefined;
    let reportDir: string | undefined;
    const pending = new Map<string, string>();
    // A wrap-failure warning is on the footer; the next good wrap clears it.
    let wrapWarned = false;

    const warn = (ctx: { ui: { setStatus(key: string, text: string | undefined): void } }, text: string | undefined) => {
      try {
        ctx.ui.setStatus("muzzle", text);
      } catch {
        // no UI in this mode
      }
    };

    pi.on("tool_call", async (event, ctx) => {
      try {
        if (event.toolName !== "bash") return;
        const input = event.input as { command?: unknown; timeout?: unknown };
        if (typeof input.command !== "string") return;
        if (bin === undefined) {
          bin = findBinary();
          // Looked up once per runtime, so the warning shows once.
          if (bin === null) warn(ctx, "muzzle not found: bash calls run unwrapped");
        }
        if (bin === null) return;
        const timeout = typeof input.timeout === "number" ? input.timeout : undefined;
        const wrapped = await wrap(bin, input.command, ctx.cwd, timeout);
        if (wrapWarned) {
          wrapWarned = false;
          warn(ctx, undefined);
        }
        if (wrapped === null || !wrapped.startsWith("muzzle run ")) return;
        reportDir ??= mkdtempSync(join(deps.tmpRoot ?? tmpdir(), "muzzle-pi-"));
        const report = join(reportDir, reportName(event.toolCallId));
        rmSync(report, { force: true });
        input.command = `${shellQuote(bin)} run --report ${shellQuote(report)} ${wrapped.slice("muzzle run ".length)}`;
        delete input.timeout;
        pending.set(event.toolCallId, report);
      } catch (e) {
        wrapWarned = true;
        warn(ctx, `muzzle wrap failed, calls run unwrapped: ${e instanceof Error ? e.message : String(e)}`);
      }
    });

    pi.on("tool_result", async (event) => {
      const report = pending.get(event.toolCallId);
      if (report === undefined) return;
      pending.delete(event.toolCallId);
      let r: Report;
      try {
        r = JSON.parse(readFileSync(report, "utf8"));
      } catch {
        // No report: aborted or killed before muzzle could write it.
        return;
      } finally {
        rmSync(report, { force: true });
      }
      const line = resultLine(r);
      const content = [...event.content];
      const last = content[content.length - 1];
      if (last?.type === "text") {
        content[content.length - 1] = { ...last, text: last.text.trimEnd() ? `${last.text.trimEnd()}\n\n${line}` : line };
      } else {
        content.push({ type: "text", text: line });
      }
      pi.appendEntry("muzzle", { toolCallId: event.toolCallId, ...r });
      // Keep structuredContent: replacing content alone would drop it.
      return { content, structuredContent: event.structuredContent };
    });

    pi.on("session_shutdown", async () => {
      if (reportDir) rmSync(reportDir, { recursive: true, force: true });
      reportDir = undefined;
      pending.clear();
    });
  };
}

export default createExtension();
