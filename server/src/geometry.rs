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

/// A dumped document whose elements can be read as numbers: a table's `Doc`, a scene's `Value`.
pub trait Elements: std::ops::Index<usize, Output = Self> {
    fn number(&self) -> Option<f64>;
}

impl Elements for serde_json::Value {
    fn number(&self) -> Option<f64> {
        self.as_f64()
    }
}

impl Elements for crate::doc::Doc {
    fn number(&self) -> Option<f64> {
        self.as_f64()
    }
}

/// A dumped Vector3 ([x, y, z]).
pub fn v3<T: Elements>(v: &T) -> [f32; 3] {
    [0, 1, 2].map(|i| v[i].number().unwrap_or(0.0) as f32)
}

/// A dumped Quaternion ([x, y, z, w]).
pub fn q<T: Elements>(v: &T) -> [f32; 4] {
    [0, 1, 2, 3].map(|i| v[i].number().unwrap_or(if i == 3 { 1.0 } else { 0.0 }) as f32)
}

/// Unity.Mathematics.Random (xorshift32), the deterministic generator the game seeds itself.
#[derive(serde::Serialize, serde::Deserialize)]
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
        // Unity asserts min < max; a reversed range from a client would wrap to billions.
        if max <= min {
            self.next_state();
            return min;
        }
        let range = max.wrapping_sub(min) as u32;
        ((self.next_state() as u64 * range as u64) >> 32) as i32 + min
    }
    pub fn next_bool(&mut self) -> bool {
        self.next_state() & 1 == 1
    }
    pub fn next_float(&mut self) -> f32 {
        f32::from_bits(0x3f80_0000 | (self.next_state() >> 9)) - 1.0
    }
}

/// UnityEngine.Matrix4x4, rows of columns; products in Matrix4x4.operator*'s order.
pub type Matrix = [[f32; 4]; 4];

pub fn matrix_mul(a: &Matrix, b: &Matrix) -> Matrix {
    std::array::from_fn(|i| std::array::from_fn(|j| a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j] + a[i][3] * b[3][j]))
}

/// Matrix4x4.TRS (Matrix4x4.Rotate with scaled columns and a translation).
pub fn trs(position: [f32; 3], q: [f32; 4], scale: [f32; 3]) -> Matrix {
    let [x, y, z, w] = q;
    let (x2, y2, z2) = (x * 2.0, y * 2.0, z * 2.0);
    let (xx, yy, zz, xy, xz, yz, wx, wy, wz) = (x * x2, y * y2, z * z2, x * y2, x * z2, y * z2, w * x2, w * y2, w * z2);
    let r = [[1.0 - (yy + zz), xy - wz, xz + wy], [xy + wz, 1.0 - (xx + zz), yz - wx], [xz - wy, yz + wx, 1.0 - (xx + yy)]];
    let mut m = [[0.0; 4]; 4];
    for i in 0..3 {
        for j in 0..3 {
            m[i][j] = r[i][j] * scale[j];
        }
        m[i][3] = position[i];
    }
    m[3][3] = 1.0;
    m
}

/// Matrix4x4.GetPosition.
pub fn matrix_position(m: &Matrix) -> [f32; 3] {
    [m[0][3], m[1][3], m[2][3]]
}

/// Matrix4x4.rotation: the scale taken out of the columns, then MatrixToQuaternion.
pub fn matrix_rotation(m: &Matrix) -> [f32; 4] {
    let len: [f32; 3] = std::array::from_fn(|j| (m[0][j] * m[0][j] + m[1][j] * m[1][j] + m[2][j] * m[2][j]).sqrt());
    let r = |i: usize, j: usize| m[i][j] / len[j];
    let trace = r(0, 0) + r(1, 1) + r(2, 2);
    let mut q = [0.0f32; 4];
    if trace > 0.0 {
        let root = (trace + 1.0).sqrt();
        q[3] = 0.5 * root;
        let root = 0.5 / root;
        q[0] = (r(2, 1) - r(1, 2)) * root;
        q[1] = (r(0, 2) - r(2, 0)) * root;
        q[2] = (r(1, 0) - r(0, 1)) * root;
    } else {
        let mut i = usize::from(r(1, 1) > r(0, 0));
        if r(2, 2) > r(i, i) {
            i = 2;
        }
        let (j, k) = ((i + 1) % 3, (i + 2) % 3);
        let root = (r(i, i) - r(j, j) - r(k, k) + 1.0).sqrt();
        q[i] = 0.5 * root;
        let root = 0.5 / root;
        q[3] = (r(k, j) - r(j, k)) * root;
        q[j] = (r(j, i) + r(i, j)) * root;
        q[k] = (r(k, i) + r(i, k)) * root;
    }
    let n = q.iter().map(|v| v * v).sum::<f32>().sqrt();
    q.map(|v| v / n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrices_round_trip_positions_and_rotations() {
        let q = mul(from_euler_y(30.0), [0.3826834, 0.0, 0.0, 0.9238795]);
        let m = matrix_mul(&trs([1.0, 2.0, 3.0], q, [2.0, 2.0, 2.0]), &trs([0.5, 0.0, 0.0], from_euler_y(200.0), [1.0; 3]));
        let back = matrix_rotation(&m);
        let want = mul(q, from_euler_y(200.0));
        let dot: f32 = (0..4).map(|i| back[i] * want[i]).sum();
        assert!(dot.abs() > 0.99999, "{back:?} {want:?}");
        let p = matrix_position(&m);
        let r = rotate(q, [1.0, 0.0, 0.0]);
        assert!((0..3).all(|i| (p[i] - ([1.0, 2.0, 3.0][i] + r[i])).abs() < 1e-5), "{p:?}");
    }

    #[test]
    fn yaw_round_trips() {
        let q = from_euler_y(73.0);
        assert!((euler_y(q) - 73.0).abs() < 0.01);
        let r = rotate_y(from_euler_y(10.0), 20.0);
        assert!((euler_y(r) - 30.0).abs() < 0.01);
    }
}

pub mod navigation;
