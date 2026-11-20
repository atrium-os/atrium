#!/usr/bin/env python3
"""Verify Stop in an installed Navigator window on the staged I1 counter fixture."""
import argparse
from pathlib import Path
import shlex
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("ssh", help="SSH wrapper for the private gate guest")
parser.add_argument("stage", help="guest directory containing input.sock and screen.png")
parser.add_argument("log", help="fresh Navigator log in that directory")
parser.add_argument("evidence", type=Path)
args = parser.parse_args()
args.evidence.mkdir(parents=True, exist_ok=True)


def guest(command, data=None):
    return subprocess.run([args.ssh, "cd " + shlex.quote(args.stage) + " && " + command],
                          input=data, stdout=subprocess.PIPE, check=True, timeout=15).stdout


def press(x, y):
    guest("nc -N -U input.sock", f"MOVE {x} {y}\nBTN 1 1\nBTN 1 0\n".encode())


def wait_for(label, check):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.15)
    raise RuntimeError(label)


def frame(name):
    path = args.evidence / (name + ".png")
    path.write_bytes(guest("cat screen.png"))
    return path


def count(name, n):
    def check():
        words = subprocess.run(["tesseract", str(frame(name)), "stdout", "--psm", "11"],
                               capture_output=True, check=True, timeout=5).stdout.splitlines()
        return f"Count {n}".encode() in words
    wait_for(f"{name}: expected Count {n}", check)


def log():
    return guest("cat " + shlex.quote(args.log))


def start(previous):
    press(390, 180)
    wait_for("missing Start prompt", lambda: log().count(b"prompt=widget ") > previous)
    time.sleep(.65)  # Wait for the 500 ms chrome guard.
    if guest("pgrep -x counter || true") or log().count(b"widget GET ") != previous:
        raise RuntimeError("the placeholder started an engine before consent")
    frame(f"start-{previous + 1}")
    press(1074, 60)


def parent():
    return guest("pgrep -x navigator-ui && pgrep -x navigator-page")


# Geometry of the 1280x840 headless fixture with a 1120x740 Navigator window.
parent_before = parent()
start(0)
count("initial", 0)
worker = guest("pgrep -x counter")
press(350, 118)
count("running", 1)
press(905, 91)
wait_for("Stop did not withdraw the engine", lambda: b"live widget stopped" in log()
         and not guest("pgrep -x counter || true"))
frame("stopped")
if parent() != parent_before or b"prompt=widget-failed" in log():
    raise RuntimeError("Stop disturbed the parent or reported a crash")
start(1)
count("restarted", 0)
replacement = guest("pgrep -x counter")
press(350, 118)
count("incremented", 1)
report = log()
if (worker == replacement or parent() != parent_before or report.count(b"widget GET ") != 2
        or report.count(b'the page child laid out "Embedded counter"') != 1):
    raise RuntimeError("unexpected worker, fetch count or parent reload")
print(f"Stop retired PID {worker.decode().strip()}. Guarded Start launched PID {replacement.decode().strip()}.")
