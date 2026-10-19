//! wire::Reader, wire::messages and the packed integer round trip.
use super::{Reader, Writer, messages};

pub fn run(data: &[u8]) {
    let _ = messages(data);
    // The first bytes pick the reads, the rest is what they read.
    let ops = data.len().min(16);
    let mut r = Reader::new(&data[ops..]);
    for op in &data[..ops] {
        match op % 14 {
            0 => drop(r.u8()),
            1 => drop(r.bool()),
            2 => drop(r.i32()),
            3 => drop(r.u32()),
            4 => drop(r.u64()),
            5 => drop(r.u16_raw()),
            6 => drop(r.i16()),
            7 => drop(r.f32()),
            8 => drop(r.skip(*op as usize * 7919)),
            9 => {
                if let Some(v) = r.packed_u16() {
                    assert_eq!(Reader::new(&Writer::new().packed_u16(v).0).packed_u16(), Some(v));
                }
            }
            10 => {
                if let Some(v) = r.packed_u32() {
                    assert_eq!(Reader::new(&Writer::new().packed_u32(v).0).packed_u32(), Some(v));
                }
            }
            11 => {
                if let Some(v) = r.packed_u64() {
                    assert_eq!(Reader::new(&Writer::new().packed_u64(v).0).packed_u64(), Some(v));
                }
            }
            12 => {
                if let Some(v) = r.packed_i32() {
                    assert_eq!(Reader::new(&Writer::new().packed_i32(v).0).packed_i32(), Some(v));
                }
                if let Some(v) = r.packed_i16() {
                    assert_eq!(Reader::new(&Writer::new().packed_i16(v).0).packed_i16(), Some(v));
                }
            }
            _ => drop(r.string()),
        }
        assert!(r.pos <= r.data.len());
        let _ = r.rest();
    }
}
