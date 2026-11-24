#!/usr/bin/env python3
"""The I6.0 fixture sites (navigator-islands.md "I6.0 qualification run").

One HTTPS server answers five names by Host header:

  app.example            the single-page app (test-assets/navigator-i6), its
                         sign-in (/login sets a session cookie), /api/me,
                         /api/items, /report (the page's check lines) and a
                         server download (/report.csv)
  api.app.example        a sibling host on the app's registrable domain
                         (/status, CORS with credentials for app.example)
  pay.example            the cross-origin payment frame
  cdn.thirdparty.example a third-party script the page must not run (D12)
  elsewhere.example      the target of the cross-origin links

Every request is one JSON line on stdout (time, host, method, path, whether
a session cookie came), and every check the page reports is a line with
"check". The run's evidence is that log: a refused request never appears.

  navigator-i6-fixture.py cert DIR        # a test CA and a leaf for the names
  navigator-i6-fixture.py serve DIR [--port 443] [--bind 0.0.0.0]
"""

import argparse
import http.server
import json
import secrets
import ssl
import subprocess
import sys
import time
from pathlib import Path

ASSETS = Path(__file__).resolve().parent.parent / "test-assets" / "navigator-i6"
NAMES = ("app.example", "api.app.example", "pay.example", "cdn.thirdparty.example", "elsewhere.example")
APP = "https://app.example"
BODY_MAX = 64 * 1024
ITEMS_MAX = 1000
TYPES = {".html": "text/html; charset=utf-8", ".js": "text/javascript", ".csv": "text/csv"}
# host -> {path: asset file}
ROUTES = {
    "app.example": {"/": "app.html", "/app.html": "app.html", "/app.js": "app.js",
                    "/second.html": "second.html", "/route/1": "app.html"},
    "pay.example": {"/frame.html": "pay-frame.html"},
    "cdn.thirdparty.example": {"/lib.js": "lib.js"},
}


def log(**fields):
    fields["t"] = round(time.time(), 3)
    print(json.dumps(fields, sort_keys=True), flush=True)


class State:
    sessions = {}  # token -> user
    items = []


class Handler(http.server.BaseHTTPRequestHandler):
    server_version = "i6-fixture"
    protocol_version = "HTTP/1.1"

    def host(self):
        h = (self.headers.get("Host") or "").lower()
        return h.rsplit(":", 1)[0] if h.count(":") == 1 else h

    def user(self):
        for part in (self.headers.get("Cookie") or "").split(";"):
            k, _, v = part.strip().partition("=")
            if k == "i6session" and v in State.sessions:
                return State.sessions[v]
        return None

    def reply(self, status, body=b"", ctype="text/plain", headers=()):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        for k, v in headers:
            self.send_header(k, v)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def body(self):
        try:
            n = int(self.headers.get("Content-Length") or "0")
        except ValueError:
            return None
        if n < 0 or n > BODY_MAX:
            return None
        return self.rfile.read(n)

    def cors(self):
        if self.headers.get("Origin") == APP:
            return (("Access-Control-Allow-Origin", APP), ("Access-Control-Allow-Credentials", "true"), ("Vary", "Origin"))
        return ()

    def log_request(self, code="-", size="-"):
        log(host=self.host(), method=self.command, path=self.path.split("?", 1)[0], status=code,
            session=self.user() is not None, origin=self.headers.get("Origin"))

    def log_message(self, *args):
        pass

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        host, path = self.host(), self.path.split("?", 1)[0]
        if host == "api.app.example" and path == "/status":
            return self.reply(200, json.dumps({"status": "api.app.example answered"}), "application/json", self.cors())
        if host == "app.example" and path == "/api/me":
            return self.reply(200, json.dumps({"user": self.user()}), "application/json")
        if host == "app.example" and path == "/report.csv":
            rows = "item\n" + "".join(f"{i}\n" for i in State.items)
            return self.reply(200, rows, "text/csv", (("Content-Disposition", 'attachment; filename="report.csv"'),))
        if host == "elsewhere.example":
            return self.reply(200, "<!doctype html><title>Elsewhere</title><h1>Another site</h1>", TYPES[".html"])
        name = ROUTES.get(host, {}).get(path)
        if not name:
            return self.reply(404, "not here")
        f = ASSETS / name
        return self.reply(200, f.read_bytes(), TYPES[f.suffix])

    def do_OPTIONS(self):
        headers = self.cors()
        if headers:
            headers += (("Access-Control-Allow-Methods", "GET, POST"), ("Access-Control-Allow-Headers", "content-type"))
        self.reply(204, b"", headers=headers)

    def do_POST(self):
        host, path = self.host(), self.path.split("?", 1)[0]
        data = self.body()
        if data is None:
            self.close_connection = True
            return self.reply(413, "too large")
        if host != "app.example":
            return self.reply(404, "not here")
        if path == "/login":
            user = "ana"
            for pair in data.decode("utf-8", "replace").split("&"):
                k, _, v = pair.partition("=")
                if k == "user" and v.isalnum() and len(v) <= 32:
                    user = v
            token = secrets.token_hex(16)
            State.sessions[token] = user
            log(check="sign-in", outcome="pass", detail=f"session for {user}")
            return self.reply(303, b"", headers=(("Location", "/app.html"),
                ("Set-Cookie", f"i6session={token}; Path=/; Secure; HttpOnly; SameSite=Lax")))
        try:
            msg = json.loads(data or b"{}")
        except ValueError:
            return self.reply(400, "not JSON")
        if not isinstance(msg, dict):
            return self.reply(400, "not an object")
        if path == "/report":
            log(check=str(msg.get("name"))[:64], outcome=str(msg.get("outcome"))[:16], detail=str(msg.get("detail", ""))[:256])
            return self.reply(204)
        if path == "/api/items":
            text = str(msg.get("text", ""))[:256]
            if len(State.items) < ITEMS_MAX:
                State.items.append(text)
            return self.reply(200, json.dumps({"count": len(State.items)}), "application/json")
        return self.reply(404, "not here")


def make_cert(out):
    out.mkdir(parents=True, exist_ok=True)
    run = lambda *a: subprocess.run(a, check=True, cwd=out, capture_output=True)
    run("openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
        "-keyout", "ca.key", "-out", "ca.pem", "-days", "30", "-subj", "/CN=Atrium I6 fixture CA",
        "-addext", "basicConstraints=critical,CA:true", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    run("openssl", "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
        "-keyout", "server.key", "-out", "server.csr", "-subj", "/CN=app.example")
    (out / "san.ext").write_text("subjectAltName=" + ",".join(f"DNS:{n}" for n in NAMES)
                                  + "\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n")
    run("openssl", "x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
        "-out", "server.pem", "-days", "30", "-extfile", "san.ext")
    print(out / "ca.pem")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("cert")
    c.add_argument("dir", type=Path)
    s = sub.add_parser("serve")
    s.add_argument("dir", type=Path)
    s.add_argument("--port", type=int, default=443)
    s.add_argument("--bind", default="0.0.0.0")
    a = p.parse_args()
    if a.cmd == "cert":
        return make_cert(a.dir)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(a.dir / "server.pem", a.dir / "server.key")
    httpd = http.server.ThreadingHTTPServer((a.bind, a.port), Handler)
    httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
    log(serving=f"{a.bind}:{a.port}", names=list(NAMES))
    httpd.serve_forever()


if __name__ == "__main__":
    sys.exit(main())
