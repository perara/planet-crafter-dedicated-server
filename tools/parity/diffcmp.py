#!/usr/bin/env python3
"""Compares the parity.py results of two runs of the same sweep (two builds of this server, or
a replay against the stored one): for each script, the difference lines only one of the runs
has. Tells at once whether a change made anything new differ, without reading 30 diffs.

    diffcmp.py RUN_A RUN_B [SHOWN]

RUN_A, RUN_B   folders holding NAME.diff files (sweep.sh / replay-ours.sh results; a replay's
               worker folders may be given as several folders joined with a comma)
SHOWN          how many such lines to print per script and side (default 0: only the counts)

Output: one line per script that differs, then a summary. A pass: "differ: 0" in the summary
(the two runs have the same difference lines everywhere; the "N difference(s)" total line is
left out, so only the contents count). Python 3 standard library only.
"""
import argparse
import collections
import glob
import os


def runs(spec):
    found = {}
    for folder in spec.split(','):
        for f in glob.glob(os.path.join(folder, '*.diff')):
            found[os.path.basename(f)[:-5]] = f
    return found


def body(path):
    with open(path, errors='replace') as f:
        return [line.rstrip('\n') for line in f if line.strip() and not line.endswith('difference(s)\n')]


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('run_a')
    p.add_argument('run_b')
    p.add_argument('shown', nargs='?', type=int, default=0)
    a = p.parse_args()
    ra, rb = runs(a.run_a), runs(a.run_b)
    total = collections.Counter()
    for name in sorted(set(ra) & set(rb)):
        la, lb = body(ra[name]), body(rb[name])
        ca, cb = collections.Counter(la), collections.Counter(lb)
        only_a, only_b = ca - cb, cb - ca
        total['scripts'] += 1
        if only_a or only_b:
            total['differ'] += 1
            print(f'{name}: A {len(la)} lines, B {len(lb)}; only A: {sum(only_a.values())}, only B: {sum(only_b.values())}')
            for line in list(only_b)[:a.shown]:
                print('   only B:', line[:230])
            for line in list(only_a)[:a.shown]:
                print('   only A:', line[:230])
    print(f"scripts: {total['scripts']}, differ: {total['differ']}", '| only in A:', sorted(set(ra) - set(rb)), '| only in B:', sorted(set(rb) - set(ra)))


if __name__ == '__main__':
    main()
