# server-motd

A data plugin: the message of the day, the rules, a command list and tips that rotate. `rules.toml` is
all there is; no code runs and nothing in it can loop.

## What players see

- When they join: three lines, told to them alone (a welcome with the number of players, where to find the
  commands, and the message of the day).
- `!motd`, `!rules`, `!help` and `!online` answer the asker (`!online` says the number to everyone, once in
  30 seconds). Each has a cooldown, so a player cannot flood the chat with them.
- Every 10 minutes one tip is said in the chat, the next one each time, round and round. Every 30 minutes
  the community link. Nothing is said while nobody is online (`min_players = 1`).

## What the admin does

1. Edit `rules.toml`: the texts at the top (`[vars]`, `[[on_join]]`, `[[command]]`) and the tips in the
   `say = [ ... ]` list.
2. `pcds plugin check .` and `pcds plugin test .` (the scenario in `scenarios/` walks through all of it).
3. `pcds plugin install . ` and `pcds plugin enable server-motd`. The plugin asks only for the right to read
   joins and chat, to be called every few seconds for the tips, and to say things in the chat.
4. A change to the texts is `pcds plugin install . --force` and a new approval (the files changed); with
   `pcds plugin watch . --approve` on a test server it is one step.

## What it shows

- A rotating announcement: `say` in `[[every]]` is one text or a list.
- `min_players` on `[[every]]`: no announcements to an empty server.
- Per-command cooldowns and `{name}`, `{players}`, `{planet}`, `{time}`.
- That a data plugin can be tested like any other: `pcds plugin test .`.

## Limits

A text is at most 2000 bytes (a chat line is 500 characters: longer is cut by the game). At most 64 commands,
16 join and leave entries and 64 `[[every]]` entries.
