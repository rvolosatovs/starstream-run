use core::ops::Deref;

use std::sync::Arc;

use tracing::{debug, instrument};
use wasi_preview1_component_adapter_provider::{
    WASI_SNAPSHOT_PREVIEW1_ADAPTER_NAME, WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER,
};
use wasmtime::component::{
    Component, ComponentExportIndex, ExportLookup, Func, HasSelf, Instance, InstancePre, Linker,
    LinkerInstance, ResourceAny, ResourceType, Type, types,
};
use wasmtime::error::Context as _;
use wasmtime::{
    AsContext, AsContextMut, Config, Engine, Store, StoreContext, StoreContextMut, bail,
};

pub mod bindings {
    wasmtime::component::bindgen!({
        inline: "
            package starstream:host;

            world host {
                import starstream:std/builtin;
                import starstream:std/cardano;
            }
        ",
        imports: {
            "starstream:std/builtin.implements-method": tracing | trappable,
            default: tracing,
        }
    });
}

/// Store data backing a [`Contract`]'s instances.
///
/// A `Contract` is generic over its store-data type `T`, which carries the
/// `starstream:std` host-import state and so must implement the generated
/// builtin and cardano host traits ([`bindings::starstream::std::builtin::Host`]
/// / [`bindings::starstream::std::cardano::Host`]) — these are wired into the
/// linker by [`Contract::new`]. The runtime itself does not provide an
/// implementation; the CLI and web crates each supply their own.
///
/// Each UTXO handle is its own instantiation with its own store, so the caller
/// passes the store data value in when minting or loading a UTXO (see
/// [`Contract::create_utxo`] / [`Contract::load_utxo`]) rather than the runtime
/// default-constructing it.
///
/// This trait is a blanket alias for those bounds, so any type that satisfies
/// them implements it automatically.
pub trait Host:
    bindings::starstream::std::builtin::Host
    + bindings::starstream::std::cardano::Host
    + EventHandler
    + 'static
{
}

impl<T> Host for T where
    T: bindings::starstream::std::builtin::Host
        + bindings::starstream::std::cardano::Host
        + EventHandler
        + 'static
{
}

/// Receives ABI events as the guest emits them.
///
/// An `abi`'s `event` (`emit Foo(..)`) is lowered to an imported host function;
/// [`link_abi_event_function`] installs a shim that forwards every emission here
/// with the emitting interface name, the event name and its positional argument
/// values. The runtime supplies no implementation — the CLI logs events, the
/// web crate buffers them for display in its UI.
pub trait EventHandler {
    fn emit_event(&mut self, instance: &str, name: &str, params: &[wasmtime::component::Val]);
}

pub enum Val {
    Bool(bool),
    S8(i8),
    U8(u8),
    S16(i16),
    U16(u16),
    S32(i32),
    U32(u32),
    S64(i64),
    U64(u64),
    Char(char),
    String(String),
    Record(Vec<(String, Val)>),
    Tuple(Vec<Val>),
}

impl From<Val> for wasmtime::component::Val {
    fn from(v: Val) -> Self {
        match v {
            Val::Bool(v) => Self::Bool(v),
            Val::S8(v) => Self::S8(v),
            Val::U8(v) => Self::U8(v),
            Val::S16(v) => Self::S16(v),
            Val::U16(v) => Self::U16(v),
            Val::S32(v) => Self::S32(v),
            Val::U32(v) => Self::U32(v),
            Val::S64(v) => Self::S64(v),
            Val::U64(v) => Self::U64(v),
            Val::Char(v) => Self::Char(v),
            Val::String(v) => Self::String(v),
            Val::Record(vs) => Self::Record(vs.into_iter().map(|(k, v)| (k, v.into())).collect()),
            Val::Tuple(vs) => Self::Tuple(vs.into_iter().map(Into::into).collect()),
        }
    }
}

impl TryFrom<wasmtime::component::Val> for Val {
    type Error = wasmtime::Error;

    fn try_from(v: wasmtime::component::Val) -> Result<Self, Self::Error> {
        match v {
            wasmtime::component::Val::Bool(v) => Ok(Self::Bool(v)),
            wasmtime::component::Val::S8(v) => Ok(Self::S8(v)),
            wasmtime::component::Val::U8(v) => Ok(Self::U8(v)),
            wasmtime::component::Val::S16(v) => Ok(Self::S16(v)),
            wasmtime::component::Val::U16(v) => Ok(Self::U16(v)),
            wasmtime::component::Val::S32(v) => Ok(Self::S32(v)),
            wasmtime::component::Val::U32(v) => Ok(Self::U32(v)),
            wasmtime::component::Val::S64(v) => Ok(Self::S64(v)),
            wasmtime::component::Val::U64(v) => Ok(Self::U64(v)),
            wasmtime::component::Val::Char(v) => Ok(Self::Char(v)),
            wasmtime::component::Val::String(v) => Ok(Self::String(v)),
            wasmtime::component::Val::Record(vs) => {
                let vs = vs
                    .into_iter()
                    .map(|(k, v)| {
                        let v = Self::try_from(v)?;
                        Ok((k, v))
                    })
                    .collect::<wasmtime::Result<_>>()?;
                Ok(Self::Record(vs))
            }
            wasmtime::component::Val::Tuple(vs) => {
                let vs = vs
                    .into_iter()
                    .map(Self::try_from)
                    .collect::<wasmtime::Result<_>>()?;
                Ok(Self::Tuple(vs))
            }
            _ => bail!("unexpected value type"),
        }
    }
}

fn componentize(wasm: impl AsRef<[u8]>) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context as _;

    wit_component::ComponentEncoder::default()
        .validate(true)
        .module(wasm.as_ref())
        .context("failed to set core component module")?
        .adapter(
            WASI_SNAPSHOT_PREVIEW1_ADAPTER_NAME,
            WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER,
        )
        .context("failed to add WASI adapter")?
        .encode()
        .context("failed to encode a component")
}

#[instrument(level = "trace", skip_all)]
fn load_component(engine: &Engine, wasm: impl AsRef<[u8]>) -> wasmtime::Result<Component> {
    let wasm = wasm.as_ref();
    if wasmparser::Parser::is_core_wasm(wasm) {
        let wasm = componentize(wasm).map_err(wasmtime::Error::from_anyhow)?;
        Component::from_binary(engine, &wasm)
    } else {
        Component::from_binary(engine, wasm)
    }
}

/// Link ABI event [`types::ComponentFunc`] in a [`LinkerInstance`]
#[instrument(level = "trace", skip_all)]
pub fn link_abi_event_function<T: EventHandler>(
    linker: &mut LinkerInstance<T>,
    _ty: types::ComponentFunc,
    instance: &str,
    name: &str,
) -> wasmtime::Result<()> {
    debug!(instance, name, "linking ABI instance event function");
    let instance = Arc::<str>::from(instance);
    let name = Arc::<str>::from(name);
    linker.func_new(
        &Arc::clone(&name),
        move |mut store, _ty, params, _results| {
            store.data_mut().emit_event(&instance, &name, params);
            Ok(())
        },
    )
}

/// Link dynamic imported instance in a [`LinkerInstance`].
#[instrument(level = "trace", skip_all)]
pub fn link_instance<T: EventHandler>(
    engine: &Engine,
    linker: &mut LinkerInstance<T>,
    ty: &types::ComponentInstance,
    instance: &str,
) -> wasmtime::Result<()> {
    if let Some(_utxo) = ty.get_export(engine, "utxo") {
        bail!("coordination scripts not supported yet")
    }
    for (name, types::ComponentExtern { ty, .. }) in ty.exports(engine) {
        debug!(name, "linking ABI instance item");
        match ty {
            types::ComponentItem::ComponentFunc(ty) => {
                link_abi_event_function(linker, ty, instance, name)?;
            }
            types::ComponentItem::CoreFunc(..) => {
                bail!("ABI instance core function imports unsupported")
            }
            types::ComponentItem::Module(..) => bail!("ABI instance module imports unsupported"),
            types::ComponentItem::Component(..) => {
                bail!("ABI instance component imports unsupported")
            }
            types::ComponentItem::ComponentInstance(..) => {
                bail!("ABI instance component instance imports unsupported")
            }
            types::ComponentItem::Type(..) => {}
            types::ComponentItem::Resource(..) => {
                bail!("ABI instance resource imports unsupported")
            }
        }
    }
    Ok(())
}

/// Link dynamic imports of the contract
#[instrument(level = "trace", skip_all)]
pub fn link_dynamic_imports<T: EventHandler>(
    engine: &Engine,
    linker: &mut Linker<T>,
    ty: &types::Component,
) -> wasmtime::Result<()> {
    for (name, types::ComponentExtern { ty, .. }) in ty.imports(engine) {
        if let Some(("starstream:std", ..)) = name.split_once('/') {
            debug!(?name, "skipping builtin instance import");
            continue;
        }
        match ty {
            types::ComponentItem::ComponentFunc(..) => {
                bail!("root instance function imports unsupported")
            }
            types::ComponentItem::CoreFunc(..) => {
                bail!("core function imports unsupported")
            }
            types::ComponentItem::Module(..) => bail!("module imports unsupported"),
            types::ComponentItem::Component(..) => bail!("component imports unsupported"),
            types::ComponentItem::ComponentInstance(ty) => {
                let mut linker = linker
                    .instance(name)
                    .with_context(|| format!("failed to instantiate `{name}` in the linker"))?;
                debug!(?name, "linking root instance");
                link_instance(engine, &mut linker, &ty, name)?;
            }
            types::ComponentItem::Type(..) => {}
            types::ComponentItem::Resource(..) => {
                bail!("root instance resource imports unsupported")
            }
        }
    }
    Ok(())
}

pub struct Contract<T: 'static> {
    pre: InstancePre<T>,
    ty: types::Component,
}

// Hand-written so the bound is `T` (not `T: Clone`): `InstancePre<T>` and
// `types::Component` are both cheap-to-clone handles regardless of `T`.
impl<T: 'static> Clone for Contract<T> {
    fn clone(&self) -> Self {
        Self {
            pre: self.pre.clone(),
            ty: self.ty.clone(),
        }
    }
}

impl<T: 'static> Deref for Contract<T> {
    type Target = InstancePre<T>;

    fn deref(&self) -> &Self::Target {
        &self.pre
    }
}

impl<T: Host> Contract<T> {
    /// Load, link and instantiate `wasm` (a component or a core module with a
    /// `component-type` section), returning a handle to drive it.
    #[instrument(level = "trace", skip_all)]
    pub fn new(wasm: impl AsRef<[u8]>) -> anyhow::Result<Self> {
        let wasm = wasm.as_ref();

        let mut config = Config::new();
        config.guest_debug(true).wasm_component_model(true);

        debug!("creating engine");
        let engine = Engine::new(&config).context("failed to create engine")?;

        debug!("loading component");
        let component = load_component(&engine, wasm)?;

        let mut linker = Linker::new(&engine);

        debug!("linking component imports");
        bindings::Host_::add_to_linker::<_, HasSelf<_>>(&mut linker, |cx| cx)
            .context("failed to link builtins")?;
        link_dynamic_imports(&engine, &mut linker, &component.component_type())?;

        let ty = linker
            .substituted_component_type(&component)
            .context("failed to derive component type")?;

        debug!("pre-instantiating component");
        let pre = linker
            .instantiate_pre(&component)
            .context("failed to pre-instantiate component")?;

        Ok(Self { pre, ty })
    }

    #[instrument(level = "trace", skip_all)]
    fn get_utxo_typed(
        &self,
        name: &str,
        instance_ty: types::ComponentInstance,
    ) -> wasmtime::Result<UtxoExport> {
        let engine = self.engine();
        let component = self.component();
        let instance_idx = component
            .get_export_index(None, name)
            .context("export not found")?;
        let Some(types::ComponentExtern {
            ty: types::ComponentItem::Resource(resource_ty),
            ..
        }) = instance_ty.get_export(engine, "utxo")
        else {
            bail!("instance does not export `utxo` resource")
        };
        let storage = instance_ty
            .get_export(engine, "storage")
            .map(|types::ComponentExtern { ty, .. }| {
                let types::ComponentItem::Type(Type::Record(ty)) = ty else {
                    bail!("`storage` export is not a record")
                };

                let (get_ty, get) = component
                    .get_export(Some(&instance_idx), "get-storage")
                    .context("`get-storage` export not found")?;
                let types::ComponentItem::ComponentFunc(get_ty) = get_ty else {
                    bail!("`get-storage` export is not a function")
                };
                let mut get_params = get_ty.params();
                let (Some((_, Type::Borrow(get_resource_ty))), None) =
                    (get_params.next(), get_params.next())
                else {
                    bail!(
                        "`get-storage` does not take borrowed resource type as the only parameter"
                    );
                };
                if get_resource_ty != resource_ty {
                    bail!("`get-storage` resource type does not match UTXO resource type");
                }
                let mut get_results = get_ty.results();
                let (Some(Type::Record(get_record_ty)), None) =
                    (get_results.next(), get_results.next())
                else {
                    bail!("`get-storage` does not return a record as the only return value");
                };
                if get_record_ty != ty {
                    bail!("`get-storage` record type does not match storage type");
                }

                let (set_ty, set) = component
                    .get_export(Some(&instance_idx), "set-storage")
                    .context("`set-storage` export not found")?;
                let types::ComponentItem::ComponentFunc(set_ty) = set_ty else {
                    bail!("`set-storage` export is not a function")
                };
                let mut set_params = set_ty.params();
                let (Some((_, Type::Record(set_record_ty))), None) =
                    (set_params.next(), set_params.next())
                else {
                    bail!("`set-storage` does not take a storage record as the only parameter");
                };
                if set_record_ty != ty {
                    bail!("`set-storage` record type does not match storage type");
                }
                let mut set_results = set_ty.results();
                let (Some(Type::Own(set_resource_ty)), None) =
                    (set_results.next(), set_results.next())
                else {
                    bail!(
                        "`set-storage` does not return an owned resource as the only return value"
                    );
                };
                if set_resource_ty != resource_ty {
                    bail!("`set-storage` resource type does not match UTXO resource type");
                }

                Ok(UtxoStorageExport { ty, get, set })
            })
            .transpose()?;
        Ok(UtxoExport {
            resource_ty,
            instance_ty,
            instance_idx,
            storage,
        })
    }

    #[instrument(level = "trace", skip_all)]
    pub fn get_utxo(&self, name: &str) -> wasmtime::Result<UtxoExport> {
        let types::ComponentExtern { ty, .. } = self
            .ty
            .get_export(self.engine(), name)
            .context("export not found")?;
        let types::ComponentItem::ComponentInstance(ty) = ty else {
            bail!("export is not an instance")
        };
        self.get_utxo_typed(name, ty)
    }

    #[instrument(level = "trace", skip_all)]
    pub fn utxos(&self) -> impl Iterator<Item = (&str, wasmtime::Result<UtxoExport>)> {
        let engine = self.engine();
        self.ty.exports(engine).filter_map(|(name, ty)| {
            let types::ComponentExtern {
                ty: types::ComponentItem::ComponentInstance(ty),
                ..
            } = ty
            else {
                return None;
            };
            Some((name, self.get_utxo_typed(name, ty)))
        })
    }

    #[instrument(level = "trace", skip_all)]
    fn get_utxo_constructor_typed(
        &self,
        utxo: &UtxoExport,
        name: &str,
        ty: types::ComponentFunc,
    ) -> wasmtime::Result<ConstructorExport> {
        let idx = self
            .component()
            .get_export_index(Some(&utxo.instance_idx), name)
            .context("export not found")?;

        let (Some(Type::Own(resource_ty)), None) = ({
            let mut result_tys = ty.results();
            (result_tys.next(), result_tys.next())
        }) else {
            bail!("function does not return a single resource value")
        };
        if resource_ty != utxo.resource_ty {
            bail!("function return value does not match UTXO resource type");
        }
        Ok(ConstructorExport { ty, idx })
    }

    #[instrument(level = "trace", skip_all)]
    pub fn get_utxo_constructor(
        &self,
        utxo: &UtxoExport,
        name: &str,
    ) -> wasmtime::Result<ConstructorExport> {
        let types::ComponentExtern { ty, .. } = utxo
            .instance_ty
            .get_export(self.engine(), name)
            .context("export not found")?;
        let types::ComponentItem::ComponentFunc(ty) = ty else {
            bail!("export is not a function")
        };
        self.get_utxo_constructor_typed(utxo, name, ty)
    }

    #[instrument(level = "trace", skip_all)]
    pub fn utxo_constructors<'a>(
        &'a self,
        utxo: &'a UtxoExport,
    ) -> impl Iterator<Item = (&'a str, wasmtime::Result<ConstructorExport>)> {
        utxo.instance_ty
            .exports(self.engine())
            .filter_map(move |(name, ty)| {
                let types::ComponentExtern {
                    ty: types::ComponentItem::ComponentFunc(ty),
                    ..
                } = ty
                else {
                    return None;
                };
                if !name.starts_with("[static]") {
                    return None;
                }
                Some((name, self.get_utxo_constructor_typed(utxo, name, ty)))
            })
    }

    #[instrument(level = "trace", skip_all)]
    fn get_utxo_method_typed(
        &self,
        utxo: &UtxoExport,
        name: &str,
        ty: types::ComponentFunc,
    ) -> wasmtime::Result<MethodExport> {
        let idx = self
            .component()
            .get_export_index(Some(&utxo.instance_idx), name)
            .context("export not found")?;

        let Some((_, Type::Borrow(resource_ty))) = ty.params().next() else {
            bail!("function does not take borrowed resource type as first parameter");
        };
        if resource_ty != utxo.resource_ty {
            bail!("resource type does not match UTXO resource type");
        }
        Ok(MethodExport { ty, idx })
    }

    #[instrument(level = "trace", skip_all)]
    pub fn get_utxo_method(&self, utxo: &UtxoExport, name: &str) -> wasmtime::Result<MethodExport> {
        let types::ComponentExtern { ty, .. } = utxo
            .instance_ty
            .get_export(self.engine(), name)
            .context("export not found")?;
        let types::ComponentItem::ComponentFunc(ty) = ty else {
            bail!("export is not a function")
        };
        self.get_utxo_method_typed(utxo, name, ty)
    }

    #[instrument(level = "trace", skip_all)]
    pub fn utxo_methods<'a>(
        &'a self,
        utxo: &'a UtxoExport,
    ) -> impl Iterator<Item = (&'a str, wasmtime::Result<MethodExport>)> {
        utxo.instance_ty
            .exports(self.engine())
            .filter_map(move |(name, ty)| {
                let types::ComponentExtern {
                    ty: types::ComponentItem::ComponentFunc(ty),
                    ..
                } = ty
                else {
                    return None;
                };
                if !name.starts_with("[method]") {
                    return None;
                }
                Some((name, self.get_utxo_method_typed(utxo, name, ty)))
            })
    }

    #[instrument(level = "trace", skip_all)]
    fn new_store(&self, ctx: T) -> Store<T> {
        Store::new(self.engine(), ctx)
    }

    #[instrument(level = "trace", skip_all)]
    fn instantiate(&self, ctx: T) -> wasmtime::Result<(Store<T>, Instance)> {
        let mut store = self.new_store(ctx);

        debug!("instantiating component");
        let instance = self
            .pre
            .instantiate(&mut store)
            .context("failed to instantiate component")?;
        Ok((store, instance))
    }

    #[instrument(level = "trace", skip_all)]
    #[cfg(feature = "async")]
    async fn instantiate_async(&self, ctx: T) -> wasmtime::Result<(Store<T>, Instance)>
    where
        T: Send,
    {
        let mut store = self.new_store(ctx);

        #[cfg(feature = "trace")]
        {
            use core::marker::PhantomData;

            use tracing::{error, trace};
            use wasmtime::DebugEvent;

            struct DebugHandler<T>(PhantomData<fn() -> T>);

            impl<T> Clone for DebugHandler<T> {
                fn clone(&self) -> Self {
                    Self(PhantomData)
                }
            }

            impl<T: 'static + Send> wasmtime::DebugHandler for DebugHandler<T> {
                type Data = T;

                async fn handle(
                    &self,
                    mut store: StoreContextMut<'_, Self::Data>,
                    event: DebugEvent<'_>,
                ) {
                    match event {
                        DebugEvent::Breakpoint => {
                            let frames: Vec<_> = store.debug_exit_frames().collect();
                            for frame in frames {
                                match frame.wasm_function_index_and_pc(&mut store) {
                                    Ok(Some((f, pc))) => debug!(?f, ?pc, "frame"),
                                    Ok(None) => trace!("skip trampoline frame"),
                                    Err(err) => error!(?err),
                                }
                            }
                        }
                        DebugEvent::HostcallError(..)
                        | DebugEvent::Exception(..)
                        | DebugEvent::Trap(..)
                        | DebugEvent::EpochYield => {}
                    }
                }
            }
            store.set_debug_handler(DebugHandler::<T>(PhantomData));
            {
                let Some(mut bp) = store.edit_breakpoints() else {
                    bail!("invalid engine config")
                };
                bp.single_step(true)
                    .context("failed to enable single-step debugging")?;
            }
        }
        debug!("instantiating component");
        let instance = self
            .pre
            .instantiate_async(&mut store)
            .await
            .context("failed to instantiate component")?;
        Ok((store, instance))
    }

    #[instrument(level = "trace", skip_all)]
    fn construct_utxo(
        &self,
        ctx: T,
        name: impl ExportLookup,
        params: impl AsRef<[wasmtime::component::Val]>,
    ) -> wasmtime::Result<Utxo<T>> {
        let (mut store, instance) = self.instantiate(ctx)?;
        let f = instance
            .get_func(&mut store, name)
            .context("failed to lookup constructor function export")?;
        debug!("calling constructor function");
        let mut results = [wasmtime::component::Val::Bool(false)];
        f.call(&mut store, params.as_ref(), &mut results)
            .context("failed to call constructor function")?;
        let [wasmtime::component::Val::Resource(resource)] = results else {
            bail!("invalid return value")
        };
        Ok(Utxo {
            store,
            instance,
            resource,
        })
    }

    #[instrument(level = "trace", skip_all)]
    #[cfg(feature = "async")]
    async fn construct_utxo_async(
        &self,
        ctx: T,
        name: impl ExportLookup,
        params: impl AsRef<[wasmtime::component::Val]>,
    ) -> wasmtime::Result<Utxo<T>>
    where
        T: Send,
    {
        let (mut store, instance) = self.instantiate_async(ctx).await?;
        let f = instance
            .get_func(&mut store, name)
            .context("failed to lookup constructor function export")?;
        debug!("calling constructor function");
        let mut results = [wasmtime::component::Val::Bool(false)];
        f.call_async(&mut store, params.as_ref(), &mut results)
            .await
            .context("failed to call constructor function")?;
        let [wasmtime::component::Val::Resource(resource)] = results else {
            bail!("invalid return value")
        };
        Ok(Utxo {
            store,
            instance,
            resource,
        })
    }

    #[instrument(level = "trace", skip_all)]
    pub fn create_utxo(
        &self,
        ctx: T,
        ConstructorExport { idx, .. }: &ConstructorExport,
        params: impl AsRef<[wasmtime::component::Val]>,
    ) -> wasmtime::Result<Utxo<T>> {
        self.construct_utxo(ctx, idx, params)
    }

    #[instrument(level = "trace", skip_all)]
    #[cfg(feature = "async")]
    pub async fn create_utxo_async(
        &self,
        ctx: T,
        ConstructorExport { idx, .. }: &ConstructorExport,
        params: impl AsRef<[wasmtime::component::Val]>,
    ) -> wasmtime::Result<Utxo<T>>
    where
        T: Send,
    {
        self.construct_utxo_async(ctx, idx, params).await
    }

    #[instrument(level = "trace", skip_all)]
    pub fn load_utxo(
        &self,
        ctx: T,
        UtxoStorageExport { set, .. }: &UtxoStorageExport,
        fields: impl Into<Vec<(String, wasmtime::component::Val)>>,
    ) -> wasmtime::Result<Utxo<T>> {
        self.construct_utxo(ctx, set, [wasmtime::component::Val::Record(fields.into())])
    }

    #[instrument(level = "trace", skip_all)]
    #[cfg(feature = "async")]
    pub async fn load_utxo_async(
        &self,
        ctx: T,
        UtxoStorageExport { set, .. }: &UtxoStorageExport,
        fields: impl Into<Vec<(String, wasmtime::component::Val)>>,
    ) -> wasmtime::Result<Utxo<T>>
    where
        T: Send,
    {
        self.construct_utxo_async(ctx, set, [wasmtime::component::Val::Record(fields.into())])
            .await
    }
}

#[derive(Clone)]
pub struct UtxoStorageExport {
    ty: types::Record,
    get: ComponentExportIndex,
    set: ComponentExportIndex,
}

impl UtxoStorageExport {
    #[must_use]
    pub fn ty(&self) -> &types::Record {
        &self.ty
    }
}

#[derive(Clone)]
pub struct UtxoExport {
    resource_ty: ResourceType,
    instance_ty: types::ComponentInstance,
    instance_idx: ComponentExportIndex,
    storage: Option<UtxoStorageExport>,
}

impl UtxoExport {
    #[must_use]
    pub fn storage(&self) -> Option<&UtxoStorageExport> {
        self.storage.as_ref()
    }
}

#[derive(Clone)]
pub struct ConstructorExport {
    ty: types::ComponentFunc,
    idx: ComponentExportIndex,
}

impl ConstructorExport {
    #[must_use]
    pub fn ty(&self) -> &types::ComponentFunc {
        &self.ty
    }
}

#[derive(Clone)]
pub struct MethodExport {
    ty: types::ComponentFunc,
    idx: ComponentExportIndex,
}

impl MethodExport {
    #[must_use]
    pub fn ty(&self) -> &types::ComponentFunc {
        &self.ty
    }
}

pub struct Utxo<T: 'static> {
    store: Store<T>,
    instance: Instance,
    resource: ResourceAny,
}

impl<T: 'static> AsContext for Utxo<T> {
    type Data = T;

    fn as_context(&self) -> StoreContext<'_, Self::Data> {
        self.store.as_context()
    }
}

impl<T: 'static> AsContextMut for Utxo<T> {
    fn as_context_mut(&mut self) -> StoreContextMut<'_, Self::Data> {
        self.store.as_context_mut()
    }
}

impl<T: 'static> Utxo<T> {
    pub fn store(&mut self) -> &mut Store<T> {
        &mut self.store
    }

    #[must_use]
    pub fn resource(&self) -> ResourceAny {
        self.resource
    }

    pub fn storage(&mut self, export: &UtxoStorageExport) -> UtxoStorage<'_, T> {
        UtxoStorage {
            utxo: self,
            get: export.get,
        }
    }

    fn get_function_export(&mut self, name: impl ExportLookup) -> wasmtime::Result<Func> {
        self.instance
            .get_func(&mut self.store, name)
            .context("function export not found")
    }

    #[instrument(level = "trace", skip_all)]
    pub fn call(
        &mut self,
        export: &MethodExport,
        params: impl AsRef<[wasmtime::component::Val]>,
    ) -> wasmtime::Result<Box<[wasmtime::component::Val]>> {
        let f = self.get_function_export(export.idx)?;
        let mut results = vec![wasmtime::component::Val::Bool(false); export.ty.results().len()];
        f.call(&mut self.store, params.as_ref(), &mut results)
            .context("failed to call method")?;
        Ok(results.into_boxed_slice())
    }

    #[instrument(level = "trace", skip_all)]
    #[cfg(feature = "async")]
    pub async fn call_async(
        &mut self,
        export: &MethodExport,
        params: impl AsRef<[wasmtime::component::Val]>,
    ) -> wasmtime::Result<Box<[wasmtime::component::Val]>>
    where
        T: Send,
    {
        let f = self.get_function_export(export.idx)?;
        let mut results = vec![wasmtime::component::Val::Bool(false); export.ty.results().len()];
        f.call_async(&mut self.store, params.as_ref(), &mut results)
            .await
            .context("failed to call method")?;
        Ok(results.into_boxed_slice())
    }

    #[instrument(level = "trace", skip_all)]
    pub fn drop(mut self) -> wasmtime::Result<T> {
        self.resource.resource_drop(&mut self.store)?;
        Ok(self.store.into_data())
    }

    #[instrument(level = "trace", skip_all)]
    #[cfg(feature = "async")]
    pub async fn drop_async(mut self) -> wasmtime::Result<T>
    where
        T: Send,
    {
        self.resource.resource_drop_async(&mut self.store).await?;
        Ok(self.store.into_data())
    }
}

pub struct UtxoStorage<'a, T: 'static> {
    utxo: &'a mut Utxo<T>,
    get: ComponentExportIndex,
}

impl<T: 'static> UtxoStorage<'_, T> {
    #[instrument(level = "trace", skip_all)]
    pub fn get(&mut self) -> wasmtime::Result<Vec<(String, wasmtime::component::Val)>> {
        let f = self.utxo.get_function_export(self.get)?;
        let mut results = [wasmtime::component::Val::Bool(false); 1];
        f.call(
            &mut self.utxo.store,
            &[wasmtime::component::Val::Resource(self.utxo.resource)],
            &mut results,
        )
        .context("failed to call function")?;
        let [wasmtime::component::Val::Record(vs)] = results else {
            bail!("invalid return value")
        };
        Ok(vs)
    }

    #[instrument(level = "trace", skip_all)]
    #[cfg(feature = "async")]
    pub async fn get_async(&mut self) -> wasmtime::Result<Vec<(String, wasmtime::component::Val)>>
    where
        T: Send,
    {
        let f = self.utxo.get_function_export(self.get)?;
        let mut results = [wasmtime::component::Val::Bool(false); 1];
        f.call_async(
            &mut self.utxo.store,
            &[wasmtime::component::Val::Resource(self.utxo.resource)],
            &mut results,
        )
        .await
        .context("failed to call function")?;
        let [wasmtime::component::Val::Record(vs)] = results else {
            bail!("invalid return value")
        };
        Ok(vs)
    }
}
