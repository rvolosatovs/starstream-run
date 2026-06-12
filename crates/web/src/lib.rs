//! Browser entry point for `starstream-run`.
//!
//! Compiled to `wasm32-unknown-unknown` and driven from
//! [`web/index.html`](../web/index.html): the page reads an uploaded `.wasm`
//! file and hands its bytes to [`instantiate`], which calls
//! [`starstream_run::Contract::new`].
//! `starstream-run` itself executes the guest with wasmtime's Pulley
//! interpreter; we enable its `wasmtime-custom-virtual-memory` feature and
//! satisfy wasmtime's mmap/TLS needs without a real OS underneath — see
//! [`wasmtime`].
//!
//! The UI is built directly on the typed runtime API: a [`Contract`] holds the
//! loaded [`starstream_run::Contract`] plus a table of live [`Utxo`] handles.
//! [`Contract::describe`] reports the exported `utxo`-owning instances (their
//! constructors, methods and `storage` fields) so the page can render input
//! widgets; calling a constructor instantiates a fresh [`Utxo`] and records it
//! as a handle, and method/storage calls are routed to that handle. Values
//! cross the JS boundary as JSON strings, converted to/from
//! [`wasmtime::component::Val`] against each function's declared type.
//!
//! Every method that runs guest code ([`Contract::call`],
//! [`Contract::storage_get`]) is `async` (a `Promise` on the JS side): it
//! drives wasmtime's `*_async` APIs, whose fibers suspend the wasm activation
//! via JSPI — see [`fiber`].

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tracing_subscriber::fmt::MakeWriter;
use wasm_bindgen::prelude::*;
// Spelled with a leading `::` because the local `mod wasmtime` (the platform
// shim below) shadows the crate name.
use ::wasmtime::component::{Type, Val, types};

use starstream_run::{Utxo, UtxoExport, bindings};

mod fiber;
mod wasmtime;

/// The Cardano context a contract can observe via the `starstream:std/cardano`
/// host functions, configured from the page's Cardano form (defaulting to 0).
#[derive(Clone, Copy, Default)]
struct CardanoCtx {
    /// Block height reported to the guest via `cardano#block-height`.
    block_height: i64,
    /// Current slot reported to the guest via `cardano#current-slot`.
    current_slot: i64,
}

/// Store data for browser-run contracts. The web UI does not model a host
/// ledger, so this carries the [`CardanoCtx`] the page configures plus a shared
/// buffer of ABI events the guest emits during a call.
///
/// `events` is an `Arc<Mutex<…>>` shared with the owning [`Contract`] (and every
/// UTXO's store), so emissions from any instantiation accumulate in one place;
/// [`Contract::drain_events`] hands them to the page for display.
#[derive(Clone, Default)]
struct Ctx {
    cardano: CardanoCtx,
    events: Arc<Mutex<Vec<Value>>>,
}

impl bindings::starstream::std::builtin::Host for Ctx {
    fn implements_method(&mut self, hash: (u64, u64, u64, u64)) -> ::wasmtime::Result<()> {
        tracing::error!("called builtin#implements_method {hash:?}");
        Ok(())
    }
}

impl bindings::starstream::std::cardano::Host for Ctx {
    fn block_height(&mut self) -> i64 {
        self.cardano.block_height
    }

    fn current_slot(&mut self) -> i64 {
        self.cardano.current_slot
    }
}

impl starstream_run::EventHandler for Ctx {
    fn emit_event(&mut self, instance: &str, name: &str, params: &[Val]) {
        let args: Vec<Value> = params.iter().map(val_to_json).collect();
        self.events.lock().unwrap().push(json!({
            "instance": instance,
            "name": name,
            "params": args,
        }));
    }
}

/// Wire up panic reporting and tracing once, when the module is loaded.
#[wasm_bindgen(start)]
fn start() {
    struct CraneliftProfiler;
    impl cranelift_codegen::timing::Profiler for CraneliftProfiler {
        fn start_pass(&self, _pass: cranelift_codegen::timing::Pass) -> Box<dyn std::any::Any> {
            Box::new(())
        }
    }

    console_error_panic_hook::set_once();
    _ = cranelift_codegen::timing::set_thread_profiler(Box::new(CraneliftProfiler));

    tracing_subscriber::fmt()
        .with_writer(MakeConsoleWriter)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .init();
}

/// A loaded, linked contract, exposed to JS so the page can drive its exported
/// `utxo` resources interactively.
///
/// Each minted handle is its own instantiation: calling a constructor builds a
/// fresh [`Utxo`] (with its own store) and stores it under an integer id;
/// methods and `storage` accesses are routed to that handle by id.
#[wasm_bindgen]
pub struct Contract {
    inner: starstream_run::Contract<Ctx>,
    handles: HashMap<u32, Handle>,
    next_id: u32,
    /// The Cardano context seeded into every freshly minted [`Utxo`]'s store
    /// (see [`Contract::set_cardano`]). Each instantiation copies this in.
    cardano: CardanoCtx,
    /// Shared event buffer handed to every UTXO's [`Ctx`]; drained by
    /// [`Contract::drain_events`] for display on the page.
    events: Arc<Mutex<Vec<Value>>>,
}

/// A live `utxo` handle: the instantiated [`Utxo`] and the export it came from
/// (for `storage`). Method exports are re-resolved per call from the instance
/// name the page passes in.
struct Handle {
    export: UtxoExport,
    utxo: Utxo<Ctx>,
}

#[wasm_bindgen]
impl Contract {
    /// JSON description of the exported instances that own a `utxo` resource,
    /// their constructors, methods and `storage` fields.
    ///
    /// Shape: `{ instances: [{ name, resource, constructors: [func], methods:
    /// [func], storage: [{name, kind}] | null }] }`, where `func` is
    /// `{ export, label, params: [{name, kind}] }`. `kind` is a scalar type
    /// name (`u64`, `bool`, `string`, …) or `"json"` for everything else
    /// (entered as raw JSON on the page).
    pub fn describe(&self) -> Result<String, JsError> {
        // Collect owned `(name, UtxoExport)` pairs first so the `utxos()`
        // borrow of `self.inner` is released before we re-borrow it below.
        let utxos: Vec<(String, UtxoExport)> = self
            .inner
            .utxos()
            .filter_map(|(name, utxo)| utxo.ok().map(|utxo| (name.to_string(), utxo)))
            .collect();

        let mut instances = Vec::with_capacity(utxos.len());
        for (name, utxo) in &utxos {
            let constructors: Vec<Value> = self
                .inner
                .utxo_constructors(utxo)
                .filter_map(|(export, ctor)| ctor.ok().map(|ctor| func_json(export, ctor.ty(), 0)))
                .collect();
            let methods: Vec<Value> = self
                .inner
                .utxo_methods(utxo)
                // Skip the leading `borrow<utxo>` (`self`) parameter — it is
                // injected from the handle, not entered by the user.
                .filter_map(|(export, method)| {
                    method.ok().map(|method| func_json(export, method.ty(), 1))
                })
                .collect();
            let storage = utxo.storage().map(|storage| {
                storage
                    .ty()
                    .fields()
                    .map(|field| json!({ "name": field.name, "kind": kind_str(&field.ty) }))
                    .collect::<Vec<_>>()
            });
            instances.push(json!({
                "name": name,
                "resource": "utxo",
                "constructors": constructors,
                "methods": methods,
                "storage": storage,
            }));
        }
        serde_json::to_string(&json!({ "instances": instances }))
            .map_err(|err| JsError::new(&err.to_string()))
    }

    /// Invoke `func` within the exported `instance`.
    ///
    /// A `[static]…` constructor mints a fresh handle and returns
    /// `[{"$handle": id}]`. Any other (method) call expects the `self` handle
    /// as the first entry of the `args_json` array (`{"$handle": id}`); the
    /// remaining entries are the method's parameters. Returns a JSON array of
    /// results.
    pub async fn call(
        &mut self,
        instance: String,
        func: String,
        args_json: String,
    ) -> Result<String, JsError> {
        let args: Vec<Value> =
            serde_json::from_str(&args_json).map_err(|err| JsError::new(&err.to_string()))?;
        let utxo = self.inner.get_utxo(&instance).map_err(err_to_js)?;

        if func.starts_with("[static]") {
            let ctor = self
                .inner
                .get_utxo_constructor(&utxo, &func)
                .map_err(err_to_js)?;
            let params = convert_args(ctor.ty().params(), 0, &args).map_err(|e| js_err(&e))?;
            let ctx = Ctx {
                cardano: self.cardano,
                events: Arc::clone(&self.events),
            };
            let new = fiber::run(self.inner.create_utxo_async(ctx, &ctor, &params))
                .await?
                .map_err(err_to_js)?;
            let id = self.next_id;
            self.next_id += 1;
            self.handles.insert(
                id,
                Handle {
                    export: utxo,
                    utxo: new,
                },
            );
            return Ok(json!([{ "$handle": id }]).to_string());
        }

        let method = self
            .inner
            .get_utxo_method(&utxo, &func)
            .map_err(err_to_js)?;
        let id = handle_id(args.first())
            .ok_or_else(|| JsError::new("a method call needs a `$handle` as its first argument"))?;
        let params = convert_args(method.ty().params(), 1, &args[1..]).map_err(|e| js_err(&e))?;

        let handle = self
            .handles
            .get_mut(&id)
            .ok_or_else(|| JsError::new("unknown handle"))?;
        let mut full = Vec::with_capacity(params.len() + 1);
        full.push(Val::Resource(handle.utxo.resource()));
        full.extend(params);

        let results = fiber::run(handle.utxo.call_async(&method, &full))
            .await?
            .map_err(err_to_js)?;
        let results: Vec<Value> = results.iter().map(val_to_json).collect();
        serde_json::to_string(&results).map_err(|err| JsError::new(&err.to_string()))
    }

    /// Mint a fresh handle by *loading* a `storage` record into a new `utxo`,
    /// instead of calling a `[static]` constructor.
    ///
    /// `storage_json` is a JSON object of the instance's `storage` fields; it is
    /// lowered against the storage type and passed to the `set-storage` export
    /// (which returns `own<utxo>`) via [`starstream_run::Contract::load_utxo_async`].
    /// Returns `[{"$handle": id}]`, matching [`Contract::call`]'s constructor
    /// branch, so the page records the new handle the same way.
    #[wasm_bindgen(js_name = loadUtxo)]
    pub async fn load_utxo(
        &mut self,
        instance: String,
        storage_json: String,
    ) -> Result<String, JsError> {
        let value: Value =
            serde_json::from_str(&storage_json).map_err(|err| JsError::new(&err.to_string()))?;
        let utxo = self.inner.get_utxo(&instance).map_err(err_to_js)?;
        let storage = utxo
            .storage()
            .cloned()
            .ok_or_else(|| JsError::new("this resource has no storage"))?;
        let Val::Record(fields) =
            json_to_val(&Type::Record(storage.ty().clone()), &value).map_err(|e| js_err(&e))?
        else {
            return Err(JsError::new("storage value must be a record"));
        };
        let ctx = Ctx {
            cardano: self.cardano,
            events: Arc::clone(&self.events),
        };
        let new = fiber::run(self.inner.load_utxo_async(ctx, &storage, fields))
            .await?
            .map_err(err_to_js)?;
        let id = self.next_id;
        self.next_id += 1;
        self.handles.insert(
            id,
            Handle {
                export: utxo,
                utxo: new,
            },
        );
        Ok(json!([{ "$handle": id }]).to_string())
    }

    /// Read a handle's `storage` record as a JSON object.
    #[wasm_bindgen(js_name = storageGet)]
    pub async fn storage_get(&mut self, id: u32) -> Result<String, JsError> {
        let handle = self
            .handles
            .get_mut(&id)
            .ok_or_else(|| JsError::new("unknown handle"))?;
        let storage = handle
            .export
            .storage()
            .cloned()
            .ok_or_else(|| JsError::new("this resource has no storage"))?;
        let mut storage = handle.utxo.storage(&storage);
        let fields = fiber::run(storage.get_async()).await?.map_err(err_to_js)?;
        let obj: Map<String, Value> = fields
            .iter()
            .map(|(name, val)| (name.clone(), val_to_json(val)))
            .collect();
        Ok(Value::Object(obj).to_string())
    }

    /// Drop a live handle by its id, running the guest resource's destructor.
    ///
    /// Removes the handle from the table and calls
    /// [`starstream_run::Utxo::drop_async`], which invokes the resource's
    /// `[dtor]` in the guest before the [`Utxo`] (and its store) are freed. This
    /// runs guest code, so it is `async` (a `Promise` on the JS side) and driven
    /// through the JSPI fiber glue like the other guest-invoking methods.
    #[wasm_bindgen(js_name = dropResource)]
    pub async fn drop_resource(&mut self, id: u32) -> Result<(), JsError> {
        let handle = self
            .handles
            .remove(&id)
            .ok_or_else(|| JsError::new("unknown handle"))?;
        fiber::run(handle.utxo.drop_async())
            .await?
            .map_err(err_to_js)
    }

    /// Drain the ABI events emitted since the last drain, as a JSON array of
    /// `{ instance, name, params }` (newest last). The page shows these in the
    /// invocation panel's log instead of routing them through the trace log.
    #[wasm_bindgen(js_name = drainEvents)]
    #[must_use]
    pub fn drain_events(&self) -> String {
        let mut events = self.events.lock().unwrap();
        let drained: Vec<Value> = std::mem::take(&mut events);
        Value::Array(drained).to_string()
    }

    /// Set the Cardano context (`cardano#block-height` / `cardano#current-slot`)
    /// reported to guests. Applies to UTXOs minted *after* this call; existing
    /// handles keep the context they were instantiated with.
    #[wasm_bindgen(js_name = setCardano)]
    pub fn set_cardano(&mut self, block_height: i64, current_slot: i64) {
        self.cardano = CardanoCtx {
            block_height,
            current_slot,
        };
    }
}

/// Load and link an uploaded contract for interactive use.
///
/// `component` may be a full component or a core module with a `component-type`
/// custom section. Errors are surfaced to JS as a thrown `Error`.
#[wasm_bindgen]
pub fn instantiate(component: &[u8]) -> Result<Contract, JsError> {
    let inner = starstream_run::Contract::new(component).map_err(err_to_js)?;
    Ok(Contract {
        inner,
        handles: HashMap::new(),
        next_id: 0,
        cardano: CardanoCtx::default(),
        events: Arc::default(),
    })
}

/// Render one function (constructor or method) as a describe entry, dropping
/// the first `skip` parameters (used to hide a method's `self`).
fn func_json(export: &str, ty: &types::ComponentFunc, skip: usize) -> Value {
    let params: Vec<Value> = ty
        .params()
        .skip(skip)
        .map(|(name, ty)| json!({ "name": name, "kind": kind_str(&ty) }))
        .collect();
    // Export names look like `[static]utxo.new` / `[method]utxo.plus-chips`.
    let label = export.rsplit('.').next().unwrap_or(export);
    json!({ "export": export, "label": label, "params": params })
}

/// A short, JS-friendly tag for a parameter/field type. Anything that isn't a
/// scalar maps to `"json"`, for which the page offers a raw JSON text box.
fn kind_str(ty: &Type) -> &'static str {
    match ty {
        Type::Bool => "bool",
        Type::S8 => "s8",
        Type::U8 => "u8",
        Type::S16 => "s16",
        Type::U16 => "u16",
        Type::S32 => "s32",
        Type::U32 => "u32",
        Type::S64 => "s64",
        Type::U64 => "u64",
        Type::Float32 => "f32",
        Type::Float64 => "f64",
        Type::Char => "char",
        Type::String => "string",
        _ => "json",
    }
}

/// Convert positional JSON arguments to typed [`Val`]s against a function's
/// parameter list, skipping the first `skip` parameters.
fn convert_args<'a>(
    params: impl Iterator<Item = (&'a str, Type)>,
    skip: usize,
    args: &[Value],
) -> Result<Vec<Val>, String> {
    let tys: Vec<Type> = params.skip(skip).map(|(_, ty)| ty).collect();
    if tys.len() != args.len() {
        return Err(format!(
            "expected {} argument(s), got {}",
            tys.len(),
            args.len()
        ));
    }
    tys.iter()
        .zip(args)
        .map(|(ty, arg)| json_to_val(ty, arg))
        .collect()
}

/// Lower a JSON value into a [`Val`] of the given component type. Supports the
/// scalar types plus `record`/`tuple`/`list`/`option`; other types are
/// rejected.
// JSON numbers coerced into the guest's narrower integer/float types: the
// truncation/sign loss is the intended lossy conversion.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn json_to_val(ty: &Type, v: &Value) -> Result<Val, String> {
    let as_i = |v: &Value| {
        v.as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .ok_or_else(|| format!("expected an integer, got `{v}`"))
    };
    let as_u = |v: &Value| {
        v.as_u64()
            .or_else(|| v.as_f64().map(|f| f as u64))
            .ok_or_else(|| format!("expected an unsigned integer, got `{v}`"))
    };
    let as_f = |v: &Value| {
        v.as_f64()
            .ok_or_else(|| format!("expected a number, got `{v}`"))
    };
    match ty {
        Type::Bool => v
            .as_bool()
            .map(Val::Bool)
            .ok_or_else(|| format!("expected a boolean, got `{v}`")),
        Type::S8 => Ok(Val::S8(as_i(v)? as i8)),
        Type::U8 => Ok(Val::U8(as_u(v)? as u8)),
        Type::S16 => Ok(Val::S16(as_i(v)? as i16)),
        Type::U16 => Ok(Val::U16(as_u(v)? as u16)),
        Type::S32 => Ok(Val::S32(as_i(v)? as i32)),
        Type::U32 => Ok(Val::U32(as_u(v)? as u32)),
        Type::S64 => Ok(Val::S64(as_i(v)?)),
        Type::U64 => Ok(Val::U64(as_u(v)?)),
        Type::Float32 => Ok(Val::Float32(as_f(v)? as f32)),
        Type::Float64 => Ok(Val::Float64(as_f(v)?)),
        Type::Char => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected a string for char, got `{v}`"))?;
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Ok(Val::Char(c)),
                _ => Err("expected exactly one character".into()),
            }
        }
        Type::String => v
            .as_str()
            .map(|s| Val::String(s.to_string()))
            .ok_or_else(|| format!("expected a string, got `{v}`")),
        Type::List(list) => {
            let arr = v
                .as_array()
                .ok_or_else(|| format!("expected an array for list, got `{v}`"))?;
            let elem = list.ty();
            arr.iter()
                .map(|x| json_to_val(&elem, x))
                .collect::<Result<Vec<_>, _>>()
                .map(Val::List)
        }
        Type::Record(record) => {
            let obj = v
                .as_object()
                .ok_or_else(|| format!("expected an object for record, got `{v}`"))?;
            let mut fields = Vec::new();
            for field in record.fields() {
                let value = obj
                    .get(field.name)
                    .ok_or_else(|| format!("missing record field `{}`", field.name))?;
                fields.push((field.name.to_string(), json_to_val(&field.ty, value)?));
            }
            Ok(Val::Record(fields))
        }
        Type::Tuple(tuple) => {
            let arr = v
                .as_array()
                .ok_or_else(|| format!("expected an array for tuple, got `{v}`"))?;
            let tys: Vec<Type> = tuple.types().collect();
            if tys.len() != arr.len() {
                return Err(format!(
                    "expected a {}-tuple, got {} elements",
                    tys.len(),
                    arr.len()
                ));
            }
            tys.iter()
                .zip(arr)
                .map(|(ty, x)| json_to_val(ty, x))
                .collect::<Result<Vec<_>, _>>()
                .map(Val::Tuple)
        }
        Type::Option(opt) => {
            if v.is_null() {
                Ok(Val::Option(None))
            } else {
                json_to_val(&opt.ty(), v).map(|val| Val::Option(Some(Box::new(val))))
            }
        }
        _ => Err("unsupported parameter type".into()),
    }
}

/// Lift a result [`Val`] into JSON for display. Resource handles returned by a
/// guest are rendered as `{"$resource": true}` (they are not tracked as
/// re-callable handles in this UI).
fn val_to_json(v: &Val) -> Value {
    match v {
        Val::Bool(b) => json!(b),
        Val::S8(n) => json!(n),
        Val::U8(n) => json!(n),
        Val::S16(n) => json!(n),
        Val::U16(n) => json!(n),
        Val::S32(n) => json!(n),
        Val::U32(n) => json!(n),
        Val::S64(n) => json!(n),
        Val::U64(n) => json!(n),
        Val::Float32(n) => json!(n),
        Val::Float64(n) => json!(n),
        Val::Char(c) => json!(c.to_string()),
        Val::String(s) => json!(s),
        Val::List(xs) | Val::Tuple(xs) => Value::Array(xs.iter().map(val_to_json).collect()),
        Val::Record(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), val_to_json(v)))
                .collect(),
        ),
        Val::Option(o) => o.as_deref().map_or(Value::Null, val_to_json),
        Val::Resource(_) => json!({ "$resource": true }),
        other => json!(format!("{other:?}")),
    }
}

/// Extract a `{"$handle": <id>}` resource handle id from a JSON value.
#[allow(clippy::cast_possible_truncation)]
fn handle_id(v: Option<&Value>) -> Option<u32> {
    v?.get("$handle")?.as_u64().map(|id| id as u32)
}

/// Render an error's full context chain into a thrown JS `Error`.
fn err_to_js(err: impl std::fmt::Debug) -> JsError {
    JsError::new(&format!("{err:?}"))
}

/// Wrap a plain error string as a thrown JS `Error`.
fn js_err(msg: &str) -> JsError {
    JsError::new(msg)
}

/// `tracing` → `console.log` bridge. `tracing_subscriber`'s `fmt` layer asks a
/// [`MakeWriter`] for a fresh writer per event; we buffer the formatted line
/// and write it to the console when that writer is dropped. The runtime runs
/// inside a Web Worker (see `web/contract.worker.js`), which forwards these
/// console lines to the page so each contract's log panel can colour them by
/// the level token the line starts with.
struct MakeConsoleWriter;

impl<'a> MakeWriter<'a> for MakeConsoleWriter {
    type Writer = ConsoleWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ConsoleWriter(Vec::new())
    }
}

struct ConsoleWriter(Vec<u8>);

impl io::Write for ConsoleWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ConsoleWriter {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let msg = String::from_utf8_lossy(&self.0);
        // The worker (web/contract.worker.js) patches `console` to forward
        // these lines to the page, which colours them by their level token.
        web_sys::console::log_1(&JsValue::from_str(msg.trim_end()));
    }
}
