"""The exhaustive parity sweep: probe scripts generated from the game tables, so every group, every
interaction its prefab carries and every vehicle is exercised (SWEEP.md has the matrix).

    python3 sweep.py gen DATA_DIR OUT_DIR [START.json]
                                                write OUT_DIR/<script>.json and OUT_DIR/rows.json
                                                (START: the start save, for the scene objects' ids)
    python3 sweep.py report DATA_DIR OUT_DIR RUN_DIR [CS_DIR]
                                                the matrix with each row's result, from the
                                                parity.py diffs in RUN_DIR (<script>.diff),
                                                every difference classified by sweep_classes.json
                                                (RUN_DIR/matrix.md, classified.json); with the
                                                decompiled game code, the RPCs each component
                                                sends (components.md)

DATA_DIR holds the tables (gamedata.json, netdata.json, rpcs.json). Every call a script makes
carries a "tag": "Group|interaction", which the probe prints and parity.py shows on each
differing step, so a difference names the row it belongs to. A row the sweep cannot exercise
says why in rows.json; nothing is left out silently.
"""
import json
import math
import os
import re
import sys

# The world the scripts run in: the start save's Prime (README "Parity with the game"). Everything
# is built in the empty land east of the escape pod, within 200 m of it: RequireEnergy switches a
# machine off further than that from every player, and the reference's host stands at the pod.
POD = (598.74, 682.76)
GROUND_Y = 3.0
# Buildings stand higher: the start save is just below TerraformStage4-Water, when Prime's big lake
# starts to rise over this land (up to about 30 m), and RequireEnergy turns off what is under water.
BUILD_Y = 60.0
PRIME = 'Prime'
# DataConfig.BuildPanelSubType: WallDoor, FloorGlass, FloorAngleGlass.
PANEL_LOOK = {'Wall': 4, 'Floor': 6, 'FloorAngle': 15}
# Flat ground in the start save, heights measured on the real host with items dropped on a 20 m
# grid (x, y, z): the outside growers need terrain under them to plant on.
GROUND = [(520, 1.5, 560), (520, 0.9, 580), (520, 0.8, 600), (520, 1.0, 620), (520, 1.0, 640),
          (540, 1.7, 560), (540, 1.2, 580), (540, 0.8, 600), (540, 1.0, 620), (540, 1.3, 640), (540, 1.1, 660),
          (560, 1.5, 560), (560, 1.2, 580), (560, 0.9, 620),
          (580, 1.4, 620), (580, 1.1, 640), (580, 0.9, 660), (580, 1.2, 680), (580, 1.5, 700),
          (600, 1.7, 660), (600, 1.7, 680), (600, 0.8, 700), (600, 1.1, 720)]
# A fusion generator: 1485 units of energy each; every build script adds enough of them that the
# planet's energy stays positive with all its machines running.
GENERATOR = 'EnergyGenerator6'


class Tables:
    def __init__(self, data_dir):
        d = json.load(open(f'{data_dir}/gamedata.json'))
        self.assets = d['assets']
        self.layouts = d['layouts']
        self.prefabs = d['groupPrefabs']
        self.panels = d['panels']['groups']
        self.groups = {}
        for key, v in self.assets.items():
            kind = key.split(':')[0]
            if kind in ('GroupDataItem', 'GroupDataConstructible'):
                self.groups[v['id']] = dict(v, kind='item' if kind == 'GroupDataItem' else 'constructible', asset=key)
        self.by_asset = {g['asset']: gid for gid, g in self.groups.items()}
        rpcs = json.load(open(f'{data_dir}/rpcs.json'))
        self.server_rpcs = sorted(r['name'] for r in rpcs.values() if r['to'] == 'server')

    def ref(self, asset):
        """The group id an asset reference ("GroupDataItem:X") names."""
        if asset not in self.by_asset:
            raise KeyError(f'no group for {asset}')
        return self.by_asset[asset]

    def recipe(self, gid):
        return [self.ref(a) for a in self.groups[gid]['recipeIngredients']]

    def components(self, gid, ty=None):
        cs = self.prefabs.get(gid, {}).get('components', [])
        return [c for c in cs if ty is None or c['type'] == ty]

    def behaviours(self, gid):
        layout = self.layouts.get(gid)
        return layout['behaviours'] if layout else []

    def behaviour(self, gid, cls):
        return [b['behaviourId'] for b in self.behaviours(gid) if b['class'] == cls]

    def items(self):
        return sorted(g for g, v in self.groups.items() if v['kind'] == 'item')

    def water_only(self, gid):
        """A building the game lets a player place only on water (ConstraintOnSurfaces): the
        start save's land has none, so these are built together in sweep_water."""
        return any(c['fields']['allowedTaggedSurfaces'] == ['SurfaceWater'] for c in self.components(gid, 'ConstraintOnSurfaces'))

    def constructibles(self, wet=False):
        """Every constructible; the buildings for water only when `wet` (and only they then)."""
        return sorted(g for g, v in self.groups.items() if v['kind'] == 'constructible' and self.water_only(g) == wet)


# Where the probe player stands: by the escape pod, a little towards the build land, so the
# reference (which otherwise sees a player that never moved at 0,0,0) and this server measure the
# same distances to it (visibility, nearest player, vehicle ownership). The host stands at the pod.
STAND = [606.0, 2.5, 680.0]
# Gauges run out (oxygen after about 50 s outside): the probe breathes, drinks and eats this often.
KEEP_ALIVE_MS = 30000


class Script:
    """One probe script; steps are tagged "Group|interaction". It starts by placing the player
    at STAND and keeps it alive."""

    def __init__(self, name, player_id, stand=STAND):
        self.name, self.id, self.steps, self.rows = name, player_id, [], {}
        self.since_alive = 0
        self.move(stand)

    def move(self, position, tag='player|stand'):
        self.steps.append({'transform': 'player:ClientNetworkTransform', 'position': list(position), 'after': 300, 'tag': tag})

    def keep_alive(self, ms):
        self.since_alive += ms
        if self.since_alive >= KEEP_ALIVE_MS:
            self.since_alive = 0
            for rpc in ('BreathServerRpc', 'DrinkServerRpc', 'EatServerRpc'):
                self.steps.append({'rpc': f'PlayerGaugesHandler.{rpc}', 'on': 'player:PlayerGaugesHandler', 'args': [100], 'after': 100, 'tag': 'player|keep alive'})

    def call(self, rpc, on, args, tag, after=None, cap=None, create=False):
        self.keep_alive(after or 300)
        step = {'rpc': rpc, 'on': on, 'args': args, 'tag': tag}
        if after:
            step['after'] = after
        if cap:
            var, cap_rpc, arg = cap
            step['capture'] = {'var': var, 'rpc': cap_rpc, 'arg': arg}
        if create:
            step['capture'] = {'var': create, 'create': True}
        self.steps.append(step)
        group, interaction = tag.split('|', 1)
        self.rows.setdefault((group, interaction), 0)
        self.rows[(group, interaction)] += 1

    def wait(self, ms):
        while ms > 0:
            self.steps.append({'wait': min(ms, KEEP_ALIVE_MS)})
            self.keep_alive(min(ms, KEEP_ALIVE_MS))
            ms -= KEEP_ALIVE_MS

    def json(self):
        # The second player of a pair needs a name of its own: a name already online is refused
        # here (README "Differences").
        return {'name': 'SweepB' if self.name.endswith('.B') else 'Sweep', 'id': self.id, 'steps': self.steps}


# ---- the RPCs, as the game's client sends them --------------------------------------------------

INV = 'InventoriesHandler'
WOH = 'WorldObjectsHandler'


def add_new(s, group, inv, tag, var=None):
    s.call(f'{INV}.AddNewItemToInventoryServerRpc', INV, [f'#{group}', inv], tag,
           cap=(var, f'{INV}.AddNewItemClientRpc', 1) if var else None)


def retrieve(s, inv, tag, after=None):
    s.call(f'{INV}.GetOrCreateNewInventoryServerRpc', INV, [False, 0, inv, 0, None, None], tag, after=after)


def remove(s, wo, inv, destroy, tag):
    s.call(f'{INV}.RemoveItemFromInventoryServerRpc', INV, [wo, inv, destroy], tag)


def build(s, group, pos, tag, var, rot=(0, 0, 0, 1), after=800):
    s.call(f'{WOH}.CreateAndInstantiateWorldObjectServerRpc', WOH, [f'#{group}', list(pos), list(rot), False, False, True, False], tag,
           after=after, cap=(var, f'{WOH}.CreateAndInstantiateWorldObjectClientRpc', 0))


def inventory_of(s, t, group, obj, tag, prefix):
    """GetInventoryId on each InventoryAssociatedProxy: the variables prefix0, prefix1, ..."""
    names = []
    for k, b in enumerate(t.behaviour(group, 'InventoryAssociatedProxy')):
        var = f'{prefix}{k}'
        s.call('InventoryAssociatedProxy.GetInventoryIdServerRpc', [f'${obj}', b], [], tag,
               cap=(var, 'InventoryAssociatedProxy.GetInventoryIdClientRpc', 2))
        names.append(var)
    return names


def grid(origin, cols, spacing, y=BUILD_Y):
    """Positions on a grid from origin, row by row."""
    # A little off the grid, never the same twice: equal distances to an optimizer or a station
    # make the game's unstable sort pick any of the tied machines.
    i = 0
    while True:
        yield (round(origin[0] + (i % cols) * spacing + (i * 0.37) % 1.3, 2), y, round(origin[1] + (i // cols) * spacing + (i * 0.71) % 1.7, 2))
        i += 1


def prelude(s, size=300):
    """A scratch inventory ($bin) and an empty backpack."""
    s.call(f'{INV}.GetOrCreateNewInventoryServerRpc', INV, [True, size, 0, 0, None, None], 'player|scratch inventory',
           cap=('bin', f'{INV}.RetrieveInventoryClientRpc', 1))
    s.call(f'{INV}.TransferAllItemsServerRpc', INV, ['$inventory', '$bin'], 'player|scratch inventory')


def power(s, t, groups, origin):
    """Enough fusion generators for the energy these groups draw."""
    need = -sum(min(0.0, t.groups[g].get('unitGenerationEnergy', 0.0)) for g in groups)
    n = math.ceil(need / t.groups[GENERATOR]['unitGenerationEnergy']) + 1
    for i, p in zip(range(n), grid(origin, 4, 8)):
        build(s, GENERATOR, p, f'{GENERATOR}|power for the run', f'power{i}')


# ---- families -----------------------------------------------------------------------------------

def items_family(t, chunk):
    """Every item: created, two of it moved together between inventories, moved back, destroyed,
    dropped, picked up and destroyed, and created on the floor."""
    s = Script(f'sweep_items_{chunk[0]}', 76561197999990001)
    prelude(s)
    spots = grid((620.0, 720.0), 20, 1.5, GROUND_Y)
    for i, g in enumerate(chunk[1]):
        a, b = f'a{i}', f'b{i}'
        add_new(s, g, '$bin', f'{g}|create', a)
        add_new(s, g, '$bin', f'{g}|create', b)
        s.call(f'{INV}.TransferAllSameGroupServerRpc', INV, ['$bin', '$inventory', f'#{g}'], f'{g}|move all of a group')
        s.call(f'{INV}.TransferItemServerRpc', INV, ['$inventory', '$bin', f'${b}'], f'{g}|move one')
        remove(s, f'${b}', '$bin', True, f'{g}|destroy')
        remove(s, f'${a}', '$inventory', False, f'{g}|drop')
        s.call(f'{WOH}.DropOnFloorServerRpc', WOH, [f'${a}', list(next(spots)), 0.0, True, False, 0, 0.0], f'{g}|drop', after=500)
        s.call(f'{INV}.AddWorldObjectToInventoryServerRpc', INV, [f'${a}', f'#{g}', '$inventory', True], f'{g}|pick up')
        remove(s, f'${a}', '$inventory', True, f'{g}|destroy')
        s.call(f'{WOH}.CreateAndDropOnFloorServerRpc', WOH, [f'#{g}', list(next(spots)), 0.0, True, False, 0.0], f'{g}|create on the floor', after=500)
    retrieve(s, '$inventory', 'player|final inventory', after=1000)
    retrieve(s, '$bin', 'player|final inventory')
    return s


GAUGE_RPC = {'Eatable': 'EatServerRpc', 'Drinkable': 'DrinkServerRpc', 'Breathable': 'BreathServerRpc', 'Purify': 'PurifyServerRpc'}


def use_family(t):
    """Every consumable used as the game's inventory screen does, every equipable equipped and
    taken off with the follow-ups PlayerEquipment sends, then the gauges left to run."""
    s = Script('sweep_use', 76561197999990002)
    prelude(s)
    spots = grid((620.0, 760.0), 10, 2.0, GROUND_Y)
    for i, g in enumerate(t.items()):
        v = t.groups[g]
        use, value, var = v['usableType'], v['value'], f'u{i}'
        if use in GAUGE_RPC:
            add_new(s, g, '$inventory', f'{g}|use: {use}', var)
            s.call(f'PlayerGaugesHandler.{GAUGE_RPC[use]}', 'player:PlayerGaugesHandler', [value], f'{g}|use: {use}')
            remove(s, f'${var}', '$inventory', True, f'{g}|use: {use}')
        elif use == 'AquireTokens':
            add_new(s, g, '$inventory', f'{g}|use: tokens', var)
            s.call('TokensHandler.GainTokensServerRpc', 'TokensHandler', [value], f'{g}|use: tokens')
            remove(s, f'${var}', '$inventory', True, f'{g}|use: tokens')
        elif use == 'ConsumeByDroping':
            add_new(s, g, '$inventory', f'{g}|use: drop to consume', var)
            remove(s, f'${var}', '$inventory', False, f'{g}|use: drop to consume')
            s.call(f'{WOH}.DropOnFloorServerRpc', WOH, [f'${var}', list(next(spots)), 0.0, True, False, 1, 0.0], f'{g}|use: drop to consume', after=1500)
        elif use == 'Buildable':
            build(s, g, next(spots), f'{g}|use: build', var)
    s.wait(5000)
    for i, g in enumerate(t.items()):
        v = t.groups[g]
        ty = v['equipableType']
        if ty == 'Null':
            continue
        var, tag = f'e{i}', f'{g}|equip: {ty}'
        add_new(s, g, '$inventory', tag, var)
        s.call(f'{INV}.TransferItemServerRpc', INV, ['$inventory', '$equipment', f'${var}'], tag)
        equipment_follow_ups(s, g, v, True, tag)
        s.wait(1500)
        tag = f'{g}|unequip: {ty}'
        s.call(f'{INV}.TransferItemServerRpc', INV, ['$equipment', '$inventory', f'${var}'], tag)
        equipment_follow_ups(s, g, v, False, tag)
        remove(s, f'${var}', '$inventory', True, tag)
    # Something that is no equipment, into the equipment (the inventory refuses it).
    add_new(s, 'Iron', '$inventory', 'Iron|equip: not equipment', 'noequip')
    s.call(f'{INV}.TransferItemServerRpc', INV, ['$inventory', '$equipment', '$noequip'], 'Iron|equip: not equipment')
    s.wait(20000)
    retrieve(s, '$inventory', 'player|final inventory')
    retrieve(s, '$equipment', 'player|final inventory')
    return s


def equipment_follow_ups(s, g, v, added, tag):
    """PlayerEquipment.UpdateAfterEquipmentChange: what the client sends after the transfer."""
    ty, value = v['equipableType'], v['value']
    tier = int(g[-1]) if g[-1].isdigit() else 0
    if ty == 'BackpackIncrease':
        s.call(f'{INV}.SetInventorySizeServerRpc', INV, ['$inventory', value if added else -value, [POD[0], GROUND_Y, POD[1]]], tag)
        s.call('PlayersDataManager.SetBackpackStatusServerRpc', 'PlayersDataManager', [added, tier], tag)
    if ty == 'EquipmentIncrease':
        s.call(f'{INV}.SetInventorySizeServerRpc', INV, ['$equipment', value if added else -value, [POD[0], GROUND_Y, POD[1]]], tag)
    if ty == 'OxygenTank':
        s.call('PlayerGaugesHandler.UpdateGaugesDependingOnEquipmentServerRpc', 'player:PlayerGaugesHandler', ['$equipment'], tag)
    if ty == 'AirFilter':
        s.call('PlayerGaugesHandler.SetHasRebreatherServerRpc', 'player:PlayerGaugesHandler', ['$equipment'], tag)
    if ty == 'Jetpack':
        s.call('PlayersDataManager.SetJetpackStatusServerRpc', 'PlayersDataManager', [added, tier], tag)
    if ty == 'MultiToolLight':
        s.call('PlayersDataManager.SetLightStatusServerRpc', 'PlayersDataManager', [added, value], tag)
    if ty == 'Skin':
        s.call('PlayersDataManager.SetSkinStatusServerRpc', 'PlayersDataManager', [value if added else 0], tag)
    s.call(f'{INV}.CheckInventoryWatchAndDirtyServerRpc', INV, ['$inventory'], tag)
    s.call(f'{INV}.CheckInventoryWatchAndDirtyServerRpc', INV, ['$equipment'], tag)


def stations(t):
    """craftableIdentifier -> the constructible with an ActionCrafter for it (the escape pods'
    T1 crafter: the save's own pod)."""
    out = {}
    for g in t.constructibles() + t.constructibles(wet=True):
        for c in t.components(g, 'ActionCrafter'):
            ident = c['fields']['craftableIdentifier']
            if ident != 'Null' and not g.startswith('EscapePod'):
                out.setdefault(ident, g)
        for c in t.components(g, 'ActionOpenable'):
            ident = c['fields'].get('craftableType')
            if ident not in (None, 'Null'):
                out.setdefault(ident, g)
    return out


def craft_spawn(t, station, pos):
    """ActionCrafter.craftSpawn of a station built at pos (no rotation)."""
    c = t.components(station, 'ActionCrafter')[0]
    d = c['poses']['craftSpawn']['position']
    return [round(pos[i] + d[i], 4) for i in range(3)], c['fields']['cantCraftIfSpawnContainsObject']


def craft_family(t, name, groups, player_id):
    """Every item with a recipe crafted at a station that crafts it, as CraftManager does: the
    ingredients leave the backpack, the item comes in (or, crafted in the world, is built at the
    station's craft spawn), the station animates, the craft counts. A rocket is then sent into
    space from its launch platform (ActionSendInSpace)."""
    s = Script(name, player_id)
    prelude(s)
    st = stations(t)
    built, at = {}, {}
    used = sorted({t.groups[g]['craftableInList'][0] for g in groups} - {'CraftStationT1'})
    power(s, t, [st[i] for i in used], (630.0, 790.0))
    spots = grid((660.0, 600.0), 5, 24.0)
    for ident in used:
        built[ident] = f'st_{ident}'
        at[ident] = next(spots)
        build(s, st[ident], at[ident], f'{st[ident]}|build (station)', f'st_{ident}')
    for i, g in enumerate(groups):
        ident = t.groups[g]['craftableInList'][0]
        tag = f'{g}|craft at {ident}'
        recipe = t.recipe(g)
        for ing in recipe:
            add_new(s, ing, '$inventory', tag)
        if ident in built:
            ccb = t.behaviour(st[ident], 'ConstructibleProxy')
            s.call('ConstructibleProxy.StartCraftAnimationServerRpc', [f'${built[ident]}', ccb[0]], [f'#{g}'], tag)
        if t.groups[g]['craftedInWorld']:
            spot, check = craft_spawn(t, st[ident], at[ident])
            s.call(f'{WOH}.CreateAndInstantiateWorldObjectServerRpc', WOH, [f'#{g}', spot, [0, 0, 0, 1], True, check, True, False], tag,
                   after=800, cap=(f'c{i}', f'{WOH}.CreateAndInstantiateWorldObjectClientRpc', 0))
            s.call(f'{INV}.RemoveItemsFromInventoryServerRpc', INV, [[f'#{x}' for x in recipe], '$inventory', True], tag)
            s.call(f'{WOH}.AddOneToTotalCraftServerRpc', WOH, [], tag)
            if t.components(st[ident], 'ActionSendInSpace') and t.components(g, 'MachineRocket'):
                tag = f'{g}|launch'
                s.wait(1500)
                s.call('ConstructibleProxy.SendInSpaceServerRpc', [f'${built[ident]}', ccb[0]], [], tag, after=1500)
                s.wait(20000)
            continue
        s.call(f'{INV}.RemoveItemsFromInventoryServerRpc', INV, [[f'#{x}' for x in recipe], '$inventory', True], tag)
        add_new(s, g, '$inventory', tag, f'c{i}')
        s.call(f'{WOH}.AddOneToTotalCraftServerRpc', WOH, [], tag)
        remove(s, f'$c{i}', '$inventory', True, tag)
    retrieve(s, '$inventory', 'player|final inventory', after=1000)
    return s


def autocraft_family(t, crafter, chunk, cells=8):
    """Every recipe through the server's own crafting (MachineAutoCrafter): ingredients in a
    container beside an autocrafter set to the item, a craft round, its output read back."""
    s = Script(f'sweep_autocraft_{crafter}_{chunk[0]}', 76561197999990004)
    prelude(s)
    power(s, t, [crafter] * cells + ['Container2'] * cells, (640.0, 790.0))
    rng = t.components(crafter, 'MachineAutoCrafter')[0]['fields']['range']
    spacing = 2 * rng + 10
    origin = (640.0, 600.0)
    for k in range(cells):
        x, z = origin[0] + (k % 3) * spacing, origin[1] + (k // 3) * spacing
        build(s, crafter, (x, BUILD_Y, z), f'{crafter}|build (autocrafter)', f'ac{k}')
        inventory_of(s, t, crafter, f'ac{k}', f'{crafter}|inventory', f'acinv{k}_')
        build(s, 'Container2', (x + 4, BUILD_Y, z), 'Container2|build (supply)', f'sup{k}')
        inventory_of(s, t, 'Container2', f'sup{k}', 'Container2|inventory', f'supinv{k}_')
    lg = t.behaviour(crafter, 'LinkedGroupsProxy')[0]
    every = t.components(crafter, 'MachineAutoCrafter')[0]['fields']['craftEveryXSec']
    for r in range(0, len(chunk[1]), cells):
        batch = chunk[1][r:r + cells]
        for k, g in enumerate(batch):
            # The group first: ingredients of this recipe can make the cell's previous one too.
            tag = f'{g}|autocraft at {crafter}'
            s.call('LinkedGroupsProxy.SetLinkedGroupsServerRpc', [f'$ac{k}', lg], [[f'#{g}']], tag)
            for ing in t.recipe(g):
                add_new(s, ing, f'$supinv{k}_0', tag)
        s.wait(int(every * 2000 + 1000))
        for k, g in enumerate(batch):
            tag = f'{g}|autocraft at {crafter}'
            retrieve(s, f'$acinv{k}_0', tag)
            retrieve(s, f'$supinv{k}_0', tag)
            s.call(f'{INV}.RemoveAndDestroyAllItemsFromInventoryServerRpc', INV, [f'$acinv{k}_0'], tag)
            s.call(f'{INV}.RemoveAndDestroyAllItemsFromInventoryServerRpc', INV, [f'$supinv{k}_0'], tag)
    return s


# What to put into a machine for it to work: by the inventory's authorization, else by machine.
def inputs_for(t, g):
    auth = [c['fields'] for c in t.components(g, 'InventoryAuthorizedGroups') if c['path'] == '']
    items = t.items()
    if auth:
        a = auth[0]
        ok = [i for i in items if (not a['authorizedItemCategories'] or t.groups[i]['itemCategory'] in a['authorizedItemCategories'])
              and (not a['authorizedItemSubCategories'] or t.groups[i]['itemSubCategory'] in a['authorizedItemSubCategories'])
              and t.groups[i]['itemCategory'] not in a['unauthorizedItemCategories']]
        if a['authorizedItemCategories'] or a['authorizedItemSubCategories']:
            return ok
    if t.components(g, 'MachineDisintegrator'):
        bad = auth[0]['unauthorizedItemCategories'] if auth else []
        return [i for i in items if t.groups[i]['recipeIngredients'] and t.groups[i]['itemCategory'] not in bad]
    return []


def build_family(t, name, groups, wait_ms, player_id, feed=True):
    """Every constructible built, each of its network behaviours used (ids, details, its
    inventories, text, colour, setting, linked groups and planet, growth, interactive state,
    panels, its station's craft animation, an outside grower's start, a recycler's run), an
    input given to machines that take one, the world left to run, then everything read again and
    deconstructed: refused while something is in its inventory, done once it is empty."""
    s = Script(name, player_id)
    prelude(s)
    power(s, t, groups, (630.0, 790.0))
    spots = grid((650.0, 600.0), 11, 11.0)
    placed = []
    for i, g in enumerate(groups):
        o = f'o{i}'
        build(s, g, next(spots), f'{g}|build', o)
        invs = inventory_of(s, t, g, o, f'{g}|inventory', f'inv{i}_')
        placed.append((g, o, invs))
        for b in t.behaviour(g, 'WorldObjectAssociatedProxy'):
            s.call('WorldObjectAssociatedProxy.GetWorldObjectIdServerRpc', [f'${o}', b], [], f'{g}|world object id')
            s.call('WorldObjectAssociatedProxy.GetWorldObjectDetailsServerRpc', [f'${o}', b], [], f'{g}|world object details')
        for inv in invs:
            retrieve(s, f'${inv}', f'{g}|inventory')
        for b in t.behaviour(g, 'TextProxy'):
            s.call('TextProxy.SetTextServerRpc', [f'${o}', b], [f'Sweep {g}'], f'{g}|text')
        for b in t.behaviour(g, 'ColorProxy'):
            s.call('ColorProxy.SetColorServerRpc', [f'${o}', b], [[0.2, 0.4, 0.6, 1.0]], f'{g}|colour')
        for b in t.behaviour(g, 'SettingProxy'):
            s.call('SettingProxy.SetSettingServerRpc', [f'${o}', b], [1], f'{g}|setting')
        for b in t.behaviour(g, 'LinkedPlanetProxy'):
            s.call('LinkedPlanetProxy.SetLinkedPlanetServerRpc', [f'${o}', b], [f'#{PRIME}'], f'{g}|linked planet')
        for b in t.behaviour(g, 'GrowthProxy'):
            s.call('GrowthProxy.GetCurrentGrowthServerRpc', [f'${o}', b], [], f'{g}|growth')
        for b in t.behaviour(g, 'ActionnableInteractiveProxy'):
            s.call('ActionnableInteractiveProxy.BroadcastToAllClientsServerRpc', [f'${o}', b], [], f'{g}|interactive state')
        for k, b in enumerate(t.behaviour(g, 'Panel')):
            # A look the panel can wear: doors on walls, glass on floors (BuildPanelSubType).
            s.call('Panel.ChangePanelServerRpc', [f'${o}', b], [PANEL_LOOK[t.panels[g][k]['panelType']], True, False], f'{g}|panel')
        cp = t.behaviour(g, 'ConstructibleProxy')
        if cp and t.components(g, 'ActionCrafter'):
            s.call('ConstructibleProxy.StartCraftAnimationServerRpc', [f'${o}', cp[0]], ['#Iron'], f'{g}|craft animation')
        if cp and t.components(g, 'MachineGrowerVegetationHarvestable') + t.components(g, 'MachineGrowerVegetationStatic'):
            s.call('ConstructibleProxy.MachineOutsideGrowerUpdateStartServerRpc', [f'${o}', cp[0]], [], f'{g}|grower start')
        lgs = t.behaviour(g, 'LinkedGroupsProxy')
        if feed:
            ins = inputs_for(t, g)
            if ins and invs:
                add_new(s, ins[i % len(ins)], f'${invs[0]}', f'{g}|input {ins[i % len(ins)]}')
            for b in lgs:
                s.call('LinkedGroupsProxy.SetLinkedGroupsServerRpc', [f'${o}', b], [[f'#{linked_group(t, g)}']], f'{g}|linked groups')
    s.wait(wait_ms)
    for g, o, invs in placed:
        for inv in invs:
            retrieve(s, f'${inv}', f'{g}|run')
        for b in t.behaviour(g, 'GrowthProxy'):
            s.call('GrowthProxy.GetCurrentGrowthServerRpc', [f'${o}', b], [], f'{g}|run')
    for g, o, invs in placed:
        base = t.behaviour(g, 'ConstructibleProxy') or t.behaviour(g, 'GroupNetworkContainer')
        if not base:
            continue
        if invs:
            add_new(s, 'Iron', f'${invs[0]}', f'{g}|deconstruct refused (inventory not empty)')
            s.call('PlayerShareState.StartDeconstructingServerRpc', 'player:PlayerShareState', [], f'{g}|deconstruct refused (inventory not empty)')
            s.call('GroupNetworkBase.DeconstructServerRpc', [f'${o}', base[0]], [0.0], f'{g}|deconstruct refused (inventory not empty)')
            for inv in invs:
                s.call(f'{INV}.RemoveAndDestroyAllItemsFromInventoryServerRpc', INV, [f'${inv}'], f'{g}|deconstruct')
        s.call('PlayerShareState.StartDeconstructingServerRpc', 'player:PlayerShareState', [], f'{g}|deconstruct')
        s.call('GroupNetworkBase.DeconstructServerRpc', [f'${o}', base[0]], [0.0], f'{g}|deconstruct', after=600)
        s.call(f'{INV}.RemoveAndDestroyAllItemsFromInventoryServerRpc', INV, ['$inventory'], f'{g}|deconstruct')
    retrieve(s, '$inventory', 'player|final inventory', after=1000)
    return s


def linked_group(t, g):
    """A group a machine can be set to: its selector's first choice, an item it can craft, ore."""
    for c in t.components(g):
        dl = c['fields'].get('defaultGroupsList') if isinstance(c['fields'], dict) else None
        if dl:
            return t.ref(dl[0])
    if t.components(g, 'MachineAutoCrafter'):
        return 'Iron' if not t.groups.get('CircuitBoard1') else 'CircuitBoard1'
    return 'Iron'


def feed_family(t):
    """Every seed, larva, egg, food, fuse, fuel, key, skin and DNA item put into a machine that
    takes it (each such machine at least once), then the world left to grow, hatch, feed and
    produce, read back three times."""
    s = Script('sweep_feed', 76561197999990007)
    prelude(s)
    takers = {g: inputs_for(t, g) for g in t.constructibles() if g in t.layouts and t.behaviour(g, 'InventoryAssociatedProxy')
              and not t.components(g, 'MachineDisintegrator') and inputs_for(t, g)}
    pairs = []
    for g in sorted(takers):
        pairs.append((g, takers[g][0]))
    fed = {i for _, i in pairs}
    for i in sorted({i for v in takers.values() for i in v} - fed):
        ms = sorted(g for g in takers if i in takers[g])
        pairs.append((ms[len(pairs) % len(ms)], i))
    # Animal feeders near the shelters (InventorySpawnContent._feederRange).
    power(s, t, [g for g, _ in pairs], (630.0, 790.0))
    spots = grid((650.0, 600.0), 12, 9.0)
    placed = []
    for k, (g, item) in enumerate(pairs):
        o = f'f{k}'
        tag = f'{g}|fed {item}'
        at = next(spots)
        build(s, g, at, tag, o)
        invs = inventory_of(s, t, g, o, tag, f'finv{k}_')
        add_new(s, item, f'${invs[0]}', f'{item}|into {g}')
        placed.append((g, item, o, invs, at))
    for r in range(3):
        s.wait(120000)
        for g, item, o, invs, at in placed:
            for inv in invs:
                retrieve(s, f'${inv}', f'{g}|fed {item}')
            for b in t.behaviour(g, 'GrowthProxy'):
                s.call('GrowthProxy.GetCurrentGrowthServerRpc', [f'${o}', b], [], f'{g}|fed {item}')
    # Harvest: what a harvestable grower made from its seed, picked up as a player does.
    for k, (g, item, o, invs, at) in enumerate(placed):
        produce = t.groups[item].get('growableGroup')
        if not t.components(g, 'MachineGrowerVegetationHarvestable') or not produce or t.ref(produce) not in t.layouts:
            continue
        produce = t.ref(produce)
        near = f'$near:{t.layouts[produce]["hash"]:08x}@{at[0]},{at[1]},{at[2]}'
        tag = f'{produce}|harvest from {g}'
        s.call('WorldObjectAssociatedProxy.GetWorldObjectIdServerRpc', [near, t.behaviour(produce, 'WorldObjectAssociatedProxy')[0]], [], tag,
               cap=(f'h{k}', 'WorldObjectAssociatedProxy.GetWorldObjectIdClientRpc', 0))
        s.call(f'{INV}.AddWorldObjectToInventoryServerRpc', INV, [f'$h{k}', f'#{produce}', '$inventory', True], tag)
    retrieve(s, '$inventory', 'player|final inventory', after=1000)
    return s


def convert_family(t):
    """Every recipe of the incubator and the DNA manipulator: the machine set to it grows it and
    turns its inventory into the item (MachineGrowerIfLinkedGroup, MachineConvertRecipe)."""
    s = Script('sweep_convert', 76561197999990008)
    prelude(s)
    jobs = []
    for ident, machine in (('CraftIncubatorT1', 'Incubator1'), ('CraftGeneticT1', 'GeneticManipulator1')):
        jobs += [(machine, g) for g in t.items() if ident in t.groups[g]['craftableInList'] and t.groups[g]['recipeIngredients']]
    power(s, t, [m for m, _ in jobs], (630.0, 790.0))
    spots = grid((650.0, 600.0), 10, 10.0)
    for k, (m, g) in enumerate(jobs):
        o, tag = f'm{k}', f'{g}|convert at {m}'
        build(s, m, next(spots), tag, o)
        invs = inventory_of(s, t, m, o, tag, f'minv{k}_')
        for ing in t.recipe(g):
            add_new(s, ing, f'${invs[0]}', tag)
        s.call('LinkedGroupsProxy.SetLinkedGroupsServerRpc', [f'${o}', t.behaviour(m, 'LinkedGroupsProxy')[0]], [[f'#{g}']], tag)
    s.wait(75000)
    for k, (m, g) in enumerate(jobs):
        tag = f'{g}|convert at {m}'
        retrieve(s, f'$minv{k}_0', tag)
        for b in t.behaviour(m, 'GrowthProxy'):
            s.call('GrowthProxy.GetCurrentGrowthServerRpc', [f'$m{k}', b], [], tag)
    return s


def optimizer_family(t):
    """Optimizers with every fuse, beside machines they speed up."""
    s = Script('sweep_optimizer', 76561197999990009)
    prelude(s)
    fuses = [g for g in t.items() if t.groups[g]['itemCategory'] == 'Fuse']
    machines = ['OreExtractor2', 'Vegetube2', 'TreeSpreader1', 'ButterflyFarm1', 'Heater2', 'Biodome2']
    opts = [g for g in t.constructibles() if t.components(g, 'MachineOptimizer')]
    power(s, t, opts + machines, (630.0, 790.0))
    spots = grid((660.0, 620.0), 6, 12.0)
    for k, g in enumerate(machines):
        build(s, g, next(spots), f'{g}|build (optimized)', f'mach{k}')
        invs = inventory_of(s, t, g, f'mach{k}', f'{g}|inventory', f'machinv{k}_')
        ins = inputs_for(t, g)
        if ins and invs:
            add_new(s, ins[0], f'${invs[0]}', f'{g}|input {ins[0]}')
    for k, g in enumerate(opts):
        build(s, g, next(spots), f'{g}|build', f'opt{k}')
        invs = inventory_of(s, t, g, f'opt{k}', f'{g}|inventory', f'optinv{k}_')
        for fuse in fuses[k::len(opts)]:
            add_new(s, fuse, f'${invs[0]}', f'{fuse}|into {g}')
    for r in range(2):
        s.wait(90000)
        for k, g in enumerate(machines):
            if t.behaviour(g, 'InventoryAssociatedProxy'):
                retrieve(s, f'$machinv{k}_0', f'{g}|run (optimized)')
            for b in t.behaviour(g, 'GrowthProxy'):
                s.call('GrowthProxy.GetCurrentGrowthServerRpc', [f'$mach{k}', b], [], f'{g}|run (optimized)')
    return s


ANIMAL_TRAITS = [[2, [0.914, 0.592, 0.486, 0.0], 0], [3, [0.588, 0.98, 0.933, 0.0], 0], [4, [0.349, 0.502, 0.624, 0.0], 0],
                 [5, [0.0, 0.0, 0.0, 0.0], 1], [6, [0.0, 0.0, 0.0, 0.0], 2], [8, [0.0, 0.0, 0.0, 0.0], 1]]


def animals_family(t):
    """Every animal of the AnimalList spawned by a wild spawner, its growth and id asked, petted
    twice (the second within the pet delay), then the spawner unregistered (despawns)."""
    s = Script('sweep_animals', 76561197999990012)
    prelude(s)
    animals = t.assets['AnimalList:AnimalList']['animals']
    for k, a in enumerate(animals):
        name, wo = a['prefab'].split(':')[1], 105550000 + k
        tag = f'animal {name}|spawn'
        pos = [650.0 + 12 * k, GROUND_Y, 640.0]
        s.call('WorldAnimalSpawnerHandler.SpawnerRegisteredServerRpc', 'WorldAnimalSpawnerHandler', [wo], tag)
        s.call('WorldAnimalSpawnerHandler.SpawnAnimalsServerRpc', 'WorldAnimalSpawnerHandler', [wo, pos, [1, 1], [[1, [0.0, 0.0, 0.0, 0.0], a['id']]] + ANIMAL_TRAITS], tag,
               after=3000, create=f'an{k}')
        s.call('GrowthProxy.GetCurrentGrowthServerRpc', [f'$an{k}', 5], [], f'animal {name}|growth')
        s.call('WorldObjectAssociatedProxy.GetWorldObjectIdServerRpc', [f'$an{k}', 4], [], f'animal {name}|world object id')
        s.call('PetProxy.ActionPetServerRpc', [f'$an{k}', 6], [], f'animal {name}|pet', after=800)
        s.call('PetProxy.ActionPetServerRpc', [f'$an{k}', 6], [], f'animal {name}|pet again (too soon)')
    s.wait(30000)
    for k, a in enumerate(animals):
        name = a['prefab'].split(':')[1]
        s.call('GrowthProxy.GetCurrentGrowthServerRpc', [f'$an{k}', 5], [], f'animal {name}|growth')
        s.call('WorldAnimalSpawnerHandler.SpawnerUnregisteredServerRpc', 'WorldAnimalSpawnerHandler', [105550000 + k], f'animal {name}|despawn', after=1000)
    return s


def hash_var(t, g):
    return f'$hash:{t.layouts[g]["hash"]:08x}'


def vehicle_family(t):
    """The truck, crafted at its garage as the game does; storage and every vehicle equipment;
    colour; taken, driven (seat lock, controller, ownership) and left; recalled to the garage.
    A second player meanwhile tries to take the truck in use, sits as a passenger, takes it once
    free; then the truck is deconstructed."""
    a = Script('sweep_vehicle.A', 76561197999990013)
    b = Script('sweep_vehicle.B', 76561197999990014)
    prelude(a)
    power(a, t, ['VehicleCrafter1'], (630.0, 790.0))
    garage = (680.0, GROUND_Y, 640.0)
    build(a, 'VehicleCrafter1', garage, 'VehicleCrafter1|build (garage)', 'garage')
    spot, check = craft_spawn(t, 'VehicleCrafter1', garage)
    tag = 'VehicleTruck|craft at CraftVehicleT1'
    for ing in t.recipe('VehicleTruck'):
        add_new(a, ing, '$inventory', tag)
    a.call('ConstructibleProxy.StartCraftAnimationServerRpc', ['$garage', 0], ['#VehicleTruck'], tag)
    a.call(f'{WOH}.CreateAndInstantiateWorldObjectServerRpc', WOH, ['#VehicleTruck', spot, [0, 0, 0, 1], True, check, True, False], tag,
           after=1000, cap=('truck', f'{WOH}.CreateAndInstantiateWorldObjectClientRpc', 0))
    a.call(f'{INV}.RemoveItemsFromInventoryServerRpc', INV, [[f'#{x}' for x in t.recipe('VehicleTruck')], '$inventory', True], tag)
    a.call(f'{WOH}.AddOneToTotalCraftServerRpc', WOH, [], tag)
    a.call('WorldObjectAssociatedProxy.GetWorldObjectIdServerRpc', ['$truck', 3], [], 'VehicleTruck|world object id')
    a.call('InventoryAssociatedProxy.GetInventoryIdServerRpc', ['$truck', 5], [], 'VehicleTruck|storage', cap=('tinv', 'InventoryAssociatedProxy.GetInventoryIdClientRpc', 2))
    a.call('InventoryAssociatedProxy.GetInventoryIdServerRpc', ['$truck', 8], [], 'VehicleTruck|equipment', cap=('teq', 'InventoryAssociatedProxy.GetInventoryIdClientRpc', 2))
    a.call(f'{INV}.BeginInventoryWatchServerRpc', INV, ['$tinv'], 'VehicleTruck|storage')
    a.call(f'{INV}.BeginInventoryWatchServerRpc', INV, ['$teq'], 'VehicleTruck|equipment')
    add_new(a, 'Iron', '$inventory', 'VehicleTruck|storage', 'iron')
    a.call(f'{INV}.TransferItemServerRpc', INV, ['$inventory', '$tinv', '$iron'], 'VehicleTruck|storage')
    for e in [g for g in t.items() if t.groups[g]['itemSubCategory'] == 'VehicleEquipment']:
        add_new(a, e, '$inventory', f'{e}|vehicle equipment', f'veq_{e}')
        a.call(f'{INV}.TransferItemServerRpc', INV, ['$inventory', '$teq', f'$veq_{e}'], f'{e}|vehicle equipment', after=600)
        retrieve(a, '$tinv', f'{e}|vehicle equipment')
        a.call(f'{INV}.TransferItemServerRpc', INV, ['$teq', '$inventory', f'$veq_{e}'], f'{e}|vehicle equipment removed', after=600)
        retrieve(a, '$tinv', f'{e}|vehicle equipment removed')
    a.call('ColorProxy.SetColorServerRpc', ['$truck', 7], [[0.9, 0.1, 0.1, 1.0]], 'VehicleTruck|colour')
    # The driver walks to the truck (UpdateNetworkRigidBodyState gives a far owner's vehicle to the
    # nearest player).
    a.move([spot[0] + 2.0, spot[1], spot[2]], 'VehicleTruck|walk to it')
    a.call('OwnershipSwitcher.AskForOwnershipServerRpc', ['$truck', 1], [], 'VehicleTruck|take control', after=600)
    a.call('UpdateNetworkRigidBodyState.SetVehicleControllerEnabledServerRpc', ['$truck', 4], [True], 'VehicleTruck|take control')
    a.call('LockNetworkPlayerPosition.SetPlayerLockedServerRpc', ['$truck', 9], [True, '$client'], 'VehicleTruck|enter (pilot seat)')
    # Driving: the owner's ClientNetworkTransform moves the truck; the second player sees it go.
    for k in range(12):
        at = [spot[0] + 2.0 * k, spot[1], spot[2] + 1.0 * k]
        a.steps.append({'transform': ['$truck', 0], 'position': at, 'after': 1500, 'tag': 'VehicleTruck|drive (owner transforms)'})
        a.move([at[0], at[1] + 2.0, at[2]], 'VehicleTruck|drive (owner transforms)')
        a.rows.setdefault(('VehicleTruck', 'drive (owner transforms)'), 0)
    a.wait(10000)   # the second player tries meanwhile
    a.call('LockNetworkPlayerPosition.SetPlayerLockedServerRpc', ['$truck', 9], [False, 0], 'VehicleTruck|exit')
    a.call('UpdateNetworkRigidBodyState.SetVehicleControllerEnabledServerRpc', ['$truck', 4], [False], 'VehicleTruck|exit')
    a.call('OwnershipSwitcher.ResetOwnershipServerRpc', ['$truck', 1], [], 'VehicleTruck|exit', after=1000)
    a.wait(30000)   # the second player drives it
    a.call('VehicleBackToGarage.MoveVehicleToPositionServerRpc', ['$garage', 2], ['$truck', spot, [0, 0, 0, 1]], 'VehicleTruck|recall to the garage', after=2000)
    a.call('OwnershipSwitcher.AskForOwnershipServerRpc', ['$truck', 1], [], 'VehicleTruck|take control', after=600)
    a.call('VehicleBackToGarage.MoveVehicleToPositionServerRpc', ['$garage', 2], ['$truck', spot, [0, 0, 0, 1]], 'VehicleTruck|park (owned)', after=1000)
    a.call('OwnershipSwitcher.ResetOwnershipServerRpc', ['$truck', 1], [], 'VehicleTruck|exit')
    retrieve(a, '$tinv', 'VehicleTruck|storage')
    a.call(f'{INV}.RemoveAndDestroyAllItemsFromInventoryServerRpc', INV, ['$tinv'], 'VehicleTruck|deconstruct')
    a.call('PlayerShareState.StartDeconstructingServerRpc', 'player:PlayerShareState', [], 'VehicleTruck|deconstruct')
    a.call('GroupNetworkBase.DeconstructServerRpc', ['$truck', 2], [0.0], 'VehicleTruck|deconstruct', after=1000)
    retrieve(a, '$inventory', 'player|final inventory', after=1000)
    # B joins once the truck is driven by A.
    # B joins once A drives: A's time up to its first transform (its join takes as long as B's).
    first = next(i for i, st in enumerate(a.steps) if st.get('tag') == 'VehicleTruck|drive (owner transforms)')
    b.delay_ms = sum(st.get('wait', 0) or st.get('after', 300) for st in a.steps[:first]) + 2000
    truck = hash_var(t, 'VehicleTruck')
    b.transforms = [truck]
    b.call('OwnershipSwitcher.AskForOwnershipServerRpc', [truck, 1], [], 'VehicleTruck|second player takes it in use', after=600)
    b.call('LockNetworkPlayerPosition.SetPlayerLockedServerRpc', [truck, 10], [True, '$client'], 'VehicleTruck|second player as passenger')
    b.call('InventoryAssociatedProxy.GetInventoryIdServerRpc', [truck, 5], [], 'VehicleTruck|second player storage', cap=('tinv', 'InventoryAssociatedProxy.GetInventoryIdClientRpc', 2))
    retrieve(b, '$tinv', 'VehicleTruck|second player storage')
    b.wait(38000)   # until A has left the truck
    b.call('LockNetworkPlayerPosition.SetPlayerLockedServerRpc', [truck, 10], [False, 0], 'VehicleTruck|second player leaves the seat')
    b.call('OwnershipSwitcher.AskForOwnershipServerRpc', [truck, 1], [], 'VehicleTruck|second player takes it free', after=600)
    b.call('UpdateNetworkRigidBodyState.SetVehicleControllerEnabledServerRpc', [truck, 4], [True], 'VehicleTruck|second player drives')
    b.call('LockNetworkPlayerPosition.SetPlayerLockedServerRpc', [truck, 9], [True, '$client'], 'VehicleTruck|second player drives')
    b.wait(15000)
    b.call('LockNetworkPlayerPosition.SetPlayerLockedServerRpc', [truck, 9], [False, 0], 'VehicleTruck|second player exits')
    b.call('UpdateNetworkRigidBodyState.SetVehicleControllerEnabledServerRpc', [truck, 4], [False], 'VehicleTruck|second player exits')
    b.call('OwnershipSwitcher.ResetOwnershipServerRpc', [truck, 1], [], 'VehicleTruck|second player exits', after=1000)
    b.wait(20000)
    return [a, b]


def seat_family(t):
    """A player who never moves locks into each of the truck's seats in turn (one script per seat)
    and stays: where the host's copy of the player is saved shows whether, and where, a seat
    takes the player along."""
    out = []
    for k, (b, name) in enumerate(zip(t.behaviour('VehicleTruck', 'LockNetworkPlayerPosition'), ['pilot', 'passenger 1', 'passenger 2'])):
        s = Script(f'sweep_seat_{k}', 76561197999990040 + k)
        tag = f'VehicleTruck|seat {name}'
        build(s, 'VehicleTruck', (640.0, 10.31, 700.0), tag, 'truck')
        s.call('LockNetworkPlayerPosition.SetPlayerLockedServerRpc', ['$truck', b], [True, '$client'], tag)
        s.wait(6000)
        out.append(s)
    return out


def sharing_family(t):
    """Two players at one container: the second watches it while the first changes it, takes from
    it, and then tries the first player's backpack (refused here: EXPECTED.md)."""
    a = Script('sweep_sharing.A', 76561197999990015)
    b = Script('sweep_sharing.B', 76561197999990016)
    build(a, 'Container3', (680.0, GROUND_Y, 660.0), 'Container3|shared container', 'box')
    a.call('InventoryAssociatedProxy.GetInventoryIdServerRpc', ['$box', t.behaviour('Container3', 'InventoryAssociatedProxy')[0]], [], 'Container3|shared container', cap=('boxinv', 'InventoryAssociatedProxy.GetInventoryIdClientRpc', 2))
    a.call(f'{INV}.BeginInventoryWatchServerRpc', INV, ['$boxinv'], 'Container3|shared container')
    a.wait(15000)   # B joins and watches
    for k, g in enumerate(['Iron', 'Cobalt', 'Silicon', 'Titanium']):
        add_new(a, g, '$boxinv', 'Container3|first player adds, second watches', f'it{k}')
    a.call('TextProxy.SetTextServerRpc', ['$box', t.behaviour('Container3', 'TextProxy')[0]], ['Shared'], 'Container3|first player renames, second watches')
    a.wait(5000)
    a.call(f'{INV}.TransferItemServerRpc', INV, ['$boxinv', '$inventory', '$it0'], 'Container3|first player takes, second watches')
    add_new(a, 'Magnesium', '$inventory', 'player|first player backpack', 'mine')
    a.wait(20000)   # B takes from the box and tries A's backpack
    retrieve(a, '$boxinv', 'Container3|first player reads after the second')
    retrieve(a, '$inventory', 'player|first player backpack')
    b.delay_ms = 8000
    box = hash_var(t, 'Container3')
    b.call('InventoryAssociatedProxy.GetInventoryIdServerRpc', [box, t.behaviour('Container3', 'InventoryAssociatedProxy')[0]], [], 'Container3|second player finds it', cap=('boxinv', 'InventoryAssociatedProxy.GetInventoryIdClientRpc', 2))
    b.call(f'{INV}.BeginInventoryWatchServerRpc', INV, ['$boxinv'], 'Container3|second player watches')
    retrieve(b, '$boxinv', 'Container3|second player watches')
    b.wait(15000)
    retrieve(b, '$boxinv', 'Container3|second player reads')
    b.call(f'{INV}.TransferAllItemsServerRpc', INV, ['$boxinv', '$inventory'], 'Container3|second player takes all, first watches')
    # The first player's backpack: its inventory id is the first player's (188 in the start save).
    b.call(f'{INV}.TransferAllItemsServerRpc', INV, [188, '$inventory'], "player|second player takes the first's backpack (refused here)")
    b.call(f'{INV}.AddNewItemToInventoryServerRpc', INV, ['#Iron', 188], "player|second player adds to the first's backpack (refused here)")
    retrieve(b, 188, "player|second player reads the first's backpack")
    b.call(f'{INV}.StopInventoryWatchServerRpc', INV, ['$boxinv'], 'Container3|second player stops watching')
    b.wait(10000)
    return [a, b]


DIRECTIONS = [(0, 1), (0, -1), (1, 1), (1, -1), (2, 1), (2, -1)]


def bounds_family(t, fine=None):
    """Where a building's deconstruction volume ends (CheckSomethingInsideIfNeeded), along each
    axis both ways: a container placed further and further out until the building can be
    deconstructed. Coarse (1 m steps), then fine (1/16 m) from the coarse result."""
    groups = sorted(g for g in t.constructibles() for c in t.components(g, 'ActionDeconstructible')
                    if c['fields'].get('checkInsideBeforeDeconstruction'))
    s = Script('sweep_bounds_' + ('fine' if fine else 'coarse'), 76561197999990017)
    spots = grid((1000.0, 1000.0), 10, 60.0)
    k = 0
    for g in groups:
        base = (t.behaviour(g, 'ConstructibleProxy') or t.behaviour(g, 'GroupNetworkContainer'))[0]
        for axis, sign in DIRECTIONS:
            p = next(spots)
            start = fine.get(f'{g} {axis} {sign}', 0.0) if fine else 0.0
            step = 1 / 16 if fine else 1.0
            tag = f'{g}|deconstruct volume {"xyz"[axis]}{"+" if sign > 0 else "-"}'
            build(s, g, p, tag, f'g{k}', after=1000)
            for n in range(1, 17):
                # Sideways one metre up: a volume may start just above the pivot.
                o = [0.0, 0.0 if axis == 1 else 1.0, 0.0]
                o[axis] = sign * (start + n * step)
                at = [round(p[i] + o[i], 4) for i in range(3)]
                build(s, 'Container1', at, f'{tag} {sign * (start + n * step):g}', f'c{k}_{n}', after=300)
                s.call('GroupNetworkBase.DeconstructServerRpc', [f'$g{k}', base], [0.0], f'{tag} {sign * (start + n * step):g}', after=300)
                s.call('GroupNetworkBase.DeconstructServerRpc', [f'$c{k}_{n}', 0], [0.0], f'{tag} {sign * (start + n * step):g}', after=300)
                s.call(f'{INV}.RemoveAndDestroyAllItemsFromInventoryServerRpc', INV, ['$inventory'], f'{tag} {sign * (start + n * step):g}')
            k += 1
    return s


def bounds_floor_family(t, high=False):
    """Where a deconstruction volume's floor is: a container one metre to the side at falling
    heights (inside first, so the building stays until the first height outside)."""
    groups = sorted(g for g in t.constructibles() for c in t.components(g, 'ActionDeconstructible')
                    if c['fields'].get('checkInsideBeforeDeconstruction'))
    # high: the floors measured above the pivot (1 down to 0.5 m, in 1/16 m) of the groups where they are;
    # the first height outside deconstructs the building, which ends the test
    s = Script('sweep_bounds_floor' + ('_high' if high else ''), 76561197999990018)
    if high:
        groups = ['Biolab', 'Pod4x', 'Pod9xB', 'Pod9xC', 'biodome']
    spots = grid((1000.0, 1000.0), 10, 60.0)
    for k, g in enumerate(groups):
        base = (t.behaviour(g, 'ConstructibleProxy') or t.behaviour(g, 'GroupNetworkContainer'))[0]
        p = next(spots)
        build(s, g, p, f'{g}|deconstruct volume floor', f'g{k}', after=1000)
        for n, dy in enumerate([1.0 - i / 16 for i in range(9)] if high else [1.0, 0.5, 0.25, 0.125, 0.0625, 0.03125, 0.0, -0.03125, -0.0625]):
            tag = f'{g}|deconstruct volume floor {dy:g}'
            build(s, 'Container1', [p[0] + 1.0, p[1] + dy, p[2]], tag, f'c{k}_{n}', after=300)
            s.call('GroupNetworkBase.DeconstructServerRpc', [f'$g{k}', base], [0.0], tag, after=300)
            s.call('GroupNetworkBase.DeconstructServerRpc', [f'$c{k}_{n}', 0], [0.0], tag, after=300)
            s.call(f'{INV}.RemoveAndDestroyAllItemsFromInventoryServerRpc', INV, ['$inventory'], tag)
    return s


def bounds_result(path):
    """The last offset still inside, per "group axis sign", from a bounds transcript."""
    out, tag = {}, None
    for line in open(path):
        l = json.loads(line)
        if l['msg'] == 'call' and l.get('tag'):
            tag = l['tag']
        if l['msg'] == 'rpc' and l['name'] == 'GroupNetworkBase.DeconstructErrorClientRpc' and tag and l['args'] == [1]:
            g, rest = tag.split('|', 1)
            if 'floor' in rest:
                continue
            d, o = rest.rsplit(' ', 2)[1:]
            key = f'{g} {"xyz".index(d[0])} {1 if d[1] == "+" else -1}'
            out[key] = max(out.get(key, 0.0), abs(float(o)))
    return out


def growth_family(t):
    """Every group with a GrowthProxy built; its growth asked, reset to 0 and to 40 (the owner
    committing it, GrowthProxy.SetGrowth), asked again."""
    s = Script('sweep_growth', 76561197999990019)
    groups = [g for g in t.constructibles() if g in t.layouts and t.behaviour(g, 'GrowthProxy')]
    power(s, t, groups, (630.0, 790.0))
    spots = grid((650.0, 600.0), 10, 12.0)
    for k, g in enumerate(groups):
        b = t.behaviour(g, 'GrowthProxy')[0]
        build(s, g, next(spots), f'{g}|growth reset', f'o{k}')
        s.call('GrowthProxy.GetCurrentGrowthServerRpc', [f'$o{k}', b], [], f'{g}|growth reset')
        s.call('GrowthProxy.ResetCurrentGrowthServerRpc', [f'$o{k}', b], [0.0], f'{g}|growth reset')
        s.call('GrowthProxy.GetCurrentGrowthServerRpc', [f'$o{k}', b], [], f'{g}|growth reset')
        s.call('GrowthProxy.ResetCurrentGrowthServerRpc', [f'$o{k}', b], [40.0], f'{g}|growth reset')
        s.call('GrowthProxy.GetCurrentGrowthServerRpc', [f'$o{k}', b], [], f'{g}|growth reset')
    return s


def ground_family(t):
    """Every outside grower built on the ground (not in the air), started, and left to plant."""
    s = Script('sweep_ground', 76561197999990021)
    prelude(s)
    groups = [g for g in t.constructibles() if g in t.layouts
              and (t.components(g, 'MachineGrowerVegetationHarvestable') or t.components(g, 'MachineGrowerVegetationStatic'))]
    assert len(groups) <= len(GROUND), 'more growers than measured ground spots'
    power(s, t, groups, (630.0, 790.0))
    placed = []
    for k, (g, (x, y, z)) in enumerate(zip(groups, GROUND)):
        build(s, g, (x, y + 0.1, z), f'{g}|build on the ground', f'o{k}')
        invs = inventory_of(s, t, g, f'o{k}', f'{g}|inventory on the ground', f'inv{k}_')
        placed.append((g, f'o{k}', invs))
        cp = t.behaviour(g, 'ConstructibleProxy')
        if cp:
            s.call('ConstructibleProxy.MachineOutsideGrowerUpdateStartServerRpc', [f'$o{k}', cp[0]], [], f'{g}|grower start on the ground')
    s.wait(120000)
    for g, o, invs in placed:
        for inv in invs:
            retrieve(s, f'${inv}', f'{g}|planted on the ground')
    return s


def recycle_family(t):
    """Every item put in a recycling machine and recycled (ConstructibleProxy.RecycleContent): its
    ingredients dropped, or itself when it has none or cannot be recycled."""
    s = Script('sweep_recycle', 76561197999990020)
    power(s, t, ['RecyclingMachine'], (630.0, 790.0))
    at = (680.0, BUILD_Y, 640.0)
    build(s, 'RecyclingMachine', at, 'RecyclingMachine|build (recycler)', 'rec')
    inv = inventory_of(s, t, 'RecyclingMachine', 'rec', 'RecyclingMachine|inventory', 'recinv')[0]
    cp = t.behaviour('RecyclingMachine', 'ConstructibleProxy')[0]
    bounds = [[at[0], at[1] + 1.0, at[2] + 3.0], [1.0, 1.0, 1.0]]
    for g in t.items():
        tag = f'{g}|recycle'
        add_new(s, g, f'${inv}', tag)
        s.call('ConstructibleProxy.RecycleContentServerRpc', ['$rec', cp], bounds, tag, after=400)
        retrieve(s, f'${inv}', tag)
        s.call(f'{INV}.RemoveAndDestroyAllItemsFromInventoryServerRpc', INV, [f'${inv}'], tag)
    return s


def scene_family(t, start):
    """The scene's own world objects (ids 101xxxxxx, in the start save): asked for, grown, mined
    (picked up), disabled, destroyed and re-generated, one set of ids per group."""
    text = open(start, encoding='utf-8-sig').read().split('@')[3]
    ids = {}
    for r in text.split('|'):
        if r.strip():
            o = json.loads(r)
            if 100000000 <= o['id'] < 110000000 and o['gId'] in t.groups:
                ids.setdefault(o['gId'], []).append(o['id'])
    s = Script('sweep_scene', 76561197999990021)
    prelude(s)
    for g in sorted(ids):
        pool = ids[g]
        pick = lambda k: pool[k % len(pool)]
        h, tag = f'#{g}', f'{g}|scene object'
        s.call(f'{WOH}.IsWorldObjectFromSceneActiveServerRpc', WOH, [pick(0)], tag)
        s.call(f'{WOH}.SetSceneWorldObjectGrowthServerRpc', WOH, [pick(0), h, [3, 10]], tag)
        s.call(f'{WOH}.RefreshSceneWorldObjectGrowthServerRpc', WOH, [pick(0)], tag)
        s.call(f'{WOH}.GenerateRandomAppearanceObjectServerRpc', WOH, [100.0, h, pick(1), -1, [610.0, GROUND_Y, 700.0], [0, 0, 0, 1]], tag)
        s.call(f'{WOH}.IsWorldObjectFromSceneActiveServerRpc', WOH, [pick(1)], tag)
        s.call(f'{INV}.AddWorldObjectToInventoryServerRpc', INV, [pick(2), h, '$inventory', True], tag)
        s.call(f'{WOH}.IsWorldObjectFromSceneActiveServerRpc', WOH, [pick(2)], tag)
        s.call(f'{WOH}.DisableWorldObjectFromSceneServerRpc', WOH, [pick(3), h, True, [0.0, 0.0, 0.0]], tag)
        s.call(f'{WOH}.DisableWorldObjectFromSceneServerRpc', WOH, [pick(4), h, True, [612.0, GROUND_Y, 700.0]], tag, after=500)
        s.call(f'{WOH}.DestroyWorldObjectFromSceneServerRpc', WOH, [pick(5), h, 0.0], tag)
        s.call(f'{WOH}.DestroyWorldObjectFromSceneServerRpc', WOH, [pick(5), h, 0.0], tag)
    retrieve(s, '$inventory', 'player|final inventory', after=1000)
    return s


def interplanetary_family(t):
    """TransferGroups from a planet's containers: items in containers built on Prime are taken
    to the backpack by a client asking for them by group, planet and amount."""
    s = Script('sweep_transfer', 76561197999990022)
    prelude(s)
    power(s, t, ['Container2'] * 2, (630.0, 790.0))
    build(s, 'Container2', (650.0, BUILD_Y, 640.0), 'Container2|interplanetary transfer', 'box')
    inv = inventory_of(s, t, 'Container2', 'box', 'Container2|interplanetary transfer', 'boxinv')[0]
    for g, n in (('Iron', 3), ('Cobalt', 2), ('Silicon', 4)):
        for _ in range(n):
            add_new(s, g, f'${inv}', f'{g}|interplanetary transfer')
    s.wait(3000)
    for g, n in (('Iron', 2), ('Cobalt', 5), ('Silicon', 4), ('Titanium', 1)):
        s.call('LogisticInterplanetaryManager.TransferGroupsServerRpc', 'LogisticInterplanetaryManager', [f'#{g}', n, f'#{PRIME}', '$inventory'], f'{g}|interplanetary transfer', after=800)
    retrieve(s, '$inventory', 'player|final inventory')
    retrieve(s, f'${inv}', 'Container2|interplanetary transfer')
    return s


def generate(t, out, start=None):
    os.makedirs(out, exist_ok=True)
    scripts, rows = [], {}

    def chunks(xs, n):
        size = math.ceil(len(xs) / n)
        return [(k + 1, xs[k * size:(k + 1) * size]) for k in range(n)]

    for c in chunks(t.items(), 4):
        scripts.append(items_family(t, c))
    scripts.append(use_family(t))
    craftable = [g for g in t.items() if t.groups[g]['craftableInList'] and t.groups[g]['recipeIngredients']]
    in_hand = [g for g in craftable if not t.groups[g]['craftedInWorld']]
    for c in chunks(in_hand, 2):
        scripts.append(craft_family(t, f'sweep_craft_{c[0]}', c[1], 76561197999990003))
    # Rockets for space and drones; the departure rockets end the game on the planet (rockets.json)
    # and the truck has its own script.
    in_world = [g for g in craftable if t.groups[g]['craftedInWorld'] and t.groups[g]['craftableInList'][0] in ('CraftRocket', 'CraftDroneT1')]
    scripts.append(craft_family(t, 'sweep_craft_world', in_world, 76561197999990006))
    with_recipe = [g for g in t.items() if t.groups[g]['recipeIngredients']]
    for c in chunks(with_recipe, 3):
        scripts.append(autocraft_family(t, 'AutoCrafter1', c))
    incubator = [g for g in t.items() if 'CraftIncubatorT1' in t.groups[g]['craftableInList'] and t.groups[g]['recipeIngredients']]
    scripts.append(autocraft_family(t, 'Incubator2', (1, incubator)))
    scripts += [feed_family(t), convert_family(t), optimizer_family(t), animals_family(t)]
    scripts += vehicle_family(t) + sharing_family(t) + seat_family(t)
    scripts += [growth_family(t), ground_family(t), recycle_family(t), interplanetary_family(t)]
    if start:
        scripts.append(scene_family(t, start))
    buildable = [g for g in t.constructibles() if g in t.layouts]
    wet = [g for g in t.constructibles(wet=True) if g in t.layouts]
    scripts.append(build_family(t, 'sweep_water', wet, 300000, 76561197999990030))
    for c in chunks(buildable, 3):
        scripts.append(build_family(t, f'sweep_build_{c[0]}', c[1], 300000, 76561197999990010 + c[0]))
    for s in scripts:
        js = s.json()
        if getattr(s, 'delay_ms', None):
            js['delay_ms'] = s.delay_ms  # the runner starts this second player that much later
        if getattr(s, 'transforms', None):
            js['transforms'] = s.transforms
        json.dump(js, open(f'{out}/{s.name}.json', 'w'), indent=0)
        for (g, i), n in s.rows.items():
            rows.setdefault(g, {}).setdefault(i, []).append(s.name)
    for g in t.groups:
        if g not in t.layouts:
            rows.setdefault(g, {}).setdefault('build', []).append('none: no prefab in the tables')
    json.dump(rows, open(f'{out}/rows.json', 'w'), indent=1, sort_keys=True)
    print(f'{len(scripts)} scripts, {sum(len(s.steps) for s in scripts)} steps, '
          f'{len(rows)} groups, {sum(len(v) for v in rows.values())} rows')


# ---- the inventory: what each component sends --------------------------------------------------

def rpc_map(cs_dir):
    """Class.Method -> the RPCs its body calls, for every class in the decompiled game code;
    then each component's RPCs: the ones it calls directly and through those wrappers."""
    bodies = {}
    for f in os.listdir(cs_dir):
        if not f.endswith('.cs'):
            continue
        cls = f[:-3]
        text = open(f'{cs_dir}/{f}', errors='replace').read()
        parts = re.split(r'\n\t(?:public|private|protected|internal)[^\n]*?(\w+)\(', text)
        bodies[cls] = {parts[i]: parts[i + 1] for i in range(1, len(parts) - 1, 2)}
    rpc = re.compile(r'\b(\w+(?:Server|Client)Rpc)\(')
    wrappers = {}
    for cls, methods in bodies.items():
        for m, body in methods.items():
            for r in rpc.findall(body):
                if r != m and r in methods:
                    wrappers.setdefault(f'{cls}.{m}', set()).add(f'{cls}.{r}')
    by_name = {}
    for w, rs in wrappers.items():
        by_name.setdefault(w.split('.', 1)[1], set()).update(rs)
    called = re.compile(r'\.(\w+)\(')

    def sends(cls):
        out = set()
        for m, body in bodies.get(cls, {}).items():
            out |= {f'{cls}.{r}' for r in rpc.findall(body) if r in bodies[cls]}
            for name in called.findall(body):
                out |= by_name.get(name, set())
        return out
    return sends


def inventory(t, cs_dir):
    """Component type -> groups carrying it, and the RPCs its behaviour sends."""
    sends = rpc_map(cs_dir)
    by_type = {}
    for g in t.groups:
        for c in t.components(g):
            by_type.setdefault(c['type'], set()).add(g)
    return {ty: {'groups': sorted(gs), 'rpcs': sorted(sends(ty))} for ty, gs in sorted(by_type.items())}


# ---- the report ---------------------------------------------------------------------------------

def parse_diff(path):
    """(tag, label, line) for every difference line parity.py printed."""
    out, tag = [], None
    if not os.path.exists(path):
        return None
    for line in open(path):
        line = line.rstrip('\n')
        if line.startswith('## step'):
            m = re.search(r' \[(.*)\]$', line)
            tag = m.group(1) if m else 'player|join'
        elif line.startswith('# '):
            tag = None
        elif line.startswith(('- ', '+ ', '~ ')):
            m = re.match(r'~ (?:object )?(\S+)', line)
            out.append((tag, None if tag else (m.group(1) if m else None), line))
    return out


def settled(line):
    """A saved position the host's physics moved: within a few metres sideways of ours (it fell
    onto the terrain or rolled down a slope), or a rotation it gave an object ours keeps upright."""
    m = re.search(r'\.pos: ref "([^"]+)" ours "([^"]+)"', line)
    if m:
        a, b = ([float(x) for x in v.split(',')] for v in m.groups())
        return math.hypot(a[0] - b[0], a[2] - b[2]) < 15
    return bool(re.search(r'\.rot: ref "[^"]+" ours "0,0,0,1"$', line))


def noise_key(tag, line):
    """What a difference is about, to find the same kind of difference between two reference
    runs: the step's tag and the RPC (or create/destroy), a state variable, a save field."""
    m = re.search(r'"name": "([^"]+)"', line)
    if line.startswith(('- ', '+ ')):
        what = m.group(1) if m else re.sub(r'#\d+', '#', (re.search(r'"msg": "(\w+)", "object": "([^"]+)"', line) or re.search('()()', '')).group(0))
        return ('event', tag, what)
    m = re.match(r'~ (?:object )?(\S+) (\S+)', line)
    if m:
        return ('state', re.sub(r'#\d+|@.*', '', m.group(1)), m.group(2))
    m = re.match(r'(\w+)\[\d+\] \(([^)]*)\)\.(\w+)', line) or re.match(r'(\w+)\[\d+\]() (field order)', line)
    if m:
        return ('save', m.group(1), m.group(3))
    m = re.match(r'(\w+) order at', line) or re.match(r'(\w+): \d+ records', line)
    return ('save', m.group(1), 'order') if m else ('other', line)


def noise_keys(run, script):
    keys = set()
    for tag, _, line in parse_diff(f'{run}/{script}.noise') or []:
        keys.add(noise_key(tag, line))
    base = script.split('.')[0]
    if os.path.exists(f'{run}/{base}.savenoise') and not script.endswith('.B'):
        for line in open(f'{run}/{base}.savenoise'):
            keys.add(noise_key(None, line.rstrip('\n')))
    return keys


def classify(rules, script, tag, line, noise=()):
    for r in rules:
        if r.get('kind') == 'settled':
            if settled(line):
                return r['class'], r['why']
            continue
        if re.search(r.get('script', ''), script) and re.search(r.get('tag', ''), tag or '') and re.search(r['line'], line):
            return r['class'], r['why']
    if noise and noise_key(tag, line) in noise:
        return 'd', 'the game\'s own randomness or timing: the same kind of difference between two reference runs'
    return 'unclassified', ''


def report(t, out, run, rules_path, cs_dir=None):
    rows = json.load(open(f'{out}/rows.json'))
    rules = json.load(open(rules_path))
    prefab_group = {}
    for g, l in t.layouts.items():
        prefab_group.setdefault(l.get('prefab') or g, g)
    results = {}   # (group, interaction) -> [(script, class, why, line)]
    ran = set()
    for f in sorted(os.listdir(out)):
        if not f.endswith('.json') or f == 'rows.json':
            continue
        script = f[:-5]
        diffs = parse_diff(f'{run}/{script}.diff')
        if diffs is None:
            continue
        ran.add(script)
        noise = noise_keys(run, script)
        for tag, label, line in diffs:
            if tag:
                key = tuple(tag.split('|', 1))
            else:
                m = re.match(r'new:([^#]+)#', label or '')
                key = (prefab_group.get(m.group(1), m.group(1)), 'state') if m else ('world', 'state')
            results.setdefault(key, []).append((script, *classify(rules, script, tag, line, noise), line))
        for line in open(f'{run}/{script.split(".")[0]}.savediff') if os.path.exists(f'{run}/{script.split(".")[0]}.savediff') and not script.endswith('.B') else []:
            line = line.rstrip('\n')
            if re.match(r'\d+ save difference', line):
                continue
            m = re.match(r'\w+\[\d+\] \(([^)]*)\)', line)
            key = (m.group(1), 'save') if m and m.group(1) in rows else ('world', 'save')
            results.setdefault(key, []).append((script, *classify(rules, script, None, line, noise), line))
    lines = []
    totals = {'groups': 0, 'rows': 0, 'matched': 0, 'differ': 0, 'not run': 0}
    classes = {}
    for g in sorted(rows, key=str.lower):
        totals['groups'] += 1
        cells = []
        for i in sorted(rows[g]):
            scripts = rows[g][i]
            totals['rows'] += 1
            if scripts[0].startswith('none:'):
                cells.append(f'{i}: {scripts[0]}')
                totals['not run'] += 1
                continue
            if not any(sc in ran for sc in scripts):
                cells.append(f'{i}: not run')
                totals['not run'] += 1
                continue
            got = results.get((g, i), [])
            if not got:
                totals['matched'] += 1
                continue
            totals['differ'] += 1
            cl = sorted({c for _, c, _, _ in got})
            for c in cl:
                classes[c] = classes.get(c, 0) + 1
            cells.append(f'{i}: ' + '/'.join(cl))
        used = sorted({sc.replace('sweep_', '') for v in rows[g].values() for sc in v if not sc.startswith('none:')})
        lines.append(f'| {g} | {t.groups[g]["kind"] if g in t.groups else "-"} | {len(rows[g])} | {", ".join(used)} | {"; ".join(cells) or "all match"} |')
    for key in sorted(k for k in results if k[0] not in rows):
        lines.append(f'| {key[0]} ({key[1]}) | - | - | ' + '/'.join(sorted({c for _, c, _, _ in results[key]})) + ' |')
    detail = []
    for key in sorted(results):
        for script, c, why, line in results[key]:
            detail.append({'group': key[0], 'interaction': key[1], 'script': script, 'class': c, 'why': why, 'line': line[:600]})
    json.dump(detail, open(f'{run}/classified.json', 'w'), indent=1)
    print(json.dumps({'totals': totals, 'rows with differences, by class': classes}, indent=1))
    open(f'{run}/matrix.md', 'w').write('| group | kind | interactions | scripts | differences (class), none: all match |\n|---|---|---|---|---|\n' + '\n'.join(lines) + '\n')
    if cs_dir:
        inv = inventory(t, cs_dir)
        scripts_of = {}   # component type -> scripts of the rows of the groups carrying it
        for ty, v in inv.items():
            scripts_of[ty] = sorted({sc.replace('sweep_', '') for g in v['groups'] for rr in rows.get(g, {}).values() for sc in rr if not sc.startswith('none:')})
        with open(f'{run}/components.md', 'w') as f:
            f.write('| component | groups carrying it | RPCs its behaviour sends | scripts that run groups carrying it |\n|---|---|---|---|\n')
            for ty, v in inv.items():
                names = ', '.join(r.split('.', 1)[1] if r.startswith(ty + '.') else r for r in v['rpcs']) or '-'
                sc = scripts_of[ty]
                f.write(f'| {ty} | {len(v["groups"])} | {names} | {", ".join(sc) if len(sc) <= 8 else ", ".join(sc[:8]) + ", ..."} |\n')
    return totals, classes


def main():
    cmd, data, out = sys.argv[1:4]
    t = Tables(data)
    if cmd == 'gen':
        generate(t, out, sys.argv[4] if len(sys.argv) > 4 else None)
    elif cmd == 'bounds':
        # bounds DATA OUT [COARSE_TRANSCRIPT]: the coarse script, or the fine one from its result
        fine = None
        if len(sys.argv) > 4:
            got = bounds_result(sys.argv[4])
            groups = {k.split(' ')[0] for k in got}
            fine = {f'{g} {a} {sg}': got.get(f'{g} {a} {sg}', 0.0) for g in groups for a, sg in DIRECTIONS}
        s = bounds_family(t, fine)
        json.dump(s.json(), open(f'{out}/{s.name}.json', 'w'), indent=0)
        print(f'{out}/{s.name}.json')
    elif cmd == 'bounds-floor':
        s = bounds_floor_family(t, len(sys.argv) > 4)
        json.dump(s.json(), open(f'{out}/{s.name}.json', 'w'), indent=0)
        print(f'{out}/{s.name}.json')
    elif cmd == 'bounds-result':
        print(json.dumps(bounds_result(out), indent=1, sort_keys=True))
    elif cmd == 'report':
        here = os.path.dirname(os.path.abspath(__file__))
        report(t, out, sys.argv[4], f'{here}/sweep_classes.json', sys.argv[5] if len(sys.argv) > 5 else None)
    else:
        sys.exit(f'unknown command {cmd}')


if __name__ == '__main__':
    main()
