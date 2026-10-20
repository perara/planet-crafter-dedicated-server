# scaling-rules

A data plugin that changes the server's speed rules by the number of players online. Three bands
(`[[when_players]]` with `at_least` and `at_most`):

| Players | `speed.growth` | `speed.machines` |
|---|---|---|
| 1 to 2 | 2.0 | 1.5 |
| 3 to 5 | 1.5 | 1.25 |
| 6 and more | 1.0 | 1.0 |

The rules change once when the band changes (not every tick), through the same checked path as
`pcds rules set`, so the change is in the log (`plugin scaling-rules: changed the rules`) and in the
event stream (`rule_change`). `!tier` tells a player what the world is doing.

## What the admin approves

`rules.write` (it changes the `speed` rules: nothing else: the host refuses any other section for a plugin),
`world.read` (for `{players}` in the `!tier` text), `chat.say`, `event.chat` and `event.tick`.

## Things to know

- The plugin owns `speed.growth` and `speed.machines` while it runs: a value you set by hand is put back at
  the next change of band. Take other rules out of its way by not naming them here.
- With nobody online (0 players) no band matches and the rules stay as the last band left them. With
  `world.pauseWhenEmpty` the world stands still then anyway.
- Edit the numbers in `rules.toml`, `pcds plugin test .` walks the bands, `pcds plugin install . --force`.
