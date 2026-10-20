#!/usr/bin/env python3
"""Resolves git merge conflicts in server/src/coverage.txt (the test-coverage map): two lanes
each added tests to the same `rpc` / `module` lines, and the right merge of such a line is
the union of both lines' tests. Every conflict hunk is replaced by the lines of both sides
merged by their key (the first two words: kind and name); the tests of the same key are
united (in the order they first appear), a trailing "  # comment" is kept (ours first).

    merge-coverage.py server/src/coverage.txt        (rewrites the file in place)

Run it on a file with conflict markers (<<<<<<< ======= >>>>>>>) after `git merge` stopped;
then `git add` it and run `cargo test coverage::`, which checks every RPC and module is in
the map. Lines outside conflict hunks are untouched. A pass: no marker is left in the file and
the coverage test passes. Python 3 standard library only.
"""
import argparse


def key(line):
    w = line.split(None, 2)
    return (w[0], w[1]) if len(w) > 1 else (line,)


def merge(ours, theirs):
    res, order = {}, []
    for line in ours + theirs:
        k = key(line)
        if k not in res:
            res[k] = line
            order.append(k)
            continue
        head, _, comment = res[k].partition('  #')
        head2, _, comment2 = line.partition('  #')
        tokens = head.split()
        tokens += [t for t in head2.split() if t not in tokens]
        keep = comment or comment2
        res[k] = ' '.join(tokens) + (('  #' + keep) if keep else '')
    return [res[k] for k in order]


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('file')
    path = p.parse_args().file
    out, ours, theirs, mode = [], [], [], 0
    for line in open(path).read().split('\n'):
        if line.startswith('<<<<<<<'):
            mode, ours, theirs = 1, [], []
        elif line.startswith('=======') and mode == 1:
            mode = 2
        elif line.startswith('>>>>>>>') and mode == 2:
            out += merge(ours, theirs)
            mode = 0
        elif mode == 1:
            ours.append(line)
        elif mode == 2:
            theirs.append(line)
        else:
            out.append(line)
    open(path, 'w').write('\n'.join(out))


if __name__ == '__main__':
    main()
