//! What a plugin may ask the world (capability `world.read`): read-only copies, taken at the
//! moment of the call. Answers use the codec; new fields come at the end.

use alloc::string::String;
use alloc::vec::Vec;

use crate::codec::{DecodeError, Reader, Writer};

pub const Q_WORLD: u32 = 1;
pub const Q_PLAYERS: u32 = 2;
/// The rules that differ from the defaults, as JSON text (the same text as RuleChange).
pub const Q_RULES: u32 = 3;

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
}

impl WorldInfo {
    pub fn encode(&self, w: &mut Writer) {
        w.i64(self.game_ms).str(&self.planet).u32(self.players).u32(self.world_objects).u32(self.network_objects).f64(self.terraformation).bool(self.standing_still);
    }
    pub fn decode(bytes: &[u8]) -> Result<WorldInfo, DecodeError> {
        let mut r = Reader::new(bytes);
        Ok(WorldInfo { game_ms: r.i64()?, planet: r.string()?, players: r.u32()?, world_objects: r.u32()?, network_objects: r.u32()?, terraformation: r.f64()?, standing_still: r.bool()? })
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
    }
    pub fn decode(r: &mut Reader<'_>) -> Result<PlayerInfo, DecodeError> {
        let (client, name, steam_id) = (r.u64()?, r.string()?, r.string()?);
        let position = [r.f32()?, r.f32()?, r.f32()?];
        let gauges = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
        Ok(PlayerInfo { client, name, steam_id, position, gauges })
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
