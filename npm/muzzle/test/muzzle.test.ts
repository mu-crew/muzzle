// Extension tests (spec §Testing "Extension tests"). They drive the real
// muzzle binary: `MUZZLE_BIN`, or target/debug/muzzle from `cargo build`.
// Each test gets a private XDG config and state.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { beforeEach, test } from "node:test";
import { fileURLToPath } from "node:url";

import { createExtension, findMuzzle, GUARD, reportName, resultLine } from "../extension/muzzle.ts";

const here = dirname(fileURLToPath(import.meta.url));
const BIN = process.env.MUZZLE_BIN ?? resolve(here, "../../../target/debug/muzzle");
assert.ok(existsSync(BIN), `no muzzle binary at ${BIN}; run cargo build or set MUZZLE_BIN`);

const CONFIG = `
[[rule]]
name = "gate"
match = 'fake-gate'
slots = 1
`;

let home: string;
beforeEach(() => {
  home = mkdtempSync(join(tmpdir(), "muzzle-ext-test-"));
  mkdirSync(join(home, "config/muzzle"), { recursive: true });
  writeFileSync(join(home, "config/muzzle/config.toml"), CONFIG);
  process.env.XDG_CONFIG_HOME = join(home, "config");
  process.env.XDG_STATE_HOME = join(home, "state");
  // A test run under muzzle must not read as nested.
  delete process.env.MUZZLE_HOLD;
  delete process.env.MUZZLE_CLASS;
  // Each test loads a fresh copy; the process-wide claim would refuse it.
  delete (globalThis as any)[GUARD];
});

type Handler = (event: any, ctx: any) => any;

/** Just enough of ExtensionAPI and ExtensionContext for this extension. */
function load(findBinary: () => string | null = () => BIN, handlers: Record<string, Handler[]> = {}) {
  const entries: { type: string; data: any }[] = [];
  const status: (string | undefined)[] = [];
  const pi = {
    on(name: string, h: Handler) {
      (handlers[name] ??= []).push(h);
    },
    appendEntry(type: string, data: any) {
      entries.push({ type, data });
    },
  };
  createExtension({ findBinary, tmpRoot: home })(pi as any);
  const ctx = { cwd: home, ui: { setStatus: (_k: string, t: string | undefined) => status.push(t) } };
  const emit = async (name: string, event: any) => {
    let out;
    for (const h of handlers[name] ?? []) out = await h(event, ctx);
    return out;
  };
  return { emit, entries, status, handlers };
}

function call(id: string, input: Record<string, unknown>) {
  return { type: "tool_call", toolName: "bash", toolCallId: id, input };
}

/** Run a bash tool input the way pi does: `bash -c <command>`. */
function runBash(command: string) {
  return execFileSync("bash", ["-c", command], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
}

function result(id: string, input: Record<string, unknown>, text: string) {
  return {
    type: "tool_result",
    toolName: "bash",
    toolCallId: id,
    input,
    content: [{ type: "text", text }],
    structuredContent: { output: text, exit_code: 0 },
    isError: false,
    details: undefined,
  };
}

test("a matching call is rewritten through muzzle wrap, with a report per toolCallId", async () => {
  const { emit } = load();
  const input: Record<string, unknown> = { command: `echo fake-gate "it's" $HOME` };
  await emit("tool_call", call("call_1", input));
  const cmd = input.command as string;
  assert.ok(cmd.startsWith(`'${BIN}' run --report '`), cmd);
  assert.ok(cmd.includes(`/${reportName("call_1")}' --rule=gate --slots 1 -- bash -c `), cmd);
  // The original string survives the quoting: run it.
  assert.match(runBash(cmd), /^fake-gate it's \/\S*\n$/);
});

test("a non-matching call is left alone", async () => {
  const { emit } = load();
  const input = { command: "ls -la", timeout: 30 };
  await emit("tool_call", call("c", input));
  assert.deepEqual(input, { command: "ls -la", timeout: 30 });
});

test("other tools are left alone", async () => {
  const { emit } = load();
  const input = { command: "fake-gate" };
  await emit("tool_call", { type: "tool_call", toolName: "read", toolCallId: "c", input });
  assert.deepEqual(input, { command: "fake-gate" });
});

test("the timeout moves to --run-timeout and leaves the input (D24)", async () => {
  const { emit } = load();
  const input: Record<string, unknown> = { command: "fake-gate", timeout: 120 };
  await emit("tool_call", call("c", input));
  assert.ok(!("timeout" in input), JSON.stringify(input));
  assert.match(input.command as string, / --run-timeout 120 -- bash -c 'fake-gate'$/);
});

test("without a timeout there is no --run-timeout", async () => {
  const { emit } = load();
  const input: Record<string, unknown> = { command: "fake-gate" };
  await emit("tool_call", call("c", input));
  assert.doesNotMatch(input.command as string, /--run-timeout/);
});

test("the result gets the D26 line and a muzzle custom entry, and the report is deleted", async () => {
  const { emit, entries } = load();
  const input: Record<string, unknown> = { command: "echo fake-gate" };
  await emit("tool_call", call("call/1", input));
  const out = runBash(input.command as string);
  const reports = readdirSync(home).filter((d) => d.startsWith("muzzle-pi-"));
  assert.equal(reports.length, 1);
  const dir = join(home, reports[0]);
  assert.deepEqual(readdirSync(dir), [reportName("call/1")]);

  const r = await emit("tool_result", result("call/1", input, out));
  const text: string = r.content[0].text;
  assert.match(text, /^fake-gate\n\nmuzzle: waited \d+s for 1 slots, ran \d+s$/);
  assert.deepEqual(r.structuredContent, { output: out, exit_code: 0 }, "structuredContent is kept");
  assert.equal(entries.length, 1);
  assert.equal(entries[0].type, "muzzle");
  const e = entries[0].data;
  assert.equal(e.toolCallId, "call/1");
  assert.equal(e.slots, 1);
  assert.equal(e.rule, "gate");
  assert.equal(e.exit, 0);
  assert.equal(typeof e.waited_s, "number");
  assert.equal(typeof e.ran_s, "number");
  assert.ok(text.endsWith(resultLine(e)), "line and entry carry the same numbers");
  assert.deepEqual(readdirSync(dir), [], "report file deleted");

  // A second result for the same id is not touched again.
  assert.equal(await emit("tool_result", result("call/1", input, out)), undefined);
});

test("parallel calls keep their own reports", async () => {
  const { emit, entries } = load();
  const a: Record<string, unknown> = { command: "echo fake-gate a" };
  const b: Record<string, unknown> = { command: "echo fake-gate b" };
  await emit("tool_call", call("a", a));
  await emit("tool_call", call("b", b));
  const outB = runBash(b.command as string);
  const outA = runBash(a.command as string);
  await emit("tool_result", result("b", b, outB));
  await emit("tool_result", result("a", a, outA));
  assert.deepEqual(
    entries.map((e) => e.data.toolCallId),
    ["b", "a"],
  );
});

test("a result with no report (aborted before muzzle wrote it) is left alone", async () => {
  const { emit, entries } = load();
  const input: Record<string, unknown> = { command: "fake-gate" };
  await emit("tool_call", call("c", input));
  assert.equal(await emit("tool_result", result("c", input, "Command aborted")), undefined);
  assert.equal(entries.length, 0);
});

test("a result for an unwrapped call is left alone", async () => {
  const { emit, entries } = load();
  const input = { command: "ls" };
  await emit("tool_call", call("c", input));
  assert.equal(await emit("tool_result", result("c", input, "x")), undefined);
  assert.equal(entries.length, 0);
});

test("muzzle missing: one footer warning, every call untouched", async () => {
  let lookups = 0;
  const { emit, status } = load(() => {
    lookups++;
    return null;
  });
  for (const id of ["a", "b", "c"]) {
    const input = { command: "fake-gate", timeout: 5 };
    await emit("tool_call", call(id, input));
    assert.deepEqual(input, { command: "fake-gate", timeout: 5 });
  }
  assert.equal(lookups, 1);
  assert.equal(status.length, 1);
  assert.match(status[0] ?? "", /muzzle not found/);
});

test("a failing wrap never blocks: the call runs as it is", async () => {
  // Bad config makes `muzzle wrap` exit 2.
  writeFileSync(join(home, "config/muzzle/config.toml"), "nonsense = [");
  const { emit, status } = load();
  const input = { command: "fake-gate", timeout: 5 };
  await assert.doesNotReject(emit("tool_call", call("c", input)));
  assert.deepEqual(input, { command: "fake-gate", timeout: 5 });
  assert.match(status.at(-1) ?? "", /muzzle wrap failed/);
});

test("a good wrap after a failed one clears the footer warning", async () => {
  writeFileSync(join(home, "config/muzzle/config.toml"), "nonsense = [");
  const { emit, status } = load();
  await emit("tool_call", call("a", { command: "fake-gate" }));
  assert.match(status.at(-1) ?? "", /muzzle wrap failed/);
  // Config fixed mid-session: the next wrap succeeds.
  writeFileSync(join(home, "config/muzzle/config.toml"), CONFIG);
  const input: Record<string, unknown> = { command: "fake-gate" };
  await emit("tool_call", call("b", input));
  assert.match(input.command as string, / run --report /);
  assert.equal(status.at(-1), undefined, JSON.stringify(status));
  // Nothing more to clear: later calls leave the footer alone.
  const n = status.length;
  await emit("tool_call", call("c", { command: "fake-gate" }));
  assert.equal(status.length, n);
});

test("a wrap that answers garbage never blocks", async () => {
  const fake = join(home, "garbage-muzzle");
  writeFileSync(fake, "#!/bin/sh\necho not-json\n");
  chmodSync(fake, 0o755);
  const { emit } = load(() => fake);
  const input = { command: "fake-gate" };
  await assert.doesNotReject(emit("tool_call", call("c", input)));
  assert.deepEqual(input, { command: "fake-gate" });
});

test("findMuzzle: the package's own platform binary first, then PATH", () => {
  const own = join(home, "own/muzzle");
  const onPath = join(home, "path");
  mkdirSync(dirname(own), { recursive: true });
  mkdirSync(onPath);
  writeFileSync(own, "#!/bin/sh\n");
  writeFileSync(join(onPath, "muzzle"), "\x7fELF");
  for (const f of [own, join(onPath, "muzzle")]) chmodSync(f, 0o755);
  const seen: string[] = [];
  const resolveOwn = (id: string) => {
    seen.push(id);
    return own;
  };
  const missing = () => {
    throw new Error("MODULE_NOT_FOUND");
  };
  assert.equal(findMuzzle({ resolve: resolveOwn, platform: "linux-x64", path: onPath }), own);
  assert.deepEqual(seen, ["@mu-crew/muzzle-linux-x64/bin/muzzle"]);
  assert.equal(findMuzzle({ resolve: missing, platform: "linux-x64", path: onPath }), join(onPath, "muzzle"));
  assert.equal(findMuzzle({ resolve: resolveOwn, platform: "sunos-x64", path: onPath }), join(onPath, "muzzle"));
  assert.equal(findMuzzle({ resolve: missing, platform: "linux-x64", path: join(home, "none") }), null);
  // A script shim (bin/muzzle from `npm i -g`) is skipped for the next hit.
  const shimDir = join(home, "shim");
  mkdirSync(shimDir);
  writeFileSync(join(shimDir, "muzzle"), "#!/usr/bin/env node\n");
  chmodSync(join(shimDir, "muzzle"), 0o755);
  const both = `${shimDir}:${onPath}`;
  assert.equal(findMuzzle({ resolve: missing, platform: "linux-x64", path: both }), join(onPath, "muzzle"));
  assert.equal(findMuzzle({ resolve: missing, platform: "linux-x64", path: shimDir }), null);
  // Not executable is not found.
  chmodSync(join(onPath, "muzzle"), 0o644);
  assert.equal(findMuzzle({ resolve: missing, platform: "linux-x64", path: onPath }), null);
});

test("reportName keeps nested ids inside the report dir", () => {
  assert.equal(reportName("toolu_01AbC-x_y"), "toolu_01AbC-x_y.json");
  assert.ok(!reportName("p/1").includes("/"));
  assert.ok(!reportName("../x").includes("/"));
  assert.notEqual(reportName("a/b"), reportName("a_b"));
});

test("a nested run (inside an outer muzzle run, D17) gets a line without slots", async () => {
  const { emit, entries } = load();
  const input: Record<string, unknown> = { command: "echo fake-gate" };
  await emit("tool_call", call("n", input));
  const out = execFileSync(BIN, ["run", "--slots", "1", "--", "bash", "-c", input.command as string], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  });
  const r = await emit("tool_result", result("n", input, out));
  assert.match(r.content[0].text, /\n\nmuzzle: nested in an outer claim, ran \d+s$/);
  assert.equal(entries[0].data.slots, null);
});

test("two copies in one pi register once, and a wrapped call gets one result line", async () => {
  const first = load();
  // The second copy shares pi (the same handler table): the pi package plus -e.
  const second = load(() => BIN, first.handlers);
  assert.deepEqual(
    Object.fromEntries(Object.entries(first.handlers).map(([k, v]) => [k, v.length])),
    { tool_call: 1, tool_result: 1, session_shutdown: 1 },
  );
  const input: Record<string, unknown> = { command: "echo fake-gate" };
  await second.emit("tool_call", call("c", input));
  assert.equal((input.command as string).split(" run --report ").length, 2, input.command as string);
  const r = await second.emit("tool_result", result("c", input, runBash(input.command as string)));
  assert.equal(r.content[0].text.match(/^muzzle: /gm)?.length, 1, r.content[0].text);
  assert.equal(first.entries.length + second.entries.length, 1);
});

test("session_shutdown releases the claim, so a reload registers again", async () => {
  const first = load();
  await first.emit("session_shutdown", { type: "session_shutdown", reason: "reload" });
  assert.equal(load().handlers.tool_call?.length, 1);
});

test("a command already starting with this extension's own rewrite is not rewritten again", async () => {
  // A wrap that matches everything, so only the extension's own check stops a second rewrite.
  const fake = join(home, "always-muzzle");
  writeFileSync(fake, `#!/bin/sh\necho '{"command":"muzzle run --slots 1 -- bash -c x"}'\n`);
  chmodSync(fake, 0o755);
  const { emit } = load(() => fake);
  const input: Record<string, unknown> = { command: "echo fake-gate" };
  await emit("tool_call", call("a", input));
  const once = input.command as string;
  await emit("tool_call", call("b", input));
  assert.equal(input.command, once);
});

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function held(): number {
  return JSON.parse(execFileSync(BIN, ["status", "--json"], { encoding: "utf8" })).held;
}

async function until(what: string, ok: () => boolean) {
  for (let i = 0; i < 100; i++) {
    if (ok()) return;
    await sleep(50);
  }
  assert.fail(`timed out waiting for ${what}`);
}

test(
  "bin/muzzle execs the binary, so a group SIGKILL leaves no job and no slots (D16)",
  { skip: typeof (process as any).execve !== "function" && "this Node has no process.execve" },
  async () => {
    // An `npm i -g` layout: the shim finds its platform package by require.resolve.
    const platform = `${process.platform}-${process.arch}`;
    const modules = join(home, "lib/node_modules/@mu-crew");
    const shim = join(modules, "muzzle/bin/muzzle");
    mkdirSync(dirname(shim), { recursive: true });
    copyFileSync(resolve(here, "../bin/muzzle"), shim);
    chmodSync(shim, 0o755);
    mkdirSync(join(modules, `muzzle-${platform}/bin`), { recursive: true });
    symlinkSync(BIN, join(modules, `muzzle-${platform}/bin/muzzle`));

    const jobPid = join(home, "job.pid");
    // detached: a new session and group, as pi runs a bash call.
    const group = spawn(
      "bash",
      ["-c", `'${shim}' run --slots 1 -- bash -c 'echo $$ > ${jobPid}; exec sleep 30'`],
      { detached: true, stdio: "ignore" },
    );
    try {
      await until("the job to start", () => existsSync(jobPid) && readFileSync(jobPid, "utf8").endsWith("\n"));
      const job = Number(readFileSync(jobPid, "utf8"));
      assert.equal(held(), 1);
      process.kill(-group.pid!, "SIGKILL");
      await until("the job to die", () => !alive(job));
      await until("the slots to free", () => held() === 0);
    } finally {
      // On failure the job leaked into a group of its own: kill both.
      const job = existsSync(jobPid) ? Number(readFileSync(jobPid, "utf8")) : 0;
      for (const pgid of [group.pid!, job]) {
        try {
          if (pgid) process.kill(-pgid, "SIGKILL");
        } catch {
          // already gone
        }
      }
    }
  },
);
