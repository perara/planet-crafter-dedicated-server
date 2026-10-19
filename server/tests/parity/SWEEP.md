# The parity sweep

Every group of the game, every interaction its prefab carries and every vehicle, run on the real
game host and on this server from the same start save, and every difference classified.
`sweep.py gen` writes the scripts from the tables (nothing is hand-picked); `sweep.sh` runs them;
`sweep.py report` builds the matrix below from the runs; `sweep_classes.json` holds the reasons a
difference is not a bug. README "Parity with the game" says how to bring the reference up.

## How to rerun

```bash
cd server/tests/parity
python3 sweep.py gen DATA_DIR OUT                     # OUT/<script>.json, OUT/rows.json
REF_DIR=... DATA_DIR=... START=start.json \
  agent-run planetcrafter --cpus 6 -- ./sweep.sh both RUN OUT/sweep_*.json   # reference and ours
REF_DIR=... DATA_DIR=... START=start.json \
  agent-run planetcrafter --cpus 6 -- ./sweep.sh ref2 RUN OUT/sweep_*.json   # the reference again
python3 sweep.py report DATA_DIR OUT RUN [CS_DIR]     # RUN/matrix.md, classified.json, components.md
```

`START` is the start save with a host record of its own (README). `RUN/NAME.diff` is what
parity.py found between the game and this server, `RUN/NAME.noise` between two runs of the game:
a difference of the same kind there is the game's own randomness or timing (class d). Every other
difference is matched against `sweep_classes.json` (classes b, c, e); what matches nothing is
`unclassified`, a bug until shown otherwise.

The scripts (one player each unless named `.A`/`.B`, a pair run together):

| script | what it exercises |
|---|---|
| sweep_items_1..4 | every item: created twice in a scratch inventory, both moved to the backpack (TransferAllSameGroup), one moved back and destroyed, the other dropped (DropOnFloor), picked up (AddWorldObjectToInventory) and destroyed; then one created on the floor (CreateAndDropOnFloor) |
| sweep_use | every consumable used as the inventory screen does (Eat/Drink/Breath/Purify with its value, GainTokens, drop to consume, build a buildable item); every equipable equipped and taken off with PlayerEquipment's follow-ups (inventory and equipment size, backpack/jetpack/light/skin status, oxygen tank, rebreather); something that is no equipment into the equipment |
| sweep_craft_1..2 | every item with a recipe crafted at a station that crafts it, as CraftManager does |
| sweep_craft_world | every item crafted in the world: rockets at a launch platform (then sent into space) and drones at a drone station |
| sweep_autocraft_AutoCrafter1_1..3, _Incubator2_1 | every recipe through the server's own crafting: an autocrafter set to it, the ingredients in a container beside it |
| sweep_build_1..3 | every constructible built, each of its network behaviours used (ids, details, inventories, text, colour, setting, linked groups and planet, growth, interactive state, panels, craft animation, outside grower start), an input given to machines that take one, five minutes to run, everything read again, deconstruction refused while its inventory is not empty and done once empty |
| sweep_feed | every seed, larva, egg, food, fuse, fuel, key, skin and DNA item in a machine that takes it (each such machine at least once), six minutes of growing, hatching and producing, then the harvest picked up |
| sweep_convert | every recipe of the incubator and the DNA manipulator, grown and converted |
| sweep_optimizer | both optimizers with every fuse, beside machines they speed up |
| sweep_animals | every animal of the AnimalList spawned, its growth and id asked, petted twice, despawned |
| sweep_vehicle.A/.B | the truck crafted at its garage, its storage and every vehicle equipment, colour, taken, driven by owner transforms, left, recalled and parked, deconstructed; a second player takes it in use (refused), sits as a passenger, sees it driven, drives it once free |
| sweep_sharing.A/.B | two players at one container: the second watches while the first changes it, takes from it, then tries the first player's backpack |

Every script places its player by the escape pod (as the reference sees a probe that never moved
at 0,0,0) and keeps it breathing, drinking and eating (it would die after 50 s outside).
