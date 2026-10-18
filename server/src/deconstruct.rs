//! GroupNetworkBase.DeconstructServerRpc and ActionDeconstructible.CheckDeconstructOnServer:
//! taking a building apart gives its recipe back after the deconstruction animation.

use serde_json::json;

use crate::game::Game;
use crate::net::Target;

/// DeconstructStatus.
const OK: i32 = 0;

pub fn deconstruct(game: &mut Game, client: u64, object: u64, time: f32) {
    let Some(wo) = game.net.objects.get(&object).and_then(|o| o.wo) else { return };
    if game.ext.deconstructing.contains(&wo) || !crate::machines::deconstructible(game, wo) {
        return;
    }
    let status = check(game, wo);
    if status != OK {
        game.net.rpc(Target::Only(client), object, "GroupNetworkBase.DeconstructErrorClientRpc", &[json!(status)]);
        return;
    }
    game.ext.deconstructing.insert(wo);
    game.net.rpc(Target::All, object, "GroupNetworkBase.DeconstructClientRpc", &[json!(time)]);
    game.net.rpc(Target::Only(client), object, "GroupNetworkBase.StartDeconstructClientRpc", &[]);
    crate::panels::reset_contiguous_panels(game, object);
    crate::scene::schedule(game, time, crate::scene::Timer::FinalDeconstruct(wo, client));
}

/// ActionDeconstructible.CheckDeconstructOnServer.
fn check(game: &Game, wo: i32) -> i32 {
    crate::machines::deconstruct_status(game, wo)
}

/// FinalDeconstructServer.
pub fn finish(game: &mut Game, wo: i32, client: u64) {
    game.ext.deconstructing.remove(&wo);
    let Some(w) = game.wos.get(&wo) else { return };
    let (hash, position) = (game.data.groups[w.group].hash, w.position);
    if crate::model::is_from_scene(wo) {
        game.wos.get_mut(&wo).unwrap().dont_save = false;
    }
    let backpack = game.clients.get(&client).and_then(|c| c.ready.as_ref()).map(|s| s.inventory).unwrap_or(-1);
    let position = game.wos[&wo].net.and_then(|n| game.net.objects.get(&n)).map(|o| o.position).unwrap_or(position);
    game.retrieve_resources_from_deconstruction(wo, hash, backpack, position, client);
    game.destroy_world_object(wo, true);
}
