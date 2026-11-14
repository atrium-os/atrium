#!/usr/bin/env python3
"""A fake Ethernet LAN for atrium-net's gate (docs/spec/atrium-net.md §10).

QEMU's `-netdev socket,udp=127.0.0.1:7801,localaddr=127.0.0.1:7802` puts the
guest's second NIC on this script: every guest frame arrives as one UDP
datagram on 7801 and every datagram we send to 7802 is a frame for the guest.

On the LAN (10.88.0.0/24): the router 10.88.0.1 (ARP, ping, DHCP server) and a
DNS server 10.88.0.53 that answers gate.atrium.test with 10.88.0.77.

IPv6: the router fe80::1 sends RAs (on solicitation and every 30 s) for
fd77:0:0:88::/64 with RDNSS fd77:0:0:88::53 and fe80::1; it answers neighbor
solicitations, pings and DNS (AAAA gate.atrium.test = fd77:0:0:88::77) on
fe80::1, fd77:0:0:88::1 and fd77:0:0:88::53.

Control on UDP 127.0.0.1:7803, one word per datagram, answered with JSON:
  good | hostile | silent   -- DHCP behaviour
  hostile6 | good6          -- a rogue router fe80::66 sends bad RAs every 2 s
  flap6                     -- kernel stress: RAs every 0.2 s and unsolicited
                               NAs for fe80::1 without the Router flag
  stats                     -- counters
"""
import json, random, socket, struct, sys

LAN = ("127.0.0.1", 7801)
GUEST = ("127.0.0.1", 7802)
CTL = ("127.0.0.1", 7803)
RMAC = bytes.fromhex("024e000000fe")
ROUTER = bytes([10, 88, 0, 1])
DNS = bytes([10, 88, 0, 53])
YOU = bytes([10, 88, 0, 50])
LEASE = int(sys.argv[1]) if len(sys.argv) > 1 else 120
MAGIC = bytes([99, 130, 83, 99])

mode = "good"
stats = {"discover": 0, "request": 0, "renew": 0, "rebind": 0, "reboot": 0,
         "hostile_sent": 0, "dns": 0, "ping": 0, "arp": 0, "frames": 0,
         "rs": 0, "ra": 0, "ns": 0, "ping6": 0, "dns6": 0, "hostile6_sent": 0}
mode6 = "good6"
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


def dhcp_reply(req, kind, extra=None, yi=YOU, mangle=None):
    xid, chaddr = req[4:8], req[28:44]
    b = bytes([2, 1, 6, 0]) + xid + b"\0\0\0\0" + b"\0" * 4 + yi + ROUTER + b"\0" * 4 + chaddr
    b += b"\0" * 192 + MAGIC
    o = [(53, bytes([kind])), (54, ROUTER)]
    if kind != 6:
        o += [(1, bytes([255, 255, 255, 0])), (3, ROUTER), (6, DNS), (51, struct.pack("!I", LEASE))]
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


def handle_dhcp(src_mac, req):
    if len(req) < 240 or req[0] != 1 or req[236:240] != MAGIC:
        return []
    o, i = {}, 240
    while i < len(req) and req[i] != 255:
        if req[i] == 0:
            i += 1
            continue
        o[req[i]] = req[i + 2:i + 2 + req[i + 1]]
        i += 2 + req[i + 1]
    kind = o.get(53, b"\0")[0]
    ciaddr = req[12:16]
    if kind == 1:
        stats["discover"] += 1
    elif kind == 3:
        if ciaddr != b"\0\0\0\0":
            stats["renew" if 54 not in o else "request"] += 1
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
        r = dhcp_reply(req, 2)
    elif kind == 3:
        want = o.get(50, ciaddr)
        r = dhcp_reply(req, 5 if want == YOU else 6)
    else:
        return []
    return [eth(src_mac, RMAC, 0x0800, ip(ROUTER, b"\xff" * 4 if ciaddr == b"\0\0\0\0" else ciaddr, 17,
                                         udp(ROUTER, b"\xff" * 4, 67, 68, r)))]


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
    hit = name == "gate.atrium.test" and qtype in (1, 28)
    rflags = 0x8180 | (0 if name.endswith("atrium.test") else 3)
    if hit and qtype == 1:
        ans = b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 60, 4) + bytes([10, 88, 0, 77])
    elif hit:
        ans = b"\xc0\x0c" + struct.pack("!HHIH", 28, 1, 60, 16) + V6_HOST
    else:
        ans = b""
    return struct.pack("!HHHHHH", qid, rflags, 1, 1 if hit else 0, 0, 0) + question + ans


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
MINE6 = (R_LL, R_GLOBAL, DNS6)


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
    mtu = bytes([5, 1, 0, 0]) + struct.pack("!I", 1500)
    sll = bytes([1, 1]) + RMAC
    body = bytes([64, 0]) + struct.pack("!HII", 1800, 0, 0) + pi + mtu + sll + rdnss(600, [DNS6, R_LL])
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


def frame(f):
    stats["frames"] += 1
    if len(f) < 14:
        return []
    dst, src, typ = f[:6], f[6:12], struct.unpack("!H", f[12:14])[0]
    p = f[14:]
    if typ == 0x86DD:
        return frame6(src, p)
    if typ == 0x0806 and len(p) >= 28 and p[6:8] == b"\0\x01":
        tpa = p[24:28]
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
            return handle_dhcp(src, body[8:])
        if dport == 53 and d == DNS:
            stats["dns"] += 1
            a = dns_answer(body[8:])
            if a:
                return [eth(src, RMAC, 0x0800, ip(DNS, s, 17, udp(DNS, s, 53, sport, a)))]
    if proto == 1 and len(body) >= 8 and body[0] == 8 and d in (ROUTER, DNS):
        stats["ping"] += 1
        r = b"\0\0\0\0" + body[4:]
        r = r[:2] + struct.pack("!H", csum(r)) + r[4:]
        return [eth(src, RMAC, 0x0800, ip(d, s, 1, r))]
    return []


def main():
    global mode, mode6
    lan = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    lan.bind(LAN)
    ctl = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    ctl.bind(CTL)
    import select
    print(f"lan: up, lease {LEASE} s", flush=True)
    import time
    next_ra = next_bad = 0.0
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
        r, _, _ = select.select([lan, ctl], [], [], 0.1 if mode6 == "flap6" else 1.0)
        if lan in r:
            f, _ = lan.recvfrom(65536)
            for out in frame(f):
                lan.sendto(out, GUEST)
        if ctl in r:
            c, a = ctl.recvfrom(256)
            w = c.decode().strip()
            if w in ("good", "hostile", "silent"):
                mode = w
                print(f"lan: mode {w}", flush=True)
            if w in ("good6", "hostile6", "flap6"):
                mode6 = w
                print(f"lan: mode {w}", flush=True)
            ctl.sendto(json.dumps(dict(stats, mode=mode)).encode(), a)


if __name__ == "__main__":
    main()
