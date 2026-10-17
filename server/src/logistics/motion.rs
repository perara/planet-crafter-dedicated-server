//! Drone flight as LogisticManager.DroneTransformUpdateJob computes it (Unity.Mathematics
//! quaternion math in floats), and the NetworkTransform state the server sends for a drone.
//!
//! Drones fly straight through everything: the job never asks the physics engine (the game's
//! DroneColliding only tints a drone that passes through something, on clients). So there is
//! no collision avoidance to approximate.

use crate::wire::Writer;

pub type V3 = [f32; 3];
/// x, y, z, w.
pub type Q = [f32; 4];

pub const IDENTITY: Q = [0.0, 0.0, 0.0, 1.0];
/// Unity's float epsilon, as the job and Drone.SetTarget compare against it.
pub const EPSILON: f32 = 1.192_092_9e-7;

pub fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot3(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

pub fn distance_sq(a: V3, b: V3) -> f32 {
    let d = sub(a, b);
    dot3(d, d)
}

pub fn distance(a: V3, b: V3) -> f32 {
    distance_sq(a, b).sqrt()
}

pub fn length_sq(a: V3) -> f32 {
    dot3(a, a)
}

fn normalize4(q: Q) -> Q {
    let s = 1.0 / (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    q.map(|v| v * s)
}

/// quaternion(float3x3) of Unity.Mathematics (branchless, sign-mask selection).
fn from_matrix(u: V3, v: V3, w: V3) -> Q {
    let bits = f32::to_bits;
    let u_sign = bits(u[0]) & 0x8000_0000;
    let t = v[1] + f32::from_bits(bits(w[2]) ^ u_sign);
    let u_mask = ((u_sign as i32) >> 31) as u32;
    let t_mask = (bits(t) as i32 >> 31) as u32;
    let tr = 1.0 + u[0].abs();
    let flips = [0u32, 0x8000_0000, 0x8000_0000, 0x8000_0000];
    let u_flips = [0u32, 0x8000_0000, 0, 0x8000_0000];
    let t_flips = [0x8000_0000u32, 0x8000_0000, 0x8000_0000, 0];
    let a = [tr, u[1], w[0], v[2]];
    let b = [t, v[0], u[2], w[1]];
    let mut value = [0f32; 4];
    for i in 0..4 {
        let flip = flips[i] ^ (u_mask & u_flips[i]) ^ (t_mask & t_flips[i]);
        value[i] = a[i] + f32::from_bits(bits(b[i]) ^ flip);
    }
    let zwxy = [value[2], value[3], value[0], value[1]];
    let value: Q = std::array::from_fn(|i| f32::from_bits((bits(value[i]) & !u_mask) | (bits(zwxy[i]) & u_mask)));
    let wzyx = [value[3], value[2], value[1], value[0]];
    let value: Q = std::array::from_fn(|i| f32::from_bits((bits(wzyx[i]) & !t_mask) | (bits(value[i]) & t_mask)));
    normalize4(value)
}

/// quaternion.LookRotation(forward, up). Like the game, `forward` is passed unnormalized.
pub fn look_rotation(forward: V3, up: V3) -> Q {
    let c = cross(up, forward);
    let len = length_sq(c).sqrt();
    let t = c.map(|v| v / len);
    from_matrix(t, cross(forward, t), forward)
}

/// math.slerp (Unity.Mathematics).
pub fn slerp(q1: Q, mut q2: Q, t: f32) -> Q {
    let mut dt = q1[0] * q2[0] + q1[1] * q2[1] + q1[2] * q2[2] + q1[3] * q2[3];
    if dt < 0.0 {
        dt = -dt;
        q2 = q2.map(|v| -v);
    }
    if dt < 0.9995 {
        let angle = dt.acos();
        let s = 1.0 / (1.0 - dt * dt).sqrt();
        let w1 = (angle * (1.0 - t)).sin() * s;
        let w2 = (angle * t).sin() * s;
        std::array::from_fn(|i| q1[i] * w1 + q2[i] * w2)
    } else {
        // nlerp: q2 already has the sign of the dot product.
        normalize4(std::array::from_fn(|i| q1[i] + t * (q2[i] - q1[i])))
    }
}

/// q * v.
pub fn rotate(q: Q, v: V3) -> V3 {
    let u = [q[0], q[1], q[2]];
    let t = cross(u, v).map(|c| c * 2.0);
    let c = cross(u, t);
    std::array::from_fn(|i| v[i] + q[3] * t[i] + c[i])
}

/// Quaternion.eulerAngles.y, in [0, 360).
pub fn euler_y(q: Q) -> f32 {
    let [x, y, z, w] = q;
    let yaw = (2.0 * (x * z + w * y)).atan2(1.0 - 2.0 * (x * x + y * y)).to_degrees();
    if yaw < 0.0 { yaw + 360.0 } else { yaw }
}

/// Mathf.DeltaAngle.
fn delta_angle(current: f32, target: f32) -> f32 {
    let d = (target - current) - ((target - current) / 360.0).floor() * 360.0;
    if d > 180.0 { d - 360.0 } else { d }
}

/// One frame of DroneTransformUpdateJob for a drone not yet at its target: turn towards the
/// target, then move along the heading the drone had before turning.
pub fn step(position: V3, rotation: Q, target: V3, forward_velocity: f32, angular_velocity: f32, dt: f32) -> (V3, Q) {
    let turned = slerp(rotation, look_rotation(sub(target, position), [0.0, 1.0, 0.0]), angular_velocity * dt);
    let ahead = rotate(rotation, [0.0, 0.0, dt * forward_velocity]);
    ([position[0] + ahead[0], position[1] + ahead[1], position[2] + ahead[2]], turned)
}

// ---- NetworkTransform ---------------------------------------------------------------------

const POSITION_BITS: [u32; 3] = [2, 4, 8];
const ROT_Y: u32 = 32;
const TELEPORTING: u32 = 1 << 10;
const INTERPOLATE: u32 = 1 << 11;
const SYNCHRONIZATION: u32 = 1 << 15;
const RELIABLE_SEQUENCED: u32 = 1 << 19;
/// The drone container's NetworkTransform thresholds: GroupNetworkContainer.CreateChild
/// multiplies them by _movingObjectPrecisionFactor (10) for a child with a Rigidbody, which
/// drones have (they sync position and the Y angle only). Measured on the real server: no
/// position change under 1.0 and no angle change under 10 degrees is ever sent.
const POSITION_THRESHOLD: f32 = 1.0;
const ROTATION_THRESHOLD: f32 = 10.0;

/// The authority's NetworkTransform state for one drone: what it last sent.
#[derive(Clone, Debug)]
pub struct Transform {
    /// Position x, y, z and the Y angle.
    last: [f32; 4],
    /// The first tick after a spawn sends everything as a teleport.
    teleport: bool,
}

impl Default for Transform {
    fn default() -> Self {
        Transform { last: [0.0; 4], teleport: true }
    }
}

fn state(flags: u32, tick: i32, values: &[f32]) -> Vec<u8> {
    let mut w = Writer::new();
    w.packed_u32(flags).packed_i32(tick);
    values.iter().for_each(|v| {
        w.f32(*v);
    });
    w.0
}

impl Transform {
    /// NetworkTransform.OnNetworkTick on the authority: the state to send this tick, if any axis
    /// moved past its threshold since it was last sent.
    pub fn tick(&mut self, position: V3, rotation: Q, tick: i32) -> Option<Vec<u8>> {
        let ry = euler_y(rotation);
        let now = [position[0], position[1], position[2], ry];
        if std::mem::take(&mut self.teleport) {
            self.last = now;
            let flags = POSITION_BITS.iter().fold(ROT_Y | TELEPORTING | INTERPOLATE | RELIABLE_SEQUENCED, |f, b| f | b);
            return Some(state(flags, tick, &now));
        }
        let mut flags = INTERPOLATE | RELIABLE_SEQUENCED;
        let mut values = Vec::new();
        for i in 0..3 {
            if (self.last[i] - now[i]).abs() >= POSITION_THRESHOLD {
                self.last[i] = now[i];
                flags |= POSITION_BITS[i];
                values.push(now[i]);
            }
        }
        if delta_angle(self.last[3], ry).abs() >= ROTATION_THRESHOLD {
            self.last[3] = ry;
            flags |= ROT_Y;
            values.push(ry);
        }
        (!values.is_empty()).then(|| state(flags, tick, &values))
    }

    /// The object's custom sync data for a spawn (NetworkTransform.OnSynchronize): one behaviour
    /// with the full state, flagged as a synchronization.
    pub fn sync_tail(behaviour: u16, position: V3, rotation: Q, tick: i32) -> Vec<u8> {
        let flags = POSITION_BITS.iter().fold(ROT_Y | TELEPORTING | INTERPOLATE | SYNCHRONIZATION | RELIABLE_SEQUENCED, |f, b| f | b);
        let body = state(flags, tick, &[position[0], position[1], position[2], euler_y(rotation)]);
        let mut w = Writer::new();
        w.u8(1).u16(behaviour).i32(body.len() as i32).bytes(&body);
        w.0
    }
}

/// NetworkTransformMessage: object id, behaviour id, state.
pub fn message(object: u64, behaviour: u16, state: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.packed_u64(object).packed_i32(behaviour as i32).bytes(state);
    w.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::hex;

    #[test]
    fn look_rotation_and_euler_agree() {
        let q = look_rotation([5.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        assert!((euler_y(q) - 90.0).abs() < 1e-3, "{q:?}");
        let back = look_rotation([0.0, 0.0, -3.0], [0.0, 1.0, 0.0]);
        assert!((euler_y(back) - 180.0).abs() < 1e-3);
        assert!((euler_y(look_rotation([-1.0, 0.0, 0.0], [0.0, 1.0, 0.0])) - 270.0).abs() < 1e-3);
        let f = rotate(q, [0.0, 0.0, 1.0]);
        assert!((f[0] - 1.0).abs() < 1e-5 && f[2].abs() < 1e-5);
    }

    #[test]
    fn first_frames_match_the_real_server() {
        // Oracle: a Drone1 released at (10, 4, 10) facing +Z towards (25, 4, 10) turned to
        // 15.0001 degrees in its first 30 fps frame and only then started to move sideways.
        let (p, q) = step([10.0, 4.0, 10.0], IDENTITY, [25.0, 4.0, 10.0], 12.5, 5.0, 1.0 / 30.0);
        assert!((euler_y(q) - 15.0).abs() < 0.01, "{}", euler_y(q));
        assert!((p[0] - 10.0).abs() < 1e-6 && (p[2] - 10.0 - 12.5 / 30.0).abs() < 1e-5, "{p:?}");
    }

    #[test]
    fn encodes_states_like_the_real_server() {
        // Captured from the oracle: drone 37's spawn sync, its first tick (a teleport), then a
        // turn of the Y angle only.
        let pos = [10.0, 4.0, 10.0];
        assert_eq!(Transform::sync_tail(3, pos, IDENTITY, 24764), hex("01030016000000736144c30b0600002041000080400000204100000000"));
        let mut t = Transform::default();
        assert_eq!(t.tick(pos, IDENTITY, 24765).unwrap(), hex("736140d30b0600002041000080400000204100000000"));
        let ry = f32::from_le_bytes([0x4f, 0x00, 0x70, 0x41]);
        let turned = [0.0, (ry / 2.0).to_radians().sin(), 0.0, (ry / 2.0).to_radians().cos()];
        assert_eq!(t.tick(pos, turned, 24766).unwrap()[..6], hex("034140e30b06")[..]);
        assert!(t.tick([10.5, 4.0, 10.0], turned, 24767).is_none(), "under the thresholds");
        assert_eq!(message(37, 3, &[0xaa]), hex("520231aa"));
    }
}
