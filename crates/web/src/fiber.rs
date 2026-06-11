//! The embedder side of wasmtime's `custom-fiber` C ABI, plus the JSPI
//! plumbing that lets the async wasmtime APIs run in the browser.
//!
//! With the `custom-fiber` feature, wasmtime's async support (which normally
//! switches native stacks with inline assembly — impossible from inside a
//! wasm32 sandbox) calls out to two C-ABI hooks instead:
//! `wasmtime_fiber_init` and `wasmtime_fiber_switch`. On wasm targets those
//! are *imports* from the `env` module; the JS half lives in
//! [`web/fiber-env.js`](../web/fiber-env.js) — resolved as the bare module
//! specifier `env` via the import map in `index.html` (or the resolve hook in
//! `repro.mjs` under Node) — and implements them with JSPI
//! (`WebAssembly.Suspending` / `WebAssembly.promising`): the browser engine
//! does the actual stack switching, and the glue swaps this module's
//! shadow-stack pointer (`__stack_pointer`, exported via a link arg in
//! `build.rs`) on every switch.
//!
//! JSPI can only suspend an activation entered through a
//! `WebAssembly.promising`-wrapped export, and wasm-bindgen calls its exports
//! plainly. So anything that may suspend — every wasmtime `*_async` call;
//! each guest invocation runs on a fiber — is shipped as a *job* to
//! [`starstream_fiber_run`], a raw export the glue wraps in `promising`:
//! [`run`] stages the job, hands its address to the glue's `drive`, and
//! awaits the resulting promise. The glue serializes jobs so at most one such
//! root activation is in flight at a time — root activations share the main
//! shadow stack, which is only safe LIFO.

use core::future::Future;
use core::pin::pin;
use core::ptr;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// A fiber entry function, as passed to the `wasmtime_fiber_init` hook:
/// `entry(entry_arg0, top_of_stack)`.
pub type FiberEntry = extern "C" fn(*mut u8, *mut u8) -> *mut u8;

/// First-activation trampoline. JS calls this — wrapped in
/// `WebAssembly.promising`, so it is its own suspendable activation — when a
/// fiber is first switched to. `entry` returns only once the fiber's closure
/// has completed; the JS glue then performs the final switch back to whoever
/// last resumed the fiber.
#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_fiber_enter(entry: FiberEntry, arg0: *mut u8, top_of_stack: *mut u8) {
    entry(arg0, top_of_stack);
}

/// Run one staged fiber job. Only ever called by the JS glue, through
/// `WebAssembly.promising`.
///
/// # Safety
///
/// `job` must be the address of a live `&mut dyn FnMut()` staged by [`run`],
/// not aliased for the duration of the call; [`run`] keeps it alive (and
/// unaliased) until the `drive` promise this call settles is awaited.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn starstream_fiber_run(job: usize) {
    let job = unsafe { &mut *(job as *mut &mut dyn FnMut()) };
    job();
}

#[wasm_bindgen(module = "env")]
extern "C" {
    /// Queue `job` to run on a fresh `promising`-wrapped
    /// [`starstream_fiber_run`] activation. The returned promise settles once
    /// the job has returned (suspensions included); it rejects if the
    /// activation trapped or the glue was never [`set up`](module docs).
    fn drive(job: usize) -> js_sys::Promise;

    /// Resume the [`block_on`] parked on [`starstream_fiber_park`], or — if
    /// none is parked (the wake fired while `poll` was still running) — mark
    /// the wake pending so the next park returns immediately.
    fn starstream_fiber_unpark();
}

#[cfg(target_family = "wasm")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    /// Suspend the current activation until [`starstream_fiber_unpark`] is
    /// called. A raw wasm import (no wasm-bindgen glue): the glue's
    /// `WebAssembly.Suspending` must land directly in the import slot — a
    /// `Suspending` object is not callable as a plain JS function, so it
    /// cannot sit behind wasm-bindgen's import shims.
    fn starstream_fiber_park();
}

/// Host (non-wasm) builds compile this crate but never run it; a defined stub
/// keeps the linker happy where a raw wasm import cannot.
#[cfg(not(target_family = "wasm"))]
unsafe fn starstream_fiber_park() {
    unreachable!("fiber parking requires the JSPI glue and only exists on wasm targets")
}

/// Drive a wasmtime future to completion on a JSPI-suspendable activation.
///
/// The future is polled inside a job shipped to the glue's `drive`; awaiting
/// the returned promise keeps every borrow the future holds alive for
/// exactly as long as the job can run. A rejected promise (the activation
/// trapped, or the glue is missing) surfaces as `Err`.
pub async fn run<F: Future>(fut: F) -> Result<F::Output, JsError> {
    let mut fut = Some(fut);
    let mut out = None;
    {
        let mut job: &mut dyn FnMut() = &mut || {
            let fut = fut.take().expect("fiber job ran twice");
            out = Some(block_on(fut));
        };
        JsFuture::from(drive(&raw mut job as usize))
            .await
            .map_err(|err| JsError::new(&format!("fiber activation failed: {err:?}")))?;
    }
    Ok(out.take().expect("fiber job did not run"))
}

/// Poll a wasmtime future to completion on this (single-threaded) guest.
///
/// Wasmtime's `*_async` futures do most of their waiting *inside* the fiber:
/// a guest call suspends via `wasmtime_fiber_switch`, which JSPI turns into a
/// suspension of the whole [`starstream_fiber_run`] activation — usually the
/// call has finished by the time `poll` returns. If `poll` nevertheless
/// returns `Pending` (an async host import stashed the waker — none exist
/// today, but the machinery is real), the activation parks on the glue's
/// `starstream_fiber_park` until the waker resumes it via
/// `starstream_fiber_unpark`, then polls again.
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let waker = unpark_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            // SAFETY: `block_on` only ever runs inside a
            // `starstream_fiber_run` activation, which the glue entered
            // through `WebAssembly.promising`, so this import may suspend it.
            Poll::Pending => unsafe { starstream_fiber_park() },
        }
    }
}

/// A [`Waker`] that unparks the [`block_on`] parked in this job's
/// [`starstream_fiber_run`] activation. Stateless: jobs are serialized by the
/// glue's `drive`, so at most one `block_on` exists at a time and the JS side
/// needs no token to know whom to wake. A wake that arrives while `poll` is
/// still running is remembered on the JS side, so the following park returns
/// immediately; at worst that causes a spurious re-poll, which the `Waker`
/// contract permits.
fn unpark_waker() -> Waker {
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(ptr::null(), &VTABLE)
    }
    fn wake(_: *const ()) {
        starstream_fiber_unpark();
    }
    fn noop(_: *const ()) {}
    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, noop);
    unsafe { Waker::from_raw(RawWaker::new(ptr::null(), &VTABLE)) }
}
