# hashwork — see ../praxis/hashwork.px for the description.
#
# `collections.Counter` rather than a plain dict, because its absent-reads-as-
# zero semantics are exactly Praxis's `Counter[T]` (§6.2) and a `dict.get(k, 0)`
# dance would be a different program.
import sys
from collections import Counter


def main():
    n = int(sys.stdin.read().strip())

    m = {}
    seen = set()
    counts = Counter()

    state = 12345
    i = 0
    while i < n:
        state = (state * 1103515245 + 12345) % 2147483648
        k = state % 65536
        m[k] = i
        seen.add(k % 1024)
        counts[k % 256] += 1
        i += 1

    probe = 999
    hits = 0
    j = 0
    while j < n:
        probe = (probe * 1103515245 + 12345) % 2147483648
        k = probe % 98304
        if k in m:
            hits += 1
        j += 1

    print(len(m))
    print(len(seen))
    print(sum(counts.values()))
    print(hits)


main()
