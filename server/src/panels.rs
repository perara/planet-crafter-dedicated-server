//! Building panels (walls and floors of habitats): PanelsResources maps a panel's sub type to
//! the group whose recipe it costs.

use crate::game::Game;

/// DataConfig.BuildPanelSubType.
const SUB_TYPES: [&str; 17] = ["WallNull", "WallPlain", "WallCorridor", "WallGlass", "WallDoor", "FloorLight", "FloorGlass", "FloorPlain", "FloorLab", "WallLab", "FloorNoLight", "WallInside", "None", "WallWaterLife", "FloorAnglePlain", "FloorAngleGlass", "FloorAngleLight"];

/// PanelsResources.GetPanelGroupConstructible.
pub fn panel_group(game: &Game, sub_type: i32) -> Option<usize> {
    let name = SUB_TYPES.get(sub_type as usize)?;
    let r = &game.data.singletons["PanelsResources"];
    let i = r["panelsSubtypes"].as_array()?.iter().position(|s| s.as_str() == Some(name))?;
    game.data.group_of_ref(r["panelsGroupItems"].get(i)?.as_str()?)
}
