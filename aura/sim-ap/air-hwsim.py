#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""The Linux half of Aura's simulated air (docs/spec/wifi.md section 6.2).

Registers as mac80211_hwsim's wireless medium (the netlink interface
wmediumd uses), so every frame the hwsim radios send comes here instead of
going to the other radios. Each frame goes out as one UDP datagram to
aura-air in the FreeBSD VM; each datagram from there is handed to the
hwsim radio as a received frame. Every transmitted frame is reported back
as acknowledged, as a clean channel would.

Datagram: b"AAIR" | u16 freq | i8 signal | u8 0 | 802.11 frame (no FCS).
Test only; the AP side runs hostapd unmodified.

With several hwsim radios (one hostapd BSS each, on different channels),
each radio's channel is learned from its own frames and a frame from the
FreeBSD side goes to the radio on its channel. The signal the FreeBSD side
sees per channel comes from SIGNAL_FILE ("FREQ DBM" lines, re-read when it
changes; default -40), so a test can make one AP fade and another grow.

usage: air-hwsim.py HOST PORT
"""

import os
import select
import socket
import struct
import sys
import time

NETLINK_GENERIC = 16
GENL_ID_CTRL = 0x10
CTRL_CMD_GETFAMILY = 3
CTRL_ATTR_FAMILY_ID = 1
CTRL_ATTR_FAMILY_NAME = 2
NLM_F_REQUEST = 1
NLMSG_ERROR = 2

HWSIM_CMD_REGISTER = 1
HWSIM_CMD_FRAME = 2
HWSIM_CMD_TX_INFO_FRAME = 3

A_ADDR_RECEIVER = 1
A_ADDR_TRANSMITTER = 2
A_FRAME = 3
A_FLAGS = 4
A_RX_RATE = 5
A_SIGNAL = 6
A_TX_INFO = 7
A_COOKIE = 8
A_FREQ = 19

TX_CTL_NO_ACK = 2
TX_STAT_ACK = 4

MAGIC = b"AAIR"
SIGNAL_FILE = "/tmp/air-signal"


class Signals:
    def __init__(self):
        self.mtime = None
        self.by_freq = {}

    def get(self, freq):
        try:
            m = os.stat(SIGNAL_FILE).st_mtime
        except OSError:
            m = None
        if m != self.mtime:
            self.mtime = m
            self.by_freq = {}
            if m is not None:
                with open(SIGNAL_FILE) as f:
                    for line in f:
                        p = line.split()
                        if len(p) == 2:
                            self.by_freq[int(p[0])] = max(-100, min(0, int(p[1])))
        return self.by_freq.get(freq, -40)


def attr(t, data):
    n = 4 + len(data)
    return struct.pack("<HH", n, t) + data + b"\0" * ((4 - n % 4) % 4)


def attrs(buf):
    out = {}
    i = 0
    while i + 4 <= len(buf):
        n, t = struct.unpack_from("<HH", buf, i)
        if n < 4:
            break
        out[t & 0x3FFF] = buf[i + 4 : i + n]
        i += (n + 3) & ~3
    return out


class Genl:
    def __init__(self):
        self.s = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, NETLINK_GENERIC)
        self.s.bind((0, 0))
        self.s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
        self.seq = 1

    def send(self, family, cmd, payload, flags=NLM_F_REQUEST):
        body = struct.pack("<BBH", cmd, 1, 0) + payload
        self.seq += 1
        msg = struct.pack("<IHHII", 16 + len(body), family, flags, self.seq, 0) + body
        self.s.send(msg)

    def recv(self):
        """Yields (type, cmd, attrs) for each message in one datagram."""
        data = self.s.recv(65536)
        i = 0
        while i + 16 <= len(data):
            n, typ, _fl, _seq, _pid = struct.unpack_from("<IHHII", data, i)
            if n < 16:
                break
            body = data[i + 16 : i + n]
            if typ == NLMSG_ERROR:
                err = struct.unpack_from("<i", body)[0]
                if err != 0:
                    print(f"air: netlink error {err}", file=sys.stderr)
            elif len(body) >= 4:
                yield typ, body[0], attrs(body[4:])
            i += (n + 3) & ~3

    def family(self, name):
        self.send(GENL_ID_CTRL, CTRL_CMD_GETFAMILY, attr(CTRL_ATTR_FAMILY_NAME, name.encode() + b"\0"))
        for _ in range(4):
            for typ, _cmd, a in self.recv():
                if typ == GENL_ID_CTRL and CTRL_ATTR_FAMILY_ID in a:
                    return struct.unpack("<H", a[CTRL_ATTR_FAMILY_ID][:2])[0]
        raise SystemExit("air: no MAC80211_HWSIM family (modprobe mac80211_hwsim?)")


def main():
    host, port = sys.argv[1], int(sys.argv[2])
    nl = Genl()
    fam = nl.family("MAC80211_HWSIM")
    nl.send(fam, HWSIM_CMD_REGISTER, b"")
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    udp.connect((host, port))
    radios = {}  # freq -> hwsim address of the radio on it
    sig = Signals()
    stats = {"up": 0, "down": 0, "down_err": 0}
    last = time.time()
    print(f"air: medium registered, family {fam}, peer {host}:{port}", flush=True)
    while True:
        r, _, _ = select.select([nl.s, udp], [], [], 1.0)
        if nl.s in r:
            for typ, cmd, a in nl.recv():
                if typ != fam or cmd != HWSIM_CMD_FRAME:
                    continue
                tx = a.get(A_ADDR_TRANSMITTER)
                frame = a.get(A_FRAME)
                if tx is None or frame is None:
                    continue
                flags = struct.unpack("<I", a[A_FLAGS][:4])[0] if A_FLAGS in a else 0
                freq = struct.unpack("<I", a[A_FREQ][:4])[0] if A_FREQ in a else 0
                if not freq:
                    freq = next((f for f, r in radios.items() if r == tx), 0)
                if freq:
                    radios[freq] = tx
                # Report the frame delivered and acknowledged.
                info = a.get(A_TX_INFO, b"")
                rates = bytearray(info)
                if len(rates) >= 2:
                    rates[1] = 1  # first rate, one attempt
                    for j in range(2, len(rates), 2):
                        rates[j] = 0xFF  # idx -1: unused
                        if j + 1 < len(rates):
                            rates[j + 1] = 0
                st = flags if flags & TX_CTL_NO_ACK else flags | TX_STAT_ACK
                nl.send(
                    fam,
                    HWSIM_CMD_TX_INFO_FRAME,
                    attr(A_ADDR_TRANSMITTER, tx)
                    + attr(A_FLAGS, struct.pack("<I", st))
                    + attr(A_SIGNAL, struct.pack("<i", -40))
                    + attr(A_TX_INFO, bytes(rates))
                    + attr(A_COOKIE, a.get(A_COOKIE, b"\0" * 8)),
                )
                try:
                    udp.send(MAGIC + struct.pack("<HbB", freq, sig.get(freq), 0) + frame)
                    stats["up"] += 1
                except OSError:
                    pass  # the FreeBSD side is not up yet
        if udp in r:
            try:
                d = udp.recv(16384)
            except OSError:
                d = b""
            freq = struct.unpack_from("<H", d, 4)[0] if len(d) >= 8 else 0
            radio = radios.get(freq)
            if len(d) >= 8 + 10 and d[:4] == MAGIC and radio is not None:
                # The AP hears us as well as we hear it.
                s_dbm = sig.get(freq)
                nl.send(
                    fam,
                    HWSIM_CMD_FRAME,
                    attr(A_ADDR_RECEIVER, radio)
                    + attr(A_FRAME, d[8:])
                    + attr(A_RX_RATE, struct.pack("<I", 0))
                    + attr(A_SIGNAL, struct.pack("<i", s_dbm))
                    + attr(A_FREQ, struct.pack("<I", freq)),
                )
                stats["down"] += 1
            else:
                stats["down_err"] += 1
        if time.time() - last > 30:
            last = time.time()
            print(f"air: {stats}", flush=True)


if __name__ == "__main__":
    main()
