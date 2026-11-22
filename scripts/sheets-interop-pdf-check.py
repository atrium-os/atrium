#!/usr/bin/env python3
"""Check PDFs exported by the Sheets desktop print interoperability profile."""
import subprocess
import sys
from pathlib import Path


def poppler(*args):
    return subprocess.check_output(args, text=True)


def check(path, with_print_area):
    pages = poppler("pdftotext", "-layout", str(path), "-").rstrip().split("\f")
    assert len(pages) == 4, (path, len(pages))
    first, next_down, next_across, last = pages

    for token in ("FrozenRow", "FrozenCol", "TitleRow", "TitleCol", "8", "168"):
        assert token in first.split(), (path, token)
    # The frozen bands repeat along their respective pagination axes. The
    # separate explicit titles stay on the page where their cells fall.
    assert "FrozenRow" in next_down and "FrozenCol" in next_across, path
    assert all("TitleRow" not in page and "TitleCol" not in page for page in pages[1:]), path
    assert "FarBody" in last, path
    for token in ("OutBottom", "OutRight"):
        assert any(token in page for page in pages) != with_print_area, (path, token)

    images = [
        line.split()[:5]
        for line in poppler("pdfimages", "-list", str(path)).splitlines()[2:]
    ]
    expected = [["1", "0", "image", "48", "30"]] if with_print_area else []
    assert images == expected, (path, images)
    print(f"PASS {path.name}: four pages, cell edit, frozen and explicit titles, print area, pictures")


if __name__ == "__main__":
    directory = Path(sys.argv[1])
    check(directory / "xlsx.pdf", with_print_area=True)
    check(directory / "ods.pdf", with_print_area=False)
