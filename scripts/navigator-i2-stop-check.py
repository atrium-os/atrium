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
parser.add_argument("--tier", action="store_true", help="check live-part transitions with NAVIGATOR_UI_AX=1")
parser.add_argument("--recycle", action="store_true", help="use counter-recycle.html, a 10-second idle timeout and UI AX")
parser.add_argument("--reports", action="store_true", help="verify the counter's typed origin and title on both launches")
parser.add_argument("--lifecycle", action="store_true", help="use the pinned counter-lifecycle fixture with UI AX")
parser.add_argument("--unload", action="store_true", help="use the pinned counter-unload fixture with UI AX")
parser.add_argument("--pending-stop", action="store_true",
                    help="use counter-unload and the retired-unload-ui.patch gate build with UI AX")
args = parser.parse_args()
if args.reports or args.lifecycle or args.unload or args.pending_stop:
    args.tier = True
args.evidence = args.evidence.resolve()
args.evidence.mkdir(parents=True, exist_ok=True)


def guest(command, data=None):
    return subprocess.run([args.ssh, "cd " + shlex.quote(args.stage) + " && " + command],
                          input=data, stdout=subprocess.PIPE, check=True, timeout=15).stdout


def press(x, y):
    guest("nc -N -U input.sock", f"MOVE {x} {y}\nBTN 1 1\nBTN 1 0\n".encode())


def wait_for(label, check, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.15)
    raise RuntimeError(label)


def frame(name):
    path = args.evidence / (name + ".png")
    def read_frame():
        data = guest("cat screen.png")
        if not data.startswith(b"\x89PNG\r\n\x1a\n") or not data.endswith(b"IEND\xaeB`\x82"):
            return False
        path.write_bytes(data)
        return True
    wait_for("incomplete headless frame", read_frame)
    return path


def count(name, n):
    def check():
        words = subprocess.run(["tesseract", str(frame(name)), "stdout", "--psm", "11"],
                               capture_output=True, check=True, timeout=5).stdout.splitlines()
        return f"Count {n}".encode() in words
    wait_for(f"{name}: expected Count {n}", check)


def log():
    return guest("cat " + shlex.quote(args.log))


def tier(attached):
    if not (args.tier or args.recycle):
        return
    expected = (b'Static: The main document from parent.example is static text and pictures. '
                b'1 live part from https://widget.example.' if attached else
                b'Static: This page is text and pictures. No code from parent.example ran to make it, and none runs now.')
    def matches():
        labels = [line for line in log().splitlines() if b'button "Static:' in line]
        return labels and expected + b'"' in labels[-1]
    wait_for("unexpected tier chip: " + expected.decode(), matches)


def start(previous):
    press(390, 180)
    wait_for("missing Start prompt", lambda: log().count(b"prompt=widget ") > previous)
    time.sleep(.65)  # Wait for the 500 ms chrome guard.
    if guest("pgrep -x counter || true") or log().count(b"widget GET ") != previous:
        raise RuntimeError("the placeholder started an engine before consent")
    tier(False)
    frame(f"start-{previous + 1}")
    press(1074, 60)


def parent():
    return guest("pgrep -x navigator-ui && pgrep -x navigator-page")


def recycle():
    before = parent()
    tier(False)
    start(0)
    count("initial", 0)
    first = guest("pgrep -x counter")
    for n in (1, 2):
        time.sleep(6)
        press(350, 118)
        count(f"active-{n}", n)
    if guest("pgrep -x counter") != first:
        raise RuntimeError("active engine was replaced")
    wait_for("idle retirement", lambda: b"live widget stopped" in log() and not guest("pgrep -x counter || true"))
    tier(False)
    if b"retired idle island" not in guest("cat frescod.log") or b"prompt=widget-failed" in log():
        raise RuntimeError("idle retirement was not a compositor stop")
    start(1)
    count("restarted", 0)
    tier(True)
    second = guest("pgrep -x counter")
    jid = int(guest("ps -o jid= -p " + str(int(second))))
    jail = guest(f"jls -j {jid} name").decode().strip()
    rules = guest("rctl -n " + shlex.quote("jail:" + jail))
    (args.evidence / "rctl.txt").write_bytes(rules)
    if b":memoryuse:sigkill=268435456" not in rules or first == second:
        raise RuntimeError(f"unexpected counter limit or PID: {rules!r}")
    # Add a diagnostic rule to this ephemeral jail. The signed kill rule stays in force.
    witness = f"jail:{jail}:memoryuse:log=256M"
    guest("rctl -a " + shlex.quote(witness))
    for n in (1, 2):
        press(350, 118)
        count(f"before-pressure-{n}", n)
    press(350, 118)
    wait_for("memory pressure did not end worker", lambda: b"prompt=widget-failed" in log()
             and not guest("pgrep -x counter || true"))
    tier(False)
    frame("memory-exit")
    kernel = guest("dmesg")
    messages = [line for line in kernel.splitlines() if jail.encode() in line and b"memoryuse:log=268435456" in line
                and f"pid {int(second)} (counter)".encode() in line]
    if not messages:
        raise RuntimeError("missing kernel memory-pressure witness")
    (args.evidence / "pressure.txt").write_bytes(b"\n".join(messages) + b"\n")
    time.sleep(.65)  # The failure prompt has its own press guard.
    press(1074, 60)
    start(2)
    count("recovered", 0)
    tier(True)
    third = guest("pgrep -x counter")
    press(350, 118)
    count("recovered-increment", 1)
    if third in (first, second) or log().count(b"widget GET ") != 3:
        raise RuntimeError("unexpected recovery worker or fetch count")
    if parent() != before:
        raise RuntimeError("parent changed on retirement")
    print(f"Idle retired PID {int(first)} after two increments. Restarted PID {int(second)} in jail {jail}.")
    print(rules.decode().strip())


def lifecycle():
    from PIL import Image
    import re

    def edges(path):
        with Image.open(path) as png:
            image = png.convert("RGB")
            amber = (250, 206, 97)
            xs = [x for x in range(250, image.width) if image.getpixel((x, 200)) == amber]
            ys = [y for y in range(75, image.height) if image.getpixel((600, y)) == amber]
            return xs, ys

    parents = parent()
    start(0)
    count("before-resize", 0)
    tier(True)
    worker = guest("pgrep -x counter")
    bounds = edges(args.evidence / "before-resize.png")
    if bounds != ([288, 929], [80, 441]):
        raise RuntimeError(f"unexpected initial compositor frame: {bounds}")
    press(350, 118)
    count("after-resize", 1)
    match = re.search(rb"widget requested (\d+)x(\d+)", log())
    if not match or int(match[1]) < 884:
        raise RuntimeError("missing wider document extent from the worker")
    if edges(args.evidence / "after-resize.png") != bounds or guest("pgrep -x counter") != worker:
        raise RuntimeError("resize changed the slot or replaced the worker")
    press(434, 118)
    wait_for("missing voluntary close", lambda: b"widget reported voluntary close" in log()
             and not guest("pgrep -x counter || true"))
    tier(False)
    frame("closed")
    if b"prompt=widget-failed" in log():
        raise RuntimeError("voluntary close was presented as a crash")
    start(1)
    count("restarted", 0)
    tier(True)
    if guest("pgrep -x counter") == worker or parent() != parents:
        raise RuntimeError("unexpected worker or parent identity after restart")
    print(f"Requested {int(match[1])}x{int(match[2])}. The compositor kept the 640x360 slot.")
    print("Finish retired the worker. Guarded Start reopened Count 0 on the same parent.")


def unload():
    before = parent()
    start(0)
    count("opened", 0)
    tier(True)
    worker = guest("pgrep -x counter")
    for n, answer in [(1, "Stay"), (2, "Timeout"), (3, "Leave")]:
        press(350, 118)
        count(f"count-{n}", n)
        time.sleep(1.05)  # The page prompt rate limit is one per second.
        press(434, 118)
        wait_for("missing signed unload prompt", lambda: log().count(b"prompt=widget-unload ") >= n
                 and b"Leave the widget from https://widget.example?" in log())
        time.sleep(.65)
        frame(f"question-{n}")
        if answer != "Timeout":
            press(1074 if answer == "Stay" else 1008, 60)
        expected = f"widget unload {n} leave={str(answer == 'Leave').lower()}".encode()
        wait_for("missing broker decision", lambda: expected in log(), 35 if answer == "Timeout" else 12)
        if answer != "Leave":
            count(f"stayed-{n}", n)
            tier(True)
            if guest("pgrep -x counter") != worker or b"widget reported voluntary close" in log():
                raise RuntimeError("Stay retired or replaced the worker")
    wait_for("worker remained after Leave", lambda: not guest("pgrep -x counter || true"))
    tier(False)
    frame("closed")
    report = log()
    if (parent() != before or report.count(b"widget reported voluntary close") != 1
            or report.count(b"widget GET ") != 1 or b"prompt=widget-failed" in report):
        raise RuntimeError("unexpected parent, reload or retirement outcome")
    print(f"Stay preserved worker {int(worker)} at Count 1; timeout preserved Count 2. Increment reached 3, then Leave retired it exactly once.")


def pending_stop():
    def prompt(opened):
        report = log()
        traces = [line for line in report.splitlines() if line.startswith(b"navigator-ui: trace ")]
        trees = report.split(b'navigator-ui: ax window "Navigator"')
        if not traces or len(trees) == 1:
            return False
        expected = b" prompt=widget-unload " if opened else b" prompt=- "
        return expected in traces[-1] and all(
            (item in trees[-1]) == opened for item in
            (b'ax   region "Leave the widget', b'ax     button "Leave"', b'ax     button "Stay"'))

    def visible_question(name):
        image = subprocess.run(["magick", str(frame(name)), "-crop", "900x45+240+41",
                                "+repage", "-resize", "240%", "png:-"],
                               capture_output=True, check=True, timeout=5).stdout
        words = subprocess.run(["tesseract", "stdin", "stdout", "--psm", "6"], input=image,
                               capture_output=True, check=True, timeout=5).stdout.lower()
        return b"leave the widget" in words

    def question(name):
        time.sleep(1.05)
        press(434, 118)
        wait_for(name, lambda: prompt(True))
        time.sleep(.65)
        wait_for("unload question missing from frame", lambda: visible_question(name))

    parents = parent()
    start(0)
    count("first", 0)
    first = guest("pgrep -x counter")
    press(350, 118)
    count("first-edited", 1)
    question("retiring-question")
    press(905, 135)  # The prompt moves the compositor's Stop button down 44 px.
    wait_for("Stop left the old worker or its prompt", lambda: prompt(False)
             and b"live widget stopped" in log() and not guest("pgrep -x counter || true"))
    tier(False)
    wait_for("retired question still painted", lambda: not visible_question("prompt-withdrawn"))
    start(1)
    count("replacement", 0)
    second = guest("pgrep -x counter")
    if first == second:
        raise RuntimeError("guarded Start did not create a new worker")
    press(350, 118)
    count("replacement-edited", 1)
    question("replacement-question")
    press(1008, 60)  # Gate UI writes the retired Leave first, followed by the current Stay.
    wait_for("retired answer affected replacement query 1", lambda:
             b"sent retired Leave for matching document and query 1" in log()
             and b"widget unload 1 leave=false" in log() and prompt(False), 8)
    tier(True)
    press(350, 118)
    count("replacement-survived", 2)
    if guest("pgrep -x counter") != second:
        raise RuntimeError("replacement changed after the retired Leave")
    question("current-question")
    press(1008, 60)
    wait_for("current Leave failed", lambda: b"widget unload 2 leave=true" in log()
             and not guest("pgrep -x counter || true") and prompt(False))
    tier(False)
    frame("closed")
    report = log()
    if (parent() != parents or report.count(b"widget GET ") != 2
            or report.count(b"widget reported voluntary close") != 1
            or b"prompt=widget-failed" in report or b"widget unload 1 leave=true" in report):
        raise RuntimeError("unexpected parent, fetch or retirement outcome")
    print(f"Stop withdrew query 1 and worker {int(first)}. Replacement {int(second)} stayed on query 1,"
          " reached Count 2 after the retired Leave, and closed once on its own query 2.")


if args.pending_stop:
    pending_stop()
    raise SystemExit(0)


if args.unload:
    unload()
    raise SystemExit(0)


if args.lifecycle:
    lifecycle()
    raise SystemExit(0)


if args.recycle:
    recycle()
    raise SystemExit(0)


# Geometry of the 1280x840 headless fixture with a 1120x740 Navigator window.
parent_before = parent()
tier(False)
start(0)
count("initial", 0)
tier(True)
if args.tier:
    press(418, 27)
    def details_visible():
        region = subprocess.run(["magick", str(frame("details")), "-crop", "360x65+372+112",
                                 "+repage", "-resize", "300%", "-colorspace", "Gray", "-auto-level", "png:-"],
                                capture_output=True, check=True, timeout=5).stdout
        words = subprocess.run(["tesseract", "stdin", "stdout", "--psm", "6"], input=region,
                               capture_output=True, check=True, timeout=5).stdout
        return b"widget.example" in words and b"embedded part" in words
    wait_for("visible details did not name the embedded origin", details_visible)
    press(418, 27)
worker = guest("pgrep -x counter")
press(350, 118)
count("running", 1)
press(905, 91)
wait_for("Stop did not withdraw the engine", lambda: b"live widget stopped" in log()
         and not guest("pgrep -x counter || true"))
tier(False)
frame("stopped")
if parent() != parent_before or b"prompt=widget-failed" in log():
    raise RuntimeError("Stop disturbed the parent or reported a crash")
start(1)
count("restarted", 0)
tier(True)
replacement = guest("pgrep -x counter")
press(350, 118)
count("incremented", 1)
report = log()
if (worker == replacement or parent() != parent_before or report.count(b"widget GET ") != 2
        or report.count(b'the page child laid out "Embedded counter"') != 1):
    raise RuntimeError("unexpected worker, fetch count or parent reload")
print(f"Stop retired PID {worker.decode().strip()}. Guarded Start launched PID {replacement.decode().strip()}.")

if args.tier:
    guest("kill -TERM " + str(int(replacement)))
    wait_for("missing widget failure prompt", lambda: b"prompt=widget-failed" in log())
    tier(False)
    frame("failed")
    if parent() != parent_before:
        raise RuntimeError("engine failure disturbed the parent")
    print("Static parent retained. Live-part count: idle 0, offered 0, attached 1, stopped 0, restarted 1, failed 0.")

if args.reports:
    started = b"navigatord: widget reported start for https://widget.example"
    title = b"navigatord: initial widget title received (7 bytes)"
    sequence = [line for line in log().splitlines() if line.startswith((started, title))]
    if sequence != [started, title, started, title]:
        raise RuntimeError(f"unexpected counter reports: {sequence!r}")
    print("Both guarded launches reported the document origin and seven-byte title in order.")
