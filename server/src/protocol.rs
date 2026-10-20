//! Netcode messages the server sends and the RPCs it understands.

use crate::wire::{Reader, Writer};

pub const CONNECTION_APPROVED: u32 = 0;
pub const CONNECTION_REQUEST: u32 = 1;
pub const CHANGE_OWNERSHIP: u32 = 2;
pub const CLIENT_CONNECTED: u32 = 3;
pub const CLIENT_DISCONNECTED: u32 = 4;
pub const CLIENT_RPC: u32 = 5;
pub const CREATE_OBJECT: u32 = 6;
pub const DESTROY_OBJECT: u32 = 7;
pub const NETWORK_TRANSFORM: u32 = 12;
pub const RPC: u32 = 16;
pub const NETWORK_VARIABLE_DELTA: u32 = 13;
pub const TIME_SYNC: u32 = 21;
/// DisconnectReasonMessage: shown to a kicked or refused player.
pub const DISCONNECT_REASON: u32 = 8;

/// DisconnectReasonMessage: the batch that tells a kicked or refused player why.
pub fn reason_batch(reason: &str) -> Vec<u8> {
    let units: Vec<u16> = reason.encode_utf16().collect();
    let mut w = Writer::new();
    w.packed_i32(0).u32(units.len() as u32);
    units.iter().for_each(|u| {
        w.u16(*u);
    });
    crate::wire::batch(&[(DISCONNECT_REASON, w.0)])
}

/// NetworkObject.SceneObject flag bits.
pub const HAS_PARENT: u16 = 1 << 1;
pub const HAS_TRANSFORM: u16 = 1 << 3;

/// A NetworkObject.SceneObject as read off the wire.
pub struct SceneObjectInfo {
    pub flags: u16,
    pub hash: u32,
    pub id: u64,
    pub owner: u64,
    #[cfg_attr(not(test), allow(dead_code))] // the spawn tests check it
    pub parent: Option<u64>,
    pub position: Option<[f32; 3]>,
    pub rotation: Option<[f32; 4]>,
    pub sync: Vec<u8>,
}

const IS_LATEST_PARENT_SET: u16 = 1 << 4;

pub fn read_scene_object(r: &mut Reader) -> Option<SceneObjectInfo> {
    let (flags, hash, id, owner) = (r.u16_raw()?, r.u32()?, r.packed_u64()?, r.packed_u64()?);
    let mut parent = None;
    if flags & HAS_PARENT != 0 {
        parent = Some(r.packed_u64()?);
        if flags & IS_LATEST_PARENT_SET != 0 {
            r.packed_u64()?;
        }
    }
    let (mut position, mut rotation) = (None, None);
    if flags & HAS_TRANSFORM != 0 {
        position = Some([r.f32()?, r.f32()?, r.f32()?]);
        rotation = Some([r.f32()?, r.f32()?, r.f32()?, r.f32()?]);
        r.skip(12)?;
    }
    r.i32()?;
    let n = r.i32()? as usize;
    let sync = r.rest().get(..n)?.to_vec();
    r.skip(n)?;
    Some(SceneObjectInfo { flags, hash, id, owner, parent, position, rotation, sync })
}

/// ConnectionApprovedMessage: the client's own id and every object it is given.
pub fn read_approved(body: &[u8]) -> Option<(u64, Vec<SceneObjectInfo>)> {
    let mut r = Reader::new(body);
    for _ in 0..r.packed_i32()? {
        r.u32()?;
        r.packed_i32()?;
    }
    let owner = r.packed_u64()?;
    r.packed_i32()?;
    for _ in 0..r.i32()? {
        r.u64()?;
    }
    let n = r.u32()?;
    let objects = (0..n).map(|_| read_scene_object(&mut r)).collect::<Option<Vec<_>>>()?;
    Some((owner, objects))
}

/// DestroyObjectMessage v2: object id, flags (4 = destroy the GameObject).
pub fn destroy_object(id: u64) -> (u32, Vec<u8>) {
    (DESTROY_OBJECT, Writer::new().packed_u64(id).u8(4).0.clone())
}

pub fn client_connected(client: u64) -> (u32, Vec<u8>) {
    (CLIENT_CONNECTED, Writer::new().packed_u64(client).bool(false).0.clone())
}

pub fn client_disconnected(client: u64) -> (u32, Vec<u8>) {
    (CLIENT_DISCONNECTED, Writer::new().packed_u64(client).0.clone())
}

pub fn time_sync(tick: i32) -> (u32, Vec<u8>) {
    (TIME_SYNC, Writer::new().packed_i32(tick).0.clone())
}

/// EncodingHelpers.Encode: base-48 digits, least significant first (the invite code prefix).
pub fn invite_code(mut value: u64) -> String {
    const MAP: &[u8; 48] = b"ABCDEFGHJKMNPQRSTUVWXYZabcdefghjkmnpqrstuvxyz234";
    if value == 0 {
        return "A".into();
    }
    let mut out = String::new();
    while value != 0 {
        out.push(MAP[(value % 48) as usize] as char);
        value /= 48;
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn invite_code_matches_the_game() {
        // The plugin logged "invite code pr2rqesRqVB4242" for game server 90295105288932371.
        assert_eq!(super::invite_code(90295105288932371), "pr2rqesRqVB");
    }

    /// What a kicked player is shown: one DisconnectReasonMessage (type 8), a packed i32 0 (as the
    /// bytes always were), then the text's UTF-16 length and units.
    #[test]
    fn the_disconnect_reason_is_one_message_of_type_8_with_the_text_in_utf16() {
        let batch = super::reason_batch("Hi\u{e5}");
        let messages = crate::wire::messages(&batch);
        assert_eq!(messages.len(), 1);
        assert_eq!((messages[0].0, messages[0].1), (super::DISCONNECT_REASON, &[1u8, 3, 0, 0, 0, b'H', 0, b'i', 0, 0xe5, 0][..]));
        assert_eq!(super::DISCONNECT_REASON, 8);
    }
}
