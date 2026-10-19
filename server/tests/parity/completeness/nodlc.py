"""A save without what stands on the DLC planets, so the real game host (which owns no DLC under Wine
without Steam, and then treats those planets' objects differently) and this server can be compared:

    python3 nodlc.py DATA_DIR SAVE.json OUT.json

Removes the world objects of the planets with a Steam app id (netdata.json), the inventories they own
(linked and secondary) with what is inside, and the procedural instances of those planets.
The planets' unit records stay.
"""
import json
import sys


def main():
    data, src, dst = sys.argv[1:4]
    n = json.load(open(f'{data}/netdata.json'))
    dlc = {p['hash'] for p in n['planets'] if p['steamAppId']}
    text = open(src, encoding='utf-8-sig', newline='').read()
    sec = text.split('@')
    objects = [json.loads(r) for r in sec[3].split('|') if r.strip()]
    inventories = [json.loads(r) for r in sec[4].split('|') if r.strip()]
    instances = [json.loads(r) for r in sec[9].split('|') if r.strip()]
    gone = {o['id'] for o in objects if o.get('planet') in dlc}
    drop_inv = set()
    for o in objects:
        if o['id'] in gone:
            if o.get('liId'):
                drop_inv.add(o['liId'])
            drop_inv.update(int(i) for i in str(o.get('siIds', '')).split(',') if i)
    for i in inventories:
        if i['id'] in drop_inv:
            gone.update(int(w) for w in str(i['woIds']).split(',') if w)
    import re
    keep = {3: [o for o in objects if o['id'] not in gone], 4: [i for i in inventories if i['id'] not in drop_inv], 9: [i for i in instances if i.get('planet') not in dlc]}
    for k, rows in keep.items():
        pre, _, post = re.match(r'(\s*)(.*?)(\s*)$', sec[k], re.S).groups()
        sec[k] = pre + '|\n'.join(json.dumps(r, separators=(',', ':')) for r in rows) + post
    open(dst, 'w', encoding='utf-8-sig', newline='').write('@'.join(sec))
    print(f'{len(gone)} objects, {len(drop_inv)} inventories, {len([i for i in instances if i.get("planet") in dlc])} instances removed')


if __name__ == '__main__':
    main()
