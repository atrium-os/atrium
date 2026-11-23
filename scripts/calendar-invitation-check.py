"""Installed synthetic Mail → Calendar → Mail RSVP on a marked disposable guest.

Use calendar-keyboard-check.py's arguments and staging, plus the matching
atrium-mail, mail-parse-jail, calendar-ical and Mail's invitation_probe example.
"""
import importlib.util
import shlex
import sys
import time
from pathlib import Path

spec = importlib.util.spec_from_file_location("keyboard", Path(__file__).with_name("calendar-keyboard-check.py"))
keyboard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(keyboard)


def click(ui, x, y):
    ui.run("nc -w 3 -N -U inject.sock", f"MOVE {x} {y}\nBTN 1 1\nBTN 1 0\n".encode())
    time.sleep(0.6)


def guest(ui, action):
    return ui.run(shlex.join(["sh", "calendar-installed-guest-check.sh", ui.marker, action])).decode()


def open_mail(ui, first=False):
    if first:
        click(ui, 996, 23)  # Done in the unconfigured synthetic account's Settings.
    click(ui, 900, 415)  # Bring Mail forward if Calendar is over part of its window.


def review(ui, y):
    click(ui, 330, y)
    click(ui, 754, 185)


def run(ui):
    print(guest(ui, "invitation-setup"))
    assert ui.control("fingerprint", "personal").strip() == b"empty"
    open_mail(ui, first=True)
    review(ui, 248)
    ui.see("review-original", "Review invitation", "14:00", "Studio A", "Work calendar")
    assert ui.control("fingerprint", "work").strip() == b"empty"
    click(ui, 568, 550)
    ui.see("saved-original", "Invitation saved on this device", "14:00")
    saved = ui.store("work")
    # The headless compositor has no Forum window manager. Close Calendar to
    # return to Mail and exercise the persisted meeting on the next launch.
    ui.control("close", "work")
    open_mail(ui)
    review(ui, 178)
    ui.see("review-change", "Current", "14:00", "Studio A", "Proposed", "15:00", "Studio B")
    assert ui.store("work") == saved
    click(ui, 80, 462)
    ui.see("response-before-cancel", "Review your invitation response", "ACCEPTED", "To organizer")
    click(ui, 70, 323)
    ui.control("close", "work")
    print(guest(ui, "invitation-check-empty"))
    open_mail(ui, first=True)
    review(ui, 178)
    ui.see("review-again", "Review invitation change", "14:00", "15:00")
    click(ui, 80, 462)
    ui.see("response-before-send", "Review your invitation response", "ACCEPTED", "15:00")
    click(ui, 487, 322)
    assert ui.store("work") == saved
    print(guest(ui, "invitation-submit"))
    assert ui.control("fingerprint", "personal").strip() == b"empty"
    print("PASS preview, local save, reschedule comparison, Cancel, Send, SMTP receipt and replay")


if __name__ == "__main__":
    if len(sys.argv) < 6:
        sys.exit("Usage: calendar-invitation-check.py GUEST_DIR MARKER EVIDENCE_DIR ssh [options] HOST")
    run(keyboard.Desktop(*sys.argv[1:4], sys.argv[4:]))
