#!/usr/bin/env python3
"""Pack the pty recording and the plain ANSI captures into one compact JSON blob."""
import json
import re
import sys

sys.path.insert(0, ".")
from vt import sgr, DEFAULT, CSI  # noqa: E402

attrs = []
index = {}


def attr_id(a):
    key = json.dumps(a)
    if key not in index:
        index[key] = len(attrs)
        attrs.append(list(a))
    return index[key]


def pack_rows(rows):
    return [[[text, attr_id(tuple(a) if isinstance(a, list) else a)] for text, a in row] for row in rows]


# ---- the debugger recording -------------------------------------------------
tui = json.load(open("tui-frames.json"))
frames = []
for f in tui["frames"]:
    if "full" in f:
        frames.append({"t": f["t"], "f": pack_rows(f["full"])})
    else:
        frames.append({"t": f["t"], "d": [[i, pack_rows([row])[0]] for i, row in f["d"]]})

# Collapse frames closer together than 40ms — the pty flushes a redraw in
# several writes and every intermediate one is a half-drawn screen.
merged = []
for fr in frames:
    if merged and "d" in fr and "d" in merged[-1] and fr["t"] - merged[-1]["t"] < 0.04:
        rows = {i: r for i, r in merged[-1]["d"]}
        rows.update({i: r for i, r in fr["d"]})
        merged[-1]["d"] = sorted(rows.items())
        merged[-1]["t"] = fr["t"]
    else:
        merged.append(fr)


# ---- the plain captures -----------------------------------------------------
def ansi_lines(text):
    """ANSI text with no cursor motion -> list of lines of [text, attrId]."""
    lines, cur, attr = [], [], DEFAULT
    i, n = 0, len(text)
    buf = []

    def flush():
        if buf:
            cur.append([("".join(buf)), attr_id(attr)])
            buf.clear()

    while i < n:
        c = text[i]
        if c == "\x1b":
            m = CSI.match(text, i)
            if m:
                if m.group(3) == "m":
                    raw = m.group(2)
                    ps = [int(x) if x.isdigit() else 0 for x in raw.split(";")] if raw else []
                    flush()
                    attr = sgr(attr, ps)
                i = m.end()
                continue
            i += 1
            continue
        if c == "\n":
            flush()
            lines.append(cur)
            cur = []
        elif c == "\r":
            pass
        else:
            buf.append(c)
        i += 1
    flush()
    if cur:
        lines.append(cur)
    return lines


caps = json.load(open("captures.json"))
packed_caps = {}
for name, v in caps.items():
    packed_caps[name] = {
        "out": ansi_lines(v["out"]),
        "err": ansi_lines(v["err"]),
        "code": v["code"],
    }

blob = {"attrs": attrs, "tui": {"cols": tui["cols"], "rows": tui["rows"], "frames": merged}, "caps": packed_caps}
out = json.dumps(blob, separators=(",", ":"))
open("demo-data.json", "w").write(out)
print("demo-data.json", len(out), "bytes,", len(merged), "frames,", len(attrs), "attrs")
print("attrs:", attrs)
