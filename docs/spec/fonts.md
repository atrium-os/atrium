# System fonts: the font library

Status: built 2026-11-17. Shankar asked for a larger open font set in the base
system (Office stand-ins, other scripts, everyday families), licensing
permitting, and approved this design: files in one folder, no font server.

## 1. What ships

The files, their licences and sources are listed in `fonts/README.md`. In
short: 106 static TrueType faces (about 33 MB on disk).

- **Office stand-ins**: Liberation Serif, Sans and Mono 2.1.5 (metrics of
  Times New Roman, Arial and Courier New), Carlito (Calibri), Caladea
  (Cambria). IBM Plex Serif rounds out the system's own Plex.
- **Scripts**: Noto Sans/Serif (Latin, Greek, Cyrillic), Noto Sans Mono,
  Symbols, Symbols 2 and Math, and the Noto faces for Devanagari, Bengali,
  Gurmukhi, Gujarati, Oriya, Tamil, Telugu, Kannada, Malayalam, Sinhala,
  Thai, Lao, Khmer, Myanmar, Tibetan, Georgian, Armenian, Hebrew, Arabic
  (Naskh), Syriac, Thaana, Ethiopic and Cherokee.
- **Everyday families**: Source Sans 3, Source Serif 4, Source Code Pro, Fira
  Sans and Mono, Roboto, Inter.

Rules: unmodified upstream files, each folder with its licence; OFL or Apache
2.0 only (no GPL, LGPL, AGPL; no Microsoft or Apple fonts); static TrueType
outlines only (the PDF writer subsets `glyf` fonts, and a mapped file is drawn
as is, with no instancing). CJK is left for an on-demand install as a font
package (§5; about 6 MB a weight as static TrueType).

The Navigator's pinned set (`navigator-render::fontset`, IBM Plex and DejaVu,
compiled in and canonicalized) is unchanged and never reads the library: a
web page must not learn which fonts a system has.

## 2. No font server

The fonts are read-only files in one system folder, `/usr/share/fonts/atrium`,
a copy of the repo's `fonts/` folders (`scripts/install-fonts.sh`; VM scripts
use `scripts/vm-fonts.sh`). A process maps a face the first time it needs it,
so all processes share the page cache's one copy and nobody loads fonts it
never draws. There is no daemon to ask, crash or attack.

Each face's BLAKE3 address is pinned in `atrium-fonts/src/catalog.rs`. The
library hashes a file when it first maps it and refuses one that differs: a
changed or truncated file is an error, never drawn. The folder is root-owned
and read-only; a jail that draws text gets it read-only (Portcullis `fonts`
capability) or, for the Fresco renderer, as a copy in its bundle.

Lookup order for the folder: `ATRIUM_FONTS_DIR`, then
`/usr/share/fonts/atrium`, then (a build run from the source tree, tests) the
repo's `fonts/`.

## 3. Matching

`atrium_fonts::Library` is a table, not a configuration language.

1. A family we ship, by name, ignoring case and spacing.
2. A metric-compatible stand-in (`ALIASES`, `metric: true`): Times New Roman,
   Times, Nimbus Roman, Tinos to Liberation Serif; Arial, Helvetica, Nimbus
   Sans, Arimo to Liberation Sans; Courier New, Courier, Nimbus Mono, Cousine
   to Liberation Mono; Calibri to Carlito; Cambria to Caladea. Lines break
   where the author's did.
3. The nearest look (`metric: false`): Verdana, Tahoma, Segoe UI and other
   sans names to Liberation Sans; Georgia to Source Serif 4; Consolas, Menlo
   to Liberation Mono; Mangal, Nirmala UI, Latha and other Windows script
   fonts to their Noto face; another face of a known family ("Calibri Light",
   "Arial Black") to that family.
4. By class: a serif-sounding name to Liberation Serif, a mono one to IBM Plex
   Mono, anything else (and no name) to IBM Plex Sans, the system face.

Within the family, the face nearest the asked slant and then weight; an
upright face stands in for a missing italic, never a shear.

A character the chosen face lacks goes, in order, to its script's Noto face
(a Unicode block table, `SCRIPTS`), the pinned DejaVu fallback, then Noto
Sans, Symbols, Symbols 2, Math and Serif.

## 4. Who uses it

- `officina-text::PinnedFonts` (Write, Slides, Sheets text, Mail compose, the
  PDF export): faces `0..n` are the pinned set's, `n + id` the library's,
  loaded as first used. `face_bytes`, `face_address` and `face_family` cover
  both.
- `pergola-document::Faces` (DocumentView, Write's and Slides' drawing):
  registers a library face with Pergola when a glyph first names it, from the
  mapped bytes without a copy (`pergola::Face::from_static`).
- `fresco-scene-server` (frescod's server-side text, what a Pergola `Text`
  node draws with): a name it has no built-in alias for is looked up in the
  library, so any Pergola app can name a shipped family or a stand-in. A
  character the run's font lacks falls back the same way (its script's Noto
  face, then the general ones), and those runs, and any run of a complex
  script, are shaped whole with rustybuzz (conjuncts, vowel signs, Arabic
  joining, right-to-left order within the run); the run's own Latin text stays
  one glyph per character, as Pergola measures it. Fallback glyphs share the
  run's atlas page, so a run is still one slot.

## 5. Installed font packages

Fonts beyond the shipped set (CJK first) come as Opifex `kind = "font"`
packages (`docs/spec/opifex.md` §8.1): platform-signed, OFL or Apache 2.0,
static TrueType. `opifexd` puts each in `/usr/share/fonts/atrium-extra/<id>/`
and rewrites the root-owned pin list `atrium-extra/pins` atomically.
`atrium-fonts` (`src/extra.rs`) reads that list, never the fonts' name tables:

```text
# atrium-fonts pins 1
<package>\t<family>\t<package>/<file>.ttf\t<weight>\t<0|1 italic>\t<ranges>\t<blake3:hex>\t<size>
```

`ranges` are the code points the face is for (`3040-30FF,4E00-9FFF`, hex, at
most 64). A line that breaks a rule (package id, a file outside its package
folder or not `.ttf`, weight, ranges, address) is skipped; the header must be
first; at most 4096 faces. An installed face gets the next id after the shipped
ones (`atrium_fonts::faces()` lists both), is matched by family name like a
shipped one, and comes in fallback after the character's shipped script face and
before the general Noto faces (`Library::fallback`). Its file is mapped only when
its length and BLAKE3 match its line.

A process reads the list once. frescod reads it again (`extra::refresh`, at most
once a second, only when the list's time or length changed) when no face has a
character, so an install shows up in Pergola labels without a restart; other
apps see it at their next start. Ids never move within a process: a newer list
only adds faces, and an uninstalled face's file fails its check. The Portcullis
`fonts` capability mounts `/usr/share/fonts`, so `atrium-extra` is in it; the
Fresco renderer bundle copies it as of the bundle.

Checked with Noto Sans JP 2.004 (static TTF instances, OFL, 5.7 MB a weight,
from the `@expo-google-fonts/noto-sans-jp` npm package, which carries Google
Fonts' static files): Write's PDF export embeds subset CJK, and frescod draws
a Japanese label in the VM.

## 6. Open

- Bidi is built (UAX #9, `unicode-bidi`). frescod: a run is one line in
  visual order, right-to-left level runs shaped right to left. officina-text
  (`officina-text/src/bidi.rs`): glyphs stay logical for measuring and line
  breaking; each line gets a visual order (`Line::visual`), a right-to-left
  paragraph starts at the right edge (start indent and alignment mirrored),
  brackets at right-to-left levels are mirrored for shaping, and the caret
  and clicks follow the visual order (`ParaLayout::caret_x`, `hit_x`).
  Paragraph direction is a paragraph property (`ParaProps::rtl`; unset, the
  first strong character decides), kept in the native file and read and
  written as `w:bidi` (.docx), `style:writing-mode` (.odt) and `rtl`
  (.pptx); LibreOffice keeps it both ways in both formats
  (`officina-convert/examples/bidi_lo.rs`, 4/4). A selection across
  directions highlights each stretch as drawn (`ParaLayout::range_spans`).
  Left: arrow keys move in logical order; no command yet to set a
  paragraph's direction in Write.
- Tessera already holds a content hash for every file; once the VFS exposes it
  the library can compare that with the pinned value instead of hashing.
- Colour emoji waits on colour glyph support. CJK packages wait on Opifex
  publishing them; Han characters go to the first installed CJK face, with
  no language preference yet (SC/TC/JP/KR forms differ).
- Write's style font list (`STYLE_FONTS`) to offer the new families.
