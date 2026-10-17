//! NetworkTransformMessage: object id, behaviour id, then the transform state
//! (NetworkTransformState.NetworkSerialize: bit flags, tick, optional state id, then a value per
//! synchronized axis that moved past its threshold).

use crate::wire::Reader;

const POSITION_AXES: [u32; 3] = [2, 4, 8];
const ROT_AXES: [u32; 3] = [16, 32, 64];
const TELEPORTING: u32 = 1 << 10;
const QUATERNION_SYNC: u32 = 1 << 12;
const QUATERNION_COMPRESSION: u32 = 1 << 13;
const HALF_FLOATS: u32 = 1 << 14;
const TRACK_STATE_ID: u32 = 1 << 28;
const IN_LOCAL_SPACE: u32 = 1;

/// The axes one state carries; the others keep their last value.
#[derive(Debug, Default, PartialEq)]
pub struct Update {
    pub object: u64,
    pub position: [Option<f32>; 3],
    /// Euler angles (degrees) when the transform syncs them.
    pub euler: [Option<f32>; 3],
    /// The full rotation when the transform syncs quaternions (uncompressed).
    pub quaternion: Option<[f32; 4]>,
}

/// The update a message carries; None for half-float or local-space states (not decoded).
pub fn update(body: &[u8]) -> Option<Update> {
    let mut r = Reader::new(body);
    let object = r.packed_u64()?;
    r.packed_i32()?;
    let flags = r.packed_u32()?;
    r.packed_i32()?;
    if flags & (HALF_FLOATS | IN_LOCAL_SPACE) != 0 {
        return None;
    }
    if flags & TRACK_STATE_ID != 0 {
        r.u32()?;
    }
    let mut u = Update { object, ..Default::default() };
    for (i, axis) in POSITION_AXES.iter().enumerate() {
        if flags & axis != 0 {
            u.position[i] = Some(r.f32()?);
        }
    }
    let rotates = ROT_AXES.iter().any(|a| flags & a != 0);
    if flags & QUATERNION_SYNC == 0 {
        for (i, axis) in ROT_AXES.iter().enumerate() {
            if flags & axis != 0 {
                u.euler[i] = Some(r.f32()?);
            }
        }
    } else if rotates && (flags & TELEPORTING != 0 || flags & QUATERNION_COMPRESSION == 0) {
        // ponytail: compressed quaternions (a packed u32) are skipped; no game transform uses them.
        u.quaternion = Some([r.f32()?, r.f32()?, r.f32()?, r.f32()?]);
    }
    Some(u)
}

/// (object id, position, rotation) of a state carrying the whole position.
#[cfg(test)]
pub fn position_rotation(body: &[u8]) -> Option<(u64, [f32; 3], Option<[f32; 4]>)> {
    let u = update(body)?;
    let position = [u.position[0]?, u.position[1]?, u.position[2]?];
    let mut rotation = [0.0, 0.0, 0.0, 1.0];
    u.apply(&mut [0.0; 3], &mut rotation);
    let rotates = u.quaternion.is_some() || u.euler.iter().any(Option::is_some);
    Some((u.object, position, rotates.then_some(rotation)))
}

impl Update {
    /// NetworkTransform.ApplyUpdatedState: present axes replace the last state's; Euler angles
    /// merge on the current rotation's eulerAngles.
    pub fn apply(&self, position: &mut [f32; 3], rotation: &mut [f32; 4]) {
        for (p, v) in position.iter_mut().zip(self.position) {
            if let Some(v) = v {
                *p = v;
            }
        }
        if let Some(q) = self.quaternion {
            *rotation = q;
        } else if self.euler.iter().any(Option::is_some) {
            let mut e = euler_angles(*rotation);
            for (a, v) in e.iter_mut().zip(self.euler) {
                if let Some(v) = v {
                    *a = v;
                }
            }
            *rotation = from_euler(e);
        }
    }
}

/// Quaternion.eulerAngles: degrees in [0, 360), Z then X then Y.
pub fn euler_angles(q: [f32; 4]) -> [f32; 3] {
    let [x, y, z, w] = q;
    let sx = (2.0 * (w * x - y * z)).clamp(-1.0, 1.0);
    let (yaw, roll) = if sx.abs() < 0.99999 {
        ((2.0 * (x * z + w * y)).atan2(1.0 - 2.0 * (x * x + y * y)), (2.0 * (x * y + w * z)).atan2(1.0 - 2.0 * (x * x + z * z)))
    } else {
        // Gimbal lock: the whole turn on Y.
        ((2.0 * (w * y - x * z)).atan2(1.0 - 2.0 * (y * y + z * z)), 0.0)
    };
    [sx.asin(), yaw, roll].map(|a| a.to_degrees().rem_euclid(360.0))
}

/// Quaternion.Euler(x, y, z): Y * X * Z.
pub fn from_euler(e: [f32; 3]) -> [f32; 4] {
    let half = |deg: f32| deg.to_radians() / 2.0;
    let (x, y, z) = (half(e[0]), half(e[1]), half(e[2]));
    let qx = [x.sin(), 0.0, 0.0, x.cos()];
    let qy = [0.0, y.sin(), 0.0, y.cos()];
    let qz = [0.0, 0.0, z.sin(), z.cos()];
    crate::geometry::mul(crate::geometry::mul(qy, qx), qz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::hex;

    #[test]
    fn reads_a_captured_player_position() {
        // Captured from the real server: player 28 at (558.2, 3.3, 494.1).
        let u = update(&hex("c201017361406248cd8c0b44e82b5340cd0cf74300000000")).unwrap();
        assert_eq!(u.object, 28);
        let p = u.position.map(Option::unwrap);
        assert!((p[0] - 558.2).abs() < 0.01 && (p[1] - 3.3).abs() < 0.01 && (p[2] - 494.1).abs() < 0.01, "{p:?}");
    }

    /// A state for object 5 with these flags and floats (behaviour 0, tick 7).
    fn message(flags: u32, floats: &[f32]) -> Vec<u8> {
        let mut w = crate::wire::Writer::new();
        w.packed_u64(5).packed_i32(0).packed_u32(flags).packed_i32(7);
        for f in floats {
            w.bytes(&f.to_le_bytes());
        }
        w.0
    }

    #[test]
    fn partial_states_merge_into_the_last_one() {
        let (mut p, mut q) = ([1.0, 2.0, 3.0], from_euler([10.0, 20.0, 30.0]));
        // Only Y moved, then only the yaw turned.
        update(&message(4, &[5.0])).unwrap().apply(&mut p, &mut q);
        update(&message(32, &[90.0])).unwrap().apply(&mut p, &mut q);
        assert_eq!(p, [1.0, 5.0, 3.0]);
        let e = euler_angles(q);
        assert!((e[0] - 10.0).abs() < 0.01 && (e[1] - 90.0).abs() < 0.01 && (e[2] - 30.0).abs() < 0.01, "{e:?}");
        // Quaternion sync carries the whole rotation.
        let r = from_euler([0.0, 45.0, 0.0]);
        let u = update(&message(QUATERNION_SYNC | 16 | 32 | 64, &r)).unwrap();
        assert_eq!(u.quaternion, Some(r));
        // Half floats are not decoded.
        assert!(update(&message(HALF_FLOATS | 2, &[1.0])).is_none());
    }

    #[test]
    fn euler_round_trips() {
        for e in [[0.0, 73.0, 0.0], [30.0, -120.0, 45.0], [-60.0, 10.0, 170.0]] {
            let back = euler_angles(from_euler(e));
            assert!(back.iter().zip(e).all(|(a, b)| ((a - b + 540.0) % 360.0 - 180.0).abs() < 0.01), "{e:?} -> {back:?}");
        }
        assert!((euler_angles(crate::geometry::from_euler_y(73.0))[1] - 73.0).abs() < 0.01);
    }
}
