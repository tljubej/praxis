#!/usr/bin/env python3
"""Drive a program on a sized pty and record its raw output with timestamps.

Usage: record.py OUT.json COLSxROWS -- cmd args...   (script reads a keys file)

Keystrokes come from a JSON list on stdin: [[delay_seconds, "bytes"], ...]
"""
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import time
import fcntl


def main():
    out_path = sys.argv[1]
    cols, rows = (int(x) for x in sys.argv[2].split("x"))
    sep = sys.argv.index("--")
    cmd = sys.argv[sep + 1:]
    keys = json.load(sys.stdin)

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLORTERM"] = "truecolor"
        os.environ["LINES"] = str(rows)
        os.environ["COLUMNS"] = str(cols)
        os.execvp(cmd[0], cmd)

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    start = time.time()
    events = []
    queue = list(keys)
    next_at = start + queue[0][0] if queue else None
    deadline = start + 60.0

    while True:
        now = time.time()
        if now > deadline:
            break
        timeout = 0.05
        if next_at is not None:
            timeout = min(timeout, max(0.0, next_at - now))
        r, _, _ = select.select([fd], [], [], timeout)
        if r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            events.append([round(time.time() - start, 4), data.decode("utf-8", "replace")])
        now = time.time()
        if queue and next_at is not None and now >= next_at:
            _, payload = queue.pop(0)
            os.write(fd, payload.encode("utf-8"))
            next_at = now + queue[0][0] if queue else None
            if not queue:
                deadline = min(deadline, now + 3.0)
        try:
            done, _ = os.waitpid(pid, os.WNOHANG)
            if done == pid:
                # Drain whatever is left.
                for _ in range(20):
                    r, _, _ = select.select([fd], [], [], 0.05)
                    if not r:
                        break
                    try:
                        data = os.read(fd, 65536)
                    except OSError:
                        break
                    if not data:
                        break
                    events.append([round(time.time() - start, 4), data.decode("utf-8", "replace")])
                break
        except ChildProcessError:
            break

    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    os.close(fd)

    with open(out_path, "w") as f:
        json.dump({"cols": cols, "rows": rows, "events": events}, f)
    print(f"{out_path}: {len(events)} events, {sum(len(e[1]) for e in events)} bytes")


main()
