"""Group the differences parity.py found into kinds, and say which the game also makes against itself.

    python3 classify.py RUN.diff [RUN.noise] [--top N] [--examples]

A kind is a difference line with its numbers, places and ids blanked ("N"). For each kind: how many
lines, how many the same kind has in RUN.noise (two runs of the real game against each other: its
own randomness and timing, class d), and whether sweep_classes.json lists it (class b, c, e: a deliberate
deviation). What is left is the kinds nobody has explained.
"""
import collections
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
RULES = json.load(open(os.path.join(os.path.dirname(HERE), 'sweep_classes.json')))


def kind(line):
    mark = line[:1]
    body = line[2:] if line[:2] in ('+ ', '- ', '~ ') else line
    body = re.sub(r'@-?[0-9.]+,-?[0-9.]+,-?[0-9.]+', '@P', body)
    body = re.sub(r'new:[A-Za-z]+#\d+', 'new:X#N', body)
    body = re.sub(r'WO@\d+\.\d+', 'WO@N', body)
    body = re.sub(r'\{.*', '{...}', body) if mark in '+-' else body
    body = re.sub(r'\(writes ref \d+, ours \d+\)', '(writes N)', body)
    body = re.sub(r'ref .*? ours .*', 'ref X ours Y', body)
    body = re.sub(r'-?\d+(\.\d+)?(E[+-]?\d+)?', 'N', body)
    return mark + ' ' + body[:150]


def lines(path):
    if not path or not os.path.exists(path):
        return []
    return [l.rstrip('\n') for l in open(path, errors='replace') if l[:1] in '+-~' and not l.startswith('---')]


def rule_class(line):
    for r in RULES:
        pat = r.get('line')
        if pat and r['class'] in 'bce' and re.search(pat, line) and not r.get('script') and not r.get('tag'):
            return r['class']
    return ''


def main():
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    top = int(sys.argv[sys.argv.index('--top') + 1]) if '--top' in sys.argv else 40
    diff, noise = lines(args[0]), lines(args[1]) if len(args) > 1 else []
    k = collections.Counter(kind(l) for l in diff)
    n = collections.Counter(kind(l) for l in noise)
    ex = {}
    for l in diff:
        ex.setdefault(kind(l), l)
    total = Counter_total = sum(k.values())
    classes = collections.Counter()
    unexplained = []
    for sig, c in k.most_common():
        cls = 'd' if n[sig] * 2 >= c else rule_class(ex[sig])
        classes[cls or '-'] += c
        if not cls:
            unexplained.append((sig, c))
    print(f'{total} difference lines, {len(k)} kinds; by class (d: the game does the same against itself, b/c/e: sweep_classes.json): {dict(classes)}')
    print(f'{len(unexplained)} kinds, {sum(c for _, c in unexplained)} lines unexplained; the largest:')
    for sig, c in unexplained[:top]:
        print(f'{c:6}  noise {n[sig]:5}  {sig}')
        if '--examples' in sys.argv:
            print('        e.g.', ex[sig][:300])


if __name__ == '__main__':
    main()
