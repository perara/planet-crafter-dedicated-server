#[unsafe(no_mangle)] pub extern "C" fn noop() -> i32 { 0 }
static mut BUF: [u8; 256] = [0; 256];
#[unsafe(no_mangle)] pub extern "C" fn inbox() -> *mut u8 { core::ptr::addr_of_mut!(BUF) as *mut u8 }
#[unsafe(no_mangle)] pub extern "C" fn on_event(kind: i32, len: i32, say: extern "C" fn(*const u8, i32)) -> i32 {
    let b = unsafe { &*core::ptr::addr_of!(BUF) };
    let mut h: u32 = kind as u32;
    for i in 0..(len as usize).min(64) { h = h.wrapping_mul(31).wrapping_add(b[i] as u32); }
    if h % 100 == 7 { say(b.as_ptr(), 8) }
    h as i32
}
#[unsafe(no_mangle)] pub extern "C" fn spin(n: i32) -> i32 {
    let mut x: u32 = 1;
    for i in 0..n as u32 { x = x.wrapping_mul(1664525).wrapping_add(1013904223 ^ i); }
    x as i32
}
