"""Check the PDFs received by the isolated IPPS simulator."""
import hashlib
import re
import subprocess
import sys
from pathlib import Path


def pages_of(pdf):
    text = subprocess.check_output(["pdftotext", "-raw", str(pdf), "-"], text=True)
    return text.rstrip("\f").split("\f")


def numbered_lines(pattern, body):
    return [int(number) for number in re.findall(pattern, body, re.M)]


def printed_events(job):
    lifecycle = [("create", None)]
    if job >= 14:
        lifecycle.append(("pin-accepted", job))
    lifecycle.extend([("send", job), ("receipt", job), ("complete", job)])
    return lifecycle


spool = Path(sys.argv[1])
original = Path(sys.argv[4]) if len(sys.argv) > 4 else None
job_count = int(sys.argv[5]) if original else 11
responses = Path(sys.argv[7]).read_text(encoding="utf-8").splitlines() if len(sys.argv) > 7 else []
outcomes = [line.split() for line in responses]
job_ids = list(range(1, job_count + 1))
job_ids.extend(int(job) for response, job, _ in outcomes if response == "recovery")
failed_uploads = {int(job): mode for response, job, mode in outcomes if response == "send-failure"}
expected = [spool / f"job-{job}.pdf" for job in sorted([*job_ids, *failed_uploads])]
assert set(spool.glob("*.pdf")) == set(expected), list(spool.iterdir())
transcript = Path(sys.argv[2]).read_text(encoding="utf-8")
events = []
receipts = []
for line in transcript.splitlines():
    if line.startswith("fake printer: Create-Job job "):
        events.append(("create", None))
    elif line.startswith("fake printer: Print-Job job "):
        events.append(("print-job", None))
    elif response := re.fullmatch(r"fake printer: PIN response ([a-z-]+) (\d+|-)", line):
        events.append((response[1], response[2]))
    elif cancel := re.match(r"^fake printer: Cancel-Job job (\d+) ", line):
        events.append(("cancel", int(cancel[1])))
    elif result := re.fullmatch(r"fake printer: Cancel-Job result ([a-z-]+) (\d+)", line):
        events.append((result[1], int(result[2])))
    elif send_result := re.fullmatch(r"fake printer: Send-Document result ([a-z-]+) (\d+)", line):
        events.append((f"send-{send_result[1]}", int(send_result[2])))
    elif poll := re.match(r"^fake printer: Get-Job-Attributes job (\d+) ", line):
        assert int(poll[1]) not in failed_uploads, line
    elif accepted := re.fullmatch(r"fake printer: accepted fixture PIN for job (\d+) before document", line):
        events.append(("pin-accepted", int(accepted[1])))
    elif send := re.match(r"^fake printer: Send-Document job (\d+) ", line):
        events.append(("send", int(send[1])))
    elif receipt := re.fullmatch(
        r"fake printer: got job (\d+) \((\d+) bytes\) (.*) sha256=([a-f0-9]{64}) format=(\S+) tls=(\S+)",
        line,
    ):
        events.append(("receipt", int(receipt[1])))
        receipts.append(receipt.groups())
    elif completed := re.fullmatch(r"fake printer: job (\d+) completed", line):
        events.append(("complete", int(completed[1])))

expected_events = [
    event for job in range(1, job_count + 1) for event in printed_events(job)
]
for response, printer_job, cancel_result in outcomes:
    if response == "recovery":
        expected_events.extend(printed_events(int(printer_job)))
    elif response == "send-failure":
        job = int(printer_job)
        expected_events.extend(printed_events(job)[:-1])
        expected_events.append((f"send-{cancel_result}", job))
    elif response != "replay":
        expected_events.extend([("create", None), (response, printer_job)])
        if printer_job != "-":
            job = int(printer_job)
            expected_events.extend([("cancel", job), (cancel_result, job)])
assert events == expected_events, events

jobs = Path(sys.argv[3]).read_text(encoding="utf-8")
completed_jobs = re.findall(
    r"^ +([0-9]+) +printer [0-9]+ +Done +([0-9]+)/([0-9]+) +(.+?) *$",
    jobs,
    re.M,
)
desktop_job_ids = list(range(1, job_count + 1))
skipped = set()
if len(sys.argv) > 6:
    # Earlier rejections consume local IDs without creating IPP jobs.
    skipped = {int(job) for job in sys.argv[6].split(",")}
    desktop_job_ids = [job for job in range(1, job_count + len(skipped) + 1) if job not in skipped]
    for job in skipped:
        assert re.search(rf"^ +{job} +printer [0-9]+ +Failed +0/2 ", jobs, re.M), jobs
if outcomes:
    first_followup = job_count + len(skipped) + 1
    for desktop_job, (response, printer_job, cancel_result) in enumerate(outcomes, first_followup):
        if response == "recovery":
            desktop_job_ids.append(desktop_job)
            assert (str(desktop_job), "2", "2", f"PIN recovery after {cancel_result}") in completed_jobs, jobs
            print(f"IPPS simulator: fresh PIN after {cancel_result}, job {printer_job} uploaded once")
        elif response == "replay":
            if cancel_result.startswith("create-"):
                title = f"PIN response {cancel_result} without-id none"
            elif cancel_result.startswith("send-"):
                title = f"PIN upload failure {cancel_result.removeprefix('send-')}"
            else:
                title = f"PIN response ignored with-id {cancel_result}"
            title = re.escape(title)
            assert re.search(
                rf"^ +{desktop_job} +printer [0-9]+ +Failed +0/2 +{title} +Arca did not supply a usable PIN for this job *$",
                jobs,
                re.M,
            ), jobs
            print(f"IPPS simulator: replay after {cancel_result}, no printer job or PDF")
        elif response == "send-failure":
            title = re.escape(f"PIN upload failure {cancel_result}")
            assert re.search(rf"^ +{desktop_job} +printer [0-9]+ +Failed +0/2 +{title} ", jobs, re.M), jobs
            print(f"IPPS simulator: Send-Document {cancel_result}, job {printer_job} uploaded once and failed at 0/2")
        else:
            id_mode = "without-id" if printer_job == "-" else "with-id"
            title = re.escape(f"PIN response {response} {id_mode} {cancel_result}")
            assert re.search(rf"^ +{desktop_job} +printer [0-9]+ +Failed +0/2 +{title} ", jobs, re.M), jobs
            result = "no job ID" if printer_job == "-" else f"job {printer_job} cancellation {cancel_result}"
            print(f"IPPS simulator: PIN response {response}, {result}, no document request or PDF")
assert [int(job) for job, _, _, _ in completed_jobs] == desktop_job_ids, completed_jobs
completed_by_printer = dict(zip(job_ids, completed_jobs))
for pdf, receipt in zip(expected, receipts):
    job, size, title, digest, document_format, tls = receipt
    printer_job = int(job)
    if printer_job in failed_uploads:
        completed = total = "2"
        assert title == f"PIN upload failure {failed_uploads[printer_job]}", receipt
    else:
        _, completed, total, expected_title = completed_by_printer[printer_job]
        assert title == expected_title, (receipt, expected_title)
    assert pdf.name == f"job-{job}.pdf", (pdf, receipt)
    assert document_format == "application/pdf", (job, document_format)
    assert tls in ("TLSv1.2", "TLSv1.3"), (job, tls)
    payload = pdf.read_bytes()
    assert len(payload) == int(size), (job, size, len(payload))
    assert hashlib.sha256(payload).hexdigest() == digest, (job, "upload SHA-256 mismatch")
    info = subprocess.check_output(["pdfinfo", str(pdf)], text=True)
    page_count = re.search(r"^Pages: +([0-9]+)$", info, re.M)
    assert page_count and completed == total == page_count[1], (job, completed, total, info)
print(f"IPPS simulator: {len(job_ids)} completed jobs and {len(failed_uploads)} failed uploads; PDF format, TLS, titles, page counts and upload hashes verified")

html, sheets, slides_hidden_off, slides_hidden_on, notes_hidden_on, notes_hidden_off, mail, plain_text, picture, pdf_button, pdf_shortcut = expected[:11]

pages = pages_of(html)
body = "\n".join(pages)
assert len(pages) == 4, len(pages)
assert numbered_lines(r"HTML fixture paragraph (\d+)", body) == list(range(1, 96)), body
assert "HTML FIRST HEADING" in pages[0] and "HTML FINAL HEADING" in pages[-1], pages
assert "REPLACEMENT" not in body
print("Quick Look: four pages with all 95 original paragraphs after pathname replacement")

pages = pages_of(sheets)
assert len(pages) == 1, len(pages)
for token in ("Quarterly Chart Proof", "North 4", "South 7", "West 5"):
    assert token in pages[0], pages[0]
assert "OUTRIGHT" not in pages[0] and "OUTBELOW" not in pages[0], pages[0]
images = subprocess.check_output(["pdfimages", "-list", str(sheets)], text=True)
sizes = re.findall(r"^\s*1\s+\d+\s+image\s+(\d+)\s+(\d+)\s+", images, re.M)
assert sorted(sizes) == [("24", "18"), ("31", "19")], images
print("Sheets: saved-area cells, chart and both distinct pictures; outside cells and picture omitted")

for pdf, setting, paper, expected_groups in (
    (slides_hidden_off, "off", "a4", [["FIRST", "SECOND", "THIRD"], ["FOURTH"]]),
    (slides_hidden_on, "on", "letter", [["FIRST", "HIDDEN", "SECOND"], ["THIRD", "FOURTH"]]),
):
    pages = pages_of(pdf)
    assert all("SPEAKER" not in page and "Speaker line" not in page for page in pages), setting
    groups = [
        re.findall(r"(FIRST|SECOND|THIRD|FOURTH|HIDDEN) SLIDE", page)
        for page in pages
    ]
    assert groups == expected_groups, (setting, groups)
    info = subprocess.check_output(
        ["pdfinfo", "-f", "1", "-l", str(len(pages)), str(pdf)], text=True
    )
    sizes = re.findall(r"^Page\s+\d+ size:.*\((A4|letter)\)$", info, re.M | re.I)
    assert [size.lower() for size in sizes] == [paper] * len(pages), (setting, info)
    print(f"Slides: Include Hidden Slides {setting}, {paper.upper()} handout groups {groups}")

for pdf, include_hidden in ((notes_hidden_on, True), (notes_hidden_off, False)):
    pages = pages_of(pdf)
    body = "\n".join(pages)
    assert len(pages) == (10 if include_hidden else 9), len(pages)
    assert numbered_lines(r"Speaker line (\d+)", body) == list(range(1, 91)), body
    notes = [
        ("FIRST SLIDE", "FINAL SPEAKER LINE"),
        ("SECOND SLIDE", "SECOND SPEAKER NOTE"),
        ("FOURTH SLIDE", "FOURTH SPEAKER NOTE"),
    ]
    if include_hidden:
        notes.append(("HIDDEN SLIDE", "HIDDEN SPEAKER NOTE"))
    else:
        assert "HIDDEN SLIDE" not in body and "HIDDEN SPEAKER NOTE" not in body, body
    for slide, note in notes:
        assert body.count(note) == 1 and any(slide in page and note in page for page in pages), note
    setting = "on" if include_hidden else "off"
    print(f"Slides speaker notes: {len(pages)} pages, all 90 numbered lines and the final line, hidden slides {setting}")

pages = pages_of(mail)
body = "\n".join(pages)
assert len(pages) == 3, len(pages)
assert "Mail print fixture" in pages[0], pages[0]
assert numbered_lines(r"Message body line (\d+)", body) == list(range(1, 76)), body
assert "synthetic-receipt.bin" in pages[-1] and "SECRET_ATTACHMENT_PAYLOAD" not in body
print("Mail: three pages with all 75 body lines and the attachment filename")


pages = pages_of(plain_text)
body = "\n".join(pages)
assert len(pages) == 5, len(pages)
assert numbered_lines(r"Print fixture line (\d+)", body) == list(range(1, 121)), body
assert "FIRST QUICK LOOK LINE" in pages[0] and "FINAL QUICK LOOK LINE" in pages[-1], pages
assert "REPLACEMENT" not in body
print("Quick Look plain text: five pages with all 122 original lines after pathname replacement")


assert len(pages_of(picture)) == 1
raster = subprocess.check_output(["pdftoppm", "-f", "1", "-l", "1", "-r", "72", str(picture)])
magic, dimensions, depth, pixels = raster.split(b"\n", 3)
assert magic == b"P6" and depth == b"255"
width, height = map(int, dimensions.split())
assert len(pixels) == 3 * width * height


def color_bounds(color):
    points = [
        (i // 3 % width, i // 3 // width)
        for i in range(0, len(pixels), 3)
        if pixels[i:i + 3] == color
    ]
    assert points, color.hex()
    x, y = zip(*points)
    return min(x), min(y), max(x), max(y)


left = color_bounds(bytes.fromhex("ff0033"))
right = color_bounds(bytes.fromhex("0044ff"))
assert left[2] + 1 == right[0] and left[1::2] == right[1::2], (left, right)
image_width = right[2] - left[0] + 1
image_height = left[3] - left[1] + 1
assert abs(image_width - 2 * image_height) <= 3, (left, right)
assert abs(left[0] + right[2] + 1 - width) <= 3, (left, right)
assert abs(left[1] + left[3] + 1 - height) <= 3, (left, right)
print(f"Quick Look image: both colors in a centered {image_width} x {image_height} raster")


for path, action in ((pdf_button, "Print button"), (pdf_shortcut, "viewer Ctrl+P")):
    pages = [page.strip() for page in pages_of(path)]
    assert pages == ["ORIGINAL PDF FIRST PAGE", "ORIGINAL PDF FINAL PAGE"], (action, pages)
    print(f"Quick Look PDF {action}: both original pages in order after pathname replacement")


if original:
    original_pages = pages_of(original)
    for job, printer_job in zip(desktop_job_ids[11:], job_ids[11:]):
        pdf = spool / f"job-{printer_job}.pdf"
        assert pages_of(pdf) == original_pages, (job, "Printed PDF differs from its input")
        print(f"Diagnostic job {job}: both original pages printed")
    for printer_job in failed_uploads:
        pdf = spool / f"job-{printer_job}.pdf"
        assert pages_of(pdf) == original_pages, (printer_job, "Failed upload PDF differs from its input")
