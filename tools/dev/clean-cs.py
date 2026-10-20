#!/usr/bin/env python3
"""Makes decompiled game code readable: strips the Netcode-generated RPC plumbing from the
decompiled C# (the __rpc_handler_N methods, InitializeRPCS, the send-stage block at the top of
every RPC method, [ClientRpc]/[ServerRpc]/[DebuggerHidden]/[CompilerGenerated] attributes and
empty lines), so what is left is the game logic: the part the server ports (parity work reads
the original and the port side by side).

    clean-cs.py DECOMPILED_DIR OUT_DIR

DECOMPILED_DIR  *.cs files, one per class, from a decompiler (ILSpy, dnSpy ...) run on the game's
                Assembly-CSharp.dll; not part of this repository (the game's code is not ours)
OUT_DIR         the cleaned copies, same names (created)

A pass: OUT_DIR holds one file per input and "grep -c __rpc_handler" over it prints 0. Python 3
standard library only.
"""
import argparse
import pathlib
import re

PLUMBING = re.compile(r"\b(__rpc_handler_\d+|InitializeRPCS|__initializeVariables|__initializeRpcs|__getTypeName)\(")
ATTRIBUTES = re.compile(r"\s*\[(DebuggerHidden|CompilerGenerated|Rpc\(|ClientRpc|ServerRpc|MethodImpl)")


def clean(lines):
    out, i = [], 0
    while i < len(lines):
        line = lines[i]
        # Generated handlers and registration: drop whole methods.
        if PLUMBING.search(line) and not line.strip().startswith('//'):
            j, depth, started = i, 0, False
            while j < len(lines):
                depth += lines[j].count('{') - lines[j].count('}')
                started = started or '{' in lines[j]
                j += 1
                if started and depth == 0:
                    break
            i = j
            continue
        # RPC send stage: from the NetworkManager guard to the exec-stage switch.
        if line.strip() == 'NetworkManager networkManager = base.NetworkManager;':
            j = i
            while j < len(lines) and '__rpc_exec_stage = __RpcExecStage.Send;' not in lines[j]:
                j += 1
            if j < len(lines):
                i = j + 1
                continue
        if ATTRIBUTES.match(line):
            i += 1
            continue
        out.append(line)
        i += 1
    return [x for x in out if x.strip()]


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('src', type=pathlib.Path)
    p.add_argument('dst', type=pathlib.Path)
    a = p.parse_args()
    a.dst.mkdir(parents=True, exist_ok=True)
    for path in a.src.glob('*.cs'):
        lines = path.read_text(encoding='utf-8', errors='replace').splitlines()
        (a.dst / path.name).write_text('\n'.join(clean(lines)) + '\n')


if __name__ == '__main__':
    main()
