# Folium: Atrium's own PDF reader

Status: accepted 2026-11-15 as recommended (D1–D20); building. P1 built
2026-11-15 (`folium-syntax`, `folium-doc`, `folium-dump`, fuzz target).
P2 built 2026-11-15: `folium-font`, `folium-content`, `folium-render`
(tiny-skia; Liberation as the standard 14), first corpus score 73/74 pages
within tolerance of Poppler; the `atrium-pdf` engine (lazy pages, 512 px
tiles in a 192 MB cache, 24 display lists, worker pool with cancellation;
first screen 7–163 ms on the heavy set) as Limen's `pdf-viewer` child;
Scrinium quick look sends PDFs (`vm-open-files.sh`); `officina-pdf` on the
shared base (`folium-syntax::write`, `folium-font::subset` and ToUnicode,
`folium-content::ops`), export gate unchanged (1,474 pages in 5.4 s, pages
pixel-identical, read back by Folium 12/12). Open: content and render fuzz
targets, the vector Fresco path (D11).
Asked by Shankar (2026-11-15): after comparing an existing pure-Rust
renderer with our own, "let's build our own... build the specs and let me
review".

Related: [opening-files.md](opening-files.md) §7 (quick look) and D5,
[limen.md](limen.md) §2.1 (`doc-viewer`), [officina.md](officina.md) §11.3
(PAdES signing), §14 (printing and PDF), §16.17 (`officina-pdf`, the
writer), [sigillum.md](sigillum.md) §10.2 (signing a digest),
[atrium-navigator-backend.md](atrium-navigator-backend.md) §6 ("PDF is split
out entirely: separate parser, separate threat surface"),
[atrium-navigator-threat-model.md](atrium-navigator-threat-model.md) §2.4
(decoders in jails), [LICENSING-POLICY.md](../LICENSING-POLICY.md).

## 1. Short answer

- **Folium** is a set of Rust crates that read PDF: syntax, document
  model, content interpreter, fonts, text. It's ours end to end, except for a
  few self-contained decoders we already trust elsewhere (Flate, JPEG, the
  TrueType/CFF outline reader), and two codecs (JPEG 2000, JBIG2) that are
  borrowed only if a permissive pure-Rust one passes our fuzzing, else
  written by us (D2).
- Everything that reads a PDF runs **in a jail with no network and no
  files**, given the document as a read-only descriptor, under budgets
  written down in §6 (like navigator-svg's profile). A crafted PDF attacks
  that jail and nothing else.
- **Nothing in a PDF runs.** No JavaScript, ever. No launch actions. Embedded
  files are listed, never opened by themselves. Links go to the parent,
  which decides (§3).
- Opening, scrolling and zooming cost what is on screen, never the
  page count, image sizes or page detail: lazy parsing, images decoded at
  the size drawn, cancellable work, raster tiles past a node budget, and
  no CPU when nothing changes (§7.1).
- Pages are drawn **through Pergola and Fresco** (filled paths, glyph
  runs, images) so they stay sharp at any zoom and the GPU path applies.
  The effects the scene can't express yet (soft masks, blend modes,
  knockout groups, and clip paths that aren't rectangles until Fresco
  gains them, D11) are drawn on the CPU into an image for that part only
  (D3).
- One app, **Atrium PDF** (`org.atrium.pdf`, the `atrium-pdf` that
  officina.md §4 names), opens `.pdf` files from Scrinium and provides a new
  Limen role, **`pdf-viewer`**, for quick look, mail attachments and the
  Navigator (D4).
- Reading comes first (P1–P4). Forms, annotations, incremental save,
  signatures with Sigillum and printing come after, on the same document
  model, which shares its object writer with `officina-pdf` (§9).

## 2. What exists

| piece | where | use here |
|---|---|---|
| PDF writer (fonts subset, images, links, page streaming) | `officina-pdf` | moves onto Folium's base crates (§5.1); its subsetter becomes `folium-font`'s |
| Image decoders with ceilings: PNG, JPEG (zune-jpeg), GIF, WebP | `navigator-image` | DCT images (`zune-jpeg`), the `Ceilings` budget pattern |
| A bounded parser profile (counts, depth, work) | `navigator-svg::profile` | the model for §6 |
| Font reading and shaping: ttf-parser, rustybuzz, swash | `fresco-text`, `navigator-fonts` | TrueType and CFF outlines |
| Pure-Rust Flate (miniz_oxide), no C zlib | `navigator-fonts` | FlateDecode |
| Scene nodes: `Fill` (Fresco's FILL: curves, non-zero or even-odd, solid or gradient paint, an affine, a **rectangle** clip), `Glyphs`, `Image` | `pergola::node`, `fresco_protocol::FillParams` | the drawing target; strokes become fills on our side; clip paths need D11 |
| tiny-skia (BSD-3), already in the tree | `atrium-splash`, `aqueduct-gpu-host` | CPU fallback for groups (D3) |
| Limen embeds with a descriptor, jailed workers via portcullisd | `limen`, `navigator-page` | the `pdf-viewer` role |
| Quick look pane in Scrinium; pictures via `DocKind::Picture` | `scrinium`, `navigator-page` | PDFs get the same pane |
| Signing a digest with the user's token, PIN in Sigillum's sheet | `sigillum` | PAdES (P6) |

Not here yet: any PDF reading at all. Today a `.pdf` in quick look shows
only its kind and size; one that is really HTML shows as text.

## 3. Threats

PDF is a container format with a long history of exploits. What we expect
in a hostile file, and the rule for each:

| threat | rule |
|---|---|
| JavaScript (document, page, field, annotation actions; AcroForm calc) | never run, no interpreter linked; actions are parsed only to be listed and dropped |
| Launch, GoToR/GoToE to other files, ImportData, SubmitForm | never followed; Launch is never shown as a link |
| URI links | sent to the parent as `LinkActivated` (as `doc-viewer` does); the parent asks or opens the Navigator; `javascript:` and `file:` refused in the worker |
| Embedded files and attachments | listed with name and size; "Save…" goes through the picker as a new file; never opened, previewed or typed by the worker |
| XFA (XML forms, often with script) | not interpreted; the static AcroForm fallback is shown if present, else a line saying the form needs Adobe software |
| Font programs (Type 1 charstrings, CFF, TrueType hinting) | outlines only, no hinting bytecode run; our charstring interpreter has a stack, operand, subroutine-depth and op-count limit |
| Decompression bombs (Flate, LZW, chained filters) | each stream's decoded size and the document's total decoded bytes are capped (§6) before allocation, checked as output grows |
| Recursion and cycles (object references, page tree, Form XObjects, Type 3 glyphs, patterns, `Resources` inheritance) | depth limits and visited sets; a cycle is an error for that object, not a hang |
| Broken or malicious cross-reference tables | xref read lazily; on damage, a bounded scan rebuilds it (once per document); objects past the file's end are absent |
| Huge pages, images or paths | page size, image pixels and path segments capped; images decoded at the painted size, never larger than the ceiling |
| Encryption (RC4, AES) | standard security handler only; password typed into the viewer's own sheet, never sent to the parent; weak RC4 files open but say so |
| "Shadow" and incremental-update attacks on signatures | when verifying (P6), the viewer shows which revision was signed and what changed after it; by default it shows the signed revision's state alongside |
| Polyglots (a PDF that is also a ZIP, HTML, a program) | the file's type is Scrinium's decision (opening-files §4.2); the worker only reads PDF syntax and never hands the bytes to anything else |
| Timing / CPU exhaustion | a per-page time budget in the worker; past it the page shows what was drawn and "This page took too long"; the jail has an rctl CPU and memory limit |

The worker holds only its descriptor and its frescod connection. It never
gets a path, a folder, the network, Arca or another file.

## 4. Scope

**In v1 (P1–P4): reading.**

- PDF 1.0 to 2.0, including object streams and cross-reference streams,
  linearized files read like any other.
- All standard filters: Flate, LZW, ASCIIHex, ASCII85, RunLength,
  CCITTFax, DCT, JPX, JBIG2 (with globals), Crypt.
- Fonts: TrueType, Type 1 (embedded and the standard 14 by substitution),
  CFF and OpenType/CFF, Type 3, Type 0 with CID fonts, predefined and
  embedded CMaps, ToUnicode.
- Graphics: paths, clipping, line styles, all colour spaces (DeviceGray,
  RGB, CMYK, CalGray/CalRGB, Lab, ICCBased read by component count and
  its alternate, Indexed, Separation, DeviceN, Pattern), shadings 1–7,
  tiling and shading patterns, inline images, image masks and stencil
  masks, soft masks, transparency groups, blend modes, optional content
  (layers, with the default configuration).
- Annotations drawn from their appearance streams (links, highlights,
  notes, stamps, widgets), and **comments readable**: a comments pane
  listing every markup annotation with its author, date, text, replies
  and review state, in page order, each jumping to its place (§9.1).
- Document: page labels, outline (bookmarks), named destinations, the
  document information and XMP title.
- Text: extraction in reading order for find, copy and accessibility;
  word and line boxes for selection.

**After v1:** filling AcroForms (P5), adding annotations (P5), incremental
save (P5), signature verification and PAdES signing (P6), print path (P7),
tagged PDF structure for accessibility (P7), redaction (later, only with a
true content removal, never a black box over text).

**Not planned:** JavaScript, XFA, 3D (U3D/PRC), embedded multimedia
(RichMedia, Flash), the public-key security handler (until asked).

## 5. Pieces

| crate | what | notes |
|---|---|---|
| `folium-syntax` | lexer, objects, indirect references, xref tables and streams, object streams, damaged-xref rebuild, all filters, decryption | reads through `pread` on the descriptor, not the whole file; every allocation charged (§6) |
| `folium-doc` | page tree with inherited attributes, resources, outline, names, page labels, annotations, AcroForm fields, optional content, embedded file list, document info | read-only in v1; P5 adds edits as a new revision |
| `folium-font` | font dictionaries and programs: TrueType and CFF outlines (`ttf-parser`), our Type 1 parser and charstring interpreter, Type 3, CID fonts, encodings, CMaps, ToUnicode, standard-14 substitution (D7) | glyph outlines cached per font, charged to the budget |
| `folium-content` | the content stream interpreter: graphics state, text state, operators to a **display list** of paths, glyph runs, images, shadings and groups | one interpreter for drawing and text extraction, so find and copy match what is drawn |
| `folium-render` | display list to Pergola nodes; the CPU fallback (tiny-skia) for groups the scene can't express; tiles at device scale | §7 |
| `folium-text` | glyph runs to words, lines and blocks in reading order; search; selection boxes | used by find, copy and the accessibility outline |
| `atrium-pdf` | the app and the Limen `pdf-viewer` child: one binary, two entry points like `navigator-page` | §8 |

Borrowed (D2): `miniz_oxide` (Flate), `zune-jpeg` (DCT), `ttf-parser`
(TrueType and CFF outlines). Codecs we may borrow if a permissive pure-Rust
one passes our fuzzing and budget hooks, else write: JPEG 2000, JBIG2.
Ours: everything else, including LZW, CCITTFax, RunLength, the ASCII
filters, decryption (with the `aes` and `md-5`/`sha2` RustCrypto crates for
the primitives), Type 1, and CMaps (the predefined CMaps are data from
Adobe's `cmap-resources`, BSD-3).

### 5.1 One base for reading and writing, shared with Officina (asked by Shankar, 2026-11-15)

*"Perhaps we should share the Folium crate with Officina so they share a
common base?"* Yes. Today `officina-pdf` (about 700 lines) writes PDF with
its own string formatting, number and string escaping, cross-reference
table and TrueType subsetter. Folium needs all of that read the other way,
and P5 needs it written again for incremental save. So the base is one set
of crates that both sides use, with the dependency one way: Officina
depends on Folium, never the reverse.

| shared piece | crate | reader uses it for | Officina uses it for |
|---|---|---|---|
| objects, lexer, serializer, xref read and write, object streams | `folium-syntax` | parsing hostile files, under §6's budgets | writing export output (`officina-pdf` keeps only the walk from `officina_text::paint` items to pages) |
| filters (Flate both ways, the rest decode-only) | `folium-syntax` | decoding streams | compressing content, images and fonts |
| content operators as one typed `Op` enum | `folium-content` | the interpreter's input | the export's output, written by the serializer instead of format strings |
| fonts: reading programs, and subsetting TrueType (moved from `officina-pdf/src/font.rs`), ToUnicode CMaps both ways | `folium-font` | drawing text, extraction | embedding subsets |
| signatures: byte ranges, CMS/PAdES, verification | `folium-sign` (P6) | showing and checking signatures | "Export as signed PDF" from Write, Sheets and Slides (officina.md §11.3) |
| PDF/A pieces: XMP metadata, output intent | `folium-doc` | showing PDF/A conformance | PDF/A-2b export, which officina.md §16.17 lists as not yet done |

What this buys:

- One escaping and serialization path, tested by both sides, instead of
  two that can disagree.
- **A free round-trip gate.** Officina already paints a page for the
  screen and for PDF from the same items; Folium reading that PDF back and
  drawing it must match Officina's own screen paint. Every Officina
  export becomes a Folium test, and every Folium bug in reading what we
  write shows up at once.
- Slides and Sheets get PDF export from the same base when they need it.

What it costs: `officina-pdf` is moved onto `folium-syntax`,
`folium-content` and `folium-font` in P2 (when those crates exist), with
the Officina export gate (1,474 pages, officina.md §16.17) run before and
after to show nothing changed in the output's rendering or speed. The
reader's budgets live in the reading functions only, so writing pays no
cost for them.

## 6. Budgets

As `navigator-svg::profile` does for SVG, the limits are constants in one
file, documented, and each has a test that a file just past it is refused
cleanly:

| limit | value (initial) |
|---|---|
| file size read | 1 GiB (pages read lazily; a large file costs only what is drawn) |
| objects in the xref | 8 M |
| nesting depth of arrays/dictionaries | 64 |
| one stream decoded | 256 MiB |
| all streams decoded, held at once | 512 MiB (the jail's rctl is the hard stop) |
| filter chain length | 8 |
| page tree depth | 64 |
| Form XObject / pattern / Type 3 nesting | 16 |
| operators per page | 16 M |
| path segments per page | 4 M |
| image pixels per page (decoded, at painted size) | 64 M |
| one image's natural size | 32 768 per axis, 256 MiB frame |
| fonts per document | 4 096; one program at most 32 MiB |
| Type 1 charstring ops per glyph | 64 K; subroutine depth 10 |
| time per page | 5 s on the worker, then the page is shown as far as it got |
| damaged-xref rebuild | once per document, bounded by the file size |

Fuzzing (cargo-fuzz) covers `folium-syntax` (objects, xref, each filter),
`folium-font` (Type 1, CFF via our wrapper, CMaps) and `folium-content`;
we report the coverage reached, not exec counts
(navigator-backend §7).

## 7. Drawing

The display list is drawn into the window through Pergola:

- **Paths** (fill with non-zero or even-odd) become `Node::Fill`. Strokes
  (width, caps, joins, miter limit, dashes) are turned into fills by our
  stroker in `folium-render`, as FILL expects. Rectangular clips (the most
  common, page and cell boxes) use FILL's clip; any other clip path needs
  a path clip in Fresco (D11), and until then is drawn by the CPU fallback
  for the clipped content.
- **Text** becomes glyph runs. Glyphs from embedded fonts are drawn as
  outlines (paths) cached per font and size band, since PDF fonts are
  subsets with private encodings that a shaper can't use; text in a
  standard-14 font with a substitute uses the substitute's outlines.
- **Images** are decoded in the worker at the painted size (as
  navigator-image does) and become `Node::Image`.
- **Shadings** of types 2 and 3 (axial, radial) become gradients where
  Fresco has them; types 1 and 4–7 are sampled to an image at the painted
  size.
- **Groups** with a soft mask, a non-Normal blend mode, knockout or
  isolation are drawn with tiny-skia into an image for that group at the
  painted size (D3). The rest of the page stays vector.

Colour: CMYK and the other device spaces convert with simple formulas in
v1; ICC profiles are read for their component count (colour management is
later work, for the whole system, not just here).

### 7.1 Huge documents, many images, high zoom (asked by Shankar, 2026-11-15)

*"The same caution during design as Officina. Huge PDFs with lots of
images, high zoom etc. should not become sluggish or hog the CPU."* The
rule is Officina's (officina.md §7.4): **the cost of opening, scrolling
and zooming depends on what is on screen, never on the number of pages,
the size of the images or how detailed a page is.** A 20,000-page
manual, a 2,000-page book of 600 dpi scans and a city map with millions
of path segments should feel like a two-page letter. A PDF is
read-only to us in v1, so this is easier than Officina's case in one way
(no repagination) and harder in another (we don't control what's inside a
page).

**Opening reads almost nothing.**

- The trailer and cross-reference table are read from the end of the
  file with `pread`; objects are parsed when first asked for. The file is
  never read whole, and never mapped (the descriptor has no mmap right,
  opening-files §6.3).
- Page count comes from the page tree's `/Count`. Page sizes come from
  each page's `MediaBox`, `CropBox`, `Rotate` and `UserUnit`, small
  dictionaries read without touching the content. The tree is walked
  from the reading position outwards; until the walk is done, unseen pages
  take the size of their neighbours and ScrollView's anchoring keeps the
  view from jumping when a size turns out different. So the scrollbar is
  right at once for nearly every file, and exact soon after.
- The outline (bookmarks) and page labels load when the sidebar asks.
- A damaged cross-reference table is rebuilt by one bounded scan in the
  background; pages are shown as the scan finds them, in file order.

**Only pages near the view do any work.**

- Pages are the bands of pergola.md §10.4. About three screens of pages
  are resident as scene groups; the rest are evicted with their decoded
  content. A 20,000-page document holds the same scene as a 10-page one.
- A page's content stream is interpreted once into a **display list**,
  kept in an LRU with a fixed budget (start at 128 MB) and dropped first
  under memory pressure. The display list carries a bounding box per item
  and a coarse spatial grid, so drawing a part of a page visits only the
  items in that part.
- Shared resources (fonts, images, Form XObjects, patterns used on many
  pages) are keyed by object number and decoded once for the document,
  under the same budget.

**Images: decoded at the size they're drawn, never larger.**

- An image's pixel size and colour space come from its dictionary, so a
  page lays out and draws its placeholder without decoding anything.
- Pixels are decoded only for resident pages, at the size they appear on
  screen at the current zoom, rounded up to a power-of-two step so a small
  zoom change reuses them. A 600 dpi scan shown at 100% decodes to about
  the screen's size, not 5,100 by 6,600.
- Decoders that can, decode small directly: JPEG 2000 decodes only the
  resolution levels needed; JBIG2 and CCITT decode a band at a time into a
  downsampler; Flate and LZW images are decoded row by row into the
  downsampler, so a huge image never exists at full size. JPEG is the
  common scan format and zune-jpeg only decodes full size, so a scaled
  DCT decode (1/2, 1/4, 1/8, the last from DC coefficients alone) is ours,
  in P4; until then a JPEG is capped by the natural-frame ceiling in §6.
- Decoded pixels live in an LRU with a fixed budget (start at 256 MB, as
  Officina). An image that isn't ready draws a placeholder in its frame;
  nothing on the page moves when it arrives. Thumbnails decode at
  thumbnail size from the same machinery.

**Zoom: tiles, scaled at once, redrawn when it settles.**

- During a pinch or Ctrl+wheel, ScrollView scales what is already drawn
  (pergola.md §10.4), so zooming never waits. About 100 ms after it
  settles, the visible area is redrawn at the new scale, visible tiles
  first; tiles from older zoom levels stay until replaced.
- A page is drawn as **vector** (Fill, Glyphs and Image nodes) when the
  items visible in the view fit a per-window node budget (start at 8,000
  nodes, under Fresco's 65,536-op frame ceiling, officina.md §10.5).
  When they don't (a whole map, a dense chart, a CAD drawing at fit-page),
  the view is drawn as **raster tiles** (512 px, at device resolution)
  made from the display list on the worker pool, and becomes vector again
  as zooming in brings the visible items under the budget. Either way the
  scene sent to Fresco stays under a fixed ceiling, whatever the page
  holds.
- At high zoom only the items under the view are visited (the spatial
  grid), so 6,400% on a detailed map costs about what 100% on a plain
  page does. Page coordinates are kept in f64 so tiles line up at any
  zoom; the zoom range is 10% to 6,400%.

**The CPU is used for what's visible, then left alone.**

- All parsing, interpreting, decoding and tile drawing runs on a worker
  pool off the UI thread, at most one thread fewer than the cores, at
  background priority. The UI thread only builds scene groups from
  finished results, inside the frame budget.
- Every task carries the generation of the view it was asked for. A
  scroll or zoom that makes it stale cancels it: interpreters check
  between operators, decoders between rows or bands, so a cancelled page
  stops within a few milliseconds instead of finishing.
- Order: visible tiles, then the rest of the visible pages, then one
  screen ahead in the direction of travel. Nothing more is prefetched.
  Thumbnails are made only for the thumbnails on screen. When those are
  done the worker pool is empty and the app uses **no CPU** (no timers, no
  polling), until the view or the window changes.
- Find runs in the background, page by page from the current one, with
  results as they come; each page's extracted text is kept in a small
  cache so a second search is quicker. It never holds up scrolling and it
  stops when the find bar closes.
- On battery (energy-policy.md), the pool uses fewer threads and the
  look-ahead screen is skipped.
- The jail's rctl limits are the backstop, not the plan: memory 1 GiB,
  CPU the pool's share.

Everything here is a target with a gate (§10).

### 7.2 Measured: board schematics and layouts (asked by Shankar, 2026-11-15)

*"Analyse a PDF with lots of vector shapes like a board schematic. This
is one of the heavy examples."* Four PDFs were made with KiCad 7's
`kicad-cli` from its own demo boards (`video`, an 8-sheet A3 design, and
`kit-dev-coldfire-xilinx_5213`): the schematics, and each board's layout
with all copper, silkscreen, fab and edge layers on one page, which is
how boards are often shared for review. Counted from the decoded content
streams; the reference times are PDFium, one thread, on the cloud
container, for comparison only:

| file | page content | operators | painted paths | segments | largest path | PDFium fit 1600 px / 400% / 1600% window |
|---|---|---|---|---|---|---|
| video layout (A3) | 9.5 MB | 476,401 | 47,508 | 304,228 | 93,788 segments | 220 / 64 / 10 ms |
| coldfire layout (A4) | 4.3 MB | 235,959 | 33,731 | 137,596 | 28,441 | 126 / 124 / 38 ms |
| video schematic, densest sheet | 2.4 MB | 192,276 | 55,064 | 55,455 | 37 | 129 / 25 / 12 ms (sheet 2) |
| coldfire schematic, sheet 1 | 1.9 MB | 153,761 | 44,994 | 46,458 | 37 | 98 / 31 / 8 ms |

What they look like inside, and what each means for Folium:

1. **Many tiny paths, not a few big ones.** A schematic sheet is 20,000
   to 55,000 separately stroked paths with a median of **one segment
   each** (every wire piece, pin and glyph stroke is its own
   `m l S`), each preceded by its own colour and often its own line
   width. Drawn one path per scene node, a sheet at fit-page is
   50,000 nodes: over the 8,000-node budget of §7.1 and close to
   Fresco's 65,536-op frame ceiling. So the display list **coalesces runs
   of paths with the same paint state** (stroke colour, width, cap, join,
   dash; fill colour and rule), in drawing order, into one item, and
   strokes are expanded into one fill per run. These files use a handful
   of colours and widths, so a sheet becomes a few hundred items and
   stays vector at every zoom. Order is kept: two runs merge only when
   nothing painted between them overlaps (checked on bounding boxes), so
   the stacking of a layout's layers comes out right.
2. **Text is strokes; the searchable text is invisible.** KiCad draws
   every label with its stroke font (lines), and writes the same text
   again in render mode 3 (invisible) for search and copy (1,419 text
   objects on the densest sheet, all `3 Tr`). So drawing never needs a
   font here, and find, copy and the accessibility outline must read
   invisible text, never skip it. Other tools (Altium, OrCAD) use real
   fonts instead; both are common.
3. **A few huge polygons.** A layout's copper pour is one path of up to
   **94,000 segments** with thousands of holes, spanning the board. It
   is inside the 262,144-verb limit of one Fresco FILL, but sent whole it
   would be redrawn for every tile it touches. So paths above a size
   (start at 4,096 segments) are **cut per tile** in the display list
   (clipped to each 512 px tile at the zoom being drawn, and cached with
   the tile), and below a zoom where its detail is under a device pixel,
   **simplified** for that zoom (vertices closer than a quarter pixel
   dropped; 17% of the video layout's line segments are under 0.5 pt, so
   invisible at fit-page). The page's full detail is kept for zooming
   in.
4. **Zoom needs local coordinates.** At 6,400% an A3 page is about
   100,000 px across, past FILL's ±32,768 coordinate range. Paths are
   kept in page space as f64 and sent re-based to the tile's or view's
   origin (§7.1 already keeps f64; this is the reason).
5. **Simple structure.** No Form XObjects, no images, four fonts, `q`/`Q`
   depth 1, no transparency. These pages are heavy only by count, so the
   interpreter's per-operator cost matters most: 476,000 operators must
   interpret in well under 100 ms (Rust, streaming from the decoder,
   cancellable between operators), and the spatial grid makes 1,600%
   cost a few milliseconds, as PDFium's culling shows.
6. **First page.** PDFium needs 220 ms for the video layout at fit on one
   thread; Folium's 150 ms first-page target (§10) therefore depends on
   drawing tiles on the worker pool in parallel, and on coalescing
   (fewer, larger fills raster faster in Fresco's compute renderer than
   tens of thousands of small ones).

These four KiCad files are generated from the `kicad-demos` package by a
host-side script and are the "heavy vector" set in the corpus (they are
not committed: KiCad's demos are GPL, which is fine for test input on the
host, not for the repository). Vendor schematics from Altium, OrCAD and
Cadence (real fonts, hatched fills, embedded logos) join them as they are
found or supplied.

### 7.3 Measured: a vendor board guide from Word (2026-11-15)

Shankar's copy of Lattice's CertusPro-NX Versa Evaluation Board user guide
(75 pages, 10 MB, made by Microsoft Word 365, tagged, 10 subset fonts
including CID TrueType, 154 images, 132 of them with soft masks, and the
board's schematics in an appendix). It is kept outside the repository and
the shared files, in the corpus on the host only. A different kind of
heavy from KiCad's:

| | content | PDFium, one page at fit 1,600 px |
|---|---|---|
| typical text page | a few thousand operators, a picture or two | 5 to 20 ms (median 8 ms) |
| schematic page 39 | one Form XObject, 34,504 operators, 3,635 paths, 1,568 clips | **3,535 ms** |
| schematic page 38 | one Form XObject (6.7 MB), 186,435 operators, 162,659 segments (138,831 curves) | 2,554 ms |
| whole document | 228,000 page operators plus the forms | 26.8 s for all 75 pages, of which about 25 s is the 12 schematic pages |

Poppler draws page 39 in 0.55 s and page 38 in 0.75 s: the content isn't
large, it is drawn wastefully. What is inside, and what Folium does:

1. **A clip per element, all the same.** Each schematic is an EMF or
   Visio drawing Word converted into one Form XObject, and the conversion
   wraps **every element** in `q`, a clip, the element and `Q`: page 39 has
   1,526 clips, every one the same axis-aligned rectangle (written as a
   four-point path, not `re`), slightly larger than the form's box. PDFium
   builds a clip mask for each. Folium (a) recognises a clip path that is
   an axis-aligned rectangle however it is written, and uses FILL's
   rectangle clip for it; (b) drops a clip that contains the bounding box
   of everything drawn under it, or equals the clip already in force; (c)
   lets runs merge across `q`/`Q` when the clip and paint state come out
   the same (§7.2 point 1). Page 39 then has no clips at all and becomes a
   few hundred coalesced items.
2. **Curves, many.** Symbols are drawn with Béziers (139,000 `c` on page
   38). They are flattened once per zoom band into the display list's
   cache, at a quarter-pixel tolerance, not per tile per frame.
3. **A non-uniform form matrix** (x scaled 0.116, y 0.155). Strokes are
   widened in the form's own space and then transformed, so a line keeps
   its true (elliptical) pen, as the spec requires; the stroker takes the
   full matrix, not a scalar width.
4. **Text as many short runs.** 1,596 text objects on page 39, each with
   its own `Tm` and one `TJ` (every label placed alone). Glyph runs with
   the same font, size and colour on the page are coalesced into one
   `Glyphs` item per run of drawing order, as paths are.
5. **Images are not the problem here.** The largest is 1.3 MP; all 154
   together are 12 MP. They exercise soft masks (alpha), the ordinary
   case on any Word or PowerPoint export, so `SMask` images are drawn
   straight as RGBA, without the group path of §7.
6. **Tagged.** Word writes the structure tree and marked content around
   everything (`BDC`/`EMC`, up to 294 on a page). The interpreter carries
   them for text extraction and accessibility at no drawing cost.

Gate: this file joins the heavy set. Every page under 150 ms on 4 cores,
the twelve schematic pages included; scrolling through the appendix at
4,000 px/s with ScrollView's gates; the whole document's text extracted
in reading order matches the tagged structure's order.

## 8. The app and the role

**Atrium PDF** (`org.atrium.pdf`): a signed bundle with `[[opens]]` view
for `application/pdf`, the system's default for PDF (replacing the
"Navigator's document worker" line in opening-files §5.2). Graphics, no
network, no audio. Opened from Scrinium with the file as a read-only
descriptor through the opens socket, as Write and Player are. The window
has a page column, a sidebar with thumbnails and the outline, zoom (fit
width, fit page, percent), find, copy, page number and labels, and a
password sheet when the file is encrypted. Links inside the document jump;
outside links go to the Navigator, with the address shown first.

**Mail visual copies.** A separate fixed Limen role, `pdf-sanitizer`, runs the
same binary with the capability-free sanitizer bundle. An `ExportImages`
request carries the source descriptor. A complete conversion returns
`ImageCopy` with a sealed PDF descriptor. An encrypted document returns
`PasswordNeeded`; other refusals return `Error` without a descriptor. The
strict rendering profile and user-visible losses are specified in Mail §8.2.1.

**`pdf-viewer` Limen role**: `takes_fd`, `plain` slot, user role whose
default is `org.atrium.pdf`. Parent to child: `Load { dark }` with the
descriptor, `GoTo { page }`, `Find { query, step }`, `SetZoom`. Child to
parent: `Loaded { pages, title }`, `LinkActivated { url }` (http, https,
mailto only), `Found`, `Error { code }`, and `PasswordNeeded` (the
password itself is typed into the child's own sheet, never the parent's).
Scrinium's quick look sends PDFs to it; a mail client's attachment
preview and the Navigator's PDF links will too.

Why its own role rather than a new `DocKind` for `doc-viewer`: PDF is a
separate parser and a separate threat surface (navigator-backend §6), so it
gets a separate binary and jail, and limend picks providers per role.

## 9. After reading

- **P5 forms, comments and review, save.** Filling AcroForm text, check,
  radio and choice fields, regenerating appearance streams for the fields
  changed; adding, replying to and resolving comments (§9.1). Saving is an **incremental update**
  (a new revision appended, earlier bytes untouched, so earlier signatures
  stay valid), written with the object writer shared with `officina-pdf`.
  The app gets a read-write descriptor only when opened for edit
  (opening-files D8).
- **P6 signatures.** All the ways to sign, and verifying, are in §9.3.
- **P7 printing and accessibility.** The future print service takes PDF
  (officina.md §14); Folium renders pages for printers without PDF
  support. Tagged PDF structure feeds the accessibility outline.

### 9.1 Annotations, comments and review (asked by Shankar, 2026-11-15)

PDF's comments are markup annotations, and review is built from them:
replies point at the comment they answer (`IRT`), and review states
(Accepted, Rejected, Cancelled, Completed, and Marked) are small replies
with a `State`. Folium uses exactly that, so a review done in Atrium PDF
reads correctly in Acrobat, Preview or pdf.js, and theirs read in ours.

- **Reading (P2–P3).** Every annotation is drawn from its appearance
  stream; one without a stream (common in files from other tools) gets an
  appearance made by us from its type, colour and rectangle. The comments
  pane lists markup annotations (sticky notes, highlight, underline,
  strike-out, squiggly, free text, ink, line, square, circle, polygon,
  stamp, caret, file attachment) grouped into threads, with author, date,
  the text, the review state per reviewer, and a filter by author, type
  and state. Comment text is plain text to us: the rich-text form (`RC`,
  an XHTML fragment) is reduced to text and paragraphs, never rendered as
  HTML.
- **Commenting (P5).** Select text and highlight, underline, strike out
  or add a note; a sticky note anywhere; free text; ink with a pen or
  finger; boxes, circles and arrows; stamps (Approved, Draft and so on,
  drawn by us). Reply in the pane; set a state (Accepted, Rejected,
  Completed); mark resolved. The author is the user's display name, from
  their account. Each comment's appearance stream is written by us, so
  other viewers show it as we do.
- **Saving (P5).** Comments are saved as an incremental update, so the
  original bytes and any earlier signatures stay intact. Comment-only
  changes keep a signature valid where the signature's permissions allow
  annotations (DocMDP level 3), and the viewer says when they don't.
- **Comparing and summarizing (P5).** "Comments summary" makes a short
  list (page, author, text, state) to copy or export as text. Comparing two
  PDFs' text, as Write compares documents (officina.md §16.26), is later
  work.
- **With Officina.** See §9.2: Write's comments and tracked changes go
  out as these annotations, and a PDF review comes back into Write.

### 9.2 What Officina's review gives PDF (asked by Shankar, 2026-11-15)

*"See how Officina handles reviews and see if any of those translate into
PDF."* Officina's review (officina.md §16.21–16.27) rests on five ideas.
Each was checked against what PDF can carry:

| Officina | in PDF | how |
|---|---|---|
| **Comments** on a range, threads, resolved (§16.21) | yes, directly | a Highlight over the range's quads with the text in `Contents`, author `T`, date `M`; replies are annotations with `IRT` pointing at it; "resolved" is a review-state reply `Completed` by whoever resolved it. A range that crosses pages is one annotation per page tied with `RT /Group`, shown as one comment |
| **The page reads clean**; changes only as a thin bar and a count; one lane to go through them (§16.22) | yes, as the viewer's design | Atrium PDF does the same: the page shows the document; markup annotations are drawn faintly, the one picked in the comments lane fully; the status line says "12 comments by 3 people · Review". The lane is the comments pane of §9.1, with Next, Previous, and state buttons by key. "Show all markup" draws everything as other readers do |
| **Edits, not fragments**: word-level, replacement as one, moves, formatting batched, with a sentence ("“thirty” → “fourteen”") (§16.24) | yes for text edits, as markup | an insertion is a Caret with the inserted text; a deletion a StrikeOut; a replacement a StrikeOut and a Caret grouped (`RT /Group`, the text-edit intents other readers use for "replace text"), so Acrobat and pdf.js show them as edits. The sentence goes in `Subj`. Formatting changes and moves have no PDF markup kind: they go as a note with the sentence ("moved “Costs” above “Plan”", "bold on 6 headings") |
| **Change sets**, one person's pass, optionally named (§16.22) | partly | the author is standard (`T`); the set's name rides in a key of our own (`/AtSet`), which other readers ignore and we group by. Accept and reject can't apply to a PDF's text (it isn't reflowable), so in PDF they are review states (Accepted, Rejected) for whoever exported or returns it |
| **Compare** two versions, word level, moves, formatting apart (§16.26) | yes, for text | Folium extracts each PDF's text in reading order as paragraphs (`folium-text`); Officina's `diff()` (Myers, paragraph pairing, moves, the alignment slide of §16.27) runs on them unchanged, since it is generic over sequences. The result is a set of edits shown in the lane over the second PDF, with deletions as faint marks where the text was. Not a pixel diff, though "show pixel differences" for drawings can come later |
| **Merge** reviewed copies, conflicts as one card (§16.27) | yes, simpler than Officina's | our saves are incremental updates (§9), so every reviewer's copy is the original's bytes plus their appended comments. Merging takes each copy's annotations (deduplicated by their unique name `NM`) and states onto one copy; comments never overwrite each other. The only conflicts: two people changing the same comment's text (shown as one card, both kept), and a copy whose page content (not just comments) changed, which is refused with "this copy was edited, not only reviewed" and offered as a compare instead |

From that, three pieces of work across Write and Atrium PDF:

1. **Export review to PDF (Write, P5).** Write ▸ Export PDF gains
   "Include comments and changes": *Clean* (default, Write's Reading
   view: the proposed text, with comments and edits as annotations) or
   *All markup* (Write's All view: old text drawn struck through as well).
   Each annotation carries an anchor key of our own (`/AtAnchor`: the
   paragraph id and offsets in the source document) so it can come back.
2. **Import review from PDF (Write, P5).** The loop people actually run:
   Write exports a PDF, it goes to someone who only has a PDF reader, they
   comment and mark edits in Acrobat, Preview, pdf.js or Atrium PDF, and
   it comes back. Write ▸ Import Review… reads the annotations: comments
   become comments (with threads and resolved), Caret, StrikeOut and
   replace groups become tracked changes by that reviewer, each in a
   change set named after them, so the normal review lane goes through
   them. Placement uses `/AtAnchor` when the annotation came from our
   export (exact), else the annotation's quads matched to the source text
   through the page's extracted text and the export's layout (the same
   paginated document Write exported from), and what can't be placed is
   listed, never dropped silently.
3. **Review in Atrium PDF (P3 reading, P5 writing).** The lane, states,
   compare and merge above, for any PDF.

The engine pieces are shared, not copied: Officina's `diff()` and edit
grouping move to a small crate both can use (`officina-diff`, depending on
neither), and the annotation reader and writer are `folium-doc`'s, used by
Write's export and import.

### 9.3 Signing: drawn, certificate, and other ways (asked by Shankar, 2026-11-15)

*"Signing by drawing the signature on the mouse or trackpad, digital
certificate, or other ways?"* There are two different things people call
"signing a PDF", and Atrium PDF offers both, together or apart, and says
plainly which one a document has:

- a **visible signature**: a mark on the page that looks like a signature
  (drawn, typed or an image). It shows intent; it proves nothing about who
  made it, and anyone can copy it.
- a **digital signature**: a cryptographic signature over the file's bytes
  with a certificate. It proves who signed (as far as the certificate's
  issuer vouches) and that nothing changed since. It may have a visible
  part, or none.

**Ways to make the visible part (P5).**

| way | how |
|---|---|
| Draw | a signing pad sheet: mouse, trackpad, pen (pressure from Tactus where the device gives it) or finger; strokes smoothed into curves, variable width; Clear and Done. Saved as vector ink, not a bitmap, so it stays sharp at any zoom |
| Type | the name typed and set in one of a few signature-style OFL fonts, picked from a row of previews |
| Image | a scan or photo of a signature, picked with the system picker; the background is taken out (threshold on paper colour) and it is kept as an image |
| Initials | the same three ways, kept separately, for "initial every page" |

Saved signatures and initials are the user's own and live in **Arca**
(arca.md), not in the app or a file: an item kind `SignatureMark` that
arcad releases only to an app that holds the new capability
`identity.signature-mark` (Atrium PDF and Write), and only after a press in
the sign sheet (frescod activation purpose `IDENTITY`, as Sigillum's sheets
use). So a hostile PDF can't make the viewer stamp the user's signature,
and no other app can read it.

Placing it: into a signature field if the form has one (§4 forms), else
anywhere the user drops it, sized by dragging. It is written as an
annotation with our own appearance stream, so every reader shows it. On
its own (no certificate) the status line and the signature panel say
**"Signed with a drawn signature (not verified)"**, so nobody takes it for
more than it is. Flattening it into the page content is an explicit
choice in Save As, never the default.

**Digital signatures with a certificate (P6).**

- **Where the key is.** (a) A hardware token through Sigillum: a YubiKey or
  other PIV card, an OpenPGP card, or an Indian **DSC** USB token (Class 3,
  e.g. the InnaIT token through its jailed vendor module, sigillum §16.5).
  (b) A software certificate (a `.p12`/`.pfx` the user imports), held in
  Arca as today's software client certificates are (arca.md §8.5); the key
  never leaves arcad, which signs the digest itself. In both cases the app
  only ever sends a digest and gets a signature back; how the user
  approves it (PIN, password, a fingerprint, whatever Arca and Sigillum
  allow for that key) is entirely theirs and not Folium's concern
  (sigillum §10.2), with the
  chooser listing certificates that can sign documents (key usage
  digitalSignature or nonRepudiation).
- **What is written.** PAdES baseline: B-B with no network, B-T with a
  timestamp from a TSA, B-LT with the certificate chain and revocation
  answers (OCSP or CRL) embedded so it verifies years later, B-LTA with a
  document timestamp on top. The network parts (TSA, OCSP, CRL) go out
  through Nuntius from a small fetch the user's settings allow, never from
  the viewer's jail. Default: B-T when online and a TSA is set, else B-B,
  upgraded to B-LT when the file is next saved online.
- **Kinds of signature.** *Approval* (the usual, any number of signers,
  each an incremental update), and *certification* (the first signer
  certifies the document and says what may change after: nothing, form
  filling, or form filling and comments, PDF's DocMDP). Field locks (lock
  these fields when signed) come with the form field.
- **Visible appearance.** A digital signature can use the drawn, typed or
  image mark from above as its appearance, with the signer's name, date
  and "Digitally signed" written beside it by us; or be invisible (listed
  in the signature panel only). This is the default when a certificate is
  chosen: draw or pick your mark once, sign with the token.
- **Several signers.** "Request signatures" adds signature fields, each
  named for a person and in an order; each signer signs their field in
  turn as an incremental update, so earlier signatures stay valid. Sending
  it to them is the user's own business (mail, share); Atrium PDF only
  shows whose turn it is.

**Verifying (P6, reading starts in P3).** The signature panel lists each
signature: signer, issuer, time (and whether the time is the signer's
clock or a timestamp authority's), the PAdES level, and one of: valid,
valid but changed after signing (with what changed: comments, form fields,
or the content itself, by comparing revisions, §3), not trusted (chain
doesn't reach a trusted root), or broken (bytes don't match). Trusted
roots are the system's store plus India's CCA root for DSC certificates
(so Class 3 signatures from Indian authorities verify) and the EU trusted
lists later if asked. Revocation is checked online through Nuntius when
the user opens the panel and the network is up; embedded answers (B-LT)
are used offline. A drawn-only signature is listed as "not a digital
signature".

**Other ways, and what we'd need.**

| way | what it is | recommendation |
|---|---|---|
| **Aadhaar eSign** (India) | an online signature from a licensed eSign service provider (eMudhra, NSDL, CDAC and others), OTP or biometric on Aadhaar, the certificate made per signature | later, as an option (D20): it needs a contract with a provider and their API over the network, and sends the document's hash and the user's Aadhaar number to them. Built as a separate signer behind the same "digest in, signature out" boundary, so the viewer doesn't change |
| **Device key** ("signed on this computer") | a key in Arca or the TPM, self-issued | no: it proves only "this machine", which readers outside Atrium can't check, and it would look like more than it is |
| **Passkey / FIDO2** | WebAuthn signatures | no for documents: they sign a challenge for a site, not a document format any reader verifies |
| **Cloud signing services** (DocuSign and the like) | the document goes to their service | not built in; a user can still upload a PDF to one through the Navigator |

## 10. Testing

- **Corpora**, kept outside the repo like Officina's (never committed):
  pdf.js's test files (Apache-2.0), PDFium's test corpus (BSD), qpdf's
  tests (Apache-2.0), the veraPDF and Isartor PDF/A sets, a GovDocs1
  sample (public domain) for real-world variety, plus files we write
  ourselves for each limit and threat in §3 and §6.
- **Differential rendering** against reference rasterizers run on the
  build host only, never shipped: pdftoppm (Poppler) and MuPDF are GPL
  and AGPL, which the licensing policy forbids in the runtime but not as
  host-side test oracles (D8). The score is per page: the share of pages
  within a pixel-difference threshold, reported as a number with the
  worst pages listed, as Officina's LibreOffice gates do.
- **Text** against the corpora's known text where it exists, and against
  `pdftotext` on the host the same way.
- **Gates in the VM**: quick look of a PDF in Scrinium through
  `pdf-viewer` (checks like the picture's: its own jail and uid, read-only,
  no path, no `/media`, the jail goes when the pane closes); the app opened
  from Scrinium; a hostile set (JavaScript actions, Launch, a bomb, a
  reference cycle, an endless content stream) shown or refused within the
  budgets, with no network and no file access attempted.
- **Speed and scale** (§7.1), on the dev VM for comparison between builds
  and on real hardware for numbers:

  | case | target |
  |---|---|
  | first page of a typical document | drawn under 150 ms after the descriptor arrives |
  | open a 20,000-page document | first page under 300 ms; memory for the document under 64 MB before any page is drawn |
  | scroll a 2,000-page book of 600 dpi scans at 4,000 px/s | no stall over 100 ms at 60 Hz, p99.9 frame work within two periods (ScrollView's gates); placeholders allowed; memory flat at the cache budgets |
  | the §7.2 and §7.3 heavy set (KiCad schematics and layouts, the Word-made board guide) | first page under 150 ms on 4 cores; fit-page scene under the node budget, vector; pinch 25% to 6,400% with every frame on time |
| pinch from 25% to 6,400% on a map with 1 M path segments | every frame on time (scaled tiles); sharp within 300 ms after it settles |
  | scene per window | resident nodes and scene bytes under a fixed ceiling at any page count, page complexity and zoom |
  | after interaction stops | worker pool idle within 1 s; 0% CPU while nothing changes |
  | scroll or zoom away from a slow page | its work cancelled within 10 ms |

## 11. Build order

| step | what | shows |
|---|---|---|
| **P1** | `folium-syntax` and `folium-doc`: objects, xref and recovery, all filters but the image codecs (DCT and CCITT go to the image layer with JPX/JBIG2, so they decode at the painted size, §7.1), decryption, page tree, outline, info; a host tool `folium-dump` (page count, objects, outline); budgets and fuzzing set up | page count and outline for the whole corpus with no panics; fuzz coverage reported |
| **P2** | `folium-font` (TrueType, CFF, Type 1, standard 14), `folium-content`, `folium-render` (paths, text, images, clips); `pdf-viewer` child; Scrinium quick look sends PDFs; from the start, not later: lazy pages, the display-list and image caches, the worker pool with cancellation, decode at painted size, vector/raster tiles (§7.1); `officina-pdf` moved onto the shared base (§5.1) | typical text-and-vector PDFs in quick look, in the VM gate; first corpus score; the §10 scale gates; Officina's export gate unchanged and its output read back by Folium |
| **P3** | `atrium-pdf` app: page column, zoom, thumbnails, outline, find and copy (`folium-text`), links, password sheet; system default for PDF | opened from Scrinium end to end in the VM |
| **P4** | the long tail: Type 3, CID fonts and CMaps, shadings 1 and 4–7, patterns, soft masks, blend modes and groups via the CPU fallback, optional content, JPX, JBIG2, CCITT; corpus score raised to the target in D9 | corpus score |
| **P5** | forms; visible signatures (draw, type, image, initials; kept in Arca); comments and review (add, reply, state, resolve, compare, merge); Write's export with comments and changes and its Import Review (§9.2); incremental save | edit gate; a review done in Atrium PDF opens with the same threads and states in pdf.js and in a host-side reader |
| **P6** | §9.3: digital signatures (Sigillum tokens, Arca software certificates), PAdES B-B to B-LTA, certification, several signers, verification panel; drawn/typed/image marks move here from P5 if P5 is long | signs with the YubiKey and the InnaIT DSC token; Acrobat-equivalent verification of our signatures by a host-side checker (pyHanko or OpenSSL CMS) and of the corpus's signed files |
| **P7** | print path, tagged PDF | with the print service |

Rough sizes: P1 about a week, P2 one to two weeks, P3 about a week, P4
several weeks and then ongoing as the corpus finds things.

## 12. Decisions, with recommendations

| # | question | recommendation |
|---|---|---|
| D1 | Names | Crates `folium-*` (Latin "leaf, page"); the app is "Atrium PDF", id `org.atrium.pdf`, binary `atrium-pdf` as officina.md already says |
| D2 | What we borrow | Only self-contained decoders we already use or that pass our fuzzing and budget hooks: `miniz_oxide`, `zune-jpeg`, `ttf-parser`, RustCrypto primitives; JPEG 2000 and JBIG2 borrowed only on the same terms, else ours. Syntax, model, interpreter, Type 1, Type 3, CMaps, text and drawing are ours |
| D3 | How pages are drawn | Vector through Pergola and Fresco, with a CPU (tiny-skia) image only for groups the scene can't express; move those into Fresco later if they turn out common |
| D4 | How other apps embed a PDF | A new Limen role `pdf-viewer`, provided by Atrium PDF in its own jail; not a new kind for the Navigator's `doc-viewer` (separate threat surface) |
| D5 | Active content | Never run JavaScript or follow Launch, GoToR or Submit; XFA not interpreted; embedded files only saved through the picker, never opened by the viewer |
| D6 | Encrypted PDFs | Read the standard security handler (RC4 and AES); the password is typed in the viewer's own sheet and not kept; saving it in Arca can come later if asked |
| D7 | The standard 14 fonts | Metric-compatible OFL substitutes: Liberation Sans, Serif and Mono for Helvetica, Times and Courier (Officina already lists them); for Symbol and ZapfDingbats, an OFL font with those glyphs mapped by name (URW's base35 fonts are AGPL, so not them) |
| D8 | Reference renderers in tests | Poppler and MuPDF as host-side oracles only, never shipped or linked; corpora never in the repo |
| D9 | Fidelity target for v1 | 95% of the corpus's pages within the pixel threshold of the reference, every miss listed with its cause; 100% with no crash, hang or budget overrun outside the listed limits |
| D10 | Order against other work | P1–P3 next (PDFs in quick look and in their own app), then back to opening-files T5 and Player end to end; P4 continues alongside |
| D11 | Clip paths, which FILL can't express (its clip is a rectangle) | Add a path clip to Fresco's FILL (a clip path id the server rasterizes as a coverage mask), done by the Fresco thread during P2; until it lands, non-rectangular clips go through the CPU fallback |
| D12 | Very detailed pages | Coalesce runs of paths with the same paint state, cut paths over 4,096 segments per tile and simplify them below pixel size (§7.2); then vector while the visible items fit a per-window node budget (start at 8,000), raster tiles from the display list past it; tuned on the corpus, so the scene stays under a fixed ceiling at any zoom |
| D13 | Folium and Officina | One base: `folium-syntax`, `folium-content`, `folium-font` (and later `folium-sign`) are used by both the reader and Officina's export; `officina-pdf` moves onto them in P2, its export gate unchanged before and after |
| D14 | Comments and review | PDF's own markup annotations with replies (`IRT`) and review states, so reviews interoperate with other readers; read in P2–P3 (pane), written in P5 as incremental updates; Write's comments exported as annotations |
| D15 | Write's PDF export of a reviewed document | Default *Clean* (proposed text, comments and edits as annotations, as Write's Reading view); *All markup* on request; annotations carry `/AtAnchor` so they can come back |
| D16 | Bringing a PDF review back into Write | Yes: Write ▸ Import Review… turns comments into comments and text-edit marks into tracked changes by each reviewer, placed by anchor or by text, listing anything it can't place |
| D17 | Sharing the review engine | Move Officina's word diff and edit grouping into `officina-diff`, used by Write and Atrium PDF; annotations read and written only by `folium-doc` |
| D18 | Visible-only signatures | Offered (draw, type, image, initials), saved in Arca as `SignatureMark` released only after a press in the sign sheet, and always labelled "drawn signature (not verified)"; never flattened unless asked |
| D19 | Digital signatures | PAdES through Sigillum tokens (PIV, OpenPGP, Indian DSC) and Arca-held software certificates; default B-T when online, B-B otherwise, B-LT on the next online save; network parts through Nuntius; trust roots the system store plus India's CCA |
| D20 | Aadhaar eSign | Later, opt-in, as a separate signer behind the same digest boundary, once a provider contract exists; device-key and passkey document signatures not offered |
