"""Print the differences of a parity.py run that are not the drones' network visibility, the loaded
signs' colour or the ground items' timing: what is left to read by hand.

    python3 nodrone.py RUN.diff
"""
import sys

cur = None
out = {}
for l in open(sys.argv[1]):
    if l.startswith('## '):
        cur = l.strip()
        out[cur] = []
    elif cur and l[:1] in '+-~':
        if any(k in l for k in ('GroupNetworkContainerWithInventory', 'Sign@', 'GroupNetworkContainer@', 'new:GroupNetworkContainer', 'GroupNetworkContainerStatic')):
            continue
        out[cur].append(l.rstrip()[:240])
for k, v in out.items():
    if v and 'steps apart' not in k:
        print(k)
        for x in v[:5]:
            print('   ', x)
