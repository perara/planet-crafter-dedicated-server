#!/usr/bin/env python3
"""Grows a save to about N world objects for the flip benchmark (bench::flipbench).

    tools/grow-world.py SAVE.json OUT.json N

The world objects, inventories and their links of SAVE are copied (ids renumbered, so every
copy is its own base: containers with their items, machines, generators, drops) until the
world has N world objects. The players stay in the first copy only. The result is a plain
save: the server loads it as any other, and what the bench measures is the state it builds.
"""
import json, sys

src, out, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
text = open(src, encoding="utf-8-sig", newline="").read()
sections = [s.strip("\r") for s in text.split("\r@") if s.strip("\r\n")]
assert len(sections) == 10, len(sections)
lines = lambda s: [json.loads(x) for x in s.split("|\n") if x.strip()]
wos, invs = lines(sections[3]), lines(sections[4])
ids = lambda s: [int(x) for x in s.split(",")] if s else []
join = lambda l: ",".join(map(str, l))

next_wo, next_inv = 300_000_000, 100_000
copies = max(1, round(n / len(wos)))
new_wos, new_invs = [], []
for c in range(copies):
    wo_map, inv_map = {}, {}
    for w in wos:
        wo_map[w["id"]] = next_wo
        next_wo += 1
    for i in invs:
        # The first copy keeps the players' own inventories (ids 1..4 are special) as they are.
        inv_map[i["id"]] = i["id"] if c == 0 else next_inv
        next_inv += 0 if c == 0 else 1
    for w in wos:
        w = dict(w)
        w["id"] = wo_map[w["id"]] if c else w["id"]
        if c and "liId" in w:
            w["liId"] = inv_map.get(w["liId"], w["liId"])
        if c and w.get("siIds"):
            w["siIds"] = join(inv_map.get(x, x) for x in ids(w["siIds"]))
        if c and w.get("linkedWo"):
            w["linkedWo"] = wo_map.get(w["linkedWo"], w["linkedWo"])
        new_wos.append(w)
    for i in invs:
        i = dict(i)
        i["id"] = inv_map[i["id"]]
        if c:
            i["woIds"] = join(wo_map.get(x, x) for x in ids(i["woIds"]))
        new_invs.append(i)
sections[3] = "|\n".join(json.dumps(w, separators=(",", ":")) for w in new_wos)
sections[4] = "|\n".join(json.dumps(i, separators=(",", ":")) for i in new_invs)
open(out, "w", encoding="utf-8", newline="").write("﻿" + "".join("\r" + s + "\r@" for s in sections))
print(f"{out}: {len(new_wos)} world objects, {len(new_invs)} inventories")
