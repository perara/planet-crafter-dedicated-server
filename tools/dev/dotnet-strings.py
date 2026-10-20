#!/usr/bin/env python3
"""The string literals a .NET assembly carries (the #US "user strings" heap of its metadata),
without a decompiler: to find out what the game's code writes, splits or compares on, for
instance the separators and field names of its save format ("hunger", "grwth", "|\\n", "\\r@"),
or to check a literal exists at all before searching the decompiled code for it.

    dotnet-strings.py list ASSEMBLY.dll [--grep TEXT ...] [--around]
        every user string with its heap offset; --grep keeps those that contain TEXT (any of them);
        --around prints the 4 strings before and after each hit (the literals of one method
        sit next to each other, so the neighbours tell which method it is)
    dotnet-strings.py separators ASSEMBLY.dll
        the one-character and whitespace-only literals (@ | \\n \\r " \\ ...) with their neighbours:
        where the code splits a text into records
    dotnet-strings.py count FILE LITERAL...
        how many times each LITERAL occurs in FILE as UTF-16 text (any binary, no parsing)

ASSEMBLY.dll is the game's Assembly-CSharp.dll (Managed folder of the game); it is read, never
written, and not part of this repository. Python 3 standard library only. A pass: `list` prints
strings (a DLL without the heap or not a .NET assembly says so and exits 1).
"""
import argparse
import re
import struct
import sys


def user_strings(data):
    """(offset, text) for every entry of the #US heap."""
    m = data.find(b'BSJB')
    if m < 0:
        raise SystemExit('not a .NET assembly (no BSJB metadata header)')
    vlen = struct.unpack_from('<I', data, m + 12)[0]
    p = m + 16 + vlen
    _flags, nstreams = struct.unpack_from('<HH', data, p)
    p += 4
    streams = {}
    for _ in range(nstreams):
        off, size = struct.unpack_from('<II', data, p)
        p += 8
        end = data.index(b'\0', p)
        streams[data[p:end].decode()] = (m + off, size)
        p = (end + 4) & ~3
    if '#US' not in streams:
        raise SystemExit('no #US heap in this assembly')
    start, size = streams['#US']
    out = []
    i = start + 1
    while i < start + size:
        b = data[i]
        if b & 0x80 == 0:
            n, h = b, 1
        elif b & 0xC0 == 0x80:
            n, h = ((b & 0x3F) << 8) | data[i + 1], 2
        else:
            n, h = ((b & 0x1F) << 24) | (data[i + 1] << 16) | (data[i + 2] << 8) | data[i + 3], 4
        if n == 0:
            i += 1
            continue
        out.append((i, data[i + h:i + h + n - 1].decode('utf-16-le', 'replace')))
        i += h + n
    return out


def cmd_list(a):
    ents = user_strings(open(a.assembly, 'rb').read())
    for k, (off, s) in enumerate(ents):
        if a.grep and not any(g in s for g in a.grep):
            continue
        print(off, repr(s))
        if a.around:
            print('    around:', [x[1][:40] for x in ents[max(0, k - 4):k + 5]])


def cmd_separators(a):
    ents = user_strings(open(a.assembly, 'rb').read())
    for k, (off, s) in enumerate(ents):
        if len(s) <= 2 and (len(s) == 1 or not s.strip()):
            print(off, repr(s), [x[1][:40] for x in ents[max(0, k - 4):k + 5]])


def cmd_count(a):
    data = open(a.file, 'rb').read()
    for lit in a.literals:
        print(repr(lit), len(re.findall(re.escape(lit.encode('utf-16-le')), data)))


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest='cmd', required=True)
    s = sub.add_parser('list')
    s.add_argument('assembly')
    s.add_argument('--grep', action='append')
    s.add_argument('--around', action='store_true')
    s.set_defaults(fn=cmd_list)
    s = sub.add_parser('separators')
    s.add_argument('assembly')
    s.set_defaults(fn=cmd_separators)
    s = sub.add_parser('count')
    s.add_argument('file')
    s.add_argument('literals', nargs='+')
    s.set_defaults(fn=cmd_count)
    a = p.parse_args()
    a.fn(a)


if __name__ == '__main__':
    main()
