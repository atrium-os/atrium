#!/usr/bin/env python3
"""A browser's part of a VPN gateway's SAML sign-in, for the Crypta SSO gate
(test tool): what the Navigator's sign-in window does, from the host. Opens
the gateway's sign-on address, follows it to the identity provider, sends
its login form (user, password), then the provider's SAML form back to the
gateway, and prints the `acSamlv2Token` the gateway set.

Usage: crypta-sso-browser.py CA_PEM LOGIN_URL USER PASSWORD HOST=ADDR:PORT...
  HOST=ADDR:PORT  where each name:port in the URLs is reached (the guest's
                  view of the network is not the host's).
"""
import http.client
import socket
import ssl
import sys
import urllib.parse
from html.parser import HTMLParser

CA, LOGIN, USER, PASSWORD = sys.argv[1:5]
MAP = {}
for m in sys.argv[5:]:
    k, v = m.split("=", 1)
    a, p = v.rsplit(":", 1)
    MAP[k] = (a, int(p))
CTX = ssl.create_default_context(cafile=CA)
# The gate CA is a test one, without the key usage strict checks ask for.
CTX.verify_flags &= ~ssl.VERIFY_X509_STRICT
jar = {}  # host -> {name: value}
seen = []


class Conn(http.client.HTTPSConnection):
    def connect(self):
        addr = MAP.get(f"{self.host}:{self.port}", (self.host, self.port))
        s = socket.create_connection(addr, timeout=30)
        self.sock = CTX.wrap_socket(s, server_hostname=self.host)


def fetch(method, url, body=None):
    for _ in range(8):
        u = urllib.parse.urlsplit(url)
        c = Conn(u.hostname, u.port or 443)
        h = {"User-Agent": "crypta-sso-gate", "Accept": "*/*"}
        cookies = jar.get(u.hostname, {})
        if cookies:
            h["Cookie"] = "; ".join(f"{k}={v}" for k, v in cookies.items())
        if body is not None:
            h["Content-Type"] = "application/x-www-form-urlencoded"
        path = u.path + ("?" + u.query if u.query else "")
        c.request(method, path or "/", body=body, headers=h)
        r = c.getresponse()
        data = r.read()
        for k, v in r.getheaders():
            if k.lower() == "set-cookie":
                n, val = v.split(";", 1)[0].split("=", 1)
                jar.setdefault(u.hostname, {})[n.strip()] = val.strip()
                seen.append((u.hostname, n.strip(), val.strip()))
        seen_status = r.status
        if r.status in (301, 302, 303, 307, 308):
            url = urllib.parse.urljoin(url, r.getheader("Location"))
            if r.status in (301, 302, 303):
                method, body = "GET", None
            continue
        return seen_status, url, data.decode("utf-8", "replace")
    raise SystemExit("too many redirects")


class Forms(HTMLParser):
    def __init__(self):
        super().__init__()
        self.forms = []

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "form":
            self.forms.append({"action": a.get("action", ""), "method": (a.get("method") or "get").lower(), "fields": []})
        elif tag == "input" and self.forms:
            self.forms[-1]["fields"].append((a.get("type", "text").lower(), a.get("name"), a.get("value", "") or ""))


def forms(html):
    p = Forms()
    p.feed(html)
    return p.forms


def send(url, form, values):
    pairs = []
    for t, n, v in form["fields"]:
        if not n or t in ("submit", "button", "image", "reset"):
            continue
        pairs.append((n, values.get(n, v)))
    act = urllib.parse.urljoin(url, form["action"])
    return fetch("POST", act, urllib.parse.urlencode(pairs))


SAVE = __import__("os").environ.get("SSO_SAVE_DIR")


def save(name, html):
    if SAVE:
        with open(f"{SAVE}/{name}", "w") as f:
            f.write(html)


st, url, html = fetch("GET", LOGIN)
save("idp-login.html", html)
f = [x for x in forms(html) if any(t == "password" for t, _, _ in x["fields"])]
if not f:
    raise SystemExit(f"no login form at {url} ({st})")
st, url, html = send(url, f[0], {"username": USER, "password": PASSWORD})
save("idp-saml-post.html", html)
saml = [x for x in forms(html) if any(n == "SAMLResponse" for _, n, _ in x["fields"])]
if not saml:
    raise SystemExit(f"no SAML form after the login ({st} at {url})")
st, url, html = send(url, saml[0], {})
tok = [v for h, n, v in seen if n == "acSamlv2Token"]
if not tok:
    raise SystemExit(f"no token: {st} at {url}: {html[:200]}")
print(tok[-1])
