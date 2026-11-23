"""Exercise installed Calendar, Files, Limen and Fasti on a marked disposable guest.

Use calendar-keyboard-check.py's arguments and staging. Also stage calendar-ical,
limend, scrinium, scrinium/atrium.toml as scrinium-calendar-test.toml, and both
atrium-calendar/tests/fixtures/*.ics files.
"""
import importlib.util
import json
import shlex
import sys
import time
from pathlib import Path

spec = importlib.util.spec_from_file_location("keyboard", Path(__file__).with_name("calendar-keyboard-check.py"))
keyboard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(keyboard)


def click(ui, x, y):
    ui.run("nc -w 3 -N -U inject.sock", f"MOVE {x} {y}\nBTN 1 1\nBTN 1 0\n".encode())
    time.sleep(0.4)


def pick(ui, row, persona="work"):
    click(ui, 604, 20)
    names = ["studio.ics", "unsupported.ics"] if persona == "work" else ["personal-only.ics"]
    # Binding a sheet can precede the next headless PNG. Move over its row
    # to request a fresh compositor frame, then inspect it before choosing.
    time.sleep(0.6)
    ui.run("nc -w 3 -N -U inject.sock", b"MOVE 270 180\n")
    text = ui.see(persona + "-picker", *names)
    other = "personal-only" if persona == "work" else "unsupported.ics"
    keyboard.require(other not in text.lower(), "Picker displayed the other persona's file")
    click(ui, 280, 183 + row * 30)
    click(ui, 625, 458)


def run(ui):
    ui.run(shlex.join(["sh", "calendar-installed-guest-check.sh", ui.marker, "import-setup"]))
    ui.control("open", "work")
    ui.see("blank-work", "Work calendar", "No events on this day")
    before = ui.control("fingerprint", "work")
    pick(ui, 0)
    ui.see("preview", "Destination . Work", "3 events", "Exhibition day", "Weekly", "Edited from 2026-11-09")
    keyboard.require(ui.control("fingerprint", "work") == before, "Preview changed the schedule")
    click(ui, 72, 558)
    ui.see("cancelled", "Agenda . local time", "No events on this day")
    keyboard.require(ui.control("fingerprint", "work") == before, "Cancel changed the schedule")

    pick(ui, 1)
    ui.see("unsupported", "Import unavailable", "weekly timed series")
    keyboard.require(ui.control("fingerprint", "work") == before, "Unsupported file partially imported")
    click(ui, 88, 514)

    pick(ui, 0)
    ui.see("before-save", "Studio review", "Moved studio review")
    click(ui, 575, 558)
    ui.see("saved", "Local copies saved", "Studio review", "Undo import")
    state = ui.store("work")
    series, all_day, moved = state["entries"]
    keyboard.require(state["revision"] == 1 and series["excluded"] == ["2026-11-09", "2026-11-16"]
                     and moved["kind"] == {"Occurrence": {"series": series["id"], "original": "2026-11-09"}}
                     and moved["date"] == "2026-11-10" and all_day["kind"] == "AllDay",
                     "Confirmed schedule does not match its preview")
    ui.control("close", "work")
    ui.control("restart", "work")
    ui.control("open", "work")
    ui.see("reopened-work", "Today", "Work calendar")
    pick(ui, 0)
    ui.see("duplicate", "already has a local copy", "Open existing day")
    click(ui, 250, 513)
    ui.see("existing", "2026-11-02", "Studio review")
    ui.open_date("2026-11-09")
    ui.see("old-date", "No events on this day")
    ui.open_date("2026-11-10")
    ui.see("moved", "Moved studio review", "16:00")
    ui.open_date("2026-11-23")
    ui.see("sibling", "Studio review", "14:00")
    keyboard.require(ui.store("work") == state, "Reopening created a duplicate or changed the series")
    ui.control("close", "work")
    ui.control("open", "personal")
    ui.see("personal-first", "Personal calendar", "No events on this day")
    pick(ui, 0, "personal")
    ui.see("personal-preview", "Destination . Personal", "3 events")
    click(ui, 72, 558)
    ui.see("personal-cancel", "No events on this day")
    keyboard.require(ui.control("fingerprint", "personal").strip() == b"empty", "Wrong persona or implicit import")
    ui.control("close", "personal")
    ports = ui.run("cat portcullis.log").decode()
    keyboard.require("worker org.atrium.calendar/ical" in ports and "removing worker jail" in ports,
                     "No evidence of jailed format parsing")
    (ui.evidence / "work-snapshot.json").write_text(json.dumps(state, indent=2))
    print("PASS reviewed import, cancellation, unsupported file, atomic save, recurrence, restart and persona separation")


if __name__ == "__main__":
    if len(sys.argv) < 6:
        sys.exit("Usage: calendar-import-check.py GUEST_DIR MARKER EVIDENCE_DIR ssh [options] HOST")
    ui = keyboard.Desktop(*sys.argv[1:4], sys.argv[4:])
    run(ui)
