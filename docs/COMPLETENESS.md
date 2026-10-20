# Is the dedicated server done?

Measured on 2026-10-20 against v0.6.2 (plus the two commits of this branch), with a real late-game
save as the driver and the real game (under Wine) as the reference. Everything below is found by
searching the code and tests, or measured; nothing is a guess. What could not be checked is listed
at the end.

## Verdict

**Done for a normal game on the free planets; done except the DLC planets and a few rough edges
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
6. **A world that was saved on a DLC planet runs, but without that planet's ground:** the real
   save's own planet (Humble) has no ore veins, water or scene objects here, so its 29 ore
   extractors stay idle; Toxicity's toxic areas are not simulated at all.
7. **Old saves: refused, and importable.** The 2022 single-planet format used to load as an empty
   world and be rewritten wrongly. It is now refused by every path that reads a world, and
   `pcds world import` converts it as the game does (same objects, inventories, planet Prime;
   the differences from the game's own save are the ones in EXPECTED.md "Old saves").
8. The autosave still holds the game thread for 0.3 s (up to 0.6 s with eight players) every time
   it runs; `integrate` writes it on a thread.
9. Start 1.0 s, 94 MB, 3.9 % of a core idle, a join in about 0.1 s, thirty minutes with eight bots
   and 72 000 actions without a fault (numbers below).
10. Not checked: Steam networking and invite codes, DLC ownership, a real game client, and any
    reference run on a DLC planet (the reference owns no DLC).

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
- client_rpcs_sent_by_name: 68
<!-- numbers:end -->

In words:

| | implemented | unit-tested | checked against the real game |
|---|---|---|---|
| groups (732; 5 have no prefab and cannot be hosted by either) | 727 of 727 | 641 of 727 by a test that names the group or a kind of machine it carries (86 are named in a test; the rest of the items go through the same inventory and crafting code) | 727 of 727 (the sweep); 445 are in the late-game save, 190 kinds of building among them (four of those have no prefab) |
| server calls (124), in 45 families | 123 of 124 | 123 of 124 | 116 of 124 |
| server calls the game's host sends back (84) | 68 are written by name; the other 16 are relayed (animation, transform), a console's, or Toxicity's | | |

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
| the tests | `cargo test --release` (370 unit, 7 cli, 4 hot update), bench fingerprint | all pass; fingerprint 15c299165b6b3eb0 unchanged |

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
| water, floating machines | `water.rs` | 4 | sweep (nine water-only buildings on land) | 14 algae generators, fish farms | done on free planets; Aqualis' sea does not move; none on DLC planets |
| growth, outside growers | `machines/growers.rs` | 9 | sweep growth/ground/feed | 67 Vegetube2, 17 Farm1 | done |
| logistics, drones | `logistics.rs`, `logistics/motion.rs` | 11 | `scripts/logistics.json`; lg_logistics | 9 stations, 318 drones | done; frames after a stall over 1 s are skipped, not replayed |
| rockets, trade and exchange, departures | `rockets*.rs` | 13 | sweep craft_world; lg_rockets | 5 departure, 4 launch, 5 trade, 12 exchange platforms | done; endings do not restart the session (documented) |
| animals, pets, shelters, feeders | `animals/spawn.rs` | 7 | sweep animals; idle runs | 6 shelters, 5 feeders | done; every player sees every animal; wild animals spawn only when a client reports its spawner |
| vehicles (rover, truck) | `animals/vehicles.rs` | 8 | sweep vehicle/seat | 2 trucks in a locker | done; no physics (the nearest player owns it) |
| weather, meteors, day and night | `meteo.rs`, `meteo/explode.rs` | 11 | `scripts/meteo_story.json` | n/a | done; impacts are computed by clients |
| terraformation units and stages | `units.rs` | 4 | every run (units compared by rate) | 5 planets | done |
| story events, messages | `meteo/story.rs`, `handlers.rs` | 18 | `scripts/meteo_story.json` | 40 events, 56 messages | done |
| wrecks, world instances, portals | `instances.rs`, `instances/generate.rs` | 7 + 1 | `scripts/instances.json` | 8 instances, 2 portal generators | **partly**: a pool of 64 layouts per size, not each wreck's own seed |
| deconstruct, recycle | `deconstruct.rs`, `machines.rs` | 10 | sweep (every building), lg_real | all | done (gap 8) |
| equipment, gauges, dying | `equipment.rs`, `gauges.rs`, `status.rs` | 6 | sweep use | 3 players | done |
| crafting | client side; `AddOneToTotalCraft`, autocrafters | 3 | sweep craft/autocraft | 32 autocrafters | done |
| blueprints, unlocks | `unlocks.rs` | 2 | `scripts/crafting.json` | 190 unlocked | done |
| genetics | `genetics.rs`, `animals/spawn.rs` | 3 | sweep convert/animals | 511 trait items | done (random traits are class d) |
| Tessera (wreck layouts) | `native/tessera.rs`, at extraction | 1 | by hand (`manual` tests) | n/a | done for 64 seeds per size |
| travel between planets | `planets.rs` | 5 | `scripts/travel_chat.json`; lg_travel | 5 planets | done for free planets; DLC planets refused |
| **DLC planets (Humble, Toxicity)** | `scene.rs`, `machines/veins.rs`, `toxic.rs` | 3 | none possible (the reference owns no DLC) | 10 554 objects | **partly**, see gap 3 |
| save, load, backups | `save.rs`, `load.rs` | 9 | load fidelity above | whole | done (fix in this branch) |
| hot update | `handoff.rs`, `front.rs`, `bank.rs` | 25 + 4 | bench handoff EQUAL | whole | done |
| access, admin, chat hooks, S3 | `access.rs`, `admin.rs`, `chathooks.rs`, `s3.rs` | 16 | not comparable (no game equivalent) | n/a | done; Steam not checked |

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
| 1 | **Saving a big world took over a minute of the game thread. Fixed.** | `pcds serve` on the late-game save: the bank sat at 100 % CPU in `memmove` for 60 s after `stop`, was killed by the front, and wrote nothing. A 60 000-object test of the old code ran for more than two minutes. Cause: `SaveFile::to_text` cut each default field out of the 2 MB text in place, 250 000 times. | One pass per default (commit 9520160, test `save::tests::default_fields_of_a_large_world_are_cut_in_linear_time`). The same cause is fixed on `integrate` (d0d9607, c28a115): drop mine when merging. |
| 2 | **A world saved on a DLC planet runs without that planet's ground.** The save's own planet (Humble) has no ore veins, water boxes or scene objects here; Toxicity's toxic areas are not simulated (the two `ToxicAreaHandler` client calls are never sent). | Humble run, five minutes: its 29 ore extractors went from 291 to 295 items (the same extractors on a free planet make one ore each 70 s); `veins.rs` and `water.rs` say "None on a DLC planet"; `toxic.rs` answers zeros. The reference cannot show the right answer (it owns no DLC and runs Humble's machines while the host is elsewhere). | Read the DLC scenes from an owner's install in `pcds extract` (like the free planets', for veins and water); the toxic areas are a feature (about one module). Until then refuse or warn when the saved planet is a DLC planet. |
| 3 | **Old-format saves were destroyed on the first save. Fixed.** The 2022 single-planet save loaded as an empty world, silently; its 5 136 objects were kept as 5 136 "players" and the next save wrote that. | The old-format save, loaded and stopped: `world: 0 world objects`, section 2 of the output held 5 136 records. The real game imports all of it (5 129 distinct objects: seven ids are listed twice in the file; and 181 more made by the machines), 358 inventories, planet Prime. | Done in this branch: the loader refuses what it cannot read (`save::layout`), `pcds world import` ports the game's conversion (`oldsave.rs`), parity-checked against the game (EXPECTED.md "Old saves"). |
| 4 | **The autosave holds the game for a third of a second or more.** | Soak: the slowest loop of the second was 250 to 620 ms at each autosave with eight bots, 3 to 27 ms in between; the 99th percentile of round trips is the autosave. | Write the autosave on a thread: done on `integrate` (ca4e517). |
| 5 | **The clock is a 32-bit float.** After about 1.5 days of play the clock's step is 15 ms, after 97 days one second; timers of 0.1 and 0.2 s then run at the wrong rate. | `game.time` is `(clock - paused) as f32` (`game.rs`); the game has the same float but does not run for months. | Keep the due times in f64: done for gauges and units on `integrate` (40242e8, c318e13); machines and logistics still to do. |
| 6 | **Drones served equal-priority demands in another order than the game. Fixed.** The game sorts its demand inventories once, at start, with `List<T>.Sort` (an introsort: equal priorities end in an order of their own above 16 elements; same for the stations by distance, `SortInventory` and the optimizer's machines). | A save with 17 demand inventories at priority 9 and 17 at priority 0 (among the save's 159), 8 items to give them: the same 8 of 17 are served in three runs of the game, in each of the two classes; this server before the port served 14 and 12 other ones of 17 (stable sort), after it 0 and 0 (`completeness/demand_order.py`). The honey shortfall of the 5-minute runs (10 to 12 lower here, the first beehive empty at the end) was **not** this: the demand honey has one inventory, and the same runs gave the same honey before and after the port. It was the game's head start of 10 to 13 s before the probe joins (class b: the same 4.4 % in the terraformation units) against the beehive's 70 s cycle of being emptied by drones and refilled; with 12 s given to this server honey ends 189 and 192 before the port, 187 and 189 after, against 190, 191 and 192 on the game. | Done: `dotnet_sort.rs`, the game's own steps read from the IL of its `mscorlib` and checked against a real Mono. |
| 7 | A corrupt entry in a save is skipped without a word (v0.6.2). | `save.rs` `entries()` filters with `.ok()`. This save has none (all 36 000 or so entries parse), so nothing was lost here. | Done on `integrate` (an error naming the entry). |
| 8 | Wrecks: a pool of 64 layouts per size, not each wreck's seed; a save made by an older generator loses all its wrecks (the game does the same). | README "Differences"; the save's 8 instances (version 15) were dropped by both hosts, 805 objects with them. | Run the Tessera port at generation time instead of the pool (known open item). |
| 9 | Escape pods can be deconstructed by a client that sends the call; the game ignores it. | lg_real: the world's pod is gone here, still there on the game. Known, EXPECTED.md "A second escape pod"; no game client sends it. | Treat the groups whose `ActionDeconstructible` sits under `WorldContainer` as not deconstructible. |
| 10 | Idle cost: 3.9 % of a core with no player, a slowest loop of 14 to 27 ms every second on this save. | nf run. | Find what the 14 ms second-by-second job is (a scan of 33 000 objects?). |
| 11 | A loaded colourless Sign gets the default colour `0,0,0,1` at load; the game leaves `0,0,0,0`. Cosmetic; unexplained (the Sign's colour child is hidden on some platforms). | 18 Signs on Prime, every run. | Check on a Steam-platform host before changing. |
| 12 | A call about an inventory id that does not exist is answered here and ignored by the game (`TransferAllItems`, `LockInventoryContent`). | lg_inventories, inventory 603. | Return silently. |
| 13 | The game's own player spawns wild animals on arrival (five crabs on Aqualis); here animals appear when a real client reports its spawner. | lg_travel. Probably the same with real clients: not verified. | Verify with a real client. |
| 14 | Documented limits: no physics (the nearest player owns what falls and drives), every player sees every animal, endings do not restart the session, sleeping does not skip the night, 64-layout wrecks. | README "Differences". | None planned. |
| 15 | `NetworkTransform.SetStateServerRpc` is not handled. | No game code sends it. | None. |

**Fixed in this branch:** gap 1 (commit 9520160), gap 3 (the `old-saves` branch: refusal and import), gap 6 (the .NET sort, below the table) and a one-line change to the probe so a script can address
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

## The old-format save

The old-format save (2022, Survival mode) is the game's format from before several planets: nine sections (world units,
player, 5 136 world objects, 329 inventories, stats, messages, story events, settings, colour layers), no planet,
no instances, no logistics. The real game opens it as Prime (it patches the file's text, JSONExport.LoadFromJson), keeps
every object (5 129 distinct ids), adds 181 made by the machines (algae seeds on the water), and writes the
10-section format. **This server** refuses it at every entry and imports it with `pcds world import` into the
same world: 5 129 objects stay 5 129, 358 inventories after the load (329 plus the 29 the machines make, as on
the game), a save then a load then a save changes nothing, and a probe player played it for four minutes.

## Big features that are open, with their scope

* DLC planet support: scene dumps for Humble and Toxicity from an owner's install (extraction work, no new game
  logic), then the toxic areas (one module, the `ToxicArea` registrations and two client calls).
* Each wreck's own Tessera seed (the port exists in `native/tessera.rs`; it runs at extraction now).
* f64 clocks for machines and logistics (gap 5).

## What could not be checked, and why

* **Steam networking and invite codes**: no Steam login here; the direct IP path was used throughout.
* **DLC ownership**: this server never checks it; the reference owns none, so nothing on Humble or Toxicity could be compared
  with the game (the runs that decide are on the save without them).
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
python3 ../mkold.py SAVE.json OLD.json [--tokens | --settings-only]   # a save in an old layout, for pcds world import (EXPECTED.md "Old saves")
```
