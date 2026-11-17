# Regenerate src/catalog.rs from the files in fonts/ (needs fonttools, blake3):
#   python3 atrium-fonts/gen-catalog.py fonts > /tmp/c && (head -4 atrium-fonts/src/catalog.rs; cat /tmp/c) > atrium-fonts/src/catalog.rs
import sys, os, glob, blake3
from fontTools.ttLib import TTFont
root = sys.argv[1]
dirs = ["liberation","carlito","caladea","ibm-plex-serif","noto","source","fira","roboto","inter"]
rows=[]
for d in dirs:
    for p in sorted(glob.glob(f"{root}/{d}/*.ttf")):
        f=TTFont(p, lazy=True)
        n=f['name']
        fam = n.getDebugName(16) or n.getDebugName(1)
        w=f['OS/2'].usWeightClass; it=bool(f['OS/2'].fsSelection & 1)
        h=blake3.blake3(open(p,'rb').read()).hexdigest()
        rows.append((fam, os.path.relpath(p, root), w, it, h, os.path.getsize(p)))
print("// Generated from the files in fonts/ (family from the name table, weight")
print("// and italic from OS/2). Re-run when a file changes; the test")
print("// `catalog_matches_files` refuses a stale row.")
print("pub const FACES: &[FaceInfo] = &[")
for fam,rel,w,it,h,sz in rows:
    print(f'    FaceInfo {{ family: "{fam}", file: "{rel}", weight: {w}, italic: {str(it).lower()}, address: "blake3:{h}", size: {sz} }},')
print("];")
