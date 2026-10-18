"""Compare two Planet Crafter saves field by field, order included: the real host's and ours.

    python3 savediff.py START.json REF.json OURS.json

A save is sections joined by "@", each a "|"-joined list of JSON records. Records with ids are
compared by id, and the order of the ids on its own; others in order. World object ids the
servers allocated (not in START.json) are paired: the same id, else group and position, else
group in order. The reference's host player (the hidden avatar
the plugin hosts with) and the game's per-run counters are left out.
"""
import difflib
import json
import re
import sys

SECTIONS = ['state', 'planets', 'players', 'worldObjects', 'inventories', 'stats', 'messages', 'storyEvents',
            'settings', 'instances', 'logistics']


def load(path):
    text = open(path, encoding='utf-8-sig').read()
    return [[json.loads(r) for r in s.split('|') if r.strip()] for s in text.strip().split('@')]


def close(a, b):
    if isinstance(a, float) or isinstance(b, float):
        try:
            return abs(float(a) - float(b)) <= 1e-3 * max(1.0, abs(float(a)), abs(float(b)))
        except (TypeError, ValueError):
            return a == b
    if isinstance(a, str) and isinstance(b, str) and ',' in a and re.fullmatch(r'[-0-9.,E ]+', a + b):
        x, y = a.split(','), b.split(',')
        return len(x) == len(y) and all(close(float(p), float(q)) for p, q in zip(x, y))
    return a == b


def main():
    start, ref, ours = (load(p) for p in sys.argv[1:4])
    known = {int(x) for x in re.findall(r'"id":(\d+)', open(sys.argv[1], encoding='utf-8-sig').read())}
    # Pair the ids each server allocated: same group and position, else order of appearance.
    def new_objects(save):
        return [o for o in save[3] if o.get('id') not in known]
    pairs = {}
    rn, on = new_objects(ref), new_objects(ours)
    for r in rn:  # the same id on both (a scene object first saved here)
        if any(o['id'] == r['id'] for o in on):
            pairs[r['id']] = r['id']
    for r in rn:
        if r['id'] in pairs:
            continue
        m = next((o for o in on if o['gId'] == r['gId'] and close(o.get('pos'), r.get('pos')) and o['id'] not in pairs.values()), None)
        if m:
            pairs[r['id']] = m['id']
    rest = [o['id'] for o in on if o['id'] not in pairs.values()]
    for r in rn:  # then by group, in order
        m = next((i for i in rest if any(o['id'] == i and o['gId'] == r['gId'] for o in on)), None) if r['id'] not in pairs else None
        if m is not None:
            pairs[r['id']] = m
            rest.remove(m)
    for r in rn:
        if r['id'] not in pairs and rest:
            pairs[r['id']] = rest.pop(0)
    rename = {v: f'NEW#{i}' for i, v in enumerate(pairs.values(), 1)} | {k: f'NEW#{i}' for i, k in enumerate(pairs, 1)}

    def norm(v):
        if isinstance(v, int) and not isinstance(v, bool):
            return rename.get(v, v)
        if isinstance(v, str) and re.fullmatch(r'[0-9,]+', v):
            return ','.join(str(rename.get(int(x), x)) for x in v.split(',') if x)
        return v

    host = next((p.get('name') for p in start[2] if p.get('host')), None)
    n = 0
    for i, name in enumerate(SECTIONS):
        a = ref[i] if i < len(ref) else []
        b = ours[i] if i < len(ours) else []
        if name == 'players':
            a = [p for p in a if p.get('name') not in (host, 'Extractor')]
            b = [p for p in b if p.get('name') not in (host, 'Extractor')]
        if name == 'stats':
            continue
        if len(a) != len(b):
            print(f'{name}: {len(a)} records in ref, {len(b)} in ours')
            n += 1
        if a and all('id' in x for x in a + b):
            # Records with ids: the order of ids, then each record against the same id's.
            ia, ib = [str(norm(x['id'])) for x in a], [str(norm(x['id'])) for x in b]
            for op, i1, i2, j1, j2 in difflib.SequenceMatcher(None, ia, ib, autojunk=False).get_opcodes():
                if op != 'equal':
                    print(f'{name} order at ref {i1} / ours {j1}: ref {ia[i1:i2][:8]} ours {ib[j1:j2][:8]}')
                    n += 1
            by_id = {str(norm(y['id'])): y for y in b}
            b = [by_id.get(str(norm(x['id'])), {}) for x in a]
        for j, (x, y) in enumerate(zip(a, b)):
            if not y:
                continue
            x, y = {k: norm(v) for k, v in x.items()}, {k: norm(v) for k, v in y.items()}
            if list(x) != list(y):
                print(f'{name}[{j}] field order: ref {list(x)} ours {list(y)}')
                n += 1
            for k in x:
                if k in y and not close(x[k], y[k]):
                    print(f'{name}[{j}] ({x.get("gId") or x.get("name") or x.get("id")}).{k}: ref {json.dumps(x[k])[:300]} ours {json.dumps(y[k])[:300]}')
                    n += 1

    if len(ref) != len(ours):
        print(f'sections: ref {len(ref)}, ours {len(ours)}')
        n += 1
    print(f'{n} save difference(s)')
    sys.exit(1 if n else 0)


if __name__ == '__main__':
    main()
