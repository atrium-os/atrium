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
(O5), pictures dropped from the store when unused (they go on saving until reopened).

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
