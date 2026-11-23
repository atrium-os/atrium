#!/usr/bin/env python3
"""Check the four-page PDFs exported by the Sheets desktop print profile."""
import subprocess
import sys
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


if __name__ == "__main__":
    directory = Path(sys.argv[1])
    check(directory / "ods.pdf")
    if len(sys.argv) > 2 and sys.argv[2] == "comments-print":
        check(directory / "xls.pdf", picture=False)
    else:
        check(directory / "xlsx.pdf")
