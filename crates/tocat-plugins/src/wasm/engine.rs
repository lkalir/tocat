//! The wasmtime side: one engine, a compiled module per path, an instance per
//! stage.
//!
//! Compilation is the expensive part and instantiation is not, so they are
//! separated. A module is compiled once per process, on the first stage that
//! names it, and kept as an [`InstancePre`] with its imports already resolved.
//! Every stage after that is an instantiation, which is a fresh linear memory
//! and little else.
//!
//! That matters because a stage is per direction per connection: under `fork`
//! with `direction = "both"`, a hundred clients is two hundred instances of
//! the same compiled code. They share nothing mutable, which is the same
//! guarantee every other plugin gives and the reason none of this needs a lock
//! on the data path.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use tocat_api::{PluginError, Result};
use wasmtime::{
    Config, Engine, Instance, InstancePre, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc, WasmParams, WasmResults,
};

use super::{
    NAME,
    abi::{self, ABI_VERSION, Outbox},
};

/// Per-store state. The limiter is what caps a guest's memory growth; without
/// it a guest could ask for as much as the platform allows, once per
/// connection.
pub struct HostState {
    limits: StoreLimits,
}

/// One process-wide engine, because a compiled module belongs to the engine
/// that produced it and sharing modules is the entire point of the cache.
fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();

    ENGINE.get_or_init(|| {
        let mut config = Config::new();

        // Fuel is how a stage that loops forever becomes a failed path rather
        // than a hung relay: `on_bytes` runs on the copy task, and nothing
        // else on that task makes progress while a guest is inside it.
        // Metering costs a few percent and is on unconditionally, since the
        // engine is shared and the alternative is an engine per fuel setting.
        config.consume_fuel(true);

        Engine::new(&config).expect("wasmtime engine with default settings")
    })
}

type Cache = Mutex<HashMap<PathBuf, InstancePre<HostState>>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Cache::default)
}

/// Compile `path`, or hand back the compilation an earlier stage paid for.
///
/// The lock is held across compilation, so two stages naming the same new
/// module do not compile it twice. Startup builds every chain once before any
/// endpoint is opened, so this is paid there rather than on the first byte.
pub fn load(path: &Path) -> Result<InstancePre<HostState>> {
    let path = path
        .canonicalize()
        .map_err(|e| config_error(format!("{}: {e}", path.display())))?;

    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(pre) = cache.get(&path) {
        return Ok(pre.clone());
    }

    let module = Module::from_file(engine(), &path)
        .map_err(|e| config_error(format!("{}: {e}", path.display())))?;

    let pre = prepare(&module).map_err(|e| config_error(format!("{}: {e}", path.display())))?;

    cache.insert(path, pre.clone());

    Ok(pre)
}

/// [`load`] without the filesystem or the cache: same validation, same import
/// refusal, bytes instead of a path.
///
/// Only the tests want that, which is what the `cfg` says. It exists so they
/// can define guests as WAT inline rather than checking in binary fixtures,
/// and so that they still go through [`prepare`]: the test that a module
/// importing WASI is refused is testing the loader, and would test nothing if
/// it built its own `InstancePre`. Drop the `cfg` if something outside the
/// tests ever needs a module from memory.
#[cfg(test)]
pub fn compile(bytes: impl AsRef<[u8]>) -> Result<InstancePre<HostState>> {
    let module = Module::new(engine(), bytes).map_err(|e| config_error(e.to_string()))?;

    prepare(&module)
}

/// Validate a compiled module and resolve its imports, of which there are
/// none.
///
/// Saying so here turns "built against WASI" into a startup error that names
/// the import, rather than a trap on the first chunk, and makes the capability
/// boundary something the loader enforces rather than something the
/// documentation asks for.
fn prepare(module: &Module) -> Result<InstancePre<HostState>> {
    if let Some(import) = module.imports().next() {
        return Err(config_error(format!(
            "guest imports {}::{}, but tocat guests import nothing. Effects are \
             queued in the outbox and applied by the host, so a guest needs no \
             host functions and cannot be built against WASI",
            import.module(),
            import.name(),
        )));
    }

    Linker::new(engine())
        .instantiate_pre(module)
        .map_err(|e| config_error(e.to_string()))
}

/// One instantiated guest, and the exports worth looking up once.
pub struct Guest {
    store: Store<HostState>,
    memory: Memory,
    fuel: u64,

    alloc: TypedFunc<i32, i32>,
    outbox: TypedFunc<(), i32>,
    on_bytes: TypedFunc<(i32, i32), ()>,
    on_eof: Option<TypedFunc<(), ()>>,
    on_tick: Option<TypedFunc<(), ()>>,

    /// Both read once, after `tocat_init`, because that is when the guest
    /// knows its options and because the host reads them once too.
    tick_interval: Option<Duration>,
    datagram_safe: bool,
}

impl Guest {
    /// Instantiate, check the ABI version, and hand the entry's `config` to
    /// `tocat_init` if the guest wants it.
    pub fn new(
        pre: &InstancePre<HostState>,
        memory_max: usize,
        fuel: u64,
        config: &[u8],
    ) -> Result<Self> {
        let state = HostState {
            limits: StoreLimitsBuilder::new().memory_size(memory_max).build(),
        };

        let mut store = Store::new(engine(), state);
        store.limiter(|state| &mut state.limits);
        set_fuel(&mut store, fuel)?;

        let instance = pre
            .instantiate(&mut store)
            .map_err(|e| config_error(format!("instantiating: {e}")))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| config_error("guest exports no memory"))?;

        let version: TypedFunc<(), i32> = required(&instance, &mut store, "tocat_abi_version")?;
        let version = version
            .call(&mut store, ())
            .map_err(|e| config_error(format!("tocat_abi_version: {e}")))?;

        if version != ABI_VERSION {
            return Err(config_error(format!(
                "guest speaks ABI version {version}, this build speaks {ABI_VERSION}"
            )));
        }

        let mut guest = Self {
            memory,
            fuel,
            alloc: required(&instance, &mut store, "tocat_alloc")?,
            outbox: required(&instance, &mut store, "tocat_outbox")?,
            on_bytes: required(&instance, &mut store, "tocat_on_bytes")?,
            on_eof: optional(&instance, &mut store, "tocat_on_eof"),
            on_tick: optional(&instance, &mut store, "tocat_on_tick"),
            tick_interval: None,
            datagram_safe: false,
            store,
        };

        // Config goes in through the same door as a chunk, since it is just
        // bytes the guest wants, and a guest with no options need not export
        // the entrypoint at all.
        if let Some(init) = optional::<(i32, i32), ()>(&instance, &mut guest.store, "tocat_init") {
            let ptr = guest.write(config)?;
            let len = config.len() as i32;

            set_fuel(&mut guest.store, fuel)?;
            init.call(&mut guest.store, (ptr, len))
                .map_err(|e| config_error(format!("tocat_init: {e}")))?;

            // A guest that rejects its options reports it the same way any
            // call does, so a bad option fails at startup carrying the guest's
            // own message rather than trapping later.
            let outbox = guest.outbox()?;
            if outbox.has(abi::FLAG_ERROR) {
                let message = abi::slice(guest.memory(), outbox.message.ptr, outbox.message.len)?;
                return Err(config_error(String::from_utf8_lossy(message).into_owned()));
            }
        }

        guest.tick_interval =
            optional::<(), i64>(&instance, &mut guest.store, "tocat_tick_interval_ns")
                .and_then(|func| func.call(&mut guest.store, ()).ok())
                .and_then(|nanos| u64::try_from(nanos).ok())
                .filter(|nanos| *nanos > 0)
                .map(Duration::from_nanos);

        guest.datagram_safe =
            optional::<(), i32>(&instance, &mut guest.store, "tocat_datagram_safe")
                .and_then(|func| func.call(&mut guest.store, ()).ok())
                .is_some_and(|safe| safe != 0);

        Ok(guest)
    }

    /// The period the guest asked for, or `None` if it wants no ticks. A guest
    /// that asks for one without exporting `tocat_on_tick` gets no timer,
    /// which is the reading that costs nothing.
    pub fn tick_interval(&self) -> Option<Duration> {
        self.tick_interval.filter(|_| self.on_tick.is_some())
    }

    /// Whether the guest claims to preserve message boundaries. False for a
    /// guest that does not say, which is the safe answer and the one the trait
    /// defaults to.
    pub fn datagram_safe(&self) -> bool {
        self.datagram_safe
    }

    pub fn on_bytes(&mut self, input: &[u8]) -> Result<()> {
        let ptr = self.write(input)?;
        let call = &self.on_bytes;

        set_fuel(&mut self.store, self.fuel)?;
        call.call(&mut self.store, (ptr, input.len() as i32))
            .map_err(|e| trap("tocat_on_bytes", &e))
    }

    pub fn on_eof(&mut self) -> Result<()> {
        let Some(call) = &self.on_eof else {
            return Ok(());
        };

        set_fuel(&mut self.store, self.fuel)?;
        call.call(&mut self.store, ())
            .map_err(|e| trap("tocat_on_eof", &e))
    }

    pub fn on_tick(&mut self) -> Result<()> {
        let Some(call) = &self.on_tick else {
            return Ok(());
        };

        set_fuel(&mut self.store, self.fuel)?;
        call.call(&mut self.store, ())
            .map_err(|e| trap("tocat_on_tick", &e))
    }

    /// The outbox left by the last call.
    pub fn outbox(&mut self) -> Result<Outbox> {
        let at = self
            .outbox
            .call(&mut self.store, ())
            .map_err(|e| trap("tocat_outbox", &e))?;

        Outbox::read(self.memory.data(&self.store), at as u32)
    }

    /// Guest memory, for reading the spans the outbox pointed at. Borrowed
    /// rather than copied, so the emission is built straight out of it.
    pub fn memory(&self) -> &[u8] {
        self.memory.data(&self.store)
    }

    /// Ask the guest where to put `bytes`, and put them there.
    ///
    /// `tocat_alloc` is an arena: the host never frees and a guest may hand
    /// back the same buffer every time, so this is a call, a bounds check and
    /// a copy rather than an allocation.
    ///
    /// The pointer it gives back is an address in its linear memory, not an
    /// offset into whatever it uses as an arena. That is the guest's most
    /// likely bug and the host cannot detect it: a wrong-but-valid address is
    /// still writable memory.
    fn write(&mut self, bytes: &[u8]) -> Result<i32> {
        let len = i32::try_from(bytes.len())
            .map_err(|_| PluginError::runtime(NAME, "chunk too large for a 32-bit guest"))?;

        let alloc = &self.alloc;

        set_fuel(&mut self.store, self.fuel)?;
        let ptr = alloc
            .call(&mut self.store, len)
            .map_err(|e| trap("tocat_alloc", &e))?;

        // Zero is how a guest says a chunk does not fit. Writing there anyway
        // would clobber whatever the guest keeps at the bottom of its memory
        // and then hand it a chunk it never agreed to take.
        if ptr <= 0 {
            return Err(PluginError::runtime(
                NAME,
                format!(
                    "guest refused a chunk of {len} bytes. A guest that meant to \
                 accept it may be returning an offset into its own arena \
                 rather than an address in its linear memory"
                ),
            ));
        }

        self.memory
            .write(&mut self.store, ptr as usize, bytes)
            .map_err(|e| {
                PluginError::runtime(NAME, format!("writing {len} bytes into the guest: {e}"))
            })
            .map(|()| ptr)
    }
}

fn required<P, R>(
    instance: &Instance,
    store: &mut Store<HostState>,
    export: &str,
) -> Result<TypedFunc<P, R>>
where
    P: WasmParams,
    R: WasmResults,
{
    instance
        .get_typed_func(store, export)
        .map_err(|e| config_error(format!("{export}: {e}")))
}

fn optional<P, R>(
    instance: &Instance,
    store: &mut Store<HostState>,
    export: &str,
) -> Option<TypedFunc<P, R>>
where
    P: WasmParams,
    R: WasmResults,
{
    instance.get_typed_func(store, export).ok()
}

fn set_fuel(store: &mut Store<HostState>, fuel: u64) -> Result<()> {
    // Zero means unmetered, which is opt-in and documented: it trades the
    // guarantee that a guest cannot hang the relay for a few percent of
    // throughput.
    let fuel = if fuel == 0 { u64::MAX } else { fuel };

    store
        .set_fuel(fuel)
        .map_err(|e| PluginError::runtime(NAME, format!("setting fuel: {e}")))
}

/// A trap is a failed direction, not a warning: the guest was mid-stream and
/// the bytes it was handed have gone nowhere.
fn trap(what: &str, error: &wasmtime::Error) -> PluginError {
    PluginError::runtime(NAME, format!("{what}: {error}"))
}

fn config_error(message: impl Into<String>) -> PluginError {
    PluginError::config(super::NAME, message.into())
}
