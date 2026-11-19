#!/usr/bin/env python3
"""Check the running inbox fixture through the guest's Fresco display and input socket."""
import argparse
import csv
import io
import hashlib
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ssh", help="guest command wrapper, such as scripts/vssh")
    parser.add_argument("requests", type=Path, help="fixture provider's request log")
    parser.add_argument("output", type=Path, help="directory for before and after PNGs")
    parser.add_argument("--where", type=Path, metavar="HOST_WORKER",
                        help="start from the blank 960 × 540 Navigator window and verify the installed worker")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)

    def remote(command, data=None):
        return subprocess.run([args.ssh, command], input=data, stdout=subprocess.PIPE,
                              check=True, timeout=20).stdout

    def snapshot(name):
        path = args.output.resolve() / (name + ".png")
        path.write_bytes(remote("cat /tmp/inbox/screen.png"))
        tsv = subprocess.check_output(["tesseract", str(path), "stdout", "--psm", "11", "tsv"],
                                      stderr=subprocess.DEVNULL, text=True)
        words = list(csv.DictReader(io.StringIO(tsv), delimiter="\t", quoting=csv.QUOTE_NONE))
        return path, words, " ".join(w["text"] for w in words if w["text"])

    def await_screen(name, expected):
        end = time.monotonic() + 20
        while True:
            path, words, text = snapshot(name)
            if expected in text:
                return path, words, text
            if time.monotonic() >= end:
                raise SystemExit(f"{expected!r} missing from {name}: {text!r}")
            time.sleep(.25)

    if args.where:
        worker = "/var/lib/atrium/apps/org.atrium.navigator.inbox-page/bin/inbox-page"
        with args.where.open("rb") as host:
            digest = hashlib.file_digest(host, "sha256").hexdigest()
        if remote("sha256 -q " + worker).decode().strip() != digest:
            raise SystemExit("Installed engine differs from host worker")
        initial = args.requests.read_text()
        if "refused" not in initial.splitlines():
            raise SystemExit("Run the cookie-free API check first")

        def inject(lines):
            remote("nc -N -U /tmp/inbox/inject.sock", ("\n".join(lines) + "\n").encode())

        def await_trace(part):
            end = time.monotonic() + 15
            while time.monotonic() < end:
                last = remote("grep 'navigator-ui: trace' /tmp/inbox/nav.log | tail -1").decode()
                if part in last:
                    return
                time.sleep(.1)
            raise SystemExit(f"Missing Navigator UI state {part!r}")

        def assert_idle():
            if args.requests.read_text() != initial or remote("pgrep -x inbox-page || true").strip():
                raise SystemExit("The fixture ran before the guarded press")

        await_trace('where=""')
        assert_idle()
        keys = []
        for c in "https://inbox.example/inbox\n":
            hid, mod = (ord(c) - ord("a") + 4, 0) if c.isalpha() else {
                ":": (51, 2), "/": (56, 0), ".": (55, 0), "\n": (40, 0)
            }[c]
            keys.extend([f"KEY {hid} 1 {mod}", f"KEY {hid} 0 {mod}"])
        inject(keys)
        await_trace("prompt=run-live")
        time.sleep(.75)  # Fresco requires the guard to be still for 500 ms.
        snapshot("offer")
        assert_idle()
        inject(["MOVE 822 63", "BTN 1 1", "BTN 1 0"])
        await_trace("prompt=- rail=248 url=https://inbox.example/inbox")
        assert_idle()
        inject(["KEY 21 1 1", "KEY 21 0 1"])  # Ctrl-R
        await_trace("prompt=run-live")
        time.sleep(.75)
        assert_idle()
        inject(["MOVE 908 63", "BTN 1 1", "BTN 1 0"])

    before, _, text = await_screen("before", "School field trip")
    if "New draft" in text:
        raise SystemExit("Draft already visible before Compose")
    # At least one entire scanline's worth of the fixture's unique heading color.
    pixels = subprocess.check_output(["magick", str(before), "-depth", "8", "rgb:-"])
    heading = bytes.fromhex("84c7a1")
    green = sum(pixels[i:i + 3] == heading for i in range(0, len(pixels), 3))
    if green < 640:
        raise SystemExit(f"External stylesheet heading missing ({green} matching pixels)")
    # OCR the small native button without its border at the fixture's fixed viewport.
    label = subprocess.check_output(["magick", str(before), "-crop", "68x18+19+124",
                                     "+repage", "-resize", "400%", "png:-"])
    name = subprocess.check_output(["tesseract", "stdin", "stdout", "--psm", "7"],
                                   input=label, stderr=subprocess.DEVNULL).decode().strip()
    if name != "Compose":
        raise SystemExit(f"Expected Compose at the click target, saw {name!r}")
    x, y = 53, 133
    remote("nc -N -U /tmp/inbox/inject.sock", f"MOVE {x} {y}\nBTN 1 1\nBTN 1 0\n".encode())
    await_screen("after", "New draft")
    markers = args.requests.read_text().splitlines()
    if "refused" not in markers or "document" not in markers:
        raise SystemExit("Missing unauthenticated control or document request")
    last_visit = markers[len(markers) - 1 - markers[::-1].index("document"):]
    if sorted(last_visit) != sorted(["document", "stylesheet", "script", "authenticated inbox"]):
        raise SystemExit(f"Unexpected requests during the latest visit: {last_visit}")
    print(f"Authenticated subject, stylesheet ({green} pixels) and Compose ({x}, {y}) verified")


if __name__ == "__main__":
    main()
