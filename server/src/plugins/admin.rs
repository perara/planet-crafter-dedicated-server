//! The admin API's plugin endpoints (answered by the bank, which hosts the plugins):
//!
//! - `GET /api/plugins`: every plugin with its status, capabilities asked and granted, budgets,
//!   counters and last error; the capabilities there are.
//! - `POST /api/plugins/reload`: read the folder again.
//! - `POST /api/plugins/enable {"id", "grant": [...]}`: approve (default: all it asks for) and reload.
//! - `POST /api/plugins/disable {"id"}`.
//! - `POST /api/plugins/events {"after", "types", "limit"}`: the event stream (stream.rs).

use pcds_plugin_api::{Kind, caps};
use serde_json::{Value, json};

use super::{Loaded, stream};
use crate::game::Game;

fn error(status: u16, e: impl std::fmt::Display) -> (u16, String) {
    (status, json!({"error": e.to_string()}).to_string())
}

pub fn plugin_json(p: &Loaded) -> Value {
    let m = p.manifest.as_ref();
    let denied: Vec<&String> = p.requested.iter().filter(|c| !p.grant.has(c)).collect();
    let calls = p.stats.calls.max(1);
    json!({
        "id": p.id,
        "name": m.map(|m| m.plugin.name.as_str()),
        "version": m.map(|m| m.plugin.version.as_str()),
        "description": m.map(|m| m.plugin.description.as_str()),
        "authors": m.map(|m| m.plugin.authors.clone()),
        "license": m.map(|m| m.plugin.license.as_str()),
        "kind": m.map(|m| m.plugin.kind.name()),
        "status": p.status.name(),
        "detail": p.status.detail(),
        "requested": p.requested,
        "granted": p.grant.names(),
        "notGranted": denied,
        "commands": p.commands.iter().map(|c| json!({"name": c, "help": m.and_then(|m| m.commands.iter().find(|x| x.name.eq_ignore_ascii_case(c))).map(|x| x.help.as_str())})).collect::<Vec<_>>(),
        "events": Kind::ALL.iter().filter(|k| p.kinds & k.bit() != 0).map(|k| k.name()).collect::<Vec<_>>(),
        "warnings": p.warnings,
        "limits": {"fuelPerEvent": p.limits.fuel_per_event, "fuelPerSave": p.limits.fuel_per_save, "memoryPages": p.limits.memory_pages, "tickEveryMs": p.limits.tick_every_ms},
        "stats": {
            "calls": p.stats.calls, "faults": p.stats.faults, "restarts": p.stats.restarts, "slow": p.stats.slow, "shed": p.stats.shed,
            "denied": p.stats.denied, "refused": p.stats.refused, "actions": p.stats.actions, "vetoes": p.stats.vetoes,
            "fuel": p.stats.fuel, "averageMicros": p.stats.nanos as f64 / calls as f64 / 1000.0, "maxMicros": p.stats.max_nanos as f64 / 1000.0,
            "lastError": p.stats.last_error, "lastFaultAt": p.stats.last_fault_unix,
        },
        "digest": p.digest,
    })
}

/// Part of /api/status: how many plugins there are, how many run, how many faults they had.
pub fn summary(game: &Game) -> Value {
    match game.ext.plugins.host.as_ref() {
        None => json!({"loaded": 0, "running": 0, "faults": 0}),
        Some(h) => json!({
            "loaded": h.plugins.len(),
            "running": h.plugins.iter().filter(|p| p.running()).count(),
            "faults": h.plugins.iter().map(|p| p.stats.faults).sum::<u64>(),
        }),
    }
}

pub fn request(game: &mut Game, method: &str, path: &str, body: &Value) -> (u16, String) {
    let dir = game.ext.plugins.place.as_ref().map(|(d, _)| d.clone());
    let id = || body["id"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| error(400, "'id' is required"));
    match (method, path) {
        ("GET", "/api/plugins") => {
            let host = game.ext.plugins.host.as_ref();
            let plugins: Vec<Value> = host.map(|h| h.plugins.iter().map(plugin_json).collect()).unwrap_or_default();
            let body = json!({
                "plugins": plugins,
                "folder": dir.map(|d| d.display().to_string()),
                "approvalsError": host.and_then(|h| h.approvals_error.clone()),
                "capabilities": caps::ALL.iter().map(|(n, d)| json!({"name": n, "allows": d})).collect::<Vec<_>>(),
                "abi": {"major": pcds_plugin_api::ABI_MAJOR, "minor": pcds_plugin_api::ABI_MINOR},
                "stream": {"epoch": game.ext.plugins.saved.stream.epoch},
            });
            (200, body.to_string())
        }
        ("POST", "/api/plugins/reload") => match super::reload(game) {
            Ok(line) => (200, json!({"ok": true, "message": line}).to_string()),
            Err(e) => error(500, e),
        },
        ("POST", "/api/plugins/enable") => {
            let (Some(dir), id) = (dir, id()) else { return error(500, "the plugin host has not started") };
            let id = match id {
                Ok(id) => id.to_string(),
                Err(e) => return e,
            };
            let grant = body["grant"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>());
            if let Err(e) = super::approve(&dir, &id, grant) {
                return error(400, e);
            }
            reload_answer(game, &id)
        }
        ("POST", "/api/plugins/disable") => {
            let (Some(dir), id) = (dir, id()) else { return error(500, "the plugin host has not started") };
            let id = match id {
                Ok(id) => id.to_string(),
                Err(e) => return e,
            };
            if let Err(e) = super::disable(&dir, &id) {
                return error(400, e);
            }
            reload_answer(game, &id)
        }
        ("POST", "/api/plugins/events") => events(game, body),
        _ => error(404, "unknown endpoint"),
    }
}

fn reload_answer(game: &mut Game, id: &str) -> (u16, String) {
    match super::reload(game) {
        Ok(_) => {
            let status = game.ext.plugins.host.as_ref().and_then(|h| h.plugins.iter().find(|p| p.id == id)).map(plugin_json);
            (200, json!({"ok": true, "plugin": status}).to_string())
        }
        Err(e) => error(500, e),
    }
}

fn events(game: &mut Game, body: &Value) -> (u16, String) {
    let mut kinds = 0u32;
    match body["types"].as_array() {
        None => kinds = stream::DEFAULT_KINDS,
        Some(types) => {
            for t in types {
                let Some(kind) = t.as_str().and_then(Kind::from_name).filter(|k| stream::STREAMABLE & k.bit() != 0) else {
                    return error(400, format!("types: {t} cannot be streamed (the kinds are: {})", Kind::ALL.iter().filter(|k| stream::STREAMABLE & k.bit() != 0).map(|k| k.name()).collect::<Vec<_>>().join(", ")));
                };
                kinds |= kind.bit();
            }
        }
    }
    let after = body["after"].as_u64().unwrap_or(0);
    let limit = body["limit"].as_u64().unwrap_or(200).clamp(1, 500) as usize;
    let ring = &mut game.ext.plugins.saved.stream;
    ring.ask(kinds);
    let answer = ring.read(after, kinds, limit);
    game.ext.plugins.refresh();
    (200, answer.to_string())
}

