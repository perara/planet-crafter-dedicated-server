"""dlc_selection.py SAVE TABLES PLANET: the group each ore extractor of the planet is set to (the player picks it among the groups of
the vein below, ActionGroupSelectorVeinExtractor), in a save the real game made, against the groups of the veins the extracted ground puts below it
(a ray down from 25 m above the extractor into each vein's boxes, as MachineGenerator does). Aggregate output only.
A few extractors of a late-game save hold placeholder items of a basic ore over no vein for it, on every planet (the free ones
too): the check is a rate to read next to a free planet's, not an equality."""
import json,sys,collections
save,tables,planet=sys.argv[1:4]
hashes={'Humble':-486276833,'Toxicity':110910045,'Prime':-1140328421,'Selenea':-1016990411,'Aqualis':-1291310150}
sec=open(save,encoding='utf-8-sig').read().split('@')
objs=[json.loads(r) for r in sec[3].split('|') if r.strip()]
scene=json.load(open(f'{tables}/scenes/{planet}.json'))
def rot(q,v):
    x,y,z,w=q; 
    # v' = v + 2*w*(u x v) + 2*(u x (u x v))
    ux,uy,uz=x,y,z
    cx,cy,cz=uy*v[2]-uz*v[1],uz*v[0]-ux*v[2],ux*v[1]-uy*v[0]
    dx,dy,dz=uy*cz-uz*cy,uz*cx-ux*cz,ux*cy-uy*cx
    return [v[0]+2*(w*cx+dx),v[1]+2*(w*cy+dy),v[2]+2*(w*cz+dz)]
def conj(q): return [-q[0],-q[1],-q[2],q[3]]
def box(c):
    b=c['box']
    if b is None: return None
    p,r,s,ctr,size=b['position'],b['rotation'],b['scale'],b['center'],b['size']
    off=rot(r,[ctr[i]*s[i] for i in range(3)])
    return [p[i]+off[i] for i in range(3)],[size[i]*abs(s[i])/2 for i in range(3)],r
def hit(origin,bx):
    c,h,r=bx
    o=rot(conj(r),[origin[i]-c[i] for i in range(3)]); d=rot(conj(r),[0,-1,0])
    if all(abs(o[i])<=h[i] for i in range(3)): return False
    near,far=0.0,1000.0
    for i in range(3):
        if abs(d[i])<1e-6:
            if abs(o[i])>h[i]: return False
            continue
        a,b=(-h[i]-o[i])/d[i],(h[i]-o[i])/d[i]
        near=max(near,min(a,b)); far=min(far,max(a,b))
        if near>far: return False
    return True
res=collections.Counter()
for o in objs:
    if o['gId'].startswith('OreExtractor') and o.get('planet')==hashes[planet] and o.get('liGrps'):
        x,y,z=[float(v) for v in o['pos'].split(',')]; origin=(x,y+25,z)
        beneath=set()
        for v in scene['oreVeins']:
            if any(c['enabled'] and box(c) and hit(origin,box(c)) for c in v['colliders']):
                beneath|={r.split(':')[1] for r in v['fields']['groups']}
        names=set(o['liGrps'].split(','))
        res['extractors with a selection']+=1
        res['selection among the groups below']+= all(any(n==b or 'Minable-'+n==b for b in beneath) for n in names)
print(planet,dict(res))
