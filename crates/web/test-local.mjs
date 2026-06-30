// Unit test for the in-browser wRPC plumbing in web/wrpc.js: the duplex
// `streamTransport`, the client `callOverTransport`, and the server
// `serveInvocation`, wired to each other over an in-process pair of
// `TransformStream`s (the browser transfers one end of each to the contract's
// Web Worker; here both ends live in the same realm). The wasm guest is
// stubbed by the `bridge` callback. Run with `node test-local.mjs` from
// `crates/web` (after `npm run build:wrpc`).

import assert from "node:assert/strict";

import { t } from "./web/vendor/wrpc/index.js";
import {
  streamTransport,
  callOverTransport,
  serveInvocation,
  valueToJson,
} from "./web/wrpc.js";

const INSTANCE = "starstream:utxo/score-progress";

// Connect a client transport to a server transport over two TransformStreams,
// serve one invocation with `bridge`, and drive the client. Returns the
// client's JSON result string; rejects if either side errors.
async function roundtrip(func, paramTypes, resultTypes, args, bridge) {
  const c2s = new TransformStream(); // client -> server
  const s2c = new TransformStream(); // server -> client
  const client = streamTransport({ readable: s2c.readable, writable: c2s.writable });
  const server = streamTransport({ readable: c2s.readable, writable: s2c.writable });

  const served = serveInvocation(server, paramTypes, resultTypes, bridge);
  const results = await callOverTransport(client, INSTANCE, func, paramTypes, resultTypes, args);
  await served;
  return results;
}

// 1. valueToJson lowers the package's `jco` value shapes to the page's JSON.
assert.equal(valueToJson(t.u64, 84n), 84);
assert.equal(valueToJson(t.u64, 9007199254740993n), "9007199254740993"); // > 2^53 -> string
assert.equal(valueToJson(t.option(t.u32), undefined), null);
assert.deepEqual(valueToJson(t.list(t.u8), new Uint8Array([1, 2, 3])), [1, 2, 3]);
assert.deepEqual(
  valueToJson(t.record({ a: t.u64, b: t.string }), { a: 5n, b: "hi" }),
  { a: 5, b: "hi" },
);
console.log("ok: valueToJson");

// 2. A result-less method (the score methods): the server sees the decoded
// argument and returns nothing; the client gets an empty result array.
{
  let seen = null;
  const out = await roundtrip("plus-chips", [t.u64], [], [7], async (jsonArgs) => {
    seen = jsonArgs;
    return [];
  });
  assert.deepEqual(seen, [7]);
  assert.equal(out, "[]");
  console.log("ok: result-less invocation");
}

// 3. A method with results: arguments flow client -> server, results flow back
// and are lowered to JSON (u64 -> number).
{
  const out = await roundtrip("double", [t.u64], [t.u64, t.string], [21], async ([n]) => {
    assert.equal(n, 21);
    return [n * 2, "ok"];
  });
  assert.deepEqual(JSON.parse(out), [42, "ok"]);
  console.log("ok: invocation with results");
}

// 4. A bridge failure: the server invocation rejects, and (because the method
// is result-less) the client still resolves — in the browser the Worker's
// `postMessage` error response is what surfaces the failure to the page.
{
  const c2s = new TransformStream();
  const s2c = new TransformStream();
  const client = streamTransport({ readable: s2c.readable, writable: c2s.writable });
  const server = streamTransport({ readable: c2s.readable, writable: s2c.writable });

  const served = serveInvocation(server, [t.u64], [], async () => {
    throw new Error("not callable");
  });
  await callOverTransport(client, INSTANCE, "boom", [t.u64], [], [1]);
  await assert.rejects(served, /not callable/);
  console.log("ok: bridge failure rejects the server side");
}

console.log("\nALL PASSED");
