// One Starstream contract, running in its own Web Worker.
//
// Each loaded contract gets a dedicated Worker (its own JS realm), so the JSPI
// fiber glue in fiber-env.js — which keeps module-global state and permits only
// one in-flight root activation — is isolated per contract for free, and the
// contracts run on separate threads. The page (index.html) compiles the ~11 MB
// runtime module once and hands every Worker the same precompiled
// `WebAssembly.Module`, so there is no per-Worker recompile.
//
// Protocol (postMessage):
//   page -> worker: { type: "boot", module }          // the precompiled runtime
//                   { id, op, ...args }                 // an RPC call, see `ops`
//   worker -> page: { type: "ready" } | { type: "boot-error", error }
//                   { type: "log", text }               // runtime tracing / panics
//                   { id, ok: true, result } | { id, ok: false, error }

import { instantiate, initSync } from "./pkg/starstream_run_web.js";
import { setup } from "./fiber-env.js";

// A Worker has no `document`, so the runtime's on-page log bridge no-ops here;
// forward what it writes to the console (tracing lines, panics) to the page,
// which routes it to this contract's log panel.
for (const level of ["log", "info", "warn", "error", "debug"]) {
  const original = console[level].bind(console);
  console[level] = (...args) => {
    original(...args);
    postMessage({ type: "log", text: args.map(String).join(" ") });
  };
}

// The single contract this Worker hosts; created by the `instantiate` op.
let contract = null;

// RPC handlers — each mirrors a method on the wasm-bindgen `Contract`. Values
// cross as JSON strings; the page owns the JSON <-> component-value mapping.
const ops = {
  instantiate: ({ bytes }) => void (contract = instantiate(bytes)),
  describe: () => contract.describe(),
  call: ({ instance, func, args }) => contract.call(instance, func, args),
  loadUtxo: ({ instance, storage }) => contract.loadUtxo(instance, storage),
  storageGet: ({ handle }) => contract.storageGet(handle),
  storageSet: ({ handle, value }) => contract.storageSet(handle, value),
  dropResource: ({ handle }) => contract.dropResource(handle),
  setCardano: ({ blockHeight, currentSlot }) =>
    void contract.setCardano(blockHeight, currentSlot),
};

// Resolves once the runtime is instantiated and the fiber glue is wired; every
// RPC awaits it so calls that arrive before boot finishes just queue.
let booted = null;

onmessage = async ({ data }) => {
  if (data.type === "boot") {
    // Instantiate the precompiled runtime synchronously (no recompile) and wire
    // the JSPI fiber glue to its exports.
    booted = Promise.resolve().then(() => setup(initSync({ module: data.module })));
    booted.then(
      () => postMessage({ type: "ready" }),
      (err) => postMessage({ type: "boot-error", error: String(err) }),
    );
    return;
  }

  const { id, op } = data;
  try {
    await booted;
    const handler = ops[op];
    if (!handler) throw new Error(`unknown op: ${op}`);
    postMessage({ id, ok: true, result: await handler(data) });
  } catch (err) {
    postMessage({ id, ok: false, error: String(err?.message ?? err) });
  }
};
