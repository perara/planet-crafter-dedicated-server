//! The game's world model, ported from SpaceCraft.WorldObject and SpaceCraft.Inventory, with the
//! conversions to and from the save's JsonableWorldObject and JsonableInventory.

use serde_json::{json, Value};

use crate::clock::GameTime;
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
    #[serde(with = "crate::bincodec::floats")]
    pub position: [f32; 3],
    #[serde(with = "crate::bincodec::floats")]
    pub rotation: [f32; 4],
    #[serde(with = "crate::bincodec::floats")]
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
    pub pet_time: GameTime,
    pub count: [i32; 2],
    pub linked_wo: i32,
    pub dont_save: bool,
    pub lock_time: GameTime,
    pub energy: f32,
    pub from_db: bool,
    pub planet: i32,
    pub planet_linked: i32,
    #[serde(with = "crate::bincodec::floats")]
    pub unit_generation: [f32; 8],
    #[serde(with = "crate::bincodec::floats")]
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
            pet_time: GameTime::ZERO,
            count: [0, 0],
            linked_wo: 0,
            dont_save: false,
            lock_time: GameTime::ZERO,
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

    pub fn is_locked_in_inventory(&self, time: GameTime) -> bool {
        time > self.lock_time && self.lock_time > GameTime::ZERO
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
            // Mathf.RoundToInt: halves to even.
            "grwth": self.growth.round_ties_even() as i32,
            "set": self.setting,
            "trtInd": self.trait_type,
            "trtVal": self.trait_value,
            "hunger": JsonFloat(self.hunger),
            "count": format!("{},{}", self.count[0], self.count[1]),
            "linkedWo": self.linked_wo,
        })
    }

    /// `to_json` as the save's text, the fields at their default cut out (SaveFile::to_text cuts
    /// them from the JSON; this never writes them), appended to `out`: the same bytes, without
    /// the JSON tree in between.
    pub fn write_save(&self, data: &GameData, out: &mut String) {
        use std::fmt::Write;
        let _ = write!(out, "{{\"id\":{},\"gId\":", self.id);
        push_json_str(out, &data.groups[self.group].id);
        if self.linked_inventory != 0 {
            let _ = write!(out, ",\"liId\":{}", self.linked_inventory);
        }
        if !self.secondary_inventories.is_empty() {
            out.push_str(",\"siIds\":\"");
            push_ints(out, &self.secondary_inventories);
            out.push('"');
        }
        let groups = self.linked_groups.as_deref().map(|g| group_ids(g, data)).unwrap_or_default();
        if !groups.is_empty() {
            out.push_str(",\"liGrps\":");
            push_json_str(out, &groups);
        }
        if self.position.iter().any(|v| *v != 0.0) {
            out.push_str(",\"pos\":\"");
            push_floats(out, &self.position);
            out.push('"');
        }
        if self.rotation.iter().any(|v| *v != 0.0) {
            out.push_str(",\"rot\":\"");
            push_floats(out, &self.rotation);
            out.push('"');
        }
        let planet = self.planet_hash();
        if planet != 0 {
            let _ = write!(out, ",\"planet\":{planet}");
        }
        if self.planet_linked != 0 {
            let _ = write!(out, ",\"liPlanet\":{}", self.planet_linked);
        }
        if self.panels.as_deref().is_some_and(|p| !p.is_empty()) {
            out.push_str(",\"pnls\":\"");
            push_ints(out, self.panels.as_deref().unwrap_or_default());
            out.push('"');
        }
        if self.color != [0.0; 4] {
            out.push_str(",\"color\":\"");
            for (i, c) in self.color.iter().enumerate() {
                if i > 0 {
                    out.push('-');
                }
                push_g7(out, *c);
            }
            out.push('"');
        }
        if !self.text.is_empty() {
            out.push_str(",\"text\":");
            push_json_str(out, &self.text);
        }
        let growth = self.growth.round_ties_even() as i32;
        if growth != 0 {
            let _ = write!(out, ",\"grwth\":{growth}");
        }
        if self.setting != 0 {
            let _ = write!(out, ",\"set\":{}", self.setting);
        }
        if self.trait_type != 0 {
            let _ = write!(out, ",\"trtInd\":{}", self.trait_type);
        }
        if self.trait_value != 0 {
            let _ = write!(out, ",\"trtVal\":{}", self.trait_value);
        }
        // Only +0.0 is the default "0.0" (-0.0 is written "-0.0", NaN as null).
        if self.hunger.to_bits() != 0 {
            out.push_str(",\"hunger\":");
            out.push_str(&serde_json::to_string(&JsonFloat(self.hunger)).unwrap_or_default());
        }
        if self.count != [0, 0] {
            let _ = write!(out, ",\"count\":\"{},{}\"", self.count[0], self.count[1]);
        }
        if self.linked_wo != 0 {
            let _ = write!(out, ",\"linkedWo\":{}", self.linked_wo);
        }
        out.push('}');
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

    /// `to_json` as the save's text with the defaults cut (see WorldObject::write_save): the
    /// logistics fields go together, as SaveFile::to_text cuts them.
    pub fn write_save(&self, data: &GameData, out: &mut String) {
        use std::fmt::Write;
        let _ = write!(out, "{{\"id\":{},\"woIds\":\"", self.id);
        push_ints(out, &self.items);
        let _ = write!(out, "\",\"size\":{}", self.size);
        let (demand, supply) = (group_ids(&self.logistic.demand, data), group_ids(&self.logistic.supply, data));
        if !(demand.is_empty() && supply.is_empty() && self.logistic.priority == 0) {
            out.push_str(",\"demandGrps\":");
            push_json_str(out, &demand);
            out.push_str(",\"supplyGrps\":");
            push_json_str(out, &supply);
            let _ = write!(out, ",\"priority\":{}", self.logistic.priority);
        }
        out.push('}');
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
    let mut s = String::new();
    push_g7(&mut s, v);
    s
}

/// g7 appended to `out`, with no allocation of its own: the seven significant digits come from
/// one exact "{:.6e}" and are laid out by their exponent.
pub fn push_g7(out: &mut String, v: f32) {
    use std::fmt::Write;
    if v == 0.0 {
        out.push('0');
        return;
    }
    if !v.is_finite() {
        out.push_str(if v.is_nan() { "NaN" } else if v > 0.0 { "Infinity" } else { "-Infinity" });
        return;
    }
    // "-d.dddddde[-]x": at most 15 bytes.
    let mut buf = Stack::default();
    let _ = write!(buf, "{:.6e}", v);
    let text = buf.as_str();
    let (mantissa, exp) = text.split_once('e').unwrap_or((text, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (sign, mantissa) = mantissa.strip_prefix('-').map(|m| ("-", m)).unwrap_or(("", mantissa));
    // The digits without the point, trailing zeros cut (at least one digit stays).
    let mut digits = [0u8; 7];
    let mut n = 0;
    for d in mantissa.bytes().filter(|b| *b != b'.') {
        digits[n] = d;
        n += 1;
    }
    let digits = &digits[..n];
    let digits = &digits[..digits.iter().rposition(|d| *d != b'0').map_or(1, |i| i + 1)];
    out.push_str(sign);
    let digits = std::str::from_utf8(digits).unwrap_or("0");
    if !(-5..7).contains(&exp) {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "E{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    } else if exp < 0 {
        out.push_str("0.");
        (1..-exp).for_each(|_| out.push('0'));
        out.push_str(digits);
    } else {
        let whole = exp as usize + 1;
        if digits.len() <= whole {
            out.push_str(digits);
            (digits.len()..whole).for_each(|_| out.push('0'));
        } else {
            out.push_str(&digits[..whole]);
            out.push('.');
            out.push_str(&digits[whole..]);
        }
    }
}

/// A small text buffer on the stack for push_g7.
#[derive(Default)]
struct Stack {
    bytes: [u8; 24],
    len: usize,
}

impl Stack {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl std::fmt::Write for Stack {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        self.bytes.get_mut(self.len..end).ok_or(std::fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// A JSON string as serde_json writes it (only quotes, backslashes and control characters are
/// escaped).
pub fn push_json_str(out: &mut String, s: &str) {
    if s.bytes().all(|b| b >= 0x20 && b != b'"' && b != b'\\') {
        out.push('"');
        out.push_str(s);
        out.push('"');
    } else {
        out.push_str(&serde_json::to_string(s).unwrap_or_default());
    }
}

fn push_ints(out: &mut String, values: &[i32]) {
    use std::fmt::Write;
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{v}");
    }
}

/// floats_string appended to `out`.
pub fn push_floats(out: &mut String, values: &[f32]) {
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_g7(out, *v);
    }
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
        // ParseStringColor: divides once before it looks, so 10 and 100 come out 0.1.
        if v > 1.0 {
            for _ in 0..50 {
                v /= 10.0;
                if v < 1.0 {
                    break;
                }
            }
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

    /// The save keeps a growth the way Mathf.RoundToInt does: 2.5 is 2, 3.5 is 4.
    #[test]
    fn growth_is_saved_rounded_to_even_like_mathf_round_to_int() {
        let Some((mut g, _)) = crate::testkit::world() else { return };
        let group = g.data.group("Iron").unwrap();
        for (growth, saved) in [(2.5, 2), (3.5, 4), (0.5, 0), (12.4, 12), (12.6, 13)] {
            let id = g.create_world_object(group, 0, true);
            g.wos.get_mut(&id).unwrap().growth = growth;
            assert_eq!(g.wos[&id].to_json(&g.data)["grwth"], saved, "growth {growth}");
        }
    }

    /// DataTreatments.ParseStringColor: a component above 1 is divided by ten until it is below 1.
    #[test]
    fn color_components_above_one_are_scaled_down_as_the_game_does() {
        assert_eq!(parse_color("0.5-1-10-100"), [0.5, 1.0, 0.1, 0.1]);
        assert_eq!(parse_color("2-25-1.5-0"), [0.2, 0.25, 0.15, 0.0]);
    }

    /// g7 as it was written first: three allocations and a second formatting per float.
    fn g7_reference(v: f32) -> String {
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

    struct Xorshift(u64);

    impl Xorshift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    // The game's own numbers (0.7071068 is a quaternion's component, not 1/sqrt(2) by choice).
    #[allow(clippy::approx_constant)]
    fn push_g7_writes_what_g7_always_wrote() {
        let mut rng = Xorshift(0x9e37_79b9_7f4a_7c15);
        let mut values = vec![0.0, -0.0, 1.0, -1.0, 9.9999995, 0.99999999, 9999999.5, 1234567.0, 12345678.0, 0.00001, 0.000009999999, 1e-5, 1e7, 1e-6, f32::MAX, f32::MIN_POSITIVE, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 558.2, 0.7071068, 100.0, 1e6, 1e-45];
        // Every bit pattern class: random bits, and 7-digit decimals scaled over the whole range
        // (the values a save holds), where the rounding of the 7th digit decides.
        for _ in 0..400_000 {
            values.push(f32::from_bits(rng.next() as u32));
            let digits = (rng.next() % 90_000_000 + 10_000_000) as f32;
            values.push(digits * 10f32.powi((rng.next() % 24) as i32 - 18) * if rng.next() % 2 == 0 { 1.0 } else { -1.0 });
            values.push((rng.next() % 4_000_000) as f32 / 1000.0 - 2000.0);
            values.push((rng.next() % 2_000_000) as f32 / 1_000_000.0);
        }
        for v in values {
            let mut s = String::from("x");
            push_g7(&mut s, v);
            assert_eq!(&s[1..], g7_reference(v), "{v:e} {:#x}", v.to_bits());
            assert_eq!(g7(v), g7_reference(v));
        }
    }

    /// The world objects and inventories written straight to the save's text are the bytes the
    /// JSON route gives, whatever the fields hold: odd floats, texts that need escaping, every
    /// default and near miss.
    #[test]
    // The game's own numbers (0.7071068 is a quaternion's component, not 1/sqrt(2) by choice).
    #[allow(clippy::approx_constant)]
    fn save_entries_written_directly_are_the_json_routes_bytes() {
        let Some((_, data, _, _)) = crate::game::test_tables() else { return };
        let mut rng = Xorshift(0x2545_f491_4f6c_dd1d);
        let floats = [0.0f32, -0.0, 1.0, 558.2, -3.5e-9, 1.0e9, 0.07, f32::NAN, f32::INFINITY, 2.47, 0.7071068, 100.0];
        let mut objects = Vec::new();
        let mut inventories = Vec::new();
        for id in 0..30_000 {
            let mut w = WorldObject::new(id, (rng.next() % data.groups.len() as u64) as usize, &data);
            let pick = |rng: &mut Xorshift| if rng.next() % 3 == 0 { f32::from_bits(rng.next() as u32) } else { floats[(rng.next() % floats.len() as u64) as usize] };
            let r = rng.next();
            if r % 2 == 0 {
                w.position = [pick(&mut rng), pick(&mut rng), pick(&mut rng)];
            }
            if r % 3 == 0 {
                w.rotation = [pick(&mut rng), pick(&mut rng), pick(&mut rng), pick(&mut rng)];
            }
            if r % 5 == 0 {
                w.color = [pick(&mut rng), pick(&mut rng), pick(&mut rng), 1.0];
            }
            w.planet = if r % 7 == 0 { -1140328421 } else { 0 };
            w.planet_linked = (r % 11) as i32 % 2;
            w.linked_inventory = if r % 13 < 5 { 0 } else { (r % 1000) as i32 };
            w.secondary_inventories = (0..r % 3).map(|i| i as i32 + 7).collect();
            w.linked_groups = match r % 4 {
                0 => None,
                1 => Some(Vec::new()),
                _ => Some((0..(r >> 8) % 4).map(|i| i as usize).collect()),
            };
            w.panels = match r % 5 {
                0 => None,
                1 => Some(Vec::new()),
                _ => Some(vec![1, 2, 5]),
            };
            w.text = ["", "Iron", "a \"quoted\" \\ text", "line\nbreak\ttab\u{1}", ",\"set\":0", "\u{e5}\u{4e16}\u{1f30d}", "\u{7f}"][(r >> 16) as usize % 7].to_string();
            w.growth = [0.0, 0.4, 0.5, 99.6, -2.5, f32::NAN][(r >> 20) as usize % 6];
            w.setting = (r >> 24) as i32 % 3;
            w.trait_type = (r >> 26) as i32 % 3;
            w.trait_value = (r >> 28) as i32 % 3 * 5;
            w.hunger = [0.0, -0.0, 0.07, 5.0, f32::NAN, 1e-9][(r >> 30) as usize % 6];
            w.count = [[0, 0], [0, 1], [3, 0], [2, 2]][(r >> 33) as usize % 4];
            w.linked_wo = (r >> 35) as i32 % 2 * 77;
            w.dont_save = false;
            objects.push(w);
            let mut inv = Inventory::new(id, 12);
            inv.items = (0..r % 4).map(|i| i as i32 * 3).collect();
            inv.logistic.priority = (r >> 40) as i32 % 3;
            inv.logistic.supply = if r % 3 == 0 { vec![] } else { vec![1, 2] };
            inv.logistic.demand = if r % 5 == 0 { vec![] } else { vec![3] };
            inventories.push(inv);
        }
        let mut json = crate::save::SaveFile::empty();
        json.objects = objects.iter().map(|w| w.to_json(&data)).collect();
        json.inventories = inventories.iter().map(|i| i.to_json(&data)).collect();
        fn join(out: &mut String, n: usize, mut one: impl FnMut(usize, &mut String)) {
            for i in 0..n {
                if i > 0 {
                    out.push_str("|\n");
                }
                one(i, out);
            }
        }
        let direct = crate::save::SaveFile::empty().text_with(Some((
            &|out: &mut String| join(out, objects.len(), |i, out| objects[i].write_save(&data, out)),
            &|out: &mut String| join(out, inventories.len(), |i, out| inventories[i].write_save(&data, out)),
        )));
        assert!(json.to_text() == direct, "the direct text differs from the JSON route's");
    }

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
