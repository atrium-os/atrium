#!/usr/bin/env python3
"""IPv6 for the Wi-Fi network in atrium-net's Wi-Fi IPv6 gate
(docs/spec/atrium-net.md §10.3), run in the AP VM: router advertisements on
br0 every 3 s and in answer to a router solicitation, with prefix
fd77:0:0:77::/64 (on-link, autonomous) and RDNSS fd77:0:0:77::1. The kernel
answers neighbour solicitations and pings for fd77:0:0:77::1 on br0.

    python3 ra6.py [br0]
"""
import select, socket, struct, sys, time

IFACE = sys.argv[1] if len(sys.argv) > 1 else "br0"
PREFIX = socket.inet_pton(socket.AF_INET6, "fd77:0:0:77::")
DNS = socket.inet_pton(socket.AF_INET6, "fd77:0:0:77::1")


def mac():
    return bytes.fromhex(open(f"/sys/class/net/{IFACE}/address").read().strip().replace(":", ""))


def ra():
    # Hop limit 64, no M/O, router lifetime 1800 s.
    body = struct.pack("!BBHBBHII", 134, 0, 0, 64, 0, 1800, 0, 0)
    body += struct.pack("!BB", 1, 1) + mac()
    body += struct.pack("!BBBBIII", 3, 4, 64, 0xC0, 86400, 14400, 0) + PREFIX
    body += struct.pack("!BBHI", 25, 3, 0, 600) + DNS
    return body


def main():
    idx = socket.if_nametoindex(IFACE)
    s = socket.socket(socket.AF_INET6, socket.SOCK_RAW, socket.IPPROTO_ICMPV6)
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_HOPS, 255)
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_UNICAST_HOPS, 255)
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_IF, idx)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, IFACE.encode())
    # Join ff02::2 so router solicitations reach us.
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_JOIN_GROUP, socket.inet_pton(socket.AF_INET6, "ff02::2") + struct.pack("@I", idx))
    sent = rs = 0
    nxt = 0.0
    while True:
        now = time.time()
        if now >= nxt:
            s.sendto(ra(), ("ff02::1", 0, 0, idx))
            sent += 1
            nxt = now + 3
            print(f"ra sent={sent} rs={rs}", flush=True)
        r, _, _ = select.select([s], [], [], max(0.0, nxt - now))
        if r:
            p, a = s.recvfrom(1500)
            if p and p[0] == 133:
                rs += 1
                nxt = 0.0


if __name__ == "__main__":
    main()
