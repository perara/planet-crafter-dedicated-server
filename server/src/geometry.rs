//! Quaternion helpers for the few rotations the server computes itself.

/// Yaw (Euler Y, degrees) of a rotation.
pub fn euler_y(q: [f32; 4]) -> f32 {
    let [x, y, z, w] = q;
    let siny = 2.0 * (w * y + z * x);
    let cosy = 1.0 - 2.0 * (x * x + y * y);
    siny.atan2(cosy).to_degrees()
}

/// Quaternion.Euler(0, yaw, 0).
pub fn from_euler_y(yaw: f32) -> [f32; 4] {
    let h = yaw.to_radians() / 2.0;
    [0.0, h.sin(), 0.0, h.cos()]
}

/// q * Quaternion.Euler(0, degrees, 0) (Transform.Rotate in local space).
pub fn rotate_y(q: [f32; 4], degrees: f32) -> [f32; 4] {
    let r = from_euler_y(degrees);
    let [ax, ay, az, aw] = q;
    let [bx, by, bz, bw] = r;
    [aw * bx + ax * bw + ay * bz - az * by, aw * by - ax * bz + ay * bw + az * bx, aw * bz + ax * by - ay * bx + az * bw, aw * bw - ax * bx - ay * by - az * bz]
}


pub fn conj(q: [f32; 4]) -> [f32; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

pub fn mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1], a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0], a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3], a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2]]
}

pub fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let p = mul(mul(q, [v[0], v[1], v[2], 0.0]), conj(q));
    [p[0], p[1], p[2]]
}

pub fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// A dumped Vector3 ([x, y, z]).
pub fn v3(v: &serde_json::Value) -> [f32; 3] {
    [0, 1, 2].map(|i| v[i].as_f64().unwrap_or(0.0) as f32)
}

/// A dumped Quaternion ([x, y, z, w]).
pub fn q(v: &serde_json::Value) -> [f32; 4] {
    [0, 1, 2, 3].map(|i| v[i].as_f64().unwrap_or(if i == 3 { 1.0 } else { 0.0 }) as f32)
}

/// Unity.Mathematics.Random (xorshift32), the deterministic generator the game seeds itself.
pub struct MathRandom {
    pub state: u32,
}

impl MathRandom {
    pub fn new(seed: u32) -> MathRandom {
        let mut r = MathRandom { state: seed };
        r.next_state();
        r
    }
    pub fn next_state(&mut self) -> u32 {
        let t = self.state;
        self.state ^= self.state << 13;
        self.state ^= self.state >> 17;
        self.state ^= self.state << 5;
        t
    }
    pub fn next_int(&mut self, min: i32, max: i32) -> i32 {
        let range = max.wrapping_sub(min) as u32;
        ((self.next_state() as u64 * range as u64) >> 32) as i32 + min
    }
    pub fn next_bool(&mut self) -> bool {
        self.next_state() & 1 == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaw_round_trips() {
        let q = from_euler_y(73.0);
        assert!((euler_y(q) - 73.0).abs() < 0.01);
        let r = rotate_y(from_euler_y(10.0), 20.0);
        assert!((euler_y(r) - 30.0).abs() < 0.01);
    }
}
