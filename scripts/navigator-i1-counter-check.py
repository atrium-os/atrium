#!/usr/bin/env python3
"""Drive the I1 Limen fixture parent on an already staged FreeBSD guest."""
import argparse
import pathlib
import re
import shlex
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("ssh", help="guest SSH wrapper")
parser.add_argument("stage", help="guest directory containing the parent's input.sock and screen.png")
parser.add_argument("evidence", type=pathlib.Path)
parser.add_argument("scene", type=pathlib.Path, help="parent NSG from nsg-render")
args = parser.parse_args()
args.evidence.mkdir(parents=True, exist_ok=True)


def guest(command, data=None):
    return subprocess.run([args.ssh, "cd " + shlex.quote(args.stage) + " && " + command],
                          input=data, capture_output=True, check=True, timeout=15).stdout


def screen(label, expected):
    deadline = time.monotonic() + 15
    path = args.evidence / (label + ".png")
    while time.monotonic() < deadline:
        path.write_bytes(guest("cat screen.png"))
        result = subprocess.run(["tesseract", str(path), "stdout", "--psm", "11"],
                                capture_output=True, timeout=5)
        if re.search(expected, result.stdout.decode(errors="replace"), re.I):
            print(label, flush=True)
            return
        time.sleep(0.15)
    raise RuntimeError(f"{label}: missing {expected!r} in {path}")


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def press(x, y):
    guest("nc -N -U input.sock", f"MOVE {x} {y}\nBTN 1 1\nBTN 1 0\n".encode())


def marker(text):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if text in guest("cat parent.log").decode():
            return
        time.sleep(.15)
    raise RuntimeError("missing parent marker: " + text)


# This gate uses an untransformed, integral CSS-pixel slot in a known fixture.
slots = re.findall(r'^embed island k[0-9]+ (\d+) (\d+) (\d+) (\d+) a0 island "https://widget\.example/counter\.html"$',
                   args.scene.read_text(), re.M)
if len(slots) != 1 or any(int(n) % 64 for n in slots[0]):
    raise RuntimeError("expected one integral counter slot in the document scene")
left, top, width, height = (int(n) // 64 for n in slots[0])
bounds = f"{left} {top} {width} {height}\n".encode()
if guest("cat counter.rect") != bounds:
    raise RuntimeError("guest placement differs from the document scene")
screen("placeholder", r"widget\.example counter")
press(5, 30)
screen("outside", r"widget\.example counter")
require(b"broker GET" not in guest("cat parent.log"), "widget fetched before activation")
press(left + 90, top + 60)
screen("started", r"Count\s*0\b")
# The compositor's island strip takes space above page content.
press(left + 60, top + 55)
screen("incremented", r"Count\s*1\b")
proof = guest("pid=$(pgrep -x counter) && procstat -s \"$pid\" && jls -n jid name ip4 ip6")
require(re.search(rb"\bcounter\s+(?!0\s)\d+.*\sC\s", proof), "missing confined worker")
require(re.search(rb"name=app-org-atrium-navigator-engine--[^ ]+ ip4=disable ip6=disable", proof),
        "worker still has a network stack")
(args.evidence / "guest-worker.txt").write_bytes(proof)
guest("touch cover")
screen("covered", r"Cover window")
press(left + 60, top + 55)
marker("cover received press")
guest("unlink cover")
screen("uncovered", r"Count\s*1\b")
press(left + 60, top + 55)
screen("resumed", r"Count\s*2\b")
guest("touch close-page")
marker("counter exited after parent closed")
log = guest("cat parent.log")
require(log.count(b"broker GET 1 https://widget.example/counter.html") == 1, "unexpected widget fetch count")
require(b"capmode=1 denied-path=ECAPMODE" in log, "worker did not verify Capsicum")
guest("! pgrep -x counter")
(args.evidence / "parent.log").write_bytes(log)
