# quest-rewards

Small rewards for milestones, each given once and remembered for the player: items in the
backpack and Terra tokens for the world. It is the example for the **action API** (`give`,
`tokens`) and for keeping honest books when the host may say "not now".

## For a player

You get a reward the first time you reach each of these (the same reward each time):

| Milestone | Counted as |
|---|---|
| your first building | a building you placed while online |
| 10, 50 and 200 buildings placed | the same count |
| 1 hour of play | game time online (nothing counts while the world stands still) |
| 10 hours of play | the same count |

A reward is `item_count` of `item_group` into your backpack (default 10 Iron) and `token_amount`
Terra tokens added to the world (default 100). When both have been given you are told:
`Quest reward for 10 buildings placed: +10 Iron, +100 Terra tokens.` (The first one reads
`Quest reward for your first building: ...`, then `50 buildings placed`, `200 buildings placed`,
`1 hour of play`, `10 hours of play`.) With an item or token amount of 0 that part is left out of the text.

If your backpack has no free slot the items wait (the tokens are not held back) and you are told
nothing until they are given, at the first tick (5 seconds) after there is room.

`!rewards` tells you three lines:

```
You have placed 7 buildings and played 1h 05m. Milestones done: 2 of 6.
Next: 10 buildings placed (3 more), 10 hours of play (8h 55m more). Each gives +10 Iron, +100 Terra tokens.
World: terraformation 4200; the next milestone is 10000.
```

When rewards are earned but not given yet, the first line ends with ` 1 reward is waiting; it is given when the backpack has room.`
(or ` 2 rewards are waiting; they are given when the backpack has room.`). When everything is done the
second line is `All milestones are done.` and the third says `every milestone is reached.` when the world is past them all.

**World milestones.** When the world's terraformation first passes one of the numbers in
`world_milestones` (looked at every 30 seconds of game time), the world gets `world_token_amount` tokens
and everybody hears: `World milestone: terraformation passed 10000! The colony receives 500 Terra tokens.`
The tokens belong to the world, not to a player: "everyone online gets them" means the one shared count.
A milestone is paid once, ever, also on a world that is already past it when the plugin is first
installed (then each is paid at the checks that follow: at most 2 token changes a second go through).

## For the admin

Commands:

- `!rewards` (everyone), see above.
- `!givetokens <n>` (operators only; the host answers anyone else `!givetokens is for the server's operators.`)
  adds n Terra tokens, n from 1 to 10000. Reply `Added 250 Terra tokens.`. Anything else (0, negative,
  more than 10000, not a number, no argument) answers `Use: !givetokens <number from 1 to 10000>`; when the
  host refuses because of its rate: `Token changes are limited to 2 a second: try again in a moment.`

Settings (`pcds plugin config set quest-rewards KEY VALUE`; read again at every tick):

| Key | Default | Range | Meaning |
|---|---|---|---|
| `item_group` | `Iron` | up to 60 characters | the item's group id in the game's tables (Iron, Cobalt, ...); an empty value means Iron |
| `item_count` | 10 | 1 to 50 | items per personal milestone (the host takes 50 at most in one go) |
| `token_amount` | 100 | 0 to 10000 | tokens per personal milestone; 0: none |
| `world_token_amount` | 500 | 0 to 10000 | tokens per world milestone; 0: none (the announcement is still made) |
| `world_milestones` | `10000,100000,1000000,10000000` | up to 8 whole numbers | the terraformation values, separated by commas; a number already paid is not paid again, a new one is |

The scale of the terraformation number was not checked against the real game: change
`world_milestones` to fit it.

## Capabilities it asks for

- `event.object`: `object_placed` says which connection placed a building (`client` 0, a machine or the server, is ignored).
- `event.player`: joins and leaves keep the list of who is online, so a placement is attributed without
  asking the world (asking costs fuel for every player online).
- `event.tick`: play time, retrying rewards, the world check. `event.chat`: the two commands.
- `world.read`: who is online, `standing_still`, the terraformation, the backpack (`inventory`).
- `chat.say`: the texts. `player.give`: the items. `world.tokens`: the tokens.

## How the books are kept honest

- A part of a reward (the items, the tokens) is marked as given only when the host's call returned `Ok`.
  The state holds, for each player, the buildings placed, the seconds played and two bit masks (items
  given, tokens given), one bit for each milestone. What is owed is worked out again from these at every
  tick, so there is no queue to lose: a call that returns `Full` (16 actions in a call, 10 gives or 2
  token changes in a second) just stops that tick's payments and is tried again at the next.
- A `give` is asked of the backpack first (`inventory`): with no free slot the items wait instead of being
  counted as given and then refused. A player that is gone (`NotFound`) is left alone.
- A capability that is not granted (`Denied`) is logged once a minute at most and not tried again for 60 seconds.
  A call the host will never take (`Bad`: a bad item name) is logged and counted as given, so it does not repeat.
- **Known limit.** The host applies a `give` after the plugin has returned. If it refuses then (the
  player left in that instant, the backpack filled between the check and the apply, an unknown item
  name), the plugin is not told, and the reward stays counted as given. The same for tokens.
- At most 1000 players are kept (the one with the least progress who is not online is dropped, 3 at a tick at most).
  Someone dropped could earn the rewards again.
- Placing and removing the same building is not guarded against, but each milestone pays only once.

State (version 1): the paid world milestones (at most 32), then for each player the id, buildings placed,
seconds played and the two masks, 24 bytes each. A newer state version is ignored with a Warn line.

## Fuel

`fuel_per_event = 800000`. `pcds plugin check` prints 158116 for a tick (the first call after a start),
144949 and 12559 for the two commands and 20669 for a placed object. Measured in scenarios with 24
players: a tick 315000 on average and 410000 at most (reading the players costs about 6000 for each),
a placement 13000, `!rewards` 200000. With about 60 players online a tick would reach the budget.

## Build and test

```bash
cargo test                                                    # 17 tests with pcds_plugin_api::testing
cargo clippy --all-targets -- -D warnings
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/quest_rewards.wasm plugin.wasm   # or ../../../scripts/build-plugin-examples
pcds plugin check .
pcds plugin test .                                            # scenarios/milestones.toml, backpack.toml, operator.toml
pcds plugin install . && pcds plugin enable quest-rewards
```

A note on scenarios: `pcds plugin test` runs game time as fast as it can, but the host's rate limits count
real time. `milestones.toml` has a `reload` step before the hour of play for that reason: a fresh instance
starts the count of token changes again.

## What it teaches

- What `Error::Full` means and how to survive it without a queue: make payments a function of counters and masks.
- `give` and `tokens` are queued and may fail later; say what you promise.
- Check the backpack with `inventory` before a `give`.
- Settings of three kinds (a string, bounded ints, a list in a string) read with `Host::settings`.
- Commands with `permission = "operator"` and `min_args`/`max_args`.

## Limits and known gaps

- One reward for every personal milestone; there is no table of different rewards for different milestones.
- Buildings count when placed. A player who places and deconstructs the same building 200 times gets the 200 reward.
- A reward is counted for the player online when the milestone is reached. If their `give` is refused at apply time it is lost.
- The `world_milestones` default is a guess at the scale of the terraformation number.
- The chat command prefix is not known to a plugin: the text `Use: !givetokens ...` assumes `!`.

## Licence

MIT (see ../LICENSE).
