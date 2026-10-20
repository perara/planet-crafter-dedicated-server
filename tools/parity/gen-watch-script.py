#!/usr/bin/env python3
"""Writes a probe script that reads a list of inventories again and again, to watch what
arrives in them over time (drones filling demand inventories, a machine's output): the
inventory listings in the transcript, round after round, for transcript.py series.

    gen-watch-script.py OUT.json --player-id ID --inventories 877,690,696 [--rounds 16] [--wait-ms 15000]

--player-id     the probe player's id: any number the host does not know, not a real account's
--inventories   inventory ids of the start save, comma separated
--rounds        how many times the list is read (default 16); --wait-ms between rounds (default 15000)

Uses sweep.py's script builder (server/tests/parity/sweep.py), so the script starts as every
sweep script does (a scratch inventory, an empty backpack). Run it with sweep.sh like any
script (`sweep.sh both OUT_DIR OUT.json`). A pass: it prints the number of steps and OUT.json
is accepted by `pcds probe`. Python 3 standard library only.
"""
import argparse
import json
import os
import sys


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('out')
    p.add_argument('--player-id', type=int, required=True)
    p.add_argument('--inventories', required=True)
    p.add_argument('--rounds', type=int, default=16)
    p.add_argument('--wait-ms', type=int, default=15000)
    a = p.parse_args()
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..', 'server', 'tests', 'parity'))
    import sweep  # noqa: E402  (the repository's script builder)
    invs = [int(i) for i in a.inventories.split(',')]
    s = sweep.Script('watch', a.player_id)
    sweep.prelude(s)
    for r in range(a.rounds):
        for i in invs:
            sweep.retrieve(s, i, f'watch|{r:02d}|{i}')
        s.wait(a.wait_ms)
    json.dump(s.json(), open(a.out, 'w'))
    print(len(s.steps), 'steps')


if __name__ == '__main__':
    main()
