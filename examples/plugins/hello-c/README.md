# hello-c

The plugin interface of the Planet Crafter dedicated server, from C. The interface is plain
WebAssembly (a few imports and exports, and a byte format for events), so any language that can make a
module with those works. This plugin is the proof, and a starting point: no libc, no WASI, about 90 lines
of plugin and a header (`pcds.h`) that you copy into your own plugin.

## For a player

- `!hello` answers `Hello Ann from C! I have said hello 1 time.` and counts: the counter is for the whole
  world, all players together, and it survives restarts and hot updates (`... 2 times.`, `... 3 times.`).
- `!roll` answers `Ann rolled a 4.` A number from 1 to 6.

Both commands take no arguments: the host answers `Use: !roll` to anyone who adds some, without calling
the plugin.

## For the admin

```bash
make build            # plugin.wasm from plugin.c and pcds.h
make check            # pcds plugin check .
make scenario         # pcds plugin test .
pcds plugin install .
pcds plugin enable hello-c
```

Capabilities it asks for:

| Capability | Why |
|---|---|
| `event.chat` | the commands |
| `event.player` | to hear `player_join` (the join stirs the dice) |
| `event.tick` | to hear a `tick` every 10 seconds of game time (`tick_every_ms = 10000`; it stirs the dice again) |
| `chat.say` | to answer |

No settings. Budget: `fuel_per_event = 100000`.

Cost, from `pcds plugin check` and `pcds plugin test`: a tick or a join about 1,000 fuel; a command about
1,300 to 2,100 fuel.

## Exact behaviour, for tests

- Manifest: commands `hello` and `roll`, both `max_args = 0`; events `player_join` and `tick`; state version 1.
- `!hello` is a `tell` to the asker: `Hello <name> from C! I have said hello <n> time.` for n = 1 and
  `... <n> times.` otherwise.
- `!roll` is a `tell` to the asker: `<name> rolled a <1..6>.` The number is never 0 or 7.
- The state is 4 bytes, the counter as a little-endian `u32`, version 1. A state of another version is
  ignored with a log line (`unknown state version, starting empty`); the counter starts at 0.
- The dice are not saved: after a restart the generator starts from the same fixed seed and is stirred
  again by the joins and ticks that follow.

## What it teaches

1. **A module has no clock and no randomness.** There is no `rand`, no `time`. What a plugin may use is what
   the events carry. Here the generator is a 64-bit xorshift (`roll()`), stirred (`stir()`) with the
   `player_id` and `client` of every join, with the game time of every tick, and with the `client` of the
   asker at each `!roll`. It is good enough for a game; do not use it for anything that matters.
2. **State is bytes you define, with a version.** `on_save` writes 4 bytes and says version 1; `on_load` gets the
   bytes and the version and reads them with a reader that cannot read past the end.
3. **Events are bytes too.** `pcds.h` reads the payload of every event kind into one struct, `pcds_ev`, and
   checks every read. A payload that ends too soon is refused (the function returns -1, which the
   server counts as a fault); an event kind the header does not know is a newer server's and gets 0.
4. **The import you do not call is not in the module.** `pcds.h` declares every import of ABI 1.3; the
   module only imports `log` and `tell`, because only those are called. The server refuses a module that
   imports anything it does not provide, so this matters.

## Using pcds.h in your own plugin

Copy `pcds.h` next to your `plugin.c`, `#include "pcds.h"` once, and define:

```c
static void on_load(const u8 *state, u32 len, u32 version);   /* state is 0 when nothing was saved */
static i32  on_event(const pcds_ev *ev);                      /* 0 to go on; 1 to veto (rpc_before only) */
static u32  on_save(u8 *out, u32 cap, u32 *version);          /* bytes written (at most cap); 0: nothing */
```

What the header gives you:

| | |
|---|---|
| Imports (module `pcds`) | `pcds_log`, `pcds_say`, `pcds_tell`, `pcds_kick`, `pcds_teleport`, `pcds_rules_patch`, `pcds_give`, `pcds_tokens`, `pcds_timer_set`, `pcds_timer_cancel`, `pcds_query` (each documented in the header, with its capability). `http_request` (`net.http`) is not wrapped. |
| Exports | `pcds_abi`, `pcds_alloc`, `pcds_free`, `pcds_event`, `pcds_load`, `pcds_save`, `pcds_state_version`. `memory` is exported by the linker. `pcds_unload` (optional) is not defined; add it the same way. |
| Allocator | a stack over a static arena of 256 KiB (`-DPCDS_ARENA=...` to change). The server asks for a buffer for each event and returns it after. |
| Events | `pcds_decode` reads kinds 1 to 14 into `pcds_ev` (the table at its definition says which field each kind sets). Short payload: -1. Unknown kind: 0. A veto (return 1) counts only for `PCDS_RPC_BEFORE`. |
| Reading | `pcds_reader` with `pcds_u8`, `pcds_u32`, `pcds_u64`, `pcds_i32`, `pcds_i64`, `pcds_f32`, `pcds_string` (all checked), `pcds_arg(ev, i)` for a command's arguments, `pcds_str_is` to compare a string with a literal |
| Writing | `pcds_text` (a fixed buffer that drops what does not fit) with `pcds_put`, `pcds_puts`, `pcds_putu`, `pcds_puti` |

The header is MIT OR Apache-2.0, like the Rust crate. It is the only file the plugin needs besides its own
code.

## Build

```bash
clang --target=wasm32 -nostdlib -O2 -Wall -Wextra -Wl,--no-entry -o plugin.wasm plugin.c
```

`plugin.wasm` in this folder was built with **clang 23.1.1 (LLD 23.1.1, x86_64-pc-linux-gnu)** and is 3,925
bytes. **The bytes of the module depend on the compiler's version** (and on its flags), so another clang makes
another file with the same behaviour. For this reason the repository's check that every example's
`plugin.wasm` equals what its source builds (`scripts/build-plugin-examples --check`) covers the Rust
plugins only and does not compare this module byte for byte. Both `plugin.c` and `pcds.h` compile with no
warning under `-Wall -Wextra`.

## Limits and known gaps

- Only the commands, the join and the tick are handled; `on_event` ignores everything else.
- The dice are shared by all players and are not meant to be unpredictable; a player who knows the code
  and watches the chat could work out the generator's state.
- There is no `pcds_unload` and no `http_request` wrapper (the Rust crate has both).
- `scenarios/basic.toml` is the test. A native `make test` was not added: the scenario runs the module in
  the real host. (The header's reading of every event kind was also fuzzed natively under AddressSanitizer
  while writing it; that test is not kept.)
