//! A plugin that is a WebAssembly module, run by wasmi (docs/plugins.md, "Why wasmi").
//!
//! The module's side of the contract is `pcds-plugin-api`; the host's side is here:
//!
//! - **Imports** (module `pcds`): `log`, `query`, `say`, `tell`, `kick`, `teleport`,
//!   `rules_patch`, `http_request`, each checked against the capabilities granted (ctx.rs).
//! - **Exports**: `memory`, `pcds_abi`, `pcds_alloc`, `pcds_event`; optional `pcds_free`,
//!   `pcds_load`, `pcds_save`, `pcds_state_version`.
//! - **Limits**: fuel per call (the call stops with a trap when it is spent), memory pages, one
//!   instance, one memory, one table. No WASI, no clock, no random numbers, no files, no sockets:
//!   a module can only compute and call the imports above.
//! - **Host memory safety**: every pointer a module hands over is checked against its memory; one
//!   outside it traps the call.

use pcds_plugin_api::{ABI_MAJOR, ABI_MINOR, Event, Level, Verdict};
use wasmi::{Caller, Config, CustomFuelCosts, Engine, Error, Extern, Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc};

use super::ctx::Ctx;
use super::limits::{self, Limits};
use super::runtime::{Cost, Fault, Runtime};
use super::state::Blob;

/// The imports a module may have (names under `pcds`).
pub(super) const IMPORTS: [&str; 8] = ["log", "query", "say", "tell", "kick", "teleport", "rules_patch", "http_request"];

/// The engine every plugin shares: fuel on, nothing a module could use to find out about the
/// machine or to do work the host does not count.
pub fn engine() -> Engine {
    let mut config = Config::default();
    config.consume_fuel(true);
    // wasmi's own price for bulk memory (memory.fill and copy, table ops) is a unit of fuel per 64
    // bytes: a fill of the 64 MiB a module may have cost a million fuel and 14 ms, where a million
    // fuel is meant to be a third of a millisecond. Four bytes a unit puts a fill at about the time
    // of the instructions the same fuel buys (the other two costs are wasmi's own numbers).
    config.fuel_cost(CustomFuelCosts { bytes_copied_per_fuel: limits::BYTES_COPIED_PER_FUEL, fuel_per_bytes_translated: 7, fuel_per_bytes_validated: 2 });
    Engine::new(&config)
}

/// What the host learned about a module before running any of it.
#[derive(Debug, Clone)]
pub struct Inspected {
    pub imports: Vec<String>,
    pub bytes: usize,
}

/// Parses and validates a module and checks it against the contract: only known imports, the
/// required exports. Nothing of it runs.
pub fn compile(engine: &Engine, bytes: &[u8]) -> Result<(Module, Inspected), String> {
    if bytes.len() > limits::MAX_MODULE_BYTES {
        return Err(format!("the module is {} bytes: at most {} are loaded", bytes.len(), limits::MAX_MODULE_BYTES));
    }
    let module = Module::new(engine, bytes).map_err(|e| format!("not a valid WebAssembly module: {e}"))?;
    let mut imports = Vec::new();
    for i in module.imports() {
        let name = format!("{}.{}", i.module(), i.name());
        if i.module() != "pcds" || !IMPORTS.contains(&i.name()) || i.ty().func().is_none() {
            return Err(format!("the module imports {name}, which this server does not provide (it provides {})", IMPORTS.iter().map(|n| format!("pcds.{n}")).collect::<Vec<_>>().join(", ")));
        }
        imports.push(name);
    }
    let exports: Vec<String> = module.exports().map(|e| e.name().to_string()).collect();
    for needed in ["memory", "pcds_abi", "pcds_alloc", "pcds_event"] {
        if !exports.iter().any(|e| e == needed) {
            return Err(format!("the module does not export {needed}"));
        }
    }
    Ok((module, Inspected { imports, bytes: bytes.len() }))
}

/// What the instances of one host hold, for counting: a test (and the listing) can tell that an
/// unloaded plugin left nothing behind.
#[derive(Debug, Default)]
pub struct Gauge {
    pub instances: std::sync::atomic::AtomicUsize,
    /// 64 KiB pages of linear memory held.
    pub pages: std::sync::atomic::AtomicUsize,
}

impl Gauge {
    pub fn live(&self) -> (usize, usize) {
        (self.instances.load(std::sync::atomic::Ordering::SeqCst), self.pages.load(std::sync::atomic::Ordering::SeqCst))
    }
}

struct Inner {
    ctx: Ctx,
    memory: Option<Memory>,
    limits: StoreLimits,
}

struct Funcs {
    event: TypedFunc<(u32, u32, u32), i32>,
    alloc: TypedFunc<u32, u32>,
    free: Option<TypedFunc<(u32, u32), ()>>,
    load: Option<TypedFunc<(u32, u32, u32), i32>>,
    save: Option<TypedFunc<(), u64>>,
    unload: Option<TypedFunc<(), i32>>,
    state_version: Option<TypedFunc<(), u32>>,
    memory: Memory,
}

pub struct Wasm {
    engine: Engine,
    module: Module,
    store: Store<Inner>,
    funcs: Funcs,
    limits: Limits,
    gauge: std::sync::Arc<Gauge>,
    pages: usize,
}

impl Drop for Wasm {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.gauge.instances.fetch_sub(1, SeqCst);
        self.gauge.pages.fetch_sub(self.pages, SeqCst);
    }
}

fn trap(msg: &str) -> Error {
    Error::new(msg)
}

/// The module's memory and the host's state, together.
fn split<'a>(c: &'a mut Caller<'_, Inner>) -> Result<(&'a mut [u8], &'a mut Inner), Error> {
    let memory = c.data().memory.ok_or_else(|| trap("the module has no memory"))?;
    Ok(memory.data_and_store_mut(c))
}

/// `len` bytes at `ptr` in the module's memory; a range outside it traps.
fn slice(memory: &[u8], ptr: u32, len: u32) -> Result<&[u8], Error> {
    let end = (ptr as usize).checked_add(len as usize).filter(|e| *e <= memory.len() && len as usize <= limits::MAX_PAYLOAD);
    end.map(|end| &memory[ptr as usize..end]).ok_or_else(|| trap("the module gave the host a pointer outside its memory"))
}

/// Takes `fuel` out of the call's budget: the host's work for a plugin is the plugin's to pay,
/// or a module that names big buffers would have the host copy, convert and parse for as long as
/// its instructions last (a million-byte `log` costs the host a thousand times what the call
/// instruction costs the module). Out of fuel ends the call as any other overrun does.
fn charge(c: &mut Caller<'_, Inner>, fuel: u64) -> Result<(), Error> {
    let left = c.get_fuel()?;
    if fuel > left {
        c.set_fuel(0)?;
        return Err(Error::from(wasmi::TrapCode::OutOfFuel));
    }
    c.set_fuel(left - fuel)
}

/// At most `max` bytes at `ptr` as text, paid for in fuel: the call, and a fuel for each byte.
/// A text longer than `max` is cut there (every user of the text cuts it shorter still).
fn text(c: &mut Caller<'_, Inner>, ptr: u32, len: u32, max: usize) -> Result<String, Error> {
    let len = len.min(max as u32);
    charge(c, limits::HOST_CALL_FUEL + len as u64 * limits::FUEL_PER_BYTE)?;
    let (mem, _) = split(c)?;
    Ok(String::from_utf8_lossy(slice(mem, ptr, len)?).into_owned())
}

fn linker(engine: &Engine) -> Result<Linker<Inner>, String> {
    let mut l = <Linker<Inner>>::new(engine);
    let e = |e: wasmi::errors::LinkerError| e.to_string();
    l.func_wrap("pcds", "log", |mut c: Caller<'_, Inner>, level: i32, ptr: u32, len: u32| -> Result<(), Error> {
        let level = match level {
            0 => Level::Error,
            1 => Level::Warn,
            3 => Level::Debug,
            _ => Level::Info,
        };
        // A log line is cut to LOG_LINE_CHARS characters: more bytes than that are never looked at.
        let line = text(&mut c, ptr, len, limits::LOG_LINE_CHARS * 4)?;
        c.data_mut().ctx.log(level, &line);
        Ok(())
    })
    .map_err(e)?;
    l.func_wrap("pcds", "query", |mut c: Caller<'_, Inner>, kind: u32, _arg: u32, _arg_len: u32, out: u32, cap: u32| -> Result<i32, Error> {
        charge(&mut c, limits::HOST_CALL_FUEL)?;
        let (mem, inner) = split(&mut c)?;
        // The answer is built once for a call (the world stands still during it): asking again costs the copy.
        let answer = match inner.ctx.query(kind) {
            Ok(a) => a,
            Err(code) => return Ok(code),
        };
        let n = answer.len().min(cap as usize);
        let total = answer.len() as i32;
        let end = (out as usize).checked_add(n).filter(|e| *e <= mem.len()).ok_or_else(|| trap("the module gave the host a buffer outside its memory"))?;
        mem[out as usize..end].copy_from_slice(&answer[..n]);
        charge(&mut c, n as u64 * limits::FUEL_PER_BYTE)?;
        Ok(total)
    })
    .map_err(e)?;
    l.func_wrap("pcds", "say", |mut c: Caller<'_, Inner>, ptr: u32, len: u32| -> Result<i32, Error> {
        let t = text(&mut c, ptr, len, limits::MAX_TEXT_BYTES)?;
        Ok(c.data_mut().ctx.say(&t))
    })
    .map_err(e)?;
    l.func_wrap("pcds", "tell", |mut c: Caller<'_, Inner>, client: u64, ptr: u32, len: u32| -> Result<i32, Error> {
        let t = text(&mut c, ptr, len, limits::MAX_TEXT_BYTES)?;
        Ok(c.data_mut().ctx.tell(client, &t))
    })
    .map_err(e)?;
    l.func_wrap("pcds", "kick", |mut c: Caller<'_, Inner>, client: u64, ptr: u32, len: u32| -> Result<i32, Error> {
        let t = text(&mut c, ptr, len, limits::MAX_TEXT_BYTES)?;
        Ok(c.data_mut().ctx.kick(client, &t))
    })
    .map_err(e)?;
    l.func_wrap("pcds", "teleport", |mut c: Caller<'_, Inner>, client: u64, x: f32, y: f32, z: f32| -> i32 { c.data_mut().ctx.teleport(client, [x, y, z]) }).map_err(e)?;
    l.func_wrap("pcds", "rules_patch", |mut c: Caller<'_, Inner>, ptr: u32, len: u32| -> Result<i32, Error> {
        if len as usize > limits::MAX_PATCH_BYTES {
            c.data_mut().ctx.refused += 1;
            return Ok(pcds_plugin_api::host::E_BAD);
        }
        // Parsed as JSON before anything else is looked at: a fuel for each byte copied and more for the parse.
        charge(&mut c, len as u64 * limits::FUEL_PER_PARSED_BYTE)?;
        let t = text(&mut c, ptr, len, limits::MAX_PATCH_BYTES)?;
        Ok(c.data_mut().ctx.rules_patch(&t))
    })
    .map_err(e)?;
    // ABI 1.2. The request is a buffer of its own size limit (a body may be a megabyte): a length over
    // it is refused with a code, a range outside the memory traps as everywhere.
    l.func_wrap("pcds", "http_request", |mut c: Caller<'_, Inner>, ptr: u32, len: u32| -> Result<i64, Error> {
        charge(&mut c, limits::HOST_CALL_FUEL)?;
        let (mem, inner) = split(&mut c)?;
        let end = (ptr as usize).checked_add(len as usize).filter(|e| *e <= mem.len()).ok_or_else(|| trap("the module gave the host a pointer outside its memory"))?;
        if len as usize > limits::MAX_HTTP_REQUEST {
            inner.ctx.refused += 1;
            return Ok(pcds_plugin_api::host::E_BAD as i64);
        }
        // Nothing is copied for a plugin that may not make requests.
        if let Some(code) = inner.ctx.http_refused() {
            return Ok(code);
        }
        let bytes = mem[ptr as usize..end].to_vec();
        // The copy, and the request taken apart, checked and copied again (a byte of URL, header or body costs the host more than a byte of text).
        charge(&mut c, len as u64 * limits::FUEL_PER_PARSED_BYTE)?;
        Ok(c.data_mut().ctx.http_request(&bytes))
    })
    .map_err(e)?;
    Ok(l)
}

fn fault(e: Error) -> Fault {
    match e.as_trap_code() {
        Some(wasmi::TrapCode::OutOfFuel) => Fault::OutOfFuel,
        _ => Fault::Trap(e.to_string()),
    }
}

impl Wasm {
    /// An instance of the module, not started yet. The module's start function runs here (with the
    /// load budget); `pcds_load` runs in `start`.
    pub fn new(engine: &Engine, module: &Module, ctx: Ctx, limits: Limits, gauge: std::sync::Arc<Gauge>) -> Result<Wasm, String> {
        use std::sync::atomic::Ordering::SeqCst;
        let (store, funcs) = instantiate(engine, module, ctx, limits)?;
        gauge.instances.fetch_add(1, SeqCst);
        let mut w = Wasm { engine: engine.clone(), module: module.clone(), store, funcs, limits, gauge, pages: 0 };
        w.account();
        Ok(w)
    }

    /// The memory held now, in the gauge.
    fn account(&mut self) {
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.funcs.memory.size(&self.store) as usize;
        if now >= self.pages {
            self.gauge.pages.fetch_add(now - self.pages, SeqCst);
        } else {
            self.gauge.pages.fetch_sub(self.pages - now, SeqCst);
        }
        self.pages = now;
    }

    /// Calls an export with the fuel of this call; the fuel it used.
    fn used(&self, budget: u64) -> u64 {
        budget.saturating_sub(self.store.get_fuel().unwrap_or(0))
    }

    /// Writes `bytes` into a buffer the module allocates; its address.
    fn put(&mut self, bytes: &[u8]) -> Result<u32, Fault> {
        let ptr = self.funcs.alloc.call(&mut self.store, bytes.len() as u32).map_err(fault)?;
        if ptr == 0 && !bytes.is_empty() {
            return Err(Fault::Bad("pcds_alloc gave no memory".into()));
        }
        self.funcs.memory.write(&mut self.store, ptr as usize, bytes).map_err(|_| Fault::Bad("pcds_alloc gave an address outside the memory".into()))?;
        Ok(ptr)
    }

    fn release(&mut self, ptr: u32, len: usize) -> Result<(), Fault> {
        match self.funcs.free {
            Some(free) => free.call(&mut self.store, (ptr, len as u32)).map_err(fault),
            None => Ok(()),
        }
    }
}

fn instantiate(engine: &Engine, module: &Module, ctx: Ctx, limits: Limits) -> Result<(Store<Inner>, Funcs), String> {
    let store_limits = StoreLimitsBuilder::new().memory_size(limits.memory_pages as usize * 65536).instances(1).memories(1).tables(1).table_elements(10_000).trap_on_grow_failure(false).build();
    let mut store = Store::new(engine, Inner { ctx, memory: None, limits: store_limits });
    store.limiter(|inner| &mut inner.limits);
    store.set_fuel(limits::FUEL_LOAD).map_err(|e| e.to_string())?;
    let instance: Instance = linker(engine)?.instantiate_and_start(&mut store, module).map_err(|e| format!("cannot start: {e}"))?;
    let memory = instance.get_export(&store, "memory").and_then(Extern::into_memory).ok_or("the module's memory is not a memory")?;
    store.data_mut().memory = Some(memory);
    let func = |name: &str| instance.get_typed_func::<(), u32>(&store, name).map_err(|e| format!("{name}: {e}"));
    let abi = func("pcds_abi")?.call(&mut store, ()).map_err(|e| format!("pcds_abi: {e}"))?;
    if abi >> 16 != ABI_MAJOR || (abi & 0xffff) > ABI_MINOR {
        return Err(format!("the module was built for plugin ABI {}.{}; this server speaks {ABI_MAJOR}.{ABI_MINOR} (same major, and no newer minor)", abi >> 16, abi & 0xffff));
    }
    let funcs = Funcs {
        event: instance.get_typed_func(&store, "pcds_event").map_err(|e| format!("pcds_event: {e}"))?,
        alloc: instance.get_typed_func(&store, "pcds_alloc").map_err(|e| format!("pcds_alloc: {e}"))?,
        free: instance.get_typed_func(&store, "pcds_free").ok(),
        load: instance.get_typed_func(&store, "pcds_load").ok(),
        save: instance.get_typed_func(&store, "pcds_save").ok(),
        unload: instance.get_typed_func(&store, "pcds_unload").ok(),
        state_version: instance.get_typed_func(&store, "pcds_state_version").ok(),
        memory,
    };
    Ok((store, funcs))
}

impl Runtime for Wasm {
    fn ctx(&mut self) -> &mut Ctx {
        &mut self.store.data_mut().ctx
    }

    fn start(&mut self, state: Option<&Blob>) -> Result<(), Fault> {
        let Some(load) = self.funcs.load else { return Ok(()) };
        self.store.set_fuel(limits::FUEL_LOAD).map_err(|e| Fault::Trap(e.to_string()))?;
        let (ptr, len, version) = match state {
            Some(b) => (self.put(&b.bytes)?, b.bytes.len(), b.version),
            None => (0, 0, 0),
        };
        let code = load.call(&mut self.store, (ptr, len as u32, version)).map_err(fault)?;
        if state.is_some() {
            self.release(ptr, len)?;
        }
        if code < 0 {
            return Err(Fault::Bad(format!("pcds_load refused its state (returned {code})")));
        }
        Ok(())
    }

    fn event(&mut self, event: &Event<'_>, payload: &[u8]) -> Result<(Verdict, Cost), Fault> {
        let budget = self.limits.fuel_per_event;
        self.store.set_fuel(budget).map_err(|e| Fault::Trap(e.to_string()))?;
        let ptr = self.put(payload)?;
        let code = self.funcs.event.call(&mut self.store, (event.kind() as u32, ptr, payload.len() as u32)).map_err(fault)?;
        self.release(ptr, payload.len())?;
        let cost = Cost { fuel: self.used(budget) };
        self.account();
        match code {
            0 => Ok((Verdict::Continue, cost)),
            1 => Ok((Verdict::Veto, cost)),
            c if c < 0 => Err(Fault::Bad(format!("the plugin reported error {c}"))),
            c => Err(Fault::Bad(format!("pcds_event returned {c}: 0 and 1 are the answers"))),
        }
    }

    fn save(&mut self) -> Result<Option<Blob>, Fault> {
        let Some(save) = self.funcs.save else { return Ok(None) };
        self.store.set_fuel(self.limits.fuel_per_save).map_err(|e| Fault::Trap(e.to_string()))?;
        let packed = save.call(&mut self.store, ()).map_err(fault)?;
        if packed == 0 {
            return Ok(None);
        }
        let (ptr, len) = ((packed >> 32) as u32, (packed & 0xffff_ffff) as u32);
        if len as usize > limits::MAX_STATE_BYTES {
            return Err(Fault::Bad(format!("the state is {len} bytes: at most {} are kept", limits::MAX_STATE_BYTES)));
        }
        let mut bytes = vec![0u8; len as usize];
        self.funcs.memory.read(&self.store, ptr as usize, &mut bytes).map_err(|_| Fault::Bad("pcds_save pointed outside the memory".into()))?;
        let version = match self.funcs.state_version {
            Some(f) => f.call(&mut self.store, ()).map_err(fault)?,
            None => 0,
        };
        self.release(ptr, len as usize)?;
        Ok(Some(Blob { version, bytes }))
    }

    fn unload(&mut self) -> Result<(), Fault> {
        let Some(unload) = self.funcs.unload else { return Ok(()) };
        self.store.set_fuel(self.limits.fuel_per_save).map_err(|e| Fault::Trap(e.to_string()))?;
        let code = unload.call(&mut self.store, ()).map_err(fault)?;
        if code < 0 {
            return Err(Fault::Bad(format!("pcds_unload returned {code}")));
        }
        Ok(())
    }

    fn restart(&mut self, state: Option<&Blob>) -> Result<(), Fault> {
        let ctx = self.store.data().ctx.renewed();
        let (store, funcs) = instantiate(&self.engine, &self.module, ctx, self.limits).map_err(Fault::Trap)?;
        self.store = store;
        self.funcs = funcs;
        self.account();
        self.start(state)
    }
}
