#!/usr/bin/env python3
"""Prints the header facts of Unity SerializedFiles (the game's *.assets, level*, sharedassets*
files): format version, the Unity version string that wrote them, target platform and whether
type trees are stored. What the table reader (`pcds read-game`) must handle depends on these:
run it on the files of a new game version before it reads them, to see whether the format moved.

    unity-header.py FILE...        one line per file:  name version unity-version platform type-tree

Version 22 and later use 64-bit sizes and a header of 48 bytes before the metadata; the older
formats 20. A file that is not a SerializedFile prints "ERR ...". A pass: the version and Unity
version strings print for every file of the game (a type-tree value of 0 means the file stores
no type trees: its objects can only be read with the type trees of another file or the game's
own assemblies). Python 3 standard library only.
"""
import argparse
import pathlib
import struct


def info(path):
    b = pathlib.Path(path).read_bytes()[:4096]
    version = struct.unpack('>I', b[8:12])[0]
    endian = b[16]
    off = 48 if version >= 22 else 20
    fmt = '<' if endian == 0 else '>'
    end = b.index(b'\0', off)
    unity = b[off:end].decode()
    platform = struct.unpack(fmt + 'i', b[end + 1:end + 5])[0]
    return version, unity, platform, b[end + 5]


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('files', nargs='+')
    for path in p.parse_args().files:
        try:
            print(pathlib.Path(path).name, *info(path))
        except Exception as e:  # not a SerializedFile, or too short: say so and go on
            print(pathlib.Path(path).name, 'ERR', e)


if __name__ == '__main__':
    main()
