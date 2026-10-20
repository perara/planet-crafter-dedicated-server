# {id}

A plugin for the Planet Crafter dedicated server, written in C. The plugin interface is plain
WebAssembly, so any language that makes a module with the right imports and exports works; this is
the proof, and a starting point.

```bash
make build        # clang --target=wasm32 -nostdlib -O2 -Wall -Wextra ... -o plugin.wasm
make scenario     # pcds plugin test .: the module in the real host, as scenarios/basic.toml says
make check        # pcds plugin check .
make install      # copies it to the server's run/plugins/
pcds plugin enable {id}
```

`plugin.c` is the plugin: it defines three functions, `on_load` (the saved state, once), `on_event` (every
event the manifest asks for) and `on_save` (the state to keep). `pcds.h` is the rest, and the file you copy
into your own plugins: it declares everything the server provides (`pcds_say`, `pcds_tell`, `pcds_kick`,
`pcds_teleport`, `pcds_give`, `pcds_timer_set`, `pcds_query` and the others, each with the capability it
needs), exports what the server needs (the allocator, the events, the state), and reads the bytes of every
event kind into one struct, `pcds_ev`, refusing a payload that ends too soon. The imports your plugin does
not call are not in the module.

You need `clang` with the wasm32 target and `wasm-ld`; no libc is used. The bytes of the module depend on
the clang version. The whole contract is the table in docs/plugins-api.md, so you can also write the
exports by hand in Zig, AssemblyScript, TinyGo or anything else.

A fuller example, with a die and a tick, is `examples/plugins/hello-c/` in the server's repository.
