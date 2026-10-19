//! transform::update and what is done with it.
use super::{euler_angles, from_euler, update};

pub fn run(data: &[u8]) {
    let Some(u) = update(data) else { return };
    let p = [1.0, 2.0, 3.0];
    let q = [0.0, 0.0, 0.0, 1.0];
    if u.finite() {
        if let Some((p, q)) = u.applied(p, q) {
            assert!(p.iter().chain(&q).all(|v| v.is_finite()));
            // Merging a second time (onto a state that may be large) must not panic either.
            let _ = u.applied(p, q);
        }
    }
    let mut f = [0.0f32; 4];
    for (i, c) in data.chunks(4).take(4).enumerate() {
        let mut b = [0u8; 4];
        b[..c.len()].copy_from_slice(c);
        f[i] = f32::from_le_bytes(b);
    }
    let _ = euler_angles(f);
    let _ = from_euler([f[0], f[1], f[2]]);
}
