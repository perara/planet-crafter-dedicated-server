// Bench guest: the same three workloads as bench_native.rs, as a no_std wasm module.
#![no_std]
use core::panic::PanicInfo;
#[panic_handler] fn panic(_: &PanicInfo) -> ! { core::arch::wasm32::unreachable() }

#[link(wasm_import_module = "host")]
unsafe extern "C" { fn now() -> i32; fn say(ptr: i32, len: i32); }

static mut BUF: [u8; 256] = [0; 256];
#[unsafe(no_mangle)] pub extern "C" fn inbox() -> i32 { core::ptr::addr_of!(BUF) as i32 }
// W0: nothing
#[unsafe(no_mangle)] pub extern "C" fn noop() -> i32 { 0 }
// W1: one call back into the host
#[unsafe(no_mangle)] pub extern "C" fn ping() -> i32 { unsafe { now() } }
// W2: a typical event: look at 64 payload bytes, sometimes answer
#[unsafe(no_mangle)] pub extern "C" fn on_event(kind: i32, len: i32) -> i32 {
    let b = unsafe { &*core::ptr::addr_of!(BUF) };
    let mut h: u32 = kind as u32;
    for i in 0..(len as usize).min(64) { h = h.wrapping_mul(31).wrapping_add(b[i] as u32); }
    if h % 100 == 7 { unsafe { say(inbox(), 8) } }
    h as i32
}
// W3: compute
#[unsafe(no_mangle)] pub extern "C" fn spin(n: i32) -> i32 {
    let mut x: u32 = 1;
    for i in 0..n as u32 { x = x.wrapping_mul(1664525).wrapping_add(1013904223 ^ i); }
    x as i32
}
