"""Check the four PDFs received by the isolated IPPS simulator."""
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
expected = [spool / f"job-{job}.pdf" for job in range(1, 5)]
assert set(spool.glob("*.pdf")) == set(expected), list(spool.iterdir())
html, sheets, slides, mail = expected

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
images = subprocess.check_output(["pdfimages", "-list", str(sheets)], text=True)
assert len(re.findall(r"^\s*1\s+\d+\s+image\s+", images, re.M)) == 2, images
print("Sheets: one page with saved-area cells, chart title, and two images")

groups = [
    re.findall(r"(FIRST|SECOND|THIRD|FOURTH|HIDDEN) SLIDE", page)
    for page in pages_of(slides)
]
assert groups == [["FIRST", "SECOND", "THIRD"], ["FOURTH"]], groups
print("Slides: two handout pages, visible slides grouped 3 + 1")

pages = pages_of(mail)
body = "\n".join(pages)
assert len(pages) == 3, len(pages)
assert "Mail print fixture" in pages[0], pages[0]
assert numbered_lines(r"Message body line (\d+)", body) == list(range(1, 76)), body
assert "synthetic-receipt.bin" in pages[-1] and "SECRET_ATTACHMENT_PAYLOAD" not in body
print("Mail: three pages with all 75 body lines and the attachment filename")
