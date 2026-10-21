//! What a plugin did, kept for the people who run it: its last log lines, its last faults and what
//! each kind of event costs it. Small and bounded; nothing here is in the snapshot, and a restart of
//! the server starts them empty (the log of the server holds the long history).

use std::collections::VecDeque;
use std::sync::Mutex;

use pcds_plugin_api::Level;

use super::limits;

/// One line a plugin (or the host, about the plugin) wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// Counts the lines the plugin has had since it was loaded, from 1.
    pub seq: u64,
    /// Unix seconds.
    pub unix: u64,
    pub level: Level,
    pub text: String,
}

/// The last `LOG_KEPT` lines of one plugin. Shared between the plugin's context (which writes the
/// plugin's own lines) and the host (which writes its own lines about the plugin), so a restart of
/// the instance after a fault keeps the history.
#[derive(Debug, Default)]
pub struct LogRing(Mutex<(u64, VecDeque<LogLine>)>);

impl LogRing {
    pub fn push(&self, level: Level, text: &str) {
        let Ok(mut guard) = self.0.lock() else { return };
        let (count, lines) = &mut *guard;
        if lines.len() >= limits::LOG_KEPT {
            lines.pop_front();
        }
        *count += 1;
        lines.push_back(LogLine { seq: *count, unix: super::approvals::now(), level, text: text.to_string() });
    }

    /// The newest `n` lines, oldest first.
    pub fn last(&self, n: usize) -> Vec<LogLine> {
        let Ok(guard) = self.0.lock() else { return Vec::new() };
        guard.1.iter().skip(guard.1.len().saturating_sub(n)).cloned().collect()
    }

    /// The lines after number `seq` (as `LogLine::seq` counts).
    pub fn after(&self, seq: u64) -> Vec<LogLine> {
        let Ok(guard) = self.0.lock() else { return Vec::new() };
        guard.1.iter().filter(|l| l.seq > seq).cloned().collect()
    }
}

/// One fault, for the history in the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultRecord {
    /// Unix seconds.
    pub unix: u64,
    /// The event or call it happened in (`chat`, `saving its state`).
    pub during: String,
    pub text: String,
}

/// What one kind of event cost a plugin since it was loaded.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KindStats {
    pub calls: u64,
    pub fuel: u64,
    pub max_fuel: u64,
    pub nanos: u64,
    pub max_nanos: u64,
}

impl KindStats {
    pub fn add(&mut self, fuel: u64, nanos: u64) {
        self.calls += 1;
        self.fuel += fuel;
        self.max_fuel = self.max_fuel.max(fuel);
        self.nanos += nanos;
        self.max_nanos = self.max_nanos.max(nanos);
    }
}
