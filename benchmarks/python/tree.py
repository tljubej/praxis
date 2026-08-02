# tree — see ../praxis/tree.px for the description.
#
# Nodes are `(l, r, v)` tuples: Python's compact record, and the form a Python
# programmer reaches for when the record is three ints and the traversal is the
# hot loop. The arena shape is kept so all three walk the same structure.
import sys


def build(ns, depth, v):
    if depth == 0:
        ns.append((-1, -1, v))
        return len(ns) - 1
    li = build(ns, depth - 1, (v * 2 + 1) % 1000003)
    ri = build(ns, depth - 1, (v * 2 + 2) % 1000003)
    ns.append((li, ri, v))
    return len(ns) - 1


def walk(ns, i, salt):
    l, r, v = ns[i]
    if l < 0:
        return (v + salt) % 1000003
    return (v + walk(ns, l, salt) + walk(ns, r, salt) * 3) % 1000003


def main():
    reps = int(sys.stdin.read().strip())
    depth = 16

    nodes = []
    root = build(nodes, depth, 1)

    acc = 0
    r = 0
    while r < reps:
        acc = (acc + walk(nodes, root, r)) % 1000003
        r += 1

    print(len(nodes))
    print(acc)


main()
