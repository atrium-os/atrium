#!/usr/bin/env python3
"""Verify the visible fidelity notices in the installed Sheets matrix screenshots."""
import io
import re
import subprocess
import sys
from pathlib import Path

from PIL import Image


def read_box(path, box, layout):
    with Image.open(path) as source:
        assert source.size == (1280, 720), (path, source.size)
        crop = source.convert("L").crop(box)
        scaled = crop.resize((crop.width * 3, crop.height * 3))
        encoded = io.BytesIO()
        scaled.save(encoded, format="PNG")
    result = subprocess.run(
        ["tesseract", "stdin", "stdout", "--psm", str(layout)],
        input=encoded.getvalue(),
        capture_output=True,
        check=True,
    )
    return " ".join(result.stdout.decode().lower().split())


def check(directory, file, *, want=(), absent=(), dialog=False):
    box = (350, 258, 750, 467) if dialog else (0, 693, 1100, 720)
    visible = read_box(directory / file, box, 6 if dialog else 7)
    for phrase in want:
        assert re.search(phrase, visible), (file, phrase, visible)
    for phrase in absent:
        assert not re.search(phrase, visible), (file, phrase, visible)
    print(f"PASS {file}: {visible}")


def matrix(directory):
    limited = "row formats on distant blank cells were limited"
    for kind in ("xlsx", "ods"):
        check(directory, f"appearance/{kind}-appearance-initial.png", want=(limited, "import notes"))
        check(directory, f"appearance/{kind}-appearance-reopened.png", absent=(limited, "import notes"))

    check(directory, "appearance/xlsm-import-note-1.png", want=("1 of 2", limited), dialog=True)
    check(directory, "appearance/xlsm-import-note-2.png", want=("2 of 2", "inert: macros", "never run"), dialog=True)
    for state in ("initial", "saved", "reopened"):
        check(directory, f"legacy/xlsm-{state}.png", want=("macros are inactive",))
    check(
        directory, "legacy/xls-initial.png",
        want=("merged cells shown unmerged", "opened read.only", "save asks where to put a copy"),
    )
    check(
        directory, "legacy/xls-native-reopened.png",
        absent=("opened read.only", "merged cells shown unmerged"),
    )

    for fmt, utf8, ansi in (("csv", "01", "03"), ("tsv", "02", "04")):
        for num, encoding in ((utf8, "utf8"), (ansi, "ansi")):
            file = f"delimited/{num}-{encoding}.{fmt}"
            if encoding == "ansi":
                check(directory, f"{file}-initial.png", want=("windows.1252", "import notes"))
                check(directory, f"{file}-import-note.png", want=("1 of 1", "windows.1252"), dialog=True)
            else:
                check(directory, f"{file}-initial.png", absent=("windows.1252", "import notes"))
            check(directory, f"{file}-saved.png", want=(f"{fmt} keeps formula results as values",))
            check(directory, f"{file}-reopened.png", want=("3 rows", "4 columns"), absent=("windows.1252", "import notes"))


if __name__ == "__main__":
    matrix(Path(sys.argv[1]))
