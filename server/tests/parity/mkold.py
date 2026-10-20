"""A save in one of the game's old layouts, made from a save of the current layout (the sweep's
start save, say), for the import tests and the parity run of the import against the game:

    python3 mkold.py SAVE.json OUT.json [--tokens | --settings-only] [--chill]

default        the oldest layout (2022, one planet; 9 sections, each "\\n" + JSON + "\\n@"): the units of
               Prime, the host (or first) player with the unlocked groups, the world objects of Prime
               (no planet field, a "wear" field the game no longer has), the inventories they use
               (the player's become 1 and 2), stats, messages, story events, a game mode, and the
               colour layers (the game reads none of them any more).
--tokens       the layout between: the first section also holds terraTokens, allTimeTerraTokens and
               unlockedGroups (last), the player record holds no groups.
--settings-only  the current layout whose game settings are the old ones: a game mode only.
--chill        the mode is Chill (else the save's: Standard, Intense and Hardcore map differently).

Nothing of the save is copied into the repository: run it on a save of your own.
"""
import json
import sys

PRIME = -1140328421


def sections(path):
    text = open(path, encoding='utf-8-sig').read()
    return [[json.loads(r) for r in s.split('|') if r.strip()] for s in text.strip().split('@')]


def main():
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    flags = {a for a in sys.argv[1:] if a.startswith('--')}
    src, dst = args
    s = sections(src)
    world, planets, players, objects, inventories, stats, messages, story, settings, instances = (s + [[]] * 10)[:10]
    world, stats, settings = world[0], stats[0], settings[0]
    mode = 'Chill' if '--chill' in flags else settings.get('mode', 'Standard')
    dump = lambda v: json.dumps(v, separators=(',', ':'))

    if '--settings-only' in flags:
        s[8] = [{'mode': mode, 'hasPlayedIntro': settings.get('hasPlayedIntro', False)}]
        out = '﻿' + ''.join('\r' + '|\n'.join(dump(r) for r in sec) + '\r@' for sec in s[:10])
        open(dst, 'w', encoding='utf-8').write(out)
        return

    prime = next(p for p in planets if p['planetId'] == 'Prime')
    # What the save's procedural instances (wrecks) hold is not on the planet.
    held = set()
    for i in instances:
        for k in ('woIdsGenerated', 'woIdsDropped'):
            held.update(int(x) for x in i.get(k, '').split(',') if x)
    kept = [o for o in objects if o.get('planet', 0) in (0, PRIME) and o['id'] not in held]
    host = next((p for p in players if p.get('host')), players[0])

    # Inventories: the player's become 1 and 2, whoever had those ids moves to a free one.
    by_id = {i['id']: i for i in inventories}
    used = {o.get('liId', 0) for o in kept} | {int(x) for o in kept for x in o.get('siIds', '').split(',') if x}
    used.discard(0)
    free = max(list(by_id) + [2]) + 1
    remap = {host['inventoryId']: 1, host['equipmentId']: 2}
    for old in (1, 2):
        if old not in remap and old in by_id:
            remap[old] = free
            free += 1
    new = lambda i: remap.get(i, i)
    mine = {new(i): by_id[i] for i in list(used) + [host['inventoryId'], host['equipmentId']] if i in by_id}
    invs = []
    for i, inv in sorted(mine.items()):
        inv = dict(inv)
        inv['id'] = i
        invs.append({k: inv[k] for k in ('id', 'woIds', 'size') if k in inv} | {k: inv[k] for k in ('demandGrps', 'supplyGrps', 'priority') if k in inv})
    old_objects = []
    for o in kept:
        o = {k: v for k, v in o.items() if k != 'planet'}
        if o.get('liId'):
            o['liId'] = new(o['liId'])
        if o.get('siIds'):
            o['siIds'] = ','.join(str(new(int(x))) for x in o['siIds'].split(',') if x)
        # Fields at their default are written in 2022's files, and "wear" was a field then.
        full = {'id': o['id'], 'gId': o['gId'], 'liId': o.get('liId', 0), 'liGrps': o.get('liGrps', ''), 'pos': o.get('pos', '0,0,0'), 'rot': o.get('rot', '0,0,0,0'), 'wear': 0,
                'pnls': o.get('pnls', ''), 'color': o.get('color', ''), 'text': o.get('text', ''), 'grwth': o.get('grwth', 0)}
        old_objects.append(full | {k: v for k, v in o.items() if k not in full})

    units = {'unitOxygenLevel': prime['unitOxygenLevel'], 'unitHeatLevel': prime['unitHeatLevel'], 'unitPressureLevel': prime['unitPressureLevel'], 'unitBiomassLevel': prime['unitPlantsLevel']}
    player = {'playerPosition': host['playerPosition'], 'playerRotation': host['playerRotation'], 'unlockedGroups': world['unlockedGroups'],
              'playerGaugeOxygen': host['playerGaugeOxygen'], 'playerGaugeThirst': host['playerGaugeThirst'], 'playerGaugeHealth': host['playerGaugeHealth']}
    if '--tokens' in flags:
        units |= {'terraTokens': world['terraTokens'], 'allTimeTerraTokens': world['allTimeTerraTokens'], 'unlockedGroups': world['unlockedGroups']}
        del player['unlockedGroups']
    layers = [{'layerId': f'PCLayer{n}', 'colorBase': '0.25-0.79-0.24-1', 'colorCustom': '1-1-1-1', 'colorBaseLerp': 100, 'colorCustomLerp': 0} for n in range(1, 12)]
    old = [[units], [player], old_objects, invs, [{'craftedObjects': stats['craftedObjects'], 'totalSaveFileLoad': 0, 'totalSaveFileTime': 0}],
           messages, [{'stringId': e['stringId']} for e in story], [{'mode': mode, 'hasPlayedIntro': settings.get('hasPlayedIntro', False)}], layers]
    open(dst, 'w', encoding='utf-8').write(''.join('\n' + '|\n'.join(dump(r) for r in sec) + '\n@' for sec in old))
    print(f'{dst}: {len(old_objects)} world objects, {len(invs)} inventories, mode {mode}', file=sys.stderr)


main()
