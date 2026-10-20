//! What a plugin may ask the world (capability `world.read`): read-only copies, taken at the
//! moment of the call. Answers use the codec; new fields come at the end.

use alloc::string::String;
use alloc::vec::Vec;

use crate::codec::{DecodeError, Reader, Writer};

pub const Q_WORLD: u32 = 1;
pub const Q_PLAYERS: u32 = 2;
/// The rules that differ from the defaults, as JSON text (the same text as RuleChange).
pub const Q_RULES: u32 = 3;
/// ABI 1.3. A player's backpack: the argument is the client (`u64`); the answer is `InventoryInfo`.
pub const Q_INVENTORY: u32 = 4;
/// ABI 1.3. The world objects near a point: the argument is `NearbyArgs`; the answer a count and
/// that many `Nearby`, the closest first.
pub const Q_NEARBY: u32 = 5;
/// ABI 1.3. One world object: the argument is `ObjectRef`; the answer a `bool` (found) and, if so,
/// an `ObjectInfo`.
pub const Q_OBJECT: u32 = 6;
/// ABI 1.3. The plugin's own settings (`[[config]]` in its manifest, as the admin set them or as
/// their defaults say): a count and that many (`str` key, `str` value). Needs no capability, and
/// never holds a secret.
pub const Q_CONFIG: u32 = 7;

/// The most `Q_NEARBY` returns, and the widest radius it looks at.
pub const MAX_NEARBY: u32 = 64;
pub const MAX_NEARBY_RADIUS: f32 = 1000.0;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorldInfo {
    pub game_ms: i64,
    pub planet: String,
    pub players: u32,
    pub world_objects: u32,
    pub network_objects: u32,
    /// The planet's terraformation index (the game's own total).
    pub terraformation: f64,
    /// The world stands still (paused by the admin, or empty with world.pauseWhenEmpty).
    pub standing_still: bool,
    /// ABI 1.3. The wall clock, Unix milliseconds.
    pub unix_ms: i64,
}

impl WorldInfo {
    pub fn encode(&self, w: &mut Writer) {
        w.i64(self.game_ms).str(&self.planet).u32(self.players).u32(self.world_objects).u32(self.network_objects).f64(self.terraformation).bool(self.standing_still).i64(self.unix_ms);
    }
    pub fn decode(bytes: &[u8]) -> Result<WorldInfo, DecodeError> {
        let mut r = Reader::new(bytes);
        Ok(WorldInfo { game_ms: r.i64()?, planet: r.string()?, players: r.u32()?, world_objects: r.u32()?, network_objects: r.u32()?, terraformation: r.f64()?, standing_still: r.bool()?, unix_ms: r.i64()? })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlayerInfo {
    pub client: u64,
    pub name: String,
    /// Verified SteamID64 as text (invite-code players), else empty.
    pub steam_id: String,
    pub position: [f32; 3],
    /// Oxygen, water, health, toxicity.
    pub gauges: [f32; 4],
    /// ABI 1.3. The player's id in this world's save (see `Event::PlayerJoin`).
    pub player_id: u64,
    /// ABI 1.3. The server's operator (`access.operators` lists the SteamID64s the admin trusts).
    pub operator: bool,
}

impl PlayerInfo {
    pub fn encode(&self, w: &mut Writer) {
        w.u64(self.client).str(&self.name).str(&self.steam_id);
        self.position.iter().for_each(|v| {
            w.f32(*v);
        });
        self.gauges.iter().for_each(|v| {
            w.f32(*v);
        });
        w.u64(self.player_id).bool(self.operator);
    }
    pub fn decode(r: &mut Reader<'_>) -> Result<PlayerInfo, DecodeError> {
        let (client, name, steam_id) = (r.u64()?, r.string()?, r.string()?);
        let position = [r.f32()?, r.f32()?, r.f32()?];
        let gauges = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
        Ok(PlayerInfo { client, name, steam_id, position, gauges, player_id: r.u64()?, operator: r.bool()? })
    }
}

pub fn encode_players(players: &[PlayerInfo], w: &mut Writer) {
    w.u32(players.len() as u32);
    players.iter().for_each(|p| p.encode(w));
}

pub fn decode_players(bytes: &[u8]) -> Result<Vec<PlayerInfo>, DecodeError> {
    let mut r = Reader::new(bytes);
    let n = r.u32()? as usize;
    // A player takes more than 4 bytes: a count the bytes cannot hold is a short answer.
    if n > bytes.len() / 4 {
        return Err(DecodeError::Short);
    }
    (0..n).map(|_| PlayerInfo::decode(&mut r)).collect()
}

/// One kind of item in a backpack and how many of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemCount {
    /// The item's group id, as the game's tables name it (`Iron`, `WaterBottle1`).
    pub group: String,
    pub count: u32,
}

/// A player's backpack (`Q_INVENTORY`): the items by group, and how full it is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InventoryInfo {
    pub slots_used: u32,
    pub slots_total: u32,
    /// Sorted by group id.
    pub items: Vec<ItemCount>,
}

impl InventoryInfo {
    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.slots_used).u32(self.slots_total).u32(self.items.len() as u32);
        for i in &self.items {
            w.str(&i.group).u32(i.count);
        }
    }
    pub fn decode(bytes: &[u8]) -> Result<InventoryInfo, DecodeError> {
        let mut r = Reader::new(bytes);
        let (slots_used, slots_total, n) = (r.u32()?, r.u32()?, r.u32()? as usize);
        // An item takes at least 8 bytes: a count the bytes cannot hold is a short answer.
        if n > r.rest().len() / 8 {
            return Err(DecodeError::Short);
        }
        let items = (0..n).map(|_| Ok(ItemCount { group: r.string()?, count: r.u32()? })).collect::<Result<_, DecodeError>>()?;
        Ok(InventoryInfo { slots_used, slots_total, items })
    }
    /// How many of `group` the backpack holds.
    pub fn count(&self, group: &str) -> u32 {
        self.items.iter().find(|i| i.group == group).map_or(0, |i| i.count)
    }
}

/// The argument of `Q_NEARBY`: a sphere. `max` is cut to `MAX_NEARBY`, `radius` to `MAX_NEARBY_RADIUS`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NearbyArgs {
    pub center: [f32; 3],
    pub radius: f32,
    pub max: u32,
}

impl NearbyArgs {
    pub fn encode(&self, w: &mut Writer) {
        w.f32(self.center[0]).f32(self.center[1]).f32(self.center[2]).f32(self.radius).u32(self.max);
    }
    pub fn decode(bytes: &[u8]) -> Result<NearbyArgs, DecodeError> {
        let mut r = Reader::new(bytes);
        Ok(NearbyArgs { center: [r.f32()?, r.f32()?, r.f32()?], radius: r.f32()?, max: r.u32()? })
    }
}

/// Which object `Q_OBJECT` means: the id of a world object (what `ObjectPlaced` and
/// `ObjectDestroyed` carry), or of a network object (what an RPC's `object` is).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectRef {
    World(i32),
    Net(u64),
}

impl ObjectRef {
    pub fn encode(&self, w: &mut Writer) {
        match self {
            ObjectRef::World(id) => w.u8(0).u64(*id as i64 as u64),
            ObjectRef::Net(id) => w.u8(1).u64(*id),
        };
    }
    pub fn decode(bytes: &[u8]) -> Result<ObjectRef, DecodeError> {
        let mut r = Reader::new(bytes);
        match r.u8()? {
            0 => Ok(ObjectRef::World(r.u64()? as i64 as i32)),
            _ => Ok(ObjectRef::Net(r.u64()?)),
        }
    }
}

/// A world object as `Q_OBJECT` and `Q_NEARBY` show it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObjectInfo {
    /// The world object's id.
    pub id: i32,
    /// The network object's id (what clients and RPCs call it); 0 when it has none.
    pub net: u64,
    /// The object's group id (`Drill1`, `Container1`).
    pub group: String,
    pub position: [f32; 3],
}

impl ObjectInfo {
    pub fn encode(&self, w: &mut Writer) {
        w.i32(self.id).u64(self.net).str(&self.group).f32(self.position[0]).f32(self.position[1]).f32(self.position[2]);
    }
    pub fn decode(r: &mut Reader<'_>) -> Result<ObjectInfo, DecodeError> {
        Ok(ObjectInfo { id: r.i32()?, net: r.u64()?, group: r.string()?, position: [r.f32()?, r.f32()?, r.f32()?] })
    }
}

/// An object and its distance from the point asked about.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Nearby {
    pub object: ObjectInfo,
    pub distance: f32,
}

pub fn encode_nearby(found: &[Nearby], w: &mut Writer) {
    w.u32(found.len() as u32);
    for n in found {
        n.object.encode(w);
        w.f32(n.distance);
    }
}

pub fn decode_nearby(bytes: &[u8]) -> Result<Vec<Nearby>, DecodeError> {
    let mut r = Reader::new(bytes);
    let n = r.u32()? as usize;
    // An object takes more than 4 bytes: a count the bytes cannot hold is a short answer.
    if n > bytes.len() / 4 {
        return Err(DecodeError::Short);
    }
    (0..n).map(|_| Ok(Nearby { object: ObjectInfo::decode(&mut r)?, distance: r.f32()? })).collect()
}

pub fn encode_settings(settings: &[(String, String)], w: &mut Writer) {
    w.u32(settings.len() as u32);
    for (k, v) in settings {
        w.str(k).str(v);
    }
}

pub fn decode_settings(bytes: &[u8]) -> Result<Vec<(String, String)>, DecodeError> {
    let mut r = Reader::new(bytes);
    let n = r.u32()? as usize;
    if n > bytes.len() / 8 {
        return Err(DecodeError::Short);
    }
    (0..n).map(|_| Ok((r.string()?, r.string()?))).collect()
}
