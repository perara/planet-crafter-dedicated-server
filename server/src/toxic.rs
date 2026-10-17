//! SpaceCraft.ToxicAreaHandler, server side. Toxic areas (ToxicArea: scene objects cleaned up
//! by picking up their toxic world objects) exist only on Toxicity, a DLC planet the standalone
//! server never offers: every planet here has none. So the progress query answers zeros, as
//! the real game does on Prime, and area registrations have nothing to report.

use serde_json::json;

use crate::game::{Args, Game};
use crate::net::Target;

/// (progress, toxic time) of the toxic area a client tracks (GetTrackedAreaByClient).
pub fn tracked_area_progress(_game: &Game, _client: u64) -> Option<(f32, f32)> {
    None
}

pub fn rpc(game: &mut Game, client: u64, name: &str, _a: &Args) -> bool {
    match name {
        "ToxicAreaHandler.GetCurrentPlanetToxicProgressServerRpc" => {
            game.net.rpc_scene(Target::Only(client), "ToxicAreaHandler.GetCurrentPlanetToxicProgressClientRpc", &[json!(0), json!(0), json!(0), json!(0)]);
        }
        "ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc"
        | "ToxicAreaHandler.UnregisterToAreaToxicProgressServerRpc"
        | "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc"
        | "ToxicAreaHandler.UnregisterToAreaToxicFinishedServerRpc" => {}
        _ => return false,
    }
    true
}
