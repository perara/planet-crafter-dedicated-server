//! What the host needs of a plugin, whatever it is made of: a WebAssembly module (`wasm.rs`) or a
//! rules file the host runs itself (`data.rs`).

use pcds_plugin_api::{Event, Verdict};

use super::ctx::Ctx;
use super::state::Blob;

/// A call that did not end as it should. Each counts against the plugin's error budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// The call used up its fuel.
    OutOfFuel,
    /// The module trapped (unreachable, a panic compiled to one, a bad memory access, a limit).
    Trap(String),
    /// The answer was no answer (a value out of range, a pointer outside the memory, state over
    /// the limit, or an error the plugin reported itself).
    Bad(String),
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::OutOfFuel => f.write_str("out of fuel (the call did more than its budget)"),
            Fault::Trap(t) => write!(f, "trap: {t}"),
            Fault::Bad(t) => write!(f, "bad answer: {t}"),
        }
    }
}

/// What one event call cost.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cost {
    pub fuel: u64,
}

pub trait Runtime: Send + Sync {
    fn ctx(&mut self) -> &mut Ctx;

    /// Starts the plugin: the module's start function and `pcds_load` with the state it kept.
    fn start(&mut self, state: Option<&Blob>) -> Result<(), Fault>;

    /// One event. `payload` is the event as bytes (`Event::encode`) when the plugin is a module;
    /// a data plugin reads the event itself.
    fn event(&mut self, event: &Event<'_>, payload: &[u8]) -> Result<(Verdict, Cost), Fault>;

    /// The state to keep, if the plugin has any.
    fn save(&mut self) -> Result<Option<Blob>, Fault>;

    /// The last call before the instance is dropped (the plugin is switched off, removed or faulted
    /// off), within the budget of a save. A plugin without one has nothing to do.
    fn unload(&mut self) -> Result<(), Fault> {
        Ok(())
    }

    /// A fresh instance after a fault, started with the state it last gave.
    fn restart(&mut self, state: Option<&Blob>) -> Result<(), Fault>;
}
