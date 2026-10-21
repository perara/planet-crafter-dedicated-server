# The Codex branches on top of 0.8.1

Between 0.8.0 and 0.8.1 the owner's Codex sessions worked on their own branches, made from 0.8.0
(`c5ca29e`): eleven commits, all contained in `codex/server-second-pass-20261021`. Main moved by 104
commits in the same days (the operator's work, the performance and memory work, the acceptance and soak
fixes). This page says what each Codex commit meant, what became of it on top of 0.8.1, and what was
decided on the way. The commit hashes of this tree are in the history: `git log --grep "cherry picked"`
finds each Codex commit by its old hash (`git cherry-pick -x` wrote it into the message).

The rule for every decision below: the game is the reference. A change that makes the server do what
the game does not is either a rule that is off by default (everything drones do: their order, their
flights, their lost cargo), or a fix of something that leaves a player's game stale or loses a player's
item outside the drones, written down here with the game's own code next to it, so that it can be taken
out again (each is a commit of its own). The owner decides which of the second kind stay (see the end).

## What is where

| Codex commit | Subject | Here | What became of it |
|---|---|---|---|
| `b1ebe49` | Check full rockets when automatic launch is enabled | `f868403` | taken as it was |
| `5701dd0` (branch `diagnose-trade-autolaunch`, not in the tip) | Reproduce enabling automatic trade launch after filling the rocket | `6913017`, `5527932` | taken; one test reworded |
| `8d1b51e` | Reproduce six state transition issues | `58c5831` | taken (the tests move into their modules in the next one) |
| `6b8e3cc` | Preserve cargo and synchronize machine state across transitions | `fb9bf4d`, `5527932` | taken, adapted to the compact machine parts |
| `97de8cb` | Refresh remote player map markers | `911b9b4` | taken |
| `c4f16fd` | Improve drone dispatch and add optional aerial obstacle clearance | `7a29ad0`, `e2c90b4` | taken, then put behind two rules (`drones.nearestFirst`, `drones.avoidObstacles`), off |
| `f5cc0bf` | Read signed Unity terrain heightmaps | `dd5aa1d` | taken (behind `drones.avoidObstacles`) |
| `d4b1251` | Spread route planning across ticks | `ab2c124` | taken (behind `drones.avoidObstacles`) |
| `f563984` | Reuse static terrain indexes | `bd39943` | taken (behind `drones.avoidObstacles`) |
| `8f81461` | Clear overhead buildings when terrain raises a route | `1fb2108` | taken (behind `drones.avoidObstacles`) |
| `78d739e` | Cheaper clearance queries; ordinary meteor showers | `65f8098` | taken; the meteor test needs nothing from the drone rules |
| `4c55791` | Reduce static table and world state memory | `e1e5671`, `f03c470` | not taken as a commit: main already has all of it; three tests and one idea were kept |

`5701dd0` is not part of the tip. Its four tests are diagnostics for the same bug as `b1ebe49` (it was
made first, to show the bug), and they cover more than the one test of `b1ebe49`: enabling after
filling, loading a full enabled rocket, and refilling. They are in. One of them (`refilling after a
toggle recovers`) assumed the rocket stays on the ground when the setting is switched on while it is
full, which is the bug; it now switches the setting on while the rocket is one item short.

## 1. Automatic launch when the rocket is already full (`b1ebe49`)

**What it fixes.** A trade rocket or an interplanetary exchange rocket with "launch when full" (setting
1) launches when the last item goes in. If the rocket was filled first and the setting switched on
after, nothing happened until an item was taken out and put back.

**Files.** `rockets/back_and_forth.rs` (`launch_if_full`, the one check that the item changes and the
launch of an enabled rocket already used), `handlers.rs` (`SettingProxy.SetSettingServerRpc` calls it).

**The game.** `SettingProxy.SetSetting` only sets the world object's setting;
`MachineRocketBackAndForth.OnRocketBackAndForthInventoryModified` runs on an inventory change and in
`OnEnable`. So the game does the same: this is a deliberate difference, and a harmless one (the player
asked for it and the rocket is full).

**Tests.** `rockets::tests::enabling_automatic_launch_checks_full_rockets` (seven cases: both rocket
kinds, 24 and 25 items, one in flight, setting off, a wrong destination, a destructor; a repeated enable
does not ignite a flying rocket again) and the four of `5701dd0`. On the late-game save a trade platform
of the world, filled and then switched on, launches (`bench::drones_on_a_late_world`).

**Main moved.** The departure-platform fix of main (`5e8ff44`, where a platform finds its rocket) is in
the same file family and does not touch this code; no conflict. The compact machine parts did not touch
it either.

## 2. The six state transitions (`8d1b51e`, `6b8e3cc`)

`8d1b51e` is six failing tests, one for each finding of the audit; `6b8e3cc` is the fix of all six, with
the tests moved next to the code they test. Each is a place where the game does something that loses an
item or leaves a client's view stale. Each is a deliberate difference from the game; the sixth, which is a
drone's, is a rule that is off, the other five are on.

| # | The finding | What the game does (its code) | What the server does now | Test |
|---|---|---|---|---|
| 1 | Something put in a building while it is being deconstructed is destroyed with it | `GroupNetworkBase.DeconstructServerRpc` runs `CheckDeconstructOnServer` once, at the start; `FinalDeconstructServer` (after the animation) does not check again | the check is made again when the animation ends; if it fails the player gets the "not empty" message, nothing is refunded, the building stays, and its neighbouring corridor panels stay as they were (they are changed only when the deconstruction succeeds, not when it starts) | `deconstruct::tests::deconstruction_rechecks_contents_before_completion`, `panels::tests` |
| 2 | A rocket platform, generator or grower built after an optimizer's fuse was put in is not boosted until the fuse is taken out and put back | `MachineOptimizer.OnConstructibleInstantiated` boosts a new building only if `WorldObject.HasUnitGeneration()`, which a rocket platform has not (and a grower with no unit has not either) | the building is boosted when it has unit generation, a generator, a grower, or is a rocket platform, after all its components exist | `machines::production::tests::new_rocket_is_boosted_without_reinserting_optimizer_fuse`, `newly_built_generators_and_growers_receive_existing_fuses` |
| 3 | A joining client does not hear the growth boost of the growers | the game sends `SetGrowerSpeedMultiplier` when the boost changes, not to a client that joins later | a joining client is told each grower's current boost (the rule `speed.growth` and the fuses) | `machines::production::tests::joiners_receive_current_growth_speed_including_rules_and_fuses` |
| 4 | A destructor switched to "destroy when full" while it is full keeps its items until the next item | `MachineDestructInventoryIfFull.TryToCleanInventory` runs when the inventory is set and when it changes, not when the setting changes | switching it on checks at once | `machines::production::tests::enabling_full_destructor_clears_its_inventory` |
| 5 | Picking up, dropping or moving a saved object counts it again in the totals of objects of its group | `WorldObjectsHandler.UpdateWorldObjectSaveState` calls `AddToSpecificLists`, which adds one to `_allWorldObjectsGroups` each time | the count changes only when an object's save state really changes; replacing an object, restoring and deleting keep the totals right (`set_save_state` is the one place) | `objects::tests::drops_pickups_and_save_transitions_count_each_object_once`, `group_counts_balance_replacements_unsaved_buildings_and_restores` |
| 6 | A drone whose delivery fails loses its cargo | `Drone.EndAssignement` destroys the first item in the drone's inventory (the demand is full or gone: the item is deleted) | **behind the rule `drones.keepCargo`, off:** the cargo is dropped on the ground where the drone is, on the drone's planet (also while that planet is not the one that is loaded), and can be picked up again; off, it is destroyed as the game does | `logistics::tests::a_drone_preserves_cargo_if_destination_fills_during_flight`, `cancelled_drone_cargo_is_saved_on_its_own_planet`, `a_drone_whose_demand_goes_keeps_its_load_with_the_rule` (the rule on) and, for the rule off, `a_drone_loses_cargo_if_destination_fills_during_flight_as_in_the_game` and main's `a_drone_whose_demand_goes_loses_its_load`, which Codex had turned round: that test was written from the game's code (`FindDemandForDroneOrDestroyContent` looks for a demand, but `CreateNewTaskForWorldObject` refuses a drone's own inventory, so the load is always destroyed), the clearest sign that this is a decision and not a repair |

**Files.** `deconstruct.rs`, `machines.rs`, `machines/production.rs`, `hooks.rs`, `handlers.rs`,
`objects.rs` (`set_save_state`, `remove_from_count_lists`, `drop_cargo_on_floor`, the planet of a drop),
`logistics.rs`, `rockets/interplanetary.rs`, `inventories.rs`, `instances.rs`,
`animals/interpolation.rs`, `meteo/explode.rs`, `model.rs` (`has_unit_generation` goes), `panels.rs`.

**Main moved.** The machines are compact now (a `Machine` of 48 bytes with its parts boxed: `parts_mut()`
and `existing_parts_mut()` where the Codex tests wrote `.generator`). The tests were ported with that
and nothing else; the production code needed no change. `rules.limits.buildings` counts placed buildings
(not the group's count) as on main.

**What it does to the numbers.** On the bench world (`bench::bench`, four players, 300 s) the save is
identical; the messages are those of 0.8.1 except for the map markers of section 3. On the late-game
save with 45 drones (300 s, four players acting every 200 ms) the final save is identical to the one of
0.8.1 (hash `9a80f3ec94f29506`, every world object the same) with the rules off, and the messages differ
only in the network lists of the group counts (fix 5: 1,503 count messages of 0.8.1 against 1,283; the
server reads those counts nowhere, only the clients do). With `drones.keepCargo` on the world holds 79
more objects at the end of the five minutes: the cargo the game destroys (Algae1Seed 23, Iridium 19,
Dolomite 7 and others). The late-game bench's message fingerprint with the rules off is `a12ec1edd906593c`
(0.8.1: `10200a59a15ad951`); with the join refresh of section 3 switched off it is `d99a3275aa6a7254`; the
save is the same in all three.

## 3. Map markers of the other players (`97de8cb`)

**What it fixes.** The game's client makes a player's map marker only when that player's proxy starts
with the roster entry (the name) already there. A player who joined while another was still being set
up saw no marker or no name.

**What it does.** After a player's identity is known (`PlayerReady`), the server despawns and spawns the
other ready players' proxies for that player, and that player's proxy for the others, in an order that
puts the roster before the proxy. The owner's own player is never touched, and a peer that is still
loading is refreshed only when it is ready.

**Evidence and its limit.** The tests read the byte stream (the roster delta comes before the spawn, in
both join orders, owners keep their object, the roster has no duplicates after a leave). Nothing here
could be run against a real game client, so that the marker really appears is the Codex session's
finding and not checked again. The game's own host sends none of this, so it is a difference from the
game, in the join, and it moves the bench fingerprint of the bench world: 35 more messages for four
joiners (`15c299165b6b3eb0` becomes `879b6a2783052bbf`; with the refresh switched off the fingerprint
and the 45,558 messages of 0.8.1 come back, and the save hash is the same either way).

**Main moved.** `players.rs` and `net.rs` changed a lot (compact network objects); the code needed no
change.

## 4. Drones (`c4f16fd`, `f5cc0bf`, `d4b1251`, `f563984`, `8f81461`, `78d739e`)

### What the Codex work did

* Dispatch: a demand takes from the supply nearest to it; an idle drone takes the free job of the highest
  priority and the nearest pickup; the drone that is sent from a station is the one of the nearest
  station that has one, counted once instead of sorting all the stations for every job; a drone arrives
  by its own minimum distance; a drone with nothing to do on its planet no longer ends the round for the
  drones after it.
* Clearance: `pcds drone-map GAME_DIR TABLES_DIR` reads the colliders of the terrain (box, sphere,
  capsule, mesh bounds, Unity terrain heightmaps in conservative patches) and of the buildings from the
  game's files into `drone-navigation.json`; the drones then climb straight up, cross above every
  collider on their route and go straight down. Planning is bounded (16 drones per tick, rotating), the
  terrain index is made once, the buildings' index again after a build or a demolition, queries walk the
  crossed cells.

### What the game does, and what was decided

In the game none of this exists. `LogisticManager.GetANonAttributedTask` returns the first free task in
the order of a .NET dictionary; the stations are sorted by rounded distance with an unstable sort; the
round of idle drones ends (`break`) at the first drone with nothing to do on its own planet; every drone
arrives by the minimum distance of the drone that was added last (`_accessMinTargetDistance`); and
`DroneTransformUpdateJob` flies straight through everything (the game's `DroneColliding` only tints a
drone that passes through something). Where this server's order differs, the parity sweep and the
game's own behaviour on a late-game world differ, and an operator who wants the game's behaviour has no
way to get it. So these are rules, **off by default**, and with the rules off the game's code runs
unchanged (the game's versions are the functions that remain; the Codex ones are the `_nearest`
functions and the branches behind the rule):

* `drones.nearestFirst`: the dispatch above.
* `drones.keepCargo`: the cargo of a failed delivery is kept (section 2, fix 6).
* `drones.avoidObstacles`: the clearance. It is refused (`Rules::validate`, at start and when set)
  while there is no map, or the map is malformed or of another game build than the tables.

Codex gated the clearance by the presence of the map file, so that a server that had the file used it;
that has no off switch short of deleting the file, and the scheduling had no gate at all. The map is
also 55 MB of JSON and 18 MB parsed (730,000 boxes for the six planets), which the Codex version read
and kept at every start. Now the file is read when the rule is first validated or the drones first need
it, kept from then on, and a planet's terrain index is made when a drone first flies there.

### What changed because main moved

* `set_logistic_tasks` is main's search by group with the candidate lists shared between demands. A
  "nearest supply first" order is a different list for each demand, so with `drones.nearestFirst` on the
  supplies' candidates are built per demand (sorted by the supply's distance to the demand, equal
  distances keeping the order of the supplies); the shared lists stay the code for the game's order.
  Main's oracle test (`the_search_for_tasks_makes_the_tasks_the_first_search_made_in_the_same_order`)
  holds for the game's order and is unchanged.
* `release_drones` and `assign_idle_drones` have the game's version (main's, with `dotnet_sort`) and the
  nearest-first version, chosen by the rule; `next_task` is the one choice used where a drone takes its
  next job.
* `GameData` holds the map as a `DroneMap` (read when needed) and the terrain bounds are `Arc<[Bounds]>`
  (serde's `rc` feature), an idea of `4c55791` that main did not have: the terrain grids of the six
  planets share the one 18 MB instead of each copying their planet's.
* The staged integration originally used snapshot format 9 for the new drone rules.
  The final 0.8.2 release freezes the format 7 binary layout and preserves those rules
  in its named header. Stock 0.8.1 remains format 8 and needs a restart install.

### Defaults and what the rules cost (late-game save, 45 drones, 300 s, four players)

`bench::drones_on_a_late_world` (ignored, `PCDS_BENCH_SAVE`, tables with a drone map):

| Mode | User instructions per tick | Tick p99 | p99.9 | Slowest tick | World objects gained in 300 s |
|---|---|---|---|---|---|
| 0.8.1 (before) | 136,190 | 234 us | 1,035 us | 6.0 ms | (same world as the next row) |
| all rules off (the default, now) | 135,590 | 245 us | 1,095 us | 4.2 ms | 85 |
| `drones.nearestFirst` | 167,073 | 338 us | 2,316 us | 5.5 ms | 98 |
| `drones.avoidObstacles` | 209,150 | 643 us | 2,065 us | 28.0 ms | 225 |
| `drones.keepCargo` | 135,780 | 429 us | 1,720 us | 7.0 ms | 164 |
| all three | 232,264 | 485 us | 2,432 us | 22.4 ms | 248 |

The rows "0.8.1" and "all rules off" are `bench::bench` on the same late-game save, best of two
runs each; the others `bench::drones_on_a_late_world` (its "all rules off" row is 136,884 instructions in the same run; the machine was busy and the count moves by about 1%). With the rules off the tick costs what it did
before the Codex work: 135.6 k instructions against 136.2 k (the bench of the small world: 9,357
against 9,070 per tick at the same messages, within the noise of a count that also depends on the
joins). `nearestFirst` costs 23% more in this world, because the supplies' candidates are listed again
for each demand (the 1 Hz search is what the p99.9 shows); the clearance costs the grids and the
planning. The first flight on a planet makes its terrain index in one tick (the 30 ms slowest tick;
Humble has 99,000 boxes, Prime 146,000).

On a real server (`tools/perf/loadrun.py`, the same late-game save copied, eight players acting every
200 ms for 180 s, `world.dlcPlanets=Humble`), the bank's CPU in user time, idle and under load, and the
bank's memory:

| | idle | load | bank RSS at start / during / after | slowest tick of the last minute |
|---|---|---|---|---|
| 0.8.1 | 1.50% | 2.14% | 32.9 / 23.4 / 23.8 MB | 7.1 ms |
| this tree, rules off | 1.40% | 2.26% | 32.6 / 32.9 / 22.1 MB | 8.4 ms |
| this tree, both rules on (map made by `pcds drone-map`) | 1.80% | 2.99% | 64.4 / 25.7 / 26.1 MB | 17.1 ms |

(`perf stat`'s instruction counts of these runs are not used: on this hybrid CPU a process that lands
on the efficiency cores is counted by another counter, and the three runs differ by 2x without the
user time differing. The bench counts both kinds of cores.) With the rules on, the start reads the
55 MB map once (the 64 MB peak) and the process keeps 4 MB more afterwards. Nothing stopped, no player
was kicked, and the drones moved items (the bench says how many in 300 s).

The clearance makes drones deliver fewer items in the same time (91 items changed container against
117, 90 against 160 for the two together): the drones climb above what is in their way, and the boxes
are conservative. This is the cost of the rule, and why it is off. `keepCargo` costs nothing in
instructions and keeps 79 objects that the game would have destroyed.

The clearance costs instructions (the grids) and delivers fewer items in the same time, because drones
that must cross a mountain's box climb to above it; that is what it is for, and what the README says
("a mountain can make a drone fly high and long"). The first drone to fly on a planet makes that
planet's terrain index in one tick (about 25 ms on Humble); the later changes are one building's worth.

## 5. Memory (`4c55791`)

The commit is, in what it does, the work main already has: it is a port, on the 0.8.0 base, of main's
own memory work (`2f36371`, `8dcca83` and the rest are commits that main contains). To check this on
the tree and not on the message: the reverse of the commit's patch applies to every hunk of
`animals/*`, `bincodec.rs`, `clock.rs`, `data.rs`, `facts.rs`, `game.rs`, `genetics.rs`, `geometry.rs`,
`inventories.rs`, `machines/veins.rs`, `meteo*`, `newworld.rs`, `planets*`, `players.rs`, `rockets*`,
`scene.rs`, `toxic.rs`, `unlocks.rs`, `water.rs` and `testkit.rs` (that is, the same lines are already
in the tree), and the hunks of `net.rs`, `model.rs`, `machines*`, `doc.rs`, `compact.rs`,
`gamedata.rs` that do not reverse-apply are the same ideas written differently (main's world object
is 24 bytes, its network objects share their shapes, its tables are `Doc`s, its machines are boxed). The
functions the commit lists are in main: `in_range` narrowing by the objects with a network object in the
order of the list of all, the reused rocket id buffer, the cached prefab components, `compact_json`.

Nothing in it was dropped silently. What was new to main:

* three tests, which pass on main's representations unchanged and so show they behave the same:
  `doc::tests::state_documents_keep_the_old_json_codec_bytes`,
  `gamedata::tests::cached_components_keep_order_root_filter_and_edits`,
  `model::tests::compact_objects_keep_flat_snapshot_bytes_and_float_bits` (`e1e5671`);
* the shared terrain bounds of the drone map (above), which matter only with the map.

The memory budgets (`bench::budget::the_memory_of_the_world_stays_within_its_budget`) are unchanged and
hold; `bench::scale` is in the verification below.

## 6. What is not here: the Codex worktree

`/home/per/Documents/Codex/2026-10-20/fin/work/server-second-pass` had uncommitted edits when this work
was done (read with `git diff`, never written): `server/src/energy.rs`, `server/src/geometry/navigation.rs`,
`server/src/machines.rs`, `server/src/machines/growers.rs`, `server/src/net.rs`, `server/src/units.rs`.
They are not in this tree. They are a next round of the same kind of work, on the 0.8.0 base: the
walk over the machines with energy without copying the list (`energy.rs`), the machines' generator,
vegetation and harvestable parts boxed (`machines.rs`, `machines/growers.rs`: main already has its
machines boxed in another shape, as `parts_mut()`), the network flush that reuses its id buffer and
writes the messages straight to the output (`net.rs`, with a new test), the drone clearance query that
visits the boxes through a callback and trims the index's spare capacity
(`geometry/navigation.rs`), and a one-line change in `units.rs`. It cannot be taken as it stands:
`machines.rs` and `net.rs` would conflict with the compact versions on main, and `energy.rs` and
`navigation.rs` should go in after a measurement here. It is for the Codex session to say it is
finished, and for a later round to port.

## Verification

VERIFICATION


## 0.8.2 release integration

The earlier section describing unfinished work is historical. The completed second pass
(a76429d, 6e92c88, 40bfee1), steady transport changes (090b4a5, 8149236, 8e1c44e),
and final Steam teardown (8e1c44e..aaf1c18) are integrated in the 0.8.2 release branch.
Main's compact machine Parts and its existing wrapped UDP payload copy are retained;
the energy walk, network flush, fixed poll arrays, bounded navigation construction,
and server-owned Steam relay cleanup are adapted to those representations.
The format 7 compatibility adapter excludes the new drone rules from the frozen
binary layout and preserves them by name in the header, alongside the new logging
and backup settings. Public drone defaults stay off; existing installations must
explicitly enable the drone options they want. Final release evidence is recorded
by the release coordinator separately from the historical measurements above.
