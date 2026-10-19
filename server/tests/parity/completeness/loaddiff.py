"""Load fidelity: what a server wrote back of a save it loaded (INPUT -> OUTPUT), without any
personal data: only counts per section, group and field.

    python3 loaddiff.py INPUT.json OUTPUT.json [--json]

Records with ids are matched by id; the others in order. Floats are equal within 1e-3 relative
(savediff.py's rule). Reports, per section: records only in the input (dropped), only in the
output (added), and per (group, field) how many records differ, with one example value shape.
"""
import collections
import json
import re
import sys

SECTIONS = ['state', 'planets', 'players', 'worldObjects', 'inventories', 'stats', 'messages', 'storyEvents',
            'settings', 'instances', 'logistics']


def load(path):
    text = open(path, encoding='utf-8-sig').read()
    return [[json.loads(r) for r in s.split('|') if r.strip()] for s in text.strip().split('@')]


def close(a, b):
    if isinstance(a, (int, float)) and isinstance(b, (int, float)) and not isinstance(a, bool) and not isinstance(b, bool):
        return abs(a - b) <= 1e-3 * max(1.0, abs(a), abs(b))
    if isinstance(a, str) and isinstance(b, str) and ',' in a and re.fullmatch(r'[-0-9.,eE ]+', a + b):
        x, y = a.split(','), b.split(',')
        try:
            return len(x) == len(y) and all(close(float(p), float(q)) for p, q in zip(x, y))
        except ValueError:
            return a == b
    return a == b


def shape(v):
    s = json.dumps(v)
    return s if len(s) < 40 else s[:37] + '...'


def key(section, rec, i):
    if section == 'planets':
        return rec.get('planetId')
    if section in ('messages', 'storyEvents'):
        return rec.get('stringId')
    if section == 'instances':
        return rec.get('seed', i)
    if 'id' in rec:
        return rec['id']
    return i


def compare(a, b):
    out = {}
    for si, name in enumerate(SECTIONS):
        x = a[si] if si < len(a) else []
        y = b[si] if si < len(b) else []
        kx = {key(name, r, i): r for i, r in enumerate(x)}
        ky = {key(name, r, i): r for i, r in enumerate(y)}
        res = {'in': len(x), 'out': len(y), 'dropped': 0, 'added': 0, 'order_changed': False,
               'dropped_by_group': collections.Counter(), 'added_by_group': collections.Counter(), 'field_diffs': collections.Counter(), 'examples': {}}
        res['order_changed'] = [key(name, r, i) for i, r in enumerate(x) if key(name, r, i) in ky] != [key(name, r, i) for i, r in enumerate(y) if key(name, r, i) in kx]
        for k, r in kx.items():
            if k not in ky:
                res['dropped'] += 1
                res['dropped_by_group'][r.get('gId', '-')] += 1
                continue
            o = ky[k]
            g = r.get('gId', '-')
            for f in set(r) | set(o):
                if f in ('id', 'name') and name == 'players':
                    pass
                rv, ov = r.get(f, '<absent>'), o.get(f, '<absent>')
                if name == 'players' and f in ('id', 'name'):
                    if rv != ov:
                        res['field_diffs'][(g, f)] += 1
                    continue
                if not close(rv, ov):
                    res['field_diffs'][(g, f)] += 1
                    res['examples'].setdefault((g, f), (shape(rv), shape(ov)) if name != 'players' else ('-', '-'))
        for k, r in ky.items():
            if k not in kx:
                res['added'] += 1
                res['added_by_group'][r.get('gId', '-')] += 1
        out[name] = res
    return out


def main():
    a, b = load(sys.argv[1]), load(sys.argv[2])
    res = compare(a, b)
    if '--json' in sys.argv:
        print(json.dumps({n: {**{k: v for k, v in r.items() if k not in ('dropped_by_group', 'added_by_group', 'field_diffs', 'examples')},
                              'dropped_by_group': dict(r['dropped_by_group']), 'added_by_group': dict(r['added_by_group']),
                              'field_diffs': {f'{g}.{f}': c for (g, f), c in r['field_diffs'].items()},
                              'examples': {f'{g}.{f}': e for (g, f), e in r['examples'].items()}} for n, r in res.items()}, indent=1))
        return
    for n, r in res.items():
        print(f"{n}: in {r['in']} out {r['out']} dropped {r['dropped']} added {r['added']} order_changed {r['order_changed']}")
        for g, c in r['dropped_by_group'].most_common(40):
            print(f'   dropped {g}: {c}')
        for g, c in r['added_by_group'].most_common(40):
            print(f'   added {g}: {c}')
        for (g, f), c in sorted(r['field_diffs'].items(), key=lambda kv: -kv[1])[:60]:
            print(f'   differs {g}.{f}: {c}   e.g. {r["examples"].get((g, f))}')
    print('sections', len(a), len(b))


if __name__ == '__main__':
    main()
