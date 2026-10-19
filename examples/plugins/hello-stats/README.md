# hello-stats

A WebAssembly plugin written in Rust: `!stats` and `!uses` in the chat. Read
`docs/plugins-guide.md` for how it is made, step by step.

```bash
../../../scripts/build-plugin-examples   # builds plugin.wasm (needs the wasm32-unknown-unknown target)
pcds plugin check .
pcds plugin install .
pcds plugin enable hello-stats
```
