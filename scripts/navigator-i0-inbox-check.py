#!/usr/bin/env python3
"""Check the running inbox fixture through the guest's Fresco display and input socket."""
import argparse
import csv
import io
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ssh", help="guest command wrapper, such as scripts/vssh")
    parser.add_argument("requests", type=Path, help="fixture provider's request log")
    parser.add_argument("output", type=Path, help="directory for before and after PNGs")
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
