#!/usr/bin/env python3
"""Reads the game's code as IL when there is no decompiled C# (or the decompiler got a
method wrong): the instructions of a type's methods with their call, field and type
operands named, and a scan for every call site of a method name (an unstable Sort, an
OrderBy: the order the game does things in matters for parity).

    il-dump.py method ASSEMBLY.dll TYPE [METHOD_SUBSTRING]
        every method of the type named TYPE (or those whose name contains METHOD_SUBSTRING):
        offset, opcode, operand (calls as Class::Method, fields, types)
    il-dump.py scan ASSEMBLY.dll [--pattern Sort --pattern OrderBy]
        every method that calls something whose name contains a pattern (default Sort,
        OrderBy): "Namespace.Type::Method -> Callee"; prints the number of sites last

Requires the third-party packages dnfile and dncil (`pip install dnfile dncil`; not part of
this repository's Python, which is standard-library only), and the game's Assembly-CSharp.dll
from its Managed folder (read only, not part of this repository). A pass: `method` prints the
method headers ("===== Namespace Type Method") and instructions; `scan` ends with "N sites".
For text rather than code, dotnet-strings.py needs nothing installed.
"""
import argparse
import bisect


def need():
    try:
        import dnfile
        from dncil.cil.body.reader import read_method_body_from_bytes
        from dncil.clr.token import Token
    except ImportError:
        raise SystemExit('il-dump.py needs: pip install dnfile dncil')
    return dnfile, read_method_body_from_bytes, Token


def body(pe, row, read):
    return read(pe.get_data(row.Rva, 6000))


def member(mt, tok):
    """A call/field/type token as readable text."""
    table, idx = tok >> 24, (tok & 0xFFFFFF) - 1
    try:
        if table == 0x0A:
            r = mt.MemberRef[idx]
            c = r.Class.row
            return f"{getattr(c, 'TypeName', '?')}::{r.Name}"
        if table == 0x2B:
            m = mt.MethodSpec[idx].Method.row
            if hasattr(m, 'Class'):
                return f"{getattr(m.Class.row, 'TypeName', '?')}::{m.Name}"
            return str(m.Name)
        if table == 0x06:
            return str(mt.MethodDef[idx].Name)
        if table == 0x04:
            return str(mt.Field[idx].Name)
        if table == 0x02:
            return str(mt.TypeDef[idx].TypeName)
        if table == 0x01:
            return str(mt.TypeRef[idx].TypeName)
        if table == 0x1B:
            return 'typespec'
    except Exception as e:  # a token the table cannot resolve: say so, keep going
        return f'?{e}'
    return ''


def operand_token(ins):
    op = ins.operand
    if op is None:
        return None
    return op.value if hasattr(op, 'value') else op


def cmd_method(a):
    dnfile, read, Token = need()
    pe = dnfile.dnPE(a.assembly)
    mt = pe.net.mdtables
    types = list(mt.TypeDef)
    for ti, t in enumerate(types):
        if str(t.TypeName) != a.type:
            continue
        end = types[ti + 1].MethodList[0].row_index if ti + 1 < len(types) else len(mt.MethodDef) + 1
        start = t.MethodList[0].row_index if t.MethodList else end
        for mi in range(start, end):
            m = mt.MethodDef[mi - 1]
            if a.method and a.method not in str(m.Name):
                continue
            print('=====', t.TypeNamespace, t.TypeName, m.Name)
            if m.Rva == 0:
                continue
            try:
                b = body(pe, m, read)
            except Exception as e:
                print('ERR', e)
                continue
            for ins in b.instructions:
                tok = operand_token(ins)
                if isinstance(tok, int) and (isinstance(ins.operand, Token) or ins.opcode.name.startswith(
                        ('call', 'ldfld', 'stfld', 'newobj', 'ldsfld', 'stsfld', 'ldftn', 'ldelem', 'stelem', 'box', 'unbox',
                         'constrained', 'ldtoken', 'castclass', 'isinst'))):
                    print(f'  {ins.offset:04x} {ins.opcode.name} tok {tok >> 24:02x}:{tok & 0xFFFFFF} {member(mt, tok)}')
                else:
                    print(f'  {ins.offset:04x} {ins.opcode.name} {ins.operand if ins.operand is not None else ""}')


def cmd_scan(a):
    dnfile, read, _ = need()
    pe = dnfile.dnPE(a.assembly)
    mt = pe.net.mdtables
    types = list(mt.TypeDef)
    starts = [(t.MethodList[0].row_index, i) for i, t in enumerate(types) if t.MethodList]
    keys = [s[0] for s in starts]
    patterns = a.pattern or ['Sort', 'OrderBy']
    hits = 0
    for mi in range(1, len(mt.MethodDef) + 1):
        m = mt.MethodDef[mi - 1]
        if m.Rva == 0:
            continue
        t = types[starts[bisect.bisect_right(keys, mi) - 1][1]]
        try:
            b = read(pe.get_data(m.Rva, 20000))
        except Exception:
            continue
        for ins in b.instructions:
            if ins.opcode.name in ('call', 'callvirt') and ins.operand is not None:
                name = member(mt, operand_token(ins))
                if any(p in name for p in patterns):
                    print(f'{t.TypeNamespace}.{t.TypeName}::{m.Name}  -> {name}')
                    hits += 1
    print(a.assembly.replace('\\', '/').split('/')[-1], hits, 'sites')


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest='cmd', required=True)
    s = sub.add_parser('method')
    s.add_argument('assembly')
    s.add_argument('type')
    s.add_argument('method', nargs='?')
    s.set_defaults(fn=cmd_method)
    s = sub.add_parser('scan')
    s.add_argument('assembly')
    s.add_argument('--pattern', action='append')
    s.set_defaults(fn=cmd_scan)
    a = p.parse_args()
    a.fn(a)


if __name__ == '__main__':
    main()
