"use strict";

const assert = require("node:assert/strict");
const path = require("node:path");
const process = require("node:process");
const pty = require("node-pty");

if (process.platform !== "win32") {
  process.exit(0);
}

const input = [
  "\x1b[A",
  "\x1b[B",
  "\x1b[C",
  "\x1b[D",
  "\x1b[1;5A",
  "\x1b[1;5B",
  "\x1b[1;5C",
  "\x1b[1;5D",
  "\x1b.",
  "\x03",
].join("");
const expectedHex = Buffer.from(input, "utf8").toString("hex");
const executable = path.resolve("target", "debug", "mosh-client.exe");
const child = pty.spawn(executable, [], {
  cols: 80,
  rows: 24,
  cwd: process.cwd(),
  env: {
    ...process.env,
    MOSHCATTY_CONPTY_TEST: "1",
    MOSHCATTY_CONPTY_TEST_BYTES: String(Buffer.byteLength(input, "utf8")),
  },
});

let output = "";
let settled = false;
const timeout = setTimeout(() => {
  if (settled) return;
  settled = true;
  child.kill();
  assert.fail(`ConPTY input probe timed out: ${JSON.stringify(output)}`);
}, 10_000);

child.onData((data) => {
  output += data;
  const match = output.match(/MOSHCATTY_INPUT_HEX=([0-9a-f]+)/i);
  if (!match || settled) return;
  settled = true;
  clearTimeout(timeout);
  assert.equal(match[1].toLowerCase(), expectedHex);
  child.kill();
});

child.onExit(({ exitCode }) => {
  if (settled) return;
  settled = true;
  clearTimeout(timeout);
  assert.fail(`ConPTY input probe exited with ${exitCode}: ${JSON.stringify(output)}`);
});

setTimeout(() => child.write(input), 250);
