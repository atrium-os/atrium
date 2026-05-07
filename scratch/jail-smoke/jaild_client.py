#!/usr/bin/env python3
"""
Tiny client for atrium-jaild's protocol. Frames JSON requests
with a 4-byte LE length prefix, prints the response. Recognises
SCM_RIGHTS-attached fds (procdesc) on response.

Usage:
    jaild_client.py <socket> <command>

Where <command> is one of:
    ping
    create <name> <path> [<children_max>]
    remove <jid>
    exec   <name> <path> <bin> [<arg>...]    # exec <bin> in jail
"""
import array, json, os, socket, struct, sys

def send(s, msg):
    body = json.dumps(msg).encode("utf-8")
    s.sendall(struct.pack("<I", len(body)) + body)

def recvmsg_frame(s):
    """Receive one length-prefixed frame plus optional SCM_RIGHTS fd.
    Returns (parsed_json, fd_or_None)."""
    fds = array.array("i")
    cmsg_buf_size = socket.CMSG_SPACE(fds.itemsize * 4)  # room for up to 4 fds
    msg, ancdata, flags, _addr = s.recvmsg(4, cmsg_buf_size)
    if len(msg) < 4:
        return (None, None)
    length = struct.unpack("<I", msg)[0]
    body = b""
    while len(body) < length:
        chunk = s.recv(length - len(body))
        if not chunk:
            return (None, None)
        body += chunk

    fd = None
    for level, ctype, cdata in ancdata:
        if level == socket.SOL_SOCKET and ctype == socket.SCM_RIGHTS:
            fds.frombytes(cdata[:len(cdata) - len(cdata) % fds.itemsize])
            if len(fds) > 0:
                fd = fds[0]
    return (json.loads(body.decode("utf-8")), fd)

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
    elif cmd == "exec":
        name, path, binary = sys.argv[3], sys.argv[4], sys.argv[5]
        argv = sys.argv[5:]
        send(s, {
            "kind":         "create_jail",
            "name":         name,
            "path":         path,
            "children_max": 0,
            "mounts":       [],
            "exec": {
                "path": binary,
                "argv": argv,
                "env":  [{"key": "PATH", "value": "/bin:/usr/bin"}],
                "uid":  1001,
                "gid":  1001,
            },
        })
    else:
        print(f"unknown command: {cmd}"); sys.exit(2)

    resp, fd = recvmsg_frame(s)
    print(json.dumps(resp, indent=2))
    if fd is not None:
        print(f"[procdesc fd received: {fd}]", file=sys.stderr)
        # In a real client we'd kqueue EVFILT_PROCDESC on this fd
        # to learn when the child exits. For the smoke test, just
        # poll wait4 via os.waitpid using the pid (works because
        # we still have it from response.pid). Then close.
        if resp and resp.get("kind") == "jail_created" and resp.get("pid"):
            pid = resp["pid"]
            print(f"[waiting on pid {pid} ...]", file=sys.stderr)
            try:
                wp, status = os.waitpid(pid, 0)
                print(f"[pid {wp} exited status={status}]", file=sys.stderr)
            except OSError as e:
                print(f"[waitpid failed: {e}]", file=sys.stderr)
        os.close(fd)

if __name__ == "__main__":
    main()
