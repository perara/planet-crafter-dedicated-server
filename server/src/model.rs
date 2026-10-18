//! The game's world model, ported from SpaceCraft.WorldObject and SpaceCraft.Inventory, with the
//! conversions to and from the save's JsonableWorldObject and JsonableInventory.

use serde_json::{json, Value};

use crate::gamedata::{GameData, UNIT_TYPES};

/// World objects with ids below this come with a planet's scene (WorldObjectsIdHandler).
pub const FIRST_PLAYER_MADE_ID: i32 = 200_000_000;

pub fn is_from_scene(id: i32) -> bool {
    id < FIRST_PLAYER_MADE_ID
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WorldObject {
    pub id: i32,
    pub group: usize,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub color: [f32; 4],
    pub text: String,
    pub linked_groups: Option<Vec<usize>>,
    pub linked_inventory: i32,
    pub secondary_inventories: Vec<i32>,
    pub panels: Option<Vec<i32>>,
    pub growth: f32,
    pub setting: i32,
    pub trait_type: i32,
    pub trait_value: i32,
    pub hunger: f32,
    pub pet_time: f32,
    pub count: [i32; 2],
    pub linked_wo: i32,
    pub dont_save: bool,
    pub lock_time: f32,
    pub energy: f32,
    pub from_db: bool,
    pub planet: i32,
    pub planet_linked: i32,
    pub unit_generation: [f32; 8],
    pub unit_multiplier: [f32; 8],
    /// The network object standing for this world object in the scene (the game's GameObject).
    pub net: Option<u64>,
}

impl WorldObject {
    pub fn new(id: i32, group: usize, data: &GameData) -> WorldObject {
        let g = &data.groups[group];
        WorldObject {
            id,
            group,
            position: [0.0; 3],
            rotation: [0.0; 4],
            color: [0.0; 4],
            text: String::new(),
            linked_groups: None,
            linked_inventory: 0,
            secondary_inventories: Vec::new(),
            panels: None,
            growth: 0.0,
            setting: 0,
            trait_type: 0,
            trait_value: 0,
            hunger: 0.0,
            pet_time: 0.0,
            count: [0, 0],
            linked_wo: 0,
            dont_save: false,
            lock_time: 0.0,
            energy: 1.0,
            from_db: false,
            planet: 0,
            planet_linked: 0,
            unit_generation: if g.constructible { g.units } else { [0.0; 8] },
            unit_multiplier: [0.0; 8],
            net: None,
        }
    }

    pub fn is_placed(&self) -> bool {
        self.position.iter().map(|v| v * v).sum::<f32>() > 0.0
    }

    /// GetPlanetHash: only placed objects belong to a planet.
    pub fn planet_hash(&self) -> i32 {
        if self.is_placed() { self.planet } else { 0 }
    }

    pub fn reset_position(&mut self) {
        self.position = [0.0; 3];
        self.rotation = [0.0; 4];
    }

    pub fn is_locked_in_inventory(&self, time: f32) -> bool {
        time > self.lock_time && self.lock_time > 0.0
    }

    pub fn unit_generation(&self, unit: usize) -> f32 {
        let m = self.unit_multiplier[unit];
        self.unit_generation[unit] * self.energy * if m < f32::EPSILON { 1.0 } else { m }
    }

    pub fn has_unit_generation(&self, data: &GameData) -> bool {
        data.groups[self.group].constructible && self.unit_generation.iter().any(|v| *v > 0.0)
    }

    pub fn from_json(o: &Value, data: &GameData, default_planet: i32) -> Option<WorldObject> {
        let group = data.group(o["gId"].as_str()?)?;
        let mut wo = WorldObject::new(o["id"].as_i64()? as i32, group, data);
        let int = |k: &str| o[k].as_i64().unwrap_or(0) as i32;
        wo.position = parse_floats(o["pos"].as_str().unwrap_or("")).unwrap_or([0.0; 3]);
        wo.rotation = parse_floats(o["rot"].as_str().unwrap_or("")).unwrap_or([0.0; 4]);
        wo.planet = if wo.is_placed() && int("planet") == 0 { default_planet } else { int("planet") };
        wo.planet_linked = int("liPlanet");
        wo.color = parse_color(o["color"].as_str().unwrap_or(""));
        wo.text = o["text"].as_str().unwrap_or("").to_string();
        wo.linked_groups = o["liGrps"].as_str().map(|s| groups_of(s, data));
        wo.linked_inventory = int("liId");
        wo.secondary_inventories = int_list(o["siIds"].as_str()).unwrap_or_default();
        wo.panels = int_list(o["pnls"].as_str());
        wo.growth = int("grwth") as f32;
        wo.setting = int("set");
        wo.trait_type = int("trtInd");
        wo.trait_value = int("trtVal");
        wo.hunger = o["hunger"].as_f64().unwrap_or(0.0) as f32;
        wo.count = parse_ints(o["count"].as_str().unwrap_or("")).unwrap_or([0, 0]);
        wo.linked_wo = int("linkedWo");
        Some(wo)
    }

    /// JsonableWorldObject, every field in declaration order (the writer drops defaults).
    pub fn to_json(&self, data: &GameData) -> Value {
        json!({
            "id": self.id,
            "gId": data.groups[self.group].id,
            "liId": self.linked_inventory,
            "siIds": int_list_string(&self.secondary_inventories),
            "liGrps": self.linked_groups.as_deref().map(|g| group_ids(g, data)).unwrap_or_default(),
            "pos": floats_string(&self.position),
            "rot": floats_string(&self.rotation),
            "planet": self.planet_hash(),
            "liPlanet": self.planet_linked,
            "pnls": self.panels.as_deref().map(int_list_string).unwrap_or_default(),
            "color": color_string(self.color),
            "text": self.text,
            "grwth": self.growth.round() as i32,
            "set": self.setting,
            "trtInd": self.trait_type,
            "trtVal": self.trait_value,
            "hunger": JsonFloat(self.hunger),
            "count": format!("{},{}", self.count[0], self.count[1]),
            "linkedWo": self.linked_wo,
        })
    }
}

/// Inventory.LogisticEntity: what the inventory offers and asks drones for.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Logistic {
    pub supply: Vec<usize>,
    pub demand: Vec<usize>,
    pub priority: i32,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Inventory {
    pub id: i32,
    pub size: i32,
    pub items: Vec<i32>,
    pub authorized: Vec<usize>,
    pub unauthorized: Vec<usize>,
    pub logistic: Logistic,
    pub is_dna: bool,
    pub is_equipment: bool,
}

impl Inventory {
    pub fn new(id: i32, size: i32) -> Inventory {
        Inventory { id, size, items: Vec::new(), authorized: Vec::new(), unauthorized: Vec::new(), logistic: Logistic::default(), is_dna: false, is_equipment: false }
    }

    pub fn is_full(&self) -> bool {
        self.items.len() as i32 >= self.size
    }

    pub fn from_json(o: &Value, data: &GameData, exists: impl Fn(i32) -> bool) -> Option<Inventory> {
        let mut inv = Inventory::new(o["id"].as_i64()? as i32, o["size"].as_i64().unwrap_or(0) as i32);
        inv.items = o["woIds"].as_str().unwrap_or("").split(',').filter_map(|s| s.parse().ok()).filter(|id| exists(*id)).collect();
        inv.logistic.supply = groups_of(o["supplyGrps"].as_str().unwrap_or(""), data);
        inv.logistic.demand = groups_of(o["demandGrps"].as_str().unwrap_or(""), data);
        inv.logistic.priority = o["priority"].as_i64().unwrap_or(0) as i32;
        Some(inv)
    }

    pub fn to_json(&self, data: &GameData) -> Value {
        json!({
            "id": self.id,
            "woIds": int_list_string(&self.items),
            "size": self.size,
            "demandGrps": group_ids(&self.logistic.demand, data),
            "supplyGrps": group_ids(&self.logistic.supply, data),
            "priority": self.logistic.priority,
        })
    }
}

/// A float written the way Unity's JsonUtility does (always with a decimal point).
pub struct JsonFloat(pub f32);

impl serde::Serialize for JsonFloat {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(self.0 as f64)
    }
}

/// float.ToString(InvariantCulture) on Mono: at most 7 significant digits, scientific notation
/// for exponents below -5 or from 7 on ("G7").
pub fn g7(v: f32) -> String {
    if v == 0.0 {
        return "0".into();
    }
    if !v.is_finite() {
        return if v.is_nan() { "NaN".into() } else if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let sci = format!("{:.6e}", v);
    let (mantissa, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if !(-5..7).contains(&exp) {
        let m = mantissa.trim_end_matches('0').trim_end_matches('.');
        return format!("{m}E{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    let decimals = (6 - exp).max(0) as usize;
    let s = format!("{:.*}", decimals, v);
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

pub fn floats_string(values: &[f32]) -> String {
    values.iter().map(|v| g7(*v)).collect::<Vec<_>>().join(",")
}

pub fn parse_floats<const N: usize>(s: &str) -> Option<[f32; N]> {
    let parts: Vec<f32> = s.split(',').map(|p| p.trim().parse().ok()).collect::<Option<Vec<_>>>()?;
    parts.try_into().ok()
}

fn parse_ints<const N: usize>(s: &str) -> Option<[i32; N]> {
    let parts: Vec<i32> = s.split(',').map(|p| p.trim().parse().ok()).collect::<Option<Vec<_>>>()?;
    parts.try_into().ok()
}

fn int_list(s: Option<&str>) -> Option<Vec<i32>> {
    Some(s?.split(',').filter_map(|p| p.parse().ok()).collect())
}

pub fn int_list_string(values: &[i32]) -> String {
    values.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")
}

/// DataTreatments.StringToColor: "r-g-b-a"; components above 1 are scaled down by tens.
fn parse_color(s: &str) -> [f32; 4] {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 4 {
        return [0.0; 4];
    }
    let mut c = [0.0; 4];
    for (i, p) in parts.iter().enumerate() {
        let mut v: f32 = p.replace(',', ".").parse().unwrap_or(0.0);
        for _ in 0..50 {
            if v <= 1.0 {
                break;
            }
            v /= 10.0;
        }
        c[i] = v;
    }
    c
}

pub fn color_string(c: [f32; 4]) -> String {
    if c == [0.0; 4] { String::new() } else { c.iter().map(|v| g7(*v)).collect::<Vec<_>>().join("-") }
}

/// GroupsHandler.GetGroupsViaString: comma-separated group ids.
pub fn groups_of(s: &str, data: &GameData) -> Vec<usize> {
    s.split(',').filter(|p| !p.is_empty()).filter_map(|p| data.group(p)).collect()
}

pub fn group_ids(groups: &[usize], data: &GameData) -> String {
    groups.iter().map(|g| data.groups[*g].id.as_str()).collect::<Vec<_>>().join(",")
}

pub fn unit_index(unit: &str) -> Option<usize> {
    UNIT_TYPES.iter().position(|u| *u == unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g7_matches_mono_float_formatting() {
        assert_eq!(g7(558.2), "558.2");
        assert_eq!(g7(2.011989), "2.011989");
        assert_eq!(g7(0.015173881), "0.01517388");
        assert_eq!(g7(-0.04237339), "-0.04237339");
        assert_eq!(g7(1.0), "1");
        assert_eq!(g7(1234567.0), "1234567");
        assert_eq!(g7(12345678.0), "1.234568E+07");
        assert_eq!(g7(0.0000012), "1.2E-06");
        assert_eq!(g7(0.00012), "0.00012");
    }
}
