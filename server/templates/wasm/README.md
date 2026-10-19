# {id}

A plugin for the Planet Crafter dedicated server.

```bash
rustup target add wasm32-unknown-unknown   # once
cargo build --release                      # builds target/wasm32-unknown-unknown/release/{crate}.wasm
pcds plugin check .                        # checks it and runs it on sample events
pcds plugin install .                      # copies it to the server's run/plugins/
pcds plugin enable {id}                    # shows what it asks for; you approve it
```

Then type `!hello` in the game's chat. The guide: docs/plugins-guide.md in the server's repository;
the reference: docs/plugins-api.md.
