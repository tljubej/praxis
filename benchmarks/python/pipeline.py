# pipeline — see ../praxis/pipeline.px for the description.
#
# Generator expressions are Python's fused pipeline: no intermediate list is
# built, which is the same property §6.3 claims for a Praxis chain.
# `functools.reduce` is the `fold`.
import sys
from functools import reduce


def main():
    n = int(sys.stdin.read().strip())

    v = []
    i = 0
    while i < n:
        v.append((i * 7919 + 13) % 1000003)
        i += 1

    acc = 0
    r = 0
    while r < 12:
        salt = r
        acc = (acc + sum(y for y in ((x * 3 + salt) % 1000003 for x in v) if y % 5 != 0)) % 1000003
        r += 1

    folded = 0
    s = 0
    while s < 12:
        salt = s
        folded = (
            folded
            + reduce(
                lambda a, x: (a * 2 + x) % 1000003,
                ((x + idx * salt) % 1000003 for idx, x in enumerate(v)),
                0,
            )
        ) % 1000003
        s += 1

    print(len(v))
    print(acc)
    print(folded)


main()
