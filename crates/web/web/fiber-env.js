// The embedder (JS) half of wasmtime's custom-fiber C ABI, implemented with
// JSPI (WebAssembly.Suspending / WebAssembly.promising), plus the `drive`
// runner the Rust side (`src/fiber.rs`) ships suspendable jobs to.
//
// The runtime module imports `fiber_init`/`fiber_switch` from the wasm module
// `env` (its Rust shims in `src/fiber.rs` forward wasmtime's
// `wasmtime_fiber_init`/`wasmtime_fiber_switch` hooks to them), and wasm-bindgen
// turns that into an ES import of the bare specifier "env" — which resolves to
// this file via the import map in `index.html` (browser) or the resolve hook in
// `repro.mjs` (Node).
//
// Each fiber record's `slot` holds the parked "other side" for that fiber's
// top-of-stack; a switch parks the caller, swaps the shadow-stack pointer,
// and wakes the other side — JSPI does the actual stack switching. The
// fiber's shadow stack starts at `top - 16` (the top 16 bytes are reserved by
// the wasmtime-fiber stack layout).
//
// INVARIANT: at most one in-flight root (promising) activation. Root
// activations share the module's main shadow stack, which is only safe LIFO —
// so `drive` chains jobs and runs them one at a time. Plain run-to-completion
// exports (describe, dropResource, the wasm-bindgen future-polling shims)
// called while a job is suspended nest LIFO and are fine; fibers *within* the
// job each get their own stack region and interleave freely.

let sp = null; // the module's exported __stack_pointer global
let enterFiber = null; // promising(exports.wasmtime_fiber_enter)
let runJob = null; // promising(exports.starstream_fiber_run)
const fibers = new Map(); // top_of_stack -> { entry, arg0, started, slot }

function jspiUnavailable() {
  throw new Error(
    "wasmtime async support requires JSPI (WebAssembly.Suspending) — Chromium ≥ 137, or Node ≥ 24 with --experimental-wasm-jspi",
  );
}

// Late-bind the wasm exports: this module is imported (as "env") *while* the
// runtime module is being instantiated, so the exports only exist afterwards.
// `init()` resolves to the exports object; the page passes it here before any
// contract call (fibers only run inside `drive` jobs, which check `runJob`).
export function setup(exports) {
  if (typeof WebAssembly.Suspending !== "function") {
    jspiUnavailable();
  }
  sp = exports.__stack_pointer;
  if (!(sp instanceof WebAssembly.Global)) {
    throw new Error(
      "the runtime module does not export __stack_pointer (stale build? crates/web/build.rs adds the link flag) — rebuild with `npm run build`",
    );
  }
  enterFiber = WebAssembly.promising(exports.wasmtime_fiber_enter);
  runJob = WebAssembly.promising(exports.starstream_fiber_run);
}

// Registers a new fiber whose stack spans up to `top`, scheduled to run
// `entry(arg0, top)` once it is first switched to.
export function fiber_init(top, entry, arg0) {
  fibers.set(top, { entry, arg0, started: false, slot: null });
}

// Symmetrically switch between the current execution context and the one
// associated with `top`: park the caller (the promise this Suspending import
// returns), swap the shadow-stack pointer, and wake the other side — resuming
// the fiber if it is suspended, or, when called from within the fiber itself,
// suspending the fiber and resuming whoever last switched into it. The first
// switch into a fiber enters the module through the `wasmtime_fiber_enter`
// trampoline as its own promising activation; when that activation settles,
// the fiber is dead and the final switch goes back to its last resumer — as a
// rejection if the activation trapped, so the resumer traps too instead of
// hanging.
function fiberSwitch(top) {
  const f = fibers.get(top);
  const me = { sp: sp.value, resolve: null, reject: null };
  const wait = new Promise((resolve, reject) => {
    me.resolve = resolve;
    me.reject = reject;
  });
  if (!f.started) {
    f.started = true;
    f.slot = me;
    sp.value = top - 16;
    enterFiber(f.entry, f.arg0, top).then(
      () => {
        const back = f.slot;
        fibers.delete(top);
        sp.value = back.sp;
        back.resolve();
      },
      (err) => {
        const back = f.slot;
        fibers.delete(top);
        sp.value = back.sp;
        back.reject(err);
      },
    );
  } else {
    const other = f.slot;
    f.slot = me;
    sp.value = other.sp;
    other.resolve();
  }
  return wait;
}

export const fiber_switch =
  typeof WebAssembly.Suspending === "function"
    ? new WebAssembly.Suspending(fiberSwitch)
    : jspiUnavailable;

// Parking for the Rust-side waker (`src/fiber.rs`'s `block_on`): when a
// wasmtime future returns Pending, the job activation suspends on
// `starstream_fiber_park` (a Suspending import) and the future's waker
// resumes it through `starstream_fiber_unpark`. Jobs are serialized (see
// `drive`), so at most one `block_on` is parked at a time and a single slot
// suffices. A wake with nobody parked — fired mid-poll, or by a waker that
// outlived its job — is remembered so the next park returns at once; that is
// only ever a spurious re-poll, which the waker contract allows.
let parked = null; // resolve fn of the parked activation's promise
let wakePending = false;

function park() {
  if (wakePending) {
    wakePending = false;
    return Promise.resolve();
  }
  if (parked !== null) {
    throw new Error("a fiber job is already parked — jobs must be serialized through drive()");
  }
  return new Promise((resolve) => {
    parked = resolve;
  });
}

export const starstream_fiber_park =
  typeof WebAssembly.Suspending === "function"
    ? new WebAssembly.Suspending(park)
    : jspiUnavailable;

export function starstream_fiber_unpark() {
  const resolve = parked;
  if (resolve !== null) {
    parked = null;
    resolve();
  } else {
    wakePending = true;
  }
}

// Run one staged job (`src/fiber.rs`'s `run`) on its own promising
// activation, serialized behind every previously queued job to uphold the
// one-root-activation invariant. The returned promise settles with the job's
// own outcome; the chain swallows failures so one trapped job doesn't poison
// the queue.
let chain = Promise.resolve();
export function drive(job) {
  if (runJob === null) {
    return Promise.reject(
      new Error("fiber glue not initialized — call setup(await init()) first"),
    );
  }
  const run = chain.then(() => runJob(job));
  chain = run.then(
    () => {},
    () => {},
  );
  return run;
}
