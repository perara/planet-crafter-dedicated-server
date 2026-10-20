//! A bank's game process (docs/hot-update.md): the world and the tick, started by the front
//! from the active bank's binary, on the socket the front gave it (PCDS_IPC_FD). It loads its
//! tables, says HELLO, and then either loads the save (START) or restores the snapshot of the
//! bank before it (RESTORE). It builds and parses the Netcode batches and decides what goes out
//! when; the front only moves them. It saves the world on SAVE (the front's autosave timer), on
//! STOP and when the front is gone. At a flip it stops at a tick boundary, sends what is
//! queued, writes its snapshot into a memfd and waits: EXIT (the new bank runs) or RESUME.
//!
//!   pcds bank DATA_DIR SAVE.json        (started by the front, never by hand)
//!     PCDS_DEV=flip-verify       after a restore, check the state equals the snapshot (tests)
//!     PCDS_DEV=bank-fail=WHAT    tests: "restore" dies on RESTORE, "protocol" says another protocol,
//!                        "slow-hello" holds its HELLO 4 s (the flip waits in its loading phase),
//!                        "slow-ready" holds its READY 2 s after the restore (the flip waits in
//!                        its restoring phase), "die-after-ready" ends 1 s after READY
//!
//! The modules (docs/architecture.md, "The bank, module by module"): `run` the process, `serve` the
//! loop, `start` the world, `admin` the requests (a route table), `persist` the saves, `status` the
//! status, `mail` the clients' batches, `forward` what the front is told, `plugins_glue` the host,
//! `panic` the containment policy.

mod admin;
mod forward;
mod mail;
mod panic;
mod persist;
mod plugins_glue;
mod run;
mod serve;
mod start;
mod status;

pub use mail::reason_batch;
#[cfg(test)]
pub(crate) use panic::contained;
pub use run::run;
pub use start::enforce;

use crate::ipc;
use crate::msg::BankInfo;

/// What this bank says in its HELLO, and `pcds bank-info` prints: its version, the front
/// protocol it speaks, the snapshot format it writes and reads (only that one), and the format
/// of the game tables it loads (and its read-game writes).
pub fn info() -> BankInfo {
    BankInfo::new(env!("CARGO_PKG_VERSION"), ipc::PROTOCOL, crate::handoff::FORMAT.into(), crate::native::tables::TABLES_FORMAT.into())
}

