# playtime-board

A playtime leaderboard kept in the plugin's own state. It is also the example for **upgrading
saved state**: the state has two versions and `load` reads both.

## For a player

- `!top` shows the 5 players with the most time on the server, one chat line each, told to you
  only: `1. Ann 12h 05m`. `!top 3` shows 3; the most is 10. `!top` with something that is not a
  number answers `Use: !top [number from 1 to 10]`. With nobody on the board: `Nobody has played yet.`
- `!playtime` tells you your own time and rank: `You have played 3h 00m (rank 1 of 57).`
- `!playtime Bo` tells the time of a player online or on the board: `Bo: 2h 00m played, last seen 1h ago.`
  or `Ann: 3h 00m played (online now).` Capitals do not matter; a name that begins with what you
  typed is found if only one player's does (`!playtime an` finds Ann). Last seen reads `just now`,
  `5 min ago`, `3h ago`, `2d ago`, or `unknown` for a player saved by version 1 who has not been
  seen since. Answers for a name nobody has: `No player named X is online or on the board.`, and for
  several: `Several players match: A, B. Type more of the name.`
- Time is game time. A player is credited at every tick (5 seconds) while online, and nothing
  while the world stands still. A visit shorter than one tick is not counted.

## For the admin

Setting: `board_size` (`pcds plugin config set playtime-board board_size 500`), a whole number
from 10 to 2000, default 200. At most that many players are kept. When it is full, the player with
the least time who is not online is dropped (a tie: the one seen longest ago); players online are
never dropped. At most 3 are dropped at each tick, so a board that was made smaller, or a crowd of
newcomers, takes a while to settle: from 2000 players down to 200 takes 50 minutes (600 ticks).

No other setting. Capabilities: `event.tick` (the time), `event.player` (names, last seen),
`event.chat` (the two commands), `world.read` (who is online, `standing_still`), `chat.say`.

## State, and the upgrade story

The bytes the plugin saves have a version; `load` is given the version of the bytes it is handed.

| Version | Written by | Layout |
|---|---|---|
| 1 | plugin 0.1 | a count, then (player_id u64, name, minutes u32) for each player |
| 2 | plugin 0.2 (this) | a count, then (player_id u64, name, seconds u32, last_seen_ms i64) for each player |

`load` reads version 1 and turns each player's minutes into seconds, with "last seen" unknown (0).
The next `save` writes version 2. A version newer than 2 is not understood: the plugin starts with an
empty board and writes a Warn line (`state version N is newer than this plugin knows ...`) instead of
guessing. A damaged tail is dropped, what was read before it is kept. The test
`version_1_state_loads_and_is_saved_as_version_2` builds real version-1 bytes by hand, byte for byte,
loads them, and checks the board and the next save.

## How it is built, and what fuel taught

- The board is a `Vec` sorted by `player_id`, found by binary search. A first version used a
  `BTreeMap`; walking 2000 players in WebAssembly cost enough fuel to trap a tick with a full board.
- Name search (`!playtime Bo`) turns a name down by length and first letter before it compares the rest,
  and does not allocate. The first version lower-cased every name and trapped at 2000 players.
- Dropping the least player needs a look at all of them, so it is done at most 3 times per tick.
- `fuel_per_event = 1500000`. With a full board of 2000 players whose names all start the same, a name
  search costs about 1.1 million fuel; the same on a board of 200 costs a tenth of it.
  `pcds plugin check` prints 135733 for a tick (the first call after a start costs 100000 more than the
  later ones), 5541 for a join, 4979 for a leave, 16220 and 81391 for the two commands. In a scenario
  with a full 2000 board a tick with a newcomer costs 290000 at most and `!top 10` 225000.
- `save` of a full board writes about 70 KB.

## Build and test

```bash
cargo test                                                    # 13 tests with pcds_plugin_api::testing
cargo clippy --all-targets -- -D warnings
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/playtime_board.wasm plugin.wasm   # or ../../../scripts/build-plugin-examples
pcds plugin check .
pcds plugin test .                                            # scenarios/board.toml, board-size.toml
pcds plugin install . && pcds plugin enable playtime-board
```

## What it teaches

- Keep per-player data under `player_id`; keep the last name next to it.
- Version your state, keep every reader, write only the newest. Test with real old bytes.
- Use `Tick::since_ms` for time and carry the milliseconds that do not make a second.
- Bound every collection and every loop; measure the worst case with the biggest state.
- Use `Tick::unix_ms` for "last seen": it is the only wall clock a plugin gets besides the world's.

## Limits and known gaps

- The scenario format cannot start a plugin from old saved bytes, so the upgrade is tested in `cargo test`
  and not in `pcds plugin test`.
- The clock is the tick's. Right after a start, before the first tick, "last seen" of a player who left
  reads from the world (`!playtime` asks for it), but a leave before the first tick is stamped unknown.
- A player who joins and leaves between two ticks is not counted.
- Names with a doubled space cannot be found by name (the chat collapses the spaces in what is typed).
- Capitals are ignored for the letters A to Z only; other letters must match exactly.
- The chat command prefix is not known to a plugin: the text `Use: !top ...` assumes `!`.

## Licence

MIT (see ../LICENSE).
