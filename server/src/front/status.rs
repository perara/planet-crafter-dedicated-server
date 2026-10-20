//! GET /api/status: the bank answers with the world (players, planet, rules, tick times); the
//! front adds what only it knows (the network, the invite code, the process, the banks and their
//! updates) to that answer (`overlay`), and answers alone while the world is stopped.

use serde_json::{json, Value};

use super::{Bank, Front};
use crate::ipc;
use crate::updater;

impl Front {
    /// The front's part of GET /api/status.
    pub(super) fn overlay(&self, s: &mut Value) {
        let steam = self.net.steam_clients();
        for p in s["players"].as_array_mut().into_iter().flatten() {
            if p["id"].as_u64().is_some_and(|id| steam.contains(&id)) {
                p["ip"] = "steam".into();
            }
        }
        let banks: Vec<u32> = [&self.live, &self.next].into_iter().flatten().map(|b| b.child.id()).collect();
        let (mut cpu, mut memory) = (crate::dashboard::reaped_children_cpu(), 0);
        let me = crate::dashboard::process();
        cpu += me["cpuSeconds"].as_f64().unwrap_or(0.0);
        memory += me["memoryBytes"].as_u64().unwrap_or(0);
        for pid in banks {
            let (c, m) = crate::dashboard::process_of(pid);
            cpu += c;
            memory += m;
        }
        s["network"] = self.net.network().into();
        s["inviteCode"] = self.net.invite_code.clone().into();
        s["steam"] = self.net.invite_code.is_some().into();
        s["port"] = self.net.port.into();
        s["uptimeSeconds"] = self.started.elapsed().as_secs().into();
        s["autosaveMinutes"] = self.autosave.minutes.into();
        s["process"] = json!({"cpuSeconds": cpu, "memoryBytes": memory});
        s["traffic"] = crate::dashboard::traffic();
        s["banks"] = self.banks_status();
        s["worldControl"] = true.into();
        s["update"] = self.update_status();
    }

    fn banks_status(&self) -> Value {
        let bank = |b: &Bank| json!({"bank": b.name, "pid": b.child.id(), "version": b.version(), "protocol": ipc::PROTOCOL, "format": b.info().and_then(|i| i.format), "runningSeconds": b.started.elapsed().as_secs()});
        let mut v = match &self.banks {
            Some(dir) => updater::describe(dir),
            None => json!({"dir": null, "active": null, "installed": []}),
        };
        v["live"] = self.live.as_ref().map(bank).unwrap_or(Value::Null);
        v["next"] = self.next.as_ref().map(bank).unwrap_or(Value::Null);
        v["phase"] = self.phase.label().into();
        v["flips"] = self.flips.into();
        v["crashes"] = self.crashes.len().into();
        v["frontProtocol"] = ipc::PROTOCOL.into();
        v["frontVersion"] = env!("CARGO_PKG_VERSION").into();
        v
    }

    /// GET /api/status while the world is stopped: the front's part, and which world it was.
    pub(super) fn stopped_status(&self) -> Value {
        let mut s = json!({"state": "stopped", "save": self.world_name(), "players": [], "serverVersion": env!("CARGO_PKG_VERSION"), "gameVersion": self.game_version});
        self.overlay(&mut s);
        s
    }
}
