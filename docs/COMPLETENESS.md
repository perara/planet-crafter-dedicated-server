# Is the dedicated server done?

**Status at v0.7.0:** this is the report as written against v0.6.2 on the completeness branch; its
numbers are that snapshot's. Every fix it calls "in this branch" or "on `integrate`" is in the
release now (the default-field cut `d0d9607`, the autosave thread `ca4e517`, the clock `GameTime`,
the UTP window `e65c211`, the corrupt-entry error, old-save import, wrecks and the .NET sort);
whether a gap it lists as open has been closed since was not rechecked. To measure again, see [testing.md](testing.md#completeness-map).

Measured on 2026-10-20 against v0.6.2 (plus the two commits of this branch), with a real late-game
save as the driver and the real game (under Wine) as the reference. Everything below is found by
searching the code and tests, or measured; nothing is a guess. What could not be checked is listed
at the end.

## Verdict

**Done for a normal game on the free planets and, with the owner's own tables, on the DLC planets
(the DLC planets could not be compared with the game: see "DLC planets"); a few rough edges remain
for a late-game world.** In ten lines:

1. Every one of the game's 732 groups and 124 server calls is handled in the code, and all but
   one server call (`NetworkTransform.SetStateServerRpc`, which no game code sends) have a unit test.
2. The parity sweep (a probe against the real game and this server, from the same save) ran every
   group and 116 of the 124 server calls; no difference is a bug of this server.
3. The late-game save (40 565 crafted objects, 33 583 world objects, five planets) loads as the
   real game loads it: the same objects stay and go, and what the two write back differs only
   where the game's own runs differ from each other.
4. Played for five game minutes by one probe, then played by scripts on 190 kinds of real
   building, on real containers, drones, rocket platforms, a trip to two other planets and two
   players at one box, the two hosts agree in everything except timing, the game's randomness
   and a handful of small differences (below).
5. **One serious bug was found and fixed: a save of this world took over a minute of the game
   thread** (`stop` never saved it, every autosave would freeze the game). Fixed in this branch
   (9520160); a separate fix for the same cause is already on `integrate`.
6. **A world saved on a DLC planet now runs on that planet's ground** (ore veins, water, toxic
   areas, the world instances of its portals), read by `pcds extract` from the owner's install by
   the code that reads the free planets'; Toxicity's toxic areas and the toxic gauge in them work
   (`toxic.rs`). Before, such a world ran on an empty planet. Tables without the ground stop the
   start and say what to run. See "DLC planets".
7. **Old saves: refused, and importable.** The 2022 single-planet format used to load as an empty
   world and be rewritten wrongly. It is now refused by every path that reads a world, and
   `pcds world import` converts it as the game does (same objects, inventories, planet Prime;
   the differences from the game's own save are the ones in EXPECTED.md "Old saves").
8. The autosave still holds the game thread for 0.3 s (up to 0.6 s with eight players) every time
   it runs; `integrate` writes it on a thread.
9. Start 1.0 s, 94 MB, 3.9 % of a core idle, a join in about 0.1 s, thirty minutes with eight bots
   and 72 000 actions without a fault (numbers below).
10. Not checked: Steam networking and invite codes, DLC ownership (the server checks none: the operator
    states what they own in `world.dlcPlanets`), a real game client, and any reference run on a
    DLC planet (the reference owns no DLC and has no honest way to).

## The numbers

These are produced by `server/tests/parity/completeness/map.py` and checked in CI by
`map.py check`.

<!-- numbers:begin -->
- groups: 732
- groups_without_prefab: 5
- groups_in_save: 445
- items: 455
- items_implemented: 455
- items_named_in_unit_tests: 37
- items_family_unit_tested: 375
- items_parity_checked: 455
- items_parity_with_a_bug_class: 0
- constructibles: 272
- constructibles_implemented: 272
- constructibles_named_in_unit_tests: 49
- constructibles_family_unit_tested: 266
- constructibles_parity_checked: 272
- constructibles_parity_with_a_bug_class: 0
- server_rpcs: 124
- server_rpcs_handled: 123
- server_rpcs_unit_tested: 123
- server_rpcs_parity_checked: 116
- rpc_families: 45
- rpc_families_fully_handled: 44
- rpc_families_fully_unit_tested: 45
- rpc_families_fully_parity_checked: 43
- client_rpcs: 84
- client_rpcs_sent_by_name: 70
<!-- numbers:end -->

In words:

| | implemented | unit-tested | checked against the real game |
|---|---|---|---|
| groups (732; 5 have no prefab and cannot be hosted by either) | 727 of 727 | 641 of 727 by a test that names the group or a kind of machine it carries (86 are named in a test; the rest of the items go through the same inventory and crafting code) | 727 of 727 (the sweep); 445 are in the late-game save, 190 kinds of building among them (four of those have no prefab) |
| server calls (124), in 45 families | 123 of 124 | 123 of 124 | 116 of 124 |
| server calls the game's host sends back (84) | 70 are written by name; the other 14 are relayed (animation, transform) or a console's | | |

"Implemented" for a group means that every component its prefab carries that does work on the
game's host is named in this server's code (`MATRIX.md` lists them, and the 72 classes of the game
that check for the host). "Unit-tested" is deliberately strict: the group's id or a machine of its
kind is in a unit test. "Checked against the real game" means the sweep ran the group on both hosts
and every difference was classified (`SWEEP.md`).

## What was run

| what | how | result |
|---|---|---|
| the census | `census.py` on the save: every group, where its objects are, what holds them | 445 of 732 groups present (190 of 277 buildings, 255 of 455 items); the groups it never has are listed in `MATRIX.md` |
| the map | `map.py` | groups, server calls, components, host classes: `MATRIX.md`, `*.csv` |
| load fidelity | the save loaded and written back by this server and by the real game | below |
| dynamic parity | eight probe scripts built from the save's own objects, run on both hosts, twice on the game for its noise | below |
| the non-functional run | start, memory, idle CPU, join, a 30 minute soak | below |
| the tests | `cargo test --release` (651 unit, 14 cli, 24 hot update, 4 layers, 1 admin golden), bench fingerprint | all pass with the full tables, with tables without the DLC ground and with `PCDS_TEST_DATA=none PCDS_GAME_DIR=none`; fingerprint 15c299165b6b3eb0 unchanged |

## Load fidelity

The save, loaded and written back at once, against the save in:

* **Nothing unparsable.** All 33 583 world objects, 2 246 inventories, 56 messages, 40 story events, 5 planets and 3 players
  parse; none names a group the tables do not know. (v0.6.2 would have skipped one that did not; the branch
  that makes that an error is on `integrate`.)
* **Same units, tokens, unlocked groups (190), settings, messages, story events, players**: unchanged.
* **8 procedural instances are dropped, and with them 805 objects the wrecks held.** The save's wrecks are from
  generator version 15, this server and the game both generate version 16: both unload all of them. Same on the real game
  (measured), not a loss of this server.
* **The rest differs by time only:** growth and hunger counters, drones' places, the production of the machines in the few seconds
  the game ran longer.
* **One real difference:** this server gives the 32 colourless Signs the default colour `0,0,0,1` at load, the real game
  leaves `0,0,0,0` (class a, cosmetic; gap 11).
* **The real game, same save:** it also drops the 8 instances (843 objects against 869 here: the 26 more are items that the machines
  of the planet it runs for the host used or made); it keeps the same 190 unlocked groups.

Written back against the game's own write (`savediff.py`): 32 770 against 32 718 world objects; the 60 the game has more
are items its machines used or made in the seconds it ran longer; no group of the game is missing.

## Dynamic parity

Both hosts started from the same copy of the save (with the host record `mkstart.py` makes), a probe player joined
and ran a script; the saves and the message streams were compared (`parity.py`, `savediff.py`,
`completeness/classify.py`, `aggdiff.py`). The game was run a second time for each script that matters, and what it
does against itself counts as its own noise (class d).

The save has the objects of two DLC planets (Humble and Toxicity: 10 554 objects). The reference owns no DLC and treats them
differently (their machines run while the host is on another planet), so the runs that decide are on the same save **with the DLC
planets removed** (`nodlc.py`); the full save is run once for the record.

| script | what it does | steps | difference left after the classes | verdict |
|---|---|---|---|---|
| lg_idle5 | one player stands for 5 minutes | 41 | production of 4 groups by 4 to 15 objects (random); terraformation units 4.4 % lower here because the game had run 13 s before the probe joined (class b); drones and crops equal in number (235/236 creates) | same |
| lg_real | 190 kinds of building, one real object each: ids, details, inventory, growth, text, colour, setting, linked planet, craft animation, grower start, then a minute, then read again, then deconstruct | 1 025 | none in an answer, except the Escape Pod (gap 8) | same |
| lg_inventories | 13 kinds of container, 2 each: watch, empty into a backpack, fill again, sort, lock, first of a group | 230 | invalid inventory id answered here, ignored by the game (gap 10); ids of new items | same |
| lg_logistics | demand on one box, supply on another, 90 s of drones | 25 | drone state counts (time); the supply and demand lists have the same 170 and 206 groups | same |
| lg_rockets | launch platform without a rocket, trade and exchange platform | 35 | none | same |
| lg_travel | Prime, Selenea, Aqualis, Prime; inventories read on arrival and a minute later | 130 | wild animals the game's own player spawns on arrival (5 crabs), animation state sent to animals (class b) | same |
| lg_pair.A/B | two players at one real box | 13 + 9 | the other player's object comes later on the game (class b) | same |

`MATRIX.md` and `completeness/*.csv` hold the rest. The full list of what each run did not touch is in the gap list.

## Machines, features and where they stand

| behaviour | where | unit tests | real-game evidence | in the late-game save | verdict |
|---|---|---|---|---|---|
| machines tick (generators, growers, crafters, breakers, optimizers, floaters) | `machines.rs`, `machines/{production,growers,veins}.rs` | 29 | sweep: all 272 buildings; idle run | 190 kinds | done |
| energy and "machine off without power" | `energy.rs` | 1 | sweep | all | done |
| water, floating machines | `water.rs` | 4 | sweep (nine water-only buildings on land) | 14 algae generators, fish farms | done; Aqualis' sea does not move; the DLC planets' water is read from the install (floaters on Humble's water in the late-game save settle where the game left them, less the 0.6094 of EXPECTED.md) |
| growth, outside growers | `machines/growers.rs` | 9 | sweep growth/ground/feed | 67 Vegetube2, 17 Farm1 | done |
| logistics, drones | `logistics.rs`, `logistics/motion.rs` | 11 | `scripts/logistics.json`; lg_logistics | 9 stations, 318 drones | done; frames after a stall over 1 s are skipped, not replayed |
| rockets, trade and exchange, departures | `rockets*.rs` | 13 | sweep craft_world; lg_rockets | 5 departure, 4 launch, 5 trade, 12 exchange platforms | done; endings do not restart the session (documented) |
| animals, pets, shelters, feeders | `animals/spawn.rs` | 7 | sweep animals; idle runs | 6 shelters, 5 feeders | done; every player sees every animal; wild animals spawn only when a client reports its spawner |
| vehicles (rover, truck) | `animals/vehicles.rs` | 8 | sweep vehicle/seat | 2 trucks in a locker | done; no physics (the nearest player owns it) |
| weather, meteors, day and night | `meteo.rs`, `meteo/explode.rs` | 11 | `scripts/meteo_story.json` | n/a | done; impacts are computed by clients |
| terraformation units and stages | `units.rs` | 4 | every run (units compared by rate) | 5 planets | done |
| story events, messages | `meteo/story.rs`, `handlers.rs` | 18 | `scripts/meteo_story.json` | 40 events, 56 messages | done |
| wrecks, world instances, portals | `instances.rs`, `instances/generate.rs` | 11 + 2 | `scripts/instances.json`; wreck sweep (EXPECTED.md) | 8 instances, 2 portal generators | done: each wreck is laid out from its own seed (Tessera, on a thread), the game's objects at the game's places |
| deconstruct, recycle | `deconstruct.rs`, `machines.rs` | 10 | sweep (every building), lg_real | all | done (gap 8) |
| equipment, gauges, dying | `equipment.rs`, `gauges.rs`, `status.rs` | 6 | sweep use | 3 players | done |
| crafting | client side; `AddOneToTotalCraft`, autocrafters | 3 | sweep craft/autocraft | 32 autocrafters | done |
| blueprints, unlocks | `unlocks.rs` | 2 | `scripts/crafting.json` | 190 unlocked | done |
| genetics | `genetics.rs`, `animals/spawn.rs` | 3 | sweep convert/animals | 511 trait items | done (random traits are class d) |
| Tessera (wreck layouts) | `tessera.rs` (run on a wreck's seed, off the tick), `native/wrecks.rs` (the tile sets, at extraction) | 1 | 24 wrecks and 8 world instances against the game; the six recorded wrecks of `wrecks_of_any_seed_come_out_as_the_game_lays_them` | 8 instances | done |
| travel between planets | `planets.rs` | 5 | `scripts/travel_chat.json`; lg_travel | 5 planets | done for free planets; a DLC planet only where the operator states they own it (`world.dlcPlanets`; the server cannot check ownership) |
| **DLC planets (Humble, Toxicity)** | `scene.rs`, `planets.rs`, `machines/veins.rs`, `water.rs`, `toxic.rs`, `instances/` | 15 (9 of them for toxic areas) | none possible (the reference owns no DLC); the real game's own save of a Humble world, see "DLC planets" | 10 554 objects | done, not comparable with the game; see "DLC planets" |
| save, load, backups | `save.rs`, `load.rs` | 9 | load fidelity above | whole | done (fix in this branch) |
| hot update | `handoff.rs`, `front/`, `bank/` | 25 + 4 | bench handoff EQUAL | whole | done |
| access, admin, chat hooks, S3 | `access.rs`, `admin.rs`, `chathooks/`, `front/chat_hooks.rs`, `s3.rs` | 16 | not comparable (no game equivalent) | n/a | done; Steam not checked |

## Non-functional acceptance (this server on the save)

All on this host, loaded (load average 35 and more) and with other jobs running, so times are upper bounds.
The save is the late-game save made playable by `mkstart.py` (32 770 world objects, 2 987 network objects, world on Prime).

| | measured |
|---|---|
| start to "running the world" | 1.0 s (the bench loads it in 0.91 s) |
| memory | front 7 MB; bank 94 MB at the load, 52 MB once it settles with one player; 179 to 223 MB with eight bots, flat over 30 minutes (no growth) |
| idle CPU | 3.9 % of one core with no player (the world stands still), 4.1 % with one idle player; front 0.5 % |
| the tick | 152 microseconds on average, 3.05 % of a core at real time, four players walking and acting for 300 s (the bench); the slowest loop of each second: 14.5 ms idle, 27 ms median with eight bots, 336 ms at the 90th percentile (the autosave), 620 ms at most |
| autosave on the game thread | 0.25 to 0.62 s with eight bots (autosave set to every minute to see it often); **before this branch's fix: more than 60 s, the bank was killed still saving** |
| join | approval 51 ms and the player ready 103 ms after the connection, with 3 195 objects to send |
| hot flip stand (`bench::handoff`, the snapshot taken and restored in process) | 23.7 MB snapshot (17.8 MB of it world objects), 53 ms to write, 79 ms to read, 132 ms together; two handoffs in 300 s, the run's fingerprint c45fb1986da9e7ea equal to the straight run's |
| soak: `pcds load`, 8 bots, 30 minutes, an action every 200 ms each | 72 000 actions, no panic, bank alive and saved at the end; round trips p50 0.58 ms, p95 14 to 49 ms, p99 40 to 393 ms (the autosave), worst 1.7 s |
| fingerprint, bench world | 15c299165b6b3eb0, unchanged by this branch (4 players, 300 s, seed 1); on this save c45fb1986da9e7ea |
| tests | 370 unit (9 ignored by default), 7 cli, 4 hot update: all pass, with the tables and with `PCDS_TEST_DATA=none PCDS_GAME_DIR=none` |

Not measured: a live `pcds upgrade` flip on this save (the in-process stand above stands in for it), and anything over Steam.

## Gaps, by how likely a player meets them and how bad they are

Severity is how likely a normal player or server owner meets it times how bad it is. "Fixed" is in this branch.

| # | gap | evidence | smallest fix |
|---|---|---|---|
| 1 | **Saving a big world took over a minute of the game thread. Fixed.** | `pcds serve` on the late-game save: the bank sat at 100 % CPU in `memmove` for 60 s after `stop`, was killed by the front, and wrote nothing. A 60 000-object test of the old code ran for more than two minutes. Cause: `SaveFile::to_text` cut each default field out of the 2 MB text in place, 250 000 times. | One pass per default (commit 9520160, test `save::tests::a_big_save_is_written_in_time`). The same cause is fixed on `integrate` (d0d9607, c28a115): drop mine when merging. |
| 2 | **A world saved on a DLC planet ran without that planet's ground. Fixed.** Humble had no ore veins, water boxes or world instances here; Toxicity's toxic areas were not simulated (the two `ToxicAreaHandler` client calls never sent). | The ground is in the base install, in the files and formats of the free planets': the same reader writes `scenes/<planet>.json` for every planet, the free planets' files byte for byte as before. Mining-ray machines (OreExtractor1/2) on Humble found no vein before (their rare ores never came out); floating machines stayed where the save left them. **The 29 ore extractors of the late-game save were not idle for lack of veins**: they are OreExtractor3, whose ore is the one the player chose (`setGroupsDataViaLinkedGroup`), and 17 of them were full (15 of 15); the net item count (291 to 295) also holds what the drones take out. The 12 with room made the same 89 items in 345 s before and after (`cmp` runs). | Done in this branch: `native/scenes.rs` (all planets, plus `toxicAreas`), `main.rs` (`scene_tables`), `toxic.rs`, `planets::ground_missing`, `scene::available_planets`. Tables format 4, snapshot format 7. |
| 3 | **Old-format saves were destroyed on the first save. Fixed.** The 2022 single-planet save loaded as an empty world, silently; its 5 136 objects were kept as 5 136 "players" and the next save wrote that. | The old-format save, loaded and stopped: `world: 0 world objects`, section 2 of the output held 5 136 records. The real game imports all of it (5 129 distinct objects: seven ids are listed twice in the file; and 181 more made by the machines), 358 inventories, planet Prime. | Done in this branch: the loader refuses what it cannot read (`save::layout`), `pcds world import` ports the game's conversion (`oldsave.rs`), parity-checked against the game (EXPECTED.md "Old saves"). |
| 4 | **The autosave holds the game for a third of a second or more.** | Soak: the slowest loop of the second was 250 to 620 ms at each autosave with eight bots, 3 to 27 ms in between; the 99th percentile of round trips is the autosave. | Write the autosave on a thread: done on `integrate` (ca4e517). |
| 5 | **The clock is a 32-bit float. Fixed.** After about 1.5 days of play the clock's step was 15 ms, after 97 days one second; timers of 0.1 and 0.2 s then ran at the wrong rate, the units' catch-up loop never ended from 24 days (the bank hung), and the world units' float totals stop growing at about their rate times 1.7e6. | `game.time` was `(clock - paused) as f32` (`game.rs`); the game has the same float but does not run for months. The game's own `WorldUnit._currentTotalValue` is a float too (decompiled). | Done: one `GameTime` (`clock.rs`), an f64 that does the game's float sums below an hour (bit for bit: parity, bench fingerprints, handoff) and exact sums after; every timer of the server is on it, the units' totals are f64 behind the game's float, a jump of the clock is one catch-up, the tick counter is 64 bits. docs/clock.md has the inventory of every timer and the proof (`timetravel`: nine ages from 0 to 900 days take the same steps). Snapshot format 6. |
| 6 | **Drones served equal-priority demands in another order than the game. Fixed.** The game sorts its demand inventories once, at start, with `List<T>.Sort` (an introsort: equal priorities end in an order of their own above 16 elements; same for the stations by distance, `SortInventory` and the optimizer's machines). | A save with 17 demand inventories at priority 9 and 17 at priority 0 (among the save's 159), 8 items to give them: the same 8 of 17 are served in three runs of the game, in each of the two classes; this server before the port served 14 and 12 other ones of 17 (stable sort), after it 0 and 0 (`completeness/demand_order.py`). The honey shortfall of the 5-minute runs (10 to 12 lower here, the first beehive empty at the end) was **not** this: the demand honey has one inventory, and the same runs gave the same honey before and after the port. It was the game's head start of 10 to 13 s before the probe joins (class b: the same 4.4 % in the terraformation units) against the beehive's 70 s cycle of being emptied by drones and refilled; with 12 s given to this server honey ends 189 and 192 before the port, 187 and 189 after, against 190, 191 and 192 on the game. | Done: `dotnet_sort.rs`, the game's own steps read from the IL of its `mscorlib` and checked against a real Mono. |
| 7 | A corrupt entry in a save is skipped without a word (v0.6.2). | `save.rs` `entries()` filters with `.ok()`. This save has none (all 36 000 or so entries parse), so nothing was lost here. | Done on `integrate` (an error naming the entry). |
| 8 | **Wrecks came in a pool of 64 layouts per size, not each wreck's seed. Fixed.** A save made by an older generator still loses all its wrecks (the game does the same). | The save's 8 instances (version 15) are dropped by both hosts, 805 objects with them (measured again after the fix: the same groups, the same counts). | Done: Tessera runs on the wreck's own seed when a player first enters it (0.1 to 3 s on a thread of its own; the tick spends 2.8 ms at most on the objects), and 1 706 objects of 24 wrecks equal the game's. EXPECTED.md has the comparison. |
| 9 | Escape pods can be deconstructed by a client that sends the call; the game ignores it. | lg_real: the world's pod is gone here, still there on the game. Known, EXPECTED.md "A second escape pod"; no game client sends it. | Treat the groups whose `ActionDeconstructible` sits under `WorldContainer` as not deconstructible. |
| 10 | Idle cost: 3.9 % of a core with no player, a slowest loop of 14 to 27 ms every second on this save. | nf run. | Find what the 14 ms second-by-second job is (a scan of 33 000 objects?). |
| 11 | A loaded colourless Sign gets the default colour `0,0,0,1` at load; the game leaves `0,0,0,0`. Cosmetic; unexplained (the Sign's colour child is hidden on some platforms). | 18 Signs on Prime, every run. | Check on a Steam-platform host before changing. |
| 12 | A call about an inventory id that does not exist is answered here and ignored by the game (`TransferAllItems`, `LockInventoryContent`). | lg_inventories, inventory 603. | Return silently. |
| 13 | The game's own player spawns wild animals on arrival (five crabs on Aqualis); here animals appear when a real client reports its spawner. | lg_travel. Probably the same with real clients: not verified. | Verify with a real client. |
| 14 | Documented limits: no physics (the nearest player owns what falls and drives), every player sees every animal, endings do not restart the session, sleeping does not skip the night. | README "Differences". | None planned. |
| 15 | `NetworkTransform.SetStateServerRpc` is not handled. | No game code sends it. | None. |

**Fixed in this branch:** gap 1 (commit 9520160), gap 3 (the `old-saves` branch: refusal and import), gap 6 (the .NET sort, below the table), gap 8 (the `wrecks-tessera` branch) and a one-line change to the probe so a script can address
the world's own objects (6f34b0a). Everything else above is listed with its smallest fix and not started.

### Gap 6: where the game sorts, and what this server does

Every sort in the game's code (Assembly-CSharp, Netcode, Tessera, Sylves, DeBroglie), found by its IL, not its source:

| game | sort | here |
|---|---|---|
| `LogisticManager.SetTasksCoroutine` | `_demandInventories.Sort(CompareInventoryPriority)`, once at start | `dotnet_sort` (`logistics.rs`) |
| `LogisticManager.SetLogisticTasks` | `_stationsDistancesBuffer.Sort()` (a struct, `IComparable<T>`; ties are common: the distance is rounded to whole metres) | `dotnet_sort` |
| `Inventory.AutoSort` (`SortInventoryServerRpc`) | `Sort(SortInventory)`; the comparison is no order (an item without a trait is equal to all, one with a trait is after it) | `dotnet_sort`, and the game's comparison now (it was made a total order only because Rust's sort panics on that) |
| `MachineOptimizer` (two) | `result.Sort` by `Vector3.Distance` | `dotnet_sort` (the input order, the physics order of the sphere cast, is still not the game's) |
| `WorldInstanceGenerator.CreateRecipe` | `list.Sort` by id (equal ids are the same ingredient) | `dotnet_sort` |
| `UnlockingHandler`, UI windows (DNA extractor, teleporters) | `Sort` of doubles / for the screen | not needed (ties are equal values / client only) |
| `EnvironmentUpdater`, UI, Tessera, DeBroglie, Netcode's config | `OrderBy` (stable) | stable sorts |
| Netcode `SceneEventData.SortObjectsToSync` | `Sort` of the network objects of a joining client, with a comparison that is 0 for everything (the game registers no prefab handler): the introsort still moves them above 16 objects | **not ported**: only the order in which a joining client creates its objects, no state; one call of `dotnet_sort` when wanted |

The sort is `ArraySortHelper<T>.IntrospectiveSort` of the game's Mono (read from the IL of the shipped `mscorlib.dll`): up to 16 elements an
insertion sort (2 and 3 elements by compare-and-swap), above that median-of-three quicksort and a heap sort when the depth
`2 * bit length of the count` is spent. `server/tests/dotnet_sort/reference.py` writes the same steps in Python and
`vectors.txt` holds 78 cases (equal keys, few-valued keys, the shape of the save's demand list, adversarial inputs that reach the heap sort);
`Check.cs` ran them on a real Mono (wine-mono): `List<T>.Sort(Comparison<T>)` and the struct with `IComparable<T>` gave the same
order as the Python on all 78, and this module gives it too (a unit test). Parity: `completeness/demand_order.py` and the
numbers in the table above.

Remaining differences in what the game orders: the neighbours of a machine come from a physics sphere cast, and this server uses the
order of its world objects (the same note as `production.rs`'s `ponytail:`; it is what AutoCrafter ingredients and the optimizer's
list depend on); the colour in `SortInventory`'s last tie-break is `Color.ToString()` formatted as the game does, compared by this server's
culture comparison of ASCII, not .NET's.

## DLC planets

**Where the data is.** In the base install: the game's depot holds the scenes of Humble and Toxicity (`Planet-Humble`,
`Planet-Toxicity` and their sectors and instance scenes are in the build's scene list, in the same `levelN` files and
serialized formats as the free planets'). No separate DLC files exist, and the install's only depot is the base game's, so
the data is the same on every install; ownership is a property of the Steam account, which the game checks when a host
takes a world to a planet. Nothing of it is in this repository: `pcds extract` writes it into the tables of whoever runs it.

**What was built.**

* `native/scenes.rs` reads every planet of the PlanetList (not only the free ones) and writes, besides the water volumes, ore
  veins and instance scenes, the `toxicAreas` of a planet that has `ToxicArea` components (only Toxicity; the free planets' files
  are unchanged). Tables format 4.
* `scene_tables` loads a DLC planet's file when it is there. `planets::ground_missing` stops a start on a DLC planet without
  it and refuses a travel there, telling the operator to run `pcds extract`.
* `toxic.rs`: the areas, their registration under the game's key, cleaned goo counted from the world objects at start and from
  scene objects picked up or disabled after, the clients' registrations and the progress and end calls, the tracked area the toxic
  gauge reads (`gauges.rs`), the totals the map asks for. Snapshot format 7 (the registrations).
* Ownership: `scene::available_planets` offers a DLC planet only if the operator states they own it (the rule
  `world.dlcPlanets`, empty by default). The game's own list is the host's purchases; this server has no Steam account to ask,
  so the operator's statement stands for it, and nothing is checked, faked or patched. A world saved on a DLC planet that the
  rule does not list does not start (`planets::ownership_unstated`, in `bank/start.rs`), and the message says which rule to set;
  a rules change that would take the planet away from a world standing on it is refused. `/api/status` shows `dlcPlanets`.
* Census of what a DLC planet's scenes hold that no free planet's does: the toxic classes (`ToxicArea`, `ToxicAreaVolumes`,
  `ToxicAreaDecal`, `ToxicTag`, `ToxicSwitchState`, `ActionDrinkToxicWater`), the trains' `SceneInterpolationPath` and
  `ActionInterpolator`s (the server's `SceneInterpolationHandler` serves any interpolator), `WorldAutoSpawnerOnFloor` and the
  growth-driven scene evolutions (client pictures). Of those only the areas have a server side. The `DeparturePlatformHumble` of old
  saves is `oldsave.rs`'s (2.103 has one `DeparturePlatform`). `needPurification` (Toxicity's fourth unit) and the planets' stages,
  meteo and story lists are data the units, meteo and story code already read per planet.

**Evidence, by route** (the late-game save, a public file, with a real Steam ID that is not quoted here or anywhere):

| route | result |
|---|---|
| (a) the proven reader | the install read with this build gives every free planet's `scenes/*.json`, `netdata`, `gamedata`, `rpcs`, `wrecks` and the connection request **byte for byte** the tables of v0.7.0 (`tables-diff`: 0 differences, only `tables-format` and the DLC files are new). The DLC files come from the same functions; the reader raises an error where a scene holds a shape it does not read, and none was raised. |
| (b) structure | unit test `the_dlc_planets_veins_read_from_an_install_hold_together` and `the_toxic_areas_read_from_an_install_hold_together` (they skip loudly without DLC tables): every vein group known, a collider for the ray to find, a world object exactly when a vein has content, positions within the span of the free planets, unique keys and scene ids across the toxic areas, every instance scene listed by PlanetData present (`the_portals_of_the_dlc_planets_open_all_their_scenes` opens each). |
| (c) the real game's own save | **Humble**: of the 28 extractors the player had chosen an ore for, 21 have that ore among the groups of the vein the extracted ground puts under them (`dlc_selection.py`): every extractor set to Osmium, Uraninite, Obsidian, Zeolite, Titanium, Dolomite or Bauxite. The 7 that do not are set to Sulfur, Silicon, Magnesium, Cobalt or Iron and hold two items each; the free planets show the same (7 of 14 on Prime, 4 of 5 on Selenea), so they are machines set up by hand over no vein for the ore. **Toxicity**: 3 ToxicGoo items of the save carry scene ids that the extracted areas list, with the same group (by chance one in thousands). **Floating machines** on Humble settle on the water the ground now has, at the height the save has less the 0.6094 EXPECTED.md already names for the free planets. |
| (c) behaviour | the late-game save run for 345 s on Humble, before and after: the 12 extractors with room made 89 items in both (17 were full; OreExtractor3 mines the ore chosen, not a vein's). The same 29 extractors turned into OreExtractor2 at their own places (`OreExtractor1/2` need the veins): 146 items in 160 s; **62 are ores of the vein under the extractor, and none is outside the basic ores plus that vein's**; before this branch 0 of 146 were. The world moved to Toxicity with a probe player standing in an area and registering as the game's client does: `GetCurrentPlanetToxicProgress` answers the totals, `UpdateAreaToxicProgressClientRpc` follows the registration, and the saved toxic gauge is the world's drain rate times the seconds in the area (4.96 for 24.8 s at 0.2 a second, the save's `modifierGaugeDrain`). |
| (d) synthetic | 9 tests of `toxic.rs` on areas written by hand (registration, progress and end calls, goo picked up and disabled, destroy that counts nothing, areas cleaned at start, gauge fill and the filter and the limit, leaving, registrations of two clients, a key a rounding step off, a hot update), `a_dlc_planets_veins_are_mined_like_a_free_planets`, `a_dlc_planet_is_travelled_to_only_if_the_operator_states_they_own_it`. |

**What remains unverifiable.** Against the real game: any DLC planet behaviour other than what its own save shows (the
reference owns no DLC, see EXPECTED.md); the keys of the toxic areas (made from the dump's positions in single precision;
a client that computes another is taken to mean the area within 4 of it, `KEY_SLACK`); which sectors the real host has
loaded when a remote player is in an area (here all of the planet's); whether a player who owns the DLC can take a
world with no DLC planet to one (refused here unless the operator lists the planet in `world.dlcPlanets`); the volumes, decal and map marker (client pictures of
the two calls); the Toxicity vein world objects' use by the toxic water collectors beyond the count tests.

## The old-format save

The old-format save (2022, Survival mode) is the game's format from before several planets: nine sections (world units,
player, 5 136 world objects, 329 inventories, stats, messages, story events, settings, colour layers), no planet,
no instances, no logistics. The real game opens it as Prime (it patches the file's text, JSONExport.LoadFromJson), keeps
every object (5 129 distinct ids), adds 181 made by the machines (algae seeds on the water), and writes the
10-section format. **This server** refuses it at every entry and imports it with `pcds world import` into the
same world: 5 129 objects stay 5 129, 358 inventories after the load (329 plus the 29 the machines make, as on
the game), a save then a load then a save changes nothing, and a probe player played it for four minutes.

## Big features that are open, with their scope


## What could not be checked, and why

* **Steam networking and invite codes**: no Steam login here; the direct IP path was used throughout.
* **DLC ownership and any run of the DLC planets in the real game**: this server never checks ownership (it has no Steam account to
  ask): the operator states what they own in `world.dlcPlanets`, and a DLC planet is offered only then. The reference, the game under Wine without Steam, owns no DLC
  (`PlanetList.GetIsPlanetPurchased` is false there, so its host refuses a world saved on one), and the only ways to make it
  run one are a Steam stand-in or a patch of the planet list, which would fake ownership: not done. So nothing on Humble or
  Toxicity was compared with the game; the other evidence is in "DLC planets".
* **A real game client**: the probe is a scripted client. Nothing the game draws, plays or simulates on the client was seen.
* **Physics**: where a dropped thing comes to rest, a vehicle's motion between transforms, a meteor's impact.
* **The `integrate` branch**: it was read, not run; the numbers are v0.6.2 plus this branch.
* **More than one pass of the long runs**: the game's noise was measured once (a second reference run) for the idle run and
  the logistics script; the other scripts were run once against the game.

## How to regenerate

```bash
cd server/tests/parity/completeness
python3 census.py DATA_DIR SAVE.json census.json                 # needs the save (not in the repository)
SWEEP_START=START_FULL.json python3 map.py gen DATA_DIR --cs CS_DIR   # MATRIX.md, MATRIX.csv, RPCS.csv, COMPONENTS.csv, HOSTCLASSES.csv
python3 map.py check                                             # CI, no game data
python3 nodlc.py DATA_DIR start.json start_nodlc.json            # the save without the DLC planets
python3 gen.py DATA_DIR start.json OUT_DIR                       # the probe scripts, from the save's own objects
../sweep.sh both OUT lg_*.json                                    # REF_DIR, DATA_DIR, START: see ../SWEEP.md
python3 classify.py OUT/lg_real.diff OUT/lg_real.noise            # the kinds of difference, and which the game makes against itself
python3 aggdiff.py START REF.save OURS.save REF2.save             # what each group's count did in the run
python3 loaddiff.py IN.json OUT.json                              # what a load and save changed
python3 dlc_selection.py SAVE.json DATA_DIR Humble               # a late-game save's extractors against the DLC ground read from the install
python3 ../mkold.py SAVE.json OLD.json [--tokens | --settings-only]   # a save in an old layout, for pcds world import (EXPECTED.md "Old saves")
```
