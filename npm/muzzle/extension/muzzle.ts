// muzzle pi extension (spec §Integrations "pi extension"; D22, D24, D26).
//
// tool_call on bash: ask `muzzle wrap` for the rewrite (D22: matching,
// quoting and the timeout move live in the CLI). On a match, run the wrapped
// command with `--report <tmp>/<id>.json` and drop pi's own timeout, so the
// queue wait can't trip it (D24: it is now `--run-timeout`).
// tool_result: read the report, append `muzzle: waited Ws for N slots, ran
// Rs` and a `muzzle` custom entry with the same numbers (D26).
//
// One copy per pi process: the pi package and `-e <path>` can both load this
// file, and two copies would each wrap the call (the CLI also refuses to wrap
// a command that already runs muzzle, D22). The first copy to load claims
// GUARD on globalThis; later copies register nothing. The claim is released
// on session_shutdown, so /reload and session replacement load it again.
//
// Program status (OSC 7501): pi's bash tool has no terminal, so `muzzle run`
// can't tell the terminal it is queued. Each wrapped call also gets
// `--status <tmp>/<id>.status`, which muzzle keeps holding its current
// report while queued; in interactive mode in a terminal that supports the
// protocol, this extension forwards changes to pi's own output, under a child
// id per call (`muzzle/<n>`) so parallel calls and pi's root record stay apart.
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

// `muzzle wrap` only reads config and matches the rules; it answers in a few
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

/** Terminals known to support OSC 7501, as muzzle's own sink checks. */
const STATUS_TERMINALS = new Set(["rex"]);

/** How often a queued call's status file is checked. muzzle polls every 250 ms. */
const STATUS_POLL_MS = 250;

/** Whether this pi should forward program status: interactive, in a supporting terminal. */
export function forwardsStatus(mode: string | undefined, env: NodeJS.ProcessEnv = process.env): boolean {
  return mode === "tui" && STATUS_TERMINALS.has(env.TERM_PROGRAM ?? "");
}

/** muzzle's report with its record moved to this call's child id. */
export function forCall(report: string, slot: number): string | null {
  if (!report.startsWith("\x1b]7501;") || !report.includes(":id=muzzle")) return null;
  return report.replace(/:id=muzzle(?=[:\x1b])/, `:id=muzzle/${slot}`);
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

/** The process-wide claim; registry-global so separate module copies share it. */
export const GUARD = Symbol.for("@mu-crew/muzzle/extension");

export interface Deps {
  findBinary?: () => string | null;
  tmpRoot?: string;
  /** Where forwarded status goes: pi's terminal. */
  writeTerminal?: (s: string) => void;
}

export function createExtension(deps: Deps = {}) {
  const findBinary = deps.findBinary ?? (() => findMuzzle());
  return function (pi: ExtensionAPI) {
    const g = globalThis as Record<symbol, unknown>;
    if (g[GUARD] !== undefined) return;
    const claim = {};
    g[GUARD] = claim;
    let bin: string | null | undefined;
    let reportDir: string | undefined;
    const pending = new Map<string, string>();
    // A wrap-failure warning is on the footer; the next good wrap clears it.
    let wrapWarned = false;

    // toolCallId -> the status file, its child slot and the last report sent.
    const watched = new Map<string, { file: string; slot: number; sent: string }>();
    let nextSlot = 0;
    let poller: ReturnType<typeof setInterval> | undefined;
    const writeTerminal = deps.writeTerminal ?? ((s: string) => process.stdout.write(s));
    const send = (s: string) => {
      try {
        writeTerminal(s);
      } catch {
        // a closed terminal; status is a courtesy
      }
    };
    const forward = () => {
      for (const w of watched.values()) {
        let report: string;
        try {
          report = readFileSync(w.file, "utf8");
        } catch {
          continue; // not queued (yet): muzzle writes it only while waiting
        }
        if (report === w.sent) continue;
        const out = forCall(report, w.slot);
        if (out === null) continue;
        w.sent = report;
        send(out);
      }
    };
    const unwatch = (toolCallId: string) => {
      const w = watched.get(toolCallId);
      if (w === undefined) return;
      watched.delete(toolCallId);
      // Read once more, then make sure no record outlives the call (a killed
      // muzzle never writes its clear).
      forward();
      if (w.sent !== "" && !w.sent.includes("state=clear")) send(`\x1b]7501;state=clear:id=muzzle/${w.slot}\x1b\\`);
      rmSync(w.file, { force: true });
      if (watched.size === 0 && poller !== undefined) {
        clearInterval(poller);
        poller = undefined;
      }
    };

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
        // Belt and braces for the guard: our own rewrite is never rewritten.
        if (input.command.startsWith(`${shellQuote(bin)} run `)) return;
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
        let status = "";
        if (forwardsStatus(ctx.mode)) {
          const file = `${report.slice(0, -".json".length)}.status`;
          rmSync(file, { force: true });
          watched.set(event.toolCallId, { file, slot: nextSlot++, sent: "" });
          poller ??= setInterval(forward, STATUS_POLL_MS);
          poller.unref?.();
          status = `--status ${shellQuote(file)} `;
        }
        input.command = `${shellQuote(bin)} run --report ${shellQuote(report)} ${status}${wrapped.slice("muzzle run ".length)}`;
        delete input.timeout;
        pending.set(event.toolCallId, report);
      } catch (e) {
        wrapWarned = true;
        warn(ctx, `muzzle wrap failed, calls run unwrapped: ${e instanceof Error ? e.message : String(e)}`);
      }
    });

    pi.on("tool_result", async (event) => {
      unwatch(event.toolCallId);
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
      for (const id of [...watched.keys()]) unwatch(id);
      if (g[GUARD] === claim) delete g[GUARD];
      if (reportDir) rmSync(reportDir, { recursive: true, force: true });
      reportDir = undefined;
      pending.clear();
    });
  };
}

export default createExtension();
