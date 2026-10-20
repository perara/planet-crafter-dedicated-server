"""Which demands the drones serve first: the order of equal priorities, seen on the real game.

    python3 demand_order.py make START.json OUT.json       # OUT.json.ids names the containers
    python3 demand_order.py served OUT.json.ids NAME=SAVE.json...

`make`: 17 empty containers of the planet the world starts on (the ones nearest a depot) get the
demand "BalzarQuartz" at priority 9, so they stand alone on top of the demand list; 17 more get "PulsarQuartz" at
priority 0, among the 159 demand inventories of the save that are at 0 too. Each demand inventory holds one
item. The two depots with the quartz keep 8 items each, so only 8 of the 17 can ever be served.
Which 8 depends only on the order of the demand list: the game sorts it once, at start, with
List<T>.Sort (an introsort: equal priorities end in an order of its own, src/dotnet_sort.rs).
Run lg_idle5 from OUT.json on the game (three times: it is the same every time) and on this server.

`served`: for each class a line of X (has its item) and . per container, and how many containers
differ between each two runs.
"""
import json
import math
import re
import sys


def make(src, dst):
    sec = open(src, encoding='utf-8-sig', newline='').read().split('@')
    objs = [json.loads(r) for r in sec[3].split('|') if r.strip()]
    invs = [json.loads(r) for r in sec[4].split('|') if r.strip()]
    prime = -1140328421
    byid = {i['id']: i for i in invs}
    depot = {o['liId']: o for o in objs if o['gId'] == 'PlanetaryDeliveryDepot1' and o.get('planet') == prime}
    pos = lambda o: tuple(float(v) for v in o['pos'].split(','))
    plain = lambda o: o.get('liId') in byid and not byid[o['liId']]['woIds'] and not byid[o['liId']].get('demandGrps') and not byid[o['liId']].get('supplyGrps')
    near = sorted((o for o in objs if o['gId'] in ('Container3', 'Counter1', 'Counter2') and o.get('planet') == prime and plain(o)), key=lambda o: math.dist(pos(o), pos(depot[1722])))[:34]
    chosen = near[0::2] + near[1::2]
    ids = {}
    for k, (cls, item, priority) in enumerate((('A', 'BalzarQuartz', 9), ('B', 'PulsarQuartz', 0))):
        ids[cls] = [c['liId'] for c in chosen[k * 17:(k + 1) * 17]]
        for i in ids[cls]:
            byid[i].update(size=1, demandGrps=item, supplyGrps='', priority=priority)
    gone = set()
    for inv_id in (1722, 1721):
        ws = [w for w in byid[inv_id]['woIds'].split(',') if w]
        byid[inv_id]['woIds'] = ','.join(ws[:8])
        gone.update(int(w) for w in ws[8:])
    objs = [o for o in objs if o['id'] not in gone]
    for k, rows in ((3, objs), (4, invs)):
        pre, _, post = re.match(r'(\s*)(.*?)(\s*)$', sec[k], re.S).groups()
        sec[k] = pre + '|\n'.join(json.dumps(r, separators=(',', ':')) for r in rows) + post
    open(dst, 'w', encoding='utf-8-sig', newline='').write('@'.join(sec))
    json.dump(ids, open(dst + '.ids', 'w'))


def served(ids_path, runs):
    ids = json.load(open(ids_path))
    res = {}
    for a in runs:
        name, path = a.split('=', 1)
        text = open(path, encoding='utf-8-sig').read()
        invs = [json.loads(r) for r in text.strip().split('@')[4].split('|') if r.strip()]
        n = {i['id']: len([w for w in str(i['woIds']).split(',') if w]) for i in invs}
        res[name] = {k: ''.join('X' if n.get(i) else '.' for i in v) for k, v in ids.items()}
    for k in ids:
        print(f'class {k}:')
        for name in res:
            print(f'  {name:10} {res[name][k]}  served {res[name][k].count("X")}')
        print('  containers that differ:  ' + ''.join(f'{b:>10}' for b in res))
        for a in res:
            print(f'  {a:22}' + ''.join(f'{sum(x != y for x, y in zip(res[a][k], res[b][k])):>10}' for b in res))


if __name__ == '__main__':
    if sys.argv[1] == 'make':
        make(*sys.argv[2:4])
    else:
        served(sys.argv[2], sys.argv[3:])
