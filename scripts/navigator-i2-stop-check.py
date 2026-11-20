#!/usr/bin/env python3
"""Verify Stop in an installed Navigator window on the staged I1 counter fixture."""
import argparse
import csv
import io
import json
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
parser.add_argument("--navigate-away", action="store_true",
                    help="follow the parent link while the unload prompt is open, then go Back")
parser.add_argument("--parked-viewer", action="store_true",
                    help="continue to the pond with NAVIGATOR_UI_IDLE_S=5, then go Back twice")
parser.add_argument("--saved-record", type=Path, metavar="TLS_LOG",
                    help="fill the broker cache after parking and verify a saved-record restore against the fixture log")
parser.add_argument("--broker-restart", type=Path, metavar="TLS_LOG",
                    help="use a fresh persistent broker without NAVIGATOR_UI_OPEN and relaunch on the saved trail")
parser.add_argument("--unrecorded-restart", type=Path, metavar="TLS_LOG",
                    help="restart with an Unrecorded widget page selected alongside the saved Personal trail")
args = parser.parse_args()
if args.unrecorded_restart:
    args.broker_restart = args.unrecorded_restart
if args.broker_restart:
    args.navigate_away = True
if args.saved_record:
    args.parked_viewer = True
if args.reports or args.lifecycle or args.unload or args.pending_stop or args.navigate_away or args.parked_viewer:
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
        result = check()
        if result:
            return result
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
    before = len(log())
    press(390, 180)
    wait_for("missing Start prompt", lambda: b"prompt=widget " in log()[before:])
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


def pending_stop():
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


def page_word(name, word, top, height):
    # Restrict OCR to the document slot so rail and toolbar labels cannot satisfy it.
    image = subprocess.run(["magick", str(frame(name)), "-crop", f"870x{height}+249+{top}",
                            "+repage", "-resize", "300%", "png:-"],
                           capture_output=True, check=True, timeout=5).stdout
    data = subprocess.run(["tesseract", "stdin", "stdout", "--psm", "11", "tsv"], input=image,
                          capture_output=True, check=True, timeout=5).stdout.decode()
    for row in csv.DictReader(io.StringIO(data), delimiter="\t"):
        if row["text"].lower() == word:
            x = int(row["left"]) + int(row["width"]) // 2
            y = int(row["top"]) + int(row["height"]) // 2
            return 249 + x // 3, top + y // 3
    return None


def back_to(path):
    before = len(log())
    press(270, 30)
    expected = f" url=https://parent.example/{path} live=shown ".encode()
    wait_for(f"Back did not show {path}", lambda: expected in log()[before:])


def fill_cache_and_return():
    # Each page has its own URL and sends a real link activation through the viewer.
    for number in range(1, 17):
        link = wait_for(f"missing link to flowerbed {number}",
                        lambda: page_word("flowerbeds", "flowerbeds", 41, 160))
        press(*link)
        title = f'the page child laid out "Flowerbed {number}"'.encode()
        wait_for(f"flowerbed {number} failed to load", lambda: title in log())
    for number in range(15, 0, -1):
        back_to(f"bed-{number}.html")
    back_to("pond.html")


def return_from_pond(original_viewer):
    pond = page_word("garden", "pond", 41, 180)
    if not pond:
        raise RuntimeError("missing Visit pond link")
    press(*pond)
    wait_for("pond page did not render", lambda: page_word("pond", "tadpoles", 41, 150))
    parked = b"navigator-ui: parked https://parent.example/counter-parent.html at y="
    wait_for("counter page did not finish parking", lambda: parked in log()
             and original_viewer not in guest("pgrep -x navigator-page").splitlines(), 20)
    if guest("pgrep -x counter || true"):
        raise RuntimeError("parking restarted the widget")
    if args.saved_record:
        fill_cache_and_return()
    back_to("away.html")
    wait_for("Back did not render the garden", lambda: page_word("garden-back", "marigolds", 41, 150))
    return set(guest("pgrep -x navigator-page").splitlines())


def open_address(address, title):
    punct = {".": 55, "/": 56, "-": 45}
    codes = [punct[c] if c in punct else ord(c) - ord("a") + 4 for c in address]
    # Focus the address field and type as a user would, so relaunch has no forced URL.
    events = "KEY 15 1 1\nKEY 15 0 1\n"
    for key in codes + [40]:
        events += f"KEY {key} 1\nKEY {key} 0\n"
    guest("nc -N -U input.sock", events.encode())
    wait_for(f"{title} did not open", lambda: f'the page child laid out "{title}"'.encode() in log())


def open_unrecorded_widget():
    before = len(log())
    guest("nc -N -U input.sock", b"KEY 17 1 3\nKEY 17 0 3\n")  # Ctrl-Shift-N
    wait_for("Unrecorded trail did not open", lambda: b'Start, Unrecorded' in log()[before:])
    open_address("parent.example/private-counter.html", "Private counter")
    wait_for("private page content missing", lambda: page_word("private-page", "marionette", 441, 100))
    press(390, 180)
    def unavailable():
        tree = log().split(b'navigator-ui: ax window "Navigator"')[-1]
        return all(label in tree for label in (b'Private counter, Unrecorded', b'Garden notes',
                   b'Live widget from https://widget.example is unavailable', b'button "Close"')) and b'button "Start"' not in tree
    wait_for("missing Unrecorded widget notice or Personal trail", unavailable)
    if guest("pgrep -x counter || true") or log().count(b"widget GET ") != 1:
        raise RuntimeError("Unrecorded widget started or fetched")
    frame("private-selected")


def check_recorded_files(root):
    # The ordinary page record must be discoverable by the same probe.
    store = shlex.quote(root + "/var/db/navigatord/store")
    for marker, expected in (("https://parent.example/away.html", 0),
                             ("private-counter.html", 1), ("Private marionette", 1)):
        status = guest(f"grep -aRFq {shlex.quote(marker)} {store}; echo $?")
        if int(status) != expected:
            raise RuntimeError(f"unexpected persistent store search result for {marker}: {status!r}")


def restart_on_garden():
    broker = int(guest("pgrep -x navigatord"))
    root = guest(f'jls -j $(ps -o jid= -p {broker}) path').decode().strip()
    trail_path = shlex.quote(root + "/var/db/navigatord/trails.json")

    def saved():
        data = guest(f"cat {trail_path} 2>/dev/null || true")
        if not data:
            return False
        record = json.loads(data)
        if args.unrecorded_restart and (len(record["trails"]) != 1 or record["earlier"]
                                       or b"private-counter.html" in data or b"Private counter" in data):
            raise RuntimeError("Unrecorded material was written to the trail file")
        trail = record["trails"][record["current"]]
        pages = trail["pages"]
        if (len(pages) != 2 or trail["current"] != 1 or pages[1].get("parent") != 0
                or [page["url"] for page in pages] != ["https://parent.example/counter-parent.html",
                                                       "https://parent.example/away.html"]
                or not all(page.get("saved") for page in pages)):
            return False
        (args.evidence / "saved-trail.json").write_bytes(data)
        return True

    wait_for("trail cursor and page record addresses were not persisted", saved)
    if args.unrecorded_restart:
        check_recorded_files(root)
    guest(f"kill -TERM {broker}")
    wait_for("old broker or UI remained", lambda: not guest("pgrep -x 'navigatord|navigator-ui' || true"))
    args.log += ".restart"
    guest("daemon -f -o " + shlex.quote(args.log) +
          ' su -m mailgate -c "/tmp/mail-button/portcullis launch --no-prompt org.atrium.navigator"')
    wait_for("new broker did not restore garden from the store", lambda:
             b"page https://parent.example/away.html brought back from the store" in log())
    if int(guest("pgrep -x navigatord")) == broker or not prompt(False):
        raise RuntimeError("broker identity or restored garden prompt is wrong")
    wait_for("restored garden not visible", lambda: page_word("relaunched", "marigolds", 41, 150))
    print(f"Persistent trail reopened at Garden in new broker {int(guest('pgrep -x navigatord'))} (was {broker}).")


def navigate_away():
    if args.broker_restart:
        open_address("parent.example/counter-parent.html", "Embedded counter")
    traffic_log = args.saved_record or args.broker_restart
    if traffic_log:
        parent_get = b'"GET /counter-parent.html HTTP/1.1"'
        requests = traffic_log.read_bytes()
        if parent_get not in requests:
            raise RuntimeError("fixture log did not record the initial parent GET")
    original_ui, original_viewer = parent().splitlines()
    start(0)
    count("first", 0)
    first = guest("pgrep -x counter")
    press(350, 118)
    count("first-edited", 1)
    question("pending")
    link = page_word("pending", "garden", 485, 80)
    if not link:
        raise RuntimeError("missing Visit garden link below the counter")
    press(*link)
    wait_for("destination failed to render", lambda: page_word("away", "marigolds", 41, 150))
    wait_for("navigation left the worker or its prompt", lambda: prompt(False)
             and not guest("pgrep -x counter || true"))
    wait_for("old question still painted on destination", lambda: not visible_question("away"))
    existing = return_from_pond(original_viewer) if args.parked_viewer else set()
    if args.unrecorded_restart:
        open_unrecorded_widget()
        private_traffic = traffic_log.read_bytes()
        private_get = b'"GET /private-counter.html HTTP/1.1"'
        if private_traffic[len(requests):].count(private_get) != 1:
            raise RuntimeError("expected one private page GET")
    if args.broker_restart:
        restart_on_garden()
        if args.unrecorded_restart:
            post = log()
            if (b"private-counter.html" in post or b"Private counter" in post or b"widget GET " in post
                    or b"trails=1 " not in post or guest("pgrep -x counter || true")):
                raise RuntimeError("private page, trail or worker returned on relaunch")
        new_ui = guest("pgrep -x navigator-ui").strip()
        if new_ui == original_ui:
            raise RuntimeError("expected a new Navigator UI")
        existing = set(guest("pgrep -x navigator-page").splitlines())
    else:
        new_ui = original_ui
    press(270, 30)  # Back, below the headless shell strip
    url = b"https://parent.example/counter-parent.html"
    expected_layouts = 2 if args.parked_viewer else 1
    if args.parked_viewer or args.broker_restart:
        source = b" brought back from the store" if traffic_log else b" opened from navigatord's cache"
        def rebuilt():
            report = log()
            return (report.count(b'the page child laid out "Embedded counter"') == expected_layouts
                    and b"page " + url + source in report)
        wait_for("Back did not rebuild the parked page from its recorded content", rebuilt)
        new_viewers = set(guest("pgrep -x navigator-page").splitlines()) - existing
        if len(new_viewers) != 1 or original_viewer in new_viewers:
            raise RuntimeError(f"expected one new viewer after Back, got {new_viewers!r}")
        viewer = new_viewers.pop()
    else:
        wait_for("Back did not restore the existing viewer", lambda: b"showed live page " + url in log())
        viewer = original_viewer
    wait_for("restored page content missing", lambda: page_word("restored", "garden", 441, 80))
    tier(False)
    if (not prompt(False) or visible_question("restored") or guest("pgrep -x counter || true")
            or viewer not in guest("pgrep -x navigator-page").splitlines()):
        raise RuntimeError("Back failed to restore the idle page without its old prompt")
    start(0 if args.broker_restart else 1)
    count("fresh", 0)
    second = guest("pgrep -x counter")
    tier(True)
    press(350, 118)
    count("fresh-edited", 1)
    question("fresh-question")
    press(1008, 60)
    wait_for("fresh Leave failed", lambda: b"widget unload 1 leave=true" in log()
             and prompt(False) and not guest("pgrep -x counter || true"))
    tier(False)
    report = log()
    if (first == second or guest("pgrep -x navigator-ui").strip() != new_ui
            or report.count(b'the page child laid out "Embedded counter"') != expected_layouts
            or report.count(b"widget GET ") != (1 if args.broker_restart else 2) or report.count(b"widget reported voluntary close") != 1
            or b"prompt=widget-failed" in report or b"widget unload 1 leave=false" in report):
        raise RuntimeError("unexpected parent reload, widget fetch, timeout, or worker retirement")
    if traffic_log:
        traffic = traffic_log.read_bytes()
        if not traffic.startswith(requests):
            raise RuntimeError("fixture log changed during the run")
        if args.unrecorded_restart and (not traffic.startswith(private_traffic)
                                        or private_get in traffic[len(private_traffic):]
                                        or b"private-counter.html" in report):
            raise RuntimeError("Unrecorded page returned after restart")
        delta = traffic[len(requests):]
        if parent_get in delta or (args.saved_record and any(f'"GET /bed-{n}.html HTTP/1.1"'.encode() not in delta for n in range(1, 17))):
            raise RuntimeError("unexpected parent refetch or missing cache-fill request")
        print("Back rebuilt the parent from its saved record without a parent GET.")
    print(f"Navigation retired {int(first)} with its question open. Original viewer {int(original_viewer)}, viewer after Back {int(viewer)}."
          f" Guarded Start opened worker {int(second)} at Count 0; it incremented and closed on its own Leave.")


if args.navigate_away or args.parked_viewer:
    navigate_away()
    raise SystemExit(0)


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
