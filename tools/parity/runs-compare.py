#!/usr/bin/env python3
"""Where the end saves of several runs differ in who holds what: for the parts of the game that
are random or order-dependent (drones sorting items into demand inventories, honey and other
items moved by logistics), a single diff means little; the question is whether this server
falls inside the spread of the real game's own runs. Works on the saves a sweep run leaves
(NAME.ref.save.json, NAME.ref2.save.json, NAME.ours.save.json) and the start save.

    runs-compare.py held START ITEM LABEL=SAVE...
        the items of group ITEM in inventories, by the group of the holder (a machine, a
        container, a player), at the start and at the end of each run
    runs-compare.py inventories START ITEM LABEL=SAVE...
        the same per inventory id: only the inventories whose count differs between the runs,
        with the holder, its priority
    runs-compare.py spread START LABEL=SAVE...
        per item group, the items held and the objects there are: the runs labelled ref... are the
        game's own, the others are flagged where they fall outside [min, max] of the game's
        runs by more than 1
    runs-compare.py demand START LABEL=SAVE...
        the demand inventories (those with demandGrps): items gained over the run, and the
        distance between every two runs (the sum of the per-inventory differences)
    runs-compare.py served IDS.json LABEL=SAVE...
        for the classes of containers mk-demand-save.py wrote to IDS.json (its OUT.ids), which
        of them ended holding anything (X) in each run, and how many differ between runs

START and SAVE are Planet Crafter saves (sections split by @, entries by |). LABEL is a name
for the column. Python 3 standard library only. A pass: for `spread`, no run flagged; for the
others, the columns of the runs that should agree are equal (a column of this server equal to a
column of the game, or inside the game's own runs' spread).
"""
import argparse
import collections
import json


def load(path):
    text = open(path, encoding='utf-8-sig').read()
    return [[json.loads(r) for r in s.split('|') if r.strip()] for s in text.strip().split('@')]


def owners(save):
    """inventory id -> the object that owns it (its own inventory, or one of its slots)"""
    owner = {o['liId']: o for o in save[3] if o.get('liId')}
    for o in save[3]:
        for i in str(o.get('siIds', '')).split(','):
            if i:
                owner.setdefault(int(i), o)
    return owner


def held(save):
    """(items per (holder group, item group), items per item group, objects per group)"""
    wo = {o['id']: o for o in save[3]}
    owner = owners(save)
    by_holder, total = collections.Counter(), collections.Counter()
    for inv in save[4]:
        h = owner.get(inv['id'])
        for w in str(inv['woIds']).split(','):
            if w and int(w) in wo:
                g = wo[int(w)]['gId']
                total[g] += 1
                by_holder[(h['gId'] if h else '(player/none)', g)] += 1
    return by_holder, total, collections.Counter(o['gId'] for o in save[3])


def labelled(specs):
    return [(s.split('=', 1)[0], s.split('=', 1)[1]) for s in specs]


def cmd_held(a):
    start = held(load(a.start))
    runs = [(n, held(load(p))) for n, p in labelled(a.runs)]
    print('totals of', a.item, 'in inventories: start', start[1][a.item], '| ' + ' | '.join(f'{n} {r[1][a.item]}' for n, r in runs))
    keys = sorted({k for _, r in runs for k in r[0] if k[1] == a.item} | {k for k in start[0] if k[1] == a.item}, key=lambda k: -start[0][k])
    print(f'{"holder":28} {"start":>6} ' + ' '.join(f'{n:>8}' for n, _ in runs))
    for k in keys:
        print(f'{k[0]:28} {start[0][k]:6} ' + ' '.join(f'{r[0][k]:8}' for _, r in runs))


def per_inventory(save, item):
    wo = {o['id']: o for o in save[3]}
    c = collections.Counter()
    for inv in save[4]:
        for w in str(inv['woIds']).split(','):
            if w and int(w) in wo and wo[int(w)]['gId'] == item:
                c[inv['id']] += 1
    return c


def cmd_inventories(a):
    s0 = load(a.start)
    owner = owners(s0)
    inv0 = {i['id']: i for i in s0[4]}
    runs = [(n, per_inventory(load(p), a.item)) for n, p in labelled(a.runs)]
    print(f'{"inv":>6} {"holder":22} {"prio":>4} ' + ' '.join(f'{n:>8}' for n, _ in runs))
    for i in sorted({k for _, r in runs for k in r}):
        vals = [r[i] for _, r in runs]
        if len(set(vals)) > 1:
            o = owner.get(i)
            print(f'{i:6} {(o["gId"] if o else "?"):22} {inv0.get(i, {}).get("priority", ""):>4} ' + ' '.join(f'{v:8}' for v in vals))


def cmd_spread(a):
    start = held(load(a.start))
    refs, runs = [], []
    for n, p in labelled(a.runs):
        (refs if n.startswith('ref') else runs).append((n, held(load(p))))
    if not refs:
        raise SystemExit('spread: at least one run labelled ref... (the game\'s own) is needed')
    groups = sorted(set(start[1]) | set(start[2]) | {g for _, h in refs + runs for g in list(h[1]) + list(h[2])})
    print(f'{len(groups)} groups; refs: {[n for n, _ in refs]}')
    for n, h in runs:
        out_held, out_obj = [], []
        for g in groups:
            for k, out in ((1, out_held), (2, out_obj)):
                r = [x[k][g] for _, x in refs]
                lo, hi, v = min(r), max(r), h[k][g]
                if v < lo - 1 or v > hi + 1:
                    out.append((g, lo, hi, v))
        print(f'{n}: held items outside the game\'s range (+-1): {len(out_held)}; objects: {len(out_obj)}')
        for g, lo, hi, v in sorted(out_held, key=lambda x: -abs(x[3] - (x[1] + x[2]) / 2))[:6]:
            print(f'    held {g}: game {lo}..{hi}, ours {v}')


def counts(save):
    return {i['id']: len([w for w in str(i['woIds']).split(',') if w]) for i in save[4]}, {i['id'] for i in save[4] if i.get('demandGrps')}


def cmd_demand(a):
    st, dem = counts(load(a.start))
    runs = {}
    for n, p in labelled(a.runs):
        c, _ = counts(load(p))
        runs[n] = {i: c.get(i, 0) - st[i] for i in dem}
    names = list(runs)
    print('demand inventories:', len(dem), ' receiving items in some run:', len({i for r in runs.values() for i, v in r.items() if v}))
    print(f'{"":8}' + ''.join(f'{n:>8}' for n in names))
    for x in names:
        print(f'{x:8}' + ''.join(f'{sum(abs(runs[x][i] - runs[y][i]) for i in dem):8}' for y in names))


def cmd_served(a):
    ids = json.load(open(a.ids))
    res = {}
    for n, p in labelled(a.runs):
        cnt = counts(load(p))[0]
        res[n] = {k: ''.join('X' if cnt.get(i, 0) else '.' for i in v) for k, v in ids.items()}
    names = list(res)
    for k in ids:
        print('class', k, 'containers (in the order of the ids file):')
        for n in names:
            print(f'  {n:8} {res[n][k]}  served {res[n][k].count("X")}')
        print('  differing containers between runs:')
        print('   ' + ' '.join(names))
        for x in names:
            print(f'  {x:8}' + ' '.join(f'{sum(p != q for p, q in zip(res[x][k], res[y][k])):>4}' for y in names))


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest='cmd', required=True)
    for name, fn, extra in (('held', cmd_held, ['item']), ('inventories', cmd_inventories, ['item']), ('spread', cmd_spread, []), ('demand', cmd_demand, [])):
        s = sub.add_parser(name)
        s.add_argument('start')
        for e in extra:
            s.add_argument(e)
        s.add_argument('runs', nargs='+', metavar='LABEL=SAVE')
        s.set_defaults(fn=fn)
    s = sub.add_parser('served')
    s.add_argument('ids')
    s.add_argument('runs', nargs='+', metavar='LABEL=SAVE')
    s.set_defaults(fn=cmd_served)
    a = p.parse_args()
    a.fn(a)


if __name__ == '__main__':
    main()
