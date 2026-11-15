# Officina: a native office suite for Atrium

**Status:** accepted 2026-11-14: Shankar agreed to all 17 decisions in §17 as recommended (§7.4 very large documents, §9.3 very large decks and §10.5 compositing safety added the same day). O1 built 2026-11-14 (§16.1); O2 built 2026-11-14 (§16.2). O4a (`.xlsx`) built 2026-11-14 (§16.3); O4b (styles and number formats) built 2026-11-14 (§16.4). O3a (text engine) built 2026-11-14 (§16.10); O3b (`RichTextEditor`) built 2026-11-14 (§16.11); O3c (`atrium-write`) built 2026-11-14 (§16.12).

*Asked by Shankar, 2026-11-14:* "a key set of apps for any [OS] is an office suite. The
traditional ones don't make sense for Atrium: Java based, huge monolithic apps. We can do
much better using all the Atrium security features, adding widgets when necessary to
Pergola, direct canvas drawing. Handling huge amounts of tables for the spreadsheet etc.
Let us scope it out first."

**Officina** (Latin: *workshop*) is the shared engine. The apps keep plain names, as
foundation apps do ([NAMING.md](../NAMING.md)): `atrium-write` (word processor),
`atrium-sheets` (spreadsheet) and `atrium-slides` (presentations).

Related specs: [pergola.md](pergola.md) (§10 ScrollView, DocumentView and bands, §11
Canvas, §12 layout), [limen.md](limen.md), [scrinium.md](scrinium.md),
[tabula.md](tabula.md), [arca.md](arca.md), [sigillum.md](sigillum.md),
[nuntius.md](nuntius.md), [resolution-independence.md](resolution-independence.md),
[atrium-document-profile-v1.md](atrium-document-profile-v1.md),
[atrium-navigator-threat-model.md](atrium-navigator-threat-model.md) (converter jail, image
decoding), [fresco-scene-groups.md](fresco-scene-groups.md), [insula.md](insula.md) §0.6.

---

## 0. Short answer

- **Three small apps over one engine, not one monolith.** Each app is its own binary and
  its own jail; they share Officina's crates (document model, text engine, grid and formula
  engine, charts) as libraries. Nothing starts that the open documents don't need.
- **One jailed process per open document.** It holds that document's file descriptor and
  nothing else of the user's. A hostile file that breaks the engine reaches one document.
- **Foreign formats are converted in throwaway jails.** `.docx`, `.xlsx`, `.pptx`, `.odt`,
  `.ods`, `.odp`, `.rtf`, `.csv` and pasted HTML are parsed by a converter with no network,
  no files, capped memory and time, which emits Officina's own model as bounded, validated
  messages (the Navigator converter's pattern). Macros, OLE objects, DDE and external links
  never run; they are reported, and kept inert only where round-tripping needs them.
- **Files come from Scrinium.** Open is the picker's descriptor; Save As needs a new picker
  mode that creates a file and replaces it atomically (§4). Home folders and "recent files"
  depend on Scrinium work not yet built.
- **Drawing is Pergola and Fresco, no private toolkit.** Text is shaped in-app into glyph
  runs; pages, slides and sheet tiles are scene groups; scrolling is `ScrollView`'s
  shift-and-strip; charts and shapes are `Canvas` fills. New Pergola widgets come out of
  this work and every app gets them: a rich-text editor, a 2D virtualized grid, horizontal
  scrolling, rulers, colour and font pickers (§10).
- **Spreadsheets built for size.** Column-chunked typed storage, shared formulas stored once
  per run, a range-based dependency graph, incremental and multi-threaded recalculation that
  computes what is on screen first and never blocks the UI. Targets: 100 million cells in a
  workbook, a 1,048,576 × 20 sheet open in seconds, 60 Hz scrolling anywhere (§8, §15).
- **Documents built for size too.** A 1,000+ page document with thousands of images, a
  TOC, cross-references and long tables: an indexed block tree, images laid out by size and
  decoded only near the viewport within a fixed budget, pagination that resumes from a page
  checkpoint and stops when breaks line up again, numbering and references derived from
  counts, pages as bands (§7.4). Decks likewise: slides laid out independently, media stored
  once and decoded near the screen, video only in the media player, and presenter mode
  preparing the next slides ahead of the click (§9.3).
- **Secrets and signatures through the system.** Document passwords through Arca's secure
  input, data-source credentials through Arca's courier, external data only through Nuntius
  with a per-document consent, document signing with Sigillum's tokens (PAdES / XAdES), keys
  never in the app.

## 1. Why not port an existing suite

| | LibreOffice / OpenOffice / OnlyOffice | Officina |
|---|---|---|
| shape | one process for every document and every format filter; millions of lines of C++ (LibreOffice also needs Java for parts) or a web app in a browser shell | three small apps, one jailed process per document, converters in their own jails |
| untrusted parsing | `.docx`/`.xls` filters run in the editor's process, with the user's files and network | a converter jail with nothing to steal and a deadline |
| macros | Basic, Python, JavaScript, VBA compatibility: run in the editor with the user's authority | none run (§17 D7) |
| UI | its own toolkit (VCL) drawing through platform backends | Pergola on Fresco; same scrolling, text, accessibility and theming as every app |
| licence | MPL-2.0 / LGPL / AGPL (OnlyOffice) | MIT / Apache-2.0 ([LICENSING-POLICY.md](../LICENSING-POLICY.md)) |

What we take from them is knowledge, not code: their format filters are the best
documentation of what real `.docx` and `.xlsx` files contain, and their test corpora are
the bar for fidelity.

## 2. Pieces

```
 atrium-write        atrium-sheets        atrium-slides          (apps: UI + commands)
      │                   │                    │
      └──────────── officina-core ─────────────┘                 (model, op log, undo, styles, native format)
           │              │               │
   officina-text    officina-grid    officina-draw               (rich text + layout | cells, formulas, recalc | shapes, charts)
           │              │               │
   Pergola widgets: RichTextEditor, GridView, Canvas, ScrollView (2D), Ruler, pickers
           │
   Fresco: glyph runs, fills, images, scene groups, SCROLL shift

 officina-convert-ooxml | -odf | -rtf | -csv | -html (via navigator-normalize)   (jailed converters)
 officina-pdf                                                                     (PDF writer, in-process)
```

| crate | holds | notes |
|---|---|---|
| `officina-core` | document model shared by the three apps, op log, undo/redo, style system, units, native format reader/writer, autosave journal | no UI; fuzzed |
| `officina-text` | rich-text model (paragraphs, runs, lists, tables, fields, notes, comments, tracked changes), incremental paragraph layout and pagination | shares shaping, line breaking (Knuth–Plass, hyphenation) and fonts with the Navigator's layout crates |
| `officina-grid` | cell storage, formula parser and evaluator, dependency graph, recalculation, sort/filter/pivot, number formats | no UI; the largest crate; fuzzed and differential-tested (§15) |
| `officina-draw` | shapes, connectors, images, charts as `Canvas` items; slide masters and layouts | tiny-skia stroker, as `Canvas` already uses |
| `officina-convert-*` | one binary per format family, run in a converter jail | parse in, emit `officina-core` messages out (§5) |
| `officina-pdf` | PDF export (and PDF/A) from laid-out pages | our own model in, so it runs in the app |
| `pergola-richtext`, `pergola-grid` | the new widgets (§10) | in Pergola's tree so other apps use them |

A text document can contain a table, a chart or a small sheet, and a slide can hold a
table, because all three apps use the same model and engines in-process. Objects inside
one Officina document are never cross-process embeds (§17 D3).

## 3. Processes and trust

```
 Scrinium picker ──fd──► limend ──fd──► atrium-sheets (doc jail, one per document)
                                           │   holds: this document's fd, its autosave dir,
                                           │          Pergola window, no network
                                           │
                                           ├── spawn ──► converter jail (per open/import)
                                           │               holds: the input bytes (fd or pipe),
                                           │               an output pipe; no fs, no net;
                                           │               rctl memory + CPU caps, deadline
                                           │
                                           ├── Nuntius (only if the user allows external data)
                                           ├── Arca (password entry, courier for data sources)
                                           └── Sigillum (sign), Tabula (clipboard), Limen (embeds)
```

**The document process.** Opening a file starts (through portcullisd, like any app) one
jailed instance of the app for that document. Its jail holds the descriptor the picker
returned, a private autosave folder, and no network. Two documents are two jails: a
crafted `.ods` that corrupts the formula engine cannot read the bank statement open in the
next window. Pergola is a library, so each process draws its own window; the per-process
cost is the engine's memory plus Pergola's, to be measured in O1 (target under 40 MB for an
empty document, §15). A light "launcher" window (new, recent files, templates) is a
separate instance with no document.

**The converter jail.** Built the way the Navigator's converter is (threat model §2,
portcullis §6.5.2e′): a fresh jail per conversion, rctl `memoryuse` and `pcpu` limits plus
the converter's own hard `RLIMIT_AS` and `RLIMIT_CPU`, a wall-clock deadline, no network,
no file system beyond its own binary. It reads the file from a descriptor and writes
messages to a pipe. It does all decompression (ZIP with ratio and total-size caps), XML
parsing (no DTDs, no external entities), image decoding (memory-safe decoders, dimensions
capped before allocation, as navigator-backend `image.rs`), and font sanitising for
embedded fonts. The document process decodes only Officina's own message format, with
every length bounded.

**What a compromise reaches**, by component:

| compromised | reaches | doesn't reach |
|---|---|---|
| converter | the one file it was converting, CPU until its deadline | any other file, network, the user's display, the document process's memory |
| document process | that document, its autosave folder, its window, anything the user grants it while open | other documents, other apps, Arca values, the network unless external data was allowed |
| Officina launcher | the recent-files list (names and grant handles, §4.3) | file contents (it opens nothing itself) |

## 4. Files

### 4.1 Open

The app asks Scrinium's picker (`limen::picker`, `Filter::file` with the format's
extensions, `write: true` when editing) and gets an open, Capsicum-narrowed descriptor
(scrinium.md §4.2). Opening from Scrinium's manager ("open with", scrinium.md §8, not built)
goes the same way: the manager hands the descriptor to a new instance.

### 4.2 Save and Save As

Writing through a read-write descriptor in place is not crash-safe: a power cut in the
middle leaves a torn file. And Save As needs a file that doesn't exist yet, which the
picker couldn't hand out. Picker additions, owned and to be built by Scrinium after its
network-shares S1 work (agreed with the Scrinium thread, 2026-11-14; nothing in O1 needs
them):

| message | where | meaning |
|---|---|---|
| `Open { filter: { kind: Save, suggested_name, extensions } }` | picker sheet | the sheet (limen slot kind `sheet`) shows a name field and folders; the user names the file. If the name exists, the sheet asks to replace before going on |
| `ToParent::SaveTarget { name, handle }` + `EMIT_FD` | picker sheet | a **write-only** descriptor to a new temporary file in the chosen folder, plus a **save handle**. The sheet then closes |
| `Commit { fsync: true }` → `Saved` | save role | Scrinium fsyncs the temporary file, `renameat`s it over the target, fsyncs the folder |
| `Abandon` | save role | Scrinium unlinks the temporary file |
| plain Save of an already-picked file | save role | the same exchange with the save handle, no sheet |

- **Commit and Abandon don't ride the sheet**, since it closes as soon as the user picks and
  writing a big document takes a while. They go to a windowless Scrinium role that limend
  starts on demand, keyed by the save handle. limend binds the handle to the parent's uid,
  app id and document process, and drops it when that process exits; Scrinium then unlinks
  any temporary file not committed.
- **limend's `EMIT_FD`** gains `O_WRONLY` regular files for `SaveTarget` only (today it passes
  only `O_RDONLY` and `O_RDWR`). Scrinium makes that change.
- **The temporary file** is created by Scrinium with `O_EXCL | O_NOFOLLOW`, mode 0600, named
  `.<name>.save-XXXXXX` in the target folder. Scrinium sweeps stale ones in folders it opens.
- **Fallback.** Where an atomic replace isn't possible (a read-only stick, a folder that can't
  take a temporary file, a network share whose server lacks atomic rename, such as SFTP
  without `posix-rename@openssh.com`, which Scrinium's SFTP server checks at connect), Save
  writes in place after a full backup to the autosave folder, and says so.

### 4.3 Recent files and reopening

Reopening a file after a restart needs a grant that outlives the process, which Atrium has
no notion of yet (scrinium.md §8 "always allow"). Scrinium issues a **bookmark**, an
Arca-sealed token naming (user, app id, file identity) that only Scrinium can redeem,
re-checking that the file still exists and the user still owns it. File identity is
(volume id, file id, generation), so a copied file is a different file: the bookmark says
the original is gone rather than opening the copy. The launcher keeps bookmarks, never paths
or descriptors. Agreed with the Scrinium thread 2026-11-14; needs Scrinium to hold the
user's home folders first (scrinium.md §8). Until then, recent files work only within a
session.

### 4.4 Autosave, recovery and versions

- **Autosave journal.** Every edit is an op in `officina-core`'s log (§6). The log is
  appended to the document's private autosave folder every few seconds and on blur; a crash
  replays it over the last saved file. Nothing is written to the user's file without Save
  (§17 D9 asks whether to change that).
- **Versions.** On Tessera, the native format is chunked so that saves of a big workbook
  share unchanged chunks (content-addressed, Tessera dedups them), and "browse versions"
  lists earlier saves. On other file systems (FAT sticks, network shares) only the current
  file exists.

## 5. Formats

### 5.1 Native format

Proposed (§17 D2): an Officina native format, `.owrite` / `.osheet` / `.oslides` (names
open), as a ZIP-free chunked container:

- a header and a manifest of chunks, each chunk postcard-encoded and length-bounded;
- spreadsheets stored by column chunk (§8.1), so a save rewrites only changed chunks and a
  load maps chunks lazily (a 10 GB workbook opens showing the first screen, not after
  reading everything);
- styles, shared strings and the formula table as their own chunks;
- the op log since the last full save can be appended (fast save of huge files);
- every bound the reader enforces is written in the spec (cells, string length, nesting,
  chunk size), in the spirit of the document profile's G3, so even a native file is safe
  to open in the document process without a converter.

ODF (`.odt` / `.ods` / `.odp`) stays first-class for interchange; it is an open standard
and what governments ask for. The alternative is to make ODF the native format (§17 D2).

### 5.2 Foreign formats and fidelity

| format | read | write | first phase |
|---|---|---|---|
| `.docx`, `.xlsx`, `.pptx` (OOXML, ECMA-376 transitional and strict) | converter | converter (writer in the doc process, our own model in) | O3 / O4 / O6 |
| `.odt`, `.ods`, `.odp` (ODF 1.3) | converter | writer | O3 / O4 / O6 |
| `.csv`, `.tsv` (encoding and delimiter detection) | converter (streaming, for huge files) | writer | O2 |
| `.rtf`, `.txt`, `.md` | converter | writer | O3 |
| pasted / dropped HTML | `navigator-normalize` in a jail, then profile → rich text | Tabula offers HTML subset | O3 |
| `.xlsm`, `.docm`, `.pptm` | as their base format; macros reported, kept inert (§5.3) | with the kept macro parts, byte for byte (D7; `.xlsm` built in O4f, §16.8) | O4 |
| `.doc`, `.xls`, `.ppt` (binary, pre-2007) | later, converter only (§17 D8) | never | O7+ |
| `.pdf` | not edited; opens in `atrium-pdf` | `officina-pdf` export, PDF/A-2b | O3 |

Writers run in the document process because their input is our own validated model; their
output is checked by reading it back through the converter in CI.

Every import ends with a **fidelity report** in the document's info panel: what was
converted approximately (an unsupported chart type drawn as a picture, a font substituted),
what was kept inert, and what was dropped. Saving back to a foreign format lists what the
format can't carry. No silent loss.

### 5.3 What never runs, and what is kept inert

| in the file | on open | on save to the same format |
|---|---|---|
| VBA / Basic / Python macros | not run; banner "this file has macros; Atrium doesn't run them" | the macro part is kept byte-for-byte and never parsed, unless the user removes it (§17 D7) |
| OLE objects, ActiveX | the cached preview image is shown; the object is not activated | kept inert |
| DDE fields, `WEBSERVICE`, web queries, external workbook links, linked images | shown with their cached values; never refreshed without consent (§11.2) | kept |
| embedded fonts | sanitised in the converter; used for this document only | kept if the font's embedding flag allows |
| embedded media (audio, video) | played through Limen's `media-player` role on a press | kept |

## 6. Document core

- **Model.** A document is a tree of typed nodes (sections, paragraphs, tables, cells,
  sheets, slides, shapes) with stable ids. Rich text inside a node is a rope of characters
  with attribute runs; sheets are §8's column chunks rather than nodes per cell.
- **Every change is an op** (insert text, set attributes, set cell range, insert rows,
  move shape). The op log gives undo/redo (inverse ops), the autosave journal (§4.4), "what
  changed since save", and is the hook for collaboration later.
- **Collaboration-ready, not collaborative.** Text ops carry ids (each character's origin as
  (replica, counter), as Eg-walker / Yjs-style sequence CRDTs do) so real-time co-editing over
  Concursus can be added without changing the model. Grid ops are range ops with
  last-writer-wins per cell. No collaboration in v1 (§17 D6).
- **Styles.** Named paragraph, character, table, cell and list styles with inheritance,
  matching what ODF and OOXML carry, so round-trips keep the user's styles rather than
  flattening to direct formatting.
- **Units.** Document geometry in points (1/72 in) with 1/20 pt ("twips") precision, the
  print world's unit, independent of the screen's pt in resolution-independence.md §3. At
  100 % zoom, one document pt is one screen pt.

## 7. Word processor (`atrium-write`)

### 7.1 Layout

The Navigator's layout is a continuous flow cut into bands; a word processor needs pages
and fast relayout while typing. Proposed (§17 D4): **share the text stack, build a new
paged, incremental layout.**

- **Shared with the Navigator:** fonts (`navigator-fonts`' pinned system set plus
  metric-compatible substitutes, §13), shaping (rustybuzz), line breaking (UAX #14), Knuth–
  Liang hyphenation, total-fit justification (profile §3.7), glyph-run emission, the band
  contract (pergola.md §10.4) with a page as a band.
- **New:** paragraph layout cached per paragraph by (content, style, width) hash, so typing
  relayouts one paragraph; pagination that resumes from the first changed page and stops
  when page breaks line up again (keep-with-next, widows and orphans, keep-together tables,
  table rows split across pages with repeated headers, footnotes, headers and footers,
  sections with their own page size and columns, floating images with wrap); fields
  (page numbers, table of contents, cross-references) resolved after pagination.
- **Views:** page view, continuous (no page breaks, for drafting), outline, and read mode
  (a page-at-a-time view at full screen).
- **Goal:** a keystroke in a 500-page document shows on screen within one frame; pagination
  of later pages finishes in the background, with page count marked approximate until then.

### 7.2 Editing

The editor is Pergola's new `RichTextEditor` (§10.1), which `atrium-write` drives in its
paged mode: caret and selection across paragraphs and table cells, IME composition,
bidirectional text, spell check underlines, drag of selections, and accessibility.

### 7.3 Features by phase

| phase | features |
|---|---|
| O3 | paragraphs and character styles, lists, tables, images, page setup, headers and footers, page numbers, find and replace, spell check, word count, PDF export, `.odt` / `.docx` open and save |
| O5 | footnotes and endnotes, sections and columns, table of contents and cross-references, comments, tracked changes (accept / reject, shown and kept on round-trip), floating images with wrap |
| later | equations (MathML; OMML converted), bibliography, mail merge from a sheet, master documents, document signing (§11.3) |

### 7.4 Very large documents (asked by Shankar, 2026-11-14)

*"What about huge docs? 1000+ pages, lots of images, links, table of contents, tables,
figures."* The design rule is the spreadsheet's: **memory and time per action depend on what
is on screen and what changed, never on the document's length.** A thesis, a 3,000-page
manual or ECMA-376 Part 1 itself (about 5,000 pages, which makes it a good test file) should
feel like a 10-page letter once it is open.

**The model is indexed, not a list.**

- Paragraphs and other blocks sit in a balanced tree (a B-tree over blocks) whose internal
  nodes keep counts: characters, blocks, headings, captions by kind, footnotes, and the
  estimated height. Finding "character 4,000,000", "heading 312", "figure 87" or "the block
  at y" is O(log n), and an insert updates counts on one path.
- Text inside a block is a rope; a block's attribute runs are stored with it.
- The native file stores the tree in chunks of a few hundred blocks (§5.1), so opening reads
  the chunk index and the chunks around the reading position; other chunks load on demand
  and in the background. Saving writes changed chunks only.

**Images: layout needs sizes, not pixels.**

- On import the converter validates each image, records its pixel size and colour space, and
  stores it once in the native file as its own content-addressed chunk (the same picture used
  twenty times is stored once). Formats the decoders don't trust well are transcoded in the
  converter to PNG or JPEG.
- Layout uses only the recorded size and the frame the document gives it, so paginating a
  document with 5,000 images decodes none of them.
- Pixels are decoded only for pages near the viewport, at the size they are drawn (a 24 MP
  photo shown 300 pt wide decodes to about that, using the JPEG decoder's scaled decode where it
  can), with memory-safe decoders and dimension caps checked before allocation, as the
  Navigator's worker does (threat model §2.4). The document process is already the jail that
  holds only this document, so decoding there gives nothing away (§17 D16).
- Decoded pixels live in an LRU cache with a fixed budget (start at 256 MB). An image that
  isn't ready draws a placeholder in its frame, the same "placeholders, not stalls" rule as
  ScrollView (pergola.md §10.1); pages never move when it arrives, since its size was known.
- Thumbnails for the page strip come from the same cache at small size.

**Pagination: checkpoints and convergence.**

- At each page start the layout saves a small **checkpoint**: the block and line it starts at,
  the floats and footnotes still waiting for a place, counters (page, footnote, list numbers
  that restart per page), and the section's page style. The Navigator's backend does the same
  at band boundaries (atrium-navigator-backend.md §4.8); the idea carries over.
- An edit relayouts its paragraph (cached per paragraph, §7.1), then repaginates from the
  checkpoint of the page it is on, and **stops as soon as a page starts at the same block and
  line with the same state as the stored checkpoint**. Typing usually changes one or two pages;
  inserting a page of text pushes page breaks along until an explicit break, a section start
  or a paragraph that soaks up the difference (Knuth–Plass's stretch does this often).
- A change that genuinely shifts every later page (a new chapter near the start) repaginates
  in the background, in order, on a worker thread. Pages on screen are done first; later page
  numbers in the status bar show "≈" until pagination reaches them. The visible page never
  waits for page 900.
- On open, the reading position is laid out first (the last place the user read, or the
  start), from the nearest checkpoint saved in the native file (checkpoints are saved with the
  document, so a reopened file needs no full pagination before showing anything). Foreign
  files have no checkpoints: their first open paginates in the background, with the scrollbar
  and page count estimated from block heights and corrected by ScrollView's anchoring, so
  reading doesn't jump.
- Pages are the bands of pergola.md §10.4. A window of pages around the viewport (about three
  screens) is resident as scene groups; everything else is evicted. A 1,000-page document
  holds the same number of scene nodes as a 10-page one.

**Table of contents, numbering, cross-references, links.**

- **Numbering** (Figure 12, Table 4, equation (3.7), heading numbers, footnote numbers) is
  derived from the order-statistic counts in the block tree, not stored in the text. Inserting
  a figure renumbers nothing physically; each caption asks the tree "how many figure captions
  precede me" when it is drawn. Cost: O(log n) per visible caption.
- **Cross-references and the TOC** need page numbers, which need pagination, and the TOC's own
  length can move pages, like LaTeX. The rule: fields that show page numbers are resolved after
  pagination reaches the target; a field whose text changed relayouts its own paragraph and
  repaginates from there; at most three rounds, and if it still doesn't settle (a page number
  that oscillates between two values at a page break) the field keeps the larger width, which
  ends it. Only fields whose target page changed are touched; a TOC of 2,000 entries updates
  the entries whose numbers moved.
- A TOC, list of figures and index are ordinary generated blocks, regenerated in the
  background when headings or captions change (a heading edit shows immediately in the
  navigation pane, in the TOC after the update).
- **Links.** Internal links and cross-references point to anchors (stable block ids), so they
  survive edits and can never dangle silently: deleting an anchor marks its references broken
  and lists them. External links open only on a press, through the Navigator, with the address
  shown first (Limen `LinkActivated`'s rule, limen.md §2.1).

**Long tables.**

- A table spanning 300 pages is laid out row by row; rows split across pages and header rows
  repeat. Only rows on resident pages have scenes.
- Automatic column widths over 50,000 rows would read every cell for every edit. So a table
  has fixed column widths once it has more than a few hundred rows (imported tables already
  carry widths in `.docx` and `.odt`); "fit to contents" is an explicit command that runs in
  the background.

**Moving around a big document.**

- A navigation pane of headings (from the block tree, so it is complete before pagination
  ends), a virtualized page-thumbnail strip, go to page / heading / figure / bookmark, and
  back and forward through jumps.
- Find searches the model's text, not the laid-out pages, streaming results as it goes, with
  a per-chunk text index built in the background so the second search is instant. Replace all
  over 1,000 pages is one op-log transaction (one undo).
- Selecting all and changing a style is one op over a range; the restyle relayouts what is on
  screen and repaginates the rest in the background.

**Export.** PDF export streams page by page from the checkpoints, embedding each image once
(by content address) and subsetting fonts, so exporting 1,500 pages needs about one page of
layout state in memory at a time. It runs in the background with progress, and the document
stays editable (the export works from a snapshot of the op log).

**Not needed: master documents.** Splitting a book into several files is how other word
processors cope with size. With the above, one file scales, so master documents are left out
of v1 (§17 D17). Files from other suites that use them import as one document.

## 8. Spreadsheet (`atrium-sheets`)

### 8.1 Storage

- **Column chunks.** Each sheet column is a list of chunks of 4,096 rows (tunable). A chunk
  picks its encoding from its contents: dense `f64`, small integers, booleans, string ids
  into the workbook's shared string pool, or sparse (row → value) for scattered cells.
  Empty chunks don't exist. A column of 1 M numbers is about 8 MB; a sheet of 20 M mostly
  numeric cells about 160 MB plus formulas and styles.
- **Formulas stored once per run.** A run of cells with the same formula in relative (R1C1)
  form, `=A2*B2` filled down a million rows, is one parsed formula plus a range, as OOXML
  "shared formulas" and LibreOffice "formula groups" do. Results live in the value chunks.
- **Styles** are interned (a style id per distinct combination) and stored run-length per
  column. Row heights and column widths are kept in a Fenwick tree (prefix sums) so pixel
  ↔ row mapping is O(log n) at any size.
- **Limits** (§17 D5): rows up to 2³¹ − 1 and columns up to 2²⁰ in the model; the target is
  100 M cells per workbook in memory. Writing `.xlsx` refuses or splits sheets past its
  1,048,576 × 16,384 limits, with the fidelity report saying so.
- **Structured tables** (named tables with header rows and `Table[Column]` references) and
  several tables per sheet, as Excel has, for `.xlsx` round-trips.

### 8.2 Formulas

- **Language:** OpenFormula (ODF 1.2 part 2) semantics with Excel's syntax and function
  names as the user-facing form, since that is what people type and what `.xlsx` carries.
  Both serialise to and from one AST.
- **Functions:** the ~400 Excel functions people use, in tiers: math, statistics, text,
  lookup (`XLOOKUP`, `INDEX`/`MATCH`, `VLOOKUP`), dates, logic and dynamic arrays
  (`FILTER`, `SORT`, `UNIQUE`, `SEQUENCE`, spill ranges) first; financial, engineering and
  the long tail later. `LAMBDA`/`LET` in O5.
- **Numbers:** IEEE `f64`, displayed to 15 significant digits as Excel does; Excel's 1900
  date system (including the 1900 leap-year quirk) and 1904 for Mac-origin files.
- **Volatile** functions (`NOW`, `RAND`, `INDIRECT`, `OFFSET`) recompute on every recalc and
  are flagged in the dependency graph.
- **External** functions (`WEBSERVICE`, stock and currency data, links to other workbooks)
  are off until the user allows them for this document (§11.2).

### 8.3 Dependency graph and recalculation

- **Range edges, not cell edges.** A formula that reads `A1:A1000000` is one edge to a range,
  kept in an interval index per column; a formula group depends on a sliding range once,
  not a million times. Changing a cell finds its dependents by interval lookup.
- **Dirty propagation, then ordered evaluation.** An edit marks dependents dirty
  transitively; evaluation runs them in dependency order (topological, by strongly
  connected components so circular references are found and reported, or iterated when the
  user turns iterative calculation on).
- **Vectorised groups.** A formula group is evaluated column at a time over the chunk
  arrays, not cell by cell, so `=A2*B2` over a million rows is a loop over two `f64` slices.
- **Parallel.** Independent groups and partitions of large groups run on a thread pool
  inside the document process.
- **Visible first, never blocking.** Recalculation runs off the UI thread. Cells on screen
  are computed first; cells still dirty draw their previous value greyed with a small
  "calculating" mark in the status bar, so typing and scrolling never wait for a big
  recalc. A new edit cancels and restarts the affected part.
- **Lookup indexes.** Repeated `XLOOKUP`/`MATCH`/`VLOOKUP` over the same column build a
  hash or sorted index of that column once, invalidated by edits to it.
- **Correctness bar:** results match Excel on a differential corpus of real workbooks
  (§15); LibreOffice's results are a second oracle where Excel's aren't available.

### 8.4 Data features

Sort, filter (auto-filter and advanced), conditional formatting (evaluated for visible
cells only), data validation, named ranges, find and replace, freeze panes, split views,
grouping and outlines, pivot tables (computed over the column chunks, so a pivot of 10 M
rows is a scan, not a formula per cell), charts (§9.2), CSV import that streams into chunks
(a 2 GB CSV loads without holding the text).

### 8.5 The grid on screen

`GridView` (§10.2) draws the sheet: 2D tiles of (say) 64 rows × 16 columns are scene
groups, scrolled by `ScrollView`'s shift-and-strip in both axes; headers and frozen rows and
columns are separate groups that scroll on one axis only. A visible screen at 1440p is about
1,500 cells, so a tile's scene is a few hundred glyph runs and rect fills. Selection, the
fill handle, the active cell editor and range highlights are an overlay group, so moving
the selection doesn't touch the tiles.

## 9. Presentations and drawing (`atrium-slides`)

### 9.1 Slides

A slide is a page of `officina-draw` shapes and text frames (the same rich-text engine),
built on masters and layouts with a theme (colours, fonts) as ODF and OOXML have them.
Editing uses Pergola's `Canvas` (§11 of pergola.md) with handles, snapping guides,
alignment and grouping. The slide sorter is a `VirtualList` of thumbnails rendered from the
same scene at a small scale.

Presenting: full screen on one monitor and presenter view (notes, next slide, timer) on
another (per-monitor placement, resolution-independence.md §7); transitions and builds as a small fixed set of
declarative animations (pergola.md §4.2) so the presenter process can stall without the
animation stalling. Video on a slide plays through Limen's `media-player` role.

### 9.2 Charts

Charts are an `officina-draw` object fed from a sheet range: bar, column, line, area,
scatter, pie and doughnut, combo, with axes, legends and labels, drawn as `Canvas` fills and
glyph runs. A chart of a million points is decimated for drawing (min/max per pixel
column), never one fill per point. The same chart object appears in text documents and
slides.

### 9.3 Very large decks (asked by Shankar, 2026-11-14)

*"The same can be said for slides too."* Yes: the rules of §7.4 hold (work follows the screen,
images laid out by size and decoded near the viewport, chunks loaded on demand). A deck
differs from a book in three ways, which change where the effort goes:

- **Slides don't flow into each other.** Editing slide 40 never moves slide 41, so there is
  no pagination chain and no checkpoints. Each slide is laid out on its own, cached by
  content hash, and only when it is shown or thumbnailed. A 1,000-slide deck costs one
  slide's layout per edit.
- **The weight is media, not text.** Real big decks are a few hundred slides of full-screen
  photos, screenshots and video, often hundreds of megabytes to gigabytes. So:
  - Each image and video is one content-addressed chunk in the native file, stored once
    however many slides use it (the same logo on 600 slides is one chunk; master and layout
    pictures are shared by reference, never copied per slide).
  - Pictures decode at the size they are drawn, into the same fixed pixel budget as §7.4.
    The editor decodes for the slide being edited and the thumbnails in view; thumbnails
    decode at thumbnail size (a 6000 × 4000 photo becomes a 200 px thumbnail with the JPEG
    decoder's scaled decode, not a full decode and a resize).
  - Video is never decoded by the deck: a slide's video is a Limen `media-player` embed
    (fresco-video.md) given the chunk as a blob, started on the slide or a press. Until then
    the slide shows the poster frame stored with it. The editor holds no video frames.
  - Opening reads the chunk index, the masters and the slides near the last one viewed;
    media chunks are read when a slide needs them. A 3 GB deck opens like a 3 MB one.
- **Presenting has a hard real-time edge.** The next slide must be ready before the click,
  and a transition must not stutter. So in presenter mode:
  - the current, next and previous slides (and the next one's builds) are laid out, their
    pictures decoded at the display's size and their scenes resident before they are needed;
    jumping to any slide from the presenter's sorter shows it at once from a lower-resolution
    decode and sharpens it within a frame or two;
  - transitions are declarative animations run by frescod (pergola.md §4.2) between two
    already-resident scene groups, so a busy app (decoding the slide after next) cannot drop
    their frames;
  - the presenter view's notes and timer run on the other monitor's window, off the audience
    window's path;
  - a "rehearse" pass can decode every slide once and report slides that won't be ready in
    time (a 200 MP photo on a slow machine) before the talk, not during it.

**The slide sorter and the slide strip** are virtualized lists (`VirtualList` / `GridView`)
of thumbnails rendered from each slide's scene at small scale and cached as small images in
the native file, so scrolling a 1,000-slide sorter never lays out 1,000 slides.

**Size on disk and when sharing.** "Reduce file size" recompresses pictures down to the
largest size they are shown at (a deck for a projector rarely needs more than 1920 px), and
trims video to the used range. It runs on a copy, reports the saving and is never automatic.
Export to `.pptx` and PDF writes each media chunk once.

**Gates** (added to §15): open a 1,000-slide, 2 GB deck with 1,500 photos to the first slide
under 1 s, memory under 500 MB; scroll the sorter at the ScrollView gates; in presenter mode,
the next slide on screen within one frame of the click, transitions with no missed frame at
60 Hz while the following slides decode.

## 10. Pergola and Fresco work this needs

These land in Pergola or Fresco, not in Officina, so every app benefits. Each is its own
piece of work with the owning thread.

### 10.1 `RichTextEditor` (Pergola)

The rich-text editor pergola.md §10.2 says will come later. One widget with two layouts: a
flowing one (mail compose, notes, comments, chat) and a paged one that `atrium-write`
drives. It needs:

- caret, selection (including across table cells and in bidi text), keyboard navigation by
  grapheme, word, line and paragraph;
- **IME** (composition and candidate windows). Atrium has no input method framework yet
  (atrium-input.md covers keys, not composition); this is a dependency for CJK and many
  Indic scripts and should be specified with Tactus (§17 D10);
- **bidi and RTL** layout (pergola.md §12 lists RTL as not yet);
- hyphenation for other languages (pergola.md §12: en-US only today);
- spell-check underlines from a checker the app supplies;
- accessibility: text, caret and selection exposed to atrium-ax.

### 10.2 `GridView` and 2D scrolling (Pergola, Fresco)

- `ScrollView` gains an x axis (`Physics` is vertical only today, pergola.md §10.5).
- frescod's `SCROLL` shift in both axes at once (the shift rule is per axis in
  fresco-scene-groups.md §4; a diagonal move needs a check in both renderers).
- `GridView`: 2D band contract (`want { rows, cols }` → tile ready / updated / evicted),
  frozen panes, headers, column and row resize, selection overlay, in-cell editor, keyboard
  model of a spreadsheet. The same widget serves Scrinium's details view, logs and any
  large table.

### 10.3 Smaller widgets

Ruler with tab stops and indents; colour picker (with document theme colours); font picker
(showing system and document fonts, substitutes marked); number-format editor; formula bar
with syntax highlighting, function completion and argument hints; sheet tab strip;
inspector side panel; split views; zoom control; page thumbnails; context menus on canvas
and grid; drag sources (Pergola accepts drops but does not start drags yet).

### 10.4 Fresco

- A shadow or blur op for shapes and slide effects (Canvas notes "no Fresco op" for box
  shadows). Until then shadows are pre-rendered fills.
- Dashed and dotted strokes are already outlines from the stroker; cell borders at hairline
  width need crisp 1-device-pixel lines at fractional scales (resolution-independence.md §5).
- Images on `Canvas` (Canvas has no image item yet).

### 10.5 A huge document must not take down compositing (asked by Shankar, 2026-11-14)

*"A big document: could it overwhelm the scene graph for Fresco? If it does, it brings down
all the compositing of the entire screen."* Three layers stop it, two of which exist today;
the third is work for the Fresco owners.

**1. Officina never sends a big scene.** Only resident pages, slides or grid tiles (about
three screens' worth, §7.4, §8.5, §9.3) are scene groups; everything else is evicted. The
scene's size follows the window, not the document: `pergola-document` keeps about 800 nodes
resident for a 67,000 px document wherever it is scrolled (pergola.md §10.2), and a full
1440p sheet is about 1,500 cells. A 5,000-page document and a 5-page one send the same
amount. This is a design rule with a gate: §15 adds "resident nodes and scene bytes per
window stay under a fixed ceiling at any document size".

**2. frescod doesn't parse or raster an app's scene (built).** In split mode, the default
(fresco-f4-split.md F4b, F4g), every client connection gets its own jailed `fresco-renderer`
(`_fresco-render`, rctl 512 MiB, 100 % CPU). The renderer parses that app's scene and, with
the compute renderer (the default), rasters it; frescod receives only finished pixels for
the window, checks the header and copies them once. So:

- a scene too big or too slow costs only that app's renderer: its frame is late, and frescod
  keeps composing the window's last good frame (the broker never blocks on a renderer,
  F3, fuzzed);
- a renderer that passes its memory cap is killed with its jail; that app's window goes, the
  rest of the screen doesn't notice;
- a frame past `fresco_compute::MAX_OPS` (65,536 ops after culling) has its excess nodes
  dropped whole and reported to the app (`fill_drop::OP_BUDGET`), never drawn by frescod at
  more cost.

frescod's per-window compose cost is proportional to the window's pixels, not to its scene.

**3. Gaps to close (Fresco owners; the cost side of scene-graph hardening H6 and H8).**

- **Refuse, don't kill.** Today an app that grows its scene without bound ends when its
  renderer hits 512 MiB, which looks to the user like a crash. Wanted: a per-client budget
  in the renderer (nodes, scene bytes, texture and glyph-atlas bytes) that refuses the
  message over it and tells the app, as `OP_BUDGET` does for ops, so the app can evict.
- **GPU renderer time.** With the GPU renderer (F4e), each window's raster runs in its own
  VM but on the GPU's shared queues, before compose. A window whose frame takes very long
  to raster (within `MAX_OPS`, but thousands of large overlapping translucent fills) can
  delay compose for everyone. Wanted: compose never waits on a window's raster past its
  deadline (use the last finished surface), and a per-window raster-time budget measured on
  the R9700, with H6's cost estimate refusing frames far over it.
- **No unsplit fallback in production.** `FRESCOD_SPLIT=0` and the Vulkan renderer put every
  client's scene back in one process. They are development modes; the gate should check a
  shipped configuration refuses them.
- **A test.** A hostile client that sends a million nodes, and one that sends 65,536
  overlapping full-window translucent fills every frame, while a second app animates: the
  second app's frames must stay on time (the ScrollView gates), on compute and GPU renderers.

## 11. Secrets, external data and signatures

### 11.1 Password-protected documents

OOXML (Agile encryption) and ODF encryption are decrypted in the converter jail. The
password is typed into an Arca secure-input slot (the `credential` slot kind), not into the
app's own text field, and passed to the converter; "remember for this file" stores it in
Arca with the document's bookmark as its recipient. Encrypting the native format: a random
per-document key, wrapped by an Arca key that only this user and app identity can unwrap
(`arca_decrypt`), or by a password for sharing outside Atrium.

### 11.2 External data

Web queries, `WEBSERVICE`, stock data, linked workbooks and remote images are **off by
default per document**. The first use shows an in-window bar naming the hosts; allowing it
gives the document process a Nuntius instance pinned to those hosts for this document only.
Credentials for a data source (a database, a REST API) go through Arca's courier (arca.md
§9), so the document process never holds the password or token.

### 11.3 Signing

Documents and PDFs signed with the user's keys through Sigillum: a FIDO / PIV / OpenPGP
token or a USB DSC token (sigillum.md §16.5), PAdES for
PDFs, XAdES for ODF and OOXML signatures. The app sends the hash; the token signs; the
PIN is typed into Sigillum's own sheet. Verification of signatures on opened documents runs
in the converter jail and shows the signer and whether the document changed since.

## 12. Clipboard, drag and drop, sharing

- **Formats on Tabula** (tabula.md §3): plain text always; an HTML subset for rich text to
  other apps; TSV for ranges; Officina's own op-encoded format between Officina apps
  (keeps styles, formulas and charts).
- **Pasting from other apps** is untrusted input: HTML goes through `navigator-normalize`
  in a jail and comes back as profile content that becomes rich text, images are decoded in
  a jail, and a pasted range is size-checked before it is applied.
- **Share** (Limen `share-target`): a document, a PDF export or a chart image.
- **Showing an untrusted document read-only** (a mail attachment preview) is not Officina:
  it is a converter run plus the Limen `doc-viewer`, with no editor started. A "Preview"
  converter output in the document profile makes that cheap (§17 D11).

## 13. Fonts, languages and accessibility

- **Fonts.** The system set (the Navigator's pinned fonts) plus metric-compatible
  substitutes for the common Office fonts, under permissive or OFL licences: Carlito
  (Calibri), Caladea (Cambria), Liberation Sans / Serif / Mono (Arial, Times New Roman,
  Courier New). Substitutions keep line breaks and page counts close; each is listed in the
  fidelity report. Users can install more fonts (Opifex font packages).
- **Languages.** Shaping is already rustybuzz; needed are IME (§10.1), bidi, vertical text
  later, hyphenation and spell-check dictionaries per language (Hunspell-format
  dictionaries; check each dictionary's licence against LICENSING-POLICY.md, since many are
  LGPL/MPL), and locale-aware number and date formats (CLDR data).
- **Accessibility.** The semantic outline idea from pergola.md §10.4 applies directly:
  headings, tables, lists, comments and slide titles exposed whether or not their band is
  resident; a sheet exposes the active cell, its address, value, formula and headers.

## 14. Printing and PDF

Atrium has no printing system yet. PDF export (`officina-pdf`) comes first and covers
"print to file". A print service (a spooler that takes PDF from apps and drives IPP
Everywhere printers through a jailed driver, no CUPS) is its own future spec; Officina only
needs it to accept PDF (§17 D12).

## 15. Gates and targets

Measured on the dev VM for comparison between builds, and on real hardware for numbers:

| what | target |
|---|---|
| start to an empty document on screen | under 300 ms (warm) |
| memory, empty document process | under 40 MB |
| open a 1,048,576 × 20 numeric `.xlsx` | under 5 s to first screen, scroll usable before the load finishes |
| open a 100 M-cell native workbook | first screen under 1 s (lazy chunks) |
| scroll anywhere in a sheet or a 500-page document | the ScrollView gates: 0 stalls over 100 ms, p99.9 ≤ 2 periods, ≤ 1 % missed, 60 Hz |
| edit a cell with 1 M dependents in one formula group | visible cells updated in one frame; the rest under 1 s on 8 cores |
| keystroke in a 500-page document | on screen within one frame |
| open a 1,500-page native document with 3,000 images | reading position on screen under 1 s; memory under 500 MB whatever the image count |
| open a 1,500-page `.docx` for the first time | first page under 3 s; full pagination in the background under 30 s |
| insert a figure near the start of a 1,500-page document | visible pages updated in one frame; all later numbers, cross-references and the TOC settled under 10 s |
| find in a 1,500-page document | first result under 100 ms; all results under 1 s once the index is built |
| PDF export of 1,500 pages with images | under 60 s on 8 cores, memory flat during export |
| open a 1,000-slide, 2 GB deck with 1,500 photos | first slide under 1 s; memory under 500 MB |
| presenting: next slide after the click | on screen within one frame; transitions with no missed frame at 60 Hz |
| scene size per window | resident nodes and scene bytes under a fixed ceiling (set in O1, about 5,000 nodes) at any document size; checked on 10-, 1,500- and 5,000-page documents |
| formula results vs Excel | 100 % of a differential corpus of real workbooks, with reasons for each listed exception |
| round-trip `.docx` / `.xlsx` / `.odt` / `.ods` | the file reopens in Word, Excel and LibreOffice without repair prompts; visual diff of rendered pages within a set threshold |
| converters | fuzzed (cargo-fuzz, a corpus of real and malformed files) with no panics outside the jail's handled exit, no memory growth past caps |

Corpora: public test suites from the format specs (the ODF and OOXML conformance sets),
LibreOffice's and Apache POI's bug-report file collections where their licences allow
use as test data, and a set Shankar provides of real files he cares about.

## 16. Build order

| phase | work | gate |
|---|---|---|
| **O0** | this spec reviewed; Pergola, Scrinium and Fresco owners agree to §10 and §4.2 | decisions in §17 made |
| **O1** | `officina-core` (model, op log, undo, native format v0), `officina-grid` storage + formula parser + evaluator for the first tier of functions + dependency graph; headless | formula differential corpus, first tier; 100 M-cell synthetic workbook within memory target; fuzzed parser |
| **O2** | `GridView` + 2D ScrollView + frescod two-axis shift; `atrium-sheets` with CSV open/save, picker open and Save As (§4.2), autosave journal | scroll gates on a 1 M-row sheet on frescod; crash-recovery test |
| **O3** | `RichTextEditor` (flowing mode first, then paged), `officina-text` paged layout, `atrium-write` with `.odt`/`.docx` converters, PDF export | round-trip gate for text; keystroke-latency gate on a 500-page document; the large-document gates of §15 on a generated 1,500-page document and ECMA-376 Part 1 |
| **O4** | `.xlsx`/`.ods` converters, styles, number formats, charts, sort/filter, structured tables, macros kept inert | round-trip gate for sheets; 1,048,576-row `.xlsx` open target |
| **O5** | tracked changes, comments, footnotes, TOC; pivot tables, conditional formats, dynamic arrays, `LAMBDA`; recent files via bookmarks (needs Scrinium home folders) | the corresponding corpus files |
| **O6** | `atrium-slides`: masters, layouts, presenter view, transitions, `.pptx`/`.odp` | round-trip gate for slides; presenter view on two monitors; the large-deck gates of §9.3 |
| **O7** | signing via Sigillum, encrypted documents, external data via Nuntius, IME-dependent scripts, legacy binary import if D8 says yes | per feature |

The spreadsheet goes first: it carries the hardest engine work (scale, recalc), it needs
only the simplest text (one-line cells), and the grid widget it forces is useful to other
apps at once.

### 16.1 O1 as built (2026-11-14)

Two crates, no dependencies outside the repository, all Rust:

- **`officina-core`**: the container (§5.1: header, chunks with CRC-32, index, trailer; a
  reader that bounds every count and offset), the append-only journal (length + CRC per
  record, replay stops at a torn tail), undo history over ops that return their own
  inverse, and the bounded wire encoding.
- **`officina-grid`**: column-chunked storage (4,096-row chunks; sparse, dense numeric and
  mixed encodings), string pool, row/column sizes by prefix sums; formula parser (Excel
  en-US syntax, bounded length, depth and arity) and printer; evaluator with 90 first-tier
  functions and Excel's coercion, comparison, 15-digit and 1900-date rules; formula groups
  (one AST per fill run) with range footprints in an interval index; recalculation that
  propagates dirty row runs, orders groups by strongly connected components, runs
  independent groups in parallel through a vectorised path, runs running totals in order,
  and evaluates mutually dependent cells by their cell-graph components (cells on a cycle
  are `#CIRC!`, judged by references as Excel does, and the result is independent of
  visiting order); undoable grid ops (every edit is "replace a rectangle with a snapshot",
  formulas kept as fills, so undoing an edit in a million-row fill costs one fill); the
  native file v0 (WORKBOOK, STRINGS, COLUMN and FORMULAS chunks, stored results, no
  recalculation on load except volatile formulas).

Gates, on the 4-core cloud machine:

| gate | result |
|---|---|
| formula conformance, first tier | 152 hand-checked cases with Excel's results (`tests/conformance.rs`), all pass; print/parse round trip on all of them. The automatic differential against Excel-saved results comes with the `.xlsx` converter (O4), since no Excel runs here |
| incremental = full | 1,000 random workbooks (formulas, fills, cycles, errors, 1–4 threads): incremental recalculation equals recalculation from scratch after every edit batch |
| 100 M cells | 10 columns × 10 M rows: 776 MB of storage (8.14 bytes a cell), 781 MB resident, loaded in 1.9 s (`examples/scale.rs`) |
| recalculation | a 10 M-row fill plus `SUM` and `AVERAGE` over whole columns: 0.4 s full; one edit under that `SUM`: 26 ms; one edit with 1 M dependents: 57 ms; `MATCH` over 10 M rows: 1.3 s with the index build, then 0.01 ms |
| native file | that workbook: 861 MB file, saved in 3.4 s and loaded in 4–5 s from memory |
| fuzzing | 2 M parser inputs (random bytes and token soup, all that parsed then evaluated, saved and reloaded), mutated native files and op records: no panic, no hang. It found two parser bugs on its first run (a slice inside a multi-byte character, an endless loop on a non-name character), both fixed |

Not in O1, by plan: visible-first recalculation and cancellation (they need the O2 view),
inserting and deleting rows with reference adjustment, styles and number formats (O4),
dynamic arrays and spills (O5), the space (intersection) operator, lazy loading of
column chunks. The memory target is met for numbers; text-heavy sheets cost more (16
bytes a cell in a mixed chunk plus the pool) and get their own measurement in O4.

### 16.2 O2 as built (2026-11-14)

- **Pergola, two-axis scrolling.** `ScrollView` scrolls across as well as down when its
  content is wider than the view: wheel `dx`, Shift+wheel, touchpad gestures railed to one
  axis unless clearly diagonal, Left/Right. `Node::Scroll` carries `offset_x` and
  `content_w`; hit testing, slots and the surface's content transform use both.
  `ScrollState.extent` (f64) lets a widget state a size an `f32` can't hold exactly, and
  `fixed_zoom` keeps a grid's scale.
- **Pergola `GridView`** (`pergola/src/widgets/grid_view.rs`). A `GridSource` trait
  (sizes by prefix sums, cells near the view only) shown as three scroll groups: the body
  (tiles of 16 rows × 4 columns, keyed, so a scroll frame changes only the three
  viewports' offsets until a tile enters), a column header that moves across only and a
  row header that moves down only. Content coordinates rebase every 8,192 px, so a sheet
  of 2^31 rows (43 billion px) stays exact in `f32` and every viewport's translation stays
  inside frescod's ±32,768 device-px bound. Selection by drag, by header and by
  corner; column resize by its header edge; the usual spreadsheet keys (arrows, Shift, Ctrl edges,
  Page, Home/End, Tab, Enter, Escape, Delete, Ctrl+Z/Y/C/X/V/A); an in-cell editor on
  `TextEdit`; numbers that don't fit show `###`, text is cut with an ellipsis.
- **frescod.** Its shift path already took any 2D move (`pack::shifts`,
  `fresco_compute::damage`); O2 adds tests that diagonal and sideways scrolls shift and
  equal full redraws (`pack.rs`), and the RDNA4 oracle cases `fresco_shift_diagonal` and
  `fresco_shift_across` (CPU reference passes; the gpusim run is on the Mac).
- **CSV** (`officina-grid/src/csv.rs`): separator detection (`,` `;` tab `|`), UTF-8 with
  or without BOM, Windows-1252 fallback, RFC 4180 quoting; values typed as a user would
  type them, formulas kept as text unless asked; writes General format at 15 digits with
  CRLF and the separator it was read with.
- **`atrium-sheets`**: the app. Toolbar (Open, Save, Save As, Undo, Redo), a formula bar
  (address and the active cell's formula), the grid, a status line. Files come from the
  system picker (scrinium.md §4.1): Open asks for one file read-write and Save writes
  back to that descriptor. **Save As is interim:** until Scrinium's Save As sheet (§4.2)
  exists it asks the picker for a folder with write access and writes `NAME.osheet`
  there (temp file, hard link, so nothing is ever replaced; `NAME 2.osheet` and so on
  when taken). Saving to CSV keeps values only and says so.
- **Autosave and crash recovery** (`atrium-sheets/src/doc.rs`). Each document journals
  every action (`officina-core` records: a run of ops, undo, redo, a column width, a
  "saved" mark) to a private folder under `~/.local/state/atrium-sheets/autosave/`,
  synced every 2 s. A base is written only when the document starts autosaving; a save
  appends a mark rather than a new base, so the journal rebuilds the undo history and an
  undo past a save, or after a recovery, replays exactly as it ran. Generations switch by
  one rename, so a new base never meets an old journal. The folder is held with `flock`,
  so a second window never takes a live document for a crashed one. At start the app
  restores a document left by a crash (or by closing with unsaved changes, as there is
  no "save changes?" prompt yet) and goes on journaling where it stopped, after cutting
  off a torn tail.

Gates, on the 4-core cloud machine:

| gate | result |
|---|---|
| crash recovery | a child process types as fast as it can (a formula every tenth action) and is killed with SIGKILL at five different moments: each time exactly the first N actions come back in order, N at least what was last durable, torn tails cut (`tests/crash.rs`). Unit tests cover every cut point of the last record, undo across a save and a second crash after a recovery |
| scroll, headless | atrium-sheets' own view on a 1,048,576-row × 10-column sheet (`examples/scroll_cost.rs`: the app's real work per vsync on a virtual clock; swipes at 4,000 px/s, diagonal swipes, wheel, Page Down, Ctrl+End/Home): at 60 and 120 Hz every pattern passes, frame work p99.9 1.6–4.6 ms, max 5.9 ms, 0 missed vsyncs, 0 stalls; at most 1,945 nodes resident |
| scroll, live on frescod | functional, in the cloud TCG VM (`scripts/sheets-vm.sh`): the app draws on a headless frescod, and the bench drives every pattern with no protocol error. The first run found a real bug: the grid's viewports were translated up to 1 M px, past frescod's ±32,768 device-px bound on a group transform, so every scroll past about 1,600 rows was refused (`BadPayload`). The grid now rebases every 8,192 px, and `scroll_cost` checks the bound every frame. TCG frame times (60–1,400 ms) are not product numbers; the timing run is on the Mac's HVF VM, pending |
| scroll, live on the Mac (HVF VM, 4 vCPUs, compute renderer, 1,048,576 rows, 60 Hz) | 2026-11-14 at 060941d + b1372312, split on: page and jump PASS; swipe, diagonal and wheel FAIL on missed vsyncs only (1.3 %, 9.8 %, 7.0 %), 0 stalls in every pattern, swipe p99 15.7 ms (was 132 ms). Two fixes got there: the app measured every new number and header label with a round trip to frescod (now cached per character, build p99 43 → 6 ms), and frescod's rasteriser took every CPU so the app was preempted mid-send for ~100 ms (send CPU 0.1–0.4 ms against 85–157 ms wall; now it leaves one CPU, send p99 127 → 11 ms). Left: one 37 ms build in the wheel run and vsyncs missed by the bench loop's wake-ups in the VM |

Not in O2: a "save changes?" prompt, the jailed one-process-per-document launch (§3; the
app runs as a plain Limen client for now), multiple sheets in the UI, row insert and
delete, fill handle, find, number formats (O4), visible-first recalculation.

### 16.3 O4a as built (2026-11-14): `.xlsx` in and out

O4 is built in steps: **O4a** `.xlsx` values, formulas and sizes (this section); O4b
styles and number formats; O4c `.ods`; O4d sort, filter, tables and defined names; O4e
charts.

- **`officina-convert`** (new crate; deps `miniz_oxide` and `quick-xml`, both permissive,
  pure Rust):
  - a ZIP reader for hostile archives (bounded entries and names, every offset checked,
    an entry inflates to at most its declared size and 4 GiB, the archive to 16 GiB,
    a declared ratio over 1000:1 refused, CRC checked, ZIP64 read, encryption refused)
    and a writer (ZIP64 when needed);
  - the `.xlsx` reader: sheets, shared and inline strings (rich runs flattened,
    `_xHHHH_` escapes), numbers, booleans, errors, formulas (plain, shared fills as one
    formula group, array formulas in their first cell, `_xlfn.` prefixes dropped),
    column widths and row heights, the 1904 flag. A sheet part is inflated on one
    thread, cut at row ends into 8 MB pieces and parsed on every core, then merged in
    order;
  - the `.xlsx` writer: one part per sheet, shared strings, a fill as one shared
    formula with its cached results, `_xlfn.` for newer functions, sizes, and Excel's
    view of our extra errors (a cycle writes 0, as Excel shows it; over a limit
    `#NUM!`). A sheet past 1,048,576 × 16,384 is refused by name;
  - the fidelity report (§5.2): notes counted, with examples: approximate, inert,
    dropped. On open, every formula's result is recalculated and compared with the
    file's cached result; differences go in the report (a differential against
    whatever wrote the file);
  - the **converter process** `officina-convert xlsx`: the file on stdin, the native
    file on stdout, notes on stderr. It reads its input, caps CPU (60 s) and address
    space (8 GiB), and on FreeBSD enters Capsicum capability mode before parsing a byte
    (checked in the VM with `truss`: `cap_enter() = 0`, then its worker threads). The
    throwaway jail (portcullis one-shot) is the next wall to add around it.
- **`atrium-sheets`** opens `.xlsx` and `.xlsm` through the converter process (the
  parser never runs in the document's process) and saves `.xlsx` with the writer, in
  process, as §5.2 says. The status line shows the report's first note; all go to the
  document's `notes`.

Gates, on the 4-core cloud machine:

| gate | result |
|---|---|
| round trip | values, text with markup characters and escapes, booleans, errors, a 500-row fill (back as one group), formulas over other sheets and newer functions, sizes: everything equal after write and read; the reader's differential finds 0 differences (`tests/roundtrip.rs`) |
| LibreOffice, both ways | `examples/lo_check.rs`: our file of the 152 conformance formulas opens in LibreOffice 24 (headless) and is saved again by it; our reader opens LibreOffice's file, our engine recalculates, and every result is still Excel's. LibreOffice's own results agree with ours on 145; the 7 others are LibreOffice departing from Excel (`"z"<TRUE`, `TRUE=1`, `SQRT(-1)` and `LN(0)` as `#VALUE!`, `CONCATENATE` with `TRUE`, `VLOOKUP` past the range, no `XLOOKUP`). No Excel runs here; a corpus of Excel-saved files is the next differential |
| hostile input | 3,000 mutated packages, every truncation, and 1,500 packages with mutated XML in every part (byte flips, inserted tokens, cuts): errors, no panic, no hang |
| 1,048,576 × 20 `.xlsx` | 177 MB file, 730 MB of sheet XML: read in 3.7 s on 4 cores (13.5 s on one, before the pieces); native hand-off 0.6 s out and 0.6 s in. So about 5 s to the document on screen, the target's edge; the screen does not yet show the top rows while the rest loads. Writing it takes 12 s (one deflate stream; parallel deflate is the fix) |

### 16.4 O4b as built (2026-11-14): styles and number formats

- **Number formats** (`officina-grid::numfmt`): Excel's format codes as Excel shows them
  in en-US: up to four sections with `[Color]` and `[condition]`, digit placeholders
  (`0 # ?`), thousands separators and scaling commas, percent, scientific and
  engineering (`##0.0E+0`), fractions (`# ?/?`, `?/8`), dates and times (`d dd ddd dddd`,
  `m`…`mmmmm`, `yy yyyy`, `h hh`, minutes by context, seconds rounded as Excel rounds,
  `.0`–`.000`, `AM/PM`, `A/P`, elapsed `[h] [m] [s]`), text sections (`@`), literals,
  `\x`, `_x` padding, `*x` fill (shown once), `[$€-407]` currency, and the built-in ids
  0–49. A code never fails to parse; unknown parts show as literals and the fidelity
  report says so. `TEXT(value, format)` uses it.
- **Styles** (`officina-grid::style`): a style is a number format, font (name, size,
  bold, italic, underline, strike, colour), solid fill, horizontal and vertical
  alignment, wrap, indent, rotation and four borders. Styles are interned per workbook;
  each column keeps runs of style ids, so a formatted column is one run at any height.
  Restyling is an undoable op carrying styles by value (`GridOp::Restyle`); the native
  file has `STYLES` and `STYLE_RUNS` chunks.
- **`.xlsx` styles**: read from `styles.xml` (custom and built-in number formats,
  fonts, solid, patterned and gradient fills, borders, alignment, the custom indexed
  palette, theme colours with Excel's tint) and written back (one `cellXfs` entry per
  style; a style reaching the last row becomes the column's style). Reported: patterned
  and gradient fills shown solid, diagonal borders, rotated and shrink-to-fit text,
  unlocked or hidden cells, row formats on empty cells, locale-specific built-in formats.
- **Sheets** shows values through their format (with the format's colour, `#` for a date
  out of range), bold, font colour, fills over the grid lines, borders by width, and
  alignment. Not yet shown: italic, underline, strike, font names and sizes, wrap and
  vertical alignment; no style editing UI yet (the inspector comes with the toolbar
  work).

Gates, on the 4-core cloud machine:

| gate | result |
|---|---|
| number formats vs LibreOffice | `examples/numfmt_lo.rs`: 1,000 value × format pairs through `TEXT()` in LibreOffice 24: 801 agree; every other case is a known LibreOffice departure from Excel where ours is Excel's (dates before 1900-03-01, negative and past-9999 dates, seconds truncated rather than rounded, `#.##` without its trailing point, General's `1E-10`) |
| styles vs LibreOffice | `examples/styles_lo.rs`: 32 cells with distinct styles (15 number formats, fonts, fills, 7 border lines, alignments) and a whole styled column go out with our writer, through LibreOffice's (which rewrites the styles part with its own fonts, theme and codes) and back through our reader: all 32 equal and showing the same text, the column kept, no report notes |
| round trip | `tests/roundtrip.rs` `styles_round_trip`: whole columns, part columns past the data, unstyled cells inside a styled column, single cells: equal after write and read; a whole formatted column stays three runs |
| 1,048,576 × 20 `.xlsx` read | 4.1 s with style runs collected (3.7 s before) |

### 16.5 O4c as built (2026-11-14): `.ods` in and out

- **Reader** (`officina-convert::ods::read`, in the converter process; `officina-convert
  ods`): sheets; floats, percentages, currencies, dates, times (ISO durations past a
  day), booleans, text with its spaces, tabs and line breaks; repeated rows and cells
  (expansion refused past 200 M cells before it happens); formulas from OpenFormula
  (`of:=SUM([.A1:.A3])`, `;` arguments, `|` array rows, quoted sheet names, `TRUE()`,
  error literals, `COM.MICROSOFT.*` names) recalculated with the same differential as
  `.xlsx`; error results (`calcext:value-type="error"`); column widths, row heights;
  cell styles with parents and the document default: ODF data styles become Excel codes
  (number, scientific, fraction, percentage with its scaling `%`, date and time parts,
  elapsed `[h]` from `truncate-on-overflow`, colours, `style:map` sections, LibreOffice's
  blank-width `_x`), fonts, fills, borders by width and kind, alignment, wrap; a column's
  default cell style is its column style. Reported: merged cells, comments, charts and
  images, conditional formats, validation, named ranges, hidden rows and columns,
  rotation, other formula namespaces (kept as values).
- **Writer** (`officina-convert::ods::write`, in the document process): ODF 1.3 with
  `mimetype` first and stored; values typed by their cell's format (date, time,
  percentage, float), booleans with a boolean data style (LibreOffice otherwise shows 1),
  formulas as OpenFormula with cached results, constant errors as error formulas, the
  same style model as data styles, cell styles and column defaults (rows run to the
  last row and through the last styled column, as LibreOffice applies column defaults
  only to cells it has). Sheets opens and saves `.ods`.

Gates, on the 4-core cloud machine:

| gate | result |
|---|---|
| `.ods` vs LibreOffice, in | `examples/ods_lo.rs`: the 152 conformance formulas, the fixture and 37 styled cells written as `.xlsx`, saved by LibreOffice 24 as `.ods`, read by ours: 152/152 show Excel's result, 37/37 styled cells show the same text with the same style, whole-column style kept; no report notes besides LibreOffice's own 7 results that differ from Excel |
| `.ods` vs LibreOffice, out | same workbook through our `.ods` writer, LibreOffice saves `.xlsx`, our reader: 152/152 Excel's results, 36/37 styled cells equal (LibreOffice turns on wrap for the cell with a line break), column style kept. LibreOffice writes our `TRUE` back as `TRUE()` and stores logical results as 1 with a boolean format |
| round trip | `tests/ods.rs`: values, formulas (fills, other sheets, arrays, newer functions), awkward text, sizes and styles equal after our write and read |
| hostile input | 1,500 mutated packages (XML tokens, repeats of 4 G rows, bad dates and durations) plus truncations: no panic; a repeated-value bomb refused at once |

### 16.6 O4d as built (2026-11-14): names, sort, filter, tables

Engine and files; the Sheets UI for them (sort and filter menus, table creation, the
filter buttons) is the next step.

- **Defined names** (`officina-grid::names`): workbook and sheet scope (a sheet name
  hides a workbook one), stored parsed as if in A1 so their relative parts follow the
  using cell, as in Excel's files; footprints and evaluation follow names up to 16 deep,
  a cycle of names is `#NAME?`; changing a name re-binds the formulas that use names.
  `.xlsx` `definedNames` (`localSheetId`), `.ods` named ranges and expressions with
  their base cell, native chunk 7.
- **Sort** (`officina-grid::sort`): Excel's order (numbers, text ignoring case, logicals,
  errors; empties last both ways), stable, by several keys; one undoable edit moving
  whole rows of the range (values, formulas copied with their relative references, as
  Excel does, and styles); a sorted fill stays one fill.
- **Filter** (`officina-grid::filter`): one auto-filter per sheet (value lists matched
  on shown text ignoring case; one or two numeric tests joined by and/or) and hidden
  row ranges, swapped as one undoable `GridOp::Filter` (journal kind 4, native chunk 8);
  hidden rows have height 0 in the view. `.xlsx`: `row hidden`, `autoFilter` with
  `filters`/`customFilters`, the hidden `_xlnm._FilterDatabase` name Excel expects.
  `.ods`: row `visibility` (`filter` / `collapse`) and anonymous database ranges with
  their conditions (LibreOffice leaves numeric order tests untyped and repeats the first
  set item; both handled). Text, wildcard and top-N filters are reported and their rows
  stay as the file hid them.
- **Structured tables** (`officina-grid::tables`): named range, header and totals rows,
  named columns, style name kept; tables and names share one namespace; tables don't
  overlap. Formulas keep structured references as written (`Sales[Amount]`,
  `[@Qty]`, `Sales[[#Headers],[A]:[C]]`, `#All/#Data/#Headers/#Totals/#This Row`, `'`
  escapes, a table's name alone for its data) and resolve them when evaluated, with the
  range's footprints (`@` is the formula's own row; outside the data rows `#VALUE!`).
  `.xlsx`: `xl/tables/tableN.xml` both ways, formulas in the file form
  (`Sales[[#This Row],[Qty]]`), shown as Excel shows them inside their table
  (`[@Qty]`). `.ods` has no structured references: formulas are written with the ranges
  they reach, the table as a named database range (header and data rows) read back as a
  table named by its header cells. Native chunk 9. Table styles are kept but not drawn
  yet (reported); filters inside tables keep their rows as the file hid them.

Gates, on the 4-core cloud machine:

| gate | result |
|---|---|
| LibreOffice both ways (`examples/ods_lo.rs`) | 4 names, a two-column auto-filter with hand-hidden rows and a table with a calculated column, used from another sheet: all kept in and out, values equal |
| round trips | `tests/roundtrip.rs` and `tests/ods.rs`: names, filter state and hidden rows, tables and their formulas equal after write and read; native round trip in the grid tests |
| hostile input | structured-reference parser: unbalanced brackets, bad keywords, three-part ranges refused; print/parse round trip; existing formula and package fuzzers clean |

### 16.7 O4e as built (2026-11-14): charts

Engine, files and drawing; the Sheets UI for making and editing charts is the next step.

- **Model** (`officina-grid::charts`): column, bar, line, area, scatter, pie and
  doughnut; standard, stacked and 100 % grouping; per-series kind for combos (columns
  with a line); title, series names (a cell or typed text), axis titles, legend
  position; series ranges are one row or one column. Anchored to a sheet from a cell
  plus an offset to a cell plus an offset (offsets in points), so a chart moves with
  its cells as in Excel. `Chart::data` reads at most a given number of points per
  series; `Chart::data_version` hashes the generations of the columns a chart reads,
  so a view redraws a chart only when its data changed. At most 4,096 charts and 255
  series per chart. Native chunk 10.
- **Drawing** (`officina-draw::chart`): a chart laid out in a box as rectangles,
  polylines, areas, wedges, dots and text, which the UI turns into its own nodes; text
  is measured by the caller. Bounded at any size: a line of a million points becomes at
  most two points per pixel column (minimum and maximum, in order), a scatter at most
  one dot per pixel, bars at most one per pixel column. Nice axis steps, a legend,
  colours from Excel's default palette (sRGB, turned linear for Pergola), a dark style.
- **Sheets**: Pergola's `GridView` gained floats (`GridSource::floats`): nodes placed
  in sheet coordinates, drawn above the cells and moving with the scroll. Sheets lays
  out each chart that touches the view and keeps the nodes until its size, data or theme
  changes.
- **`.xlsx`**: drawing parts (`twoCellAnchor`) and chart parts (`c:barChart`,
  `lineChart`, `areaChart`, `scatterChart`, `pieChart`, `doughnutChart`, several groups
  for combos) both ways, with Excel's automatic title for a one-series chart. Charts
  with typed-in data, 3-D charts, other kinds and chart styling are reported and
  dropped.
- **`.ods`**: frames anchored to the sheet (or, as LibreOffice also writes them, to a
  cell) holding an embedded chart object (`Object N/content.xml`), with the plot-area
  properties for horizontal bars, stacked and percent. ODF has no typed-in series names:
  LibreOffice's `loext:label-string` is read but not written, because LibreOffice 24.2
  drops the whole chart when it meets it, so a typed name comes back unnamed.
  LibreOffice calls a doughnut's series "circle"; a round chart's series take its kind.

Gates, on the 4-core cloud machine:

| gate | result |
|---|---|
| LibreOffice both ways (`examples/charts_lo.rs`, LibreOffice 24.2) | 9 charts (every kind, stacked and percent, scatter, a combo): `.xlsx` 9/9 the same after LibreOffice (anchors move by its rounding to cell plus offset); `.ods` 9/9 the same, anchors exact |
| round trips | `tests/roundtrip.rs`: every kind through `.xlsx` and `.ods` equal (`.ods` anchors within 0.05 pt), and the data they read equal; native round trip in the grid tests |
| drawing | `officina-draw` tests: a million-point line stays within two points per pixel column; `examples/chart_svg.rs` rendered and checked by eye |
| live | Sheets on the cloud VM shows a combo, a pie and an area chart over the grid (screenshot checked) |
| hostile input | the `.xlsx` and package fuzzers now run on a book with a table, a filter and charts; clean |

### 16.8 O4f as built (2026-11-14): macros kept inert

- **Model** (`officina-grid::macros`): a workbook may carry one set of macro parts, each
  a package path, a media type and bytes, never parsed, plus VBA's code names for the
  workbook and sheets (`codeName`, which the project's modules are bound to). The family
  (OOXML or ODF) says which format they go back into. Bounded: 1,024 parts, 32 MiB a
  part, 64 MiB in all; paths plain and relative (no `..`, no absolute, no odd
  characters), code names letters, digits and `_`. Native chunks 11 (family, code names)
  and 12 (one per part). `remove_macros` drops them.
- **`.xlsm`**: the reader keeps the VBA project the workbook links to
  (`xl/vbaProject.bin`) and its signatures (classic, agile, V3) by the project's
  relationships, with the code names. `xlsx::write` never writes macros (Excel refuses a
  `.xlsx` that has them); `xlsx::write_xlsm` writes them back with the macro-enabled
  workbook type, the project relationship, its signature relationships and the code
  names, and only parts directly in `xl/` with a known VBA media type, so a hostile
  native file can't place a part anywhere else. The project is unchanged, so its
  signature still holds.
- **`.ods`**: the reader keeps the `Basic/`, `Dialogs/` and `Scripts/` folders with
  their manifest media types; the writer writes them back with their manifest entries.
  Bindings of macros to events and buttons in `content.xml` are not kept (reported), so
  a saved file has its macros but nothing starts them by itself.
- **Across formats**: macros go back only into their own family: `.xlsm` macros are not
  written to `.ods` or `.xlsx`, Basic is not written to `.xlsm`; Sheets says so when it
  saves.
- **Sheets**: a file with macros shows "has macros; Atrium doesn't run them" by its name
  and a Remove Macros button; removing them is journaled (a crash after it doesn't bring
  them back) and takes effect at the next save. `.xlsm` files save as `.xlsm`.

Gates, on the 4-core cloud machine:

| gate | result |
|---|---|
| LibreOffice (`examples/macros_lo.rs`, LibreOffice 24.2) | our `.ods` with a Basic module → LibreOffice → `.ods`: the module's code is kept. Two real `.xlsm` files from Apache POI's test data: VBA project and code names kept; LibreOffice opening the original and opening our re-written `.xlsm` finds the same 5 VBA modules with the same code |
| round trips | `tests/roundtrip.rs`: `.xlsm` parts and code names equal through write and read and through a native file; `.xlsx` gets none; parts outside `xl/` and the other family's parts are not written; `.ods` Basic and Scripts parts equal |
| hostile input | the `.xlsx` and `.ods` fuzzers now run on packages with macros, with code-name, relationship-target and manifest-path mutations; clean |
| app | `atrium-sheets` test: removing macros survives a crash |

### 16.9 Sheets: the Data menu (2026-11-14)

The O4d/O4e engine features, usable in `atrium-sheets` (`src/data.rs`):

- A **Data** menu in the toolbar (a Pergola `MenuButton`, a pop-up window placed by
  frescod): Sort A to Z / Z to A; Filter (on or off); Show Only "value", Hide "value",
  Clear Filter in Column, Show All Rows; Format as Table / Convert to Range; Insert
  Column, Line or Pie Chart; Delete Chart. Items read the cursor's cell and are
  disabled where they can't apply.
- **What they act on**: the selection when it is more than one cell, else the table or
  filter the cursor is in, else the current region (the block of non-empty cells
  around the cursor bounded by empty rows and columns, found by walking its perimeter,
  never its area). The first row is a header when it is all text over some non-text.
  Sorting keeps the header; it is refused while a filter hides rows. Hide narrows the
  column's value list; a column with over 10,000 values offers Show Only.
- **Tables and charts are undoable**: `GridOp::Tables` and `GridOp::Charts` swap the
  workbook's list in whole (journal kinds 5 and 6), so they undo, redo and survive a
  crash like any edit. A new table is named `TableN`, columns from the header (or
  `Column1`…); a new chart takes the first column as categories when it isn't numbers,
  every other numeric column as a series named by its header, and sits right of the
  data where it covers no cells.
- **On the grid** (`GridSource::floats`): a table's outline, header tint and stripes,
  drawn for the visible part only; a mark on each filter header cell, lit on a
  filtered column.

Not yet: picking several values from a list (the menu works on the cursor's value),
number conditions in the UI, totals rows, chart editing (titles, kinds, ranges),
moving and sizing charts with the pointer. Charts leave out rows a filter (or the user) hides, as Excel's do.

Gates: `atrium-sheets` tests (region and header, sort and undo, show only / hide /
clear, table and chart from the region, undo all of it); the grid's op test (tables and
charts undo, redo and encode, a refused table left out); the cloud VM screenshot of
`examples/data_demo.rs`'s sheet.

### 16.10 O3 plan, and O3a as built (2026-11-14): the text engine

O3 is built in steps, each with its own gate:

| step | what | gate |
|---|---|---|
| O3a | `officina-text`: model, block tree, undoable ops, paragraph layout, pagination | the 1,500-page pagination gate below |
| O3b | Pergola `RichTextEditor`: caret, selection, IME, typing, formatting keys, over `officina-text` | keystroke latency on a 500-page document |
| O3c | `atrium-write`: pages as bands of pergola.md §10.4, native file and autosave journal, styles and paragraph menus | crash test; scene size on 10-, 1,500- and 5,000-page documents |
| O3d | lists and numbering, tables, images (sizes only for layout, §7.4), headers, footers, page numbers, footnotes | numbering and float tests; image-count memory gate |
| O3e | `.docx` and `.odt` in (converter process) and out | round trip; LibreOffice both ways; hostile packages; ECMA-376 Part 1 opened |
| O3f | PDF export, find and replace, word count, spell check | export and find gates of §15 |

**O3a, `officina-text`** (no UI):

- **Model** (`model.rs`). A paragraph is UTF-8 text with attribute runs (byte start,
  interned `CharAttrs` id), a named paragraph style and direct paragraph properties. Every
  attribute and property is optional, so "not set" and "set to the default" stay apart
  for the converters. Styles inherit (Normal, Heading 1–3, Title to start; depth capped
  at 16, cycles cut) and resolve to a `ParaFormat` and `CharFormat`. Page setup is A4
  with 2.54 cm margins. Lengths in files are twips; layout uses integer layout units
  (64 per point), so layout is exact and the same everywhere.
- **Block tree** (`tree.rs`). Blocks in leaves of at most 256, with a Fenwick index over
  the leaves' block and character counts: block *i*, the block holding character *c* and
  the character of block *i* are O(log n) plus one leaf. (Headings, captions and
  estimated heights join the counts with O3d.)
- **Ops** (`ops.rs`). Splice, restyle, split, join, set paragraph, insert and remove
  blocks, each with an exact inverse, so undo, redo and the journal come from
  `officina_core::history`; an op that doesn't fit the document becomes a no-op.
  3,000 random edits undo back to the start byte for byte.
- **Paragraph layout** (`layout.rs`). Shaped with rustybuzz over the pinned fonts (IBM
  Plex, DejaVu fallback per character), broken greedily at UAX #14 opportunities with
  trailing spaces hanging and an emergency break inside a word wider than the column;
  alignment, justification, indents, first-line indent, and line spacing (multiple,
  exact, at least). Layouts are cached per block by a hash of text, runs, resolved
  formats and width, so typing relayouts one paragraph.
- **Pagination** (`paginate.rs`). A page is a list of line slices; its start (block,
  line) is its checkpoint. Space before is dropped at a page top; page break before;
  widow and orphan control (two lines each side); keep together and keep with next
  (chains up to 64, only when what they keep fits on a page); a line taller than the page
  goes alone. An `Edit` (changed block range and block-count delta) drops the pages from
  the one before the edit's first change (walking back over keep-with-next chains),
  repaginates, and as soon as a new page starts at an old page's checkpoint past the edit
  takes every later page as it was, shifted by the delta. `run(max pages)` and
  `run_for(budget)` let the app paginate in the background; a second edit before
  convergence repaginates on to the end instead of composing shifts. Floats, footnotes
  and counters join the checkpoint with O3d.

Gates: 17 unit tests, among them pagination rules on fixed metrics and 300 random edits
(typing, deleting, Enter, joining, keeps, page breaks) whose incremental pages equal a
fresh pagination each time. `examples/gate_1500.rs` (release, cloud container, real
shaping): a generated document of 8,250 blocks and 4.7 M characters paginates to 1,514
pages in 2.1 s (first page 2.3 ms; 30 s allowed); with layouts cached, 13 ms. Typing 40
characters on page 757 relays 1 page per keystroke (worst 0.1 ms); Enter there relays 4
pages; inserting a page of text on page 1 relays 553 pages in 7 ms until a break lines
up; the result equals a fresh pagination.

### 16.11 O3b as built (2026-11-14): `RichTextEditor`

A new crate, `pergola-richtext`, beside `pergola-document`, so Pergola itself does
not depend on Officina's engine.

- **Editing core** (`editor.rs`, no pixels). Caret and selection as (block, byte) on
  grapheme boundaries, across paragraphs. Motions: grapheme, word (Ctrl or Alt), line
  start and end, up and down keeping the goal column, Page Up and Down by the view's
  height, document start and end; Shift extends. Typing replaces the selection; Enter
  splits (at the end of a heading the new paragraph takes the style's "next", body
  text); Shift+Enter a line break (U+2028); Tab a tab; Backspace and Delete by
  grapheme or word, joining paragraphs at their edges. Cut, copy and paste as plain
  text (newlines make paragraphs). Ctrl+B/I/U toggle by what the caret shows, on the
  selection or, with none, on what is typed next; Ctrl+0/1/2/3 Normal and Heading 1–3;
  Ctrl+L/E/R/J alignment. Undo and redo through `officina_core::history`: a run of
  typing or deleting is one step (`History::extend_last`), other actions one step each.
- **Ops say what they changed.** Each applied `TextOp` notes its block range on the
  document (`TextDoc::take_changes`, composed with `Edit::then`), so the editor hands
  the paginator one edit per action, undo and redo included. Pagination then runs for
  up to 4 ms after a key; the rest continues in later frames.
- **Geometry.** A position's page, x, line top and height (`Editor::spot`) from the
  pages' slices and the cached line layouts; a page point back to a position
  (`Editor::hit`), cluster by cluster, so a ligature or a mark stays whole.
- **Paged view** (`view.rs`). Pages down a scroll view at zoom 1 = 4/3 units a point;
  only the pages in view and one each side are sent, each a scene group keyed by its
  page number; text is shaped glyph runs (`Node::Glyphs`, faces from
  `pergola_document::Faces`), with highlight, underline and strike rects. Selection
  and caret are an overlay group, so moving the caret sends no page. Content is placed
  relative to a base moving in 8,192-unit steps (as `GridView`), so page 1,500 has
  small coordinates. While pagination runs, the page count and scroll height are
  estimated from the characters reached. Pointer press places the caret, drag
  selects; keys scroll the caret into view.

Gates: 8 tests (typing, Enter, joins and undo in steps; graphemes with combining marks
and emoji modifiers; word motions; selection across paragraphs with copy, cut and
multi-line paste; formatting keys and typing attributes; up and down keeping the
column; hit testing; edits, undo and redo repaginating to what a fresh pagination
gives; a 1,800-page document sends only the pages near the view, at small
coordinates; a click places the caret; a key changes one page and the overlay; an
arrow key only the overlay). `examples/keystroke_500.rs` (release, cloud container,
real shaping): in a 510-page document, 261 keys on page 255 (typing, Enter, Backspace,
Down) take p50 0.64 ms, p99 1.4 ms, max 1.6 ms from key to scene diff, against a
8.3 ms frame at 120 Hz.

Not yet: IME composition (no composition events reach apps yet, §17 D10), bidi, spell
underlines, accessibility, the flowing layout, double-click word selection and
Shift+click (pointer events carry neither), auto-scroll while dragging past the edge,
caret blink.

### 16.12 O3c as built (2026-11-14): `atrium-write`

- **Native file** (`officina-text/src/native.rs`, `.owrite`): an `officina_core`
  container of kind Write with DOCUMENT (page setup, next block id), STYLES, ATTRS (the
  character attribute table), BLOCKS (256 blocks a chunk, independent, so a reader can
  load the chunks near the reading position first; v0 loads all) and CHECKPOINTS (the
  page starts of the last finished pagination). Optional properties stay optional in
  the file (a mask of the fields set), so "not set" and "set to the default" survive.
  The reader is for hostile files: counts bounded by the bytes left, style and
  attribute references checked, run starts on character boundaries and in order, block
  ids unique and under the watermark; 3,000 damaged and cut files load or fail, none
  panics.
- **Autosave and recovery** (`atrium-write/src/doc.rs`), the Sheets scheme: a private
  folder with a base file and a journal (length and CRC per record), switched by
  generation, locked while a window owns it. A record is one editor action
  (`pergola_richtext::Logged`): the ops as applied, whether they joined the action
  before (a run of typing is one undo step after recovery too), and the attribute
  entries interned for them, so the replayed table numbers them the same. After a
  crash the document comes back as it was on screen, undo history included; a torn
  last record is cut off.
- **The app**: a toolbar (Open, Save, Save As, Undo, Redo, B, I, U, a Style menu with
  Normal, Title and Heading 1–3, alignment), the pages through `RichTextEditor`, a
  status line (page of pages, "≈" while pagination runs, paragraphs, the last
  message). Files come from the system picker as in Sheets; `.owrite` is native,
  anything else opens and saves as plain text (a paragraph per line, UTF-8 or
  Windows-1252) until the converters of O3e. `scripts/write-vm.sh` runs it on a
  headless frescod in the cloud VM; `examples/sample_doc.rs` writes the sample shown.

Gates: native round trip (styles, attributes, properties, checkpoints); op encodings
both ways; hostile files; a crash after typing, formatting, undo and redo recovers the
same text and runs with a torn record cut off, and undo then runs back to the file as
opened; save marks clean; text files in UTF-8 and Windows-1252. The cloud VM screenshot of the sample (`scripts/write-vm.sh`) shows the title, headings, a run of bold, italic, underline, a link, colour and highlight, and justified, centred and right-aligned paragraphs on an A4 page.

Not yet: the converters (O3e), a "save changes?" prompt, Scrinium's Save As sheet,
the jailed per-document launch, zoom and the ruler, lists, tables and images (O3d).

### 16.13 O3d part 1 as built (2026-11-14): lists, headers and footers

- **Lists** (`officina-text/src/model.rs`, `lists.rs`): a list item is a paragraph
  property `list: ListRef { id, kind: Bullet | Numbered, level 0–8 }` (in a style or
  set on the paragraph; "set to none" overrides a style's list). Numbers are not
  stored: `lists::Numbering::build` counts items per list id and level in document
  order (a deeper item restarts the levels below it), and labels cycle by level:
  `1.`, `a.`, `i.` and `•`, `◦`, `▪`. An item indents 720 twips a level and its label
  hangs 360 twips into the indent; the first line starts at the indent whatever the
  first-line indent. In the native file the list is bit 12 of the property mask; a
  reader refuses unknown bits.
- **Editing** (`pergola-richtext`): `toggle_list` (a new list continues one of the same
  kind just above), Tab and Shift+Tab change level inside items, Enter on an empty
  item and Backspace at an item's start leave the list (text kept), Ctrl+Shift+7
  numbered and Ctrl+Shift+8 bullets. Toolbar buttons "• List" and "1. List".
- **Headers and footers**: one plain header and one footer per document
  (`HeaderFooter { text, align }`, `{page}` and `{pages}` filled per page), drawn 720
  twips from the page edge outside the margins, so they never move body text.
  `TextOp::SetPage` changes page setup, header and footer as one undoable op (native op
  8, DOCUMENT carries both); a geometry change repaginates from the start. Toolbar
  "Page #" toggles "Page n of m", centred, in the footer.
- **Pagination fix**: an edit that came while the first pagination was still running
  converged onto the old pages and then stopped, so the last pages were lost until the
  next edit. The paginator now remembers where it had got to and carries on from there
  after converging; an edit past that point repaginates from the edit.

Gates: numbering and labels per level; list keys in the editor (make, nest, leave,
undo); page setup and footer undo; an edit before pagination finishes still reaches
the end and matches a fresh pagination; native round trip with lists, header and
footer.

Not yet: tables, images, section-level headers (first page, odd and even), header
editing beyond page numbers, list restart and start-at.

### 16.14 O3d part 2 as built (2026-11-14): pictures

- **Model** (`officina-text`): a picture is the character U+FFFC with a character
  attribute `object: ObjectRef { image, width, height }` (its frame in twips), so it
  flows in line with the text, is typed over, selected, copied, deleted and undone like
  any character, and every existing op serves it. The bytes live once per document in
  `TextDoc.images` (append-only while open, the same bytes stored once), with their
  pixel size read from the header when added (`navigator_image::image::natural_size`,
  nothing decoded).
- **Layout**: each U+FFFC in an object run is one glyph (`OBJECT_FACE`) as wide as the
  frame, scaled down to fit the column; the picture stands on the baseline and the line
  grows to hold it. Layout reads only the frame, so paginating any number of pictures
  decodes none (§7.4). Other characters in such a run are shaped as text.
- **Native file**: chunk 6 IMAGE, one per picture (index, pixel size, bytes, at most
  48 MB), so a reader can load them late; attribute mask bit 10 is the object. The
  reader refuses gaps, repeats, frames over 22 in and references past the pictures.
- **Editor**: `insert_image` places a picture at the caret at 96 dpi, shrunk to the
  column and 90 % of the page height; text typed beside a picture takes its formatting
  but never the picture; typed or pasted U+FFFC is dropped. The journal records the
  pictures an action added (at the end of the record, so older records still read).
- **Drawing** (`pergola-richtext/src/pictures.rs`): a picture is decoded the first time
  a page showing it is drawn, at about twice its drawn size and never above its natural
  size, by the Navigator's decoders (PNG, JPEG, GIF, WebP) with their header ceilings;
  pixels stay in a 256 MB least-recently-used cache keyed by the bytes and size; a
  picture that won't decode draws a grey frame. Pages never move either way.
- **atrium-write**: "Picture…" asks the picker for a PNG, JPEG, GIF or WebP (read
  only, at most 16 MB) and inserts it at the caret.

Gates: a picture's glyph, scale to the column and line height; native round trip with
a picture stored once; a reference past the pictures refused; insert, type beside,
paste U+FFFC, a huge picture fitted, journal replay, undo; only the picture near the
view is decoded, once, and scrolling reuses it; journal records with and without
pictures.

Not yet: decoding off the UI thread (a large JPEG decodes in the frame that first
shows it), resizing a picture by dragging its handles, floating pictures with wrap
(O5e, §16.30), pictures dropped from the store when unused (they go on saving until reopened).

### 16.15 O3d part 3 as built (2026-11-14): tables

- **Model**: a table is paragraphs with a `cell: CellRef { table, row, col, width }`
  property (direct only, never from a style). Consecutive paragraphs with the same
  table and row are a row; within it, consecutive ones with the same column are a cell,
  so a cell holds any number of paragraphs and every existing op (typing, Enter, undo,
  the journal, the native file: property mask bit 13) works in cells unchanged. Row and
  column numbers only tell neighbours apart. Cells are laid out left to right in the
  order they come, at their widths, scaled down when a row is wider than the column;
  a row has at most 63 cells and is looked through for at most 4,096 paragraphs, so a
  hostile file can't make layout quadratic.
- **Pagination**: a row is laid out whole (each cell's paragraphs at the cell's width
  inside 80-twip padding; the row as tall as its tallest cell) and goes on one page; a
  row taller than a page goes alone and is cut at its foot. Slices carry their x and
  width; pages carry the cell boxes for the borders. Keep-with-next doesn't reach into
  a table. Edits in cells repaginate and converge like any other.
- **Editor**: clicks pick the cell by x; Up and Down move to the cell above or below;
  Tab and Shift+Tab move between cells, Tab in the last cell adds a row; Backspace and
  Delete never join cells or a cell and the text around it; a selection across cells
  clears each cell's part. Commands: insert table (after the caret's paragraph, with a
  paragraph after it when needed), insert row below, insert column right (the caret's
  cell splits its width with the new one in every row), delete row, delete column (its
  width goes to the neighbour). All are one undo step.
- **atrium-write**: an Insert menu (Picture…, Table, Row Below, Column Right, Delete Row,
  Delete Column, Page Numbers) replaces the Picture and Page # buttons; table items are
  enabled in a table.

Gates: rows side by side and never split across pages; an edit and a split in a cell
repaginate like a fresh pagination; native round trip with a cell; the commands, Tab,
Enter in a cell, deletes at cell edges and across cells, Up and Down, clicks by column,
undo of all of it.

Not yet: rows split across pages and repeated header rows (§7.4 long tables), merged
cells, cell shading and border styles, column resizing by dragging, nested tables,
automatic column widths (fixed widths now, as the spec asks for long tables).

### 16.16 O3e as built (2026-11-14): `.docx` and `.odt`

- **Shared**: `officina-convert::xml`, a bounded DOM over the existing pull parser (no
  DOCTYPE, at most 2^24 elements, depth 256, 1 GiB of text), used by both readers.
  Pictures are kept only when they decode to a size (PNG, JPEG, GIF, WebP through
  `pixel_size`), at most 48 MB each and 65,536 per document.
- **`.docx` reader** (`docx::read`): styles (Word's built-in names mapped to ours,
  document defaults under Normal, basedOn chains), numbering (bullet or numbered per
  level, one list per `numId`), runs (bold, italic, underline, strike, size, colour,
  font, highlight), hyperlinks (external targets only), fields (PAGE and NUMPAGES in
  headers and footers, field results elsewhere), inline pictures, tables (grid widths,
  `gridSpan` widens a cell), page size and margins (header and footer distances
  included), the default header and footer. First-page and even headers are reported
  only when the document turns them on.
- **`.docx` writer** (`docx::write`): styles under Word's ids, numbering keyed by list
  and kind, runs, tabs and line breaks, inline pictures, tables with a grid and
  borders, `header1`/`footer1` with PAGE and NUMPAGES fields, `sectPr`.
- **`.odt` reader** (`odt::read`): named, automatic and default styles (Standard is
  Normal, `Heading_20_N` is Heading N), list styles with nesting and continued
  numbering, spans, links, spaces and tabs, page number and count fields in the
  master page, as-character frames, tables with column widths, the first master
  page's layout (header and footer heights added to the margins, with LibreOffice's
  dynamic spacing inside the height).
- **`.odt` writer** (`odt::write`): `mimetype` stored first, manifest, named styles,
  page layout and master page with header and footer, automatic paragraph, text, list
  and table styles, nested lists, tables, as-character pictures.
- **What doesn't come through** is reported, never silent: footnotes, comments,
  equations, drawings, charts and embedded objects are dropped; floating pictures,
  text boxes, merged cells, nested tables, tracked changes, sections, columns, tab
  stops, super/subscript, small caps and similar are kept approximately (each with
  its own note).
- **atrium-write** opens `.docx` and `.odt` through the sandboxed converter process
  (`officina-convert docx|odt` writes `.owrite`) and saves both in-process; the picker
  offers both. The status line shows the first fidelity note.

Gates: round trip of a document using everything the model has, both formats, with an
empty report; 2,000 hostile packages each, clean; LibreOffice both ways
(`examples/docx_lo.rs`: our `.docx`, saved by LibreOffice as `.docx`, read back, 18 of
18 checks; `examples/odt_lo.rs`: our `.odt` saved by LibreOffice as `.odt`, and our
`.docx` saved by LibreOffice as `.odt`, 36 of 36): text, styles, run formatting, links,
paragraph spacing and alignment, list kinds and levels, the table, the picture's size
and pixels, header, footer with page numbers, page size and margins.

Not yet: footnotes, comments, tracked changes kept as such, `.doc`. The real-world
corpus run is §16.18.

### 16.17 O3f as built (2026-11-14): find, word count, PDF export, spelling

- **Find and replace** (`officina-text::find`): queries with match case and whole
  words over paragraph text (never the laid-out pages, so it works while pagination
  runs); find next and previous wrap round; replace all is one action (one undo);
  `Search` finds every match a slice of paragraphs at a time. On the 1,514-page
  document all 12,550 matches come in 10 ms and the first in microseconds, so the
  per-chunk index §7.4 planned isn't needed yet. Replace all of 12,622 matches takes
  21 ms, its undo 7 ms.
- **Word count**: words (runs between spaces with a letter or digit; each ideograph a
  word), characters with and without spaces, paragraphs; kept per paragraph through
  edits so a keystroke recounts one paragraph. The status line shows words and
  characters, or "N of M words" over a selection.
- **Page painter** (`officina-text::paint`): a page as items in layout units (glyph
  runs carrying their text, rectangles, pictures, link areas). The screen view now
  draws from it, and so does PDF export, so the two can't drift apart.
- **PDF export** (`officina-pdf`): streamed page by page (layouts dropped every 64
  pages); each face embedded once as a TrueType subset that keeps glyph ids
  (CIDFontType2, Identity-H, widths, a ToUnicode map so text copies and searches);
  each picture once (JPEG as it is, others decoded by the bounded decoders and
  deflated, with an SMask for alpha); links as URI annotations (http, https and
  mailto only); title in the document information. atrium-write's **PDF…** picks a
  folder and exports in the background with progress in the status line.
- **Spelling** (`officina-spell`): our own reader for Hunspell-format dictionaries
  (prefix and suffix rules with conditions and cross products, continuation classes,
  compound rules, REP pairs with anchors, ICONV, TRY, NOSUGGEST, FORBIDDENWORD,
  NEEDAFFIX, ONLYINCOMPOUND, KEEPCASE; long and numeric flags; bounded), the SCOWL
  English (US) dictionary built in (permissive SCOWL/Ispell licence, kept in
  `officina-spell/dict/README_en_US.txt`). Suggestions: replacement pairs, swaps,
  split words, one edit, then two edits when one finds little. The editor takes a
  checker from the app and draws a dotted red line under misspelled words on the
  pages in view (cached per paragraph text; not the word being typed). Tools ›
  Spelling opens a bar with the word, its suggestions, Ignore (this session), Add
  (kept in the app's `words` file) and Next. Tools › Find and Replace (or Ctrl+F)
  opens the find bar.

Gates: find, replace and count on 1,514 pages as above; PDF export of 1,474 pages with
100 pictures in 5.4 s with peak memory going from 22 to 31 MB, poppler reads all pages,
the first paragraph's text and the last page's footer back; spelling: all 129,490 forms
the dictionary's rules make check as right, 50 of 50 common misspellings caught with the
intended word suggested for all 50 (about 60 ms a suggestion), this repository's docs
(580,086 words) checked in 0.6 s.

Not yet: PDF/A-2b (XMP metadata and an output intent), tagged PDF and bookmarks from
headings, export of a selection or a page range; dictionaries other than English (US)
and choosing a language per paragraph; spelling in headers and footers; regular
expressions in find.

### 16.18 O3 real-world corpus run (2026-11-14)

The run §7.6 asks for: `officina-convert/examples/corpus_text.rs` reads every `.docx`
and `.odt` in the given folders with our reader, compares our text with LibreOffice's
text of the same file (word counts both ways, list labels left out), and writes each
document with our writer and reads it back. The corpus is test data kept out of the
repository: Apache POI's `test-data/document` (130 files) and LibreOffice's
`sw/qa/extras/odfexport/data` (134) and `sw/qa/extras/ooxmlexport/data` (1,594), most of
them made to break readers.

- 1,858 files: 1,832 read, 26 refused with a reason (encrypted, damaged ZIP, a DTD,
  nesting past the limit, a SHA-1 encrypted `.odt`, packages with no main part), **no
  panics**; the slowest file took 0.47 s.
- Against LibreOffice on the 1,131 files where it gave text: 99.46 % of its words are in
  ours, median file 100 %, 884 files at 99 % or more both ways.
- Our writer then our reader keep the text of all 1,832.

Reader gaps the run found, now fixed: content controls and custom XML around table rows
and cells; text boxes (DrawingML, VML, `mc:AlternateContent`, groups, boxes holding
content controls) whose text now follows the anchoring paragraph in both formats,
as do ODF shapes with text; ruby (base text kept) in both formats; a `w:t` without
`xml:space="preserve"` (its ends' white space dropped, line ends inside it are spaces);
more than one `w:body`; an `.odt` whose body element has a damaged name.

What is left of the difference is by choice or out of scope: tracked deletions (we show
the document as it stands, LibreOffice's text includes deleted text); list labels in
LibreOffice's text; fields LibreOffice recalculates (document properties, formulas with
errors, dates) where we keep the stored result; content controls bound to document
properties; headers and footers (not in our body text); footnote marks.

### 16.19 O5 plan (2026-11-14)

O5 finishes the word processor's long tail (§7.3), then the spreadsheet's (§16), in
steps that each end with a gate on the matching corpus files (§16.18's corpus has
hundreds of each) and LibreOffice both ways:

| step | what | gate |
|---|---|---|
| O5a | footnotes and endnotes: anchors in the text, numbers derived from the order, note text at the foot of the anchor's page (endnotes at the end), editing in a note bar, `.docx`/`.odt` both ways, PDF | notes on the right pages after random edits; LibreOffice both ways; corpus files with notes keep their note text |
| O5b | comments: anchored ranges, replies, shown beside the page, both formats | LibreOffice both ways; corpus files with comments |
| O5c | tracked changes: insertions and deletions with author and time as marks on text (and on paragraphs), shown or hidden, accept and reject, kept on round trip | LibreOffice both ways; the corpus's tracked-change files read the same as LibreOffice with changes shown |
| O5d | sections and columns, table of contents and cross-references (fields resolved after pagination, at most three rounds, §7.4) | LibreOffice both ways; a 1,500-page TOC update gate |
| O5e | floating pictures with wrap (square, top and bottom, behind, in front) | LibreOffice both ways; layout tests |
| O5f | sheets: conditional formats, dynamic arrays and spill, `LET`, `LAMBDA` | Excel-result differential cases, as O1's |
| O5g | sheets: pivot tables (model, refresh, `.xlsx`/`.ods`) | LibreOffice both ways |
| O5h | recent files via bookmarks, once Scrinium has home folders | — |

**O5a design.** A note's anchor is U+FFFC with a `note` attribute naming an entry of
the document's note store, so the anchor is one character to the caret, find and the
op log, like a picture. A note's text is a short list of paragraphs (text, runs,
style) in that store; changing it is one op holding the old and new text, so undo and
the journal cover it. Numbers are derived (footnotes 1, 2, 3 and endnotes i, ii, iii in
document order, as Word's defaults), never stored. Pagination reserves the foot of
the page for the notes anchored on it: a line goes on a page only if it and its notes
fit, so the anchor and its note always share a page (a note taller than the page goes
alone and is cut, like a table row); endnotes follow the last paragraph on pages of
their own. Adding or removing an anchor renumbers the notes after it, so it
repaginates to the end; changing a note's text repaginates from its anchor's page.

### 16.20 O5a as built (2026-11-14): footnotes and endnotes

- **Model** (`officina-text::model`). A note anchor is U+FFFC with `CharAttrs::note`
  naming an entry of `TextDoc::notes`; a note is its kind and up to 64 paragraphs (with
  their own block ids, so the layout cache keeps them). Ids are never reused while a
  document is open, so undo brings an anchor back to its note. Numbers are derived:
  `note_numbers()` counts anchors in order (footnotes 1, 2, 3; endnotes i, ii, iii),
  cached until an op puts in or takes out an anchor. `CharAttrs::vert` adds
  superscript and subscript (drawn at two thirds size, raised a third or lowered a
  seventh), which the converters now read and write instead of dropping.
- **Ops.** `TextOp::SetNote` replaces a note's text (its inverse holds the old text).
  Inserting a note is `SetNote` then the anchor's splice, one undoable action. An op
  that moves anchors renumbers and repaginates from its block to the end, since a
  label's width can move lines; changing a note's text repaginates from its anchor's
  page. The native file has a NOTES chunk (kind 7) and the journal encodes `SetNote`.
- **Layout and pagination.** An anchor is shaped as its label, raised. A line goes on
  a page only if it and the footnotes anchored in it fit above the foot, so a note is
  always on its anchor's page; lines that move on take their notes with them; a line
  alone on an empty page goes with its notes even if they overfill it (the excess is
  cut, like a too-tall table row). Footnotes sit at the foot under a short rule, their
  label in an 18 pt hang. Endnotes follow the last paragraph under a rule and run on
  over as many pages as they need; those pages start at checkpoint (block count, line)
  and are always laid out again, so they never go stale. 200 random edits (anchors in
  and out, note text changed, typing, deleting) give the same pages as a fresh
  pagination, with every footnote on its anchor's page.
- **Painting** draws the rules, labels and note text, so the screen and the PDF show
  them alike; the PDF's text layer has the labels, not U+FFFC.
- **Editor and app.** `insert_note`, `note_at_caret`, `note_text`, `set_note_text`.
  Insert ▸ Footnote or Endnote puts the anchor at the caret and opens a note bar (like
  the find bar) whose field holds the note's text, its paragraphs marked ¶; Done puts
  it in the note, Insert ▸ Edit Note opens the bar on the note beside the caret. Text
  typed after an anchor is never part of it.
- **Files.** `.docx`: `footnotes.xml` and `endnotes.xml` (separators skipped on read and
  written as Word does, each part with its own relationships), the note number as
  `w:footnoteRef`, Word's space after it dropped on read. `.odt`: `text:note` in place
  with its citation and body. Notes inside notes, headers and footers are reported
  dropped.

Gates: the pagination and journal tests above; both formats round trip the notes
(numbers, text, run formatting) with an empty report; LibreOffice both ways 20 of 20
(`.docx`) and 40 of 40 (`.odt`) checks, the new ones being "footnote 1 and endnote i
with their text" and "superscript"; the §16.18 corpus run now reads 145 notes in 60
files, keeps all of them through our writer and reader, and is at 99.51 % of
LibreOffice's words (anchors compared as their numbers), 912 files at 99 % both ways;
1,514-page pagination unchanged (2.3 s).

Not yet: editing a note's text on the page itself (the bar edits it as one line with ¶
between paragraphs), note text split across pages (a footnote longer than the space
left is cut), separator and numbering settings (restart per page, symbols), notes in
tables' header rows repeated on each page.

### 16.21 O5b as built (2026-11-14): comments

- **Model** (`officina-text::model`). A comment is author, initials, date (ISO 8601
  UTC), text (a paragraph a line, at most 64 KB), the comment it answers, and whether
  it is resolved; `TextDoc::comments` holds them by id (at most 2^20; ids never
  reused while the document is open). What a comment covers is not stored apart from
  the text: `CharAttrs::comments` is the sorted list of comments over a run, so ranges
  move with edits for free, overlap, cross paragraphs, and vanish with their text.
  `comment_ranges()` derives each comment's first and last character;
  `live_comments()` is the anchored ones and their replies, in order, which is what
  files keep. Replies have no range: they belong to their thread.
- **Ops.** `TextOp::SetComment` puts in, changes or takes out one comment (no layout
  edit). Adding a comment is `SetComment` then a format pass putting its id on the
  range; deleting one removes it and its replies from their union range in a single
  pass, then their entries; both one undoable action. The native file has a COMMENTS
  chunk (kind 8), attribute bit 13, and the journal encodes `SetComment`.
- **Painting and the view.** The page painter emits a mark item over commented text
  (the PDF ignores it); the view draws it translucent yellow, grey when resolved.
  Balloons sit to the right of the page when the window has room: author (and
  "resolved"), up to three lines of text, the reply count; the thread at the caret
  gets an accent border, and clicking a balloon moves the caret to the comment and
  opens it. Text typed at the end of a commented range is not part of it.
- **Editor and app.** `add_comment` (selection, else the word at the caret),
  `comment_at_caret`, `thread`, `reply_comment`, `edit_comment`, `delete_comment`.
  Insert ▸ Comment and Edit Comment open a comment bar (like the note bar): the first
  comment's text, with Done, Reply, Resolve or Reopen, Delete and Close. A new comment
  left empty goes away. The author is `$USER` (else "Me") until Atrium has an account
  name to use.
- **Files.** `.docx`: `w:commentRangeStart`/`End` in runs, paragraphs, the body and
  cells; `comments.xml` (text, author, initials, date); `commentsExtended.xml` for
  threads (`w15:paraIdParent`) and resolved (`w15:done`), tied by the last paragraph's
  `w14:paraId`. A comment with a reference and no range covers the character before
  it. Written the same way, replies with their thread's range as Word does. `.odt`:
  `office:annotation` and `office:annotation-end` by name, a point annotation over the
  character before it, `dc:creator`, `dc:date`, `meta:creator-initials`,
  `loext:resolved`, and threads by LibreOffice's `loext:parent-name` (a reply may come
  before its parent; ties are made after the text). Comments in notes, headers and
  footers are reported dropped.

Gates: editor test (add, reply, resolve, overlap, delete with replies, undo, journal
replay to the same document) and app test (bar commands); both formats round trip
overlapping and cross-paragraph comments with their threads, dates and resolved state
with an empty report; LibreOffice both ways 22 of 22 (`.docx`) and 44 of 44 (`.odt`),
the new checks being "comment over “the table” with a resolved reply" and "comment
date"; the §16.18 corpus reads 90 comments in 39 files (9 files still have comments
with no place to anchor them) and our writer and reader keep all of them; text
against LibreOffice unchanged at 99.51 %.

Not yet: editing a comment on the balloon itself, rich text in comments, mentions,
showing comments in the PDF, a comments list pane, Word's paragraph-level comment
dates in local time (we keep what the file says).

### 16.22 O5c design (accepted 2026-11-15): tracked changes, compare and merge

**What goes wrong today.** Word and LibreOffice show a reviewed document as one
surface carrying everything at once: the new text, the old text struck through,
insertions underlined in one colour per author, a balloon for every formatting
change, and the comments. Three edits to a sentence become nine fragments; retyping
one letter shows as a deleted word and an inserted word; a moved paragraph shows as a
deletion here and an insertion there; and nobody can read the result. Accepting is
item by item through hundreds of fragments. Compare produces the same markup from a
character diff with no idea of moves or formatting, and merging two reviewers' copies
piles both sets of markup together, with overlapping edits shown as interleaved
fragments instead of as a conflict.

The causes are the same each time: (1) reading and reviewing are one view, (2) changes
are kept and shown at the grain they were typed (characters) rather than the grain
people think in (words, sentences, moves, "made the headings bold"), (3) there is no
unit larger than a change (who proposed what, in which pass), and (4) compare and merge
are bolted on to the markup instead of sharing one model with it.

**The design in one paragraph.** The document always reads clean: the text shown is
the proposed text, and changes are only a thin bar in the margin and a count in the
status line. Reviewing is a separate mode, like reviewing code: a lane beside the page
lists changes as *edits* (word-level, a replacement as one item, moves as moves,
formatting-only and whitespace-only edits collapsed into one line each), grouped into
*change sets* (one person's pass, optionally named); selecting an edit shows that one
edit in place, old and new, and Accept, Reject, Next go through them by key. Compare
and merge produce the same edits and change sets from two or three documents, so they
are reviewed the same way, and a real conflict (two people changing the same words
differently) is one card with three choices, never two sets of markup on top of each
other. Comments keep the balloon column (§16.21); changes never get balloons.

#### Model (officina-text)

- **Insertions** are runs with `CharAttrs::rev = Some(RevId)`; a revision is author,
  time and change set, kept in `TextDoc::revisions` (like comments).
- **Deletions are not left in the text.** Deleted text moves into a *tombstone*: a
  U+FFFC anchor with `CharAttrs::deleted = Some(DelId)`, whose entry holds the removed
  runs (and paragraph breaks with the second paragraph's properties) and its revision.
  So the text is always the proposed text: layout, find, word count, spelling, the
  caret and the PDF need nothing new, and a tombstone is one character to them like a
  note anchor (drawn as nothing, or, for the edit being reviewed, as the old text
  struck through).
- **Formatting changes** are runs with `CharAttrs::fmt_rev`, the revision holding the
  attributes the runs had; paragraph changes the same on `ParaProps`.
- **Paragraphs inserted or deleted whole** are insertions and tombstones that hold
  paragraph breaks; joining or splitting paragraphs is the same.
- **Your own pending text is just text.** Typing inside your own unaccepted
  insertion edits it; deleting part of it removes it outright (no tombstone);
  deleting someone else's insertion is a rejection of that part, recorded as yours.
  Re-formatting text twice keeps the first old attributes. This is most of why a
  tracked document stays small.
- **Accept and reject** are plain undoable ops: accepting an insertion clears `rev`,
  rejecting it deletes the text; accepting a deletion drops the tombstone, rejecting
  it splices the old text back; formatting likewise. Reject-all gives the original
  exactly, accept-all the proposed text, which the gates check.
- **Tracking** is a document setting kept in the file; while it is on the editor turns
  each typed or deleted piece into these ops, attributing it to the current author and
  change set.

#### Edits and change sets (what people see)

- An *edit* is computed, never stored: adjacent revisions of one author in one
  paragraph, widened to whole words, with a deletion next to an insertion read as a
  replacement ("quick → fast"). A tombstone and an insertion holding the same text are
  a move ("moved ‘Costs’ above ‘Plan’"). Formatting-only edits of one kind are one line
  ("bold on 6 headings"); punctuation- and white-space-only edits are marked minor.
- A *change set* is one author's revisions in one session, or a named proposal
  ("Legal's pass"). The lane lists sets, then edits; filters by person and kind; and
  Accept or Reject applies to an edit, a set, all minor edits, or everything.
- **Views.** *Reading* (default): proposed text, margin bars in the author's colour,
  "14 edits by 3 people · Review" in the status line. *Review*: the lane, and only the
  selected edit drawn in place (old struck, new marked), plus its paragraph's other
  edits faintly. *Original*: reject-all, read-only. *All markup*: every edit in place,
  Word's way, for those who want it and for printing a record.
- A change can carry a comment thread (§16.21) for the "why", shown in the lane.

#### Compare

Compare (Write ▸ Compare with…) takes two documents and produces a change set that
turns the first into the second, reviewed in the same lane (and kept as tracked
changes only if asked). It works in three passes: paragraphs are matched by stable
paragraph id when both come from one native document, else by a patience diff over
paragraph hashes with similar-paragraph pairing for edited ones; unmatched pairs with
the same text are moves; matched paragraphs get a word-level diff (Myers over word
tokens, so edits are never smaller than a word); formatting and paragraph properties
are compared apart from text, so a restyle is one formatting edit and not a
rewrite. A side-by-side view aligns matched paragraphs.

#### Merge

Copies sent out for review are saved with tracking on, so each returned copy carries
its original: reject-all of it. Merging copies whose originals are the same (same
document id and content hash) is applying each copy's change sets to one document:
edits that touch different words go in as they are, each person's as their own change
set; edits that overlap are a *conflict*, one lane entry showing the two (or more)
versions with Take A, Take B, Keep both, Edit. Copies edited without tracking are
first compared with the original (the user picks it, or the document's last
"shared for review" snapshot) and then merged the same way. The merge result is still
a reviewable document, so nothing is accepted behind anyone's back.

#### Files

`.docx`: `w:ins`, `w:del`, `w:moveFrom`/`w:moveTo`, `w:rPrChange`, `w:pPrChange`,
paragraph-mark `w:ins`/`w:del`, and `w:trackRevisions` in settings, both ways, so Word
and LibreOffice users see ordinary tracked changes. `.odt`: `text:tracked-changes`
with `text:changed-region` (insertion, deletion, format-change), `text:change`,
`text:change-start`/`end`, both ways. On reading, fragments are coalesced into our
revisions (one per author and time); edits and change sets are derived as above.

#### Steps and gates

| step | what | gate |
|---|---|---|
| O5c1 | model: insert runs, tombstones, formatting and paragraph revisions, tracking in the editor, own-text rules, accept/reject ops, native chunk and journal | 10,000 random tracked edit sequences: reject-all = original, accept-all = the same edits untracked, undo and journal replay exact |
| O5c2 | edits and change sets, Reading/Review/Original/All views, margin bars, lane, accept/reject by edit, set, kind, all | grouping tests (replacements, moves, formatting batches); live screenshots |
| O5c3 | `.docx` and `.odt` tracked changes both ways | LibreOffice both ways; the corpus's tracked-change files read the same as LibreOffice with changes shown, and our round trip keeps them |
| O5c4 | compare (paragraph matching, moves, word diff, formatting) and side by side | compare(A, B) applied to A = B and rejected = A on corpus pairs and random edits; moves found |
| O5c5 | merge of reviewed copies with conflicts | merges of random non-overlapping edits equal applying both; overlaps always become conflicts, never lost text |

Spreadsheets get the same idea later (cell-level edits in change sets, a lane, no
cell-comment clutter) but are not part of O5c.

### 16.23 O5c1 as built (2026-11-15): the tracked-changes model

- **Model** (`officina-text::model`, `track`). `CharAttrs` gained `rev` (inserted
  under a revision), `deleted` (the U+FFFC is a tombstone naming a `Deletion`) and
  `fmt` (a `FmtChange`: the revision and the earlier attribute entry the text had);
  `ParaProps::rev` marks an inserted paragraph break. `TextDoc` gained `revisions`
  (author, date, change set, set name), `deletions` (the revision and the pieces taken
  out: text, runs, and for each deleted break the following paragraph's id, style and
  properties) and `tracking`. Both stores share a generic `Store<T>`.
- **Ops.** `SetRevision`, `SetDeletion`, `SetTracking`; everything else is the
  existing splice, restyle, split and join. `tracked_delete_ops` moves what isn't the
  author's own pending text into one tombstone at the start, drops own text and own
  inserted breaks outright, and takes in tombstones of the same revision either
  side, so ten Backspaces leave one tombstone. `tracked_format_ops` keeps the first
  attributes in `fmt` and drops the mark when formatting is put back as it was.
  `resolve_ops` / `resolve_range_ops` accept or reject the changes a filter picks,
  last paragraph first; rejecting a deletion puts its pieces back (splitting at its
  breaks) with the changes inside them resolved too. `changes()` lists the marks in
  document order; `live_deletions()` follows tombstones inside tombstones.
- **Layout.** A tombstone is a zero-width glyph that draws nothing, so the page,
  find, word count, spelling and PDF show the proposed text with no other change.
- **Editor.** With tracking on, typing, Enter, Backspace, Delete, typing over a
  selection (old, then new after the tombstone), cut and the formatting keys make
  tracked changes as `author` in one revision per session; the caret steps over
  tombstones; typing never inherits tracking marks. `set_tracking`, `resolve`,
  `resolve_change`, `change_at_caret`, `next_change`. Not tracked yet: paragraph
  properties and styles, tables, pictures, notes, find-and-replace.
- **File.** Attribute mask bit 14 (`rev`) and bit 15 (a second mask: `deleted`,
  `fmt`), paragraph bit 14, REVISIONS chunk (9, with the tracking flag) and DELETIONS
  chunk (10, live deletions only); journal ops 11–13. A formatting change may only
  name an earlier attribute entry.

Gates: 10,000 random sequences of 30 tracked edits by two authors in three revisions
(insertions, deletions across paragraphs, splits, formatting, undo) keep the proposed
text equal to the same edits made untracked after every step; accepting everything
gives that document with no marks left; the native file round trips it exactly;
rejecting everything gives the original exactly, paragraph ids included (2.4 s). An
editor test covers replacement, repeated Backspace, own-typo fixes, Enter, bold,
journal replay, rejecting one change, reject all, accept all.

### 16.24 O5c2 as built (2026-11-15): edits, review mode, margin bars

- **Edits** (`officina-text::track`). `edits()` turns the marks into what a person
  would call one edit: touching changes by one revision merge, the range widens to
  whole words, and each gets a kind (insertion, deletion, replacement, move,
  paragraph, formatting), old and new text, a `minor` flag (only spaces or
  punctuation) and a sentence: “thirty” → “fourteen”, deleted “…”, moved “…”, bold in
  2 places. Replacements name only the words that differ (“two percent” → “five
  percent” reads “two” → “five”). A deletion and an insertion of the same text (8+
  characters) by one author are a move. Formatting edits group by revision and change.
  `change_sets()` groups edits by set; `resolve_edit_ops` accepts or rejects one edit
  across all its ranges.
- **The page reads clean.** The painter emits `Item::Change` for marked runs and
  tombstones; the screen draws only a 3-unit bar in the left margin per line, in the
  author's colour (authors take colours in the order they first edited, so two people
  never share one; two people on a line get two bars). PDF draws nothing. The status
  line says "7 edits by 2 people" and "Tracking changes".
- **Review mode** (Tools ▸ Review Changes). A bar under the toolbar reads "Edit 4 of
  7 · Raj · deleted “…”" with ‹ › Accept Reject Accept All Reject All Close; the lane at
  the right lists the edits in document order (author colour, kind, sentence), the
  picked one highlighted, a click picks one and scrolls to it. Only the picked edit is
  shown in place: inserted text tinted green, formatting tinted blue, a deletion as a
  red mark with what it took away struck through above it. Accepting or rejecting
  picks the next edit; every step is undoable. Comment balloons step aside while the
  lane is open. Tools ▸ Track Changes turns tracking on and off (undoable, saved).
- **Not yet:** filtering the lane by author or set, accepting a whole set from the
  lane, keyboard shortcuts for review, the Original and All views (§16.22).

Gates: an engine test that edits read as people think of them (replacement, move,
formatting in two places, minor comma); an app test (track, review from the caret,
reject one, accept one, undo, reject all, other bars close review); screenshot of
`examples/review_doc.rs` on the cloud VM (`BAR=review:3 scripts/write-vm.sh`).

### 16.25 O5c3 as built (2026-11-15): tracked changes in `.docx` and `.odt`

- **Shared** (`officina-text::track`). `expanded()` gives the document with every
  deletion put back as text marked `deleted` (nested ones too) and the paragraphs
  whose break before was deleted, for formats that keep deleted text inline.
  `make_deletions(ranges)` is the way back: text read as deleted (and deleted breaks,
  as end-of-paragraph to start-of-next ranges) becomes tombstones through
  `tracked_delete_ops`, touching ranges of one revision joined. Revisions read are
  one per author (dated by their first change; a set each).
- **`.docx`.** Read: `w:ins`/`w:moveTo` mark runs, `w:del`/`w:moveFrom` text
  (`w:delText`) is read inline then made tombstones, `w:rPrChange` gives the
  formatting before, a paragraph mark's `w:ins` marks the break after it (the next
  paragraph's `props.rev`), its `w:del` joins the next paragraph, `w:trackRevisions`
  turns tracking on. Breaks are only taken inside one cell or outside tables
  (deleted rows and cell ends are reported and kept). Write: from `expanded()`,
  runs in `w:ins`/`w:del` with author and date, `w:rPrChange` last in the run
  properties, mark `w:ins`/`w:del`, settings.xml with `w:trackRevisions`.
- **`.odt`.** ODF's model is ours: the body is the proposed text,
  `text:change` marks a deletion whose content (paragraphs) lives in its
  `text:changed-region`, and `text:change-start`/`-end` bound insertions (over
  paragraph breaks too) and formatting changes. Read: regions by id, deletions
  straight into tombstones with their pieces, an insertion open at a paragraph's
  start marks its break. ODF keeps no formatting before a format change: we write
  it as an automatic text style named in `officina:old-style` on the region (our
  namespace, which LibreOffice drops); without it the formatting before is plain.
- **Not yet:** tracked changes in notes, headers and footers (insertions kept,
  deletions dropped, as before); paragraph property changes (`w:pPrChange`,
  ODF paragraph format changes) are kept as made; deleted and inserted table rows.

Gates: the 10,000-sequence random gate now also expands and collapses every
document (deleted text inline and back) and checks the proposed text, accept all
and reject all; `.docx` and `.odt` round trips of a sample with a deletion, an
insertion, bold, an inserted and a deleted break by two people keep every change
(`docx::tests`, `odt::tests`); LibreOffice both ways: `.docx` 26/26 and `.odt`
52/52 (edits by author, accept all, reject all, tracking on). Corpus (1,858 files):
0 panics, 343 tracked edits read in 78 files, our writer then reader keep every
edit of all 78; text recall against LibreOffice 99.51 %.

### 16.26 O5c4 as built (2026-11-15): compare two versions

- **Engine** (`officina-text::compare`). `diff()` is Myers over any sequence,
  after taking off the common start and end; past 2,048 differences the middle
  is one change (bounded time and memory). `TextDoc::compare(other)` accepts both
  documents' own changes first, diffs paragraphs by text, and inside each changed
  stretch pairs paragraphs that share at least half their words (looking 64
  ahead), so an edited paragraph is one paragraph with word changes, not a
  delete and an insert. Paired and kept paragraphs are diffed by words (a lone
  space between two changes joins them); runs that read the same but look
  different become formatting changes. The result is the second document with
  the differences as tracked changes by one revision ("Compare", the author
  given): accept all gives the second, reject all gives the first.
- **Moves.** A move is found where edits are grouped (§16.24), so compared and
  hand-tracked documents read alike: a deleted paragraph and an inserted one by
  one author with the same text is "moved", and one sharing at least 60 % of its
  words (20 characters or more) is "moved and changed it". An insertion of several
  paragraphs holding the moved one is split so the move is found inside it.
- **Commands.** `compare_ops(original)` turns the open document into the
  comparison as ordinary undoable ops (revision, deletions, tombstones,
  formatting, paragraph marks), so Undo takes it back and the journal replays it;
  it refuses a document that already has tracked changes. Write's Tools menu has
  **Compare with…**: pick the earlier version, and the document opens in review
  mode on the first edit, the status line saying how many edits there are.
- **Not yet:** a side-by-side view (review mode's lane covers reading the
  edits); comparing notes, headers, footers and table structure (their text is
  compared as paragraphs); compare of documents that already carry changes
  (accept or reject them first).

Gates: 10,000 random pairs (edits, splits, joins, moves, formatting, empty
documents): accept all gives the second exactly, reject all the first, the native
file round trip keeps both, `compare_ops` undoes to the start and gives the same
edits, and a document compared with itself has none (`compare::tests`). Corpus
(1,858 files): each document compared with an edited copy of itself, 1,832 of
1,832 readable ones accept to the copy and reject to the original (largest,
15,935 paragraphs, in 49 ms). 3,000 paragraphs with scattered edits compare in
21 ms. The sample agreement (`atrium-write/examples/compare_docs.rs`) reads as
six edits: three word changes, a paragraph moved and changed, bold, an inserted
sentence (seen in review mode on the cloud VM).

### 16.27 O5c5 as built (2026-11-15): merging reviewed copies

- **Reading a copy against its original** (`officina-text::merge`). A document
  is walked into pieces, each with where it is in the document and in the
  original (everything rejected): kept text, kept paragraph breaks, and changes
  (insertions and inserted breaks take no room in the original, a tombstone the
  room of what rejecting it puts back). Touching changes are one group.
- **Merging** (`merge_ops(other)`). The original is this document with
  everything rejected; a copy with tracked changes must reject to the same text,
  a copy without is first compared with it (and refused if it shares under 30 %
  of its words, once the original has 20). Groups of both copies are sorted by
  their place in the original; groups that overlap or touch form a cluster. A
  cluster of only the other's changes is copied in over the original text there
  with its tracked marks, under its own revisions (authors, dates and change
  sets kept; a revision both copies carry is shared), with its deletions,
  pictures, notes and comments; comments the other put on text it kept are
  added to the same text here. A cluster where both made the same change is
  kept once. Any other cluster is a **conflict**: this copy's version stays,
  between two zero-width markers (U+FFFC with `conflict`), and the other
  version, with its marks, is kept whole in `TextDoc::conflicts` (a deletion's
  encoding; the CONFLICTS chunk in the native file, `SetConflict` op in the
  journal). Everything is ordinary undoable ops, built last place first.
- **Settling** (`take_ops`). Keep mine drops the markers; Take theirs puts the
  other version (with its tracked changes) in place of what is between them;
  Keep both inserts the other version as it reads after this one, as its
  author's insertion. Reject all with the other's revision picked drops its
  conflicts too; accept all leaves them to settle. Edits show a conflict as one
  entry, "Ann: “fourteen” · Ben: “twenty-one”", with the edits inside it hidden.
- **Diff alignment.** Compare now slides a pure insertion or deletion over
  equal neighbours to join the change beside it, and moves the surplus of a
  mixed change likewise, so an edited paragraph among empty ones is one edit;
  a one-for-one changed paragraph is always an edited paragraph.
- **Write.** Tools › **Merge with…** picks the copy; the status says how many
  changes came in and how many conflicts wait, and review opens on the first
  conflict. A conflict's review bar has **Keep Ann**, **Take Ben**, **Keep Both**;
  on the page the other version waits in an amber label. Saving `.docx` or
  `.odt` with conflicts left keeps this copy's version (the status says so); the
  native file keeps them.
- **Not yet:** paragraph style changes of the other copy on text both kept;
  merging copies whose original was edited in between (three-way with a
  separate base); conflicts inside notes, headers and footers.

Gates (`merge::tests`): 10,000 random pairs of copies editing separate
paragraphs, or separate words of one paragraph (words, paragraphs added and
removed, bold): the merge has no conflicts and accept all equals applying both,
reject all the original, the other copy untracked merges the same, the other
order reads the same, the native file keeps it, and undo returns the copy;
10,000 random pairs changing the same word differently: always a conflict,
reading as this copy, rejecting to the original, Take theirs giving theirs and
Keep mine giving mine. Editor and app tests replay the journal and undo.
Corpus (1,832 readable files), two copies amending every sixth paragraph
offset by three: all 1,832 reject to the original with every amendment present
once, in the text or a conflict, and 1,742 read exactly as both; the other 90
have long runs of identical or empty table cells, where the place of an edit
is ambiguous, and there the merge shows a conflict instead of guessing. The
largest (15,935 paragraphs) takes 10 s for both compares, the merge and the
checks; most take milliseconds.

### 16.28 O5d1 as built (2026-11-15): contents, cross-references, tab stops

- **Tab stops** (`ParaProps.tabs`): position from the column's start edge,
  alignment (start, centre, end, decimal) and leader (none, dots, dashes, line);
  past the last stop, one every half inch. A paragraph's stops replace its
  style's. Layout gives each tab its advance per line (end, centre and decimal
  stops measure the text after the tab) and records leaders, which the page
  painter draws as glyphs on a grid (dots, dashes) or a rule. Native file:
  paragraph property extension bits.
- **Fields** (`officina-text::fields`). A field is one U+FFFC with
  `CharAttrs.field`: a page reference or a text reference to a paragraph by its
  id, shown as a label the way a note anchor shows its number. What each shows
  is derived (`TextDoc::field_values`, never saved, not in undo). When
  pagination is done the editor resolves every field; paragraphs whose fields
  now show something else are laid out and paginated again, at most three
  rounds per change (§7.4), so a page number that moves its own paragraph can't
  loop. A reference whose paragraph is gone shows "?".
- **Table of contents.** Insert › Table of Contents lists Heading 1–3 before
  the caret's paragraph as ordinary paragraphs marked `toc` (level), each the
  heading's text, a tab to an end stop with dots, and a page reference; the
  menu item becomes Update Table of Contents, which rebuilds the entries only
  when the headings changed (page numbers keep themselves current) and keeps
  the entries' ids. Insert › Cross-reference… opens a bar to step through the
  headings and insert the page number or the heading text. All undoable.
- **Files.** `.docx`: a TOC field (`TOC \o "1-N" \h \z \u`) over the
  entries in Word's TOC N styles, `PAGEREF`/`REF` fields with their results,
  bookmarks `_Ref<id>` on referenced paragraphs, `w:tabs` (a paragraph's stops
  added to its style's, `clear` honoured, as Word does). `.odt`:
  `text:table-of-content` with the entries in Contents N styles,
  `text:bookmark-ref` (page or text), `text:bookmark`, `style:tab-stops`
  (positions from the start indent unless the file's settings say otherwise).
  Readers keep nested fields, bind references to bookmarks once the document
  is read (a reference to a missing bookmark keeps its text, with a note), and
  give a contents entry whose page number arrived as plain text (LibreOffice
  flattens the field when it saves a `.docx`) a page reference to the heading
  with its text again. The file's results show until pagination works them out.
- **Not yet:** captions and figure lists; levels
  and styles chosen for the contents; clicking an entry to go to its heading;
  references to bookmarks inside a paragraph, to numbered items and to notes.

Gates: `fields::tests` (contents with the right pages, references following
their target, rounds bounded), editor and app tests with undo. LibreOffice both
ways and across formats (`examples/toc_lo.rs`): our `.docx` and `.odt`, saved
by LibreOffice as `.docx` and as `.odt`, read back with every entry's level,
text and heading, both references and the tab stops, and Update finds the
contents current: 16/16. 1,500-page gate (`pergola-richtext/examples/
toc_1500.rs`, pinned fonts, 1,484 pages): inserting a contents of 750 entries
(22 pages) settles in 28 ms with every entry on its heading's page; pasting a
page of text after it moves 749 entries, settles in 21 ms and lays out only
those 749 and the 12 pasted paragraphs again; a renamed heading updates its
entry; the pages equal a fresh pagination. Corpus: text recall against
LibreOffice 99.51 % with fields shown as their values, all 1,832 still round
trip.

### 16.29 O5d2 as built (2026-11-15): sections and columns

- **Model.** A section starts at a paragraph marked `ParaProps.section`
  (never from a style): how it starts (next page, continuous; odd and even
  pages start on the next page for now), its columns (1–16), the gap between
  them and whether a line runs between. A mark on the first paragraph sets the
  first section's columns. Columns are equal; too narrow a page drops columns
  until each is at least a quarter inch. Native file: a paragraph property
  extension bit.
- **Pagination.** A page fills in bands, one per section that runs on it, each
  band column by column; a paragraph's lines wrap to its column's width. A
  section followed by a continuous break balances its columns above the break
  (the shortest band height that holds the text, by bisection); otherwise its
  columns fill in turn, as Word does. A next-page section starts a page. Lines
  between columns are page items the painter draws. An edit that adds, removes
  or changes a mark lays out again only the paragraphs whose column width
  changed, and pagination resumes as before (checkpoints, stopping when breaks
  line up).
- **Editor and Write.** Insert › Section Break and Continuous Section Break
  (at the start of a paragraph they mark it; elsewhere they split it), and One,
  Two, Three Columns and Line Between Columns for the caret's section, all
  undoable; the caret and clicks move through columns. A split paragraph's
  second half never carries the mark.
- **Files.** `.docx`: a `w:sectPr` in the last paragraph of each section but the
  last (an empty paragraph after a table), the last in the body; `w:type`,
  `w:cols` (`num`, `space`, `sep`; columns of their own widths read as equal),
  and every section names the header and footer. Sections with their own page
  size keep the last one's, with a note. `.odt`: a section with columns is a
  `text:section` whose style has `style:columns` (`fo:column-gap` or the
  columns' indents, a `style:column-sep` as LibreOffice draws it), balanced only
  above a continuous break; a next-page start is a paragraph naming a second
  master page, as LibreOffice keeps Word's section breaks; columns of the page
  layout become the first section's; a nested section's columns win and the
  enclosing ones come back after it; a master page named mid-document reads as
  a page break before, and a section that starts with one starts a page.
- **Not yet:** a page size, margins, header and footer per section; blank pages
  for odd and even starts; columns of their own widths; column breaks.

Gates: paginate tests (columns fill side by side then the next page; a
continuous section balances the columns above it; 300 random edits with
section marks equal a fresh pagination), editor test (breaks, columns, undo,
clicks in the second column), native round trip, `tests/sections.rs` (four
sections through `.docx` and `.odt`). LibreOffice both ways and across formats
(`examples/sections_lo.rs`): our `.docx` and `.odt`, saved by LibreOffice as
`.docx` and `.odt`, read back with every section's first paragraph, start,
columns, gap and line, and the text: 8/8. 1,500-page gate (`pergola-richtext/
examples/sections_1500.rs`, pinned fonts, 1,510 pages, 751 sections, 1,310
lines between columns): paginates in 1.7 s; a page pasted into a two-column
section settles in 3 ms laying out the 6 pasted paragraphs; setting that
section to three columns settles in 5 ms laying out its 25 paragraphs; a
continuous break inside it with one column after settles in 5 ms laying out 9;
undo returns the same pages; each time the pages equal a fresh pagination.
Corpus: recall 99.51 %, merge 1,742 exact and 1,832 safe, unchanged.

### 16.30 O5e as built (2026-11-15): floating pictures with wrap

- **Model.** A picture's frame (`ObjectRef`) may carry a `Float`: how text goes
  round it (square: beside it, on the side with more room; top and bottom;
  behind the text; in front of it), where it sits across (start, centre or end
  of, or an offset from, its column, the margins or the page) and down (an
  offset from its paragraph's top, the top margin or the page's top), and the
  room kept between it and the text. Its anchor is still the U+FFFC in the
  text, so caret, find, undo and the journal treat it as one character; in the
  line it takes no room. Native file: a character attribute extension bit.
- **Layout.** A paragraph can be laid out round *bands* (stretches of its
  height where floats cut room from either side). Each line gets the room at
  its height; where less than an inch is left it goes below the band. Lines are
  broken with guessed heights and again with the heights they got until the
  room each was given holds (at most three passes). The cache key includes the
  bands, so a paragraph no float touches lays out as before.
- **Pagination.** The paragraph anchoring a float places it on the page where
  the paragraph starts (kept inside the content box when measured from the
  text or margins, on the sheet when from the page; one that would run off the
  foot moves up). From then on every paragraph on that page whose lines meet
  a float's frame (gap included) is laid out round it. A paragraph broken
  round floats and carried on to the next column or page keeps the lines it
  was broken into; its checkpoint carries a hash of the bands, so pages only
  line up again when the room is the same. Floats in table cells are placed
  from the cell but the cell's text doesn't go round them; pictures in notes
  stay in line.
- **Drawing and editing.** The page painter draws pictures behind the text
  first and the others after it. A click on a picture puts the caret at its
  anchor. Insert › In Line with Text, Wrap Square, Wrap Top and Bottom, Behind
  Text, In Front of Text, and Picture Left, Centre, Right act on the picture
  at the caret, undoably; a newly floated picture sits at its paragraph's top
  at the column's end.
- **Files.** `.docx`: `wp:anchor` with `wp:positionH`/`wp:positionV`
  (`posOffset` or `align`; relative to column, margin, page or paragraph, with
  the others mapped to the nearest), `wrapSquare`, `wrapTopAndBottom`,
  `wrapNone` with `behindDoc`, `wrapTight`/`wrapThrough` read as square (with a
  note), `simplePos`, the gap from `dist*`. `.odt`: a `draw:frame` anchored to
  its paragraph, a character or the page with a graphic style: `style:wrap`
  (none, run-through foreground or background, the rest square),
  horizontal and vertical position and relation, `svg:x`/`svg:y`, the gap from
  the frame's margins, through parent styles. Vertical alignments (centre,
  bottom) become offsets.
- **Not yet:** dragging a picture to move it or resizing it by its handles;
  wrapping tight to a picture's outline; text in a cell going round a float;
  a float placed above the paragraph that anchors it pushing earlier text on
  its page aside; floats in headers, footers and notes.

Gates: layout and paginate tests (square, top and bottom, behind and in front
on one page; a page-relative picture where the page says and one moved up to
fit; 300 random edits with pictures floated, moved and removed, text typed,
paragraphs split and joined, the same as a fresh pagination, including a
wrapped paragraph carried on to the next page), an editor test (wrap, move,
click, caret beside it and below it, undo), native round trip, `tests/
floats.rs` (five floats through `.docx` and `.odt`). LibreOffice both ways
and across formats (`examples/floats_lo.rs`): every picture's paragraph,
frame, wrap, place and gap within 2 twips, and the text: 8/8; the sample page
(`atrium-write/examples/floats_docs.rs`) laid out by LibreOffice from our
`.docx` puts each picture and the lines round it where we do. Corpus: 86
files with floating pictures (133) paginate with every picture placed, the
slowest in 21 ms; recall 99.51 % and merge 1,742 exact, 1,832 safe,
unchanged.

### 16.31 O5f as built (2026-11-15): dynamic arrays, `LET`, `LAMBDA`, conditional formats

- **Spill.** A formula whose result is an array spills it from its cell, as
  Excel 365 does. After each recalculation pass the anchors that produced an
  array claim their area; an area that meets a value, another formula or
  another spill gives `#SPILL!` in the anchor and nothing outside it, and a
  claim that frees up spills on the next round (at most eight rounds, so
  chains of spills settle). Spilled cells are values the anchor owns: typing in
  one blocks the spill, clearing it lets it back. `A1#` is the anchor's
  current spill (`#REF!` if there is none); `@x` is the old implicit
  intersection. New errors `#SPILL!` and `#CALC!` (an empty `FILTER`, a
  lambda left in a cell).
- **Old files, new meaning.** A formula from a file that predates spilling
  (an `.xlsx` cell formula without `cm`, an OpenFormula cell that is not a
  matrix) is read with `@` put on the references and calls that would spill
  now (`=A1:A5*2` reads as `=@A1:A5*2`), so it shows what it showed; writing
  takes the `@` back off where it was implied. A dynamic formula is written
  as Excel writes it: `<f t="array" ref="area">` with `cm="1"` and the
  `XLDAPR` cell metadata part, `_xlfn.ANCHORARRAY` for `A1#`,
  `_xlfn.SINGLE` for an `@` Excel would keep, `_xlfn._xlws.` on `FILTER` and
  `SORT`, `_xlpm.` on `LET` and `LAMBDA` names. In `.ods` it is a matrix
  formula over its current spill, spill references become the range they
  cover, and Excel's newer functions get `COM.MICROSOFT.` names.
- **Functions.** `SEQUENCE`, `RANDARRAY`, `TRANSPOSE`, `FILTER`, `SORT`,
  `SORTBY`, `UNIQUE`, `VSTACK`, `HSTACK`, `TAKE`, `DROP`, `CHOOSEROWS`,
  `CHOOSECOLS`, `TOCOL`, `TOROW`, `WRAPROWS`, `WRAPCOLS`, `EXPAND`; `ROW` and
  `COLUMN` of a range give arrays; scalar functions lift over array arguments
  element by element (`#N/A` past the end of the shorter one). `LET` binds
  names in order; `LAMBDA` makes a value that can be called in place
  (`=LAMBDA(x,x*2)(5)`), named (a defined name holding one is called like a
  function, recursion included, 64 calls deep before `#LIMIT!`) or passed to
  `MAP`, `REDUCE`, `SCAN`, `BYROW`, `BYCOL`, `MAKEARRAY`; `ISOMITTED` tests a
  parameter left out. A named lambda's reads are dependencies of the cells
  calling it.
- **Conditional formats** (`officina-grid` `condfmt`). A format covers ranges
  on one sheet and holds rules in priority order: cell value compared
  (between, equal, greater…), a formula true for the cell (written for the
  first cell; relative references move), text contains / doesn't / begins /
  ends (any case), blanks and non-blanks, errors and non-errors, top or bottom
  N or N %, above or below the average (or N standard deviations from it),
  duplicates or uniques, two- and three-colour scales, data bars. Each rule
  that matches sets what its format sets (font colour, bold, italic,
  underline, strike, fill, number format) unless an earlier one set it; a
  rule with stop-if-true ends the list for that cell. Statistics (top N,
  averages, duplicates, scale and bar ends) are worked out once per
  recalculation. Sheets draws the fill or scale colour, font colour, weight
  and number format, and the data bar under the text (a tenth of the cell at
  the low end, as Excel 2007 draws it).
- **Files.** `.xlsx`: `<conditionalFormatting>` with every `cfRule` type
  above and its `dxf`s in the styles part, read in Excel's priority order
  across formats; Excel 2010's `x14` extension (repeating data bars with
  their newer options) is noted and the 2007 rule kept. `.ods`:
  LibreOffice's `calcext:conditional-formats` (conditions naming generated
  `CF` cell styles in `office:styles`, colour scales, data bars) both ways.
  LibreOffice applies only the first condition of a format that matches, so
  conditions read from `.ods` stop, and overlapping rules with different
  formats lose the later ones there; blanks and standard-deviation rules go
  as the formulas Excel itself uses. Icon sets and date rules are noted and
  dropped. Native file: its own block.
- **Not yet:** a dialog for making and editing rules (the engine and files
  have them; the app only draws them); icon sets, date-occurring rules,
  negative data bars and axes; spill outlines drawn on the grid.

Gates: `officina-grid` `tests/arrays.rs` (80 Excel spill cases with the
results Excel 365 shows, blocking and unblocking, chained spills, legacy
reading, named and recursive lambdas, native round trip) and `tests/
condfmt.rs` (each rule kind picks the cells Excel picks; stop; statistics
follow edits; scales and bars; native round trip); `officina-convert`
`tests/arrays.rs` (dynamic formulas written as Excel writes them, a sheet as
Excel 365 saves it, `.ods` matrices) and `tests/condfmt.rs` (every rule kind
through `.xlsx`, rules as Excel 2016 saves them showing what Excel shows,
`.ods` round trip). LibreOffice (`examples/condfmt_lo.rs`): our `.ods`
through LibreOffice 24.2 and back 40/40 cells look the same; our `.xlsx`
37/40 (LibreOffice drops stop-if-true); `ods_lo` and `lo_check` unchanged at
152/152 (LibreOffice 24.2 writes Excel's newer function names in lower-case
`com.microsoft.` form into `.xlsx`; we read it). The scale gate's full
recalculation of 1 M formulas: 324 → 346 ms (within its noise).

### 16.32 O5g as built (2026-11-15): pivot tables

- **Model** (`officina-grid` `pivot`). A pivot has a source range whose first
  row names its fields, a place for its output, fields down the rows, across
  the columns and as page filters (each with the items it leaves out), value
  fields (sum, count, average, max, min, product, count of numbers, standard
  deviation and variance, sample and population; a caption of their own or
  "Sum of Units"), grand totals for rows and columns and subtotals on or off.
  Items group as Excel groups them: text in any case is one item, shown as first
  met, in Excel's order (numbers, text, FALSE/TRUE, errors, "(blank)").
- **Refresh.** The output is ordinary cells, written when the pivot is made,
  changed or refreshed, as in Excel: editing the source doesn't move it until
  a refresh. One scan of the source sums each row into its leaf (one hash
  lookup a row), then leaves roll up into every subtotal they belong to: a
  1,000,000-row source with two row fields and one column field refreshes in
  337 ms (`examples/pivot_scale.rs`, release). The layout is Excel's tabular
  form: a column per row field, outer items on the first row of their group,
  "<item> Total" rows after each outer group, "Grand Total" row and column,
  column fields as header rows (with a "Values" field innermost when there are
  several value fields), page fields with "(All)", the item, or "(Multiple
  Items)" above. A refresh that would cover cells it doesn't own, another
  pivot or its own source is refused and the old output stays. Each pivot's
  shape (header rows, label columns, total rows and columns) says what part a
  cell is, for drawing.
- **Editing.** `pivot_op` makes adding, changing, refreshing or removing a
  pivot one undoable, journaled step (`GridOp::Pivots` beside the `Replace`s
  of its cells). Sheets' Data menu: Insert Pivot Table (the cursor's column,
  or the first text column, down the rows, up to three number columns summed,
  placed two columns right of the data), "Pivot <field> Across the Columns"
  and back from a cell of the source, Refresh, Delete, Refresh All. Header and
  total rows draw bold on a light band, as Excel's default pivot style.
- **Files.** `.xlsx`: a pivot cache definition per pivot (source range, each
  field's items; no records, `saveData="0"` and `refreshOnLoad="1"` so Excel
  rebuilds it on opening) and the pivot table definition in tabular form
  (`compact="0" outline="0"`), with hidden items, page fields, value fields'
  functions and captions, grand totals, subtotals; the workbook's
  `pivotCaches`. Read: sources on a sheet or a table's name, whole columns
  cut to the rows in use; compact-form pivots, values shown as percentages,
  and values down the rows are noted (shown in tabular form, plain values,
  across, once refreshed); calculated fields keep the pivot as values. The
  cells a file holds stay as they are until a refresh. `.ods`: LibreOffice's
  `table:data-pilot-tables` both ways (tabular layout, members shown or not,
  selected page, data layout field, `tableooo:display-name`, grand totals);
  groups and "show as" are noted, database and external sources keep the
  cells only. Native file: its own block, with each output's place.
- **Not yet:** a field-list pane (drag fields between axes, pick functions and
  items); compact and outline layouts; grouping dates and numbers; values shown
  as percentages; calculated fields; GETPIVOTDATA; pivots following inserted
  rows and columns; pivot charts.

Gates: `officina-grid` `tests/pivot.rs` (the cells Excel's tabular form shows
for two row fields and a column field, several value fields with and without
column fields, page filters and hidden items, refresh after edits, refusals,
undo and the journal, native round trip); `officina-convert` `tests/pivot.rs`
(the parts as Excel writes them, both formats round trip with the same
definitions, shapes and cells); `atrium-sheets` (insert, refresh, delete,
undo from the Data menu). LibreOffice 24.2 both ways (`examples/pivot_lo.rs`,
three pivots: rows and columns with a hidden item; a page filter with two
value fields; counts without the grand total column): `.ods`→`.ods` and our
`.xlsx`→LibreOffice's `.ods` 3/3 the same definitions and, refreshed, the same
cells; LibreOffice's own `.xlsx` export keeps 2/3 (it doesn't write
`rowGrandTotals="0"`).

### 16.33 O6 plan (2026-11-15): `atrium-slides`

O6 builds the slides app (§9.1, §9.3) in steps, each ending with a gate:

| step | what | gate |
|---|---|---|
| O6a | `officina-slides`, headless: a deck (slide size, theme, masters, layouts, slides), shapes (preset geometry, lines, pictures, groups, text frames on the `officina-text` engine), placeholders inherited slide → layout → master, notes, transitions and builds as data; ops with exact inverses; the `.oslides` native file with one chunk per slide and one content-addressed chunk per picture or video, read lazily; a slide drawn as a display list | random ops undo and redo to the same deck; native round trip; a 1,000-slide deck opens reading only the index, theme, masters, layouts and the first slide |
| O6b | `atrium-slides`: the slide on a Pergola `Canvas` with selection, move and resize handles, snapping guides, align and group; text editing in frames with the rich-text editor; the slide strip as a `VirtualList` of thumbnails cached by content hash; new slide from a layout; picker open, Save As, autosave journal | VM screenshots; crash-recovery test |
| O6c | `.pptx` and `.odp` read and write (masters, layouts, placeholders, theme colours and fonts, preset shapes, pictures, groups, tables as shapes, notes, transitions, simple builds; the rest kept or reported) | LibreOffice both ways on generated decks; the corpus's decks open with their slide count and text |
| O6d | presenting: full screen on one monitor, presenter view (notes, next slide, timer, sorter) on another; transitions and builds as declarative animations run by frescod between resident scene groups; video through Limen's `media-player` | two-monitor VM gate; a transition with no missed frame at 60 Hz |
| O6e | very large decks: thumbnails stored in the file, pictures decoded at the drawn size, current, next and previous slides resident while presenting, the rehearse pass, Reduce File Size | the §9.3 gates on a generated 1,000-slide, 2 GB deck with 1,500 photos |

**O6a design.** Geometry is in EMU (914,400 to the inch, 12,700 to the point), the
unit both formats can say exactly (ODF's centimetres are 360,000 EMU), so a round trip
moves nothing; text inside a frame is laid out in `officina-text` layout units. A text
frame holds a small `TextDoc`, so a frame's text has the same runs, styles, lists and
layout cache as a page of Write, and editing it is a `TextOp` wrapped in a deck op
naming the slide and shape. A placeholder on a slide names its kind and index; what it
leaves unset (position, size, fill, text style) comes from the layout's placeholder of
the same kind and index, then the master's, as both formats do. Colours are RGB or a
theme slot with luminance changes, resolved when drawn, so changing the theme recolours
the deck. Every slide, master and layout carries an id that never changes, so the op
log, thumbnails and the native file's chunks name it across moves.

### 16.34 O6a as built (2026-11-15): the deck model

- **`officina-slides`** (headless, depends on `officina-core` and `officina-text`).
  A deck is a slide size, a theme (twelve colour slots, heading and body fonts),
  masters, layouts and slides. Shapes are preset geometry by OOXML name with its
  adjust values (21 presets drawn; any other drawn as its box, reported, kept by
  name), custom paths, pictures with crop, videos with a poster, and groups with
  their own child coordinates. Fills are none, solid, linear or radial gradient,
  or a picture; outlines have dash and arrow ends. Colours are RGB or a theme
  slot with OOXML's changes (luminance, tint, shade, saturation, alpha), resolved
  when drawn, so a theme change recolours the deck.
- **Text frames** hold a small `TextDoc`: a paragraph's style id is its outline
  level, runs carry direct formatting, `+mj-lt`/`+mn-lt` name the theme's fonts.
  Level styles (character, paragraph, bullet: a character or a numbering scheme)
  come from the master's title, body or other style, then each placeholder in the
  chain, then the frame's own. Drawing turns them into the paragraphs' own
  formatting and lays the frame out with `officina-text`'s paragraph layout, so a
  frame shapes and breaks lines exactly as Write does; bullets sit in the hang,
  numbers count by level, anchors top, middle or bottom, shrink-on-overflow
  scales the text, and `‹#›` shows the slide's number.
- **Placeholders.** A slide placeholder takes what it leaves unset (frame,
  geometry, fill, outline, insets, anchor, autofit, wrap, level styles) from the
  layout placeholder with its index, else its kind, and that one from the
  master's of the same kind. New decks get PowerPoint's default 16:9 master and
  six layouts (Title Slide, Title and Content, Section Header, Two Content, Title
  Only, Blank); `new_slide` makes a slide with the layout's empty placeholders.
- **Ops.** `DeckOp`: insert, remove and move slides; slide settings (layout,
  background, notes, transition, builds, hidden); insert, remove, replace and
  reorder shapes on a slide, layout or master, inside groups too; a `TextOp` on a
  frame or the notes; theme, size, masters and layouts. Each returns its exact
  inverse; ids are never reused while a deck is open.
- **Drawing.** `draw_slide` gives a display list in EMU (background, master and
  layout decorations unless hidden, the slide's shapes): paths with paint and
  stroke, pictures, video posters and text items, each with the affine transform
  that puts it on the slide (rotation about the middle, flips, group mapping).
  Editing, thumbnails, presenting and PDF will all draw from it.
- **Native `.oslides`.** One chunk per slide, media in pieces of at most 16 MiB
  stored once whatever uses them (unused pieces are dropped on save), masters
  and layouts in one chunk, and a deck chunk with the theme, the slide order and
  the media table. Opening reads only the index, the deck and the masters;
  slides and pictures are read when first needed, a slide read and unchanged can
  be dropped back to the file, and saving copies unread slides and pieces
  without decoding them. A damaged slide or piece fails alone. The journal
  records each op; a text edit is recorded as the frame it left.
- **Not yet:** tables (O6c reads them as groups of shapes), charts on slides,
  connectors following their shapes, text on a path, 3-D and effects (shadows,
  glow); slide-number and date fields are a text marker only.

Gates (`officina-slides` `tests/deck.rs`): inheritance through layout and master;
theme colours as PowerPoint shows them; drawing order, bullets and text placement;
groups' mapping; 750 random ops over five seeds undo to the first deck and redo to
the last; 200 random ops journaled and replayed on the file's copy give the same
deck; native round trip byte for byte, with a picture used on 20 slides stored
once and a 16 MiB + video in two pieces; one flipped byte in a slide fails that
slide only, and truncated and corrupted files never panic; a 1,000-slide deck
opens and draws its first slide reading one slide (debug build, well under 1 s).

### 16.35 O6b as built (2026-11-15): the `atrium-slides` app

- **Window.** A toolbar (Open, Save, Save As, Undo, Redo; Slide menu with a new
  slide on any of the deck's layouts, Duplicate, Delete; Text Box; Shape menu
  with eight presets; Picture; Arrange menu), the slide strip at the left and the
  slide fitted on the canvas, a status line. The canvas and the strip draw from
  the same `officina-slides` display lists, cached per slide until an op touches
  it; the strip draws only the thumbnails in view, so a 1,000-slide deck costs a
  few, and slides more than 24 away from the one shown go back to the file.
- **Shapes.** A press selects the topmost shape; handles resize it; dragging
  moves it with snapping (6 px) to the slide's edges and middle and to the
  other shapes' edges and middles, with the guides that held drawn. A drag on
  the empty slide sweeps a box and selects what lies wholly inside; Select All
  takes the slide. Several selected shapes move together, align (left, centre,
  right, top, middle, bottom: to the slide for one shape, to their box for
  several), group and ungroup (children keep their places), delete, and bring
  forward or send back. Arrows nudge by a point, ten with Shift. A drag is shown
  as it goes outside undo and becomes one undo step at the end.
- **Text.** A second press, a double press or Enter on a text shape types into it;
  an empty placeholder takes typing on the first press and shows its prompt
  ("Click to add title") until then. Typing, Enter, Tab and Shift+Tab (outline
  level), Backspace and Delete, arrows, Home and End, drag and double-press
  selection, Ctrl+B/I/U, cut, copy and paste are frame ops on the `officina-text`
  engine, grouped into one undo step per frame as in Write. Caret and selection
  come from the frame's own layout, so they sit where the text is drawn.
- **Files.** Picker Open (`.oslides`; PowerPoint and OpenDocument decks say they
  come with O6c), Save over the picked file, Save As into a picked folder (a new
  name, never replacing), Picture from the picker (PNG, JPEG, GIF, WebP, SVG; up
  to 60 MB), sized to fit the slide. Pictures decode at their drawn size into a
  256 MB cache shared by the canvas and the strip.
- **Autosave.** As in Sheets: a base `.oslides` and a journal per document; every
  action is a record of deck ops taken as they applied, plus undo, redo and saves;
  a picture's bytes go in the journal before the shape that shows it. A deck that
  did not close cleanly comes back at the next start with its undo steps.
- **Gates.** The crash test (typing, shapes, a picture, undo, then the process
  gone: the deck, its media and its undo history come back) and the app tests
  (toolbar commands, select and drag as one undo step, sweep, align, move
  together, group and ungroup and undo each) pass; `scripts/slides-vm.sh` takes a
  screenshot of a deck on the cloud VM.
- **Not yet.** Shift-click to add to a selection (pointer events carry no
  modifiers yet), rotating by handle, turned text drawn turned (glyph runs are
  placed upright at their turned origin), the rich-text toolbar for frames
  (fonts, sizes, colours), notes editing in the window, thumbnails stored in the
  file (O6e).

### 16.36 O6c as built (2026-11-15): `.pptx` and `.odp`

- **`.pptx` in** (`officina-convert::pptx`). Presentation, masters with their
  colour maps and text styles, layouts, slides, notes slides, theme colours,
  fonts and line styles. Shapes keep their preset name and adjust values, custom
  outlines (arcs included) become paths, colours keep their theme slot and
  changes, placeholders their kind and index; style references (`p:style`) are
  resolved into fills, lines and text colour. Pictures and videos are read once
  however many shapes use them; tables come in as groups of cells, SmartArt as
  the shapes it was drawn with (or its text when the drawing is empty), charts
  as their fallback picture. Paragraph bullets that differ from their level's
  are a frame's own (`TextBody.bullets`, new in the model and the native file).
  Transitions and the main sequence of builds map to the model's small sets.
- **`.pptx` out.** Masters, layouts and slides keep their placeholders, level
  styles, theme colours and presets by name; notes go in notes slides under one
  notes master; builds as a timing tree; the first master's other style is the
  presentation's default text style (so other readers draw shape text as we do).
- **`.odp` in and out** (`officina-convert::odp`). ODF styles are followed
  through their parents and family defaults, and text comes in with what its
  styles give it made direct. Each master page is a master with one layout, its
  `-title` and `-outline1…9` presentation styles its level styles. Custom shapes
  LibreOffice writes for OOXML presets (`ooxml-…`) keep the preset, ODF's own
  types map to presets where one exists, and other outlines are drawn from their
  enhanced geometry (equations, modifiers, every path command); `svg:d` paths and
  point lists likewise. Out, each layout becomes a master page holding its
  master's shapes and its own; shapes are written resolved through their
  placeholder chain (geometry, fill, line, insets, level styles), presets as
  `ooxml-…` custom shapes with their outline drawn, lists nested by level with
  their bullets and indents as LibreOffice writes them, transitions in the page
  style, builds as an animation tree with LibreOffice's preset names.
- **Not kept** (reported): charts and SmartArt as live objects, tables as tables,
  emphasis and motion builds, hatches and tiled pictures (drawn solid and
  stretched), connectors' bends, picture crops in `.odp`, comments, macros.
- **Opening.** The converter process takes `pptx` and `odp` and writes
  `.oslides`; Slides opens both through it with the fidelity notes on the status
  line and saves back in the file's own format.
- **Gates.** LibreOffice both ways (`examples/slides_lo.rs`): a deck with levels
  and a numbered paragraph, a turned gradient box, an ellipse, a dashed arrow, a
  star, a picture, a group, a custom outline, a bold coloured run, notes, a
  hidden slide, transitions and builds, written as `.pptx` and `.odp`, through
  LibreOffice to both formats and read back: 84/84 checks, and LibreOffice draws
  our two files alike. Corpus (`examples/corpus_slides.rs`, files kept out of the
  repository): 519 `.pptx` from LibreOffice's and POI's test data, 505 read, 14
  refused (damaged ZIPs), 0 panics, slide count equal to LibreOffice's on all
  504 compared, 99.17 % of LibreOffice's words; 178 `.odp`, 177 read, slide count
  equal on 176/176, 97.04 % of the words (the rest: date and file-name fields
  LibreOffice fills in when it opens a file, comments, text on a picture). Every
  readable deck of both sets comes back from our own writer with the same slides
  and words, and so does it through the other format. Unit tests: round trips,
  `.odp`→`.pptx`→`.odp`, 3,000 mangled packages, looping equations and absurd
  geometry.

### 16.37 O6d as built (2026-11-15): presenting

- **Declarative animation in frescod** (pergola.md §4.2). `OP_ANIMATION_START`
  takes up to 64 tracks on a window's groups (transform, opacity or clip, each
  with a delay, a duration up to 60 s, an easing curve or `STEP`); while a track
  runs its value stands in for the group's own, so the app sets every group to
  its end state in the frame the tracks start with. `OP_ANIMATION_CANCEL` ends
  one at once; `EV_ANIMATION_FINISHED` reports the frames composed and the worst
  gap between them. frescod (and each split renderer, on its own clock) composes
  at the display's rate while anything animates; a group at opacity 0 is skipped
  in packing, so resident slides not showing cost nothing.
- **Monitors.** `OP_DISPLAY_OUTPUTS` lists them (identity, desktop rect, scale,
  refresh, internal, primary); `WindowHints.full_screen_on` puts a window full
  screen on one, which forum-wm honours (a new window avoids a monitor so
  taken). Headless frescod takes `FRESCOD_HEADLESS_OUTPUTS=WxH,WxH` side by side.
- **The show** (`officina-slides::show`, `atrium-slides::present`). Builds fall
  into steps (with-previous and after-previous timed from the step's start);
  F5 presents from the first slide, Shift+F5 from the current one, the Present
  button likewise. The slideshow goes full screen on an external monitor that is
  not the first, the presenter view on the first (current slide as built, the
  next press, notes, elapsed time, clock, buttons, a sorter to jump to any slide,
  hidden ones marked); with one monitor, the slideshow alone. Right, Down,
  Space, Page Down, N, Enter and a click move on; Left, Up, Page Up, P,
  Backspace go back a step at once; a number and Enter jumps; Home, End; B and W
  blank the screen; Esc ends. Hidden slides are skipped except by a jump; past
  the last slide a black end screen.
- **Transitions and builds as tracks.** The current, next and previous slides
  are resident, each a group, each moved shape a group inside it. Fade,
  dissolve, push, cover, uncover, wipe, split (in and out) and zoom (in and out)
  are tracks on the two slides (the old one holds at full opacity under a `STEP`
  track, then shows its own state, hidden); appear, fade, fly in, wipe, zoom,
  disappear and fade out likewise on the shapes, the slide's first step after
  the transition. A press during an animation cancels it to its end state. A
  transition's `after_ms` moves on by itself.
- **Video** through Limen's `media-player`: once the slide has arrived, up to
  two players go over its videos' boxes, handed the file as an unlinked temp
  file, and play; leaving the slide detaches them.
- **Fixes the gate found.** Each Pergola surface takes its own glyph-atlas slot
  (two windows of one app drew with each other's atlas); a text node taking over
  an id a fill had drops the fill (frescod kept drawing it).
- **Gate** (`scripts/slides-present-vm.sh`, deck from `examples/show_deck.rs`:
  every transition, five build effects, notes, a hidden slide). Headless frescod
  with two 1280×720 outputs at 60 Hz: the slideshow full screen on the second,
  the presenter view on the first, the show pressed through every step to the
  end screen with every build and transition drawn as specified and the
  presenter view matching. Smoothness: under TCG in the cloud container a frame
  takes about a second, so every animation shows 2–3 frames (worst gap 1.5 s);
  that is the emulator. On the Mac VM (HVF, 4 CPUs, headless frescod with
  its CPU renderer, both outputs 2560×720 in all) the show runs the same but
  **fails 60 Hz**: frescod composes 8–13 frames a second through a transition
  (about 60 ms of CPU a frame), worst gaps 50–130 ms; the app itself used
  0.09 s of CPU in the whole show, so the motion never waits on it. Split mode
  first showed about 5 Hz on screen because frescod did not know the renderer
  was animating (fixed: `FrameReady` carries an animating flag). Still to do:
  the figure on the GPU renderer, and the CPU renderer's cost per animated
  frame. Unit tests: monitor choice, letterboxing,
  transition and build tracks with their times, keys.

### 16.38 Presenting speed on the CPU renderer (2026-11-15)

The compute renderer (frescod in-process and `fresco-renderer` in split
mode) now draws animated groups as **sprites** (`frescod/src/sprites.rs`,
`pack.rs` promotion):

- A group that an animation moves, fades, clips or uniformly scales becomes a
  sprite: its content is rastered once into its own buffer at its own scale,
  then each frame is composited (`fresco_compute::compose::over`, or
  `over_scaled` bilinear for zooms). The window under the sprites is a
  separate buffer. Each frame rebuilds only the rect that changed.
- Slides stay sprites for 30 s after an animation. A hidden clipped group
  (the next slide) is a sprite too. It is rastered ahead while nothing moves,
  400 tiles per drawing, and frescod draws again every 8 ms until it is done.
- The outgoing slide is taken from the window's last drawing (checked against
  12 sample tiles), not rastered.
- The window under a slide is rastered only where it shows. Tiles under an
  opaque sprite wait for idle time.
- Sprite tiles that no op touches are cleared, not rastered.
- A packed part carries only the textures it draws.
- A group whose content leaves the window in its own space is not a sprite;
  it is drawn with the window, and the others stay sprites.
- Screen compose copies opaque pixels on every thread and blends only where
  needed.
- An unchanged window is neither packed nor rastered.

Mac VM (HVF, CPU renderer, 2×1280×720), the same show, worst frame gap:

| | before | after |
|---|---|---|
| in-process | 130–136 ms, median frame ~60 ms | 84–99 ms, median render 1.6–2.2 ms |
| split mode | — | 80–90 ms; 7 of 10 animations at 20–31 ms |

Still **not 60 Hz** on that CPU. The frames that miss are:

- the presenter window's slide change, which redraws the whole window
  (45–59 ms) because two of its groups (the next-slide preview's items) draw
  outside the window in their own space;
- the first frame of some builds, which raster a new sprite of 1,600–2,000
  tiles (17–47 ms) because a build item is not rastered ahead (only hidden
  clipped groups are).

Next steps: content buffers with a margin, so those groups can be sprites; and
rastering ahead hidden build items. The GPU renderer figure is still to take.
`FRESCOD_RENDER_TIMES=1` prints per-frame and per-sprite costs from both
frescod and `fresco-renderer`.

### 16.39 O6e as built (2026-11-15): very large decks

- **Previews in the file.** Each picture can carry a preview: a JPEG (quality
  82), or a PNG when the picture has transparency, at most 512 px on its longer
  side. Previews are stored as `PREVIEW` chunks plus an optional `PREVIEWS`
  table, keyed by the picture's content hash. They are kept with their
  pictures, dropped with them, and copied unread on save. A damaged table only
  costs the previews.
  - Slides makes previews on worker threads for pictures that lack one, a few
    at a time, behind any decode for the screen. They go into the file at the
    next save.
- **Opening maps the file.** A native deck opens from its descriptor with
  `mmap`, reading only the index, the deck and the masters. Slides and
  pictures are paged in as they are drawn.
  - Slides never writes over the file a deck is mapped from. A save streams
    the deck to a new file in the autosave folder, maps the deck from it,
    copies it over the target, and makes it the autosave base with an empty
    journal (`Doc::spool`, `Doc::saved_as`). Save As copies the same way.
  - The first autosave base is written on a thread. Until it is in place, a
    crash loses the first edits.
  - `.pptx` and `.odp` are still read and written in memory.
- **Pictures decode off the window's thread** (`decode.rs`: up to 4 workers,
  one core left for the window). A picture whose preview is big enough
  (thumbnails, the sorter, the presenter's next slide) draws from the preview.
  When the drawn size needs more, the preview shows at once and the full
  decode, at the drawn size, comes back from a worker. The editor and the show
  then paint again; the show waits until no animation runs. A picture with no
  preview decodes as it is drawn, as before.
- **Presenting.** The current, next and previous slides were already resident
  (O6d). The next slide now appears from previews and sharpens within about
  0.1 s.
- **Rehearse Show** (Slide menu) runs on a copy of the deck on a thread. It
  decodes every shown slide's pictures one at a time at 1920 × 1080, letting
  slides go as it goes. It names the slides over 750 ms ("may not be ready if
  you click through quickly") and the slowest picture's size.
- **Reduce File Size…** (Slide menu) asks for a folder, then works on a copy of
  the deck on a thread. Each picture is recompressed to the largest size it is
  shown at on a 1920 px screen (JPEG 85, PNG with transparency), and the
  smaller of the two is kept. SVG, GIF and pictures already near their shown
  size are kept, and so are videos (trimming them is not done). The result is
  saved as "Name (smaller).oslides" with a report of the saving.

**Gates** (`atrium-slides/examples/big_deck.rs`). The test deck is generated:
1,000 slides and 1,500 distinct 6 MP photos with their previews, 1.99 GB, kept
in scratch. Run natively on the cloud container (4 cores):

| gate | result |
|---|---|
| open and draw the first slide at 1920 × 1080 (under 1 s) | 188 ms cold, 23 ms warm |
| every thumbnail as the strip scrolls (each under a 60 Hz frame) | median 1.7 ms, p99 3.1 ms, worst 3.8 ms |
| presenter: next slide drawn from previews (under a frame) | median 7.0 ms, p99 11.0 ms, worst 14.8 ms |
| presenter: the next slide sharp | median 88 ms, p99 132 ms, worst 172 ms |
| Rehearse | 112 s; every slide ready in time (slowest 215 ms) |
| memory (under 500 MB) | peak anonymous 464 MB |

On memory: peak RSS is 2.3 GB once Rehearse has read every picture, but
1.9 GB of that is the mapped file's pages. Those are shared page cache that
the system takes back under pressure.

The unit tests (21 in `atrium-slides`, plus `officina-slides`) cover:
previews round trip and damage; save streaming then becoming the base and
recovering after a crash; preview first, then sharp; preview sizes; Rehearse
skipping hidden slides and letting slides go; Reduce shrinking to the shown
size and doing nothing a second time.

**Not yet.** Previews of the video poster are made like any picture, but video
trimming is not done. Previews are made only while the editor runs, not during
a show. The 60 Hz transition figure stays as in §16.38.

### 16.40 O7 plan (2026-11-15)

O7 is the features that lean on other parts of the system (§11, §17 D8, D10). The steps
that need nothing new from elsewhere go first:

| step | what | gate |
|---|---|---|
| O7a | password-protected documents: OOXML (the compound file, Agile and Standard encryption) and ODF (AES-CBC with PBKDF2, AES-GCM with Argon2id) opened in the converter jail and written with a password; the password asked for in the apps; native files encrypted with a random key wrapped by a password (Arca-wrapped keys once Arca offers `arca_decrypt` to apps) | LibreOffice both ways with passwords, all three apps' formats; wrong password refused; hostile containers |
| O7b | `.xls` import (BIFF8 in a compound file: cells, formulas, shared strings, number formats, styles, names, several sheets) | LibreOffice's reading of generated and corpus files: values and formulas equal |
| O7c | `.doc` import (Word 97–2003: text, paragraph and character formatting, lists, tables, pictures, notes, headers and footers) | LibreOffice's text and paragraph structure on corpus files |
| O7d | `.ppt` import (PowerPoint 97–2003: slides, masters, text, shapes, pictures, notes) | LibreOffice's slide count and text on corpus files |
| O7e | signatures: ODF and OOXML signatures verified and shown; signing through Sigillum | waits on Sigillum's signing API |
| O7f | external data through Nuntius (`WEBSERVICE`, web queries, linked workbooks, remote pictures), off by default per document | waits on a per-document Nuntius instance |
| O7g | IME-dependent scripts in the text engine | waits on the input-method framework (§17 D10, Tactus/Pergola) |

Legacy formats are import only (D8): `.doc`, `.xls` and `.ppt` open through the
converter and save as the current formats.

### 16.41 O7a as built (2026-11-15): password-protected documents

- **OOXML** (`cfb.rs`, `crypt.rs`). A bounded compound-file reader (DIFAT,
  FAT, directory walk, mini stream, every offset and chain checked) and
  writer. MS-OFFCRYPTO Agile decryption (any of SHA-1/256/384/512, AES-128 to
  256, spun key, 4096-byte segments, HMAC data integrity checked before
  anything is returned) and Standard decryption (SHA-1 50,000 spins,
  CryptDeriveKey, AES-ECB), which is what LibreOffice writes. We write Agile
  AES-256/SHA-512 with 100,000 spins.
- **ODF** (`odfcrypt.rs`). Per-part encryption (every ODF writer and
  LibreOffice's default): AES-256-CBC or AES-GCM, PBKDF2-HMAC-SHA1 or
  Argon2id over a SHA-1/SHA-256 start key, the SHA-256 or SHA-1 checksum of
  the first 1 KiB telling a wrong password. LibreOffice's newest mode (whole
  package in one `encrypted-package` entry, AES-256-GCM with Argon2id) is read
  too; it puts the GCM nonce in front of the ciphertext rather than using the
  manifest's IV. We write per-part AES-256-CBC with PBKDF2 (100,000
  iterations, SHA-256 start key), which every ODF reader opens. Blowfish (OOo
  1–2) is refused as unsupported. Costs a file asks for are capped (PBKDF2
  10 M iterations, Argon2 64 passes and 1 GiB).
- **Decryption runs in the converter**, after it has sandboxed itself
  (`launch.rs`). The app passes the password on stdin before the file
  (`officina-convert KIND --password`); the converter answers
  `needs-password` or `wrong-password` distinctly, so the app can ask (again).
  The app never parses the encrypted container. Encryption on save runs in
  the app, over bytes our own writers made.
- **Native files with a password** (`sealed.rs`): a random 256-bit data key
  seals the native bytes in 1 MiB AES-256-GCM segments (nonce = a per-save
  random file id and the segment index; the header's hash is the additional
  data, so truncation, reordering and header edits fail). The data key is
  wrapped by an Argon2id key from the password (3 passes, 64 MiB, 4 lanes).
  Changing the password rewraps the key. Arca-wrapped keys replace the
  password when Arca offers `arca_decrypt` to apps.
- **In the apps.** Write (Tools ▸ Password…), Sheets (Password…) and Slides
  (Slide ▸ Password…) set, change or remove a document's password. Opening a
  protected file shows a password bar (a secret field, Open and Cancel) and the
  file waits for it; a wrong password asks again. A protected document saves
  protected in every format that can carry a password (plain text and CSV
  can't, and say so); PDF export is not protected.
  - **The autosave journal of a protected document is sealed** with the same
    key: the base as a sealed file, each record sealed on its own. A crash
    restore asks for the password before it shows anything. Setting or
    removing a password restarts the journal (the old files are unlinked,
    not overwritten).
  - A sealed deck is read into memory, never mapped. A protected deck's save
    seals through the spool and the deck stops reading the file it may be
    saved over. Reduce File Size seals its copy.

**Gates.** `officina-convert/examples/password_lo.rs` against LibreOffice 24.2
over UNO, for `.docx`, `.xlsx`, `.pptx`, `.odt`, `.ods` and `.odp`: ours
opened by LibreOffice with the password and refused with a wrong one;
LibreOffice's (Standard OOXML, per-part AES-CBC ODF, whole-package
AES-GCM/Argon2id ODF) opened by us with the password, refused with a wrong one
and asked for with none: **54/54**. Unit tests: compound files with 8 MB
streams (DIFAT), hostile containers mutated at random never panic, a tampered
HMAC or sealed segment is caught, absurd KDF costs are refused before any
work. Each app has a test that a protected document's autosave folder and
saved file hold none of its text, that the file opens only with the password,
and that the journal restores only with it; Write, Sheets and Slides open
protected files through the real converter process.

Open: Arca-held keys instead of passwords; protected PDF export; the legacy
binary formats' RC4 encryption comes with O7b–O7d.

### 16.42 O7b as built (2026-11-15): `.xls` import

- **Records** (`officina-convert/src/xls/biff.rs`). The `Workbook` stream of
  the compound file cut into records, `CONTINUE`s joined with their breaks
  kept, so a string crossing one reads its new flags byte (the width can
  change mid-string). Substreams nested in a sheet (embedded charts) are
  walked over. Every read is checked; a short record is an error.
- **Protected files** are decrypted by the reader, inside the converter's
  sandbox: RC4 with MD5 (Excel 97/2000) and RC4 CryptoAPI (Excel 2002 on,
  LibreOffice), key restarted every 1,024 bytes, the records MS-XLS leaves in
  the clear left alone. A file protected only against writing opens with
  Excel's default password without asking. XOR obfuscation (Excel 95) is
  refused.
- **Formulas** (`xls/formula.rs`, `xls/ftab.rs`). The parsed tokens run as a
  postfix program over a stack of strings, back to the text Excel shows:
  operators with brackets put back by precedence, the 380-entry built-in
  function table, newer and add-in functions through function 255 with a
  defined or external name (`_xlfn.` dropped), references relative or not,
  whole rows and columns, 3D references through `EXTERNSHEET` and
  `SUPBOOK`, array constants, `#REF!` forms. Shared formulas are rendered at
  each cell from their `SHRFMLA`; array formulas at their first cell with
  their area claimed as a spill; data tables keep their values. The text then
  goes through the same placing, fill grouping, recalculation and
  differential as `.xlsx` (shared with `xlsx/read.rs`). Defined names are
  read from mid-grid so their relative parts keep both signs.
- **Cells and formats.** Numbers, RK and MULRK, shared and inline strings,
  booleans and errors, formula results (strings from the `STRING` record);
  fonts (font 4 skipped as BIFF8 numbers them), number formats (`FORMAT` and
  built-ins), the palette (the file's `PALETTE` over the default 56),
  fills, the four borders with colours, alignment, wrap, indent, column
  widths and hidden columns, row heights and hidden rows, default sizes, the
  1904 date system, hidden sheets shown.
- **Reported, not carried:** merged cells, comments, hyperlinks, charts,
  pictures, shapes and form controls, conditional formats, data validation,
  frozen panes, pivot tables (their values kept), macros, chart and macro
  sheets, print areas. **Refused** in words: Excel 95 and older (BIFF5 and
  before, in a compound file or bare) and XML or HTML spreadsheets saved as
  `.xls`.
- **Sheets** opens `.xls` through the converter (`officina-convert xls`,
  `--password` as for the others). It doesn't write `.xls` (§16.40, D8): a
  document opened from one has no save target, so Save asks where to keep a
  copy and says why.

**Gates.** `officina-convert/examples/xls_lo.rs`: the conformance workbook
(152 formulas) and a sheet of formats, saved as `.xls` by LibreOffice 24.2,
plain and with a password. Our engine on LibreOffice's tokens gives Excel's
result on all 149 that LibreOffice can write to `.xls` (it saves `XLOOKUP`
twice and `ISERROR(#REF!)` as `#N/A`); ours equals LibreOffice's saved
result wherever that is Excel's; values, sheet names, 14 formatted cells,
a width and a height equal; the protected copy asks for its password,
refuses a wrong one and opens with it: **26/26**. LibreOffice's 72 `.xls`
test files (`sc/qa/unit/data/xls`): the 66 BIFF8 workbooks open, 6 are
refused in words (Excel 95, bare BIFF2–5, XML); where results differ from
the files' it is functions the engine doesn't have yet (`AVERAGEIFS`,
`SUBTOTAL`, `CODE`, `NORMSDIST`, `FREQUENCY`, `DCOUNT`, `GETPIVOTDATA`),
Excel 4 macro names and user-defined functions. 68,000 randomly mutated copies
of those files never panic (one engine crash found on the way: number format
codes with multi-byte characters near the end, fixed in `numfmt.rs`). A 65,536
× 20 `.xls` from LibreOffice opens in 0.2 s. Unit tests build a workbook
record by record, plain and RC4 CryptoAPI protected (`xls::sample`), and Sheets
opens both through the real converter.

Open: the missing functions above; merged cells and comments once the grid
has them; `.xls` charts and pictures; Excel 4 macro sheets.

### 16.43 O7c as built (2026-11-15): `.doc` import

- **Approach** (`officina-convert/src/doc/`). The reader turns a Word 97–2003
  file into WordprocessingML parts (document, styles, numbering, notes,
  comments, header and footer, settings, pictures) and hands them to the
  `.docx` reader, so one reader builds the model and gives the fidelity
  notes for both formats.
- **The binary file** (`doc/bin.rs`). The FIB (Word 97 to 2007 sizes; Word 95
  and older and Word 2 refused in words, as are RTF and HTML files named
  `.doc`), the piece table (compressed cp1252 pieces and UTF-16 ones, fast
  saves' property lists), the paragraph and character property pages, sprms
  by their size codes (incl. the long `sprmTDefTable` and `sprmPChgTabs`),
  string tables, the font table, PLCs. Every read is checked; the text is
  capped at 2^28 characters.
- **Protected files** are decrypted by the reader in the converter's sandbox:
  RC4 (Word 97/2000) and RC4 CryptoAPI (Word 2002 on, LibreOffice), on
  512-byte blocks, the FIB's first 68 bytes and the encryption header left in
  the clear, the `Data` stream all encrypted (keys shared with `.xls`). XOR
  obfuscation (Word 95 style) is refused.
- **Carried** (`doc/props.rs`, `doc/mod.rs`): styles with their bases and
  toggles, character formatting (fonts, size, bold, italic, underline,
  strike, colour by index or RGB, highlight, super/subscript, small caps,
  hidden, symbols), paragraph formatting (alignment, indents, spacing, line
  spacing, keep, page break before, tabs), lists (`PlfLst`/`PlfLfo` to
  numbering with level texts and starts), tables (rows from `sprmTDefTable`,
  widths, merged cells across, nested tables' text), sections (page size,
  margins, columns, breaks), the first section's header and footer (default
  and first page), footnotes and endnotes, bookmarks, fields (incl.
  `HYPERLINK`, which the `.docx` reader now also turns into links in Word's
  own complex-field form), inline pictures (PNG and JPEG from the `Data`
  stream's OfficeArt), text boxes anchored in the main text (their text kept
  after the paragraph, by shape id from `PlcSpaMom` to `PlcftxbxTxt`),
  comments (author, initials, range from the annotation bookmarks, date from
  `AtrdExtra`, replies where the file has them), and tracked changes
  (insertions and deletions with their own authors and dates, paragraph
  marks inserted or deleted, formatting changes; tracking on from the DOP).
- **Reported, not carried:** floating pictures and shapes, pictures in
  EMF/WMF/PICT/BMP/TIFF, embedded objects (their picture kept), text boxes in
  headers and footers, and whatever the `.docx` reader reports for the
  translated parts (paragraph borders and shading, frames, more than one
  header per section, and so on). A formatting change's old formatting isn't
  in the file: it comes back as the style's.
- **Write** opens `.doc` (and `.dot`) through the converter (`officina-convert
  doc`, `--password` as for the others). It doesn't write `.doc` (§16.40,
  D8): a document opened from one has no save target, so Save asks where to
  keep a copy and says why; a protected one's copy keeps its password.

**Gates.** `officina-convert/examples/doc_lo.rs`: docx_lo's sample (everything
the model has) saved as `.doc` by LibreOffice 24.2, plain and with a password;
our `.doc` reader meets docx_lo's checks on it (texts, styles, runs, link,
spacing, lists, table and widths, picture, header and footer, notes,
comments, superscript, tracked edits by author with accept and reject,
tracking on, page), the protected copy has no text in the clear, asks for
its password, refuses a wrong one and opens with it: **31/31** (LibreOffice's
`.doc` keeps no comment threads or resolved state, so the gate expects the
reply as a comment of its own). Corpus: Apache POI's 159 and LibreOffice's
180 `.doc` test files (`sw/qa/extras/ww8*`): 311 open, 28 refused in words
(13 Word 95 or older, 1 Word 2, 9 damaged fuzzer files, 3 needing a password
we don't have, 2 not Word); 98.3 % of the words LibreOffice's text export
shows, median file 100 %, 228 files at 99 % or more both ways. Most of the
rest is tracked deletions (LibreOffice's text export includes deleted text),
text-box text (which its export leaves out and we show), and list labels and
form fields not yet derived; our writer then reader keeps the text and notes
of all 311, compare and merge are safe on all. 81,000 randomly mutated copies
of those files (half with a password) never panic; the slowest takes 23 ms
(found on the way: a damaged FIB's story lengths made the reader fill
seconds' worth of missing text, now cut at the end of the piece table). A 30,000-paragraph (8 MB)
`.doc` from LibreOffice reads in 0.18 s. Unit tests build a document byte by
byte, plain and RC4 CryptoAPI protected (`doc::sample`, which LibreOffice
also reads), and Write opens both through the real converter.

Open: floating pictures and shapes (OfficeArt drawing to anchors), EMF/WMF
pictures, form fields' text and check boxes, legal and chapter list labels,
headers and footers per section, text boxes in headers.

### 16.44 O7d as built (2026-11-15): `.ppt` import

- **Approach** (`officina-convert/src/ppt/`). As for `.doc`, the reader turns
  a PowerPoint 97–2003 file into PresentationML parts (presentation, one
  theme, a master and a blank layout per main master, slides, notes,
  pictures) and the `.pptx` reader builds the deck from them, so both
  formats share one reader and one set of fidelity notes.
- **The file** (`ppt/rec.rs`, `ppt/file.rs`). Records with the shared
  PowerPoint/OfficeArt header; the `Current User` stream points at the last
  edit, edits chain back through their persist directories (newer entries
  win). A chain pointing at itself, forward or at no edit goes on from the
  edit before it in the stream. PowerPoint 95 and older files are refused in
  words. Protected files (RC4 CryptoAPI, as PowerPoint and LibreOffice
  write them) are decrypted in the converter's sandbox: each persist object
  with its id as the block key, header included; pictures with block 0.
- **Carried** (`ppt/mod.rs`, `ppt/text.rs`, `ppt/anim.rs`): slide size;
  masters with their colour schemes and text styles by level
  (`TextMasterStyleAtom`; spacing given as a share of the line turned into
  points from the level's size); slides and their shapes (OfficeArt preset
  types mapped to DrawingML presets, groups with child frames, rotation and
  flips, solid and gradient fills with focus and angle, line colour, width,
  dash and arrowheads, freeforms from their vertices and segments, text
  insets, anchoring and wrap); placeholders, with their text from the slide
  list (`OutlineTextRefAtom`) or the shape; text runs (alignment, indents,
  spacing, bullets with character, font, size and colour, numbered
  paragraphs from the `___PPT9` tag, bold, italic, underline, font, size,
  colour, super/subscript, hyperlinks, slide number and date fields);
  WordArt's text; pictures (PNG and JPEG from the BStore or the `Pictures`
  stream, crops); solid backgrounds; speaker notes; hidden slides; slide
  transitions (cut, fade, dissolve, push, wipe, cover, uncover, split, zoom
  with speed and automatic advance); builds from PowerPoint 2002's timing
  tree in the `___PPT10` tag (preset, class, trigger, delay, length, target)
  or, without one, PowerPoint 97's per-shape `AnimationInfoAtom`.
- **Reported, not carried:** pictures in EMF/WMF/PICT/BMP/TIFF (text drawn
  inside metafiles too), pattern and texture fills (first colour), less
  common preset types and formula-driven custom shapes (rectangles), WordArt
  styling, transitions and effects outside the model's set (fades), emphasis
  and motion paths, slide comments, the date a date field showed.
- **Slides** opens `.ppt`, `.pps` and `.pot` through the converter
  (`officina-convert ppt`, `--password` as for the others). It doesn't write
  `.ppt` (§16.40, D8): a deck opened from one has no save target, so Save asks
  where to keep a copy and says why.

**Gates.** `officina-convert/examples/ppt_lo.rs`: slides_lo's sample deck
saved as `.ppt` by LibreOffice 24.2; our reader meets slides_lo's checks on
it (slide count, words, notes, pictures, hidden slide, transitions, builds,
bullet levels and numbering, turned box, gradient direction and colours,
dashed line with arrowhead, bold red run, group). LibreOffice writes the star
as a formula-driven custom shape, which we draw as a rectangle and note; the
gate checks for that note. LibreOffice's password on a `.ppt` save doesn't
encrypt the file, so the gate checks that copy opens with the same text, and
checks encryption on the sample deck (`ppt::sample`) and, given
`PPT_PROTECTED=path:password`, on Apache POI's PowerPoint-saved protected
deck: asks for the password, refuses a wrong one, opens with it: **22/22**.
Corpus: Apache POI's 147 and LibreOffice's 73 `.ppt` test files
(`sd/qa/unit/data/ppt`): 168 open, 52 refused in words (36 damaged fuzzer and
crash files, 8 exploit samples that aren't PowerPoint files, 3 PowerPoint 95,
5 needing a password; all 5 open with POI's known passwords); slide counts
equal LibreOffice's on all 162 it opens, 99.50 % of the words its export
shows (most of the rest is text inside WMF pictures); our writer then reader
and the `.odp` route keep every deck the same. 20,000 decks mutated inside
their container never panic; the slowest takes 82 ms. Unit tests build a deck
record by record, plain and protected, and Slides opens both through the real
converter.

Open: EMF/WMF pictures, formula-driven custom shapes (guide formulas), more
preset types, picture and pattern fills, tables (drawn as groups by
PowerPoint), embedded objects and sounds, comments.

## 17. Decisions (accepted 2026-11-14)

Shankar agreed to all of them as recommended, 2026-11-14.

1. **Suite shape: three apps over shared crates, one jailed process per document.**
   *Recommend yes.* The alternative, one process per app holding many documents with a
   jailed worker per document, saves memory per extra document but makes every document's
   data reachable from the shared process.
2. **Native format: Officina's own chunked format, ODF and OOXML first-class for
   interchange.** *Recommend yes.* ZIP+XML can't load lazily, can't save incrementally and
   isn't bounded by construction, which is what big sheets need. Alternative: ODF native
   (simpler story, slower at scale).
3. **Objects within the suite are in-process (a chart in a text document is the same
   engine), not Limen embeds.** *Recommend yes.* Limen stays for content from other apps
   (media, documents from untrusted sources).
4. **Word processor layout: share the Navigator's text stack, build a new paged,
   incremental layout engine.** *Recommend yes.* Bending the Navigator's flow layout into
   pages and per-keystroke relayout would compromise both.
5. **Spreadsheet scale: no Excel-sized limits in the model (2³¹ rows, 2²⁰ columns), 100 M
   cells in memory as the v1 target.** *Recommend yes.* Billions of rows (a data-table
   mode over Parquet/Arrow files) is a different tool and out of scope for now.
6. **Collaboration: model built CRDT-ready, no co-editing in v1.** *Recommend yes.* Real-time
   co-editing over Concursus is its own spec later.
7. **Macros: none run, ever, in v1; macro parts kept inert on round-trip so a user's
   `.xlsm` isn't damaged.** *Recommend yes.* A future scripting story, if wanted, would be a
   new jailed, capability-scoped language (WASM in its own jail), never VBA compatibility.
   Alternative: strip macros on save (safer to forward, but breaks users' files).
8. **Legacy binary formats (`.doc`, `.xls`, `.ppt`): import only, late (O7+), or never.**
   *Recommend import only, late.* They are still common in mail and archives, and the
   jailed converter makes their parsers' bug history containable.
9. **Saving: explicit Save, with continuous autosave to a private journal for recovery.**
   *Recommend yes.* Alternative: macOS-style "the file is always saved" with versions, which
   is natural on Tessera but surprising on sticks and shares.
10. **IME / input method framework: specify it now as a dependency (with the Tactus and
    Pergola owners).** *Recommend yes;* without it the word processor serves only scripts
    typed directly from the keyboard (no CJK, and limited Indic input).
11. **Read-only previews of office files in other apps (mail attachments, Scrinium) go
    through a converter emitting the document profile, shown by the Limen `doc-viewer`.**
    *Recommend yes;* it reuses everything and opens no editor.
12. **Printing: PDF export now, a separate print-service spec later.** *Recommend yes.*
13. **UI style: a toolbar plus an inspector side panel and a command palette, not a ribbon.**
    *Recommend yes;* closer to the visual language (docs/design/atrium-visual-language.md)
    and keeps the canvas large. Keyboard shortcuts follow common Office conventions.
14. **Names: Officina for the engine; `atrium-write`, `atrium-sheets`, `atrium-slides` for
    the apps.** *Recommend yes,* per NAMING.md's rule that apps keep plain names.
15. **Order: spreadsheet first (O1, O2, O4), then word processor (O3, O5), then slides
    (O6).** *Recommend yes,* for the reasons at the end of §16.
16. **Images in large documents decode in the document process (memory-safe decoders, caps
    before allocation), with a fixed decoded-pixel budget.** *Recommend yes,* as the
    Navigator's worker does: the process holds only this document. A decoder jail per image
    costs about 50 ms each (threat model §2.4), seconds per screen of figures.
17. **No master documents in v1; one file scales to thousands of pages (§7.4).**
    *Recommend yes.*

## 18. Not in scope

Databases (an Access / Base equivalent), diagramming beyond slide shapes (a Visio
equivalent), desktop publishing, mail, note taking (they can use `RichTextEditor`), and
real-time collaboration (D6). Each could reuse Officina's crates later.
