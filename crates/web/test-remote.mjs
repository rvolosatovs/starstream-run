// Smoke test for web/wrpc.js against the real CLI `--serve` server.
//
// Spawns `starstream-run --serve <addr> new <score.wasm>`, waits for it to come
// up, then drives the browser wRPC client end-to-end with Node's global
// `fetch` and `WebSocket`: fetch + parse the WIT, invoke the score methods over
// WebSockets, and assert the round-trip works and the wire-level value codec
// agrees with the CLI's. Run with `node test-remote.mjs` from `crates/web`.

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import assert from "node:assert/strict";

import { parseWit, RemoteContract } from "./web/wrpc.js";

const repo = fileURLToPath(new URL("../../", import.meta.url));
const cli = `${repo}target/debug/starstream-run`;
const wasm = `${repo}examples/components/score/target/wasm32-unknown-unknown/release/score.wasm`;
const addr = "127.0.0.1:18080";

// 1. Unit-check the WIT parser on the exact text the CLI emits.
const sample = `package starstream:utxo;

interface score-progress {
    plus-chips: func(chips2: u64);
    plus-mult: func(mult2: u64);
    mult-mult: func(mult-pct: u64);
    finish: func();
}
`;
const parsed = parseWit(sample);
assert.equal(parsed.package, "starstream:utxo");
assert.equal(parsed.interface, "score-progress");
assert.deepEqual(
  parsed.funcs.map((f) => f.name),
  ["plus-chips", "plus-mult", "mult-mult", "finish"],
);
assert.equal(parsed.funcs[0].params[0].name, "chips2");
assert.equal(parsed.funcs[0].params[0].ty.kind, "u64");
assert.deepEqual(parsed.funcs[3].params, []);
console.log("ok: WIT parser");

// 2. End-to-end against the live CLI.
const child = spawn(cli, ["--serve", addr, "new", wasm], { stdio: ["ignore", "pipe", "inherit"] });
let stdout = "";
child.stdout.on("data", (b) => (stdout += b));

try {
  // Wait for the HTTP/WS endpoint to accept connections.
  const deadline = Date.now() + 8000;
  for (;;) {
    try {
      const res = await fetch(`http://${addr}/`);
      if (res.ok) break;
    } catch {}
    if (Date.now() > deadline) throw new Error("the CLI never started serving");
    await sleep(100);
  }

  const remote = new RemoteContract(addr);
  const wit = await remote.connect();
  assert.match(wit, /interface score-progress/);

  const api = JSON.parse(remote.describe());
  assert.equal(api.instances.length, 1);
  assert.equal(api.instances[0].name, "starstream:utxo/score-progress");
  assert.deepEqual(
    api.instances[0].methods.map((m) => m.export),
    ["plus-chips", "plus-mult", "mult-mult", "finish"],
  );
  assert.deepEqual(
    JSON.parse(remote.implementedMethods()).sort(),
    ["finish", "mult-mult", "plus-chips", "plus-mult"],
  );
  console.log("ok: connect + describe");

  const inst = "starstream:utxo/score-progress";
  // Methods return nothing — a successful round-trip yields an empty array.
  assert.deepEqual(JSON.parse(await remote.call(inst, "plus-chips", JSON.stringify([7]))), []);
  assert.deepEqual(JSON.parse(await remote.call(inst, "plus-mult", JSON.stringify([6]))), []);
  // The injected `self` `$handle` (as the panel sends it) must be dropped.
  assert.deepEqual(
    JSON.parse(await remote.call(inst, "mult-mult", JSON.stringify([{ $handle: 0 }, 200]))),
    [],
  );
  assert.deepEqual(JSON.parse(await remote.call(inst, "finish", JSON.stringify([]))), []);
  console.log("ok: invoked plus-chips / plus-mult / mult-mult / finish over WebSockets");

  console.log("\nALL PASSED");
} finally {
  child.kill("SIGKILL");
}
