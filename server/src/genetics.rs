//! SpaceCraft.GeneticTraitHandler: genetic traits are world objects of one group whose trait
//! type, value and color travel with them.

use serde_json::Value;

use crate::game::Game;
use crate::model::WorldObject;

pub const TRAIT_GROUP: &str = "GeneticTrait";

/// DataConfig.GeneticTraitType.
const TRAIT_TYPES: [&str; 9] = ["Null", "Species", "ColorA", "ColorB", "PatternColor", "Pattern", "Variant", "Bioluminescence", "Size"];

pub fn trait_type(name: &str) -> i32 {
    TRAIT_TYPES.iter().position(|t| *t == name).unwrap_or(0) as i32
}

fn is_color(trait_type: i32) -> bool {
    TRAIT_TYPES.get(trait_type as usize).is_some_and(|t| t.contains("Color"))
}

/// The value a client sees: color traits pack the color as RGBA bytes.
pub fn trait_value(_game: &Game, w: &WorldObject) -> i32 {
    if is_color(w.trait_type) {
        let b = w.color.map(|c| (c * 255.0).round().clamp(0.0, 255.0) as u8);
        i32::from_le_bytes(b)
    } else {
        w.trait_value
    }
}

/// GetRandomGeneticTraitWithRarity, applied to a new trait object.
pub fn randomize_trait(game: &mut Game, wo: &mut WorldObject) {
    let lootable: Vec<Value> = game.data.assets.iter().filter(|(k, v)| k.starts_with("GeneticTraitData:") && v["canBeLooted"].as_bool() == Some(true)).map(|(_, v)| (**v).clone()).collect();
    if lootable.is_empty() {
        return;
    }
    for _ in 0..1000 {
        let t = &lootable[game.rng.range(0, lootable.len() as i32) as usize];
        if game.rng.range(0, 100) as i64 >= t["lootChance"].as_i64().unwrap_or(0) {
            set_trait(wo, t);
            return;
        }
    }
}

/// SetWorldObjectValuesForAGivenGeneticTrait from a GeneticTraitData asset.
pub fn set_trait(wo: &mut WorldObject, t: &Value) {
    wo.trait_type = trait_type(t["traitType"].as_str().unwrap_or("Null"));
    let c = &t["traitColor"];
    wo.color = [0, 1, 2, 3].map(|i| c[i].as_f64().unwrap_or(0.0) as f32);
    wo.trait_value = t["traitValue"].as_i64().unwrap_or(0) as i32;
}

/// SerializedData as an RPC argument: [type, [r, g, b, a], value].
pub fn set_trait_serialized(wo: &mut WorldObject, data: &Value) {
    wo.trait_type = data[0].as_i64().unwrap_or(0) as i32;
    wo.color = [0, 1, 2, 3].map(|i| data[1][i].as_f64().unwrap_or(0.0) as f32);
    wo.trait_value = data[2].as_i64().unwrap_or(0) as i32;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testkit::*;

    /// New trait objects get a random lootable trait; colour traits show their colour as RGBA
    /// bytes, the others their value; traits set from data and from RPC arguments alike.
    #[test]
    fn traits_are_random_when_new_and_show_colours_as_bytes() {
        let Some((mut g, _)) = world() else { return };
        let group = g.data.group(TRAIT_GROUP).unwrap();
        let id = g.create_world_object(group, 0, true);
        assert_ne!(g.wos[&id].trait_type, 0, "a fresh trait is a random lootable one");
        let mut w = g.wos[&id].clone();
        set_trait_serialized(&mut w, &json!([trait_type("ColorA"), [1.0, 0.5, 0.0, 1.0], 9]));
        assert_eq!(trait_value(&g, &w), i32::from_le_bytes([255, 128, 0, 255]));
        set_trait(&mut w, &json!({"traitType": "Size", "traitColor": [0, 0, 0, 0], "traitValue": 4}));
        assert_eq!((w.trait_type, trait_value(&g, &w)), (trait_type("Size"), 4));
        assert_eq!(trait_type("NoSuchTrait"), 0);
    }
}
