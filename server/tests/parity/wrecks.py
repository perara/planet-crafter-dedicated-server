"""The wreck comparison with the real game (EXPECTED.md "Wrecks are laid out by Tessera"):

    python3 wrecks.py gen OUT_DIR                       probe scripts: OUT_DIR/wrecks_mix.json (24 scene wrecks of the
                                                        three sizes, one process) and wrecks_inst.json (the 8 world
                                                        instance scenes, each with its first wreck generated)
    sweep.sh both OUT OUT_DIR/wrecks_mix.json OUT_DIR/wrecks_inst.json     (as the other scripts)
    python3 wrecks.py saves START.json REF.save.json OURS.save.json        the wreck objects the two saves hold
    python3 wrecks.py events NAME.diff                  the differences parity.py listed, after the sign of a zero, a
                                                        quaternion's sign and 0.0025 are let go

The scripts are not in scripts/: the completeness map counts what that folder sends. A wreck's place decides
its seed (the world seed halved and the distance from the origin), so the places are fixed by the generator below.
"""
import collections
import json
import math
import random
import re
import sys


def quat(yaw, tx=0.0, tz=0.0):
    """Yaw about y, then a small tilt about x and z, as a unit quaternion (x, y, z, w)."""
    def q(axis, a):
        s, c = math.sin(a / 2), math.cos(a / 2)
        return {'x': (s, 0, 0, c), 'y': (0, s, 0, c), 'z': (0, 0, s, c)}[axis]

    def mul(a, b):
        ax, ay, az, aw = a
        bx, by, bz, bw = b
        return (aw*bx + ax*bw + ay*bz - az*by, aw*by - ax*bz + ay*bw + az*bx, aw*bz + ax*by - ay*bx + az*bw, aw*bw - ax*bx - ay*by - az*bz)
    r = mul(mul(q('y', math.radians(yaw)), q('x', math.radians(tx))), q('z', math.radians(tz)))
    n = math.sqrt(sum(v * v for v in r))
    return [round(v / n, 7) for v in r]


def gen(out):
    rnd = random.Random(20261020)
    steps = []
    for i in range(24):
        d = i % 3
        ang, r = rnd.uniform(0, 2 * math.pi), rnd.uniform(300, 2400)
        pos = [round(r * math.cos(ang), 3), round(rnd.uniform(-20, 80), 3), round(r * math.sin(ang), 3)]
        yaw, tilt = rnd.choice([0, 90, 180, 270, rnd.uniform(0, 360)]), rnd.choice([0, 0, rnd.uniform(-3, 3)])
        steps.append({'rpc': 'ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc', 'on': 'ProceduralInstancesHandler', 'args': [101990000 + i, pos, quat(yaw, tilt, tilt / 2), d], 'after': 600, 'capture': {'var': f'w{i}', 'create': True}})
        steps.append({'rpc': 'GenerateOnTrigger.GenerateNetworkObjectsServerRpc', 'on': [f'$w{i}', 0], 'args': [], 'after': [4000, 6000, 14000][d]})
    steps.append({'wait': 3000})
    json.dump({'name': 'ParityA', 'id': 76561197999999901, 'steps': steps}, open(f'{out}/wrecks_mix.json', 'w'))
    scenes = ['Instance-AlienCorail', 'Instance-CargoSpaceShip', 'Instance-Cascade', 'Instance-Caves', 'Instance-Gouffre', 'Instance-HugeTrees', 'Instance-Valley', 'Instance-Volcano']
    seeds = [271828182, 314159265, 161803398, 577215664, 141421356, 173205080, 223606797, 264575131]
    steps = []
    for i, (scene, seed) in enumerate(zip(scenes, seeds)):
        d = i % 3
        # The game loads a scene the first time it is opened: the first open needs longer.
        steps.append({'rpc': 'WorldInstanceHandler.SetAndOpenWorldInstanceSceneServerRpc', 'on': 'WorldInstanceHandler', 'args': [[seed, 1200, scene, (i * 2) % 6, d, []]], 'after': 9000 if i == 0 else 4000, 'capture': {'var': f'g{i}', 'create': True}})
        steps.append({'rpc': 'GenerateOnTrigger.GenerateNetworkObjectsServerRpc', 'on': [f'$g{i}', 0], 'args': [], 'after': [4000, 6000, 14000][d]})
        steps.append({'rpc': 'WorldInstanceHandler.UnloadInstanceServerRpc', 'on': 'WorldInstanceHandler', 'args': [], 'after': 2500})
    steps.append({'wait': 3000})
    json.dump({'name': 'ParityA', 'id': 76561197999999901, 'steps': steps}, open(f'{out}/wrecks_inst.json', 'w'))


def load(path):
    return [[json.loads(r) for r in s.split('|') if r.strip()] for s in open(path, encoding='utf-8-sig').read().strip().split('@')]


def saves(start, ref, ours):
    """Objects not in START that sit in the world, matched by group and position (0.02 m)."""
    known = {int(x) for x in re.findall(r'"id":(\d+)', open(start, encoding='utf-8-sig').read())}

    def placed(save):
        inv, byid = {i['id']: i for i in save[4]}, {o['id']: o for o in save[3]}
        out = []
        for o in (o for o in save[3] if o['id'] not in known and 'pos' in o):
            items = None
            if o.get('liId') in inv:
                ids = [int(w) for w in str(inv[o['liId']].get('woIds', '')).split(',') if w]
                items = tuple(sorted(byid[i]['gId'] for i in ids if i in byid))
            out.append({'g': o['gId'], 'p': [float(v) for v in o['pos'].split(',')], 'q': [float(v) for v in o['rot'].split(',')] if o.get('rot') else None, 'items': items})
        return out
    a, b = placed(load(ref)), placed(load(ours))
    pool = collections.defaultdict(list)
    for o in b:
        pool[o['g']].append(o)
    pairs, only_ref = [], []
    for o in a:
        m = next((x for x in pool[o['g']] if all(abs(u - v) <= 0.02 for u, v in zip(o['p'], x['p']))), None)
        if m:
            pool[o['g']].remove(m)
            pairs.append((o, m))
        else:
            only_ref.append(o)
    only_ours = [x for l in pool.values() for x in l]
    print(f'placed new objects: ref {len(a)} ours {len(b)}; matched {len(pairs)}, only in ref {len(only_ref)}, only in ours {len(only_ours)}, groups {len({o["g"] for o in a})}')
    for o in only_ref[:10]:
        print('  ref only', o['g'], o['p'])
    for o in only_ours[:10]:
        print('  ours only', o['g'], o['p'])
    same_q = lambda p, q: p is None or q is None or abs(abs(sum(x * y for x, y in zip(p, q))) - 1) <= 1e-4
    print('rotation differs:', sum(1 for o, m in pairs if not same_q(o['q'], m['q'])), '; largest position difference:', max([max(abs(x - y) for x, y in zip(o['p'], m['p'])) for o, m in pairs] or [0]))
    cont = [(o, m) for o, m in pairs if o['items'] is not None and m['items'] is not None]
    print(f'containers {len(cont)}: same item count {sum(1 for o, m in cont if len(o["items"]) == len(m["items"]))}, same item groups {sum(1 for o, m in cont if o["items"] == m["items"])} (the loot is random in the game: compare REF against a second REF run too)')


def events(diff):
    def canon_q(v):
        for x in v:
            if abs(x) > 1e-4:
                return v if x > 0 else [-y for y in v]
        return v

    def close(a, b, tol=0.0025):
        if isinstance(a, float) or isinstance(b, float):
            return isinstance(a, (int, float)) and isinstance(b, (int, float)) and abs(a - b) <= tol * max(1.0, abs(a) / 1000)
        if isinstance(a, list) and isinstance(b, list):
            if len(a) == 4 and all(isinstance(x, (int, float)) for x in a + b):
                a, b = canon_q(a), canon_q(b)
            return len(a) == len(b) and all(close(x, y, tol) for x, y in zip(a, b))
        if isinstance(a, dict) and isinstance(b, dict):
            return a.keys() == b.keys() and all(close(a[k], b[k], tol) for k in a)
        return a == b
    step, ref, ours = None, collections.defaultdict(list), collections.defaultdict(list)
    for line in open(diff):
        if m := re.match(r'## step (\d+): (.*)', line):
            step = (int(m[1]), m[2])
        elif m := re.match(r'- ref:\s+(\{.*\})', line):
            ref[step].append(json.loads(m[1]))
        elif m := re.match(r'\+ ours:\s+(\{.*\})', line):
            ours[step].append(json.loads(m[1]))
    matched, left = 0, []
    for s in set(ref) | set(ours):
        r, o = list(ref[s]), list(ours[s])
        for x in list(r):
            j = next((i for i, y in enumerate(o) if close(x, y)), None)
            if j is not None:
                o.pop(j)
                r.remove(x)
                matched += 1
        left += [('ref', s, x) for x in r] + [('ours', s, y) for y in o]
    print(f'{matched} events differ in writing only; left over: {len(left)}')
    for side, s, e in left:
        print(' ', side, s[0], e.get('name') or e.get('hash'), json.dumps(e)[:200])


if __name__ == '__main__':
    cmd = sys.argv[1] if len(sys.argv) > 1 else ''
    if cmd == 'gen' and len(sys.argv) == 3:
        gen(sys.argv[2])
    elif cmd == 'saves' and len(sys.argv) == 5:
        saves(*sys.argv[2:])
    elif cmd == 'events' and len(sys.argv) == 3:
        events(sys.argv[2])
    else:
        print(__doc__)
        sys.exit(2)
