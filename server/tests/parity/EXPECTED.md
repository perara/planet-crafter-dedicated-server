# Expected differences from the real game host

What `parity.py` and `savediff.py` still report after a run of the scripts in `scripts/`
against the real game host, and why each is not a bug. Everything else they report is one.

## The reference has a host player

The real host runs a game with its own (hidden) player, the plugin's "Extractor"; this server
has none. `start.json`-style saves give that host a record of its own (id 0, `host: true`, at
the escape pod) so it does not take over a saved player.

- **New players spawn** where the host stands (PlayerStatus.GetRespawnPosition falls back to
  the host's transform); here at the first livable place, one meter above the escape pod.
  A few centimetres apart in the join script.
- **Items the server drops itself** (an inventory shrunk below its content, a recycler's
  output, dying canisters) are owned by the host, who simulates their physics: owner 0, no
  StartAndStopPhysicsAfterDelay to the probe, a `_scale` and `_transformSync` the host writes,
  and resting positions in the save. Here the nearest player gets them (README "No physics on
  the server"). The same goes for `ownershipToNearestClient` drops: the probe's own player
  never moves on the reference (it sends no transform), so the host is nearest.
- **GetPlayersPositions** answers the host's position; here nobody else is there, so no
  answer (the game sends none either when alone).
- **The probe player is saved at 0,0,0** by the reference (it never sent a transform); here at
  its spawn position. Death canisters land below it accordingly.
- **The save's `modded` is true** on the reference (BepInEx).
- **What the owner of an object writes**: a vehicle's default colour (WorldObjectColor on its
  owner), its VehicleShareData, a dropped item's `_scale` and `_transformSync`: the host owns
  them on the reference, a player here.
- **Network visibility by distance** (animals, drones, players): the reference hides them from
  the probe, whose player it sees at 0,0,0, so it sends spawns followed by despawns. This
  server shows every animal to everyone (README "Differences").
- **A space event's reference client** is the player nearest the launch: the host there. The
  host's own asteroids then drop their resources (a rocket's space event in `sweep_craft_world`);
  here the reference client is a player, whose game would.
- **Distance to the host's own player** decides more on the reference:
  - PerformancerDisableIfFarAway hides parts of a building further than 20 to 120 m from it.
    A Biolab, LightBoxMedium or Megadome1 that far from the host cannot be deconstructed there
    (DeconstructServerRpc finds no active ActionDeconstructible and answers nothing). This
    server has no host: everything is as near a player.
  - UpdateNetworkRigidBodyState gives a vehicle whose owner went further than 60 m to the
    nearest player, the host included, and tells the owner its rigidbody state by that distance
    (SetRigidBodyEnabledClientRpc).
  - The sweep scripts move their player next to the escape pod (where the host stands) first,
    which keeps most of these the same on both.

## Time

- **World units**: this server stands still until the first player joins, the reference ran
  before the probe joined. Totals are left out; rates are compared, and drifting values
  (gauges, units) by their rate over the second half of the run.
- **Network ticks** in transform states are server time.
- **Which periodic update comes first** inside one script step (sorted per step).
- **Machines and growth** run on each server's own clock: a craft, a production or a growth
  report can land a few script steps apart (parity.py lists them as shifted), and a growth
  value is compared by its rate. Prime's big lake starts to rise from TerraformStage4-Water,
  which the start save is just below; it rises over the land east of the pod a little earlier on
  the reference, and RequireEnergy sets what is under water to energy 0 (the sweep builds at
  y = 60).
- **An autocrafter's craft round** (every craftEveryXSec, the first one `_instancedAutocrafters % 50 / 10`
  s after the machine starts) runs on each server's own clock: an ingredient that arrives within
  milliseconds of a round is used by it on one server and by the next round on the other, so
  the craft's animation and the craft count (AddOneToTotalCraft) come a whole round apart.
- **Frame timing**: the reference runs at 30 frames a second (the plugin sets it), so a
  coroutine's WaitForSeconds ends up to a frame late each round; a trade rocket's growth at
  one poll can be a step behind ours (4 against 5 after 24 s; equal again later).

## Randomness in the game (Random.Range, Random.state)

- MachineOutsideGrowerUpdateStartClientRpc's random state.
- DropSomeItems on death (50 % per item).
- Unlinked blueprint chips, meteor events and their seeds, story events (which one a try
  picks, and the chance it launches: the help messages come at other moments), recycler drop
  spots, genetic traits of spawned animals, portal choices (UpdateChoices).
- What ore extractors, harvesting robots and disintegrators make (one of their groups at
  random; a disintegrator also gives its ingredients back in a random order), so the save's own machines change different groups' counts on each run; where a
  harvestable grower places its produce (so which crop a player picks near it, and where the
  next one grows); the trait a petted animal gives; the animal a DNA
  sequence without a species trait becomes (InventorySpawnContent: a random one of the
  AnimalList); the loot a built wreck container gets.

The order of the new world objects in the save (they are saved in the order they were made)
follows from these: savediff pairs them by group and place, else by rank, and a random group or
spot reorders the pairing.

Shown by running the reference twice (players: the items kept after dying; machines and
energy: the grower's random state).

## Approximations the README names

- **Wrecks are laid out by Tessera from their own seed**, on a thread of their own, a moment
  after the call (README "Differences"). Compared on 2.103 (the sweep's host, tables format 3):
  `scripts/instances.json` (3 scene wrecks, 2 world instances: 57 placed objects); 24 scene
  wrecks of the three sizes at random places, seeds and rotations (some tilted) generated one
  after the other in one process; and each of the 8 world instance scenes opened once with its
  first wreck generated. Every network object the game created, this server created too, in the
  same order, at the same place and with the same rotation. The 24 wrecks: 28 of their creation
  messages differ in writing, never in meaning: the sign of a zero (17), the third decimal of a
  position (10, at most 0.001 m, the precision a save keeps) and the sign of a whole quaternion
  (1): the same products in another order of float rounding (class b). In the saves the 1 706
  placed objects of the 24 wrecks (65 groups) are the same in both, none in one only, and the
  real game run twice puts them at exactly the same places. The 8 instances differ in 4
  messages, all of that kind. What differs is the loot inside the containers (class d): of 815
  containers 213 hold the same groups in the game against this server and in the game against
  the game (the item counts agree in 297 and 329 containers in two runs against this server, in
  305 against the game itself).
  (`wrecks.py` makes the two scripts and compares the saves and the event lists; run them with `sweep.sh both`.)
  Found and fixed by this comparison: the planters of a wreck (`TreePlanter2`, `FlowerPot2`)
  tell everyone their growth is 0 when they appear (`GrowthProxy.SetCurrentGrowthClientRpc`);
  this server did not.
  What the game keeps on its prefabs (`RandomizePropData.ignore`, set by one wreck's walk and
  read by the next one's) makes a wreck depend on the ones made before it in the process; the
  server keeps it the same way (from its start, wrecks one at a time in the order asked), and
  24 wrecks in a row agree. A wreck asked for twice, or a flip in the middle of one, makes it
  the same: the flags are in the snapshot.
- **A save from before this change keeps its wrecks.** A wreck a version of this server
  generated holds what it generated; one it had not yet generated is laid out on the seed it
  has, as before (the seeds of the 64-layout pool are seeds like any other).
- **The late-game save's 8 wreck generators (version 15)** are dropped at load by the game and
  by this server alike: all 8 generators and their objects (wreckpilar 202 to 187,
  ProceduralWreckContainer1 45 to 19, ProceduralWreckSafe 52 to 20, ... the same counts in
  both), measured again after this change.
- **Scene objects inside a built prefab**: ToxicityDiorama2 holds two wreck doors
  (WorldObjectFromScene, group WreckPilar). The game registers them as world objects when the
  diorama is built (not saved, no network object) and counts them in
  `_constructedWorldObjectsGroups`, where they stayed counted after the diorama was deconstructed in the
  reference run (measured, not read from the code). This server makes neither, so that count of
  WreckPilar is 2 lower per diorama built. Nothing else the server sends or saves differs; the
  doors would need world object ids of their own for a number nobody reads.

## Safety rules (deliberate)

- **StartInterpolation for world object id 0** is ignored. The game finds no world object of
  that id and creates a fresh one of the interpolation group (an Elevator), marks it saved and
  starts a move on it; each call after a move has ended makes one more, so a client could grow
  the save at will. The reference does this (`sweep_system`: one more Elevator counted).

- **Other players' backpacks and equipment** are refused (TransferAll, GetOrCreate,
  AddNewItem... on inventory 3 in `inventories.json`): AddOrRemoveItem false, an empty
  RetrieveInventory. The game's host takes them from anyone.
- **Unknown group hashes** in UnlockGroupGlobally are refused; the game adds any int to
  `_unlockedGroups`, and from then on its saves fail: SaveWorldData's GetGroupsStringIds
  throws on the unknown hash and the catch drops the save (the reference wrote no save after
  `crafting.json`, and did without the 12345 step).
- **A failed AddNewItemToInventory destroys the object it made.** The game leaves it in the
  world (saved as `{"id":…,"gId":…}`), one more per call against a full inventory.
- **A direct-IP player who claims id 0** gets an id from its name; the game keeps every such
  player under id 0, so they overwrite each other's save record. Scripts claim a Steam id.
- **A second connection under a name already playing** is refused (README "Differences"); the
  game lets two players share a name. The sweep's second player has a name of its own.
- **The loading-RPC gate, owner-write list, chat limits and RPC budget** never show in these
  scripts (they only send what a game client sends).

## Not comparable

- **Water**: the start save's land has none (Prime's lake is still to rise). The ten buildings
  the game lets a player place only on water (`ConstraintOnSurfaces` with SurfaceWater alone:
  AlgaeGenerator1/2, FishFarm1/2, ToxicWaterCollector1..3, TreeSpreader0, WaterCollector2,
  WaterLifeCollector1) are built on dry land in `sweep_water`; there a client could not place
  them. Nine behave the same. An algae generator plants on the water surface below it: the
  reference plants nothing on land, this server fills the generator as if it stood on water.
- **The DLC planets (Humble, Toxicity)**: nothing of them is compared with the game. The reference
  is the game under Wine without Steam: `SteamManager` is not initialised, so
  `PlanetList.GetIsPlanetPurchased` is false for a DLC planet, `GetPlanetIndex` gives -1 for it and
  the host disconnects a world saved there (`PlanetNetworkLoader.LoadPlanetFromSave`). It could be
  made to run one only by a Steam stand-in or a patch of `PlanetList`, and a faked ownership is not
  done, here or in the server (the server asks the operator instead: the rule `world.dlcPlanets` is their statement of ownership). What the server does with them is checked another way (COMPLETENESS.md
  "DLC planets"). The few things that would differ, if a licensed host were compared: the host game
  knows the toxic areas of the sectors around its own player only (here all of the planet's, so a
  remote player in a far area is hurt here and not there); an area's key is made from the dump's
  position and a client's key a rounding step off still means the area; the clients' registrations
  for an area are the server's, kept through a hot update.
- **A door on a floor** (Panel.ChangePanelServerRpc with a wall look for a floor panel, which no
  client sends): the game then also resets, at deconstruction, one of two floor panels eight
  metres apart that meet through the door look's large bounds; this server does not.
- **Groups without a prefab** (ContainerAqualis, ContainerGoldenAqualis, GenerationGroupVein,
  ProceduralInstanceGenerator, WreckServer): nothing to spawn in either host.

- **A second escape pod**: its storage is the start save's scene object (WorldUniqueId
  103976415), which the game keeps inactive in the copy, so the copy cannot be deconstructed
  there; no client deconstructs a pod through GroupNetworkBase. The three pods `sweep_build_1`
  builds (EscapePod, Humble, Toxicity) stay on the reference: they stay in its world object counts
  and its save, and their deconstruction shows as a difference here.
- **Physics the host simulates**: resting places of dropped items (some fall through the
  terrain on the reference), a dropped buildable's rotation, the truck's motion between the
  owner's transforms.

## Save layout

- `pcdsCurrentPlanet` in the first section: the planet this server is on (the game derives it
  from the host player's record, which this server has none of); the game ignores the field.
- The `host` flag goes to the last Steam player saved on the server's planet (mark_host), so
  the save loads that planet when moved back into the game.
- **Scene objects picked up** (101333311 in `worldobjects.json`) are saved at the end here; the
  game made a world object for every scene object at load, so a picked one keeps that early
  place (after the saved objects, before this session's new ones).

## Old saves (`pcds world import`)

What the game writes after it opens a save of the 2022 layout (9 sections, one planet), against what this server writes
after `pcds world import` of the same file and a start and a stop (`savediff.py IMPORTED REF OURS`, the same file on the
game under Wine). `mkold.py` makes such a save from a current one (the sweep's start save) for the two other layouts
the game converts: the one between (`--tokens`: unit levels and terraTokens in the first section) and current sections with
old game settings (`--settings-only`); the world state, the planet, the player and the settings of both come out as the
game's.

The same: the 5 129 distinct world objects (the file lists seven ids twice; the game and this server keep one of each,
the same one), their fields (a field the game no longer has, `wear`, is dropped by both), 358 inventories (329 and the 29 the
machines make), the planet's units, the world state, stats, messages, story events, the settings (the game mode becomes its dying
consequences and gauge drain: Chill 0.8 and no consequences, Intense 1.2, Hardcore 1.6). The colour layers are read by no one.
The player's `unlockedGroups` are lost by the game's conversion (its text patch moves them into the unit-levels record, which
becomes the planet record, which has no such field) and by this one; a save between the layouts keeps them.

Differences left, none a bug:

- **The reference's host player** takes the saved player's place (`host`, `planetId`; see "The reference has a host player").
  `modded` is true on the reference (BepInEx). `hasPlayedIntro` is set true by the reference's host; the import keeps the file's.
- **`worldSeed`**: the game draws a new one at every load of a file without one; the import draws one, once, and the world keeps it.
  `version` is the tables' game version (empty when the import runs before the tables exist).
- **Algae seeds** (165 on the game and here) and the growth and rotation of the growables: the game's randomness and timing (class d).
- **Floating buildings** (AlgaeGenerator, TreeSpreader0, WaterCollector2: the water-only ones) are 0.6094 higher on the game: it floats
  them to the water level of today's terraformation, which is higher than the one the 2022 save was made at; no physics here
  ("No physics on the server").
- **Signs** get the default colour here (COMPLETENESS gap 11) and **teleporters without an order number**: the game's
  `ActionUseTeleporter` numbers them from a client's script at the first spawn (`set`); here the first real client does.
- `pcdsCurrentPlanet` in the first section (see "Save layout").

