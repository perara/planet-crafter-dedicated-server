"""Compare two probe transcripts (pcds probe output, JSON lines): the real game's and the
standalone server's. Messages are matched in order per kind; timing, our own player's object id
and values listed with --ignore are left out.

Usage: python3 tools/probe_diff.py REAL.jsonl OURS.jsonl [--loose] [--ignore KEY ...]
--loose masks values that differ between servers by nature: world object ids (>= 200000000),
spawned object ids, and the probe's captured values.
"""
import difflib
import json
import sys


def mask(v, ids):
    if isinstance(v, bool):
        return v
    if isinstance(v, int) and (200_000_000 <= v < 300_000_000 or v in ids):
        return "ID"
    if isinstance(v, list):
        return [mask(x, ids) for x in v]
    if isinstance(v, dict):
        return {k: mask(x, ids) for k, x in v.items()}
    return v


def load(path, ignore, loose=False):
    lines = [json.loads(line) for line in open(path) if line.strip()]
    if loose:
        ids = {l["id"] for l in lines if l["msg"] == "create"} | {l["value"] for l in lines if l["msg"] == "captured" and isinstance(l["value"], int)}
        lines = [mask(l, ids) for l in lines]
    player = next((l["player"] for l in lines if l["msg"] == "joined"), None)
    out = []
    for line in lines:
        if line["msg"] in ("joined", "approved"):
            continue
        line.pop("t", None)
        for key in ignore:
            line.pop(key, None)
        if line.get("object") == player:
            line["object"] = "$player"
        out.append(line)
    return out


def main():
    real_path, ours_path, *rest = sys.argv[1:]
    loose = "--loose" in rest
    rest = [r for r in rest if r != "--loose"]
    ignore = rest[1:] if rest[:1] == ["--ignore"] else []
    real, ours = load(real_path, ignore, loose), load(ours_path, ignore, loose)
    a, b = [json.dumps(m, sort_keys=True) for m in real], [json.dumps(m, sort_keys=True) for m in ours]
    differences = 0
    for op, i1, i2, j1, j2 in difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes():
        if op == "equal":
            continue
        differences += max(i2 - i1, j2 - j1)
        for line in a[i1:i2]:
            print(f"- real: {line}")
        for line in b[j1:j2]:
            print(f"+ ours: {line}")
    print(f"{differences} difference(s); real {len(real)} messages, ours {len(ours)}")
    sys.exit(1 if differences else 0)


if __name__ == "__main__":
    main()
