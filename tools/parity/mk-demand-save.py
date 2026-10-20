#!/usr/bin/env python3
"""Makes a start save for the drones' order of service: empty containers turned into demand
inventories of one item each, two classes at different priorities, next to the depots that
hold only a few of that item. Whichever demand the drones serve first gets the item, so where
the items end up shows the order the game serves them in (a sort that is not stable, a tie
broken by the order of a Dictionary): run it on the game and on this server and compare with
runs-compare.py demand / served.

    mk-demand-save.py START.json OUT.json --depot INV_ID [--depot INV_ID ...]
                      [--class-a ITEM:PRIORITY] [--class-b ITEM:PRIORITY] [--count N] [--keep K]

START.json    a late-game save (not shipped) with PlanetaryDeliveryDepot1 objects on the first
              planet and empty plain containers (Container3, Counter1, Counter2) near them
--depot       inventory ids of depots; the first one is the one the containers are picked around
              (the nearest ones, taken alternately for class A and B); each depot keeps K of its items
--class-a/-b  the item and the priority of each class (defaults BalzarQuartz:9 and PulsarQuartz:0:
              A alone on top, B among the save's other demands at 0)
--count       containers per class (default 17); --keep items left in each depot (default 8, so
              that only 8 of the 2 x 17 demands can be served)

Writes OUT.json (the save) and OUT.json.ids ({"A": [inventory ids], "B": [...]}, for
runs-compare.py served). A pass: it prints how many empty containers it found and how far the
farthest chosen one is from the first depot; the save then loads in `pcds serve`. Python 3 standard
library only; start saves are the user's own data and stay out of the repository.
"""
import argparse
import json
import math
import re

PRIME = -1140328421  # the first planet's id in the save (the game's hash of its name)


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('start')
    p.add_argument('out')
    p.add_argument('--depot', type=int, action='append', required=True)
    p.add_argument('--class-a', default='BalzarQuartz:9')
    p.add_argument('--class-b', default='PulsarQuartz:0')
    p.add_argument('--count', type=int, default=17)
    p.add_argument('--keep', type=int, default=8)
    a = p.parse_args()

    sec = open(a.start, encoding='utf-8-sig', newline='').read().split('@')
    objs = [json.loads(r) for r in sec[3].split('|') if r.strip()]
    invs = [json.loads(r) for r in sec[4].split('|') if r.strip()]
    byid = {i['id']: i for i in invs}
    depot = {o['liId']: o for o in objs if o['gId'] == 'PlanetaryDeliveryDepot1' and o.get('planet') == PRIME}

    def pos(o):
        return tuple(float(v) for v in o['pos'].split(','))

    conts = [o for o in objs if o['gId'] in ('Container3', 'Counter1', 'Counter2') and o.get('planet') == PRIME and o.get('liId') in byid
             and not byid[o['liId']]['woIds'] and not byid[o['liId']].get('demandGrps') and not byid[o['liId']].get('supplyGrps')]
    print(len(conts), 'empty plain containers on the planet')
    d0 = depot[a.depot[0]]
    order = sorted(conts, key=lambda c: math.dist(pos(c), pos(d0)))[:2 * a.count]
    chosen = order[0::2] + order[1::2]
    classes = [c.split(':') for c in (a.class_a, a.class_b)]
    for k, (item, prio) in enumerate(classes):
        for c in chosen[k * a.count:(k + 1) * a.count]:
            byid[c['liId']].update(size=1, demandGrps=item, supplyGrps='', priority=int(prio))
    gone = set()
    for inv_id in a.depot:
        ws = [w for w in byid[inv_id]['woIds'].split(',') if w]
        byid[inv_id]['woIds'] = ','.join(ws[:a.keep])
        gone.update(int(w) for w in ws[a.keep:])
    objs = [o for o in objs if o['id'] not in gone]
    print('farthest', round(math.dist(pos(order[-1]), pos(d0))), 'm from the first depot')
    for k, rows in ((3, objs), (4, invs)):
        pre, _, post = re.match(r'(\s*)(.*?)(\s*)$', sec[k], re.S).groups()
        sec[k] = pre + '|\n'.join(json.dumps(r, separators=(',', ':')) for r in rows) + post
    open(a.out, 'w', encoding='utf-8-sig', newline='').write('@'.join(sec))
    json.dump({'A': [c['liId'] for c in chosen[:a.count]], 'B': [c['liId'] for c in chosen[a.count:]]}, open(a.out + '.ids', 'w'))


if __name__ == '__main__':
    main()
