# bfs — see ../praxis/bfs.px for the description.
#
# `bytearray` is Python's idiomatic dense visited mark, the way `BitSet` is
# Praxis's and `Vec<bool>` is Rust's. Everything else — the adjacency lists, the
# level-synchronous two-deque walk, the checksums — is the same algorithm on the
# same graph.
import sys
from collections import deque


def open_cell(x, y):
    return (x * 31 + y * 17 + 5) % 11 != 0


def main():
    runs = int(sys.stdin.read().strip())
    w = 320
    h = 320

    adj = []
    y = 0
    while y < h:
        x = 0
        while x < w:
            nb = []
            if open_cell(x, y):
                if x > 0 and open_cell(x - 1, y):
                    nb.append(y * w + x - 1)
                if x + 1 < w and open_cell(x + 1, y):
                    nb.append(y * w + x + 1)
                if y > 0 and open_cell(x, y - 1):
                    nb.append((y - 1) * w + x)
                if y + 1 < h and open_cell(x, y + 1):
                    nb.append((y + 1) * w + x)
            adj.append(nb)
            x += 1
        y += 1

    open_count = 0
    c = 0
    while c < w * h:
        if len(adj[c]) > 0:
            open_count += 1
        c += 1

    reached_total = 0
    dist_total = 0
    run = 0
    while run < runs:
        start = (run * 7919) % (w * h)
        while len(adj[start]) == 0:
            start = (start + 1) % (w * h)

        visited = bytearray(w * h)
        visited[start] = 1
        current = deque()
        current.append(start)
        dist = 0

        while len(current) > 0:
            nxt = deque()
            while len(current) > 0:
                node = current.popleft()
                reached_total += 1
                dist_total = (dist_total + dist) % 1000003
                for nb in adj[node]:
                    if not visited[nb]:
                        visited[nb] = 1
                        nxt.append(nb)
            current = nxt
            dist += 1
        run += 1

    print(open_count)
    print(reached_total)
    print(dist_total)


main()
