"""Reference for src/dotnet_sort.rs: the sort of the game's mscorlib (Mono, Unity 6000.3.2f1), written
from the IL of ArraySortHelper<T> / GenericArraySortHelper<T> in `Planet Crafter_Data/Managed/mscorlib.dll`
(IntrospectiveSort -> IntroSort -> PickPivotAndPartition / Heapsort / InsertionSort), element by element
as that code does it (the element being inserted or sifted is held out, not swapped).

    python3 reference.py            # rewrites vectors.txt
    python3 reference.py check      # fails when vectors.txt differs from what this computes

vectors.txt: one case per line, `name|keys|order`: the keys of the elements (compared as integers,
ascending, the comparer looks at nothing else), and the original index of the element at each place
after the sort. Inputs where the order of equal keys matters; `heap` cases reach the heap sort.
Cross-checked against a real Mono (wine-mono's csc + runtime): see Check.cs.
"""
import sys

stats = {'heap': 0}


def sort(keys, cmp=None):
    """Returns the original indices in the order the .NET sort leaves the elements."""
    a = list(range(len(keys)))
    n = len(a)
    if n < 2:
        return a
    if cmp is None:
        def cmp(x, y):
            return (keys[x] > keys[y]) - (keys[x] < keys[y])

    def swap_if_greater(i, j):
        if i != j and cmp(a[i], a[j]) > 0:
            a[i], a[j] = a[j], a[i]

    def swap(i, j):
        if i != j:
            a[i], a[j] = a[j], a[i]

    def insertion(lo, hi):
        for i in range(lo, hi):
            j = i
            t = a[i + 1]
            while j >= lo and cmp(t, a[j]) < 0:
                a[j + 1] = a[j]
                j -= 1
            a[j + 1] = t

    def down_heap(i, n, lo):
        d = a[lo + i - 1]
        while i <= n // 2:
            child = 2 * i
            if child < n and cmp(a[lo + child - 1], a[lo + child]) < 0:
                child += 1
            if not cmp(d, a[lo + child - 1]) < 0:
                break
            a[lo + i - 1] = a[lo + child - 1]
            i = child
        a[lo + i - 1] = d

    def heapsort(lo, hi):
        stats['heap'] += 1
        n = hi - lo + 1
        for i in range(n // 2, 0, -1):
            down_heap(i, n, lo)
        for i in range(n, 1, -1):
            swap(lo, lo + i - 1)
            down_heap(1, i - 1, lo)

    def partition(lo, hi):
        middle = lo + (hi - lo) // 2
        swap_if_greater(lo, middle)
        swap_if_greater(lo, hi)
        swap_if_greater(middle, hi)
        pivot = a[middle]
        swap(middle, hi - 1)
        left, right = lo, hi - 1
        while left < right:
            left += 1
            while cmp(a[left], pivot) < 0:
                left += 1
            right -= 1
            while cmp(pivot, a[right]) < 0:
                right -= 1
            if left >= right:
                break
            swap(left, right)
        swap(left, hi - 1)
        return left

    def intro(lo, hi, depth):
        while hi > lo:
            size = hi - lo + 1
            if size <= 16:
                if size == 2:
                    swap_if_greater(lo, hi)
                elif size == 3:
                    swap_if_greater(lo, hi - 1)
                    swap_if_greater(lo, hi)
                    swap_if_greater(hi - 1, hi)
                else:
                    insertion(lo, hi)
                return
            if depth == 0:
                heapsort(lo, hi)
                return
            depth -= 1
            p = partition(lo, hi)
            intro(p + 1, hi, depth)
            hi = p - 1

    intro(0, n - 1, 2 * n.bit_length())
    return a


def lcg(seed):
    s = seed
    while True:
        s = (s * 6364136223846793005 + 1442695040888963407) % (1 << 64)
        yield s >> 33


def cases():
    out = []
    for n in list(range(2, 41)) + [48, 64, 100, 253, 300]:
        out.append((f'equal{n}', [7] * n))
    for n in (5, 17, 18, 25, 33, 64, 100, 253):
        for k in (2, 3, 6):
            r = lcg(n * 31 + k)
            out.append((f'rand{n}k{k}', [next(r) % k for _ in range(n)]))
    # the shape of the game's demand list: sorted by priority, long runs, the biggest at the end
    out.append(('runs253', [9] * 3 + [8] * 11 + [5] * 30 + [3] * 50 + [0] * 159))
    out.append(('runs60', [3] * 7 + [2] * 13 + [1] * 40))
    out.append(('desc40', [40 - i // 4 for i in range(40)]))
    out.append(('asc40', [i // 4 for i in range(40)]))
    # inputs that reach the heap sort: McIlroy's adversary ("A Killer Adversary for Quicksort") makes
    # the comparisons answer so that every partition is as bad as it can be; the values it settled
    # on are the input (all different, then the same values twice over for ties).
    for n in (60, 100, 300):
        keys = killer(n)
        out.append((f'heap{n}', keys))
        out.append((f'heap{n}twice', [k // 2 for k in keys]))
    return out


def killer(n):
    gas, val, state = n, [n] * n, {'solid': 0, 'cand': 0}

    def freeze(x):
        val[x] = state['solid']
        state['solid'] += 1

    def cmp(x, y):
        if val[x] == gas and val[y] == gas:
            freeze(x if x == state['cand'] else y)
        if val[x] == gas:
            state['cand'] = x
        elif val[y] == gas:
            state['cand'] = y
        return (val[x] > val[y]) - (val[x] < val[y])

    sort(list(range(n)), cmp)
    for x in range(n):
        if val[x] == gas:
            freeze(x)
    return val


def lines():
    res = []
    for name, keys in cases():
        stats['heap'] = 0
        order = sort(keys)
        if name.startswith("heap") and not name.endswith("twice"):
            assert stats['heap'], name
        res.append(f'{name}|{",".join(map(str, keys))}|{",".join(map(str, order))}')
    return '\n'.join(res) + '\n'


if __name__ == '__main__':
    text = lines()
    path = __file__.rsplit('/', 1)[0] + '/vectors.txt'
    if sys.argv[1:] == ['check']:
        sys.exit(0 if open(path).read() == text else 'vectors.txt differs from reference.py')
    open(path, 'w').write(text)
    print(len(text.splitlines()), 'cases')
