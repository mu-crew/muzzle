#!/usr/bin/env node
// Pre-publish guard: `npm pack --dry-run --json` of each platform package must
// list an executable bin/muzzle. 0.3.1 published platform packages holding only
// package.json; run this before any `npm publish`.

const { execFileSync } = require("node:child_process");
const path = require("node:path");

const root = path.resolve(__dirname, "..");
const packages = ["muzzle-linux-x64", "muzzle-linux-arm64", "muzzle-darwin-arm64"];

let failed = false;
for (const name of packages) {
  const dir = path.join(root, "npm", name);
  let files;
  try {
    const out = execFileSync("npm", ["pack", "--dry-run", "--json"], {
      cwd: dir,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "inherit"],
    });
    files = JSON.parse(out)[0].files;
  } catch (err) {
    console.error(`${name}: npm pack failed: ${err.message}`);
    failed = true;
    continue;
  }
  const bin = files.find((f) => f.path === "bin/muzzle");
  if (!bin) {
    console.error(`${name}: bin/muzzle is not in the package`);
    failed = true;
  } else if (!(bin.mode & 0o111)) {
    console.error(`${name}: bin/muzzle is not executable (mode ${bin.mode.toString(8)})`);
    failed = true;
  } else if (!bin.size) {
    console.error(`${name}: bin/muzzle is empty`);
    failed = true;
  } else {
    console.log(`${name}: bin/muzzle ok (${bin.size} bytes)`);
  }
}
process.exit(failed ? 1 : 0);
