#!/usr/bin/env python3
"""Compares two bench dumps (PCDS_BENCH_DUMP: every message the server sent, then the final
save, server/src/bench.rs): did a change alter what the server sends or saves, and where.

    dumpcmp.py A.txt B.txt [SHOWN]

Messages: how many each dump holds, which lines only one has (as a multiset), how many
positions differ, and the message kinds of the lines that differ. Save: the entries (world
objects, inventories, planets ...) whose fields differ, with the fields; SHOWN (default 6) entries.
A pass: both dumps equal ("only in first: 0 ... only in second: 0", "entries differing 0"). A
difference in order only shows as "positions that differ" with equal multisets (the bench sorts
a tick's messages, so that is a real change). Python 3 standard library only.
"""
import argparse
import collections
import json
import re


def split(path):
    text = open(path, encoding='utf-8', newline='').read()
    i = text.find('\nsave ')
    if i < 0:
        return text, ''
    return text[:i], text[i + 6:].lstrip('﻿')


def messages(text):
    return [line for line in text.split('\n') if line and not line.startswith('join ')]


def kind(line):
    m = re.match(r'(\S+) (\d+) \[(.*?)\]', line)
    return (m.group(1), m.group(2)) if m else ('?',)


def entries(save):
    out = []
    for section in save.split('\r@'):
        for e in section.split('|\n'):
            e = e.strip('\r\n')
            if e:
                try:
                    out.append(json.loads(e))
                except ValueError:
                    out.append(e)
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('a')
    p.add_argument('b')
    p.add_argument('shown', nargs='?', type=int, default=6)
    args = p.parse_args()
    (ma, sa), (mb, sb) = split(args.a), split(args.b)
    a, b = messages(ma), messages(mb)
    print('messages', len(a), len(b))
    ca, cb = collections.Counter(a), collections.Counter(b)
    only_a, only_b = ca - cb, cb - ca
    print('only in first (multiset):', sum(only_a.values()), 'only in second:', sum(only_b.values()))
    print('positions that differ:', sum(1 for x, y in zip(a, b) if x != y))
    print('kinds only in first:', collections.Counter(kind(x) for x in only_a.elements()).most_common(5))
    print('kinds only in second:', collections.Counter(kind(x) for x in only_b.elements()).most_common(5))
    for x in list(only_a)[:2]:
        print(' A', x[:200])
    for x in list(only_b)[:2]:
        print(' B', x[:200])
    ea, eb = entries(sa), entries(sb)
    print('save entries', len(ea), len(eb))
    n = 0
    for x, y in zip(ea, eb):
        if x == y:
            continue
        n += 1
        if n <= args.shown:
            if isinstance(x, dict) and isinstance(y, dict):
                d = {k: (x.get(k), y.get(k)) for k in set(x) | set(y) if x.get(k) != y.get(k)}
                print(x.get('id', x.get('planetId', '?')), dict(list(d.items())[:6]))
            else:
                print('diff', str(x)[:100], str(y)[:100])
    print('entries differing', n)


if __name__ == '__main__':
    main()
