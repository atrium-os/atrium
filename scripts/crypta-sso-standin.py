#!/usr/bin/env python3
"""A stand-in AnyConnect gateway with SAML single sign-on, for the Crypta V4
SSO gate (docs/spec/vpn.md §9.3, §11). A test tool only, never shipped.

ocserv 1.2.4 has no SAML, so this stands in front of it:

- `POST /` with a `config-auth` `init` (and no session cookie) is answered
  with an `sso-v2-login` request: sign in at our `/+CSCOE+/saml/sp/login`,
  done at `/+CSCOE+/saml_ac_login.html`, the token in `acSamlv2Token`.
- `/+CSCOE+/saml/sp/login` sends the browser to the identity provider
  (Keycloak) with a SAML AuthnRequest (HTTP-Redirect binding).
- `/+CSCOE+/saml/sp/acs` takes the IdP's SAMLResponse (HTTP-POST binding):
  its status, destination, audience, recipient and validity are checked,
  and its signature with xmlsec1 against the IdP's certificate when one is
  given. A token is minted for the subject, set as `acSamlv2Token`, and the
  browser is sent to the final page.
- `POST /` with an `auth-reply` carrying `<sso-token>`: a token we minted
  in the last two minutes, used once. Then we sign in to ocserv for that
  subject (a test account per subject, its password from our own table),
  and hand the client ocserv's `complete` answer and its cookie.
- Anything else (CONNECT, a resumed session's requests) is carried to
  ocserv as it is: the tunnel is ocserv's.

It is also a GlobalProtect portal's SAML front, for the window's half of
GlobalProtect SAML (the tunnel's half is crypta-gp-packet-check.py --saml):

- `/global-protect/prelogin.esp` and `/ssl-vpn/prelogin.esp` answer with
  `saml-auth-method` REDIRECT and the IdP's address (AuthnRequest with our
  `/SAML20/SP/ACS`).
- `/SAML20/SP/ACS` checks the SAMLResponse the same way and answers the
  minted token as PAN does: `saml-auth-status`, `saml-username` and
  `prelogin-cookie` headers, and the same in an HTML comment.
- `/ssl-vpn/login.esp` (and the portal's getconfig) takes `user` and
  `prelogin-cookie`: a token we minted for that subject in the last two
  minutes, used once (`gp_token_ok`, `gp_token_refused`). What follows the
  login is not served (404).

Usage: crypta-sso-standin.py LISTEN_PORT CERT KEY PUBLIC_BASE IDP_SSO_URL
         BACKEND_PORT BACKEND_CA [IDP_CERT_PEM]
  PUBLIC_BASE   e.g. https://sso.crypta.test:447 (our own address)
  IDP_SSO_URL   the IdP's SAML endpoint (Keycloak: .../realms/corp/protocol/saml)
Prints one line per event to stdout; `GET /stats` on the port answers
counts as JSON (from 127.0.0.1 only).
"""
import base64
import html
import datetime
import json
from http.cookies import CookieError, SimpleCookie
import os
import re
import secrets
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import uuid
import xml.etree.ElementTree as ET
import zlib

PORT, CERT, KEY, PUBLIC, IDP_SSO, BACKEND_PORT, BACKEND_CA = sys.argv[1:8]
PORT, BACKEND_PORT = int(PORT), int(BACKEND_PORT)
IDP_CERT = sys.argv[8] if len(sys.argv) > 8 else None
ENTITY = PUBLIC + "/+CSCOE+/saml/sp/metadata"
ACS = PUBLIC + "/+CSCOE+/saml/sp/acs"
LOGIN = PUBLIC + "/+CSCOE+/saml/sp/login"
FINAL = PUBLIC + "/+CSCOE+/saml_ac_login.html"
GP_ACS = PUBLIC + "/SAML20/SP/ACS"
# The ocserv account each IdP subject signs in as (ocserv's own gate users).
ACCOUNTS = {"alice": ("alice", "alice-pw", "Staff")}

lock = threading.Lock()
tokens = {}   # token -> (subject, minted at)
gp_tokens = {}  # GlobalProtect prelogin-cookie -> (subject, minted at)
gp_minted = []  # every GlobalProtect token, for the gate to look for in logs
gp_binding = ["REDIRECT"]  # or POST: set from localhost by /gp-binding?POST
pending = {}  # AuthnRequest id -> the ACS it names
stats = dict.fromkeys((
    "init", "sso_login", "script_delivered", "script_posts", "script_accepted",
    "acs_ok", "acs_refused", "token_ok", "token_refused", "proxied",
    "gp_prelogin", "gp_prelogin_post", "gp_acs_ok", "gp_acs_refused", "gp_token_ok", "gp_token_refused",
), 0)


def log(*a):
    print(*a, flush=True)


def bump(k):
    with lock:
        stats[k] += 1


def read_request(f):
    head = b""
    while not head.endswith(b"\r\n\r\n"):
        c = f.read(1)
        if not c:
            return None
        head += c
        if len(head) > 65536:
            return None
    lines = head.decode("latin-1").split("\r\n")
    method, target, _ = (lines[0].split(" ") + ["", "", ""])[:3]
    headers = {}
    for l in lines[1:]:
        if ":" in l:
            k, v = l.split(":", 1)
            headers.setdefault(k.strip().lower(), v.strip())
    n = int(headers.get("content-length", "0") or 0)
    body = f.read(n) if n else b""
    return method, target, headers, head, body


def respond(f, status, headers=(), body=b"", ctype="text/html; charset=utf-8"):
    h = f"HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {len(body)}\r\n"
    for k, v in headers:
        h += f"{k}: {v}\r\n"
    f.write(h.encode() + b"\r\n" + body)
    f.flush()


def config_auth(inner, typ="auth-request"):
    return (f'<?xml version="1.0" encoding="UTF-8"?>\n<config-auth client="vpn" type="{typ}" aggregate-auth-version="2">'
            f"{inner}</config-auth>").encode()


# Optional installed Navigator gate. Each login gets a distinct proof solely
# through a same-origin script. Static HTML submission cannot reach the IdP.
JS_GATE = os.environ.get("CRYPTA_SSO_EXTERNAL_JS") == "1"
JS_PATH = "/+CSCOE+/saml/sp/continue.js"
POST_PATH = "/+CSCOE+/saml/sp/continue"
JS_COOKIE = "atriumScriptFlow"
js_flows = {}


def js_login(f):
    sid, proof = secrets.token_hex(16), secrets.token_hex(16)
    now = time.monotonic()
    with lock:
        expired = [k for k, (_, deadline) in js_flows.items() if deadline <= now]
        for k in expired:
            del js_flows[k]
        full = len(js_flows) >= 128
        if not full:
            js_flows[sid] = (proof, now + 120)
    if full:
        respond(f, "503 Service Unavailable")
        return
    html = (f'<!doctype html><title>VPN sign-in</title><form method="post" action="{POST_PATH}">'
            '<input type="hidden" name="proof"><button type="button" id="proceed">Continue to sign-in</button>'
            f'</form><script src="{JS_PATH}"></script>')
    respond(f, "200 OK", [("Cache-Control", "no-store"),
            ("Set-Cookie", f"{JS_COOKIE}={sid}; HttpOnly; Secure; Path=/; SameSite=Strict")], html.encode())


def js_continue(f, method, path, headers, body):
    if method == "POST" and path == POST_PATH:
        bump("script_posts")
    cookies = SimpleCookie()
    try:
        cookies.load(headers.get("cookie", ""))
    except CookieError:
        respond(f, "403 Forbidden")
        return
    cookie = cookies.get(JS_COOKIE)
    sid = cookie.value if cookie else ""
    with lock:
        flow = js_flows.get(sid)
        active = flow is not None and flow[1] > time.monotonic()
        if active and method == "POST" and path == POST_PATH:
            active = body == urllib.parse.urlencode({"proof": flow[0]}).encode()
            if active:
                del js_flows[sid]
    if not active:
        respond(f, "403 Forbidden")
    elif method == "GET" and path == JS_PATH:
        script = ("document.getElementById('proceed').addEventListener('click', () => {"
                  "const form = document.forms[0]; form.elements.proof.value = "
                  + json.dumps(flow[0]) + "; form.requestSubmit(); });")
        bump("script_delivered")
        respond(f, "200 OK", [("Cache-Control", "no-store")], script.encode(), "text/javascript")
        log("external sign-in script delivered")
    elif method == "POST" and path == POST_PATH:
        bump("script_accepted")
        respond(f, "303 See Other", [("Location", authn_request())])
        log("external sign-in form accepted")
    else:
        respond(f, "405 Method Not Allowed")


def sso_request():
    return config_auth(
        '<opaque is-for="sg"><tunnel-group>sso</tunnel-group><auth-handle>1</auth-handle></opaque>'
        '<auth id="main"><title>Single sign-on</title><message>Sign in with your organization</message>'
        f"<sso-v2-login>{LOGIN}?tgname=sso</sso-v2-login><sso-v2-login-final>{FINAL}</sso-v2-login-final>"
        "<sso-v2-token-cookie-name>acSamlv2Token</sso-v2-token-cookie-name><form></form></auth>")


def authn_request(acs=ACS, post=False):
    rid = "_" + uuid.uuid4().hex
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    x = (f'<samlp:AuthnRequest xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" '
         f'ID="{rid}" Version="2.0" IssueInstant="{now}" Destination="{IDP_SSO}" AssertionConsumerServiceURL="{acs}" '
         f'ProtocolBinding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST"><saml:Issuer>{ENTITY}</saml:Issuer>'
         '<samlp:NameIDPolicy Format="urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified" AllowCreate="true"/></samlp:AuthnRequest>')
    with lock:
        pending[rid] = acs
    if post:
        # The POST binding (SAML bindings §3.5): the request base64'd, not
        # deflated, in a page whose form the browser sends on load.
        req = html.escape(base64.b64encode(x.encode()).decode())
        return ('<!DOCTYPE html><html><head><title>Sign in</title></head><body onload="document.forms[0].submit()">'
                '<noscript><p>Press Continue to sign in.</p></noscript>'
                f'<form method="POST" action="{html.escape(IDP_SSO)}"><input type="hidden" name="SAMLRequest" value="{req}"/>'
                '<input type="hidden" name="RelayState" value="gp&amp;post"/><input type="submit" value="Continue"/></form>'
                '<script>if (1 < 2) document.forms[0].submit();</script></body></html>')
    c = zlib.compressobj(9, zlib.DEFLATED, -15)
    d = c.compress(x.encode()) + c.flush()
    return IDP_SSO + "?" + urllib.parse.urlencode({"SAMLRequest": base64.b64encode(d).decode(), "RelayState": "sso"})


NS = {"p": "urn:oasis:names:tc:SAML:2.0:protocol", "a": "urn:oasis:names:tc:SAML:2.0:assertion"}


def check_response(b64, acs=ACS):
    """The subject of a SAMLResponse we asked for, or why not."""
    raw = base64.b64decode(b64)
    x = ET.fromstring(raw)
    if IDP_CERT:
        # Keycloak names its signing key. Give xmlsec that lookup name for
        # the pinned fixture key; never load a key from the response.
        ds = "{http://www.w3.org/2000/09/xmldsig#}"
        signature = x.find(ds + "Signature")
        if x.tag != "{" + NS["p"] + "}Response" or signature is None:
            return None, "missing signed response"
        kid = signature.findtext(ds + "KeyInfo/" + ds + "KeyName") or ""
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", kid):
            return None, "missing signing key name"
        refs = signature.findall(ds + "SignedInfo/" + ds + "Reference")
        if not x.get("ID") or len(refs) != 1 or refs[0].get("URI") != "#" + x.get("ID"):
            return None, "signature does not cover the response"
        with tempfile.NamedTemporaryFile(suffix=".xml") as t:
            t.write(raw)
            t.flush()
            r = subprocess.run(["xmlsec1", "--verify", "--enabled-key-data", "key-name", "--pubkey-cert-pem:" + kid, IDP_CERT, "--id-attr:ID", "urn:oasis:names:tc:SAML:2.0:assertion:Assertion",
                                "--id-attr:ID", "urn:oasis:names:tc:SAML:2.0:protocol:Response", t.name], capture_output=True, text=True)
            if r.returncode != 0:
                return None, "signature: " + r.stderr.strip().splitlines()[-1] if r.stderr.strip() else "signature"
    st = x.find("p:Status/p:StatusCode", NS)
    if st is None or not st.get("Value", "").endswith(":Success"):
        return None, "status " + (st.get("Value") if st is not None else "?")
    if x.get("Destination") != acs:
        return None, "destination " + str(x.get("Destination"))
    irt = x.get("InResponseTo")
    with lock:
        if not irt or pending.pop(irt, None) != acs:
            return None, "not a request of ours"
    a = x.find("a:Assertion", NS)
    if a is None:
        return None, "no assertion"
    aud = [e.text for e in a.findall("a:Conditions/a:AudienceRestriction/a:Audience", NS)]
    if ENTITY not in aud:
        return None, f"audience {aud}"
    sc = a.find("a:Subject/a:SubjectConfirmation/a:SubjectConfirmationData", NS)
    if sc is None or sc.get("Recipient") != acs:
        return None, "recipient"
    na = sc.get("NotOnOrAfter")
    if na and datetime.datetime.fromisoformat(na.replace("Z", "+00:00")) < datetime.datetime.now(datetime.timezone.utc):
        return None, "expired"
    nid = a.find("a:Subject/a:NameID", NS)
    if nid is None or not nid.text:
        return None, "no subject"
    return nid.text.strip(), None


def backend():
    ctx = ssl.create_default_context(cafile=BACKEND_CA)
    ctx.check_hostname = False
    ctx.verify_flags &= ~ssl.VERIFY_X509_STRICT
    s = socket.create_connection(("127.0.0.1", BACKEND_PORT), timeout=20)
    return ctx.wrap_socket(s, server_hostname="gw.crypta.test")


def backend_post(conn, f, body, cookie=None):
    h = (f"POST / HTTP/1.1\r\nHost: gw.crypta.test\r\nUser-Agent: AnyConnect Linux_64 4.10.07061\r\nX-Transcend-Version: 1\r\n"
         f"X-Aggregate-Auth: 1\r\nContent-Type: application/xml\r\nContent-Length: {len(body)}\r\n")
    if cookie:
        h += f"Cookie: {cookie}\r\n"
    conn.sendall(h.encode() + b"\r\n" + body)
    line = f.readline().decode()
    hdrs = []
    n = 0
    while True:
        l = f.readline().decode().rstrip("\r\n")
        if not l:
            break
        k, v = l.split(":", 1)
        hdrs.append((k.strip(), v.strip()))
        if k.lower() == "content-length":
            n = int(v)
    return line, hdrs, f.read(n)


def backend_login(subject):
    """Sign in to ocserv as `subject`'s account; ocserv's complete answer."""
    user, pw, group = ACCOUNTS[subject]
    conn = backend()
    f = conn.makefile("rb")
    init = config_auth('<version who="vpn">v1</version><group-access>https://gw.crypta.test/</group-access>', "init")
    _, _, b = backend_post(conn, f, init)
    for _ in range(4):
        x = ET.fromstring(b)
        if x.get("type") == "complete":
            break
        auth = x.find("auth")
        opaque = x.find("opaque")
        parts = []
        for inp in auth.iter("input"):
            n = inp.get("name")
            parts.append(f"<{n}>{pw if inp.get('type') == 'password' else user}</{n}>")
        for sel in auth.iter("select"):
            parts.append(f"<{sel.get('name')}>{group}</{sel.get('name')}>")
        ob = ET.tostring(opaque).decode() if opaque is not None else ""
        reply = config_auth(f'<version who="vpn">v1</version>{ob}<auth>{"".join(parts)}</auth><group-select>{group}</group-select>', "auth-reply")
        line, hdrs, b = backend_post(conn, f, reply)
    else:
        raise RuntimeError("ocserv kept asking")
    conn.close()
    return line, hdrs, b


def proxy(cf, raw_sock, head, body):
    bump("proxied")
    b = backend()
    b.sendall(head + body)

    def pump(src_read, dst):
        try:
            while True:
                d = src_read(16384)
                if not d:
                    break
                dst.sendall(d)
        except OSError:
            pass
        finally:
            try:
                dst.shutdown(socket.SHUT_WR)
            except OSError:
                pass

    t = threading.Thread(target=pump, args=(b.recv, raw_sock), daemon=True)
    t.start()
    pump(cf.read1, b)
    t.join(timeout=5)


def serve(sock, peer):
    f = sock.makefile("rwb", buffering=0)
    rf = sock.makefile("rb")
    try:
        while True:
            r = read_request(rf)
            if r is None:
                return
            method, target, headers, head, body = r
            path = target.split("?")[0]
            if path == "/gp-minted" and peer[0] == "127.0.0.1" and method == "GET":
                with lock:
                    respond(f, "200 OK", body="\n".join(gp_minted).encode(), ctype="text/plain")
                continue
            if path == "/gp-binding" and peer[0] == "127.0.0.1" and method == "GET":
                with lock:
                    gp_binding[0] = "POST" if target.endswith("?POST") else "REDIRECT"
                respond(f, "200 OK", body=gp_binding[0].encode(), ctype="text/plain")
                continue
            if path == "/stats" and peer[0] == "127.0.0.1" and method == "GET":
                with lock:
                    respond(f, "200 OK", body=json.dumps(stats).encode(), ctype="application/json")
                continue
            if method == "POST" and path == "/" and b"<config-auth" in body:
                has_session = "webvpn=" in headers.get("cookie", "")
                if b'type="init"' in body and not has_session:
                    bump("init")
                    log("init: single sign-on asked")
                    respond(f, "200 OK", body=sso_request(), ctype="text/xml")
                    continue
                m = re.search(rb"<sso-token>([^<]*)</sso-token>", body)
                if m:
                    tok = m.group(1).decode()
                    with lock:
                        got = tokens.pop(tok, None)
                    if not got or time.time() - got[1] > 120:
                        bump("token_refused")
                        log("token refused")
                        respond(f, "200 OK", body=config_auth('<auth id="main"><title>Login</title><error id="1" param1="" param2="">Login failed.</error></auth>'), ctype="text/xml")
                        continue
                    bump("token_ok")
                    log(f"token taken for {got[0]}; signing in to ocserv")
                    line, hdrs, b = backend_login(got[0])
                    keep = [(k, v) for k, v in hdrs if k.lower() in ("set-cookie",)]
                    respond(f, line.split(" ", 1)[1].strip(), keep, b, ctype="text/xml")
                    continue
            if method == "GET" and path == "/+CSCOE+/saml/sp/login":
                bump("sso_login")
                if JS_GATE:
                    js_login(f)
                else:
                    respond(f, "302 Found", [("Location", authn_request())])
                continue
            if JS_GATE and path in (JS_PATH, POST_PATH):
                js_continue(f, method, path, headers, body)
                continue
            if method == "POST" and path == "/+CSCOE+/saml/sp/acs":
                form = urllib.parse.parse_qs(body.decode())
                subject, why = (None, "no SAMLResponse")
                if "SAMLResponse" in form:
                    try:
                        subject, why = check_response(form["SAMLResponse"][0])
                    except Exception as e:  # noqa: BLE001 - a stand-in says why
                        subject, why = None, f"unreadable: {e}"
                if not subject or subject not in ACCOUNTS:
                    bump("acs_refused")
                    log(f"acs refused: {why or subject}")
                    respond(f, "403 Forbidden", body=f"Sign-in refused: {why or 'unknown subject'}".encode())
                    continue
                tok = secrets.token_hex(24)
                with lock:
                    tokens[tok] = (subject, time.time())
                bump("acs_ok")
                log(f"acs: {subject} signed on; token minted")
                respond(f, "303 See Other", [("Set-Cookie", f"acSamlv2Token={tok}; Path=/; Secure; HttpOnly"), ("Location", FINAL)])
                continue
            if path in ("/global-protect/prelogin.esp", "/ssl-vpn/prelogin.esp"):
                bump("gp_prelogin")
                binding = gp_binding[0]
                if binding == "POST":
                    bump("gp_prelogin_post")
                req = base64.b64encode(authn_request(GP_ACS, post=binding == "POST").encode()).decode()
                respond(f, "200 OK", body=(f"<prelogin-response><status>Success</status><saml-auth-method>{binding}</saml-auth-method>"
                                           f"<saml-request>{req}</saml-request></prelogin-response>").encode(), ctype="application/xml")
                continue
            if method == "POST" and path == "/SAML20/SP/ACS":
                form = urllib.parse.parse_qs(body.decode())
                subject, why = (None, "no SAMLResponse")
                if "SAMLResponse" in form:
                    try:
                        subject, why = check_response(form["SAMLResponse"][0], GP_ACS)
                    except Exception as e:  # noqa: BLE001 - a stand-in says why
                        subject, why = None, f"unreadable: {e}"
                if not subject or subject not in ACCOUNTS:
                    bump("gp_acs_refused")
                    log(f"gp acs refused: {why or subject}")
                    respond(f, "403 Forbidden", body=f"Sign-in refused: {why or 'unknown subject'}".encode())
                    continue
                tok = secrets.token_hex(24)
                with lock:
                    gp_tokens[tok] = (subject, time.time())
                    gp_minted.append(tok)
                bump("gp_acs_ok")
                log(f"gp acs: {subject} signed on; token minted")
                respond(f, "200 OK", [("saml-auth-status", "1"), ("saml-username", subject), ("prelogin-cookie", tok)],
                        (f"<html><!-- <saml-auth-status>1</saml-auth-status><saml-username>{subject}</saml-username>"
                         f"<prelogin-cookie>{tok}</prelogin-cookie> --><body>Signed in. You can close this window.</body></html>").encode())
                continue
            if method == "POST" and path in ("/ssl-vpn/login.esp", "/global-protect/getconfig.esp"):
                form = urllib.parse.parse_qs(body.decode())
                tok, user = form.get("prelogin-cookie", [""])[0], form.get("user", [""])[0]
                with lock:
                    got = gp_tokens.pop(tok, None)
                if not got or got[0] != user or time.time() - got[1] > 120 or form.get("passwd", [""]) != [""]:
                    bump("gp_token_refused")
                    log("gp token refused")
                    respond(f, "512 Custom Error", body=b"Invalid username or password")
                    continue
                bump("gp_token_ok")
                log(f"gp token taken for {user}")
                args = ["", "gp-session-" + secrets.token_hex(8), "fixture", "office", user, "saml", "vsys", "corp",
                        "", "", "", "", "tunnel", "-1", "4100", "10.77.0.2"]
                respond(f, "200 OK", body=("<jnlp><application-desc>" + "".join(f"<argument>{a}</argument>" for a in args)
                                           + "</application-desc></jnlp>").encode(), ctype="application/xml")
                continue
            if path.startswith(("/ssl-vpn/", "/global-protect/")):
                respond(f, "404 Not Found", body=b"not served by the stand-in")
                continue
            if method == "GET" and path == "/+CSCOE+/saml_ac_login.html":
                respond(f, "200 OK", body=b"<!doctype html><title>Signed in</title><p>You are signed in. You can close this window.</p>")
                continue
            # The tunnel and anything else: ocserv's.
            log(f"to ocserv: {method} {path}")
            proxy(rf, sock, head, body)
            return
    except (OSError, ssl.SSLError, ET.ParseError, RuntimeError) as e:
        log(f"connection ended: {e}")
    finally:
        try:
            sock.close()
        except OSError:
            pass


def main():
    ctx = ssl.create_default_context(ssl.Purpose.CLIENT_AUTH)
    ctx.load_cert_chain(CERT, KEY)
    ls = socket.create_server(("127.0.0.1", PORT), reuse_port=False)
    log(f"stand-in gateway on 127.0.0.1:{PORT} as {PUBLIC}, IdP {IDP_SSO}, ocserv at :{BACKEND_PORT}")
    while True:
        s, peer = ls.accept()
        try:
            t = ctx.wrap_socket(s, server_side=True)
        except (ssl.SSLError, OSError):
            s.close()
            continue
        threading.Thread(target=serve, args=(t, peer), daemon=True).start()


if __name__ == "__main__":
    main()
