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
expected = [spool / f"job-{job}.pdf" for job in range(1, 10)]
assert set(spool.glob("*.pdf")) == set(expected), list(spool.iterdir())
html, sheets, slides_hidden_off, slides_hidden_on, notes_hidden_on, notes_hidden_off, mail, plain_text, picture = expected

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
