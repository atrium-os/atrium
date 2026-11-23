"""Drive the installed Calendar on a freshly staged, marked disposable guest.

Run on the host with Pillow and tesseract. Arguments are the guest staging
directory, marker, local evidence directory, and SSH command ending in its
destination. Stage the same files as calendar-installed-guest-check.sh first.
"""
import contextlib
import io
import json
import re
import shlex
import subprocess
import sys
import time
from pathlib import Path

from PIL import Image, ImageChops


FOCUS_STROKE = (189, 127, 58)
FOCUS_HALO = (244, 222, 192)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class Desktop:
    def __init__(self, directory, marker, evidence, ssh):
        self.directory = directory
        self.marker = marker
        self.evidence = Path(evidence)
        self.evidence.mkdir(parents=True, exist_ok=True)
        self.ssh = ssh

    def run(self, command, data=None):
        return subprocess.run(
            [*self.ssh, f"cd {shlex.quote(self.directory)} && {command}"],
            input=data, stdout=subprocess.PIPE, check=True, timeout=60,
        ).stdout

    def control(self, action, *args):
        return self.run(shlex.join([
            "sh", "calendar-installed-guest-check.sh", self.marker,
            "keyboard-" + action, *args,
        ]))

    def frame(self):
        # The compositor can be replacing the PNG during the transfer.
        for _ in range(12):
            try:
                raw = self.run("cat screen.png")
                with Image.open(io.BytesIO(raw)) as image:
                    return image.convert("RGB"), raw
            except (OSError, subprocess.CalledProcessError):
                time.sleep(0.1)
        raise RuntimeError("No complete compositor frame")

    def keys(self, *strokes):
        lines = []
        for code, modifiers in strokes:
            for down in (1, 0):
                lines.append(f"KEY {code} {down} {modifiers}\n")
        self.run("nc -w 3 -N -U inject.sock", "".join(lines).encode())
        time.sleep(0.18)

    def focus(self, x, y):
        # Sample the top of the rendered Pergola focus ring. Coordinates are
        # observation points. Tab is the only way this driver changes focus.
        image, _ = self.frame()
        for _ in range(48):
            if image.getpixel((x, y - 1)) == FOCUS_STROKE:
                return
            previous = self.ring(image)
            self.keys((43, 0))
            for _ in range(30):
                image, _ = self.frame()
                if self.ring(image) != previous:
                    break
                time.sleep(0.05)
            else:
                raise RuntimeError(f"Focus did not advance from {previous}")
        raise RuntimeError(f"Tab did not reach control at ({x}, {y})")

    @staticmethod
    def ring(image):
        # Only the focus ring has this solid stroke above the control and a
        # two-pixel halo above it. Wait for a *new* ring after each Tab.
        region = image.crop((0, 0, 690, 592))

        def color_mask(rgb):
            planes = [band.point([255 if i == value else 0 for i in range(256)])
                      for band, value in zip(region.split(), rgb)]
            return ImageChops.darker(ImageChops.darker(*planes[:2]), planes[2])

        stroke = color_mask(FOCUS_STROKE)
        halo = ImageChops.offset(color_mask(FOCUS_HALO), 0, 1)
        return ImageChops.darker(stroke, halo).getbbox()

    def expect_tab(self, x, y, backwards=False):
        self.keys((43, 2 if backwards else 0))
        for _ in range(30):
            image, _ = self.frame()
            if image.getpixel((x, y - 1)) == FOCUS_STROKE:
                return
            time.sleep(0.05)
        raise RuntimeError(f"Single Tab did not reach ({x}, {y})")

    def activate(self, x, y):
        self.focus(x, y)
        self.keys((40, 0))

    def field(self, x, y, value):
        self.focus(x, y)
        strokes = [(4, 1)]  # Ctrl+A replaces the whole field.
        for ch in value:
            if ch.isalpha() and ch.isascii():
                strokes.append((ord(ch.lower()) - ord("a") + 4, 2 if ch.isupper() else 0))
            elif ch.isdigit():
                strokes.append((30 + (int(ch) - 1) % 10, 0))
            else:
                strokes.append({" ": (44, 0), "-": (45, 0), ":": (51, 2)}[ch])
        self.keys(*strokes)

    def see(self, name, *labels):
        path = self.evidence / (name + ".png")
        for _ in range(6):
            image, raw = self.frame()
            path.write_bytes(raw)
            window = io.BytesIO()
            image.crop((0, 0, 700, 600)).resize((1400, 1200), Image.Resampling.LANCZOS).save(window, "PNG")
            text = subprocess.run(
                ["tesseract", "stdin", "stdout", "--psm", "11"], input=window.getvalue(),
                capture_output=True, check=True, timeout=10,
            ).stdout.decode()
            if all(re.search(label, text, re.I) for label in labels):
                print(f"PASS {name}", flush=True)
                return text
            time.sleep(0.2)
        raise RuntimeError(f"{name}: expected {labels} on screen, read {text!r}")

    def store(self, persona):
        return json.loads(self.control("store", persona))

    def open_date(self, value):
        self.field(75, 128, value)
        self.activate(80, 168)
        self.see("day-" + value, rf"(?:Today|Day) . {value}")


def plan(ui):
    review = "2026-12-01"
    ui.control("setup")
    ui.control("open", "work")
    ui.see("work-first-launch", "Today", "Work calendar", "No events on this day")
    ui.open_date(review)
    ui.field(260, 128, "Release review")
    ui.field(462, 128, "14:00")
    ui.field(555, 128, "14:45")
    ui.activate(330, 168)
    ui.control("review", "work", review, "--original-review")
    ui.open_date("2026-12-02")
    ui.field(260, 128, "Field day")
    ui.activate(450, 168)
    ui.see("all-day", "All day", "Field day")
    ui.open_date(review)

    before = ui.store("work")
    ui.activate(580, 168)
    ui.see("find", "Open for 300 minutes", "Hold 45 min")
    require(ui.store("work") == before, "Finding time changed the Work store")
    ui.activate(590, 254)
    ui.activate(580, 168)  # Show agenda after the explicit hold.
    ui.see("held", "Private hold", "Preparation", "Release review")
    held = ui.store("work")
    entries = held["entries"]
    require(len(entries) == 3, "Expected exactly three Work entries")
    expected = {
        ("Release review", review, "Weekly", 840, 885),
        ("Preparation", review, "Hold", 540, 585),
        ("Field day", "2026-12-02", "AllDay", 0, 1440),
    }
    require({(e["title"], e["date"], e["kind"], e["span"]["start"], e["span"]["end"])
             for e in entries} == expected, "Wrong dates, kinds or times in Work")

    ui.focus(590, 292)  # Last agenda control, Whole series.
    ui.expect_tab(590, 548)  # Footer follows the agenda.
    ui.expect_tab(590, 292, backwards=True)
    ui.activate(450, 292)  # Second agenda row, Edit occurrence.
    ui.field(225, 256, "15:00")
    ui.field(330, 256, "15:45")
    ui.activate(90, 325)
    ui.control("review", "work", review, "--moved-review")
    ui.activate(580, 168)
    ui.see("moved-gaps", "Open for 315 minutes")
    ui.activate(455, 44)  # Search the month and open the following occurrence.
    ui.see("search-open", "Event title", "Through", "2026-12-31")
    ui.field(160, 129, "release")
    ui.keys((40, 0))
    ui.see("weekly-search", r"2026-12-01\s+15:00", r"2026-12-08\s+14:00")
    ui.activate(610, 274)
    ui.see("following-week", "(?:Day|Today) . 2026-12-08", "14:00.14:45 Weekly . Release review")
    ui.activate(590, 548)
    ui.control("review", "work", review, "--original-review")
    require(ui.store("work")["entries"] == entries, "Undo changed other records or identities")
    ui.see("undone", "Change undone", "14:00", "Private hold")

    ui.activate(605, 44)
    ui.activate(180, 178)  # Set Week as Work's opening view.
    ui.activate(152, 44)
    ui.see("work-week", "Week of", "00:00", "12:00")
    ui.activate(56, 44)
    ui.see("work-browsing-today", r"Today . \d{4}-\d{2}-\d{2}", "Agenda . local time")
    work = ui.store("work")
    require(work["opening_view"] == "Week", "Browsing lost the Work default")
    ui.control("close", "work")
    ui.control("restart", "work")

    ui.control("open", "personal")
    ui.see("personal-first-launch", "Today", "Personal calendar", "No events on this day")
    ui.open_date(review)
    ui.see("personal-on-review-day", review, "No events on this day")
    ui.field(260, 128, "Piano lesson")
    ui.activate(210, 168)
    ui.activate(605, 44)
    ui.activate(280, 178)  # Set Month as Personal's opening view.
    ui.activate(240, 44)
    ui.see("personal-month", "December 2026", r"\bMon\b", r"\bTue\b")
    ui.activate(56, 44)
    ui.see("personal-browsing-today", r"Today . \d{4}-\d{2}-\d{2}", "Agenda . local time")
    personal = ui.store("personal")
    require(personal["opening_view"] == "Month"
            and [(e["title"], e["date"]) for e in personal["entries"]] == [("Piano lesson", review)],
            "Unexpected Personal records or default")
    ui.control("close", "personal")

    ui.control("open", "work")
    ui.see("work-reopened", "Work calendar", "Week of", "00:00", "12:00")
    ui.control("review", "work", review, "--original-review")
    require(ui.store("work") == work, "Work changed across restart or Personal planning")
    ui.activate(56, 44)
    ui.open_date(review)
    ui.see("work-after-restart", "09:00.09:45 Private hold", "14:00.14:45 Weekly . Release review")
    ui.control("close", "work")
    ui.control("open", "personal")
    month = ui.run("date -u '+%B %Y'").decode().strip()
    ui.see("personal-reopened", "Personal calendar", month, r"\bMon\b", r"\bTue\b", r"\bWed\b")
    ui.activate(56, 44)
    ui.open_date(review)
    ui.see("personal-after-reopen", "Piano lesson", "09:00.09:30")
    require(ui.store("personal") == personal, "Personal changed across reopen")
    ui.control("close", "personal")
    log = ui.run("cat fresco.log")
    (ui.evidence / "fresco.log").write_bytes(log)
    events = [line for line in log.splitlines() if b"frescod: inject " in line]
    require(events and all(re.fullmatch(rb"frescod: inject KEY \d+ [01] -> window [1-9]\d*", line)
                           for line in events), "Unexpected input or key routed to no window")
    windows = {line.split()[-1] for line in events}
    require(len(windows) == 4, "Expected keyboard input in each of the four windows")
    print(f"PASS {len(events)} compositor key events across four window launches", flush=True)


if __name__ == "__main__":
    if len(sys.argv) < 6:
        sys.exit("Usage: calendar-keyboard-check.py GUEST_DIR MARKER EVIDENCE_DIR ssh [options] HOST")
    desktop = Desktop(*sys.argv[1:4], sys.argv[4:])
    try:
        plan(desktop)
    except Exception:
        with contextlib.suppress(OSError, RuntimeError, subprocess.SubprocessError):
            _, raw = desktop.frame()
            (desktop.evidence / "failure.png").write_bytes(raw)
        raise
