"""Write the 16 static pages used to fill Navigator's broker cache."""
import argparse
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("directory", type=Path)
out = parser.parse_args().directory
out.mkdir(parents=True, exist_ok=True)
for number in range(1, 17):
    link = f'<p><a href="/bed-{number + 1}.html">Visit flowerbeds</a></p>' if number < 16 else ""
    (out / f"bed-{number}.html").write_text(
        f'<!doctype html>\n<meta charset="utf-8">\n<title>Flowerbed {number}</title>\n'
        f"<p>Flowerbed {number}</p>\n{link}\n", encoding="utf-8")
