#!/usr/bin/env python3
"""A UDP proxy that impairs a path, for gates where `tc netem` is not there.

    python3 -I scripts/lib/udp-impair.py --listen 127.0.0.1:24400 \
        --target 127.0.0.1:24300 --schedule FILE --log FILE

One client (the first address that sends) is relayed to the target and
back. The schedule is lines of `SECONDS key=value ...`, seconds from the
proxy's start; each line replaces the whole profile from then on:

    delay=MS      one-way delay each way (RTT adds twice this)
    jitter=MS     extra one-way delay, uniform in [0, MS] per packet
    loss=FRAC     packets dropped at random, each way
    rate=KBIT     each direction's link rate (a 64 KB queue, tail drop)
    blackout=1    everything dropped
    rebind=1      the proxy's upstream socket is replaced once on entering
                  this line (the host sees the client's address change)

A line with no keys is a clean path. The log gets one line per profile
change and totals at the end (packets and bytes each way, drops).
"""
import argparse
import asyncio
import heapq
import random
import socket
import sys
import time


def parse_schedule(text):
    out = []
    for n, line in enumerate(text.splitlines(), 1):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        words = line.split()
        prof = {"delay": 0.0, "jitter": 0.0, "loss": 0.0, "rate": 0.0, "blackout": False, "rebind": False}
        for w in words[1:]:
            k, _, v = w.partition("=")
            if k not in prof:
                raise SystemExit(f"schedule line {n}: unknown key {k!r}")
            prof[k] = v == "1" if k in ("blackout", "rebind") else float(v)
        out.append((float(words[0]), prof))
    out.sort(key=lambda p: p[0])
    return out


class Path:
    """One direction: its queue for the rate limit, and the delay line."""

    QUEUE = 64 * 1024

    def __init__(self):
        self.busy_until = 0.0
        self.queued = 0
        self.packets = self.bytes = self.dropped = 0

    def admit(self, now, size, prof):
        """When this packet leaves the link, or None if it is dropped."""
        if prof["blackout"] or random.random() < prof["loss"]:
            self.dropped += 1
            return None
        t = now
        if prof["rate"] > 0:
            start = max(now, self.busy_until)
            backlog = (start - now) * prof["rate"] * 1000 / 8
            if backlog + size > self.QUEUE:
                self.dropped += 1
                return None
            self.busy_until = start + size * 8 / (prof["rate"] * 1000)
            t = self.busy_until
        t += prof["delay"] / 1000 + random.uniform(0, prof["jitter"]) / 1000
        self.packets += 1
        self.bytes += size
        return t


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", required=True)
    ap.add_argument("--target", required=True)
    ap.add_argument("--schedule", required=True)
    ap.add_argument("--log", required=True)
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    random.seed(a.seed)
    sched = parse_schedule(open(a.schedule).read())
    lh, lp = a.listen.rsplit(":", 1)
    th, tp = a.target.rsplit(":", 1)
    target = (th, int(tp))
    log = open(a.log, "w", buffering=1)
    loop = asyncio.get_running_loop()
    t0 = time.monotonic()

    down = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)  # to and from the client
    down.bind((lh, int(lp)))
    down.setblocking(False)

    def upstream():
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.bind(("127.0.0.1", 0))
        s.setblocking(False)
        return s

    state = {"up": upstream(), "client": None, "prof": None, "idx": -1}
    to_host, to_client = Path(), Path()
    heap = []  # (time, seq, sock, data, addr)
    seq = [0]
    wake = asyncio.Event()

    def profile(now):
        i = state["idx"]
        while i + 1 < len(sched) and sched[i + 1][0] <= now - t0:
            i += 1
            p = sched[i][1]
            if p["rebind"]:
                old = state["up"]
                state["up"] = upstream()
                loop.remove_reader(old.fileno())
                old.close()
                loop.add_reader(state["up"].fileno(), on_up)
            log.write(f"{now - t0:.1f} profile {p} up-port {state['up'].getsockname()[1]}\n")
        state["idx"] = i
        return sched[i][1] if i >= 0 else {"delay": 0, "jitter": 0, "loss": 0, "rate": 0, "blackout": False, "rebind": False}

    def push(t, sock, data, addr):
        seq[0] += 1
        heapq.heappush(heap, (t, seq[0], sock, data, addr))
        wake.set()

    def on_down():
        while True:
            try:
                data, addr = down.recvfrom(65536)
            except BlockingIOError:
                return
            if state["client"] is None:
                state["client"] = addr
            if addr != state["client"]:
                continue
            now = time.monotonic()
            t = to_host.admit(now, len(data), profile(now))
            if t is not None:
                push(t, "up", data, target)

    def on_up():
        while True:
            try:
                data, addr = state["up"].recvfrom(65536)
            except BlockingIOError:
                return
            except OSError:
                return
            if addr != target or state["client"] is None:
                continue
            now = time.monotonic()
            t = to_client.admit(now, len(data), profile(now))
            if t is not None:
                push(t, "down", data, state["client"])

    loop.add_reader(down.fileno(), on_down)
    loop.add_reader(state["up"].fileno(), on_up)
    stop = asyncio.Event()
    loop.add_signal_handler(15, stop.set)
    loop.add_signal_handler(2, stop.set)

    async def sender():
        while not stop.is_set():
            now = time.monotonic()
            profile(now)
            while heap and heap[0][0] <= now:
                _, _, which, data, addr = heapq.heappop(heap)
                sock = state["up"] if which == "up" else down
                try:
                    sock.sendto(data, addr)
                except OSError:
                    pass
            delay = min(heap[0][0] - now, 0.05) if heap else 0.05
            wake.clear()
            try:
                await asyncio.wait_for(wake.wait(), max(delay, 0))
            except asyncio.TimeoutError:
                pass

    await asyncio.wait([asyncio.create_task(sender()), asyncio.create_task(stop.wait())], return_when=asyncio.FIRST_COMPLETED)
    log.write(
        f"totals to_host packets {to_host.packets} bytes {to_host.bytes} dropped {to_host.dropped} "
        f"to_client packets {to_client.packets} bytes {to_client.bytes} dropped {to_client.dropped}\n"
    )


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        sys.exit(0)
