// Drive the wasm-bindgen bundle the same way web/index.html does, but from
// Node, to reproduce browser-side traps with a usable stack.
import { readFile } from "node:fs/promises";
import init, { instantiate } from "./web/pkg/starstream_run_web.js";

const wasm = await readFile(new URL("./web/pkg/starstream_run_web_bg.wasm", import.meta.url));
await init({ module_or_path: wasm });

const guest = await readFile(new URL("./web/score.wasm", import.meta.url));
const contract = instantiate(guest);
const desc = JSON.parse(contract.describe());
console.log("describe:", JSON.stringify(desc, null, 2));

const inst = desc.instances[0];
const ctor = inst.constructors[0];
console.log(`calling ${inst.name} / ${ctor.export} ...`);
const res = contract.call(inst.name, ctor.export, "[]");
console.log("result:", res);

const [{ $handle: id }] = JSON.parse(res);
const method = (name, args) =>
  contract.call(inst.name, `[method]utxo.${name}`, JSON.stringify([{ $handle: id }, ...args]));
method("plus-chips", [10]);
method("plus-mult", [4]);
method("mult-mult", [150]);
const storage = JSON.parse(contract.storageGet(id));
console.log("storage:", storage);
if (storage.chips !== 10 || storage.mult !== 6) throw new Error("unexpected storage");
method("finish", []);
contract.dropResource(id);
console.log("ok");
