# {id}

A plugin for the Planet Crafter dedicated server, written in C. The plugin interface is plain
WebAssembly, so any language that makes a module with the right imports and exports works; this is
the proof, and a starting point.

```bash
make build        # clang --target=wasm32 -nostdlib -O2 ... -o plugin.wasm
make scenario     # pcds plugin test .: the module in the real host, as scenarios/basic.toml says
make check        # pcds plugin check .
make install      # copies it to the server's run/plugins/
pcds plugin enable {id}
```

`pcds.h` declares what the server provides and exports what it needs (the allocator, the events);
`plugin.c` is the plugin. You need `clang` with the wasm32 target and `wasm-ld`; no libc is used.
The whole contract is the table in docs/plugins-api.md, so you can also write the exports by hand
in Zig, AssemblyScript, TinyGo or anything else.
