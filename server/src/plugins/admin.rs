//! The admin API's plugin endpoints (answered by the bank, which hosts the plugins):
//!
//! - `GET /api/plugins`: every plugin with its status, capabilities asked and granted, budgets,
//!   counters and last error; the capabilities there are.
//! - `POST /api/plugins/reload`: read the folder again.
//! - `POST /api/plugins/enable {"id", "grant": [...], "digest"}`: approve (default: all it asks for) and
//!   reload; with the `digest` the listing showed, only if the files are still those.
//! - `POST /api/plugins/disable {"id"}`.
//! - `POST /api/plugins/events {"after", "types", "limit"}`: the event stream (stream.rs).
//! - `GET /api/plugins/tokens`, `POST /api/plugins/tokens {"name", "scopes", "ratePerMinute"}` (the
//!   token is in the answer once) and `POST /api/plugins/tokens/revoke {"id"}`: scoped tokens (tokens.rs).
//! - `POST /api/plugins/config {"id", "key", "value"}`: the admin's value for a key a plugin declares
//!   (`value` null or left out: removed). A secret's value is never in any answer.
//! - `POST /api/plugins/trust {"publicKey", "name", "grant": [...], "force"}` and
//!   `POST /api/plugins/untrust {"id"}`: the signing keys the admin trusts (sign.rs).

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
        "notice": p.notice,
        "limits": {"fuelPerEvent": p.limits.fuel_per_event, "fuelPerSave": p.limits.fuel_per_save, "memoryPages": p.limits.memory_pages, "tickEveryMs": p.limits.tick_every_ms},
        "stats": {
            "calls": p.stats.calls, "faults": p.stats.faults, "restarts": p.stats.restarts, "slow": p.stats.slow, "shed": p.stats.shed,
            "denied": p.stats.denied, "refused": p.stats.refused, "actions": p.stats.actions, "vetoes": p.stats.vetoes,
            "fuel": p.stats.fuel, "averageMicros": p.stats.nanos as f64 / calls as f64 / 1000.0, "maxMicros": p.stats.max_nanos as f64 / 1000.0,
            "lastError": p.stats.last_error, "lastFaultAt": p.stats.last_fault_unix,
            "http": {"requests": p.stats.http_requests, "answered": p.stats.http_answered, "failed": p.stats.http_failed, "bytesOut": p.stats.http_bytes_out, "bytesIn": p.stats.http_bytes_in, "inFlight": p.net.as_ref().map_or(0, |n| n.in_flight.load(std::sync::atomic::Ordering::SeqCst))},
        },
        "digest": p.digest,
        "signature": {
            "state": p.signed.state(),
            "keyId": p.signed.key_id(),
            "keyName": p.signed.trusted().map(|k| k.name.as_str()),
            // What the key vouches for out of what this plugin asks: no approval is needed for these.
            "vouches": p.signed.trusted().map(|k| p.requested.iter().filter(|c| k.grant.contains(c)).cloned().collect::<Vec<_>>()),
            "detail": match &p.signed { super::sign::Signed::Broken(why) => Some(why.as_str()), _ => None },
        },
        "net": m.and_then(|m| m.net.as_ref()).map(|n| {
            json!({
                "allow": n.allow,
                "wildcards": n.allow.iter().filter(|a| a.contains("://*.")).count(),
                "local": p.requested.iter().any(|c| c == caps::NET_HTTP_LOCAL),
                "localGranted": p.grant.has(caps::NET_HTTP_LOCAL),
                "timeoutMs": n.timeout_ms.unwrap_or(super::limits::HTTP_TIMEOUT_MS),
                "maxResponseBytes": n.max_response_bytes.unwrap_or(super::limits::HTTP_RESPONSE_BYTES),
            })
        }),
        // The keys the manifest declares and whether the admin set them; a secret's value is never here.
        "config": m.map(|m| m.config.iter().map(|k| json!({"key": k.key, "description": k.description, "secret": k.secret, "set": p.config.contains_key(&k.key), "value": (!k.secret).then(|| p.config.get(&k.key)).flatten()})).collect::<Vec<_>>()),
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
            let keys = dir.as_ref().and_then(|d| super::sign::Keys::load(d).ok());
            let plugins: Vec<Value> = host.map(|h| h.plugins.iter().map(plugin_json).collect()).unwrap_or_default();
            let body = json!({
                "plugins": plugins,
                "folder": dir.as_ref().map(|d| d.display().to_string()),
                "approvalsError": host.and_then(|h| h.approvals_error.clone()),
                "trustedKeys": keys.as_ref().map(|k| &k.keys),
                "revokedKeys": keys.as_ref().map(|k| &k.revoked),
                "keysError": host.and_then(|h| h.keys_error.clone()),
                "capabilities": caps::ALL.iter().map(|(n, d)| json!({"name": n, "allows": d})).collect::<Vec<_>>(),
                "abi": {"major": pcds_plugin_api::ABI_MAJOR, "minor": pcds_plugin_api::ABI_MINOR},
                "stream": {"epoch": game.ext.plugins.saved.stream.epoch, "consumers": game.ext.plugins.saved.stream.consumers_json()},
                "scanEverySeconds": super::POLL_EVERY.as_secs(),
                "runtime": host.map(|h| h.gauge.live()).map(|(instances, pages)| json!({"instances": instances, "memoryPages": pages})).unwrap_or_else(|| json!({"instances": 0, "memoryPages": 0})),
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
            if let Err(e) = super::approve_digest(&dir, &id, grant, body["digest"].as_str()) {
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
        ("POST", "/api/plugins/config") => {
            let Some(dir) = dir else { return error(500, "the plugin host has not started") };
            let (Some(id), Some(key)) = (body["id"].as_str(), body["key"].as_str()) else { return error(400, "'id' and 'key' are required") };
            let declared = game.ext.plugins.host.as_ref().and_then(|h| h.plugins.iter().find(|p| p.id == id)).and_then(|p| p.manifest.as_ref()).is_some_and(|m| m.config.iter().any(|k| k.key == key));
            if !declared {
                return error(400, format!("{id} is not an installed plugin that declares a config key {key}"));
            }
            let value = body["value"].as_str().filter(|v| !v.is_empty());
            match super::config::set(&dir, id, key, value) {
                Ok(()) => reload_answer(game, id),
                Err(e) => error(400, e),
            }
        }
        ("GET", "/api/plugins/tokens") => {
            let Some(dir) = dir else { return error(500, "the plugin host has not started") };
            match crate::tokens::Tokens::load(&dir) {
                Ok(t) => (200, json!({"tokens": t.tokens.iter().map(|t| t.public()).collect::<Vec<_>>(), "scopes": crate::tokens::SCOPES.iter().map(|(n, d)| json!({"name": n, "allows": d})).collect::<Vec<_>>(), "defaultRatePerMinute": crate::tokens::DEFAULT_RATE, "maxRatePerMinute": crate::tokens::MAX_RATE}).to_string()),
                Err(e) => error(500, e),
            }
        }
        ("POST", "/api/plugins/tokens") => {
            let Some(dir) = dir else { return error(500, "the plugin host has not started") };
            let scopes: Vec<String> = body["scopes"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            let rate = body["ratePerMinute"].as_u64().map(|r| r.min(u32::MAX as u64) as u32);
            match crate::tokens::create(&dir, body["name"].as_str().unwrap_or(""), &scopes, rate) {
                // The one time the token is shown: it is kept nowhere (the file holds its hash).
                Ok((made, token)) => {
                    println!("plugins: scoped token {} ({}) made: {}", made.name, made.id, made.scopes.join(" "));
                    (200, json!({"ok": true, "token": token, "info": made.public()}).to_string())
                }
                Err(e) => error(400, e),
            }
        }
        ("POST", "/api/plugins/tokens/revoke") => {
            let Some(dir) = dir else { return error(500, "the plugin host has not started") };
            let Some(id) = body["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) else { return error(400, "'id' is required (the id or the name)") };
            match crate::tokens::revoke(&dir, id) {
                Ok(gone) => {
                    println!("plugins: scoped token {} ({}) revoked", gone.name, gone.id);
                    (200, json!({"ok": true, "revoked": gone.public()}).to_string())
                }
                Err(e) => error(400, e),
            }
        }
        ("POST", "/api/plugins/trust") => {
            let Some(dir) = dir else { return error(500, "the plugin host has not started") };
            let public = match body["publicKey"].as_str().ok_or_else(|| "'publicKey' is required".to_string()).and_then(super::sign::parse_public) {
                Ok(p) => p,
                Err(e) => return error(400, e),
            };
            let grant: Vec<String> = body["grant"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            let result = super::sign::Keys::load(&dir).and_then(|mut keys| {
                let key = keys.trust(&public, body["name"].as_str().unwrap_or(""), grant, body["force"] == true)?;
                keys.save(&dir)?;
                Ok(key)
            });
            match result {
                Ok(key) => match super::reload(game) {
                    Ok(_) => (200, json!({"ok": true, "key": key, "warning": super::sign::published(&key.id).then_some(super::sign::PUBLISHED_WARNING)}).to_string()),
                    Err(e) => error(500, e),
                },
                Err(e) => error(400, e),
            }
        }
        ("POST", "/api/plugins/untrust") => {
            let Some(dir) = dir else { return error(500, "the plugin host has not started") };
            let Some(id) = body["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) else { return error(400, "'id' is required") };
            let result = super::sign::Keys::load(&dir).and_then(|mut keys| {
                let id = keys.find(id)?.id.clone();
                let key = keys.revoke(&id).ok_or_else(|| format!("no trusted key {id}"))?;
                keys.save(&dir)?;
                Ok(key)
            });
            match result {
                Ok(key) => match super::reload(game) {
                    Ok(_) => (200, json!({"ok": true, "revoked": key}).to_string()),
                    Err(e) => error(500, e),
                },
                Err(e) => error(400, e),
            }
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
    let name = body["consumer"].as_str().unwrap_or("anonymous");
    if name.is_empty() || name.len() > 32 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return error(400, "consumer: 1 to 32 letters, digits, . _ or -");
    }
    let ring = &mut game.ext.plugins.saved.stream;
    if body["unregister"] == true {
        let was = ring.unregister(name);
        game.ext.plugins.refresh();
        return (200, json!({"ok": true, "registered": false, "was": was}).to_string());
    }
    let after = body["after"].as_u64().unwrap_or(0);
    let limit = body["limit"].as_u64().unwrap_or(200).clamp(1, 500) as usize;
    if !ring.ask(name, kinds, after) {
        return error(429, format!("{} consumers are registered already: poll under one of their names, or unregister one", stream::MAX_CONSUMERS));
    }
    let answer = ring.read(after, kinds, limit);
    game.ext.plugins.refresh();
    (200, answer.to_string())
}

