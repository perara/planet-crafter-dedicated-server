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
- **A space event's reference client** is the player nearest the launch: the host there.

## Time

- **World units**: this server stands still until the first player joins, the reference ran
  before the probe joined. Totals are left out; rates are compared, and drifting values
  (gauges, units) by their rate over the second half of the run.
- **Network ticks** in transform states are server time.
- **Which periodic update comes first** inside one script step (sorted per step).
- **Frame timing**: the reference runs at 30 frames a second (the plugin sets it), so a
  coroutine's WaitForSeconds ends up to a frame late each round; a trade rocket's growth at
  one poll can be a step behind ours (4 against 5 after 24 s; equal again later).

## Randomness in the game (Random.Range, Random.state)

- MachineOutsideGrowerUpdateStartClientRpc's random state.
- DropSomeItems on death (50 % per item).
- Unlinked blueprint chips, meteor events and their seeds, recycler drop spots, genetic
  traits of spawned animals, portal choices (UpdateChoices).

Shown by running the reference twice (players: the items kept after dying; machines and
energy: the grower's random state).

## Approximations the README names

- **Procedural wrecks come in 64 layouts per size** (README "Differences"): a wreck generated
  here, outside or inside a world instance, holds the layout of its pool seed, not of the
  game's seed. Two reference runs of `instances.json` generate the same objects; ours differ.
  Opening the instance itself (seed 400000032) matches.

## Safety rules (deliberate)

- **Other players' backpacks and equipment** are refused (TransferAll, GetOrCreate,
  AddNewItem... on inventory 3 in `inventories.json`): AddOrRemoveItem false, an empty
  RetrieveInventory. The game's host takes them from anyone.
- **Unknown group hashes** in UnlockGroupGlobally are refused; the game adds any int to
  `_unlockedGroups` (a client could grow that list without end).
- **A failed AddNewItemToInventory destroys the object it made.** The game leaves it in the
  world (saved as `{"id":…,"gId":…}`), one more per call against a full inventory.
- **A direct-IP player who claims id 0** gets an id from its name; the game keeps every such
  player under id 0, so they overwrite each other's save record. Scripts claim a Steam id.
- **The loading-RPC gate, owner-write list, chat limits and RPC budget** never show in these
  scripts (they only send what a game client sends).

## Save layout

- `pcdsCurrentPlanet` in the first section: the planet this server is on (the game derives it
  from the host player's record, which this server has none of); the game ignores the field.
- The `host` flag goes to the last Steam player saved on the server's planet (mark_host), so
  the save loads that planet when moved back into the game.
- **Scene objects picked up** (101333311 in `worldobjects.json`) are saved at the end here; the
  game made a world object for every scene object at load, so a picked one keeps that early
  place (after the saved objects, before this session's new ones).
