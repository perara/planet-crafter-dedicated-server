use std::time::Instant;
use wasmtime::*;

fn best<F: FnMut()>(rounds: u32, n: u64, mut f: F) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..rounds {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed().as_nanos() as f64 / n as f64);
    }
    best
}

fn main() {
    if let Ok(file) = std::env::var("COMPILE_ONLY") {
        let bytes = std::fs::read(file).unwrap();
        for fuel in [true] {
            let mut cfg = Config::new();
            cfg.consume_fuel(fuel);
            let engine = Engine::new(&cfg).unwrap();
            let t = Instant::now();
            for _ in 0..10 { Module::new(&engine, &bytes).unwrap(); }
            println!("wasmtime (cranelift) compile of {} bytes: {:?} each", bytes.len(), t.elapsed() / 10);
        }
        return;
    }
    let path = std::env::args().nth(1).unwrap_or("../engine-bench/guest/guest.wasm".into());
    let bytes = std::fs::read(path).unwrap();
    for (fuel, epoch) in [(false, false), (true, false), (false, true)] {
        let mut cfg = Config::new();
        cfg.consume_fuel(fuel);
        cfg.epoch_interruption(epoch);
        let t = Instant::now();
        let engine = Engine::new(&cfg).unwrap();
        let engine_us = t.elapsed().as_micros();
        let t = Instant::now();
        let mut module = None;
        for _ in 0..20 { module = Some(Module::new(&engine, &bytes).unwrap()); }
        let compile_us = t.elapsed().as_micros() as f64 / 20.0;
        let module = module.unwrap();
        let mut linker = <Linker<u32>>::new(&engine);
        linker.func_wrap("host", "now", |c: Caller<'_, u32>| -> i32 { *c.data() as i32 }).unwrap();
        linker.func_wrap("host", "say", |_c: Caller<'_, u32>, _p: i32, _l: i32| {}).unwrap();
        let pre = linker.instantiate_pre(&module).unwrap();
        let t = Instant::now();
        for _ in 0..200 {
            let mut s = Store::new(&engine, 0u32);
            if fuel { s.set_fuel(1 << 40).unwrap(); }
            if epoch { s.set_epoch_deadline(1 << 40); }
            pre.instantiate(&mut s).unwrap();
        }
        let inst_us = t.elapsed().as_micros() as f64 / 200.0;
        let mut store = Store::new(&engine, 0u32);
        if epoch { store.set_epoch_deadline(1 << 40); }
        let instance = pre.instantiate(&mut store).unwrap();
        let noop = instance.get_typed_func::<(), i32>(&mut store, "noop").unwrap();
        let ping = instance.get_typed_func::<(), i32>(&mut store, "ping").unwrap();
        let on_event = instance.get_typed_func::<(i32, i32), i32>(&mut store, "on_event").unwrap();
        let spin = instance.get_typed_func::<i32, i32>(&mut store, "spin").unwrap();
        let n = 200_000u64;
        let w0 = best(5, n, || { for _ in 0..n { if fuel { store.set_fuel(1_000_000).unwrap(); } noop.call(&mut store, ()).unwrap(); } });
        let w1 = best(5, n, || { for _ in 0..n { if fuel { store.set_fuel(1_000_000).unwrap(); } ping.call(&mut store, ()).unwrap(); } });
        let w2 = best(5, n, || { for i in 0..n { if fuel { store.set_fuel(1_000_000).unwrap(); } on_event.call(&mut store, (i as i32 & 15, 64)).unwrap(); } });
        let m = 20_000u64;
        let w3 = best(5, m, || { for _ in 0..m { if fuel { store.set_fuel(10_000_000).unwrap(); } spin.call(&mut store, 10_000).unwrap(); } });
        println!("wasmtime fuel={fuel} epoch={epoch}: engine {engine_us} us, compile {compile_us:.0} us, new store+instance {inst_us:.0} us | call ns: noop {w0:.0}, host-roundtrip {w1:.0}, event(64B) {w2:.0}, spin(10k iters) {w3:.0}");
    }
}
