use std::time::Instant;

fn best<F: FnMut()>(rounds: u32, n: u64, mut f: F) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..rounds {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed().as_nanos() as f64 / n as f64);
    }
    best
}

fn wasmi_bench(path: &str) {
    use wasmi::*;
    for fuel in [false, true] {
        let mut cfg = Config::default();
        cfg.consume_fuel(fuel);
        let engine = Engine::new(&cfg);
        let bytes = std::fs::read(path).unwrap();
        let t = Instant::now();
        let mut module = None;
        for _ in 0..50 { module = Some(Module::new(&engine, &bytes).unwrap()); }
        let compile_us = t.elapsed().as_micros() as f64 / 50.0;
        let module = module.unwrap();
        let mut store = Store::new(&engine, 0u32);
        let mut linker = <Linker<u32>>::new(&engine);
        linker.func_wrap("host", "now", |c: Caller<'_, u32>| -> i32 { *c.data() as i32 }).unwrap();
        linker.func_wrap("host", "say", |_c: Caller<'_, u32>, _p: i32, _l: i32| {}).unwrap();
        if fuel { store.set_fuel(u64::MAX / 2).unwrap(); }
        let t = Instant::now();
        let mut inst = None;
        for _ in 0..50 {
            let mut s = Store::new(&engine, 0u32);
            if fuel { s.set_fuel(1 << 40).unwrap(); }
            inst = Some(linker.instantiate_and_start(&mut s, &module).unwrap());
        }
        let _ = inst;
        let inst_us = t.elapsed().as_micros() as f64 / 50.0;
        let instance = linker.instantiate_and_start(&mut store, &module).unwrap();
        let noop = instance.get_typed_func::<(), i32>(&store, "noop").unwrap();
        let ping = instance.get_typed_func::<(), i32>(&store, "ping").unwrap();
        let on_event = instance.get_typed_func::<(i32, i32), i32>(&store, "on_event").unwrap();
        let spin = instance.get_typed_func::<i32, i32>(&store, "spin").unwrap();
        let n = 200_000u64;
        let w0 = best(5, n, || { for _ in 0..n { if fuel { store.set_fuel(1_000_000).unwrap(); } noop.call(&mut store, ()).unwrap(); } });
        let w1 = best(5, n, || { for _ in 0..n { if fuel { store.set_fuel(1_000_000).unwrap(); } ping.call(&mut store, ()).unwrap(); } });
        let w2 = best(5, n, || { for i in 0..n { if fuel { store.set_fuel(1_000_000).unwrap(); } on_event.call(&mut store, (i as i32 & 15, 64)).unwrap(); } });
        if fuel {
            store.set_fuel(100_000_000).unwrap();
            let t = Instant::now();
            spin.call(&mut store, 1_000_000).unwrap();
            let used = 100_000_000 - store.get_fuel().unwrap();
            let us = t.elapsed().as_micros();
            println!("fuel calibration: spin(1M iters) used {used} fuel in {us} us = {:.0} fuel/us", used as f64 / us as f64);
            let t = Instant::now();
            store.set_fuel(100_000_000).unwrap();
            for _ in 0..100000 { noop.call(&mut store, ()).unwrap(); }
            let used = 100_000_000 - store.get_fuel().unwrap();
            println!("noop call: {} fuel each ({:.0} ns)", used as f64 / 100000.0, t.elapsed().as_nanos() as f64 / 100000.0);
        }
        let m = 2_000u64;
        let w3 = best(5, m, || { for _ in 0..m { if fuel { store.set_fuel(10_000_000).unwrap(); } spin.call(&mut store, 10_000).unwrap(); } });
        println!("wasmi fuel={fuel}: compile {compile_us:.0} us, new store+instance {inst_us:.0} us | call ns: noop {w0:.0}, host-roundtrip {w1:.0}, event(64B) {w2:.0}, spin(10k iters) {w3:.0}");
    }
}

fn native_bench(path: &str) {
    unsafe {
        let lib = libloading::Library::new(path).unwrap();
        let noop: libloading::Symbol<extern "C" fn() -> i32> = lib.get(b"noop").unwrap();
        let on_event: libloading::Symbol<extern "C" fn(i32, i32, extern "C" fn(*const u8, i32)) -> i32> = lib.get(b"on_event").unwrap();
        let spin: libloading::Symbol<extern "C" fn(i32) -> i32> = lib.get(b"spin").unwrap();
        extern "C" fn say(_: *const u8, _: i32) {}
        let n = 2_000_000u64;
        let w0 = best(5, n, || { for _ in 0..n { std::hint::black_box(noop()); } });
        let w2 = best(5, n, || { for i in 0..n { std::hint::black_box(on_event(i as i32 & 15, 64, say)); } });
        let m = 20_000u64;
        let w3 = best(5, m, || { for _ in 0..m { std::hint::black_box(spin(std::hint::black_box(10_000))); } });
        println!("native dylib (libloading): noop {w0:.1} ns, event {w2:.1} ns, spin(10k) {w3:.0} ns");
    }
}

fn rhai_bench() {
    let engine = rhai::Engine::new();
    let t = Instant::now();
    let mut ast = None;
    for _ in 0..200 {
        ast = Some(engine.compile(r#"
            fn on_event(kind, len) {
                let h = kind;
                for i in 0..min(len, 64) { h = (h * 31 + i) % 4294967296; }
                h
            }
            fn spin(n) { let x = 1; for i in 0..n { x = (x * 1664525 + 1013904223) % 4294967296; } x }
            fn noop() { 0 }
        "#).unwrap());
    }
    let compile_us = t.elapsed().as_micros() as f64 / 200.0;
    let ast = ast.unwrap();
    let mut scope = rhai::Scope::new();
    let n = 100_000u64;
    let w0 = best(3, n, || { for _ in 0..n { engine.call_fn::<i64>(&mut scope, &ast, "noop", ()).unwrap(); } });
    let w2 = best(3, n, || { for i in 0..n { engine.call_fn::<i64>(&mut scope, &ast, "on_event", (i as i64 & 15, 64i64)).unwrap(); } });
    let m = 100u64;
    let w3 = best(3, m, || { for _ in 0..m { engine.call_fn::<i64>(&mut scope, &ast, "spin", (10_000i64,)).unwrap(); } });
    println!("rhai: compile {compile_us:.0} us | call ns: noop {w0:.0}, event(64B loop) {w2:.0}, spin(10k iters) {w3:.0}");
}

fn socket_bench() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let (mut a, mut b) = UnixStream::pair().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = match b.read(&mut buf) { Ok(0) | Err(_) => return, Ok(n) => n };
            if b.write_all(&buf[..n.min(40)]).is_err() { return; }
        }
    });
    let ev = br#"{"event":"chat","id":12,"client":3,"name":"Ann","text":"!tp home"}"#;
    let mut out = [0u8; 4096];
    let n = 20_000u64;
    let rt = best(3, n, || { for _ in 0..n { a.write_all(ev).unwrap(); let mut got = 0; while got < 40 { got += a.read(&mut out[got..]).unwrap(); } } });
    println!("out-of-process unix socket round trip (70 B request, 40 B answer, echo thread): {rt:.0} ns ({:.1} us)", rt / 1000.0);
    // one-way: write only, a reader drains
    let (mut a, mut b) = UnixStream::pair().unwrap();
    std::thread::spawn(move || { let mut buf = [0u8; 65536]; while b.read(&mut buf).map(|n| n > 0).unwrap_or(false) {} });
    let n = 200_000u64;
    let ow = best(3, n, || { for _ in 0..n { a.write_all(ev).unwrap(); } });
    println!("out-of-process one-way event write (no answer awaited): {ow:.0} ns");
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let wasm = a.get(1).map(String::as_str).unwrap_or("guest/guest.wasm");
    wasmi_bench(wasm);
    native_bench(a.get(2).map(String::as_str).unwrap_or("guest/libnative.so"));
    rhai_bench();
    socket_bench();
}
