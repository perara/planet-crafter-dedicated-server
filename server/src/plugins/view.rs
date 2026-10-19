//! The game as a plugin sees it: a read-only view built when it asks (`world.read`).

use pcds_plugin_api::query::{PlayerInfo, WorldInfo};

use super::ctx::World;
use crate::game::Game;

impl World for Game {
    fn info(&self) -> WorldInfo {
        WorldInfo {
            game_ms: (self.time as f64 * 1000.0) as i64,
            planet: self.planet_id().to_string(),
            players: self.clients.values().filter(|c| c.ready.is_some()).count() as u32,
            world_objects: self.wos.len() as u32,
            network_objects: self.net.objects.len() as u32,
            terraformation: crate::units::value(self, crate::units::Kind::Terraformation, 0),
            standing_still: self.held || (self.rules.world.pause_when_empty && !self.clients.values().any(|c| c.ready.is_some())),
        }
    }

    fn players(&self) -> Vec<PlayerInfo> {
        self.clients
            .values()
            .filter_map(|c| {
                let state = c.ready.as_ref()?;
                let object = c.player.and_then(|o| self.net.objects.get(&o));
                Some(PlayerInfo {
                    client: c.id,
                    name: state.name.clone(),
                    steam_id: c.verified_steam_id.map(|i| i.to_string()).unwrap_or_default(),
                    position: object.map(|o| o.position).unwrap_or(state.position),
                    gauges: c.player.map(|o| crate::gauges::values(self, o)).unwrap_or(state.gauges),
                })
            })
            .collect()
    }

    fn rules(&self) -> String {
        changes_json(&self.rules)
    }
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
