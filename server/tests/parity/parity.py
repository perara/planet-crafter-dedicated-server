"""Compare two probe transcripts of the same script: the real game host's (REF) and ours.

    python3 parity.py DATA_DIR SAVE.json REF.jsonl OURS.jsonl [--state-only] [--events-only] [--both-game]

--both-game compares two runs of the real host (their noise).

DATA_DIR holds the network tables (netdata.json) both transcripts were taken with; SAVE.json
is the save both servers started from (its world object ids are kept, new ones are numbered).

What legitimately differs is normalized away:
- timestamps ("t") and the order of messages inside one server tick (events are compared as
  sequences per step, not by arrival time);
- network object ids the server allocates: an object is named by scene id, by prefab hash and
  position for objects in the approval, and by prefab hash and creation order for new ones;
- world object ids the server allocates (WorldObjectsIdHandler: 20xxxxxxx, not in SAVE.json):
  named by the step they first appear in and their rank there ("WO@12.1", ...);
- floats are compared to 3 decimals (0.1% for large values).

Network variable deltas are decoded with the tables and compared as state: the value each
variable ends with, and how many deltas wrote it.
"""
import difflib
import json
import re
import struct
import sys


class R:
    def __init__(self, b):
        self.b, self.i = b, 0

    def take(self, n):
        if self.i + n > len(self.b):
            raise EOFError
        v = self.b[self.i:self.i + n]
        self.i += n
        return v

    def u8(self):
        return self.take(1)[0]

    def i32(self):
        return struct.unpack('<i', self.take(4))[0]

    def u16(self):
        return struct.unpack('<H', self.take(2))[0]

    def f32(self):
        return struct.unpack('<f', self.take(4))[0]

    def packed(self, bits_mask, raw):
        n = self.b[self.i] & bits_mask
        if n == raw:
            self.i += 1
            return int.from_bytes(self.take(4 if raw == 5 else 8), 'little')
        shift = 3 if bits_mask == 7 else 4
        return int.from_bytes(self.take(n), 'little') >> shift

    def pu16(self):
        n = self.b[self.i] & 3
        if n == 1:
            return self.u8() >> 2
        if n == 2:
            return self.u16() >> 2
        self.i += 1
        return self.u16()

    def pu32(self):
        return self.packed(7, 5)

    def pu64(self):
        return self.packed(15, 9)

    def pi32(self):
        v = self.pu32()
        return (v >> 1) ^ -(v & 1)

    def pi16(self):
        v = self.pu16()
        return (v >> 1) ^ -(v & 1)


def fixed_string(r):
    n = r.u16()
    return r.take(n).decode('utf-8', 'replace')


def player_data(r):
    raw = r.take(56)
    s = R(raw)
    pid = int.from_bytes(s.take(8), 'little')
    name = fixed_string(s)
    s.i = 40
    light_tier, light_on = struct.unpack('<h?', s.take(3))
    s.i = 44
    backpack, jetpack, skin = struct.unpack('<hhh', s.take(6))
    return {'id': pid, 'name': name, 'light': [light_on, light_tier], 'backpack': backpack, 'jetpack': jetpack, 'skin': skin}


ITEMS = {
    'NetworkList<Int32>': R.pi32,
    'NetworkList<Int16>': R.pi16,
    'NetworkList<GroupCountData>': lambda r: list(struct.unpack('<ii', r.take(8))),
    'NetworkList<Message>': lambda r: list(struct.unpack('<ii', r.take(8))),
    'NetworkList<GroupPerPlanetCountData>': lambda r: list(struct.unpack('<iii', r.take(12))),
    'NetworkList<PlayerData>': player_data,
}


def value(ty, r):
    inner = ty[len('NetworkVariable<'):-1] if ty.startswith('NetworkVariable<') else None
    if inner in ('Boolean',):
        return bool(r.u8())
    if inner in ('Byte', 'SByte'):
        return r.u8()
    if inner == 'Single':
        return r.f32()
    if inner == 'Double':
        return struct.unpack('<d', r.take(8))[0]
    if inner == 'Int32':
        return r.pi32()
    if inner == 'Int16':
        return r.pi16()
    if inner == 'UInt16':
        return r.pu16()
    if inner == 'UInt32':
        return r.pu32()
    if inner in ('UInt64', 'Int64'):
        return r.pu64()
    if inner in ('Color', 'Quaternion', 'Vector4'):
        return list(struct.unpack('<4f', r.take(16)))
    if inner == 'Vector3':
        return list(struct.unpack('<3f', r.take(12)))
    if inner in ('Vector2', 'Vector2Int'):
        return list(struct.unpack('<2f', r.take(8)))
    if inner and inner.startswith('FixedString'):
        n = r.i32()
        return r.take(n).decode('utf-8', 'replace')
    if ty.startswith('WorldUnit'):
        return list(struct.unpack('<ffffi', r.take(20)))
    if inner == 'WorldInstanceData':
        seed = r.i32()
        if seed == 0:
            return None
        left = r.i32()
        n = r.i32()
        scene = r.take(2 * n).decode('utf-16-le')
        rarity, difficulty = r.i32(), r.i32()
        recipe = [r.i32() for _ in range(r.i32())]
        return {'seed': seed, 'timeLeft': left, 'scene': scene, 'rarity': rarity, 'difficulty': difficulty, 'recipe': recipe}
    if inner == 'InstanceChoices':
        seeds = [r.i32() for _ in range(r.i32())]
        left = [r.i32() for _ in range(r.i32())]
        return {'seeds': seeds, 'timeLefts': left}
    if inner and (inner.endswith('Type') or inner.endswith('Status') or inner.endswith('State')):
        return r.i32()
    if ty in ITEMS:
        return [ITEMS[ty](r) for _ in range(r.u16())]
    raise ValueError(ty)


def list_events(ty, r):
    item = ITEMS[ty]
    out = []
    for _ in range(r.u16()):
        kind = r.u8()
        if kind == 0:
            out.append(['add', item(r)])
        elif kind == 1:
            out.append(['insert', r.pi32(), item(r)])
        elif kind == 2:
            out.append(['remove', item(r)])
        elif kind == 3:
            out.append(['removeAt', r.pi32()])
        elif kind == 4:
            out.append(['value', r.pi32(), item(r)])
        elif kind == 5:
            out.append(['clear'])
        elif kind == 6:
            out.append(['full', [item(r) for _ in range(r.u16())]])
        else:
            raise ValueError(kind)
    return out


def apply_events(items, events):
    items = list(items or [])
    for e in events:
        if e[0] == 'add':
            items.append(e[1])
        elif e[0] == 'insert':
            items.insert(e[1], e[2])
        elif e[0] == 'remove':
            if e[1] in items:
                items.remove(e[1])
        elif e[0] == 'removeAt':
            if 0 <= e[1] < len(items):
                del items[e[1]]
        elif e[0] == 'value':
            if 0 <= e[1] < len(items):
                items[e[1]] = e[2]
        elif e[0] == 'clear':
            items = []
        elif e[0] == 'full':
            items = list(e[1])
    return items


class Tables:
    def __init__(self, data_dir):
        d = json.load(open(f'{data_dir}/netdata.json'))
        self.scene = {o['id']: o for o in d['sceneObjects']}
        self.prefabs = {p['hash']: p for p in d['prefabs'].values()}
        self.prefab_name = {p['hash']: p['name'] for p in d['prefabs'].values()}
        self.group_name = {g['hash']: name for name, g in d['groups'].items()}
        # World object prefabs: one layout per group (groups sharing a prefab share its hash).
        for name, layout in json.load(open(f'{data_dir}/gamedata.json'))['layouts'].items():
            self.prefabs.setdefault(layout['hash'], layout)
            self.prefab_name.setdefault(layout['hash'], layout.get('prefab') or name)

    def behaviours(self, hash_, net_id):
        if net_id in self.scene:
            return self.scene[net_id]['behaviours']
        p = self.prefabs.get(hash_)
        return p['behaviours'] if p else None


def decode_sync(behaviours, raw):
    r = R(raw)
    out = {}
    try:
        for b in behaviours:
            for v in b['vars']:
                out[f"{b['class']}.{v['name']}"] = value(v['type'], r)
    except (EOFError, ValueError, IndexError) as e:
        out['?error'] = repr(e)
    out['?tail'] = transform_tail(raw[r.i:])
    return out


def transform_tail(raw):
    """The NetworkTransform states after the variables: per state the behaviour, the packed
    flags, the tick (server time: masked) and the floats. Anything else stays hex."""
    try:
        r = R(raw)
        states = []
        for _ in range(r.u8()):
            behaviour, n = r.u16(), r.i32()
            s = R(r.take(n))
            flags = s.pu32()
            s.pu32()
            floats = [s.f32() for _ in range((n - s.i) // 4)]
            states.append({'behaviour': behaviour, 'flags': flags, 'tick': 'T', 'values': floats, 'rest': s.b[s.i:].hex()})
        return {'states': states, 'rest': raw[r.i:].hex()}
    except (EOFError, IndexError):
        return raw.hex()


def rnd(v):
    if isinstance(v, bool) or v is None:
        return v
    if isinstance(v, float):
        if v != v:
            return 'NaN'
        return round(v, 3) if abs(v) < 1000 else float('%.4g' % v)
    if isinstance(v, list):
        return [rnd(x) for x in v]
    if isinstance(v, dict):
        return {k: rnd(x) for k, x in v.items()}
    return v


def save_ids(path):
    text = open(path, encoding='utf-8-sig').read()
    return {int(x) for x in re.findall(r'"id":(\d+)', text)}


class Transcript:
    def __init__(self, path, tables, known_ids):
        self.lines = [json.loads(l) for l in open(path) if l.strip().startswith('{')]
        self.t = tables
        self.known = known_ids
        self.labels = {}     # net id -> label
        self.objects = {}    # label -> {hash, position, vars}
        self.created = {}    # hash -> count of creations during the run
        self.wo = {}         # allocated world object id -> "WO@step.n"
        self.wo_in_step = {}  # step -> world object ids first seen there
        self.step = 0
        self.state = {}      # (label, var) -> value
        self.writes = {}     # (label, var) -> deltas that wrote it
        self.series = {}     # (label, var) -> [(t, value)] of numbers that drift with time
        self.events = []     # (step, normalized line)
        self.at_join = {}    # (label, var) -> value in the approval
        self.growth = {}     # label -> [(t, growth)] the server told
        self.failed_adds = {}  # group hash -> AddNewItemToInventory calls that failed
        self.is_game = False   # the real game host's transcript
        self.player = next((l['player'] for l in self.lines if l['msg'] == 'joined'), None)
        self.client = next((l['client'] for l in self.lines if l['msg'] == 'joined'), None)
        self._run()

    def label(self, net_id):
        if net_id in self.labels:
            return self.labels[net_id]
        if net_id == self.player:
            return '$player'
        if net_id in self.t.scene:
            return 'scene:' + self.t.scene[net_id]['name']
        return f'net:{net_id}'

    def norm(self, v):
        if isinstance(v, bool):
            return v
        if isinstance(v, int) and 200_000_000 <= v < 210_000_000 and v not in self.known:
            # Named by the script step it first appeared in, and its rank there: one server
            # allocating an extra object does not renumber all later ones.
            if v not in self.wo:
                self.wo_in_step[self.step] = self.wo_in_step.get(self.step, 0) + 1
                self.wo[v] = f'WO@{self.step}.{self.wo_in_step[self.step]}'
            return self.wo[v]
        if isinstance(v, list):
            return [self.norm(x) for x in v]
        if isinstance(v, dict):
            return {k: self.norm(x) for k, x in v.items()}
        return rnd(v)

    def _object(self, l, created):
        hash_ = int(l['hash'], 16)
        name = self.t.prefab_name.get(hash_, l['hash'])
        if l['id'] == self.player or (l['owner'] == self.client and name == 'Player'):
            label = '$player'
        elif l['id'] in self.t.scene:
            label = 'scene:' + self.t.scene[l['id']]['name']
        elif created:
            self.created[hash_] = self.created.get(hash_, 0) + 1
            label = f'new:{name}#{self.created[hash_]}'
        else:
            pos = l.get('position')
            label = f'{name}@' + (','.join('%.1f' % x for x in pos) if pos else 'nopos')
            while label in self.objects:
                label += "'"
        self.labels[l['id']] = label
        beh = self.t.behaviours(hash_, l['id'])
        vars_ = decode_sync(beh, bytes.fromhex(l['sync'])) if beh is not None else {'?sync': l['sync']}
        self.objects[label] = {'hash': l['hash'], 'owner': 'me' if l['owner'] == self.client and l['owner'] else l['owner'],
                               'position': rnd(l.get('position')), 'rotation': rnd(l.get('rotation')), 'vars': self.norm(vars_)}
        for k, v in vars_.items():
            if not k.startswith('?'):
                self.state[(label, k)] = v
                if not created:
                    self.at_join[(label, k)] = v
        return label

    def _delta(self, l):
        label = self.label(l['object'])
        obj = next((o for o in self.objects.values() if False), None)
        net_hash = None
        for nid, lab in self.labels.items():
            if lab == label:
                net_hash = int(self.objects[lab]['hash'], 16)
                beh = self.t.behaviours(net_hash, nid)
                break
        else:
            beh = self.t.behaviours(None, l['object'])
        if beh is None:
            return {'?undecoded': l['hex']}
        b = next((b for b in beh if b['behaviourId'] == l['behaviour']), None)
        if b is None:
            return {'?behaviour': l['behaviour']}
        r = R(bytes.fromhex(l['hex']))
        r.i32()
        changed = {}
        try:
            for v in b['vars']:
                if not r.u8():
                    continue
                key = f"{b['class']}.{v['name']}"
                if v['type'].startswith('NetworkList<'):
                    ev = list_events(v['type'], r)
                    changed[key] = ['events', ev]
                    self.state[(label, key)] = apply_events(self.state.get((label, key)), ev)
                elif v['type'].startswith('NetworkVariable<FixedString'):
                    # FixedStringSerializer.WriteDelta: 1 = whole string; 0 = length, changed-bit
                    # vector, changed bytes.
                    if r.u8() == 1:
                        val = value(v['type'], r)
                    else:
                        n = r.pi32()
                        bits = r.take(r.i32())
                        old = bytearray((self.state.get((label, key)) or '').encode())[:n].ljust(n, b'\0')
                        for i in range(n):
                            if i // 8 < len(bits) and bits[i // 8] >> (i % 8) & 1:
                                old[i] = r.u8()
                        val = old.decode('utf-8', 'replace')
                    changed[key] = val
                    self.state[(label, key)] = val
                else:
                    val = value(v['type'], r)
                    changed[key] = val
                    self.state[(label, key)] = val
                    if isinstance(val, float) or v['type'].startswith('WorldUnit'):
                        self.series.setdefault((label, key), []).append((l['t'], val))
                self.writes[(label, key)] = self.writes.get((label, key), 0) + 1
        except (EOFError, ValueError, IndexError) as e:
            changed['?error'] = f'{e!r} at {r.i}: {l["hex"]}'
        return changed

    def _run(self):
        step = 0
        net_ids = {}
        adding = None
        for l in self.lines:
            m = l['msg']
            if m == 'approved object':
                self._object(l, created=False)
            elif m == 'create':
                label = self._object(l, created=True)
                o = self.objects[label]
                self.events.append((step, {'msg': 'create', 'object': label, 'owner': o['owner'], 'position': o['position']}))
            elif m == 'destroy':
                self.events.append((step, {'msg': 'destroy', 'object': self.label(l['id'])}))
            elif m == 'call':
                step += 1
                self.step = step
                adding = l['args'][0] if l['name'] == 'InventoriesHandler.AddNewItemToInventoryServerRpc' and 'args' in l else None
                if 'skipped' in l:
                    self.events.append((step, {'msg': 'call', 'name': l['name'], 'skipped': l['skipped'], 'tag': l.get('tag')}))
                else:
                    self.events.append((step, {'msg': 'call', 'name': l['name'], 'object': self.label(l['object']), 'args': self.norm(l['args']), 'tag': l.get('tag')}))
            elif m == 'rpc':
                args = l['args']
                if l['name'] == 'InventoriesHandler.AddNewItemClientRpc' and adding is not None and args and args[0] is False:
                    self.failed_adds[adding] = self.failed_adds.get(adding, 0) + 1
                # Growth drifts with time: compared by its rate (diff_growth), not in the events.
                if l['name'] == 'GrowthProxy.SetCurrentGrowthClientRpc' and args and isinstance(args[0], float):
                    self.growth.setdefault(self.label(l['object']), []).append((l['t'], args[0]))
                    args = ['growth']
                # A network object id the server allocated (Netcode reuses freed ones): its label.
                if l['name'] == 'WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc' and args and args[0]:
                    net_ids[args[0]] = self.label(args[0])
                    args = [net_ids[args[0]]]
                self.events.append((step, {'msg': 'rpc', 'name': l['name'], 'object': self.label(l['object']), 'args': self.norm(args)}))
            elif m == 'delta':
                self._delta(l)
            elif m in ('captured', 'capture failed', 'disconnected'):
                e = {k: self.norm(v) for k, v in l.items() if k != 't'}
                if m == 'captured' and l['value'] in net_ids:
                    e['value'] = net_ids[l['value']]
                self.events.append((step, e))


# Per-group counts of world objects: the save's machines produced for longer on the reference
# before the probe joined, so these are compared by their change during the run.
COUNTS = ('WorldObjectsHandler._allWorldObjectsGroups', 'LogisticInterplanetaryManager._itemsInPlanetaryLogisticsGroups')


def count_change(t, k):
    def table(v):
        return {tuple(x[:-1]): x[-1] for x in (v or [])}
    start, end = table(t.at_join.get(k)), table(t.state.get(k))
    change = {g: end.get(g, 0) - start.get(g, 0) for g in set(start) | set(end)}
    # The game keeps the object a failed AddNewItemToInventory made; this server destroys it
    # (EXPECTED: safety rules). Counted out of the game's change.
    if t.is_game and k[1].endswith('_allWorldObjectsGroups'):
        for g, n in t.failed_adds.items():
            change[(g,)] = change.get((g,), 0) - n
    return {str(g): v for g, v in change.items() if v}


def dumps(x):
    return json.dumps(x, sort_keys=True)


def close(a, b):
    if isinstance(a, float) and isinstance(b, float):
        return a == b or abs(a - b) <= 1e-3 * max(1.0, abs(a), abs(b))
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(close(x, y) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(close(a[k], b[k]) for k in a)
    return a == b


# A server message the other server sent the same, only a step or two earlier or later: the
# coroutines of the two servers run on their own clocks (EXPECTED: Time).
SHIFT = 10


def diff_events(ref, ours):
    n = 0
    steps = sorted({s for s, _ in ref.events} | {s for s, _ in ours.events})
    only = {}
    for s in steps:
        # Within a step the order of answers can follow a tick's (hash-based) order: compare sorted.
        a = sorted(dumps(e) for st, e in ref.events if st == s)
        b = sorted(dumps(e) for st, e in ours.events if st == s)
        for op, i1, i2, j1, j2 in difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes():
            if op != 'equal':
                only.setdefault(s, ([], []))
                only[s][0].extend(a[i1:i2])
                only[s][1].extend(b[j1:j2])
    # Pair what one server sent in a step with the same message the other sent within SHIFT steps.
    shifted = []
    for s in sorted(only):
        for x in list(only[s][0]):
            if '"msg": "call"' in x:
                continue
            for t in sorted(only, key=lambda t: abs(t - s)):
                if t != s and abs(t - s) <= SHIFT and x in only[t][1]:
                    only[s][0].remove(x)
                    only[t][1].remove(x)
                    shifted.append((s, t, x))
                    break
    for s in sorted(only):
        a, b = only[s]
        if not a and not b:
            continue
        call = next((e for st, e in ref.events if st == s and e['msg'] == 'call'), {'name': 'join'})
        print(f'## step {s}: {call["name"]}' + (f' [{call["tag"]}]' if call.get('tag') else ''))
        for x in a:
            print(f'- ref:  {x}')
        for x in b:
            print(f'+ ours: {x}')
        n += max(len(a), len(b))
    if shifted:
        print(f'## steps apart (the same message, at most {SHIFT} steps earlier or later)')
        for s, t, x in shifted:
            print(f'~ shifted: ref step {s}, ours step {t}: {x}')
        n += len(shifted)
    return n


def diff_growth(ref, ours):
    """The growth each server told for an object: the same rate (5 %) between its reports, when
    they span 30 s or more; otherwise the same values (1 %, or 0.5). Where it stood at one moment
    depends on when each server's machines started (EXPECTED: Time)."""
    n = 0
    for k in sorted(set(ref.growth) | set(ours.growth)):
        # The first report can be the spawn's, before the machine started: rates from the second.
        a, b = ref.growth.get(k, []), ours.growth.get(k, [])
        a, b = (a[1:] if len(a) > 2 else a), (b[1:] if len(b) > 2 else b)
        rate = lambda p: (p[-1][1] - p[0][1]) * 1000 / (p[-1][0] - p[0][0]) if len(p) > 1 and p[-1][0] - p[0][0] >= 30000 else None
        ra, rb = rate(a), rate(b)
        if ra is not None and rb is not None:
            if abs(ra - rb) <= max(0.05 * max(abs(ra), abs(rb)), 1e-4):
                continue
        elif len(a) == len(b) and all(abs(x[1] - y[1]) <= max(0.01 * max(abs(x[1]), abs(y[1])), 0.5) for x, y in zip(a, b)):
            continue
        print(f'~ {k} growth: ref {[round(v, 3) for _, v in a][:12]} (rate {ra}) ours {[round(v, 3) for _, v in b][:12]} (rate {rb})')
        n += 1
    return n


def diff_approved(ref, ours):
    n = 0
    header = lambda t: next((l.get('header') for l in t.lines if l['msg'] == 'approved'), None)
    if not close(header(ref), header(ours)):
        print(f'~ approval header: ref {dumps(header(ref))} ours {dumps(header(ours))}')
        n += 1
    ra = {k: v for k, v in ref.objects.items() if not k.startswith('new:')}
    oa = {k: v for k, v in ours.objects.items() if not k.startswith('new:')}
    for k in sorted(set(ra) | set(oa)):
        if k not in oa:
            print(f'- ref only object {k}: {dumps(ra[k])[:300]}'); n += 1
        elif k not in ra:
            print(f'+ ours only object {k}: {dumps(oa[k])[:300]}'); n += 1
        elif not close(ra[k], oa[k]):
            for f in ('hash', 'owner', 'position', 'rotation'):
                if not close(ra[k][f], oa[k][f]):
                    print(f'~ object {k} {f}: ref {ra[k][f]} ours {oa[k][f]}'); n += 1
            rv, ov = ra[k]['vars'], oa[k]['vars']
            for v in sorted(set(rv) | set(ov)):
                if v.startswith('WorldUnitsPlanet._unit') or v.endswith(COUNTS):
                    continue  # ours stands still until the first player; compared in the state
                if not close(rv.get(v), ov.get(v)):
                    print(f'~ object {k} {v}: ref {dumps(rv.get(v))[:400]} ours {dumps(ov.get(v))[:400]}'); n += 1
    return n


def slope(series):
    """Change per second over the second half of a series (the first half may still settle)."""
    t0, t1 = series[0][0], series[-1][0]
    if t1 - t0 < 1500 or len(series) < 4:
        return None
    half = [p for p in series if p[0] >= (t0 + t1) / 2]
    (ta, a), (tb, b) = half[0], half[-1]
    a, b = (a[0], b[0]) if isinstance(a, list) else (a, b)
    return (b - a) * 1000 / (tb - ta) if tb > ta else None


def diff_state(ref, ours):
    """Final values; a value that drifts with time (gauges, world units) by its rate instead,
    since the two servers ran for different times before and during the script."""
    n = 0
    keys = sorted(set(ref.state) | set(ours.state))
    for k in keys:
        if k[1].endswith(COUNTS):
            a, b = count_change(ref, k), count_change(ours, k)
            if a != b:
                name = lambda g: ','.join(ref.t.group_name.get(int(x), x) for x in g.strip('()').replace(' ', '').split(',') if x)
                for g in sorted(set(a) | set(b), key=name):
                    if a.get(g, 0) != b.get(g, 0):
                        print(f'~ {k[0]} {k[1]} of {name(g)}: changed by ref {a.get(g, 0)} ours {b.get(g, 0)} during the run')
                        n += 1
            continue
        a, b = ref.norm(ref.state.get(k)), ours.norm(ours.state.get(k))
        sa, sb = ref.series.get(k), ours.series.get(k)
        if isinstance(a, list) and isinstance(b, list) and len(a) == 5 and k[1].startswith('WorldUnitsPlanet.') and close(a[1:], b[1:]):
            continue  # a unit's rates are what the servers compute; its float32 total is coarse
        if sa and sb and slope(sa) is not None and slope(sb) is not None:
            ra, rb = slope(sa), slope(sb)
            rates_equal = isinstance(a, list) and close(rnd(a[1:]), rnd(b[1:])) or not isinstance(a, list)
            if rates_equal and abs(ra - rb) <= max(0.05 * max(abs(ra), abs(rb)), 1e-3):
                continue
            print(f'~ {k[0]} {k[1]}: rate ref {ra:.4g}/s ours {rb:.4g}/s; final ref {dumps(a)[:200]} ours {dumps(b)[:200]}')
            n += 1
            continue
        if isinstance(a, list) and isinstance(b, list) and len(a) == 5 and k[1].split('._unit')[0].endswith('WorldUnitsPlanet') and close(a[1:], b[1:]):
            continue  # a unit's total depends on how long each server ran; its rates do not
        if not close(a, b):
            print(f'~ {k[0]} {k[1]}: ref {dumps(a)[:400]} ours {dumps(b)[:400]} (writes ref {ref.writes.get(k, 0)}, ours {ours.writes.get(k, 0)})')
            n += 1
    return n


def main():
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    flags = {a for a in sys.argv[1:] if a.startswith('--')}
    data_dir, save, ref_path, ours_path = args
    t = Tables(data_dir)
    known = save_ids(save)
    ref, ours = Transcript(ref_path, t, known), Transcript(ours_path, t, known)
    # --both-game: two runs of the real host (the noise between them).
    ref.is_game, ours.is_game = True, '--both-game' in flags
    total = 0
    if '--state-only' not in flags and '--events-only' not in flags:
        print('# approval (objects and their variables at join)')
        total += diff_approved(ref, ours)
    if '--state-only' not in flags:
        print('# events (RPCs, spawns, despawns per script step)')
        total += diff_events(ref, ours)
    if '--events-only' not in flags:
        print('# state (network variables at the end; writes = deltas that set them)')
        total += diff_state(ref, ours)
        total += diff_growth(ref, ours)
    print(f'{total} difference(s)')
    sys.exit(1 if total else 0)


if __name__ == '__main__':
    main()
