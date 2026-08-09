#!/usr/bin/env python3
"""Minimal VT emulator: raw pty recording -> compact row-diff frames for the web player."""
import json
import re
import sys

DEFAULT = (None, None, 0)  # fg, bg, flags bitmask (1 bold, 2 dim, 4 italic, 8 underline, 16 reverse)


class Screen:
    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.clear_all()

    def clear_all(self):
        self.cells = [[(" ", DEFAULT) for _ in range(self.cols)] for _ in range(self.rows)]
        self.x = self.y = 0
        self.attr = DEFAULT

    def put(self, ch):
        if self.x >= self.cols:
            self.x = 0
            self.y += 1
        self.scroll_if_needed()
        if 0 <= self.y < self.rows:
            self.cells[self.y][self.x] = (ch, self.attr)
        self.x += 1

    def scroll_if_needed(self):
        while self.y >= self.rows:
            self.cells.pop(0)
            self.cells.append([(" ", DEFAULT) for _ in range(self.cols)])
            self.y -= 1

    def snapshot(self):
        rows = []
        for row in self.cells:
            runs, cur, buf = [], None, []
            for ch, attr in row:
                key = attr
                if key != cur:
                    if buf:
                        runs.append(["".join(buf), cur])
                    buf, cur = [], key
                buf.append(ch)
            if buf:
                runs.append(["".join(buf), cur])
            # Trim trailing default-attr whitespace run.
            while runs and runs[-1][0].strip() == "" and runs[-1][1] == DEFAULT:
                runs.pop()
            rows.append(runs)
        return rows


CSI = re.compile(r"\x1b\[([?!>]?)([0-9;:]*)([@-~])")


def sgr(attr, params):
    fg, bg, flags = attr
    if not params:
        params = [0]
    i = 0
    while i < len(params):
        p = params[i]
        if p == 0:
            fg, bg, flags = None, None, 0
        elif p == 1:
            flags |= 1
        elif p == 2:
            flags |= 2
        elif p == 3:
            flags |= 4
        elif p == 4:
            flags |= 8
        elif p == 7:
            flags |= 16
        elif p == 22:
            flags &= ~3
        elif p == 23:
            flags &= ~4
        elif p == 24:
            flags &= ~8
        elif p == 27:
            flags &= ~16
        elif 30 <= p <= 37:
            fg = p - 30
        elif p == 39:
            fg = None
        elif 40 <= p <= 47:
            bg = p - 40
        elif p == 49:
            bg = None
        elif 90 <= p <= 97:
            fg = p - 90 + 8
        elif 100 <= p <= 107:
            bg = p - 100 + 8
        elif p in (38, 48):
            if i + 1 < len(params) and params[i + 1] == 5:
                val = params[i + 2] if i + 2 < len(params) else 0
                if p == 38:
                    fg = val
                else:
                    bg = val
                i += 2
            elif i + 1 < len(params) and params[i + 1] == 2:
                r, g, b = (params[i + 2:i + 5] + [0, 0, 0])[:3]
                val = "#%02x%02x%02x" % (r, g, b)
                if p == 38:
                    fg = val
                else:
                    bg = val
                i += 4
        i += 1
    return (fg, bg, flags)


def run(rec):
    cols, rows = rec["cols"], rec["rows"]
    sc = Screen(cols, rows)
    frames = []
    prev = None

    def emit(t):
        nonlocal prev
        snap = sc.snapshot()
        if prev is None:
            frames.append({"t": round(t, 3), "full": snap})
        else:
            diff = [[i, snap[i]] for i in range(rows) if snap[i] != prev[i]]
            if not diff:
                return
            frames.append({"t": round(t, 3), "d": diff})
        prev = snap

    # An escape sequence can straddle two reads, so carry an incomplete tail
    # into the next chunk rather than printing its bytes as text.
    partial = re.compile(r"\x1b(?:\[[?!>]?[0-9;:]*|\][^\x07\x1b]*|[(#)])?$")
    pending = ""

    for t, ev in rec["events"]:
        chunk = pending + ev
        pending = ""
        i = 0
        n = len(chunk)
        while i < n:
            c = chunk[i]
            if c == "\x1b" and partial.match(chunk, i):
                pending = chunk[i:]
                break
            if c == "\x1b":
                m = CSI.match(chunk, i)
                if m:
                    priv, raw, final = m.group(1), m.group(2), m.group(3)
                    ps = [int(x) if x.isdigit() else 0 for x in raw.replace(":", ";").split(";")] if raw else []
                    p0 = ps[0] if ps else 0
                    if final == "m" and not priv:
                        sc.attr = sgr(sc.attr, ps)
                    elif final in "Hf":
                        sc.y = max(0, (ps[0] if len(ps) > 0 and ps[0] else 1) - 1)
                        sc.x = max(0, (ps[1] if len(ps) > 1 and ps[1] else 1) - 1)
                    elif final == "A":
                        sc.y = max(0, sc.y - max(1, p0))
                    elif final == "B":
                        sc.y = min(rows - 1, sc.y + max(1, p0))
                    elif final == "C":
                        sc.x = min(cols - 1, sc.x + max(1, p0))
                    elif final == "D":
                        sc.x = max(0, sc.x - max(1, p0))
                    elif final == "G":
                        sc.x = max(0, (p0 or 1) - 1)
                    elif final == "d":
                        sc.y = max(0, (p0 or 1) - 1)
                    elif final == "J":
                        blank = (" ", DEFAULT)
                        if p0 == 2 or p0 == 3:
                            sc.cells = [[blank for _ in range(cols)] for _ in range(rows)]
                        elif p0 == 0:
                            for x in range(sc.x, cols):
                                sc.cells[sc.y][x] = blank
                            for y in range(sc.y + 1, rows):
                                sc.cells[y] = [blank for _ in range(cols)]
                        elif p0 == 1:
                            for y in range(0, sc.y):
                                sc.cells[y] = [blank for _ in range(cols)]
                            for x in range(0, min(sc.x + 1, cols)):
                                sc.cells[sc.y][x] = blank
                    elif final == "K":
                        blank = (" ", DEFAULT)
                        if p0 == 0:
                            for x in range(sc.x, cols):
                                sc.cells[sc.y][x] = blank
                        elif p0 == 1:
                            for x in range(0, min(sc.x + 1, cols)):
                                sc.cells[sc.y][x] = blank
                        else:
                            sc.cells[sc.y] = [blank for _ in range(cols)]
                    elif final == "h" and priv == "?" and p0 in (1049, 47, 1047):
                        sc.cells = [[(" ", DEFAULT) for _ in range(cols)] for _ in range(rows)]
                        sc.x = sc.y = 0
                    i = m.end()
                    continue
                if chunk.startswith("\x1b]", i):
                    end = chunk.find("\x07", i)
                    end2 = chunk.find("\x1b\\", i)
                    if end == -1 or (end2 != -1 and end2 < end):
                        end = end2 + 1 if end2 != -1 else n - 1
                    i = end + 1
                    continue
                i += 2
                continue
            if c == "\r":
                sc.x = 0
            elif c == "\n":
                sc.y += 1
                sc.scroll_if_needed()
            elif c == "\b":
                sc.x = max(0, sc.x - 1)
            elif c == "\t":
                sc.x = min(cols - 1, (sc.x // 8 + 1) * 8)
            elif c == "\x07":
                pass
            elif ord(c) >= 32:
                sc.put(c)
            i += 1
        emit(t)

    return {"cols": cols, "rows": rows, "frames": frames}


if __name__ == "__main__":
    rec = json.load(open(sys.argv[1]))
    out = run(rec)
    json.dump(out, open(sys.argv[2], "w"), separators=(",", ":"))
    print(sys.argv[2], len(out["frames"]), "frames,", len(json.dumps(out)), "bytes")
