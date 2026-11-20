"""Loopback-only TLS provider for the stored local callback account.

Pass a directory containing cert.pem and key.pem. The leaf must cover the
Microsoft login, Outlook IMAP and SMTP names. Each service writes its port
to that directory. Stdout contains stage names and generations only.
"""
import base64
import json
import pathlib
import secrets
import socketserver
import ssl
import sys
import threading
import urllib.parse

ROOT = pathlib.Path(sys.argv[1])
CLIENT = "11111111-1111-4111-8111-111111111111"
lock = threading.Lock()
refresh = "demo-refresh"  # Seeded by the local callback fixture.
generation = 0
access = {}


class Peer:
    def __init__(self, sock):
        self.sock = sock
        self.reader = sock.makefile("rb")

    def line(self):
        data = self.reader.readline(8193)
        if not data.endswith(b"\r\n") or len(data) > 8192:
            raise ValueError("incomplete or oversized line")
        return data[:-2].decode("ascii")

    def put(self, data):
        self.sock.sendall(data.encode("ascii") + b"\r\n")


def token(peer):
    global refresh, generation
    if peer.line() != "POST /common/oauth2/v2.0/token HTTP/1.1":
        return
    headers = {}
    for _ in range(64):
        line = peer.line()
        if not line:
            break
        k, v = line.split(":", 1)
        headers[k.lower()] = v.strip()
    length = int(headers["content-length"])
    if not 0 < length <= 8192:
        return
    body = urllib.parse.parse_qs(peer.reader.read(length).decode("ascii"), max_num_fields=8)
    with lock:
        ok = (headers.get("host") == "login.microsoftonline.com"
              and body.get("client_id") == [CLIENT] and body.get("grant_type") == ["refresh_token"]
              and body.get("refresh_token") == [refresh])
        if ok:
            generation += 1
            refresh = secrets.token_urlsafe(32)
            bearer = secrets.token_urlsafe(32)
            access[bearer] = generation
            answer = dict(token_type="Bearer", expires_in=3600, access_token=bearer, refresh_token=refresh)
            print("rotated", generation, flush=True)
        else:
            answer = dict(error="invalid_grant")
            print("refused stale or invalid grant", flush=True)
    data = json.dumps(answer)
    peer.put("HTTP/1.1 " + ("200 OK" if ok else "400 Bad Request"))
    peer.put("Content-Type: application/json")
    peer.put("Connection: close")
    peer.put("Content-Length: " + str(len(data)))
    peer.put("")
    peer.sock.sendall(data.encode())


def login(encoded, service):
    raw = base64.b64decode(encoded, validate=True).decode("ascii")
    name = "user=ada@outlook.com\x01auth=Bearer "
    if not raw.startswith(name) or not raw.endswith("\x01\x01"):
        return False
    with lock:
        epoch = access.pop(raw[len(name):-2], None)
    if epoch is None:
        return False
    print(service, "XOAUTH2", epoch, flush=True)
    return True


def imap(peer):
    peer.put("* OK [CAPABILITY IMAP4rev1 SASL-IR AUTH=XOAUTH2] ready")
    tag, verb, mech, value = peer.line().split(" ")
    if verb != "AUTHENTICATE" or mech != "XOAUTH2" or not login(value, "IMAP"):
        peer.put(tag + " NO login refused")
        return
    peer.put(tag + " OK [CAPABILITY IMAP4rev1] authenticated")
    tag, cmd = peer.line().split(" ")
    if cmd == "NOOP":
        peer.put(tag + " OK NOOP")
        print("IMAP NOOP", flush=True)


def smtp(sock, ctx):
    p = Peer(sock)
    p.put("220 rotation fixture")
    if not p.line().startswith("EHLO "):
        return
    p.put("250-fixture")
    p.put("250 STARTTLS")
    if p.line() != "STARTTLS":
        return
    p.put("220 go ahead")
    p.reader.close()
    with ctx.wrap_socket(sock, server_side=True) as tls:
        p = Peer(tls)
        if not p.line().startswith("EHLO "):
            return
        p.put("250-fixture")
        p.put("250-AUTH XOAUTH2")
        p.put("250 SIZE 1000")
        cmd = p.line()
        if not cmd.startswith("AUTH XOAUTH2 ") or not login(cmd.split(" ")[2], "SMTP"):
            p.put("535 login refused")
            return
        p.put("235 authenticated")
        if p.line().startswith("EHLO "):
            p.put("250-fixture")
            p.put("250 SIZE 1000")
        if p.line() == "NOOP":
            p.put("250 NOOP")
            print("SMTP NOOP", flush=True)


class Listener(socketserver.ThreadingTCPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):
        print(self.service, "connection ended before protocol completed", flush=True)


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(20)
        if self.server.service == "smtp":
            smtp(self.request, self.server.ctx)
        else:
            with self.server.ctx.wrap_socket(self.request, server_side=True) as tls:
                (token if self.server.service == "token" else imap)(Peer(tls))


def context(host):
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(ROOT / "cert.pem", ROOT / "key.pem")
    ctx.set_servername_callback(lambda sock, name, ctx: None if name == host else ssl.ALERT_DESCRIPTION_UNRECOGNIZED_NAME)
    return ctx


for service, host in [("token", "login.microsoftonline.com"), ("imap", "outlook.office365.com"), ("smtp", "smtp.office365.com")]:
    server = Listener(("127.0.0.1", 0), Handler)
    server.service = service
    server.ctx = context(host)
    (ROOT / (service + ".port")).write_text(str(server.server_address[1]))
    threading.Thread(target=server.serve_forever, daemon=True).start()
threading.Event().wait()
