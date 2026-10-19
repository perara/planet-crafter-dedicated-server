# The engine comparison behind docs/plugins.md

Not part of the server's build (it pulls wasmtime: four minutes of compiling). Two small programs
that run the same three workloads (an empty call; an event with 64 bytes whose handler adds them
up; 10,000 steps of integer arithmetic) through each way of running a plugin:

```bash
cd tools/plugin-engine-bench/engine-bench
rustc --target wasm32-unknown-unknown -C opt-level=3 -C panic=abort --crate-type cdylib guest/guest.rs -o guest/guest.wasm
rustc -C opt-level=3 --crate-type cdylib guest/native.rs -o guest/libnative.so
cargo run --release                       # wasmi (with and without fuel), a native library, Rhai, a unix socket

cd ../engine-bench-wasmtime
cargo run --release                       # wasmtime (Cranelift), with fuel, with epochs, with neither
COMPILE_ONLY=../../../examples/plugins/hello-stats/plugin.wasm cargo run --release   # compile time of a real module
```

Each program prints nanoseconds per call (best of five rounds), how long a module takes to compile
and a new instance to make, and, for wasmi, how much work one unit of fuel is. The sizes in the
document are those of a program that only loads a module and calls it, built with
`--release` and stripped (`strip`), against the same without the engine.
