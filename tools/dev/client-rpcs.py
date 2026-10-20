#!/usr/bin/env python3
"""Lists every RPC the game's host sends to its clients (the methods marked [ClientRpc] or
[Rpc(SendTo.ClientsAndHost|Everyone|NotServer|Owner|NotOwner|SpecifiedInParams)]) with the
code they run on arrival: the messages a server must send for the clients to behave as in
the game. The census behind docs/COMPLETENESS.md's client-RPC counts is made from this kind
of list.

    client-rpcs.py DECOMPILED_DIR > client_rpcs.txt

DECOMPILED_DIR  *.cs files, one per class, from a decompiler run on the game's Assembly-CSharp.dll
                (not part of this repository). Output on stdout: "### File.cs :: signature
                attribute" and the method's body after the generated send stage; the count of
                RPCs goes to stderr ("TOTAL N"). A pass: TOTAL is the number of client RPCs of
                that game version (84 at the version COMPLETENESS.md was made at).
Python 3 standard library only.
"""
import argparse
import pathlib
import re
import sys

ATTR = re.compile(r'^\s*\[(ClientRpc[^\]]*|Rpc\(SendTo\.(ClientsAndHost|Everyone|NotServer|Owner|NotOwner|SpecifiedInParams)[^\]]*)\]\s*$')
SEND = '__rpc_exec_stage = __RpcExecStage.Send;'


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('root', type=pathlib.Path)
    root = p.parse_args().root
    out = []
    for f in sorted(root.glob('*.cs')):
        lines = f.read_text(encoding='utf-8', errors='replace').splitlines()
        for i, line in enumerate(lines):
            if not ATTR.match(line):
                continue
            sig = lines[i + 1].strip()
            depth, j, body, started = 0, i + 1, [], False
            while j < len(lines):
                depth += lines[j].count('{') - lines[j].count('}')
                started = started or '{' in lines[j]
                body.append(lines[j])
                if started and depth == 0:
                    break
                j += 1
            text = '\n'.join(body)
            k = text.rfind(SEND)
            exec_body = text[k + len(SEND):] if k >= 0 else text
            exec_body = '\n'.join(x.strip() for x in exec_body.splitlines() if x.strip() not in ('', '{', '}'))
            out.append((f.name, sig, line.strip(), exec_body))
    for name, sig, attr, body in out:
        print(f'### {name} :: {sig}  {attr}\n{body}\n')
    print(f'TOTAL {len(out)}', file=sys.stderr)


if __name__ == '__main__':
    main()
