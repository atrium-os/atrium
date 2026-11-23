#!/usr/bin/env python3
"""Check the four-page PDFs exported by the Sheets desktop print profile."""
import subprocess
import sys
import xml.etree.ElementTree as ET
from pathlib import Path


def poppler(*args):
    return subprocess.check_output(args, text=True)


def check(path, picture=True):
    pages = poppler("pdftotext", "-layout", str(path), "-").rstrip().split("\f")
    assert len(pages) == 4, (path, len(pages))
    expected = [
        {"NearTitle", "NearKey", "8", "168"},
        {"NearTitle", "FarKey"},
        {"FarTitle", "NearKey"},
        {"FarTitle", "FarKey", "FarBody"},
    ]
    for page, body in zip(pages, expected):
        words = page.split()
        want = body | {"Corner"}
        assert set(words) == want and len(words) == len(want), (path, words, want)

    images = [
        line.split()[:5]
        for line in poppler("pdfimages", "-list", str(path)).splitlines()[2:]
    ]
    assert images == ([["1", "0", "image", "48", "30"]] if picture else []), (path, images)
    print(f"PASS {path.name}: four pages, recalculated edit, both repeat axes, print area"
          + (", in-area picture" if picture else ""))


def check_link_style(path, box):
    # Render the link at 2 px/pt. It has a blue underline across its label,
    # in addition to the ink of the text and the neutral gridlines.
    scale = 2
    x, y = int(box["xMin"] * scale), int(box["yMin"] * scale)
    width = int(box["xMax"] * scale) - x
    height = int(box["yMax"] * scale) - y
    bitmap = subprocess.check_output([
        "pdftoppm", "-f", "1", "-l", "1", "-r", "144", "-singlefile",
        "-x", str(x), "-y", str(y), "-W", str(width), "-H", str(height), str(path),
    ])
    magic, size, maximum, pixels = bitmap.split(b"\n", 3)
    assert (magic, tuple(map(int, size.split())), maximum) == (b"P6", (width, height), b"255")
    blue = lambda r, g, b: b - r > 45 and g - r > 18
    runs = [sum(blue(*pixels[i:i + 3]) for i in range(start, start + width * 3, 3))
            for start in range(0, len(pixels), width * 3)]
    assert max(runs) >= width * 0.65, (path, runs)


def check_fidelity(path, comments):
    pages = poppler("pdftotext", "-layout", str(path), "-").rstrip().split("\f")
    assert len(pages) == 4 + int(comments), (path, len(pages))
    expected = [
        {"NearTitle", "NearKey", "8", "168", "JumpLink", "Styled"},
        {"NearTitle", "FarKey"},
        {"FarTitle", "NearKey"},
        {"FarTitle", "FarKey", "FarBody"},
    ]
    for page, body in zip(pages, expected):
        words = page.split()
        assert body <= set(words), (path, words, body)
        assert words.count("Corner") == 1, (path, words)

    text = " ".join(" ".join(pages).split())
    for forbidden in ("OutLeft", "OutRight", "OutBottom", "HiddenProof",
                      "HiddenComment", "OutsideComment", "file:///"):
        assert forbidden not in text, (path, forbidden)
    if comments:
        notes = " ".join(pages[-1].split())
        for once in ("Worksheet comments", "Rich title note", "Ana (resolved)", "Checked @Ana"):
            assert notes.count(once) == 1, (path, notes, once)
        assert {"B2", "D4", "Kim", "Jo"} <= set(notes.split()), notes
    else:
        assert "Worksheet comments" not in text and "Rich title note" not in text, path

    xml = ET.fromstring(poppler("pdftotext", "-f", "1", "-l", "1", "-bbox", str(path), "-"))
    words = list(xml.iterfind(".//{*}word"))
    coordinates = {word.text: {key: float(value) for key, value in word.attrib.items()}
                   for word in words}
    assert {"Wrap", "alpha", "beta", "Tail", "VERT", "ShrunkenLabel"} <= coordinates.keys(), coordinates
    assert coordinates["Tail"]["yMin"] > coordinates["Wrap"]["yMin"] + 10, coordinates
    vert = coordinates["VERT"]
    assert vert["yMax"] - vert["yMin"] > 1.6 * (vert["xMax"] - vert["xMin"]), vert
    right_of_rotated = vert["xMax"] + 66
    stacked = [w for w in words if w.text in "STACK" and len(w.text) == 1
               and vert["xMax"] < float(w.attrib["xMin"]) < right_of_rotated]
    stacked.sort(key=lambda w: float(w.attrib["yMin"]))
    assert "".join(w.text for w in stacked) == "STACK", (path, [w.text for w in stacked])

    images = [line.split()[:5]
              for line in poppler("pdfimages", "-list", str(path)).splitlines()[2:]]
    print_area_pictures = [image for image in images if image[3:] == ["48", "30"]]
    assert print_area_pictures == [["1", "0", "image", "48", "30"]], (path, images)
    if comments:
        assert any(image[0] == "5" and image[2] == "image" for image in images), (path, images)
    for label in ("JumpLink", "DocsLink"):
        check_link_style(path, coordinates[label])
    urls = poppler("pdfinfo", "-url", str(path))
    assert "https://example.test" not in urls and "file:///" not in urls, urls
    print(f"PASS {path.name}: styles, text geometry, four tiles, link styling"
          + (", and selected comments" if comments else ", with comments excluded"))


if __name__ == "__main__":
    directory = Path(sys.argv[1])
    if len(sys.argv) > 2 and sys.argv[2] == "fidelity":
        for fmt in ("ods", "xlsx"):
            check_fidelity(directory / f"{fmt}.pdf", comments=False)
            check_fidelity(directory / f"{fmt}-comments.pdf", comments=True)
        sys.exit(0)
    check(directory / "ods.pdf")
    if len(sys.argv) > 2 and sys.argv[2] == "comments-print":
        check(directory / "xls.pdf", picture=False)
    else:
        check(directory / "xlsx.pdf")
