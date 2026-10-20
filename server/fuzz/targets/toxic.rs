//! The toxic areas' keys: an area is found by the key a client sends, which is any number.
use super::*;

pub fn run(data: &[u8]) {
    let word = |i: usize| data.get(i * 4..i * 4 + 4).map_or(0, |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let n = 1 + data.first().copied().unwrap_or(0) as usize % 6;
    let areas: Vec<Area> = (0..n).map(|i| Area { id: word(i + 1), time: 1.0, total: word(i + 10).rem_euclid(5), todo: vec![], cleaned: word(i + 12).rem_euclid(5) }).collect();
    let key = word(0);
    if let Some(a) = find(&areas, key) {
        // What is found is the area of that key or the only one within the slack, in exact arithmetic.
        let near = |a: &Area| (a.id as i64 - key as i64).abs() <= KEY_SLACK as i64;
        assert!(a.id == key || near(a), "area {} is not near key {key}", a.id);
    } else {
        assert!(areas.iter().all(|a| a.id != key));
    }
    let _ = area_id([f32::from_bits(word(20) as u32), f32::from_bits(word(21) as u32), f32::from_bits(word(22) as u32)]);
    let _ = areas.iter().map(Area::progress).sum::<f32>();
}
