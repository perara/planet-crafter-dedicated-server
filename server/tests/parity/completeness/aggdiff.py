"""What a run changed, by group, on two hosts: the aggregate view of a savediff.

    python3 aggdiff.py START.json REF.json OURS.json [REF2.json]

For each group: objects at the start, at the end of each run, and the change. A change that the
two hosts make differently by more than the game does against itself (REF2, a second run of the
reference) is listed. Also per planet: the terraformation units; and per holder group: the items
in the inventories of its machines.
"""
import collections
import json
import sys


def load(path):
    text = open(path, encoding='utf-8-sig').read()
    return [[json.loads(r) for r in s.split('|') if r.strip()] for s in text.strip().split('@')]


PLANETS = {-1140328421: 'Prime', -486276833: 'Humble', -1016990411: 'Selenea', -1291310150: 'Aqualis', 110910045: 'Toxicity'}


def counts(save):
    """(objects per group, items per (planet of the holder, item group))"""
    by_group = collections.Counter(o['gId'] for o in save[3])
    wo = {o['id']: o for o in save[3]}
    owner = {o['liId']: o for o in save[3] if o.get('liId')}
    for o in save[3]:
        for i in str(o.get('siIds', '')).split(','):
            if i:
                owner.setdefault(int(i), o)
    held = collections.Counter()
    for inv in save[4]:
        h = owner.get(inv['id'])
        planet = PLANETS.get(h.get('planet'), '?') if h else 'players/unowned'
        for w in str(inv['woIds']).split(','):
            if w and int(w) in wo:
                held[(planet, wo[int(w)]['gId'])] += 1
    return by_group, held


def main():
    start, ref, ours = (load(p) for p in sys.argv[1:4])
    ref2 = load(sys.argv[4]) if len(sys.argv) > 4 else None
    s, _ = counts(start)
    r, rh = counts(ref)
    o, oh = counts(ours)
    r2 = counts(ref2)[0] if ref2 else None
    groups = sorted(set(s) | set(r) | set(o))
    print(f'{"group":34} {"start":>7} {"ref":>7} {"ours":>7} {"d ref":>7} {"d ours":>7} {"diff":>6} {"noise":>6}')
    rows = []
    for g in groups:
        dr, do = r[g] - s[g], o[g] - s[g]
        noise = abs(r2[g] - r[g]) if r2 else 0
        rows.append((abs(dr - do), g, s[g], r[g], o[g], dr, do, noise))
    n_flag = 0
    for diff, g, a, b, c, dr, do, noise in sorted(rows, reverse=True):
        if diff == 0:
            continue
        flag = diff > max(2 * noise, 2)
        n_flag += flag
        print(f'{g:34} {a:7} {b:7} {c:7} {dr:+7} {do:+7} {diff:6} {noise:6} {"<-- beyond the game\'s own spread" if flag else ""}')
    print(f'groups whose change differs: {len([x for x in rows if x[0]])}, beyond the game\'s own spread: {n_flag}')
    print('--- planets: terraformation units (change over the run, ref / ours)')
    for p, q, pr in zip(start[1], ref[1], ours[1]):
        for k in p:
            if k.startswith('unit'):
                print(f'{p["planetId"]:10} {k:24} {q[k]-p[k]:+.4g} {pr[k]-p[k]:+.4g}')
    print('--- items in inventories, by the holder\'s planet: ref / ours, where they differ by more than 3')
    sh, _ = counts(start)[1], None
    keys = set(rh) | set(oh)
    st = counts(start)[1]
    n = 0
    for k in sorted(keys, key=lambda k: -abs((rh[k] - st[k]) - (oh[k] - st[k]))):
        d = abs(rh[k] - oh[k])
        if d > 3 and n < 60:
            print(f'{k[0]:18} {k[1]:24} start {st[k]:5} ref {rh[k]:5} ours {oh[k]:5}')
            n += 1


if __name__ == '__main__':
    main()
