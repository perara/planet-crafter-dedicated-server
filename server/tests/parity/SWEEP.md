# The parity sweep

Every group of the game, every interaction its prefab carries and every vehicle, run on the real
game host and on this server from the same start save, and every difference classified.
`sweep.py gen` writes the scripts from the tables (nothing is hand-picked); `sweep.sh` runs them;
`sweep.py report` builds the matrix from the runs; `sweep_classes.json` holds the reasons a
difference is not a bug. README "Parity with the game" says how to bring the reference up.

## Result

| | |
|---|---|
| groups in the tables | 743 (every item and constructible; 5 have no prefab and cannot be spawned by either host) |
| interactions run per group | 382 kinds, 6701 group x interaction rows (`SWEEP.matrix.md`) |
| component types the prefabs carry | 165, each run through the groups that carry it (`SWEEP.components.md`) |
| probe scripts | 33 files, 31 runs (`.A`/`.B` pairs run together), 18 777 steps |
| rows that match the real host | 6040 |
| rows with a difference | 656: b (a listed deviation) 512, d (the game's randomness or timing) 142, c (a new deliberate safety deviation) 1, e (not comparable) 3. A row whose differences fall in several classes counts under each. |
| rows with a difference that is none of these (a: our bug) | 0 |
| bugs found and fixed | 12, each with a test and its own commit (below) |

The numbers come from the last `sweep.py report` on the real-host transcripts and saves of the
sweep (`both`) and the replays of this server against them (`ours`) at the merged tree.

## How to rerun

```bash
cd server/tests/parity
python3 sweep.py gen DATA_DIR OUT START.json          # OUT/<script>.json, OUT/rows.json
REF_DIR=... DATA_DIR=... START=START.json \
  agent-run planetcrafter --cpus 6 -- ./sweep.sh both RUN OUT/sweep_*.json   # reference and ours
REF_DIR=... DATA_DIR=... START=START.json \
  agent-run planetcrafter --cpus 6 -- ./sweep.sh ref2 RUN OUT/sweep_*.json   # the reference again
OURS_PORT=7960 REF_DIR=... DATA_DIR=... START=START.json \
  ./sweep.sh ours RUN OUT/sweep_X.json                 # only this server, against RUN's reference
python3 sweep.py report DATA_DIR OUT RUN [CS_DIR]     # RUN/matrix.md, classified.json, components.md
```

`START` is `mkstart.py`'s start save (the user's save with a host record of its own and every ore
extractor filled): the same file for the reference runs and for every replay, else hundreds of
differences appear that are none. `sweep.sh ours` starts no game host: after a fix the affected
scripts are replayed against the transcript and save of the reference already in `RUN`, in as
many parallel workers as there are ports and folders (the final pass ran three). `both` and
`ref2` need the Wine reference, one at a time.

`RUN/NAME.diff` is what parity.py found between the game and this server, `RUN/NAME.noise`
between two runs of the game: a difference of the same kind there is the game's own randomness
or timing (class d). Every other difference is matched against `sweep_classes.json` (classes
b, c, e); what matches nothing is `unclassified` and is a bug until shown otherwise. A rule marked
`paired` covers an object only when both sides created it and alike in everything but the fields
it names (the owner rule for drops never covers a different hash, position, flags or rotation);
a rule marked `same_items` covers an inventory listing only when both sides hold the same items
(only the order is random).

## The reference host

The game itself, headless under Wine and Xvfb, with BepInEx and the extractor plugin (README).
Two changes to the plugin for the sweep, both for measuring:

- `POST /api/save` saves the world on demand (the README's one-line case).
- `PCDS_KEEP_ACTIVE=1` skips `PerformancerDisableIfFarAway.UpdateComponent`, so no building part
  is hidden by its distance to the hidden host player, which would otherwise make a Biolab or a
  lamp far from the pod do nothing on the reference:

  ```csharp
  [HarmonyPrefix]
  [HarmonyPatch(typeof(PerformancerDisableIfFarAway), "UpdateComponent")]
  static bool Skip(PerformancerDisableIfFarAway __instance) { __instance.gameObject.SetActive(true); return false; }
  ```

The start save lies just below TerraformStage4-Water: Prime's big lake starts to rise over the
land east of the pod during a long run and RequireEnergy zeroes what is under it, so scripts that
build for long build at y = 60 (`BUILD_Y`); scripts that need ground (outside growers, drops) use
the flat ground east of the pod measured on the reference (`GROUND`).

## The scripts

One player each unless named `.A`/`.B`, a pair run together.

| script | what it exercises |
|---|---|
| sweep_items_1..4 | every item: created twice in a scratch inventory, both moved to the backpack (TransferAllSameGroup), one moved back and destroyed, the other dropped (DropOnFloor), picked up (AddWorldObjectToInventory) and destroyed; then one created on the floor (CreateAndDropOnFloor) |
| sweep_use | every consumable used as the inventory screen does (Eat/Drink/Breath/Purify with its value, GainTokens, drop to consume, build a buildable item); every equipable equipped and taken off with PlayerEquipment's follow-ups; something that is no equipment into the equipment |
| sweep_craft_1..2 | every item with a recipe crafted at a station that crafts it, as CraftManager does |
| sweep_craft_world | every item crafted in the world: rockets at a launch platform (then sent into space) and drones at a drone station |
| sweep_autocraft_AutoCrafter1_1..3, _Incubator2_1 | every recipe through the server's own crafting: an autocrafter set to it, the ingredients in a container beside it |
| sweep_build_1..3 | every constructible built, each of its network behaviours used (ids, details, inventories, text, colour, setting, linked groups and planet, growth, interactive state, panels in a look their type can wear, craft animation, outside grower start), an input given to machines that take one, five minutes to run, everything read again, deconstruction refused while its inventory is not empty and done once empty |
| sweep_water | the ten buildings only placeable on water, built together like sweep_build (the start land has no water) |
| sweep_ground | every other outside grower built on measured flat ground, started and left to plant for two minutes |
| sweep_feed | every seed, larva, egg, food, fuse, fuel, key, skin and DNA item in a machine that takes it (each such machine at least once), six minutes of growing, hatching and producing, then the harvest picked up |
| sweep_growth | every group with a growth built; growth asked, reset to 0 and to 40, asked again |
| sweep_convert | every recipe of the incubator and the DNA manipulator, grown and converted |
| sweep_optimizer | both optimizers with every fuse, beside machines they speed up |
| sweep_recycle | every item put in a recycling machine and recycled |
| sweep_animals | every animal of the AnimalList spawned, its growth and id asked, petted twice, despawned |
| sweep_vehicle.A/.B | the rover crafted at its garage, its storage and every vehicle equipment, colour, taken, driven by owner transforms, left, recalled and parked, deconstructed; a second player takes it in use (refused), sits as a passenger, sees it driven, drives it once free |
| sweep_seat_0..2 | a player who never moves locks into each of the rover's seats and stays: the host's saved copy of the player shows where each seat holds it |
| sweep_sharing.A/.B | two players at one container: the second watches while the first changes it, takes from it, then tries the first player's backpack |
| sweep_scene | the scene's own world objects (ids 101xxxxxx): asked for, grown, mined, disabled, destroyed and regenerated, one set of ids per group |
| sweep_transfer | items in containers on Prime taken to the backpack by group and planet |
| sweep_system | the scene elevator's interpolation handler (start, check, register, reverse, id 0) and the toxic-area registrations |

Every script places its player by the escape pod (as the reference sees a probe that never moved
at 0,0,0) and keeps it breathing, drinking and eating (it would die after 50 s outside).

Server RPCs: the sweep sends 62 of the 124. The rest (chat, planet switching, gauges, instances,
meteo, story, players, logistics queries ...) are covered by `scripts/`, the per-mechanic
scripts of the earlier parity work; the NetworkAnimator, NetworkTransform.SetState and
PlayStation RPCs by unit tests only (no game code sends them from a client under client-server
hosting, or they are a console's).

## Bugs the sweep found (all fixed, each with a test)

| commit | what | test |
|---|---|---|
| 9fd32f6 | A player's equipment inventory took anything; the game's takes the Equipment groups only (every non-constructible group of category Equipment). | `players::tests::equipment_takes_equipment_items_only` |
| e16f81d | A drone picked up from the floor stayed a drone (Drone.OnDestroy runs when its GameObject goes, the world object staying). | `logistics::tests::a_drone_picked_up_from_the_floor_is_no_drone_any_more` |
| cba8363 | The inventory proxy of a group without a prefab of its own (a container's first proxy) answered with the wrong inventory: the linked inventory is index -1. | `handlers::tests::a_container_prefabs_proxy_tells_the_linked_inventory` |
| 06a4d6e | The volumes whose content blocks a deconstruction (pods, Biolab, domes, aquarium, growers, stairs) were guessed boxes; measured on the game, all six sides, to 1/16 m. | `machines::tests::deconstruction_volumes_are_the_measured_ones` |
| 872b83d | The rover's late-join transform state carried the unreliable-delta flags of the moving vehicle it was extracted from; the real host sends the ordinary flags. | `gamedata::tests::a_resting_vehicle_synchronizes_like_any_object` |
| 2058156 | A rover's seats left a player at the vehicle's origin, unturned; the host holds it at the seat (pilot 2.065 m up and 1.963 m ahead, passengers 0.7 m aside, 2.5 m up and 1.9 m behind, turned a quarter either way) and lets go of the copy once the seat does (measured with `sweep_seat_*`). | `animals::vehicles::tests::the_rover_can_be_entered_driven_and_left`, `the_rovers_passenger_seats_hold_a_player_turned_aside` |
| 26a7817 | An owned drop's spawn message carried DontDestroyWithOwner (0xE8); the game sends 0x68 and sets the flag right after (a vehicle's OnNetworkSpawn sets it inside the spawn). parity.py did not compare spawn flags and now does. | `objects::tests::dont_destroy_with_owner_follows_the_spawn_message_for_items_but_not_vehicles` |
| f7bc57e | An unrotated drop is saved turned 90 to 179 degrees about Y, as the game does. | `objects::tests::an_unrotated_drop_is_saved_turned_about_y` |
| aa20731 | Inventories were saved in insertion order; the game's `_allWorldInventories` is a .NET Dictionary and a freed slot goes to the next inventory created (the rover's inventories saved 191,192 where the game saves 192,191). Snapshot format 3. | `inventories_keep_the_dictionary_order` |
| daa1bdc | An algae generator off the water filled its crop inventory at once; the game accepts only a ray hit on the Water layer and gives up after 52 tries. | `machines::growers::tests::algae_grow_where_a_ray_down_meets_water` |
| 029dde9 | The crops of a vegetable grower turned about straight up; the soil's normal, found in the rotations the game made, leans 7 degrees. | `machines::growers::tests::vegetable_growers_fill_grow_and_replant_when_harvested` |
| 78171bf | A client creating a rover or a drone on the floor was told to start its physics; the real host sends nothing (it announces a dropped one, and what the server itself creates, such as a recycler's output). Found by the paired-create check. | `objects::tests::a_client_creating_a_drone_or_rover_on_the_floor_is_not_told_to_start_its_physics` |

The first passes also showed mistakes of the sweep itself (floor panels given a door look no
client sends, algae built on land): those are fixed in the scripts, not in the server.

## New deliberate deviations

- **StartInterpolation for world object id 0 is ignored** (safety): the game creates a fresh,
  saved Elevator world object for it each time a move has ended, so a client could grow the save
  at will. EXPECTED.md "Safety rules"; `sweep_system`.
- **A built ToxicityDiorama2's two wreck doors** are world objects in the game (not saved, not
  networked) and counted in `_constructedWorldObjectsGroups` (27 against 25 here): class c,
  nothing reads that number. EXPECTED.md.

Everything else listed in EXPECTED.md was there before (the reference's host player, time,
randomness, the safety rules); the sweep added its "Not comparable" items.

## What could not be compared

- **Water**: the start land has none. The ten water-only buildings are built on land in
  `sweep_water`: nine behave the same; an algae generator plants nothing on land on the
  reference, and here nothing either since daa1bdc when the planet's water volumes are known.
- **Physics**: where dropped items come to rest (some roll down a slope or fall through the
  terrain on the reference), a dropped buildable's rotation, the rover's motion between the
  owner's transforms. A saved position up to 15 m sideways from ours counts as the same settling.
- **A second escape pod**: its storage is a scene object the game keeps inactive in the copy.
- **A door on a floor panel** (no client sends it) and the 5 groups without a prefab.
- **Open**: after a recycled rover this server sends
  `UpdateNetworkRigidBodyState.SetRigidBodyEnabledClientRpc(false)` and the reference never does
  (filed under class b "what the owner of a vehicle writes"). The game's coroutine probably
  waits for `GetActivePlayerController()`, which the hidden host never has; settling it needs a
  reference run that logs it.
- **The game's randomness** is shown, not assumed: the reference was run twice (`RUN/NAME.noise`,
  `NAME.savenoise`) for every script whose differences it could explain. Growth rates read from
  a few polls, saved object order, the random state of growers, ore and disintegrator outputs,
  meteor and story timings and genetic traits come out of that.

## Measured on the game (not in the tables)

The deconstruction volumes (`machines.rs`), the rover's seats (`animals/vehicles.rs`), the
vegetable growers' growing spot and the soil's normal (`machines/growers.rs`), and the flat
ground east of the pod (`GROUND` in `sweep.py`), each by a sweep script that measures it
(`sweep.py bounds` for the volumes, `sweep_seat_*`, `sweep_feed`/`sweep_build_3`, and the item
drops of the terrain map).
