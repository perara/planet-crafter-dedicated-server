//! The game as a plugin sees it: a read-only view built when it asks (`world.read`).

use pcds_plugin_api::query::{InventoryInfo, ItemCount, Nearby, ObjectInfo, ObjectRef, PlayerInfo, WorldInfo};

use super::ctx::World;
use crate::game::Game;

impl World for Game {
    fn info(&self) -> WorldInfo {
        WorldInfo {
            game_ms: (self.time.secs() * 1000.0) as i64,
            planet: self.planet_id().to_string(),
            players: self.clients.values().filter(|c| c.ready.is_some()).count() as u32,
            world_objects: self.wos.len() as u32,
            network_objects: self.net.objects.len() as u32,
            terraformation: crate::units::value(self, crate::units::Kind::Terraformation, 0),
            standing_still: self.held || (self.rules.world.pause_when_empty && !self.clients.values().any(|c| c.ready.is_some())),
            unix_ms: super::stream::unix_ms() as i64,
            command_prefix: self.rules.chat.command_prefix.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| "!".into()),
        }
    }

    fn players(&self) -> Vec<PlayerInfo> {
        self.clients.values().filter_map(|c| self.player_info(c)).collect()
    }

    fn player(&self, client: u64) -> Option<PlayerInfo> {
        self.clients.get(&client).and_then(|c| self.player_info(c))
    }

    fn rules(&self) -> String {
        changes_json(&self.rules)
    }

    fn inventory(&self, client: u64) -> Option<InventoryInfo> {
        let state = self.clients.get(&client)?.ready.as_ref()?;
        let inventory = self.inventories.get(&state.inventory)?;
        let mut counts: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
        for wo in inventory.items.iter().filter_map(|i| self.wos.get(i)) {
            *counts.entry(self.data.groups[wo.group].id.as_str()).or_default() += 1;
        }
        Some(InventoryInfo { slots_used: inventory.items.len() as u32, slots_total: inventory.size.max(0) as u32, items: counts.into_iter().map(|(group, count)| ItemCount { group: group.to_string(), count }).collect() })
    }

    fn nearby(&self, center: [f32; 3], radius: f32, max: usize) -> (Vec<Nearby>, usize) {
        let planet = self.planet_hash();
        let limit = radius * radius;
        let mut found: Vec<Nearby> = Vec::new();
        for wo in self.wos.values() {
            // Only what stands in the world: held items have no place of their own.
            if wo.net.is_none() || wo.planet_hash() != planet {
                continue;
            }
            let d2: f32 = (0..3).map(|i| (wo.position[i] - center[i]).powi(2)).sum();
            if d2 <= limit {
                found.push(Nearby { object: self.object_info(wo), distance: d2.sqrt() });
            }
        }
        found.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.object.id.cmp(&b.object.id)));
        found.truncate(max);
        (found, self.wos.len())
    }

    fn object(&self, which: ObjectRef) -> Option<ObjectInfo> {
        let id = match which {
            ObjectRef::World(id) => id,
            ObjectRef::Net(net) => self.net.objects.get(&net)?.wo?,
        };
        self.wos.get(&id).map(|wo| self.object_info(wo))
    }
}

impl Game {
    fn player_info(&self, c: &crate::game::Client) -> Option<PlayerInfo> {
        let state = c.ready.as_ref()?;
        let object = c.player.and_then(|o| self.net.objects.get(&o));
        Some(PlayerInfo {
            client: c.id,
            name: state.name.clone(),
            steam_id: c.verified_steam_id.map(|i| i.to_string()).unwrap_or_default(),
            position: object.map(|o| o.position).unwrap_or(state.position),
            gauges: c.player.map(|o| crate::gauges::values(self, o)).unwrap_or(state.gauges),
            player_id: state.id,
            operator: self.access.is_operator(c.verified_steam_id),
        })
    }

    fn object_info(&self, wo: &crate::model::WorldObject) -> ObjectInfo {
        let position = wo.net.and_then(|n| self.net.objects.get(&n)).map_or(wo.position, |o| o.position);
        ObjectInfo { id: wo.id, net: wo.net.unwrap_or(0), group: self.data.groups[wo.group].id.clone(), position }
    }
}

/// Whether the client is one of the operators the admin listed (`access.operators`).
pub fn is_operator(game: &Game, client: u64) -> bool {
    game.clients.get(&client).is_some_and(|c| game.access.is_operator(c.verified_steam_id))
}

/// The rules that differ from the defaults as one JSON object of dotted names ("speed.growth": 2),
/// without the sections a plugin has no business reading: the chat, backup and access rules hold
/// webhook URLs, keys and ban lists.
pub fn changes_json(rules: &crate::rules::Rules) -> String {
    let mut changes = serde_json::Map::new();
    for line in crate::rules::changes(rules) {
        let Some((key, value)) = line.split_once('=') else { continue };
        if super::actions::PLUGIN_RULES.contains(&key.split('.').next().unwrap_or("")) {
            changes.insert(key.to_string(), serde_json::from_str(value).unwrap_or_else(|_| value.into()));
        }
    }
    serde_json::Value::Object(changes).to_string()
}
