//! Experimental moving-container rocket visual; local rocket physics stays disabled.
use crate::game::Game;
use crate::net::{NetObj, Target, enc};
use crate::protocol::NETWORK_TRANSFORM;
use crate::wire::Writer;
use serde_json::json;

// Position axes, interpolation and reliable sequencing, as the drone's NetworkTransform uses.
const MOVING: u32 = 2 | 4 | 8 | (1 << 11) | (1 << 19);

pub(super) fn prepare(game: &Game, rocket: i32) -> Result<NetObj, &'static str> {
    let def = game.data.layouts.get("Iron").filter(|d| d.prefab == "GroupNetworkContainer")
        .ok_or("the game tables have no movable GroupNetworkContainer")?;
    let mut proxy = NetObj::from_layout(&def.layout(game.tick_number()));
    let behaviour = proxy.behaviours().find(|b| b.class == "ClientNetworkTransform").map(|b| b.index)
        .ok_or("the moving container has no ClientNetworkTransform")?;
    let wo = &game.wos[&rocket];
    let original = wo.net.and_then(|id| game.net.objects.get(&id)).ok_or("the rocket has no network object")?;
    proxy.position = original.position;
    proxy.rotation = original.rotation;
    proxy.scale = [1.0; 3];
    proxy.wo = Some(rocket);
    proxy.init("GroupNetworkContainer", "_groupHash", enc::int(game.data.groups[wo.group].hash));
    proxy.init("GroupNetworkContainer", "_active", enc::bool(wo.planet_hash() == game.planet_hash()));
    proxy.init("GroupNetworkContainer", "_scale", enc::float(1.0));
    proxy.init("GroupNetworkContainer", "_transformSync", enc::bool(true));
    proxy.tail = sync(behaviour, game.tick_number(), proxy.position).into();
    proxy.tail_position = None;
    Ok(proxy)
}

fn sync(behaviour: u16, tick: i32, position: [f32; 3]) -> Vec<u8> {
    let mut body = Writer::new();
    body.packed_u32(MOVING | (1 << 10) | (1 << 15)).packed_i32(tick).vec3(position);
    let mut tail = Writer::new();
    tail.u8(1).u16(behaviour).i32(body.0.len() as i32).bytes(&body.0);
    tail.0
}

/// A temporary signal flare and the pad's existing craft cue; neither creates a world item.
pub(super) fn launch_effect(game: &mut Game, platform: u64, position: [f32; 3]) {
    if let (Some(group), Some(def)) = (game.data.group("Flare"), game.data.layouts.get("Iron")) {
        let mut flare = NetObj::from_layout(&def.layout(game.tick_number()));
        flare.position = position;
        flare.rotation = [0.0, 0.0, 0.0, 1.0];
        flare.init("GroupNetworkContainer", "_groupHash", enc::int(game.data.groups[group].hash));
        flare.init("GroupNetworkContainer", "_active", enc::bool(true));
        flare.init("GroupNetworkContainer", "_scale", enc::float(2.0));
        flare.init("SettingProxy", "_setting", enc::int(1));
        flare.init("ColorProxy", "_color", enc::color([1.0, 0.25, 0.02, 1.0]));
        let net = game.net.spawn(flare);
        game.ext.rockets.despawns.push((game.time + 6.0, 0, net));
    }
    // CraftAnimation needs a GroupItem: Iron is a small, temporary hologram at the pad.
    if let Some(group) = game.data.group("Iron") {
        game.net.rpc(Target::All, platform, "ConstructibleProxy.StartCraftAnimationClientRpc", &[json!(game.data.groups[group].hash)]);
    }
}

pub(super) fn tick(game: &mut Game, dt: f32) {
    if game.ext.rockets.despawns.is_empty() { return; }
    let Some(hash) = game.data.layouts.get("Iron").filter(|d| d.prefab == "GroupNetworkContainer").map(|d| d.hash) else { return };
    // Existing despawn timers and network poses survive a handoff; no new snapshot field is needed.
    let flights: Vec<_> = game.ext.rockets.despawns.iter().filter(|(_, wo, net)| {
        *wo != 0 && game.net.objects.get(net).is_some_and(|o| o.hash == hash)
    }).map(|(due, wo, net)| (*due, *wo, *net)).collect();
    if flights.is_empty() { return; }
    let now = game.tick_count();
    let send = game.ext.rockets.flight_tick.replace(now) != Some(now);
    let tick = now as i32;
    for (due, wo, net) in flights {
        let elapsed = (super::ROCKET_DESPAWN_AFTER - (due - game.time)).max(0.0);
        let Some(o) = game.net.objects.get_mut(&net) else { continue };
        let Some(behaviour) = o.behaviours().find(|b| b.class == "ClientNetworkTransform").map(|b| b.index) else { continue };
        o.position[1] += (5.0 + elapsed * 4.0).min(80.0) * dt.max(0.0);
        let position = o.position;
        o.tail = sync(behaviour, tick, position).into();
        let active = game.wos.get(&wo).is_some_and(|w| w.planet == game.planet_hash());
        game.net.set(net, "GroupNetworkContainer", "_active", enc::bool(active));
        // Disconnect cleanup can turn container synchronization off; this visual stays server-owned.
        game.net.set(net, "GroupNetworkContainer", "_transformSync", enc::bool(true));
        if send && active {
            let mut body = Writer::new();
            body.packed_u64(net).packed_i32(behaviour as i32).packed_u32(MOVING).packed_i32(tick).vec3(position);
            game.out_to(Target::All, (NETWORK_TRANSFORM, body.0));
        }
    }
}
