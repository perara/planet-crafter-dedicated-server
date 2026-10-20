#!/usr/bin/env python3
"""Look into a probe transcript (the JSON lines `pcds probe` prints, one file per host and
script, NAME.ref.jsonl / NAME.ours.jsonl in a sweep run folder) when a parity.py difference
needs a closer look than its one line.

    transcript.py steps FILE LO HI              the messages of script steps LO..HI (1-based, a step
                                                starts at each "call"), without deltas
    transcript.py history FILE VAR [STOP_VAR]   everything that happened to the object the probe
                                                captured as VAR (rpc, delta, create, destroy), up to
                                                the moment STOP_VAR is captured
    transcript.py deconstructs REF OURS         per tag: the objects each host created after a
                                                DeconstructServerRpc (what a deconstruction returned),
                                                the tags where the counts differ marked <<<
    transcript.py series GROUP_HASH LABEL=FILE...   how many items of the group (the game's hash of
                                                its name) an inventory holds over time, as the
                                                inventory listings the probe received show it
                                                (RetrieveInventoryClientRpc), per --round-ms

Python 3 standard library only. A pass is not defined: these print what the transcripts hold.
Examples:  transcript.py steps run/sweep_build_1.ours.jsonl 120 130
           transcript.py history run/x.ours.jsonl o0_0 o1_0
           transcript.py deconstructs run/sweep_use.ref.jsonl run/sweep_use.ours.jsonl
"""
import argparse
import collections
import json
import sys


def lines(path):
    with open(path, encoding='utf-8') as f:
        for raw in f:
            if raw.strip():
                yield json.loads(raw)


def steps(a):
    step = 0
    for m in lines(a.file):
        if m['msg'] == 'call':
            step += 1
        if a.lo <= step <= a.hi and m['msg'] not in ('delta', 'approved object'):
            what = json.dumps(m.get('args', m.get('id', m.get('value', ''))))[:220]
            print(step, m['msg'], m.get('name', ''), what, m.get('object', ''))


def history(a):
    cap = None
    for m in lines(a.file):
        if m['msg'] == 'captured' and m['var'] == a.var:
            cap = m['value']
        if cap and m.get('object') == cap and m['msg'] in ('rpc', 'delta', 'create', 'destroy'):
            print(m['msg'], m.get('name', ''), m.get('behaviour'), m.get('args', ''), (m.get('hex') or '')[:100], m['t'])
        if a.stop_var and m['msg'] == 'captured' and m['var'] == a.stop_var:
            break


def deconstructed(path):
    tag, dec, res = None, False, collections.OrderedDict()
    for m in lines(path):
        if m['msg'] == 'call':
            tag = m.get('tag')
            dec = 'DeconstructServerRpc' in m['name']
            if dec:
                res.setdefault(tag, collections.Counter())
        elif m['msg'] == 'create' and dec and tag:
            res[tag][m.get('hash') or m.get('prefab') or '?'] += 1
    return res


def deconstructs(a):
    ref, ours = deconstructed(a.ref), deconstructed(a.ours)
    for tag in list(ref) + [t for t in ours if t not in ref]:
        n_ref = sum(ref.get(tag, {}).values())
        n_ours = sum(ours.get(tag, {}).values())
        print(f'{tag:28s} ref {n_ref:4d} ours {n_ours:4d}', '' if n_ref == n_ours else '  <<<')
        if n_ref != n_ours or a.all:
            print('    ref ', dict(ref.get(tag, {})))
            print('    ours', dict(ours.get(tag, {})))


def series(a):
    runs = {}
    for spec in a.runs:
        label, path = spec.split('=', 1)
        rows = []
        for m in lines(path):
            if m.get('msg') == 'rpc' and m['name'] == 'InventoriesHandler.RetrieveInventoryClientRpc':
                _size, inv, groups, _wos, *_ = m['args']
                rows.append((m['t'], inv, sum(1 for g in groups if g == a.group), len(groups)))
        runs[label] = rows
    invs = sorted({r[1] for rows in runs.values() for r in rows})
    for label, rows in runs.items():
        print('==', label)
        if not rows:
            continue
        t0 = min(r[0] for r in rows)
        rounds = collections.defaultdict(dict)
        for t, inv, n, total in rows:
            rounds[(t - t0) // a.round_ms][inv] = (n, total)
        print('  t(s) ' + ' '.join(f'{i:>6}' for i in invs))
        for r in sorted(rounds):
            print(f'  {r * a.round_ms // 1000:4d} ' + ' '.join(f'{rounds[r].get(i, ("", ""))[0]!s:>6}' for i in invs))


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    sub = p.add_subparsers(dest='cmd', required=True)
    s = sub.add_parser('steps')
    s.add_argument('file')
    s.add_argument('lo', type=int)
    s.add_argument('hi', type=int)
    s.set_defaults(fn=steps)
    s = sub.add_parser('history')
    s.add_argument('file')
    s.add_argument('var')
    s.add_argument('stop_var', nargs='?')
    s.set_defaults(fn=history)
    s = sub.add_parser('deconstructs')
    s.add_argument('ref')
    s.add_argument('ours')
    s.add_argument('--all', action='store_true', help='also show the tags whose counts agree')
    s.set_defaults(fn=deconstructs)
    s = sub.add_parser('series')
    s.add_argument('group', type=int, help='the group hash (the game\'s string hash of its name; stable-hash.py)')
    s.add_argument('runs', nargs='+', metavar='LABEL=FILE')
    s.add_argument('--round-ms', type=int, default=15000)
    s.set_defaults(fn=series)
    a = p.parse_args()
    a.fn(a)


if __name__ == '__main__':
    main()
