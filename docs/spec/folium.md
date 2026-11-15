# Folium: Atrium's own PDF reader

Status: proposed 2026-11-15, for Shankar's review.
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
| PDF writer (fonts subset, images, links, page streaming) | `officina-pdf` | the object writer is shared for incremental save (P5) |
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
  notes, stamps, widgets).
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

- **P5 forms, annotations, save.** Filling AcroForm text, check, radio and
  choice fields, regenerating appearance streams for the fields changed;
  adding highlights, notes and ink. Saving is an **incremental update**
  (a new revision appended, earlier bytes untouched, so earlier signatures
  stay valid), written with the object writer shared with `officina-pdf`.
  The app gets a read-write descriptor only when opened for edit
  (opening-files D8).
- **P6 signatures.** Verifying: the signer's certificate chain against the
  system trust store plus India's CCA roots (for DSC tokens), the
  signed byte ranges, what changed after each signature (§3). Signing:
  PAdES-B-B, the app computes the digest, Sigillum signs with the user's
  token after the PIN in its own sheet (officina.md §11.3, sigillum §10.2).
  Timestamps (PAdES-B-T) need a TSA over the network, so they go through
  Nuntius later.
- **P7 printing and accessibility.** The future print service takes PDF
  (officina.md §14); Folium renders pages for printers without PDF
  support. Tagged PDF structure feeds the accessibility outline.

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
  | pinch from 25% to 6,400% on a map with 1 M path segments | every frame on time (scaled tiles); sharp within 300 ms after it settles |
  | scene per window | resident nodes and scene bytes under a fixed ceiling at any page count, page complexity and zoom |
  | after interaction stops | worker pool idle within 1 s; 0% CPU while nothing changes |
  | scroll or zoom away from a slow page | its work cancelled within 10 ms |

## 11. Build order

| step | what | shows |
|---|---|---|
| **P1** | `folium-syntax` and `folium-doc`: objects, xref and recovery, all filters but JPX/JBIG2, decryption, page tree, outline, info; a host tool `folium-dump` (page count, objects, outline); budgets and fuzzing set up | page count and outline for the whole corpus with no panics; fuzz coverage reported |
| **P2** | `folium-font` (TrueType, CFF, Type 1, standard 14), `folium-content`, `folium-render` (paths, text, images, clips); `pdf-viewer` child; Scrinium quick look sends PDFs; from the start, not later: lazy pages, the display-list and image caches, the worker pool with cancellation, decode at painted size, vector/raster tiles (§7.1) | typical text-and-vector PDFs in quick look, in the VM gate; first corpus score; the §10 scale gates |
| **P3** | `atrium-pdf` app: page column, zoom, thumbnails, outline, find and copy (`folium-text`), links, password sheet; system default for PDF | opened from Scrinium end to end in the VM |
| **P4** | the long tail: Type 3, CID fonts and CMaps, shadings 1 and 4–7, patterns, soft masks, blend modes and groups via the CPU fallback, optional content, JPX, JBIG2, CCITT; corpus score raised to the target in D9 | corpus score |
| **P5** | forms, annotations, incremental save | edit gate |
| **P6** | signature verification, PAdES signing via Sigillum | signs with the YubiKey and the InnaIT DSC token |
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
| D12 | Very detailed pages | Vector while the visible items fit a per-window node budget (start at 8,000), raster tiles from the display list past it; the budget is tuned on the corpus, so the scene stays under a fixed ceiling at any zoom |
