"""Probe scripts for a late-game save, made from the objects the save really has.

    python3 gen.py DATA_DIR SAVE.json OUT_DIR

Writes OUT_DIR/lg_*.json, for `pcds probe` (README "Parity with the game") against the real game
host and this server, both started from the same save (sweep.sh does that; the save needs a host
record of its own: mkstart.py). Each script is one player; every step is tagged
"Group|what", as the sweep's are, so a difference names the group it belongs to.

  lg_idle5       a player stands for five minutes: nothing is touched, the world runs.
  lg_real        one real object of every kind of building the save has (on the planet the
                 world starts on): its ids, details, inventories, growth, text, colour, setting
                 and linked groups; the world runs; the same read again; deconstruction.
  lg_inventories real containers: watched, emptied into a backpack and filled again, sorted, locked.
  lg_logistics   demand and supply set on real containers, the drones left to carry.
  lg_rockets     the platforms of the save: rockets sent, a trade and an exchange platform asked.

The objects are addressed by prefab and place ("$near:HASH@x,y,z", a real object of the world),
their inventories by the ids the save gives them: the same on both hosts.
"""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
import sweep  # noqa: E402  (the sweep's script builder and table reader)

PRIME_HASH = -1140328421
INV = sweep.INV
WOH = sweep.WOH


def load_save(path):
    text = open(path, encoding='utf-8-sig').read()
    secs = [[json.loads(r) for r in s.split('|') if r.strip()] for s in text.strip().split('@')]
    return secs


class World:
    def __init__(self, path, tables):
        s = load_save(path)
        self.t = tables
        self.objects = s[3]
        self.inventories = {i['id']: i for i in s[4]}
        self.by_group = {}
        for o in self.objects:
            self.by_group.setdefault(o['gId'], []).append(o)
        n = json.load(open(f'{tables.data_dir}/netdata.json'))
        self.prefab_hash = {g: v['prefabHash'] for g, v in n['groups'].items()}

    def content(self, o):
        inv = self.inventories.get(o.get('liId'))
        return [int(w) for w in str(inv['woIds']).split(',') if w] if inv else []

    def on_prime(self, o):
        return 'pos' in o and o.get('planet') == PRIME_HASH

    def pick(self, group, n=1, with_content=None):
        """Up to n objects of a group standing on Prime, the ones with something inside first
        (with_content True), empty first (False) or by id (None)."""
        have = sorted((o for o in self.by_group.get(group, []) if self.on_prime(o)), key=lambda o: o['id'])
        if with_content is True:
            have.sort(key=lambda o: -len(self.content(o)))
        elif with_content is False:
            have.sort(key=lambda o: len(self.content(o)))
        return have[:n]

    def target(self, o):
        """A real network object, by its prefab and place."""
        x, y, z = (float(v) for v in o['pos'].split(','))
        return f"$near:{self.prefab_hash[o['gId']]}@{x},{y},{z}"


def tag(g, what):
    return f'{g}|{what}'


def script_idle(player_id):
    s = sweep.Script('lg_idle5', player_id)
    s.wait(300000)
    return s


def present_buildings(w):
    """Every constructible group with an object on Prime, and the behaviours its network object has."""
    out = []
    for g in sorted(w.by_group):
        if w.t.groups.get(g, {}).get('kind') != 'constructible' or not w.t.prefabs.get(g):
            continue
        if w.pick(g):
            out.append(g)
    return out


def read_pass(s, w, picks, label):
    for g, o in picks:
        tgt = w.target(o)
        for b in w.t.behaviour(g, 'WorldObjectAssociatedProxy'):
            s.call('WorldObjectAssociatedProxy.GetWorldObjectIdServerRpc', [tgt, b], [], tag(g, f'{label}: world object id'))
            s.call('WorldObjectAssociatedProxy.GetWorldObjectDetailsServerRpc', [tgt, b], [], tag(g, f'{label}: world object details'))
        for b in w.t.behaviour(g, 'InventoryAssociatedProxy'):
            s.call('InventoryAssociatedProxy.GetInventoryIdServerRpc', [tgt, b], [], tag(g, f'{label}: inventory id'))
        if o.get('liId'):
            sweep.retrieve(s, o['liId'], tag(g, f'{label}: inventory'))
        for b in w.t.behaviour(g, 'GrowthProxy'):
            s.call('GrowthProxy.GetCurrentGrowthServerRpc', [tgt, b], [], tag(g, f'{label}: growth'))


def script_real(w, player_id):
    s = sweep.Script('lg_real', player_id)
    sweep.prelude(s)
    groups = present_buildings(w)
    picks = [(g, o) for g in groups for o in w.pick(g, 1, True)]
    read_pass(s, w, picks, 'read')
    for g, o in picks:
        tgt = w.target(o)
        for b in w.t.behaviour(g, 'TextProxy'):
            s.call('TextProxy.SetTextServerRpc', [tgt, b], [f'Completeness {g}'], tag(g, 'text'))
        for b in w.t.behaviour(g, 'ColorProxy'):
            s.call('ColorProxy.SetColorServerRpc', [tgt, b], [[0.2, 0.4, 0.6, 1.0]], tag(g, 'colour'))
        for b in w.t.behaviour(g, 'SettingProxy'):
            s.call('SettingProxy.SetSettingServerRpc', [tgt, b], [1], tag(g, 'setting'))
        for b in w.t.behaviour(g, 'LinkedPlanetProxy'):
            s.call('LinkedPlanetProxy.SetLinkedPlanetServerRpc', [tgt, b], [f'#{sweep.PRIME}'], tag(g, 'linked planet'))
        for b in w.t.behaviour(g, 'ActionnableInteractiveProxy'):
            s.call('ActionnableInteractiveProxy.BroadcastToAllClientsServerRpc', [tgt, b], [], tag(g, 'interactive state'))
        cp = w.t.behaviour(g, 'ConstructibleProxy')
        if cp and w.t.components(g, 'ActionCrafter'):
            s.call('ConstructibleProxy.StartCraftAnimationServerRpc', [tgt, cp[0]], ['#Iron'], tag(g, 'craft animation'))
        if cp and w.t.components(g, 'MachineGrowerVegetationHarvestable') + w.t.components(g, 'MachineGrowerVegetationStatic'):
            s.call('ConstructibleProxy.MachineOutsideGrowerUpdateStartServerRpc', [tgt, cp[0]], [], tag(g, 'grower start'))
    s.wait(60000)
    read_pass(s, w, picks, 'run')
    # Deconstruction of one object of each building without anything inside (the game refuses
    # the others: the sweep covers the refusal).
    for g, o in picks:
        if w.content(o):
            continue
        base = w.t.behaviour(g, 'ConstructibleProxy') or w.t.behaviour(g, 'GroupNetworkContainer')
        if not base:
            continue
        s.call('PlayerShareState.StartDeconstructingServerRpc', 'player:PlayerShareState', [], tag(g, 'deconstruct'))
        s.call('GroupNetworkBase.DeconstructServerRpc', [w.target(o), base[0]], [0.0], tag(g, 'deconstruct'), after=600)
    sweep.retrieve(s, '$inventory', 'player|final inventory', after=1000)
    return s


CONTAINERS = ['Container1', 'Container2', 'Container3', 'Locker1', 'Vault1', 'Fridge1', 'Counter1', 'Counter2', 'Trashcan1', 'DisplayCase', 'ProceduralWreckSafe', 'Pod4x', 'Server1']


def script_inventories(w, player_id):
    s = sweep.Script('lg_inventories', player_id)
    sweep.prelude(s)
    for g in CONTAINERS:
        for o in w.pick(g, 2, True):
            inv = o.get('liId')
            if not inv:
                continue
            t_ = lambda what: tag(g, what)
            sweep.retrieve(s, inv, t_('retrieve'))
            s.call(f'{INV}.BeginInventoryWatchServerRpc', INV, [inv], t_('watch'))
            s.call(f'{INV}.TransferAllItemsServerRpc', INV, [inv, '$bin'], t_('empty into a backpack'))
            sweep.retrieve(s, '$bin', t_('the backpack after'))
            s.call(f'{INV}.TransferAllItemsServerRpc', INV, ['$bin', inv], t_('fill again'))
            s.call(f'{INV}.SortInventoryServerRpc', INV, [inv], t_('sort'))
            s.call(f'{INV}.LockInventoryContentServerRpc', INV, [inv, True, 5.0], t_('lock'))
            s.call(f'{INV}.CheckInventoryWatchAndDirtyServerRpc', INV, [inv], t_('check'))
            s.call(f'{INV}.StopInventoryWatchServerRpc', INV, [inv], t_('stop watching'))
            sweep.retrieve(s, inv, t_('retrieve after'))
        s.wait(300)
    # by group, as the game finds the first inventory of a group (per planet)
    for g in CONTAINERS:
        s.call(f'{INV}.GetInventoryFromFirstWorldObjectOfGroupServerRpc', INV, [f'#{g}', f'#{sweep.PRIME}'], tag(g, 'first of the group'))
    s.wait(30000)
    sweep.retrieve(s, '$bin', 'player|final inventory')
    return s


def script_logistics(w, player_id):
    """Demand on one real container and supply on another, the drones left to carry between them."""
    s = sweep.Script('lg_logistics', player_id)
    sweep.prelude(s)
    s.call('LogisticManager.RetrieveDronesCountServerRpc', 'LogisticManager', [], 'logistics|drones before')
    s.call('LogisticManager.RetrieveSupplyAndDemandGroupsServerRpc', 'LogisticManager', [], 'logistics|groups before')
    boxes = [o for o in w.pick('Container3', 400, None) if o.get('liId') and w.content(o)]
    empty = [o for o in w.pick('Container3', 400, False) if o.get('liId') and not w.content(o)]
    if boxes and empty:
        src, dst = boxes[0], empty[0]
        held = w.content(src)
        gid = next(o['gId'] for o in w.objects if o['id'] == held[0])
        s.call(f'{INV}.DirtyInventoryLogisticsServerRpc', INV, [src['liId'], [], [f'#{gid}'], 0], 'logistics|supply on a container')
        s.call(f'{INV}.DirtyInventoryLogisticsServerRpc', INV, [dst['liId'], [f'#{gid}'], [], 0], 'logistics|demand on a container')
        s.call(f'{INV}.RetrieveInventoryLogisticsServerRpc', INV, [src['liId']], 'logistics|read supply')
        s.call(f'{INV}.RetrieveInventoryLogisticsServerRpc', INV, [dst['liId']], 'logistics|read demand')
    s.wait(90000)
    s.call('LogisticManager.RetrieveDronesCountServerRpc', 'LogisticManager', [], 'logistics|drones after')
    s.call('LogisticManager.RetrieveSupplyAndDemandGroupsServerRpc', 'LogisticManager', [], 'logistics|groups after')
    if boxes and empty:
        sweep.retrieve(s, empty[0]['liId'], 'logistics|demand container after')
        sweep.retrieve(s, boxes[0]['liId'], 'logistics|supply container after')
    return s


def script_rockets(w, player_id):
    s = sweep.Script('lg_rockets', player_id)
    sweep.prelude(s)
    for g in ('LaunchPlatform', 'TradePlatform1', 'InterplanetaryExchangePlatform1', 'DeparturePlatform', 'PlanetaryDeliveryDepot1'):
        for o in w.pick(g, 1, True):
            tgt = w.target(o)
            cp = w.t.behaviour(g, 'ConstructibleProxy')
            for b in w.t.behaviour(g, 'WorldObjectAssociatedProxy'):
                s.call('WorldObjectAssociatedProxy.GetWorldObjectDetailsServerRpc', [tgt, b], [], tag(g, 'details'))
            if o.get('liId'):
                sweep.retrieve(s, o['liId'], tag(g, 'inventory'))
            for b in w.t.behaviour(g, 'GrowthProxy'):
                s.call('GrowthProxy.GetCurrentGrowthServerRpc', [tgt, b], [], tag(g, 'growth'))
            if cp and g == 'LaunchPlatform':
                s.call('ConstructibleProxy.SendInSpaceServerRpc', [tgt, cp[0]], [], tag(g, 'send in space'))
            if cp and g in ('TradePlatform1', 'InterplanetaryExchangePlatform1'):
                s.call('ConstructibleProxy.RetrieveRocketBackAndForthSpeedMultiplierServerRpc', [tgt, cp[0]], [], tag(g, 'speed multiplier'))
                s.call('ConstructibleProxy.SendInSpaceRocketBackAndForthServerRpc', [tgt, cp[0]], [], tag(g, 'send back and forth'))
    s.wait(120000)
    for g in ('LaunchPlatform', 'TradePlatform1', 'InterplanetaryExchangePlatform1'):
        for o in w.pick(g, 1, True):
            tgt = w.target(o)
            for b in w.t.behaviour(g, 'GrowthProxy'):
                s.call('GrowthProxy.GetCurrentGrowthServerRpc', [tgt, b], [], tag(g, 'growth after'))
            if o.get('liId'):
                sweep.retrieve(s, o['liId'], tag(g, 'inventory after'))
    return s


PLANET_HASH = {'Selenea': -1016990411, 'Aqualis': -1291310150}


def script_travel(w, player_id):
    """The player travels Prime -> Selenea -> Aqualis -> Prime; on each planet the machines of the
    save wake up, and their inventories are read when it arrives and a minute later."""
    s = sweep.Script('lg_travel', player_id)
    sweep.prelude(s)
    s.wait(10000)
    for planet in ('Selenea', 'Aqualis', 'Prime'):
        s.call('PlanetNetworkLoader.SwitchToPlanetServerRpc', 'PlanetNetworkLoader', [f'#{planet}', 0, 0], f'planet|travel to {planet}', after=3000)
        s.wait(5000)
        mine = [o for o in w.objects if o.get('planet') == (PLANET_HASH.get(planet, PRIME_HASH)) and 'pos' in o and o.get('liId') and w.content(o)]
        mine = sorted(mine, key=lambda o: (o['gId'], o['id']))
        picks, seen = [], set()
        for o in mine:
            if o['gId'] not in seen or len(picks) < 8:
                seen.add(o['gId'])
                picks.append(o)
            if len(picks) >= 16:
                break
        for o in picks:
            sweep.retrieve(s, o['liId'], tag(o['gId'], f'{planet}: arrival'))
        s.wait(60000)
        for o in picks:
            sweep.retrieve(s, o['liId'], tag(o['gId'], f'{planet}: a minute later'))
    return s


def script_pair(w, ids):
    """Two players at one real container: the second watches it while the first changes it and takes
    from it, then takes the rest; and both sort a second one."""
    a, b = sweep.Script('lg_pair.A', ids[0]), sweep.Script('lg_pair.B', ids[1])
    boxes = [o for o in w.pick('Container3', 400, True) if o.get('liId') and len(w.content(o)) >= 3][:2]
    c1, c2 = boxes
    g = 'Container3'
    first = w.content(c1)[0]
    a.call(f'{INV}.BeginInventoryWatchServerRpc', INV, [c1['liId']], tag(g, 'first player watches'))
    sweep.retrieve(a, c1['liId'], tag(g, 'first player reads'))
    a.wait(15000)
    sweep.add_new(a, 'Iron', c1['liId'], tag(g, 'first player adds, second watches'), 'added')
    a.call(f'{INV}.TransferItemServerRpc', INV, [c1['liId'], '$inventory', first], tag(g, 'first player takes one, second watches'))
    a.call(f'{INV}.SortInventoryServerRpc', INV, [c2['liId']], tag(g, 'first player sorts the second box'))
    a.wait(20000)
    sweep.retrieve(a, c1['liId'], tag(g, 'first player reads after the second'))
    sweep.retrieve(a, '$inventory', 'player|first player backpack')
    b.delay_ms = 8000
    b.call(f'{INV}.BeginInventoryWatchServerRpc', INV, [c1['liId']], tag(g, 'second player watches'))
    sweep.retrieve(b, c1['liId'], tag(g, 'second player reads'))
    b.wait(15000)
    sweep.retrieve(b, c1['liId'], tag(g, 'second player reads again'))
    b.call(f'{INV}.TransferAllItemsServerRpc', INV, [c1['liId'], '$inventory'], tag(g, 'second player takes all, first watches'))
    b.call(f'{INV}.SortInventoryServerRpc', INV, [c2['liId']], tag(g, 'second player sorts the second box'))
    b.call(f'{INV}.StopInventoryWatchServerRpc', INV, [c1['liId']], tag(g, 'second player stops watching'))
    b.wait(10000)
    return [a, b]


def main():
    data_dir, save, out = sys.argv[1:4]
    t = sweep.Tables(data_dir)
    t.data_dir = data_dir
    w = World(save, t)
    os.makedirs(out, exist_ok=True)
    scripts = [script_idle(76561197999990100), script_real(w, 76561197999990101), script_inventories(w, 76561197999990102),
               script_logistics(w, 76561197999990103), script_rockets(w, 76561197999990104), script_travel(w, 76561197999990107)]
    names = ['lg_idle5', 'lg_real', 'lg_inventories', 'lg_logistics', 'lg_rockets', 'lg_travel']
    for name, s in zip(names, scripts):
        json.dump(s.json(), open(f'{out}/{name}.json', 'w'))
        print(f'{name}: {len(s.steps)} steps, {len(s.rows)} rows')
    for s in script_pair(w, (76561197999990105, 76561197999990106)):
        d = s.json()
        if getattr(s, 'delay_ms', 0):
            d['delay_ms'] = s.delay_ms
        json.dump(d, open(f'{out}/{s.name}.json', 'w'))
        print(f'{s.name}: {len(s.steps)} steps')


if __name__ == '__main__':
    main()
