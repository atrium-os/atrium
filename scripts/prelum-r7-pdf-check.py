"""Check the PDFs received by the isolated IPPS simulator."""
import re
import subprocess
import sys
from pathlib import Path


def pages_of(pdf):
    text = subprocess.check_output(["pdftotext", "-raw", str(pdf), "-"], text=True)
    return text.rstrip("\f").split("\f")


def numbered_lines(pattern, body):
    return [int(number) for number in re.findall(pattern, body)]


spool = Path(sys.argv[1])
expected = [spool / f"job-{job}.pdf" for job in range(1, 8)]
assert set(spool.glob("*.pdf")) == set(expected), list(spool.iterdir())
html, sheets, slides_hidden_off, slides_hidden_on, notes_hidden_on, notes_hidden_off, mail = expected

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
