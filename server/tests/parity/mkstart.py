"""The start save of the parity runs, made from a Planet Crafter save:

    python3 mkstart.py SAVE.json START.json

- A host record of its own (id 0, name Extractor, host, at the escape pod, two empty inventories):
  the reference's hidden host player takes the saved host's place otherwise (README "Parity with
  the game"). The save's own host flag goes.
- Ore extractors full: each makes one of its ores at random every 70 s, which changes the counts
  and the save of both servers differently on every run. A full inventory stops them.
"""
import json
import re
import sys

src, dst = sys.argv[1:3]
sec = open(src, encoding='utf-8-sig').read().split('@')
inventories = [json.loads(r) for r in sec[4].split('|') if r.strip()]
objects = [json.loads(r) for r in sec[3].split('|') if r.strip()]
pod = next(o for o in objects if o['gId'] == 'EscapePod')
x, y, z = (float(v) for v in pod['pos'].split(','))
a, b = max(i['id'] for i in inventories) + 1, max(i['id'] for i in inventories) + 2
sec[2] = sec[2].replace('"host":true', '"host":false')
host = ('{"id":0,"name":"Extractor","inventoryId":%d,"equipmentId":%d,"playerPosition":"%s,%s,%s",'
        '"playerRotation":"0,0,0,1","playerGaugeOxygen":370.0,"playerGaugeThirst":100.0,"playerGaugeHealth":100.0,'
        '"playerGaugeToxic":0.0,"host":true,"planetId":"Prime","totalCraftedObjects":0,"totalTerraTokenEarned":0,"cameraView":0}') % (a, b, x, round(y + 1, 4), z)
sec[2] = sec[2].rstrip('\n') + '|\n' + host + '\n'
raw = sec[4].rstrip('\n') + '|\n{"id":%d,"woIds":"","size":28}|\n{"id":%d,"woIds":"","size":4}\n' % (a, b)
extractors = {o['liId'] for o in objects if o['gId'] == 'OreExtractor1'}
for i in inventories:
    if i['id'] in extractors:
        count = len([w for w in i['woIds'].split(',') if w])
        raw = re.sub(r'(\{"id":%d,"woIds":"[^"]*","size":)\d+' % i['id'], lambda m: m.group(1) + str(count), raw)
sec[4] = raw
open(dst, 'w', encoding='utf-8-sig').write('@'.join(sec))
