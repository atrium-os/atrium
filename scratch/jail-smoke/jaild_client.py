#!/usr/bin/env python3
"""
Tiny client for atrium-jaild's protocol. Frames JSON requests
with a 4-byte LE length prefix, prints the response.

Usage:
    jaild_client.py <socket> <command>

Where <command> is one of:
    ping
    create <name> <path> [<children_max>]
    remove <jid>
"""
import json, socket, struct, sys

def send(s, msg):
    body = json.dumps(msg).encode("utf-8")
    s.sendall(struct.pack("<I", len(body)) + body)

def recv(s):
    n_bytes = b""
    while len(n_bytes) < 4:
        chunk = s.recv(4 - len(n_bytes))
        if not chunk:
            return None
        n_bytes += chunk
    n = struct.unpack("<I", n_bytes)[0]
    body = b""
    while len(body) < n:
        chunk = s.recv(n - len(body))
        if not chunk:
            return None
        body += chunk
    return json.loads(body.decode("utf-8"))

def main():
    if len(sys.argv) < 3:
        print(__doc__); sys.exit(2)
    sock_path = sys.argv[1]
    cmd       = sys.argv[2]

    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sock_path)

    if cmd == "ping":
        send(s, {"kind": "ping"})
    elif cmd == "create":
        name = sys.argv[3]
        path = sys.argv[4]
        cmax = int(sys.argv[5]) if len(sys.argv) > 5 else 0
        send(s, {
            "kind":         "create_jail",
            "name":         name,
            "path":         path,
            "children_max": cmax,
        })
    elif cmd == "remove":
        jid = int(sys.argv[3])
        send(s, {"kind": "remove_jail", "jid": jid})
    else:
        print(f"unknown command: {cmd}"); sys.exit(2)

    resp = recv(s)
    print(json.dumps(resp, indent=2))

if __name__ == "__main__":
    main()
