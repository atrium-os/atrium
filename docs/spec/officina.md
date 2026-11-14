# Officina: a native office suite for Atrium

**Status:** proposal, 2026-11-14. Scope only, nothing built. The decisions in §17 are
Shankar's to make; each has a recommendation.

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
picker can't hand out today. Proposed picker additions (Scrinium owns them):

| message | meaning |
|---|---|
| `Open { filter: { kind: Save, suggested_name, extensions } }` | the sheet shows a name field and folders; the user names the file |
| `ToParent::SaveTarget { name }` + `EMIT_FD` | a descriptor to a **new temporary file in the chosen folder**, created by Scrinium, write-only |
| parent → child `Commit { fsync: true }` | Scrinium fsyncs and renames the temporary file over the target atomically, then sends `Saved` |
| `Abandon` | Scrinium unlinks the temporary file |

Plain Save of a file that came from the picker uses the same exchange without the sheet: the
document process holds a **save handle** (an opaque token bound to its uid and the file)
that Scrinium honours for the life of the document process. If the folder can't take a
temporary file (a read-only stick), Save falls back to writing in place after a full backup
to the autosave folder, and says so.

### 4.3 Recent files and reopening

Reopening a file after a restart needs a grant that outlives the process, which Atrium has
no notion of yet (scrinium.md §8 "always allow"). Proposed: Scrinium issues a **bookmark**,
an Arca-sealed token naming (user, app id, file identity) that only Scrinium can redeem,
re-checking that the file still exists and the user still owns it. The launcher keeps
bookmarks, never paths or descriptors. Needs Scrinium to hold the user's home folders
first (scrinium.md §8). Until then, recent files work only within a session.

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
| `.xlsm`, `.docm`, `.pptm` | as their base format; macros reported, kept inert (§5.3) | base format only unless D7 says otherwise | O4 |
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
| **O3** | `RichTextEditor` (flowing mode first, then paged), `officina-text` paged layout, `atrium-write` with `.odt`/`.docx` converters, PDF export | round-trip gate for text; keystroke-latency gate on a 500-page document |
| **O4** | `.xlsx`/`.ods` converters, styles, number formats, charts, sort/filter, structured tables, macros kept inert | round-trip gate for sheets; 1,048,576-row `.xlsx` open target |
| **O5** | tracked changes, comments, footnotes, TOC; pivot tables, conditional formats, dynamic arrays, `LAMBDA`; recent files via bookmarks (needs Scrinium home folders) | the corresponding corpus files |
| **O6** | `atrium-slides`: masters, layouts, presenter view, transitions, `.pptx`/`.odp` | round-trip gate for slides; presenter view on two monitors |
| **O7** | signing via Sigillum, encrypted documents, external data via Nuntius, IME-dependent scripts, legacy binary import if D8 says yes | per feature |

The spreadsheet goes first: it carries the hardest engine work (scale, recalc), it needs
only the simplest text (one-line cells), and the grid widget it forces is useful to other
apps at once.

## 17. Decisions for Shankar

Each with a recommendation.

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

## 18. Not in scope

Databases (an Access / Base equivalent), diagramming beyond slide shapes (a Visio
equivalent), desktop publishing, mail, note taking (they can use `RichTextEditor`), and
real-time collaboration (D6). Each could reuse Officina's crates later.
