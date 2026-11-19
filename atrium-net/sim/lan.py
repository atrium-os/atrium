#!/usr/bin/env python3
"""A fake Ethernet LAN for atrium-net's gate (docs/spec/atrium-net.md §10).

QEMU's `-netdev socket,udp=127.0.0.1:7801,localaddr=127.0.0.1:7802` puts the
guest's second NIC on this script: every guest frame arrives as one UDP
datagram on 7801 and every datagram we send to 7802 is a frame for the guest.

On the LAN (10.88.0.0/24): the router 10.88.0.1 (ARP, ping, DHCP server), a
DNS server 10.88.0.53 that answers gate.atrium.test with 10.88.0.77 and
probe.atrium.test with 198.51.100.80, and "the internet" at 198.51.100.80,
beyond the router (so a probe reaches it only through this LAN): a
minimal TCP that serves one HTTP answer per connection on port 80 (204, or a
captive portal's 302, or nothing at all).

IPv6: the router fe80::1 sends RAs (on solicitation and every 30 s) for
fd77:0:0:88::/64 with RDNSS fd77:0:0:88::53 and fe80::1; it answers neighbor
solicitations, pings and DNS (AAAA gate.atrium.test = fd77:0:0:88::77) on
fe80::1, fd77:0:0:88::1 and fd77:0:0:88::53.

Control on UDP 127.0.0.1:7803, one word per datagram, answered with JSON:
  good | hostile | silent   -- DHCP behaviour
  tunnelvision              -- good leases carrying option 121 routes 128/1,
                               1/8 and 203.0.113.0/24 via the router
  hostile6 | good6          -- a rogue router fe80::66 sends bad RAs every 2 s
  d6-off | d6-info | d6-addr -- the RAs' O / M+O flags; the DHCPv6 server on
                               fe80::1 answers Information-Requests with DNS
                               fd77:0:0:88::153, Solicit/Request/Renew/Rebind
                               with fd77:0:0:88::1:50 (T1 60 s, valid 240 s)
  d6-hostile                -- the server answers with bad messages, and every
                               second pushes one carrying the client's last
                               transaction id (so only the client's checks
                               stand between it and the host)
  flap6                     -- kernel stress: RAs every 0.2 s and unsolicited
                               NAs for fe80::1 without the Router flag
  conflict | noconflict     -- a host 02:4e:00:00:00:77 already has 10.88.0.50 and
                               answers ARP for it; after a DHCPDECLINE the server
                               offers 10.88.0.51 (noconflict: back to .50)
  web-ok | web-dead | web-captive  -- what 198.51.100.80:80 does (default ok);
                               the router's 10.88.0.1:80 serves the sign-in page
                               (GET /login: one form; POST /accept: web-ok)
  stats                     -- counters
  ac-drop                   -- reset every AnyConnect connection (a roam)
  seen                      -- (and seen-reset) every IPv4/IPv6 packet the guest
                               sent since the last reset, as {"proto dst[:port]": n}
                               (ARP, DHCP, ND and router solicitations left out):
                               the VPN gate's leak check (vpn.md §11)

A WireGuard gateway "beyond the router" at 198.51.100.51:51820: the guest's
UDP to it is relayed to 127.0.0.1:51820 (where crypta-wg-linux-vm.sh's Ubuntu
VM listens) and the answers come back from 198.51.100.51.

AnyConnect gateways at 198.51.100.52:443-446 (vpn.md §11): the guest's TCP
to them ends here, in a small TCP of this script's own, and each connection
is carried on to 127.0.0.1:8443-8446 (crypta-ocserv-vm.sh's forwards to
ocserv in the Ubuntu VM). Ports 447 and 448 go to 127.0.0.1:8447-8448: the
SAML stand-in gateway and Keycloak (crypta-sso-stand.sh). The same two are
also at 172.16.47.1:447-448, a private address beyond the router, which Arca's
couriers may reach under their test grant (a documentation address they may
not), for the sign-in window's pages.
The Crypta gate can also attach a GlobalProtect TLS gateway (198.51.100.52:449)
and a separate portal (198.51.100.54:450) to loopback fixture listeners.
"""
import json, os, random, socket, struct, sys

LAN = ("127.0.0.1", 7801)
GUEST = ("127.0.0.1", 7802)
CTL = ("127.0.0.1", 7803)
RMAC = bytes.fromhex("024e000000fe")
ROUTER = bytes([10, 88, 0, 1])
DNS = bytes([10, 88, 0, 53])
YOU = bytes([10, 88, 0, 50])
YOU2 = bytes([10, 88, 0, 51])
CMAC = bytes.fromhex("024e00000077")
conflict = False
declined = False


def you():
    return YOU2 if declined else YOU

WEB = bytes([198, 51, 100, 80])  # beyond the router
WG_GW = bytes([198, 51, 100, 51])  # a WireGuard gateway, also beyond it
WG_PORT = 51820
WG_RELAY = ("127.0.0.1", 51820)
AC_GW = bytes([198, 51, 100, 52])  # AnyConnect gateways, beyond the router
AC_PORTS = {443: 8443, 444: 8444, 445: 8445, 446: 8446, 447: 8447, 448: 8448}
if "CRYPTA_GP_TEST_PORT" in os.environ:
    AC_PORTS[449] = int(os.environ["CRYPTA_GP_TEST_PORT"])
SSO_GW = bytes([172, 16, 47, 1])  # the SAML stand-in and Keycloak, privately
GW_PORTS = {AC_GW: AC_PORTS, SSO_GW: {447: 8447, 448: 8448}}
if "CRYPTA_GP_PORTAL_PORT" in os.environ:
    GW_PORTS[bytes([198, 51, 100, 54])] = {450: int(os.environ["CRYPTA_GP_PORTAL_PORT"])}
GP_ESP_PORT = 4501
GP_ESP_RELAY = ("127.0.0.1", int(os.environ["CRYPTA_GP_TEST_ESP"])) if "CRYPTA_GP_TEST_ESP" in os.environ else None
gp_peer = None
wg_peer = None  # (guest mac, guest ip, guest port) of the last datagram relayed
seen = {}
LEASE = int(sys.argv[1]) if len(sys.argv) > 1 else 120
MAGIC = bytes([99, 130, 83, 99])

mode = "good"
net_mtu = None  # the network's MTU, in DHCP option 26 and the RAs (`mtu N`, `mtu off`)
stats = {"discover": 0, "request": 0, "renew": 0, "rebind": 0, "reboot": 0,
         "hostile_sent": 0, "dns": 0, "ping": 0, "arp": 0, "frames": 0,
         "decline": 0, "arp_conflict": 0,
         "rs": 0, "ra": 0, "ns": 0, "ping6": 0, "dns6": 0, "hostile6_sent": 0,
         "d6_solicit": 0, "d6_request": 0, "d6_renew": 0, "d6_rebind": 0, "d6_info": 0,
         "d6_hostile_sent": 0, "mdns_query": 0, "mdns_answer": 0, "mdns_a": 0}
mode6 = "good6"
d6 = "d6-off"
d6_last = None  # (client mac, client ll, xid, client id, iaid) of the last client message
web = "web-ok"
tcp = {}  # (client ip, port) -> bytes of our answer already sent
rnd = random.Random(7)


def csum(b):
    if len(b) % 2:
        b += b"\0"
    s = sum(struct.unpack("!%dH" % (len(b) // 2), b))
    while s >> 16:
        s = (s & 0xFFFF) + (s >> 16)
    return (~s) & 0xFFFF


def ip(src, dst, proto, payload):
    h = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), 0, 0, 64, proto, 0, src, dst)
    h = h[:10] + struct.pack("!H", csum(h)) + h[12:]
    return h + payload


def udp(src, dst, sport, dport, payload):
    u = struct.pack("!HHHH", sport, dport, 8 + len(payload), 0) + payload
    pseudo = src + dst + struct.pack("!BBH", 0, 17, len(u))
    c = csum(pseudo + u) or 0xFFFF
    return u[:6] + struct.pack("!H", c) + u[8:]


def eth(dst, src, typ, payload):
    return dst + src + struct.pack("!H", typ) + payload


def opts(pairs):
    out = b""
    for c, v in pairs:
        out += bytes([c, len(v)]) + v
    return out + b"\xff"


def dhcp_reply(req, kind, extra=None, yi=None, mangle=None):
    yi = yi or you()
    xid, chaddr = req[4:8], req[28:44]
    b = bytes([2, 1, 6, 0]) + xid + b"\0\0\0\0" + b"\0" * 4 + yi + ROUTER + b"\0" * 4 + chaddr
    b += b"\0" * 192 + MAGIC
    o = [(53, bytes([kind])), (54, ROUTER)]
    if kind != 6:
        o += [(1, bytes([255, 255, 255, 0])), (3, ROUTER), (6, DNS), (51, struct.pack("!I", LEASE))]
        if net_mtu:
            o.append((26, struct.pack("!H", net_mtu)))
    if extra:
        o = [x for x in o if x[0] not in {k for k, _ in extra}] + extra
    b += opts(o)
    if mangle:
        b = mangle(b)
    return b


HOSTILE = [
    ("router off-subnet", dict(extra=[(3, bytes([8, 8, 8, 8]))])),
    ("loopback address", dict(yi=bytes([127, 0, 0, 5]))),
    ("app-range address", dict(yi=bytes([100, 64, 1, 2]))),
    ("route over the app range", dict(extra=[(121, bytes([10, 100, 64, 10, 88, 0, 1]))])),
    ("route to loopback", dict(extra=[(121, bytes([8, 127, 10, 88, 0, 1]))])),
    ("four DNS servers", dict(extra=[(6, DNS * 4)])),
    ("5 s lease", dict(extra=[(51, struct.pack("!I", 5))])),
    ("prefix /31", dict(extra=[(1, bytes([255, 255, 255, 254]))])),
    ("prefix /4", dict(extra=[(1, bytes([240, 0, 0, 0]))])),
    ("non-contiguous mask", dict(extra=[(1, bytes([255, 0, 255, 0]))])),
    ("MTU 100", dict(extra=[(26, struct.pack("!H", 100))])),
    ("broadcast address", dict(yi=bytes([10, 88, 0, 255]))),
    ("DNS on loopback", dict(extra=[(6, bytes([127, 0, 0, 1]))])),
    ("truncated", dict(mangle=lambda b: b[: rnd.randrange(240, len(b))])),
    ("mutated", dict(mangle=lambda b: bytes(x if rnd.random() > 0.02 else rnd.randrange(256) for x in b))),
    ("option overload", dict(extra=[(52, bytes([3]))])),
    ("repeated option", dict(mangle=lambda b: b[:-1] + bytes([54, 4]) + DNS + b"\xff")),
]


# TunnelVision (CVE-2024-3661): a lease whose classless static routes
# (option 121) are as or more specific than a VPN's 0/1 + 128/1: 128/1,
# 1/8 and 203.0.113.0/24 (none over the app range, which atrium-net refuses
# outright).
TV = [(121, bytes([0]) + ROUTER + bytes([1, 128]) + ROUTER + bytes([8, 1]) + ROUTER
       + bytes([24, 203, 0, 113]) + ROUTER)]


def handle_dhcp(src_mac, req, dst=b"\xff" * 4):
    if len(req) < 240 or req[0] != 1 or req[236:240] != MAGIC:
        return []
    o, i = {}, 240
    while i < len(req) and req[i] != 255:
        if req[i] == 0:
            i += 1
            continue
        o[req[i]] = req[i + 2:i + 2 + req[i + 1]]
        i += 2 + req[i + 1]
    global declined
    kind = o.get(53, b"\0")[0]
    ciaddr = req[12:16]
    if kind == 4:
        stats["decline"] += 1
        stats["last_decline"] = socket.inet_ntoa(o.get(50, b"\0" * 4))
        if o.get(50) == YOU:
            declined = True
        return []
    if kind == 1:
        stats["discover"] += 1
    elif kind == 3:
        if ciaddr != b"\0\0\0\0" and 54 in o:
            stats["request"] += 1
        elif ciaddr != b"\0\0\0\0":
            # RFC 2131 4.4.5: a renewal is unicast to the server; the same
            # REQUEST broadcast is a rebind (the renewal went unanswered).
            stats["renew" if dst == ROUTER else "rebind"] += 1
        elif 54 in o:
            stats["request"] += 1
        else:
            stats["reboot"] += 1
    if mode == "silent":
        return []
    if mode == "hostile":
        name, kw = HOSTILE[stats["hostile_sent"] % len(HOSTILE)]
        stats["hostile_sent"] += 1
        stats["last_hostile"] = name
        r = dhcp_reply(req, 2 if kind == 1 else 5, **kw)
    elif kind == 1:
        r = dhcp_reply(req, 2, extra=TV if mode == "tunnelvision" else None)
    elif kind == 3:
        want = o.get(50, ciaddr)
        r = dhcp_reply(req, 5 if want == you() else 6, extra=TV if mode == "tunnelvision" else None)
    else:
        return []
    to = b"\xff" * 4 if ciaddr == b"\0\0\0\0" else ciaddr
    return [eth(src_mac, RMAC, 0x0800, ip(ROUTER, to, 17, udp(ROUTER, to, 67, 68, r)))]


def dns_answer(q):
    # One question; A for gate.atrium.test, NXDOMAIN otherwise.
    if len(q) < 12:
        return None
    qid, flags, qd = struct.unpack("!HHH", q[:6])
    i, labels = 12, []
    while i < len(q) and q[i]:
        labels.append(q[i + 1:i + 1 + q[i]].decode("ascii", "replace").lower())
        i += 1 + q[i]
    qend = i + 5
    qtype = struct.unpack("!H", q[i + 1:i + 3])[0] if qend <= len(q) else 0
    name = ".".join(labels)
    question = q[12:qend]
    hit = (name == "gate.atrium.test" and qtype in (1, 28)) or (name == "probe.atrium.test" and qtype == 1)
    rflags = 0x8180 | (0 if name.endswith("atrium.test") else 3)
    if hit and qtype == 1:
        ans = b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 60, 4) + (WEB if name.startswith("probe") else bytes([10, 88, 0, 77]))
    elif hit:
        ans = b"\xc0\x0c" + struct.pack("!HHIH", 28, 1, 60, 16) + V6_HOST
    else:
        ans = b""
    return struct.pack("!HHHHHH", qid, rflags, 1, 1 if hit else 0, 0, 0) + question + ans


# ---- mDNS: a printer on the LAN (printing.md §7.1, atrium-net print-discovery) ----
# Control words: `mdns-printer IP PORT` answers _ipp/_ipps PTR queries with
# PTR+SRV+TXT+A for one printer ("Gate Printer" at IP:PORT), unicast to the
# asker when the question has the QU bit, else to 224.0.0.251; `mdns-flood`
# answers each query with a burst of hostile answers; `mdns-off` is silent.
MDNS_SRC = bytes([10, 88, 0, 60])
MDNS_MAC = bytes.fromhex("024e0000003c")
MDNS_GROUP = bytes([224, 0, 0, 251])
mdns = "mdns-off"
mdns_printer = (bytes([10, 0, 2, 2]), 8661)


def dname(*labels):
    return b"".join(bytes([len(l)]) + l for l in labels) + b"\0"


def rr(name, typ, ttl, rdata, flush=True):
    return name + struct.pack("!HHIH", typ, 0x8001 if flush else 1, ttl, len(rdata)) + rdata


def mdns_questions(m):
    """(service, QU) for each question naming _ipp._tcp.local or _ipps._tcp.local."""
    if len(m) < 12:
        return []
    qd = struct.unpack("!H", m[4:6])[0]
    i, out = 12, []
    for _ in range(min(qd, 8)):
        labels = []
        while i < len(m) and m[i] and m[i] < 64:
            labels.append(m[i + 1:i + 1 + m[i]].lower())
            i += 1 + m[i]
        i += 1
        if i + 4 > len(m):
            break
        qt, qc = struct.unpack("!HH", m[i:i + 4])
        i += 4
        if qt in (12, 255) and labels[1:] == [b"_tcp", b"local"] and labels[:1] in ([b"_ipp"], [b"_ipps"]):
            out.append((labels[0], bool(qc & 0x8000)))
    return out


def mdns_answer(svc):
    ip_, port = mdns_printer
    inst = dname(b"Gate Printer", svc, b"_tcp", b"local")
    host = dname(b"gateprinter", b"local")
    txt = b"".join(bytes([len(t)]) + t for t in [b"txtvers=1", b"rp=ipp/print", b"pdl=application/pdf,image/urf", b"UUID=6d5f3c8e-0d4a-4f43-9a8e-61a7f1b8c0de"])
    ans = [rr(dname(svc, b"_tcp", b"local"), 12, 4500, inst, flush=False),
           rr(inst, 33, 120, struct.pack("!HHH", 0, 0, port) + host),
           rr(inst, 16, 4500, txt),
           rr(host, 1, 120, ip_)]
    return struct.pack("!HHHHHH", 0, 0x8400, 0, len(ans), 0, 0) + b"".join(ans)


def mdns_flood():
    """About 1000 hostile answers: distinct instances, pointer loops,
    oversized names and TXT, TTL-0 churn, records claiming more than they hold."""
    out = []
    ptr = dname(b"_ipp", b"_tcp", b"local")
    for k in range(1000):
        kind = k % 6
        if kind == 0:  # many distinct instances
            body = rr(ptr, 12, 4500, dname(b"Junk %d" % k, b"_ipp", b"_tcp", b"local"), flush=False)
            m = struct.pack("!HHHHHH", 0, 0x8400, 0, 1, 0, 0) + body
        elif kind == 1:  # a name that points at itself
            m = struct.pack("!HHHHHH", 0, 0x8400, 0, 1, 0, 0) + b"\xc0\x0c" + struct.pack("!HHIH", 12, 1, 120, 2) + b"\xc0\x0c"
        elif kind == 2:  # labels that run past the end, 63-byte labels x 5
            m = struct.pack("!HHHHHH", 0, 0x8400, 0, 1, 0, 0) + b"".join(bytes([63]) + b"x" * 63 for _ in range(5)) + bytes([63]) + b"y" * 10
        elif kind == 3:  # TXT of ~1400 bytes in 255-byte strings
            t = b"".join(bytes([255]) + bytes([0x41 + (k % 26)]) * 255 for _ in range(5))
            m = struct.pack("!HHHHHH", 0, 0x8400, 0, 1, 0, 0) + rr(dname(b"Junk %d" % (k - 3), b"_ipp", b"_tcp", b"local"), 16, 120, t)
        elif kind == 4:  # TTL 0: goodbye for an instance just announced
            m = struct.pack("!HHHHHH", 0, 0x8400, 0, 1, 0, 0) + rr(ptr, 12, 0, dname(b"Junk %d" % (k - 4), b"_ipp", b"_tcp", b"local"), flush=False)
        else:  # counts far beyond the records, and a record longer than the message
            m = struct.pack("!HHHHHH", 0, 0x8400, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF) + ptr + struct.pack("!HHIH", 12, 1, 120, 0xFFFF)
        out.append(m)
    return out


# A queries for .local names (atrium-mdns, spec §6.1), always answered:
# gatebox.local is a host on this LAN; evil.local answers only addresses a
# .local name may not have (off the link, loopback, broadcast); loopy.local
# answers with a name that points at itself.
MDNS_HOSTS = {b"gatebox": [bytes([10, 88, 0, 77])],
              b"evil": [bytes([8, 8, 8, 8]), bytes([127, 0, 0, 1]), bytes([10, 88, 0, 255])]}
# AAAA: gatebox has an address in the LAN's /64; evil only loopback,
# documentation, link-local and the prefix's anycast address.
MDNS_HOSTS6 = {b"gatebox": [socket.inet_pton(socket.AF_INET6, "fd77:0:0:88::77")],
               b"evil": [socket.inet_pton(socket.AF_INET6, x) for x in ("::1", "2001:db8::1", "fe80::1", "fd77:0:0:88::")]}


def mdns_a_question(m):
    """(host label, QU, qtype) for a single-question A or AAAA query for
    HOST.local."""
    if len(m) < 12 or struct.unpack("!H", m[4:6])[0] != 1:
        return None
    i, labels = 12, []
    while i < len(m) and m[i] and m[i] < 64:
        labels.append(m[i + 1:i + 1 + m[i]].lower())
        i += 1 + m[i]
    i += 1
    if i + 4 > len(m) or len(labels) != 2 or labels[1] != b"local":
        return None
    qt, qc = struct.unpack("!HH", m[i:i + 4])
    return (labels[0], bool(qc & 0x8000), qt) if qt in (1, 28) else None


def mdns_a_answer(host, qt=1):
    hdr = lambda n: struct.pack("!HHHHHH", 0, 0x8400, 0, n, 0, 0)
    if host == b"loopy":
        return hdr(1) + b"\xc0\x0c" + struct.pack("!HHIH", 1, 0x8001, 120, 4) + bytes([10, 88, 0, 78])
    addrs = (MDNS_HOSTS6 if qt == 28 else MDNS_HOSTS).get(host)
    if not addrs:
        return None
    n = dname(host, b"local")
    return hdr(len(addrs)) + b"".join(rr(n, qt, 120, a) for a in addrs)


def handle_mdns(src_mac, s, sport, m):
    aq = mdns_a_question(m)
    if aq:
        stats["mdns_a"] += 1
        a = mdns_a_answer(aq[0], aq[2])
        if not a:
            return []
        if aq[1] or sport != 5353:  # QU, or a one-shot query from its own port
            return [eth(src_mac, MDNS_MAC, 0x0800, ip(MDNS_SRC, s, 17, udp(MDNS_SRC, s, 5353, sport, a)))]
        return [eth(bytes.fromhex("01005e0000fb"), MDNS_MAC, 0x0800, ip(MDNS_SRC, MDNS_GROUP, 17, udp(MDNS_SRC, MDNS_GROUP, 5353, 5353, a)))]
    qs = mdns_questions(m)
    if not qs or mdns == "mdns-off":
        return []
    stats["mdns_query"] += 1
    qu = any(u for _, u in qs)
    if mdns == "mdns-flood":
        msgs = mdns_flood()
    else:
        msgs = [mdns_answer(svc) for svc, _ in qs]
    frames = []
    for a in msgs:
        stats["mdns_answer"] += 1
        if qu:
            frames.append(eth(src_mac, MDNS_MAC, 0x0800, ip(MDNS_SRC, s, 17, udp(MDNS_SRC, s, 5353, sport, a))))
        else:
            frames.append(eth(bytes.fromhex("01005e0000fb"), MDNS_MAC, 0x0800, ip(MDNS_SRC, MDNS_GROUP, 17, udp(MDNS_SRC, MDNS_GROUP, 5353, 5353, a))))
    return frames


# ---- "the internet": one HTTP answer per TCP connection ----------------------

def tcp_seg(s, d, sport, dport, seq, ack, flags, data=b""):
    h = struct.pack("!HHIIBBHHH", sport, dport, seq, ack, 5 << 4, flags, 65535, 0, 0) + data
    c = csum(s + d + struct.pack("!BBH", 0, 6, len(h)) + h)
    return ip(s, d, 6, h[:16] + struct.pack("!H", c) + h[18:])


PORTAL_PAGE = (b"<!doctype html><title>Gate Cafe Wi-Fi</title><h1>Gate Cafe Wi-Fi</h1>"
               b"<form method=post action=/accept><input type=hidden name=terms value=yes>"
               b"<button type=submit>Accept and connect</button></form>")
DONE_PAGE = b"<!doctype html><title>Connected</title><p>You are online."


def page(code, body, extra=b""):
    return (b"HTTP/1.1 " + code + b"\r\nContent-Type: text/html\r\n" + extra +
            b"Content-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body)


def portal_answer(req):
    """The router's own sign-in page (10.88.0.1:80): GET /login shows one
    form; POST /accept lets the client through (web-ok)."""
    global web
    if req.startswith(b"POST /accept"):
        stats["portal_accept"] = stats.get("portal_accept", 0) + 1
        web = "web-ok"
        return page(b"200 OK", DONE_PAGE)
    if req.startswith(b"GET /login") or req.startswith(b"GET / "):
        stats["portal_page"] = stats.get("portal_page", 0) + 1
        return page(b"200 OK", PORTAL_PAGE)
    return page(b"404 Not Found", b"")


def http_answer():
    if web == "web-captive":
        return (b"HTTP/1.1 302 Found\r\nLocation: http://10.88.0.1/login?from=atrium\r\n"
                b"Content-Length: 0\r\nConnection: close\r\n\r\n")
    return b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"


def handle_tcp(src_mac, s, body, srv=WEB):
    if len(body) < 20 or (web == "web-dead" and srv == WEB):
        return []
    sport, dport, seq, ack, off, flags = struct.unpack("!HHIIBB", body[:14])
    if dport != 80:
        return [eth(src_mac, RMAC, 0x0800, tcp_seg(srv, s, 80, sport, 0, seq + 1, 0x14))]  # RST
    data = body[(off >> 4) * 4:]
    key = (s, sport)
    out = []
    if flags & 0x02:  # SYN
        stats["web_syn"] = stats.get("web_syn", 0) + 1
        tcp[key] = None
        out.append(tcp_seg(srv, s, 80, sport, 1000, seq + 1, 0x12))
    elif data and key in tcp and tcp[key] is None:
        if srv == WEB:
            a = http_answer()
            stats["web_get"] = stats.get("web_get", 0) + 1
        else:
            a = portal_answer(data)
        tcp[key] = a
        # Data, then FIN with it: the client reads to the end.
        out.append(tcp_seg(srv, s, 80, sport, 1001, seq + len(data), 0x18, a))
        out.append(tcp_seg(srv, s, 80, sport, 1001 + len(a), seq + len(data), 0x11))
    elif flags & 0x01 and key in tcp:  # FIN
        a = tcp.pop(key) or b""
        out.append(tcp_seg(srv, s, 80, sport, 1002 + len(a), seq + len(data) + 1, 0x10))
    return [eth(src_mac, RMAC, 0x0800, o) for o in out]


# ---- AnyConnect: TCP carried to the host --------------------------------------

M32 = 1 << 32
ac = {}  # (guest ip, guest port, our port) -> Ac


def after(a, b):
    """a is later than b, in sequence space."""
    return 0 < (a - b) % M32 < M32 // 2


class Ac:
    """One guest connection, ended here and carried to the host's port."""

    def __init__(self, gmac, gip, gport, dport, isn, gw=AC_GW):
        import time
        self.gmac, self.gip, self.gport, self.dport, self.gw = gmac, gip, gport, dport, gw
        self.rcv = (isn + 1) % M32
        self.una = self.nxt = random.getrandbits(32)
        self.buf = b""  # from una: sent and not acknowledged, then not sent
        self.wnd = 65535
        self.host_done = self.fin_sent = self.guest_done = False
        self.sent_at = time.monotonic()
        self.host = socket.create_connection(("127.0.0.1", GW_PORTS[gw][dport]), timeout=5)
        self.host.setblocking(False)

    def seg(self, seq, flags, data=b""):
        return eth(self.gmac, RMAC, 0x0800, tcp_seg(self.gw, self.gip, self.dport, self.gport, seq % M32, self.rcv, flags, data))

    def flush(self):
        import time
        out = []
        while True:
            off = (self.nxt - self.una) % M32
            room = self.wnd - off
            chunk = self.buf[off:off + min(1360, max(room, 0))]
            if not chunk:
                break
            out.append(self.seg(self.nxt, 0x18, chunk))
            self.nxt = (self.nxt + len(chunk)) % M32
        if self.host_done and not self.fin_sent and (self.nxt - self.una) % M32 == len(self.buf):
            out.append(self.seg(self.nxt, 0x11))
            self.nxt = (self.nxt + 1) % M32
            self.fin_sent = True
        if out:
            self.sent_at = time.monotonic()
        return out

    def guest(self, body):
        sport, dport, seq, ack, off, flags = struct.unpack("!HHIIBB", body[:14])
        self.wnd = struct.unpack("!H", body[14:16])[0]
        data = body[(off >> 4) * 4:]
        out = []
        if flags & 0x04:  # RST
            self.close()
            return out
        if flags & 0x10 and after(ack, self.una) and not after(ack, self.nxt):
            n = (ack - self.una) % M32
            self.buf = self.buf[n:]
            self.una = ack
        if data or flags & 0x01:
            if seq == self.rcv:
                if data:
                    stats["ac_out"] = stats.get("ac_out", 0) + len(data)
                    try:
                        self.host.sendall(data)
                    except OSError:
                        self.host_done = True
                    self.rcv = (self.rcv + len(data)) % M32
                if flags & 0x01 and not self.guest_done:
                    self.guest_done = True
                    self.rcv = (self.rcv + 1) % M32
                    try:
                        self.host.shutdown(socket.SHUT_WR)
                    except OSError:
                        pass
            out.append(self.seg(self.nxt, 0x10))
        out += self.flush()
        if self.guest_done and self.fin_sent and self.una == self.nxt:
            self.close()
        return out

    def from_host(self):
        try:
            d = self.host.recv(65536)
        except BlockingIOError:
            return []
        except OSError:
            d = b""
        if d:
            stats["ac_in"] = stats.get("ac_in", 0) + len(d)
            self.buf += d
        else:
            self.host_done = True
        return self.flush()

    def again(self, now):
        """Go back to the first byte not acknowledged, after a second."""
        if self.una != self.nxt and now - self.sent_at > 1.0:
            if self.fin_sent and (self.nxt - self.una) % M32 == len(self.buf) + 1:
                self.fin_sent = False
            self.nxt = self.una
            return self.flush()
        return []

    def close(self):
        try:
            self.host.close()
        except OSError:
            pass
        for k, v in list(ac.items()):
            if v is self:
                del ac[k]


def handle_ac(src_mac, s, body, gw=AC_GW):
    if len(body) < 20:
        return []
    sport, dport, seq, ack, off, flags = struct.unpack("!HHIIBB", body[:14])
    key = (s, sport, dport, gw)
    c = ac.get(key)
    if flags & 0x02 and not flags & 0x10:  # SYN
        if c is not None:
            c.close()
        if dport not in GW_PORTS[gw]:
            return [eth(src_mac, RMAC, 0x0800, tcp_seg(gw, s, dport, sport, 0, (seq + 1) % M32, 0x14))]
        try:
            c = Ac(src_mac, s, sport, dport, seq, gw)
        except OSError:
            return [eth(src_mac, RMAC, 0x0800, tcp_seg(gw, s, dport, sport, 0, (seq + 1) % M32, 0x14))]
        stats["ac_syn"] = stats.get("ac_syn", 0) + 1
        ac[key] = c
        f = c.seg(c.nxt, 0x12)
        c.nxt = (c.nxt + 1) % M32
        c.una = c.nxt
        return [f]
    if c is None:
        if flags & 0x04:
            return []
        return [eth(src_mac, RMAC, 0x0800, tcp_seg(gw, s, dport, sport, ack, (seq + len(body) - (off >> 4) * 4) % M32, 0x14))]
    return c.guest(body)


# ---- IPv6 -------------------------------------------------------------------

def a6(s):
    return socket.inet_pton(socket.AF_INET6, s)


R_LL = a6("fe80::1")
ROGUE_LL = a6("fe80::66")
R_GLOBAL = a6("fd77:0:0:88::1")
DNS6 = a6("fd77:0:0:88::53")
V6_HOST = a6("fd77:0:0:88::77")
PREFIX = a6("fd77:0:0:88::")
ALL_NODES = a6("ff02::1")
D6_DNS = a6("fd77:0:0:88::153")
D6_ADDR = a6("fd77:0:0:88::1:50")
MINE6 = (R_LL, R_GLOBAL, DNS6, D6_DNS)


def ip6(src, dst, nxt, payload, hlim=255):
    return struct.pack("!IHBB", 0x60000000, len(payload), nxt, hlim) + src + dst + payload


def l4sum6(src, dst, nxt, body):
    return csum(src + dst + struct.pack("!IxxxB", len(body), nxt) + body)


def icmp6(src, dst, typ, code, rest, hlim=255, bad_sum=False):
    m = bytes([typ, code, 0, 0]) + rest
    c = l4sum6(src, dst, 58, m) ^ (1 if bad_sum else 0)
    return ip6(src, dst, 58, m[:2] + struct.pack("!H", c) + m[4:], hlim)


def mcast_mac(a):
    return b"\x33\x33" + a[12:]


def rdnss(life, servers):
    return bytes([25, 1 + 2 * len(servers), 0, 0]) + struct.pack("!I", life) + b"".join(servers)


def good_ra():
    pi = bytes([3, 4, 64, 0xC0]) + struct.pack("!III", 86400, 14400, 0) + PREFIX
    mtu = bytes([5, 1, 0, 0]) + struct.pack("!I", net_mtu or 1500)
    sll = bytes([1, 1]) + RMAC
    flags = {"d6-info": 0x40, "d6-addr": 0xC0, "d6-hostile": 0xC0}.get(d6, 0)
    body = bytes([64, flags]) + struct.pack("!HII", 1800, 0, 0) + pi + mtu + sll + rdnss(600, [DNS6, R_LL])
    stats["ra"] += 1
    return eth(mcast_mac(ALL_NODES), RMAC, 0x86DD, icmp6(R_LL, ALL_NODES, 134, 0, body))


BAD6 = ["::1", "ff02::1", "::ffff:10.88.0.53", "2002:a58:35::1", "fd66::bad"]


def rogue(opts, src=ROGUE_LL, hlim=255, bad_sum=False, cut=None):
    body = bytes([64, 0]) + struct.pack("!HII", 0, 0, 0) + opts
    p = icmp6(src, ALL_NODES, 134, 0, body, hlim=hlim, bad_sum=bad_sum)
    if cut:
        p = p[:40 + cut]
    return eth(mcast_mac(ALL_NODES), b"\x02\x4e\x00\x00\x00\x66", 0x86DD, p)


HOSTILE6 = [
    ("RDNSS loopback", lambda: rogue(rdnss(600, [a6("::1")]))),
    ("RDNSS multicast", lambda: rogue(rdnss(600, [a6("ff02::1")]))),
    ("RDNSS IPv4-mapped", lambda: rogue(rdnss(600, [a6("::ffff:10.88.0.53")]))),
    ("RDNSS 6to4", lambda: rogue(rdnss(600, [a6("2002:a58:35::1")]))),
    ("zero-length option", lambda: rogue(bytes([25, 0, 0, 0, 0, 0, 0, 0]) + rdnss(600, [a6("fd66::bad")]))),
    ("RDNSS even length", lambda: rogue(bytes([25, 2, 0, 0]) + struct.pack("!I", 600) + a6("fd66::bad")[:8])),
    ("option past the end", lambda: rogue(bytes([25, 9, 0, 0]) + struct.pack("!I", 600) + a6("fd66::bad"))),
    ("hop limit 64", lambda: rogue(rdnss(600, [a6("fd66::bad")]), hlim=64)),
    ("global source", lambda: rogue(rdnss(600, [a6("fd66::bad")]), src=a6("fd66::66"))),
    ("bad checksum", lambda: rogue(rdnss(600, [a6("fd66::bad")]), bad_sum=True)),
    ("truncated", lambda: rogue(rdnss(600, [a6("fd66::bad")]), cut=30)),
    ("thirteen servers", lambda: rogue(rdnss(600, [a6("fd66::%x" % i) for i in range(13)]))),
]


def hostile6():
    name, mk = HOSTILE6[stats["hostile6_sent"] % len(HOSTILE6)]
    stats["hostile6_sent"] += 1
    stats["last_hostile6"] = name
    return mk()


def frame6(src_mac, p):
    if len(p) < 40 or p[0] >> 4 != 6:
        return []
    plen, nxt = struct.unpack("!HB", p[4:7])
    s, d, body = p[8:24], p[24:40], p[40:40 + plen]
    if nxt == 58 and len(body) >= 4:
        typ = body[0]
        if typ == 133:
            stats["rs"] += 1
            return [good_ra()]
        if typ == 135 and len(body) >= 24 and body[8:24] in MINE6 and s != bytes(16):
            stats["ns"] += 1
            t = body[8:24]
            # Router, Solicited, Override: without R a host drops us as its router.
            na = struct.pack("!I", 0xE0000000) + t + bytes([2, 1]) + RMAC
            return [eth(src_mac, RMAC, 0x86DD, icmp6(t, s, 136, 0, na))]
        if typ == 128 and d in MINE6:
            stats["ping6"] += 1
            return [eth(src_mac, RMAC, 0x86DD, icmp6(d, s, 129, 0, body[4:], hlim=64))]
    if nxt == 17 and len(body) >= 8 and d == ALL_D6 and s[:2] == b"\xfe\x80" and body[:4] == b"\x02\x22\x02\x23":
        return dhcp6(src_mac, s, body[8:struct.unpack("!H", body[4:6])[0]])
    if nxt == 17 and len(body) >= 8 and d in MINE6:
        sport, dport = struct.unpack("!HH", body[:4])
        if dport == 53:
            stats["dns6"] += 1
            a = dns_answer(body[8:])
            if a:
                u = struct.pack("!HHHH", 53, sport, 8 + len(a), 0) + a
                c = l4sum6(d, s, 17, u) or 0xFFFF
                u = u[:6] + struct.pack("!H", c) + u[8:]
                return [eth(src_mac, RMAC, 0x86DD, ip6(d, s, 17, u, hlim=64))]
    return []


# ---- DHCPv6 server (RFC 8415, the parts the client uses) --------------------

ALL_D6 = a6("ff02::1:2")
SDUID = b"\x00\x03\x00\x01" + RMAC


def o6(code, body):
    return struct.pack("!HH", code, len(body)) + body


def opts6(b):
    out, i = {}, 0
    while i + 4 <= len(b):
        c, n = struct.unpack("!HH", b[i:i + 4])
        out.setdefault(c, b[i + 4:i + 4 + n])
        i += 4 + n
    return out


def ia_na(iaid, t1, t2, addrs):
    b = struct.pack("!III", iaid, t1, t2)
    for a, p, v in addrs:
        b += o6(5, a + struct.pack("!II", p, v))
    return o6(3, b)


def d6_send(cmac, cll, msg, bad_sum=False):
    u = struct.pack("!HHHH", 547, 546, 8 + len(msg), 0) + msg
    c = (l4sum6(R_LL, cll, 17, u) or 0xFFFF) ^ (1 if bad_sum else 0)
    u = u[:6] + struct.pack("!H", c) + u[8:]
    return eth(cmac, RMAC, 0x86DD, ip6(R_LL, cll, 17, u, hlim=255))


def d6_good(typ, xid, cid, iaid):
    m = bytes([typ]) + xid + o6(1, cid) + o6(2, SDUID)
    if iaid is not None:
        m += ia_na(iaid, 60, 96, [(D6_ADDR, 120, 240)])
    return m + o6(23, D6_DNS)


def d6_bad(typ, xid, cid, iaid):
    head = lambda c=cid, s=SDUID: bytes([typ]) + xid + o6(1, c) + (o6(2, s) if s else b"")
    ia = lambda a, p=120, v=240, i=iaid, t=(60, 96): ia_na(i, t[0], t[1], [(a6(a), p, v)])
    kinds = [
        ("link-local address", head() + ia("fe80::bad")),
        ("multicast address", head() + ia("ff02::1:3")),
        ("loopback address", head() + ia("::1")),
        ("IPv4-mapped address", head() + ia("::ffff:10.88.0.66")),
        ("6to4 address", head() + ia("2002:a58:66::1")),
        ("preferred beyond valid", head() + ia("fd66::53", p=500, v=100)),
        ("valid lifetime 0", head() + ia("fd66::54", p=0, v=0)),
        ("another client's id", head(c=b"\x00\x03\x00\x01\x02\x00\x00\x00\x00\x99") + ia("fd66::c1")),
        ("no server id", head(s=None) + ia("fd66::51")),
        ("another IAID", head() + ia("fd66::52", i=iaid ^ 1)),
        ("option past the end", head() + ia("fd66::56")[:-6] + struct.pack("!HH", 23, 400) + D6_DNS),
        ("200-byte server id", head(s=bytes(200)) + ia("fd66::57")),
        ("bad DNS servers", head() + ia(socket.inet_ntop(socket.AF_INET6, D6_ADDR)) + o6(23, a6("::1") + a6("ff02::1") + a6("::ffff:10.88.0.53") + a6("2002:a58:35::1"))),
        ("odd DNS option", head() + o6(23, a6("fd66::5") + b"\x01")),
        ("huge lifetimes", head() + ia(socket.inet_ntop(socket.AF_INET6, D6_ADDR), p=0xFFFFFFFF, v=0xFFFFFFFF, t=(0xFFFFFFFF, 0xFFFFFFFF))),
        ("IA_NA status NoAddrsAvail with an address", head() + o6(3, struct.pack("!III", iaid, 0, 0) + o6(13, b"\x00\x02") + o6(5, a6("fd66::58") + struct.pack("!II", 9, 9)))),
        ("bad checksum", None),
    ]
    name, m = kinds[stats["d6_hostile_sent"] % len(kinds)]
    stats["d6_hostile_sent"] += 1
    stats["last_d6_hostile"] = name
    return name, m if m is not None else head() + ia("fd66::55")


def d6_hostile_push():
    if not d6_last:
        return []
    cmac, cll, xid, cid, iaid = d6_last
    out = []
    for typ in (2, 7):
        name, m = d6_bad(typ, xid, cid, iaid if iaid is not None else 0)
        out.append(d6_send(cmac, cll, m, bad_sum=(name == "bad checksum")))
    return out


def dhcp6(cmac, cll, m):
    global d6_last
    if len(m) < 4 or d6 == "d6-off":
        return []
    typ, xid, o = m[0], m[1:4], opts6(m[4:])
    cid = o.get(1)
    if not cid:
        return []
    iaid = struct.unpack("!I", o[3][:4])[0] if 3 in o and len(o[3]) >= 4 else None
    d6_last = (cmac, cll, xid, cid, iaid)
    name = {1: "d6_solicit", 3: "d6_request", 5: "d6_renew", 6: "d6_rebind", 11: "d6_info"}.get(typ)
    if not name:
        return []
    stats[name] += 1
    if d6 == "d6-hostile":
        return d6_hostile_push()
    if typ == 11:
        return [d6_send(cmac, cll, bytes([7]) + xid + o6(1, cid) + o6(2, SDUID) + o6(23, D6_DNS) + o6(32, struct.pack("!I", 600)))]
    if d6 != "d6-addr" or iaid is None:
        return []
    return [d6_send(cmac, cll, d6_good(2 if typ == 1 else 7, xid, cid, iaid))]


def see(typ, p):
    """Count what the guest sent, by protocol and destination."""
    if typ == 0x0800 and len(p) >= 20:
        proto, d = p[9], socket.inet_ntoa(p[16:20])
        body = p[(p[0] & 15) * 4:]
        if proto == 17 and len(body) >= 4 and struct.unpack("!H", body[2:4])[0] == 67:
            return
        # The kernel's port unreachable for our unicast DHCP answer (atrium-net
        # takes DHCP by BPF; nothing listens on 68): DHCP's, not a leak.
        if proto == 1 and len(body) >= 8 + 20 + 4 and body[0] == 3 and body[17] == 17:
            q = body[8 + (body[8] & 15) * 4:]
            if len(q) >= 4 and struct.unpack("!HH", q[:4]) == (67, 68):
                return
        port = f":{struct.unpack('!H', body[2:4])[0]}" if proto in (6, 17) and len(body) >= 4 else ""
        k = f"{ {1: 'icmp', 6: 'tcp', 17: 'udp'}.get(proto, proto)} {d}{port}"
    elif typ == 0x86DD and len(p) >= 40:
        nxt, d = p[6], socket.inet_ntop(socket.AF_INET6, p[24:40])
        if nxt == 58 and len(p) > 40 and p[40] in (133, 135, 136, 143):
            return
        if nxt == 17 and len(p) >= 44 and struct.unpack("!H", p[42:44])[0] == 547:
            return
        k = f"ip6/{nxt} {d}"
    else:
        return
    seen[k] = seen.get(k, 0) + 1


def frame(f, relay=None):
    global wg_peer, gp_peer
    stats["frames"] += 1
    if len(f) < 14:
        return []
    dst, src, typ = f[:6], f[6:12], struct.unpack("!H", f[12:14])[0]
    p = f[14:]
    see(typ, p)
    if typ == 0x86DD:
        return frame6(src, p)
    if typ == 0x0806 and len(p) >= 28 and p[6:8] == b"\0\x01":
        tpa = p[24:28]
        if conflict and tpa == YOU and src != CMAC:
            stats["arp_conflict"] += 1
            r = struct.pack("!HHBBH", 1, 0x0800, 6, 4, 2) + CMAC + YOU + p[8:14] + p[14:18]
            return [eth(src, CMAC, 0x0806, r)]
        if tpa in (ROUTER, DNS):
            stats["arp"] += 1
            r = struct.pack("!HHBBH", 1, 0x0800, 6, 4, 2) + RMAC + tpa + p[8:14] + p[14:18]
            return [eth(src, RMAC, 0x0806, r)]
        return []
    if typ != 0x0800 or len(p) < 20:
        return []
    ihl = (p[0] & 15) * 4
    proto, s, d = p[9], p[12:16], p[16:20]
    body = p[ihl:struct.unpack("!H", p[2:4])[0]]
    if proto == 17 and len(body) >= 8:
        sport, dport = struct.unpack("!HH", body[:4])
        if dport == 67:
            return handle_dhcp(src, body[8:], d)
        if d == WG_GW and dport == WG_PORT and relay is not None:
            stats["wg_out"] = stats.get("wg_out", 0) + 1
            wg_peer = (src, s, sport)
            relay.sendto(body[8:], WG_RELAY)
            return []
        if d == AC_GW and dport == GP_ESP_PORT and relay is not None and GP_ESP_RELAY:
            gp_peer = (src, s, sport)
            relay.sendto(body[8:], GP_ESP_RELAY)
            return []
        if dport == 5353 and d == MDNS_GROUP:
            return handle_mdns(src, s, sport, body[8:])
        if dport == 53 and d == DNS:
            stats["dns"] += 1
            a = dns_answer(body[8:])
            if a:
                return [eth(src, RMAC, 0x0800, ip(DNS, s, 17, udp(DNS, s, 53, sport, a)))]
    if proto == 6 and d == WEB:
        return handle_tcp(src, s, body)
    if proto == 6 and d in GW_PORTS:
        return handle_ac(src, s, body, d)
    if proto == 6 and d == ROUTER:
        return handle_tcp(src, s, body, ROUTER)
    if proto == 1 and len(body) >= 8 and body[0] == 8 and d in (ROUTER, DNS, WEB):
        stats["ping"] += 1
        r = b"\0\0\0\0" + body[4:]
        r = r[:2] + struct.pack("!H", csum(r)) + r[4:]
        return [eth(src, RMAC, 0x0800, ip(d, s, 1, r))]
    return []


def main():
    global mode, mode6, web, d6, conflict, declined, mdns, mdns_printer, net_mtu
    lan = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    lan.bind(LAN)
    ctl = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    ctl.bind(CTL)
    relay = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    relay.bind(("127.0.0.1", 0))
    import select
    print(f"lan: up, lease {LEASE} s", flush=True)
    import time
    next_ra = next_bad = next_d6 = 0.0
    while True:
        now = time.monotonic()
        if now >= next_ra:
            lan.sendto(good_ra(), GUEST)
            next_ra = now + 30
        if mode6 == "hostile6" and now >= next_bad:
            lan.sendto(hostile6(), GUEST)
            next_bad = now + 2
        if mode6 == "flap6" and now >= next_bad:
            lan.sendto(good_ra(), GUEST)
            na = struct.pack("!I", 0x20000000) + R_LL + bytes([2, 1]) + RMAC
            lan.sendto(eth(mcast_mac(ALL_NODES), RMAC, 0x86DD, icmp6(R_LL, ALL_NODES, 136, 0, na)), GUEST)
            next_bad = now + 0.2
        if d6 == "d6-hostile" and now >= next_d6:
            for out in d6_hostile_push():
                lan.sendto(out, GUEST)
            next_d6 = now + 1
        hosts = [c.host for c in ac.values() if not c.host_done]
        r, _, _ = select.select([lan, ctl, relay] + hosts, [], [], 0.1 if mode6 == "flap6" else 0.5)
        for c in list(ac.values()):
            outs = c.from_host() if c.host in r else []
            outs += c.again(time.monotonic())
            for out in outs:
                lan.sendto(out, GUEST)
        if lan in r:
            f, _ = lan.recvfrom(65536)
            for out in frame(f, relay):
                lan.sendto(out, GUEST)
        if relay in r:
            m, a = relay.recvfrom(65536)
            if a == WG_RELAY and wg_peer:
                stats["wg_in"] = stats.get("wg_in", 0) + 1
                gmac, gip, gport = wg_peer
                lan.sendto(eth(gmac, RMAC, 0x0800, ip(WG_GW, gip, 17, udp(WG_GW, gip, WG_PORT, gport, m))), GUEST)
            if a == GP_ESP_RELAY and gp_peer:
                gmac, gip, gport = gp_peer
                lan.sendto(eth(gmac, RMAC, 0x0800, ip(AC_GW, gip, 17, udp(AC_GW, gip, GP_ESP_PORT, gport, m))), GUEST)
        if ctl in r:
            c, a = ctl.recvfrom(256)
            w = c.decode().strip()
            if w in ("good", "hostile", "silent", "tunnelvision"):
                mode = w
                print(f"lan: mode {w}", flush=True)
            if w in ("web-ok", "web-dead", "web-captive"):
                web = w
                print(f"lan: {w}", flush=True)
            if w in ("conflict", "noconflict"):
                conflict = w == "conflict"
                declined = False
                print(f"lan: {w}", flush=True)
            if w in ("d6-off", "d6-info", "d6-addr", "d6-hostile"):
                d6 = w
                print(f"lan: {w}", flush=True)
                lan.sendto(good_ra(), GUEST)
            if w == "ac-drop":
                # Every AnyConnect connection reset, as a roam leaves them.
                for c in list(ac.values()):
                    lan.sendto(c.seg(c.nxt, 0x14), GUEST)
                    c.close()
                print("lan: ac-drop", flush=True)
            if w in ("seen", "seen-reset"):
                ctl.sendto(json.dumps(seen).encode(), a)
                if w == "seen-reset":
                    seen.clear()
                continue
            ws = w.split()
            if len(ws) == 2 and ws[0] == "mtu":
                net_mtu = None if ws[1] == "off" else int(ws[1])
                print(f"lan: mtu {net_mtu}", flush=True)
                lan.sendto(good_ra(), GUEST)
            if ws and ws[0] in ("mdns-printer", "mdns-flood", "mdns-off"):
                mdns = ws[0]
                if ws[0] == "mdns-printer" and len(ws) == 3:
                    mdns_printer = (socket.inet_aton(ws[1]), int(ws[2]))
                print(f"lan: {w}", flush=True)
            if w in ("good6", "hostile6", "flap6"):
                mode6 = w
                print(f"lan: mode {w}", flush=True)
            ctl.sendto(json.dumps(dict(stats, mode=mode, web=web, d6=d6, mdns=mdns)).encode(), a)


if __name__ == "__main__":
    main()
