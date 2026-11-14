#!/usr/bin/env python3
"""A fake Ethernet LAN for atrium-net's gate (docs/spec/atrium-net.md §10).

QEMU's `-netdev socket,udp=127.0.0.1:7801,localaddr=127.0.0.1:7802` puts the
guest's second NIC on this script: every guest frame arrives as one UDP
datagram on 7801 and every datagram we send to 7802 is a frame for the guest.

On the LAN (10.88.0.0/24): the router 10.88.0.1 (ARP, ping, DHCP server) and a
DNS server 10.88.0.53 that answers gate.atrium.test with 10.88.0.77.

Control on UDP 127.0.0.1:7803, one word per datagram, answered with JSON:
  good | hostile | silent   -- DHCP behaviour
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
         "hostile_sent": 0, "dns": 0, "ping": 0, "arp": 0, "frames": 0}
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
    hit = name == "gate.atrium.test" and qtype == 1
    rflags = 0x8180 | (0 if hit or name.endswith("atrium.test") else 3)
    ans = (b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 60, 4) + bytes([10, 88, 0, 77])) if hit else b""
    return struct.pack("!HHHHHH", qid, rflags, 1, 1 if hit else 0, 0, 0) + question + ans


def frame(f):
    stats["frames"] += 1
    if len(f) < 14:
        return []
    dst, src, typ = f[:6], f[6:12], struct.unpack("!H", f[12:14])[0]
    p = f[14:]
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
    global mode
    lan = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    lan.bind(LAN)
    ctl = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    ctl.bind(CTL)
    import select
    print(f"lan: up, lease {LEASE} s", flush=True)
    while True:
        r, _, _ = select.select([lan, ctl], [], [])
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
            ctl.sendto(json.dumps(dict(stats, mode=mode)).encode(), a)


if __name__ == "__main__":
    main()
