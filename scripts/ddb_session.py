#!/usr/bin/env python3
"""Talk to FreeBSD ddb over QEMU's TCP serial.

Usage:
  ddb_session.py "<commands separated by ;>" [timeout-per-cmd]
  ddb_session.py break          # send the alt_break sequence to drop into ddb
  ddb_session.py continue       # send 'c' to resume from ddb
  ddb_session.py quit           # quit the debugger entirely (q)
"""
import socket, sys, time, select

HOST, PORT = "127.0.0.1", 4444

def open_sock():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.connect((HOST, PORT))
    s.setblocking(False)
    return s

def drain(s, timeout):
    """Read up to `timeout` seconds of idle. Auto-responds to ddb pager
    by sending space whenever the trailing buffer matches '--More--'."""
    buf = b""
    deadline = time.time() + timeout
    while time.time() < deadline:
        rl, _, _ = select.select([s], [], [], 0.1)
        if rl:
            try:
                chunk = s.recv(8192)
                if not chunk:
                    break
                buf += chunk
                # Reset deadline on activity
                deadline = time.time() + timeout
                # Check for pager marker; respond with space.
                if b"--More--" in buf[-32:]:
                    s.sendall(b" ")
                    # consume the marker so we don't react twice
                    idx = buf.rfind(b"--More--")
                    buf = buf[:idx] + buf[idx + len(b"--More--"):]
            except BlockingIOError:
                pass
    return buf

def cmd_break():
    s = open_sock()
    s.sendall(b"\r")
    time.sleep(0.2)
    s.sendall(b"~\x02")
    time.sleep(0.4)
    out = drain(s, 1.5)
    sys.stdout.buffer.write(out)
    sys.stdout.flush()
    s.close()

def cmd_run(cmds, per_cmd_timeout):
    s = open_sock()
    # Drain any prior output first
    out = drain(s, 0.5)
    sys.stdout.buffer.write(out)
    s.sendall(b"\n")
    out = drain(s, 1.0)
    sys.stdout.buffer.write(out)
    sys.stdout.flush()
    for c in cmds.split(";"):
        c = c.strip()
        if not c:
            continue
        s.sendall((c + "\n").encode())
        time.sleep(0.3)
        out = drain(s, per_cmd_timeout)
        sys.stdout.buffer.write(out)
        sys.stdout.flush()
    s.close()

if len(sys.argv) >= 2 and sys.argv[1] == "break":
    cmd_break()
elif len(sys.argv) >= 2 and sys.argv[1] == "continue":
    cmd_run("c", 2.0)
elif len(sys.argv) >= 2 and sys.argv[1] == "quit":
    cmd_run("q", 2.0)
else:
    cmds = sys.argv[1] if len(sys.argv) > 1 else "c"
    t = float(sys.argv[2]) if len(sys.argv) > 2 else 3.0
    cmd_run(cmds, t)
