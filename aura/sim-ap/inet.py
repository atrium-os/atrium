#!/usr/bin/env python3
"""The Wi-Fi network's "internet" for atrium-net's N3 gate
(docs/spec/atrium-net.md §4.2), run in the AP VM:

- DNS on 10.77.0.1:53 answers probe.atrium.test with 192.0.2.80, an address
  beyond the router (the gate puts it on the AP's lo), so a probe only
  reaches it through this network;
- HTTP on port 80 answers by /tmp/inet-mode: `ok` 204, `captive` a 302 to a
  sign-in page, `dead` nothing (the request hangs past the probe's timeout).
"""
import socket, struct, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WEB = bytes([192, 0, 2, 80])


def mode():
    try:
        return open("/tmp/inet-mode").read().strip()
    except OSError:
        return "ok"


def dns():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("10.77.0.1", 53))
    while True:
        q, a = s.recvfrom(1500)
        if len(q) < 12:
            continue
        i, labels = 12, []
        while i < len(q) and q[i]:
            labels.append(q[i + 1:i + 1 + q[i]].decode("ascii", "replace").lower())
            i += 1 + q[i]
        qend = i + 5
        if qend > len(q):
            continue
        qtype = struct.unpack("!H", q[i + 1:i + 3])[0]
        hit = ".".join(labels) == "probe.atrium.test" and qtype == 1
        ans = b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 60, 4) + WEB if hit else b""
        r = q[:2] + struct.pack("!HHHHH", 0x8180 | (0 if hit else 3), 1, 1 if hit else 0, 0, 0) + q[12:qend] + ans
        s.sendto(r, a)
        print(f"dns {'.'.join(labels)} -> {'192.0.2.80' if hit else 'NXDOMAIN'}", flush=True)


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        m = mode()
        print(f"http {self.client_address[0]} {self.path} mode={m}", flush=True)
        if m == "dead":
            time.sleep(30)
            return
        if m == "captive":
            self.send_response(302)
            self.send_header("Location", "http://10.77.0.1/portal")
        else:
            self.send_response(204)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *a):
        pass


threading.Thread(target=dns, daemon=True).start()
ThreadingHTTPServer(("", 80), H).serve_forever()
