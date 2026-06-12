// Drive the wasm-bindgen bundle the way a contract Worker does, but from Node,
// to reproduce browser-side traps with a usable stack.
//
// Requires JSPI for the wasmtime fiber glue:
//   node --experimental-wasm-jspi repro.mjs   (Node ≥ 24)
import { readFile } from "node:fs/promises";

if (typeof WebAssembly.Suspending !== "function") {
  console.error("JSPI is unavailable — run with: node --experimental-wasm-jspi repro.mjs");
  process.exit(1);
}

// The generated glue imports the JSPI fiber glue from ../fiber-env.js directly
// (a relative specifier — see crates/web/src/fiber.rs), so no module-resolution
// shim is needed here; it resolves the same in Node as in the browser/Worker.
import init, { instantiate } from "./web/pkg/starstream_run_web.js";
import { setup as setupFibers } from "./web/fiber-env.js";

const wasm = await readFile(new URL("./web/pkg/starstream_run_web_bg.wasm", import.meta.url));
setupFibers(await init({ module_or_path: wasm }));

const guest = await readFile(new URL("./web/score.wasm", import.meta.url));
const contract = instantiate(guest);
const desc = JSON.parse(contract.describe());
console.log("describe:", JSON.stringify(desc, null, 2));

const inst = desc.instances[0];
const ctor = inst.constructors[0];
console.log(`calling ${inst.name} / ${ctor.export} ...`);
const res = await contract.call(inst.name, ctor.export, "[]");
console.log("result:", res);

const [{ $handle: id }] = JSON.parse(res);
const method = (name, args) =>
  contract.call(inst.name, `[method]utxo.${name}`, JSON.stringify([{ $handle: id }, ...args]));
await method("plus-chips", [10]);
await method("plus-mult", [4]);
await method("mult-mult", [150]);
const storage = JSON.parse(await contract.storageGet(id));
console.log("storage:", storage);
if (storage.chips !== 10 || storage.mult !== 6) throw new Error("unexpected storage");
await method("finish", []);
contract.dropResource(id);
console.log("ok");
