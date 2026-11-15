#!/usr/bin/env python3
"""Compare Folium's renders with Poppler's (pdftoppm) page by page.

  scripts/folium-compare.py [--dpi 72] [--pages 5] [--out DIR] FILE.pdf...

Builds folium-render's folium-png example, renders the first --pages
pages of each file with both, and prints per page the percentage of
pixels that still differ strongly after a slight blur (so antialiasing
and glyph weight don't count), and the share of pages under 0.5%, the
first corpus score of folium.md §10. Renders stay in --out
(a scratch folder): never commit renders of files you don't own.
Needs Poppler's pdftoppm and Python's PIL."""
import argparse, os, subprocess, sys, tempfile, time
from PIL import Image, ImageChops, ImageFilter

ap = argparse.ArgumentParser()
ap.add_argument('--dpi', type=int, default=72)
ap.add_argument('--pages', type=int, default=5)
ap.add_argument('--out', default=tempfile.mkdtemp(prefix='folium-cmp-'))
ap.add_argument('--threshold', type=float, default=0.5)
ap.add_argument('files', nargs='+')
a = ap.parse_args()
root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
crate = os.path.join(root, 'folium-render')
subprocess.run(['cargo', 'build', '-q', '--release', '--example', 'folium-png'], cwd=crate, check=True)
tool = os.path.join(crate, 'target', 'release', 'examples', 'folium-png')
os.makedirs(a.out, exist_ok=True)
scores = []
for n, f in enumerate(a.files):
    try:
        np_ = int(subprocess.run(['pdfinfo', f], capture_output=True, text=True).stdout.split('Pages:')[1].split()[0])
    except Exception:
        print(f'{f}: pdfinfo failed, skipped')
        continue
    last = min(np_, a.pages)
    pre = os.path.join(a.out, f'f{n}')
    t = time.time()
    r = subprocess.run([tool, f, f'1-{last}', str(a.dpi), pre], capture_output=True, text=True)
    tf = time.time() - t
    if r.returncode != 0:
        print(f'{f}: FOLIUM FAILED {r.stderr.strip()[-200:]}')
        scores += [255.0] * last
        continue
    t = time.time()
    subprocess.run(['pdftoppm', '-r', str(a.dpi), '-f', '1', '-l', str(last), '-png', f, pre + '-pop'], capture_output=True)
    tp = time.time() - t
    line = []
    for p in range(1, last + 1):
        cands = [x for x in os.listdir(a.out) if x.startswith(f'f{n}-pop-') and int(x.rsplit('-', 1)[1].split('.')[0]) == p]
        try:
            ia = Image.open(f'{pre}-{p}.png').convert('L')
            ib = Image.open(os.path.join(a.out, cands[0])).convert('L')
        except Exception:
            line.append('?')
            scores.append(255.0)
            continue
        if ia.size != ib.size:
            ib = ib.resize(ia.size)
        # Antialiasing and glyph weight differ between any two renderers;
        # blur both a little and count pixels that still differ a lot.
        bl = ImageFilter.GaussianBlur(1.2)
        diff = ImageChops.difference(ia.filter(bl), ib.filter(bl)).point(lambda v: 255 if v > 48 else 0)
        d = 100.0 * diff.histogram()[255] / (ia.width * ia.height)
        scores.append(d)
        line.append(f"{d:.2f}")
    print(f'{os.path.basename(f)}: {" ".join(line)}  (folium {tf:.2f} s, poppler {tp:.2f} s)')
ok = sum(1 for s in scores if s < a.threshold)
print(f'\n{ok}/{len(scores)} pages with under {a.threshold}% of pixels differing from Poppler; renders in {a.out}')
