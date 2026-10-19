"""Census of a Planet Crafter save: what is in it, by group, as aggregate counts only.

    python3 census.py DATA_DIR SAVE.json OUT.json

No player id, name, position or other personal field is read into the output: players are
counted. OUT.json holds, per group of the game's tables (every one, present or not):
  world        objects with a place in the world (a position), total and per planet
  inventoried  objects inside an inventory, and what kind of holder: player, vehicle, structure
  fields       how many objects use each optional save field (growth, text, colour, setting,
               linked groups, panels, genetic trait, hunger ...)
and the planets' terraformation units, settings, unlocked groups, story events and messages.
"""
import collections
import json
import sys


def load(path):
    text = open(path, encoding='utf-8-sig').read()
    return [[json.loads(r) for r in s.split('|') if r.strip()] for s in text.strip().split('@')]


def tables(data_dir):
    n = json.load(open(f'{data_dir}/netdata.json'))
    gd = json.load(open(f'{data_dir}/gamedata.json'))
    kinds = {}
    for key, v in gd['assets'].items():
        k = key.split(':')[0]
        if k in ('GroupDataItem', 'GroupDataConstructible'):
            kinds[v['id']] = {'kind': 'item' if k == 'GroupDataItem' else 'constructible',
                              'category': v.get('itemCategory') if k == 'GroupDataItem' else v.get('groupCategory')}
    planets = {p['hash']: p['id'] for p in n['planets']}
    return sorted(n['groups']), kinds, planets


VEHICLES = ('VehicleTruck',)


def census(data_dir, save):
    groups, kinds, planets = tables(data_dir)
    s = load(save)
    state, planet_rows, players, objects, inventories = s[0][0], s[1], s[2], s[3], s[4]
    wo = {o['id']: o for o in objects}
    # inventory id -> holder: a player's, or the world object whose liId it is.
    holder = {}
    for p in players:
        holder[p.get('inventoryId')] = ('player', '-')
        holder[p.get('equipmentId')] = ('player', '-')
    for o in objects:
        if o.get('liId'):
            kind = 'vehicle' if o['gId'] in VEHICLES else 'structure'
            holder.setdefault(o['liId'], (kind, o['gId']))
    rows = {g: {'group': g, **kinds.get(g, {'kind': '?', 'category': None}), 'total': 0, 'world': 0, 'by_planet': collections.Counter(),
                'in_player': 0, 'in_vehicle': 0, 'in_structure': 0, 'in_unowned': 0, 'fields': collections.Counter(), 'held_by': collections.Counter()}
            for g in groups}
    unknown = collections.Counter()
    in_inv = {}
    for inv in inventories:
        for w in str(inv.get('woIds', '')).split(','):
            if w:
                in_inv[int(w)] = inv['id']
    for o in objects:
        r = rows.get(o['gId'])
        if r is None:
            unknown[o['gId']] += 1
            continue
        r['total'] += 1
        if o['id'] in in_inv:
            kind, owner = holder.get(in_inv[o['id']], ('unowned', '-'))
            r['in_' + kind] += 1
            if kind == 'structure':
                r['held_by'][owner] += 1
        if 'pos' in o:
            r['world'] += 1
            r['by_planet'][planets.get(o.get('planet'), str(o.get('planet')))] += 1
        for f in ('liId', 'color', 'set', 'liGrps', 'text', 'grwth', 'pnls', 'siIds', 'hunger', 'trtInd', 'trtVal', 'liPlanet', 'count'):
            if f in o:
                r['fields'][f] += 1
    inv_stats = collections.Counter()
    for inv in inventories:
        inv_stats['inventories'] += 1
        inv_stats['with demandGrps'] += bool(inv.get('demandGrps'))
        inv_stats['with supplyGrps'] += bool(inv.get('supplyGrps'))
        inv_stats['with priority'] += bool(inv.get('priority'))
        inv_stats['empty'] += not str(inv.get('woIds', '')).strip(',')
    unlocked = [g for g in str(state.get('unlockedGroups', '')).split(',') if g]
    settings = {k: v for k, v in s[8][0].items() if k not in ('saveDisplayName', 'worldSeed')}
    out = {
        'save': {'objects': len(objects), 'inventories': len(inventories), 'players': len(players), 'planets': [p['planetId'] for p in planet_rows],
                 'messages': len(s[6]), 'story_events': [e['stringId'] for e in s[7]], 'instances': len(s[9]), 'logistics_records': len(s[10]),
                 'crafted': s[5][0].get('craftedObjects'), 'sections': len(s)},
        'unknown_groups': dict(unknown),
        'unlocked_groups': unlocked,
        'planets': {p['planetId']: {k: v for k, v in p.items() if k != 'planetId'} for p in planet_rows},
        'settings': settings,
        'inventory_stats': dict(inv_stats),
        'groups': {g: {**{k: v for k, v in r.items() if k not in ('by_planet', 'fields', 'held_by')},
                       'by_planet': dict(r['by_planet']), 'fields': dict(r['fields']), 'held_by': dict(r['held_by'].most_common(6))} for g, r in rows.items()},
    }
    return out


if __name__ == '__main__':
    data_dir, save, out = sys.argv[1:4]
    result = census(data_dir, save)
    json.dump(result, open(out, 'w'), indent=1, sort_keys=True)
    present = [g for g, r in result['groups'].items() if r['total']]
    print(f"{len(present)} of {len(result['groups'])} groups present; {result['save']['objects']} objects")
