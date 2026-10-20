#!/usr/bin/env python3
"""The game's string hash (its "stable" hash of a name: the group ids, planet ids and
inventory hashes in the save and on the wire), printed as the signed 32-bit number the game
uses. The same function as `stable_hash` in server/src/data.rs, for a shell.

    stable-hash.py NAME...        one line per name:  HASH  NAME

Examples:  stable-hash.py Prime        ->  -1140328421  Prime  (a planet's id in the save)
A pass: the hash of "Prime" is -1140328421. Python 3 standard library only.
"""
import argparse


def i32(x):
    return ((x + 2**31) % 2**32) - 2**31


def stable_hash(s):
    u = [ord(c) for c in s]
    h1 = h2 = 5381
    i = 0
    while i < len(u) and u[i] != 0:
        h1 = i32((i32(h1 << 5) + h1) ^ u[i])
        if i == len(u) - 1 or u[i + 1] == 0:
            break
        h2 = i32((i32(h2 << 5) + h2) ^ u[i + 1])
        i += 2
    return i32(h1 + i32(h2 * 1566083941))


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('names', nargs='+')
    for name in p.parse_args().names:
        print(stable_hash(name), name)


if __name__ == '__main__':
    main()
