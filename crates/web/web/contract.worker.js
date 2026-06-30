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
//
// UTXO *methods* are not invoked via a plain `call` op: the page invokes them
// over wRPC (`{ op: "invoke", ... }`), handing this Worker a duplex stream pair
// over which it serves the invocation (`serveInvocation`) — decoding the
// wRPC-framed parameters, dispatching to the wasm `Contract::call`, and
// encoding the results back. The `{ id, ok, error }` response (and any drained
// events) still travels over `postMessage`, so the page learns whether the
// guest ran to completion.

import { instantiate, initSync } from "./pkg/starstream_run_web.js";
import { setup } from "./fiber-env.js";
import { serveInvocation, streamTransport } from "./wrpc.js";

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
  // Serve one wRPC method invocation over the transferred stream pair. The
  // `self` receiver is omitted on the wire (as for a CLI-served UTXO), so the
  // target handle rides in the message envelope and is re-injected here as the
  // `{ $handle }` first argument the wasm `Contract::call` expects.
  invoke: async ({ handle, instance, func, paramTypes, resultTypes, readable, writable }) =>
    serveInvocation(
      streamTransport({ readable, writable }),
      paramTypes,
      resultTypes,
      async (jsonArgs) => {
        const args = [{ $handle: handle }, ...jsonArgs];
        return JSON.parse(await contract.call(instance, func, JSON.stringify(args)));
      },
    ),
  loadUtxo: ({ instance, storage }) => contract.loadUtxo(instance, storage),
  storageGet: ({ handle }) => contract.storageGet(handle),
  implementedMethods: ({ handle }) => contract.implementedMethods(handle),
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
  // Drain any ABI events the guest emitted during this op and ship them with
  // the response, so the page can show them in the invocation panel's log.
  const drainEvents = () => {
    try {
      return contract ? JSON.parse(contract.drainEvents()) : [];
    } catch {
      return [];
    }
  };
  try {
    await booted;
    const handler = ops[op];
    if (!handler) throw new Error(`unknown op: ${op}`);
    const result = await handler(data);
    postMessage({ id, ok: true, result, events: drainEvents() });
  } catch (err) {
    postMessage({ id, ok: false, error: String(err?.message ?? err), events: drainEvents() });
  }
};
