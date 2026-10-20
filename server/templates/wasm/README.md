# {id}

A plugin for the Planet Crafter dedicated server, written in Rust.

```bash
rustup target add wasm32-unknown-unknown   # once
make build                                 # cargo build --target wasm32-unknown-unknown --release, then plugin.wasm
make test                                  # cargo test: your code against a mock server, on this computer
make scenario                              # pcds plugin test .: the module in the real host (scenarios/ or scenario.toml)
make check                                 # pcds plugin check .: what it asks for, what each event costs
make install                               # copies it to the server's run/plugins/
pcds plugin enable {id}                    # shows what it asks for; you approve it
```

Without `make`: `cargo build --target wasm32-unknown-unknown --release`, then copy
`target/wasm32-unknown-unknown/release/{crate}.wasm` to `plugin.wasm`. `just` works with the justfile.

Then type `!hello` in the game's chat.

- `plugin.toml` is the manifest: what the plugin is, what it may do, its commands and settings.
- `src/lib.rs` is the plugin. `load` starts it, `event` is called for what the manifest subscribed to,
  `save` is what survives restarts and hot updates.
- Edit, `make build`, and `pcds plugin watch . --approve` installs every new build on your test
  server and keeps the plugin's state.
- Ready to share: `pcds plugin keygen NAME`, `pcds plugin sign --key NAME.key .`, `make package`.

The guide: docs/plugins-guide.md in the server's repository; the reference: docs/plugins-api.md.
