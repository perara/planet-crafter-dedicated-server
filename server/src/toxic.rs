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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::net::Target;
    use crate::testkit::*;

    /// No toxic areas on the planets the server offers: the progress is all zeros (as the game
    /// answers on Prime), registrations have nothing to report, and nobody is in an area.
    #[test]
    fn planets_without_toxic_areas_answer_zero_progress() {
        let Some((mut g, _)) = world() else { return };
        send_scene(&mut g, 1, "ToxicAreaHandler.GetCurrentPlanetToxicProgressServerRpc", &[]);
        assert_eq!(sent(&g, "ToxicAreaHandler.GetCurrentPlanetToxicProgressClientRpc").into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::Only(1), vec![json!(0), json!(0), json!(0), json!(0)])]);
        g.net.out.clear();
        for name in ["ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc", "ToxicAreaHandler.UnregisterToAreaToxicProgressServerRpc", "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc", "ToxicAreaHandler.UnregisterToAreaToxicFinishedServerRpc"] {
            send_scene(&mut g, 1, name, &[json!(5)]);
        }
        assert!(g.net.out.is_empty());
        assert_eq!(super::tracked_area_progress(&g, 1), None);
    }
}
