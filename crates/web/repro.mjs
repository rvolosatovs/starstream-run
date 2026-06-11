// Drive the wasm-bindgen bundle the same way web/index.html does, but from
// Node, to reproduce browser-side traps with a usable stack.
//
// Requires JSPI for the wasmtime fiber glue:
//   node --experimental-wasm-jspi repro.mjs   (Node ≥ 24)
import { readFile } from "node:fs/promises";
import { registerHooks } from "node:module";

if (typeof WebAssembly.Suspending !== "function") {
  console.error("JSPI is unavailable — run with: node --experimental-wasm-jspi repro.mjs");
  process.exit(1);
}

// The bundle imports the JSPI fiber glue as the bare specifier "env" (the
// browser resolves it via the import map in index.html); point it at
// fiber-env.js here. Registered before the bundle is imported, hence the
// dynamic imports below.
const envUrl = new URL("./web/fiber-env.js", import.meta.url).href;
registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === "env") {
      return { url: envUrl, shortCircuit: true };
    }
    return nextResolve(specifier, context);
  },
});

const { default: init, instantiate } = await import("./web/pkg/starstream_run_web.js");
const { setup: setupFibers } = await import("env");

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
