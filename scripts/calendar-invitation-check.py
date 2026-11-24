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
    # Mail puts first-contact information above this button for a new sender.
    for _ in range(8):
        frame, _ = ui.frame()
        for top in range(165, 280):
            red, green, blue = frame.getpixel((660, top))[:3]
            if red < 40 and green > 95 and blue > 95:
                click(ui, 754, top + 8)
                return
        time.sleep(0.15)
    raise RuntimeError("The invitation review button did not appear")


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
    ui.control("close", "work")
    print(guest(ui, "invitation-submit"))
    assert ui.control("fingerprint", "personal").strip() == b"empty"
    print("PASS preview, local save, reschedule comparison, Cancel, Send, SMTP receipt and replay")


def series(ui):
    print(guest(ui, "series-setup"))
    open_mail(ui, first=True)

    def open_item(id, name, *labels):
        review(ui, 178 + (6 - id) * 70)
        ui.see(name, *labels)

    def apply(name, *labels):
        click(ui, 568, 550)
        ui.see(name, *labels)
        ui.control("close", "work")

    open_item(1, "weekly-invitation", "Whole weekly series", "14:00", "Save whole series")
    assert ui.control("fingerprint", "work").strip() == b"empty"
    apply("weekly-saved", "Invitation saved", "14:00")
    master = ui.store("work")
    open_item(2, "move-one", "One occurrence", "2026-12-08", "2026-12-09", "15:00")
    assert ui.store("work") == master
    click(ui, 80, 462)
    ui.see("occurrence-response", "One occurrence", "2026-12-08", "2026-12-09", "ACCEPTED")
    click(ui, 487, 355)
    ui.control("close", "work")
    print(guest(ui, "series-submit"))
    assert ui.store("work") == master
    open_mail(ui, first=True)
    open_item(2, "review-move-again", "One occurrence", "15:00")
    apply("moved", "Invitation saved", "15:00")
    open_item(3, "change-series", "Whole weekly series", "14:00", "16:00")
    apply("series-changed", "Invitation saved", "16:00")
    after_change = ui.store("work")
    rows = after_change["entries"]
    assert len(rows) == 2
    assert any(r["kind"] == "Weekly" and r["span"]["start"] == 960 and r["excluded"] == ["2026-12-08"] for r in rows)
    assert any(r["date"] == "2026-12-09" and r["span"]["start"] == 900 for r in rows)

    open_item(6, "spoofed-cancellation", "Review cancellation", "sender or reply address differs")
    click(ui, 568, 550)
    assert ui.store("work") == after_change
    ui.control("close", "work")
    open_item(4, "cancel-occurrence", "Review cancellation", "One occurrence", "2026-12-08", "2026-12-09", "Remove this occurrence")
    assert ui.store("work") == after_change
    apply("one-cancelled", "Cancellation applied", "No events")
    remaining = ui.store("work")["entries"]
    assert len(remaining) == 1 and remaining[0]["excluded"] == ["2026-12-08"] and remaining[0]["span"]["start"] == 960
    open_item(5, "cancel-series", "Review cancellation", "Whole weekly series", "Remove whole series")
    click(ui, 70, 550)
    assert ui.store("work")["entries"] == remaining
    ui.control("close", "work")
    open_item(5, "cancel-series-again", "Whole weekly series", "Remove whole series")
    apply("series-cancelled", "Cancellation applied", "No events")
    assert ui.store("work")["entries"] == []
    open_item(1, "stale-series", "already cancelled or superseded")
    assert ui.store("work")["entries"] == []
    assert ui.control("fingerprint", "personal").strip() == b"empty"
    ui.control("close", "work")
    print(guest(ui, "series-check-empty"))
    print("PASS scoped RSVP, preserved exception, single and whole cancellation, spoof, replay and empty outbox")


def inspect_moves(ui):
    review(ui, 248)
    ui.see("bounded-overview", "4 meetings", "last 2026-12-22", "Review 2 included moves")
    click(ui, 355, 412)
    ui.see("bounded-first-move", "Included move 1 of 2", "2026-12-08", "2026-12-09", "Studio B")
    click(ui, 355, 412)
    ui.see("bounded-second-move", "Included move 2 of 2", "2026-12-15", "2026-12-17", "Studio C", "response applies to the whole series")


def bounded(ui):
    print(guest(ui, "bounded-setup"))
    open_mail(ui, first=True)

    inspect_moves(ui)
    click(ui, 70, 550)
    assert ui.control("fingerprint", "work").strip() == b"empty"
    ui.control("close", "work")
    inspect_moves(ui)
    click(ui, 80, 462)
    ui.see("bounded-response", "Whole weekly series", "4 meetings", "2 included moves", "ACCEPTED")
    # Dismiss the response and confirm the inspected local schedule separately.
    click(ui, 70, 355)
    ui.control("close", "work")
    assert ui.control("fingerprint", "work").strip() == b"empty"
    inspect_moves(ui)
    click(ui, 568, 550)
    ui.see("bounded-saved", "Invitation saved", "14:00")
    rows = ui.store("work")["entries"]
    master = next(r for r in rows if r["kind"] == "Weekly")
    assert len(rows) == 3 and master["invitation"]["last"] == "2026-12-22"
    assert master["excluded"] == ["2026-12-08", "2026-12-15"]
    assert sorted((r["date"], r["span"]["start"]) for r in rows if r is not master) == [("2026-12-09", 900), ("2026-12-17", 660)]
    ui.control("close", "work")
    review(ui, 178)
    ui.see("bounded-extension", "last regular meeting 2026-12-29", "Current series", "last 2026-12-22")
    click(ui, 568, 550)
    ui.see("bounded-extended", "Invitation saved")
    saved = ui.store("work")["entries"]
    assert len(saved) == 3 and next(r for r in saved if r["kind"] == "Weekly")["invitation"]["last"] == "2026-12-29"
    assert ui.control("fingerprint", "personal").strip() == b"empty"
    ui.control("close", "work")
    print(guest(ui, "series-check-empty"))
    print("PASS bounded review, both included moves, dismissal, whole-series RSVP review, atomic save, extension and empty outbox")


def cancel_dates(ui):
    print(guest(ui, "cancel-dates-setup"))
    open_mail(ui, first=True)
    inspect_moves(ui)
    click(ui, 568, 550)
    ui.see("group-series-saved", "Invitation saved")
    baseline = ui.store("work")
    master = next(r for r in baseline["entries"] if r["kind"] == "Weekly")
    survivor = next(r for r in baseline["entries"] if r["date"] == "2026-12-17")
    ui.control("close", "work")

    def inspect_cancellations():
        review(ui, 178)
        ui.see("group-overview", "Review cancellation", "2 occurrences", "Review 2 dates")
        click(ui, 355, 412)
        ui.see("group-moved-date", "Cancellation 1 of 2", "2026-12-08", "2026-12-09", "15:00", "Studio B")
        click(ui, 568, 550)  # The final action must still be unavailable.
        assert ui.store("work") == baseline
        click(ui, 355, 412)
        ui.see("group-regular-date", "Cancellation 2 of 2", "2026-12-22", "14:00", "Studio A", "Remove reviewed dates")

    inspect_cancellations()
    click(ui, 70, 550)
    assert ui.store("work") == baseline
    ui.control("close", "work")
    inspect_cancellations()
    click(ui, 568, 550)
    ui.see("group-cancelled", "Cancellation applied", "Undo cancellation")
    after = ui.store("work")
    assert after["revision"] == baseline["revision"] + 1
    assert len(after["entries"]) == 2 and survivor in after["entries"]
    expected_master = dict(master, excluded=["2026-12-08", "2026-12-15", "2026-12-22"])
    assert expected_master in after["entries"]
    assert sorted((c["original"], c["sequence"]) for c in after["cancelled"]) == [("2026-12-08", 2), ("2026-12-22", 4)]
    click(ui, 590, 562)
    ui.see("group-undone", "Change undone", "15:00")
    restored = ui.store("work")
    assert restored == dict(baseline, revision=after["revision"] + 1)
    assert ui.control("fingerprint", "personal").strip() == b"empty"
    ui.control("close", "work")
    print(guest(ui, "series-check-empty"))
    print("PASS reviewed moved and regular dates, cancelled together, preserved sibling schedules, one Undo restored both, empty outbox")


def exdates(ui):
    print(guest(ui, "exdates-setup"))
    open_mail(ui, first=True)
    inspect_moves(ui)
    click(ui, 568, 550)
    ui.see("exdates-series-saved", "Invitation saved")
    ui.open_date("2026-12-09")
    click(ui, 450, 268)
    ui.field(80, 256, "2026-12-10")
    ui.field(225, 256, "16:30")
    ui.field(330, 256, "17:15")
    click(ui, 92, 506)
    ui.see("exdates-local-move", "2026-12-10", "16:30")
    baseline = ui.store("work")
    moved = next(e for e in baseline["entries"] if e["date"] == "2026-12-10")
    assert moved["span"] == {"start": 990, "end": 1035}
    sibling = next(e for e in baseline["entries"] if e["date"] == "2026-12-17")

    def inspect(keep):
        ui.control("close", "work")
        review(ui, 178)
        ui.see("exdates-overview", "Whole weekly series", "Review 2 dates")
        click(ui, 355, 412)
        ui.see("exdates-conflict", "Skipped date", "2026-12-08", "2026-12-10", "16:30", "Keep my move", "Skip it here")
        click(ui, 355, 412)
        ui.see("exdates-choice-required", "You edited this occurrence locally")
        click(ui, 105 if keep else 290, 300)
        ui.see("exdates-choice", "Selected", "keep my move" if keep else "skip it here")
        click(ui, 355, 412)
        ui.see("exdates-regular", "Skipped date", "2026-12-22", "Saving the series skips this meeting")
        assert ui.store("work") == baseline

    inspect(True)
    click(ui, 75, 550)
    assert ui.store("work") == baseline
    inspect(True)
    click(ui, 80, 462)
    response = ui.see("exdates-response", "Whole weekly series", "2 skipped dates", "ACCEPTED")
    assert "16:30" not in response and "2026-12-10" not in response
    click(ui, 70, 355)
    assert ui.store("work") == baseline
    for keep in (True, False):
        inspect(keep)
        click(ui, 568, 550)
        ui.see("exdates-saved", "Invitation saved")
        saved = ui.store("work")
        assert saved["revision"] == baseline["revision"] + 1
        assert sibling in saved["entries"] and (moved in saved["entries"]) == keep
        master = next(r for r in saved["entries"] if r["kind"] == "Weekly")
        assert master["date"] == "2026-12-01" and master["span"]["start"] == 840
        assert master["excluded"] == ["2026-12-08", "2026-12-15", "2026-12-22"]
        click(ui, 590, 562)
        ui.see("exdates-undone", "Change undone")
        restored = ui.store("work")
        assert restored == dict(baseline, revision=saved["revision"] + 1)
        baseline = restored
    assert ui.control("fingerprint", "personal").strip() == b"empty"
    ui.control("close", "work")
    print(guest(ui, "series-check-empty"))
    print("PASS private local move choices, each skipped date reviewed, regular and moved siblings preserved, Undo and empty outbox")


def future(ui):
    print(guest(ui, "future-setup"))
    open_mail(ui, first=True)
    inspect_moves(ui)
    click(ui, 568, 550)
    ui.see("range-series-saved", "Invitation saved")
    # Move a later exception across the range boundary on this device.
    ui.open_date("2026-12-17")
    click(ui, 450, 268)
    ui.field(80, 256, "2026-12-07")
    ui.field(225, 256, "11:30")
    ui.field(330, 256, "12:15")
    click(ui, 92, 506)
    ui.see("range-private-move", "2026-12-07", "11:30")
    baseline = ui.store("work")
    local = next(r for r in baseline["entries"] if r["date"] == "2026-12-07")
    assert local["span"] == {"start": 690, "end": 735}

    def inspect():
        ui.control("close", "work")
        review(ui, 178)
        ui.see("range-overview", "This and future occurrences", "2026-12-08", "Review 3 dates")
        click(ui, 355, 412)
        ui.see("range-anchor", "Range date 1 of 3", "2026-12-09", "15:00", "16:00")
        click(ui, 355, 412)
        ui.see("range-preserved", "Range date 2 of 3", "2026-12-15", "2026-12-07", "11:30", "separately edited meeting")
        click(ui, 355, 412)
        ui.see("range-last", "Range date 3 of 3", "2026-12-22", "2026-12-23", "16:00", "Apply reviewed range")
        assert ui.store("work") == baseline

    inspect()
    click(ui, 80, 462)
    response = ui.see("range-rsvp", "This and future occurrences", "2026-12-08", "ACCEPTED")
    assert "11:30" not in response
    click(ui, 70, 355)
    inspect()
    click(ui, 568, 550)
    ui.see("range-saved", "Invitation saved")
    saved = ui.store("work")
    assert saved["revision"] == baseline["revision"] + 1 and local in saved["entries"]
    base = next(r for r in saved["entries"] if r["kind"] == "Weekly")
    assert base["date"] == "2026-12-01" and base["span"] == {"start": 840, "end": 885}
    assert base["excluded"] == ["2026-12-08", "2026-12-15", "2026-12-22"]
    changed = [r for r in saved["entries"] if r is not base and r != local]
    assert sorted(r["date"] for r in changed) == ["2026-12-09", "2026-12-23"]
    assert all(r["span"] == {"start": 960, "end": 1020} for r in changed)
    click(ui, 590, 562)
    ui.see("range-undone", "Change undone")
    assert ui.store("work") == dict(baseline, revision=saved["revision"] + 1)
    assert ui.control("fingerprint", "personal").strip() == b"empty"
    ui.control("close", "work")
    print(guest(ui, "series-check-empty"))
    print("PASS scoped range RSVP, private exception across boundary, earlier meeting preserved and atomic Undo")


def long_range(ui):
    print(guest(ui, "long-range-setup"))
    open_mail(ui, first=True)

    def item(number):
        review(ui, 178 + (4 - number) * 70)

    def advance(name, *labels):
        click(ui, 355, 412)
        ui.see("long-" + name, *labels)

    def save(name):
        click(ui, 568, 550)
        ui.see("long-saved-" + name, "Invitation saved")

    def move(day, destination):
        ui.open_date(day)
        click(ui, 450, 268)
        ui.field(80, 256, destination)
        ui.field(225, 256, "11:30")
        ui.field(330, 256, "12:15")
        click(ui, 92, 506)
        ui.see("local-" + destination, destination, "11:30")

    item(1)
    ui.see("long-twelve", "12 meetings", "2027-02-16")
    save("master")
    move("2027-01-12", "2027-01-14")
    baseline = ui.store("work")
    ui.control("close", "work")
    item(2)
    ui.see("long-overview", "through 2027-02-16", "examples and saved exceptions", "Review 4 dates")
    advance("anchor", "2026-12-08", "2026-12-09", "16:00")
    advance("sample", "2026-12-15", "2026-12-16", "16:00")
    advance("exception", "2027-01-12", "2027-01-14", "11:30", "separately edited")
    advance("last", "2027-02-16", "2027-02-17", "16:00")
    assert ui.store("work") == baseline
    save("range")
    ui.open_date("2027-02-24")
    ui.see("long-before-extension", "No events")
    ui.control("close", "work")
    item(3)
    ui.see("long-extension", "no end date", "New dates inherit", "Review date")
    advance("first-new", "2027-02-23", "2027-02-24", "16:00")
    save("extension")
    ui.open_date("2027-02-24")
    ui.see("long-inherited", "16:00", "17:00")
    move("2027-03-17", "2027-03-18")
    baseline = ui.store("work")
    ui.control("close", "work")
    item(4)
    ui.see("long-open-review", "no end date", "examples and saved exceptions", "Review 3 dates")
    advance("spring-anchor", "2027-03-09", "2027-03-10", "16:00", "18:00")
    advance("spring-exception", "2027-03-16", "2027-03-18", "11:30", "separately edited")
    advance("spring-example", "2027-03-23", "2027-03-24", "18:00")
    assert ui.store("work") == baseline
    save("open-range")
    for day, value in [("2026-12-01", "14:00"), ("2027-01-14", "11:30"),
                       ("2027-02-24", "16:00"), ("2027-03-17", "No events"),
                       ("2027-03-18", "11:30"), ("2036-12-03", "18:00")]:
        ui.open_date(day)
        ui.see("long-result-" + day, value)
    rows = ui.store("work")["entries"]
    assert len(rows) == 5
    assert ui.control("fingerprint", "personal").strip() == b"empty"
    ui.control("close", "work")
    print(guest(ui, "series-check-empty"))
    print("PASS long and open-ended range review, inherited extension, distant agenda and both private moves")


def cancel_future(ui, reinstate=False):
    print(guest(ui, "reinstate-setup" if reinstate else "cancel-future-setup"))
    open_mail(ui, first=True)
    review(ui, 318 if reinstate else 248)
    ui.see("cut-master", "Whole weekly series")
    click(ui, 568, 550)
    ui.see("cut-saved", "Invitation saved")

    def move(day, dest, start, end):
        ui.open_date(day)
        click(ui, 450, 268)
        ui.field(80, 256, dest)
        ui.field(225, 256, start)
        ui.field(330, 256, end)
        click(ui, 92, 506)
        ui.see("cut-move-" + dest, dest, start)

    move("2026-12-08", "2026-12-18", "10:00", "10:45")
    move("2026-12-22", "2026-12-11", "16:30", "17:15")
    baseline = ui.store("work")
    earlier = next(r for r in baseline["entries"] if r["date"] == "2026-12-18")
    local = next(r for r in baseline["entries"] if r["date"] == "2026-12-11")

    def inspect(keep):
        ui.control("close", "work")
        review(ui, 248 if reinstate else 178)
        ui.see("cut-overview", "This and future occurrences", "from 2026-12-15 onward", "no end date", "Review 4 dates")
        click(ui, 355, 412)
        ui.see("cut-earlier", "2026-12-08", "2026-12-18", "10:00", "stays put")
        click(ui, 355, 412)
        ui.see("cut-start", "2026-12-15", "14:00", "removes this meeting")
        click(ui, 355, 412)
        ui.see("cut-private", "2026-12-22", "2026-12-11", "16:30", "Keep private copy", "Remove it here")
        click(ui, 355, 412)
        ui.see("cut-choice-required", "Keep private copy", "Remove it here")
        click(ui, 105 if keep else 290, 300)
        click(ui, 355, 412)
        ui.see("cut-regular", "2026-12-29", "removes this meeting", "Apply range cancellation")
        assert ui.store("work") == baseline

    if reinstate:
        inspect(True)
        click(ui, 568, 550)
        ui.see("reinstatement-cancelled", "Cancellation applied")
        cancelled = ui.store("work")
        for keep in (True, False):
            ui.control("close", "work")
            review(ui, 178)
            ui.see("reinstatement", "Review reinstatement", "2026-12-15", "no end date")
            click(ui, 355, 412)
            ui.see("reinstatement-start", "2026-12-15", "Returns", "2026-12-16", "16:00")
            click(ui, 355, 412)
            ui.see("reinstatement-copy", "2026-12-11", "16:30", "2026-12-23", "16:00", "Only you")
            click(ui, 105 if keep else 290, 300)
            ui.see("reinstatement-choice", "Restore reviewed dates")
            assert ui.store("work") == cancelled
            click(ui, 568, 550)
            ui.see("reinstatement-saved", "Invitation saved")
            saved = ui.store("work")
            assert earlier in saved["entries"] and not saved.get("cancelled")
            assert any(e["id"] == local["id"] for e in saved["entries"]) == keep
            ui.open_date("2026-12-23")
            ui.see("reinstatement-returned", "Studio planning", "16:00")
            ui.open_date("2026-12-18")
            ui.see("reinstatement-earlier", "Studio planning", "10:00")
            click(ui, 590, 562)
            ui.see("reinstatement-undo", "Change undone")
            assert ui.store("work") == dict(cancelled, revision=saved["revision"] + 1)
            cancelled = ui.store("work")
        assert ui.control("fingerprint", "personal").strip() == b"empty"
        ui.control("close", "work")
        print(guest(ui, "series-check-empty"))
        print("PASS restored weekly dates, both private copy choices, earlier move, Undo, empty outbox")
        return
    inspect(True)
    click(ui, 70, 550)
    assert ui.store("work") == baseline
    for keep in (True, False):
        inspect(keep)
        click(ui, 568, 550)
        ui.see("cut-applied", "Cancellation applied", "Undo cancellation")
        saved = ui.store("work")
        assert saved["revision"] == baseline["revision"] + 1 and earlier in saved["entries"]
        master = next(r for r in saved["entries"] if r["kind"] == "Weekly")
        assert master["date"] == "2026-12-01" and master["span"] == {"start": 840, "end": 885}
        assert master["invitation"]["last"] == "2026-12-08"
        copies = [r for r in saved["entries"] if r["id"] == local["id"]]
        assert len(copies) == int(keep)
        if keep:
            assert copies[0]["kind"] == "Appointment" and "invitation" not in copies[0]
            assert (copies[0]["date"], copies[0]["span"]) == (local["date"], local["span"])
        assert len(saved["cancelled"]) == 1 and saved["cancelled"][0]["this_and_future"]
        click(ui, 590, 562)
        ui.see("cut-undone", "Change undone")
        restored = ui.store("work")
        assert restored == dict(baseline, revision=saved["revision"] + 1)
        baseline = restored
    assert ui.control("fingerprint", "personal").strip() == b"empty"
    ui.control("close", "work")
    print(guest(ui, "series-check-empty"))
    print("PASS both boundary crossings, private copy and removal, untouched prefix, one Undo and empty outbox")


if __name__ == "__main__":
    runner = {"--reinstate": lambda ui: cancel_future(ui, reinstate=True), "--cancel-future": cancel_future, "--long-range": long_range, "--future": future, "--exdates": exdates, "--cancel-dates": cancel_dates, "--bounded": bounded, "--series": series}.get(next(iter(sys.argv[1:]), ""), run)
    if runner != run:
        del sys.argv[1]
    if len(sys.argv) < 6:
        sys.exit("Usage: calendar-invitation-check.py [--series|--bounded|--cancel-dates|--exdates|--future|--long-range|--cancel-future|--reinstate] GUEST_DIR MARKER EVIDENCE_DIR ssh [options] HOST")
    runner(keyboard.Desktop(*sys.argv[1:4], sys.argv[4:]))
