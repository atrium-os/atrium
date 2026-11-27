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
- **Files come from Scrinium.** Open gives the app a descriptor, and Save As
  publishes an atomic copy. The Sheets launcher reopens files in personal
  home folders through Scrinium bookmarks (§4).
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
 Scrinium picker ──fd──► portcullisd ──fd──► atrium-sheets (one jail per workbook)
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

**Sheets document launches.** The launcher window offers Open and New. New
uses a Sheets press that Portcullis checks before starting a blank workbook.
The embedded Open picker gives the selected file directly from Scrinium to
Portcullis with the picker's own press and the parent persona from Limen.
Files, downloads and attachments use the same document launcher. Each live
workbook slot has its own jail root, UID and temporary writable layer. The
temporary layer is discarded at teardown. Each workbook journals into its
own size-limited recovery volume at `/atrium/recovery`. After a crash,
**Recover…** opens a single pending workbook directly. When several are
pending, the launcher lists their current names and opens the chosen one
in a fresh jail. Chooser rows expand to show the full filename, wrapping it
when it exceeds the launcher width. A confirmed Save As updates the chooser label and the
autosave name to the filename Scrinium published, including any collision
suffix. If Portcullis misses the update, it reconciles the chooser label from
the durable autosave name after the document jail is torn down. A workbook
can remain open through a Portcullis restart. If it then crashes, Recover
releases its abandoned mounts before presenting the workbook. A clean close
ends the document jail. New can reuse that slot with a fresh recovery identity
while other workbooks remain open. If the computer restarts before cleanup,
New can still reuse the cleanly closed slot and Recover can reopen a sibling
with its unsaved edits. If a Save As name update was interrupted, the first
post-reboot chooser reads the published name from the detached autosave.
If a recovery image is missing or fails to mount, the launcher shows the
workbook name in the error, including when the chooser already listed it.
If the image mounts but preparing the document jail fails, the launcher
names the workbook and prompts the user to try Recover again. If the jail
starts but cannot execute Sheets, the launcher identifies the workbook whose
recovery failed to start. The user can retry once the executable is available.
If startup takes longer than the acknowledgement deadline, the launcher names
the workbook that is starting. Pressing Recover again keeps that notice visible
while the original launch continues. The chooser keeps other pending workbooks
selectable beneath the notice. Each can open and save during the delay. Once the
image is available, Recover can reopen it with its original identity.
Canceling the picker keeps the previous name. A protected workbook asks for its password there. Cancel
closes that recovery window and leaves the generation available to try again.
The recovery reader replays the base and journal. Save As obtains a new
picker grant, which subsequent saves use. Don’t Save discards that workbook’s recovery.
Review pickers give comparison inputs to the current workbook.

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

Sheets asks for one workbook using the picker’s `Workbook` mode. The file
descriptor goes to the new jail. The window that requested Open receives
completion and keeps its own workbook. Opening from the Files window uses
the same Portcullis launcher. A version-one sealed Officina envelope confirms the
`.osheet` suffix for a protected workbook. Sheets asks for the password inside
the new document jail. When the prompt would crowd the password field, Sheets
wraps the full prompt above the field and its Open and Cancel buttons.

### 4.2 Save and Save As

For a workbook opened through Scrinium, Sheets encodes the current revision and
sends a sealed snapshot and its session save grant through Limen. Plain
native saves stream directly into the sealed memory file, avoiding a second
workbook-sized buffer. Scrinium stages the bytes in the selected directory,
syncs the file, checks the original entry and revision, atomically replaces
it and syncs the directory. Sheets
keeps the grid interactive while waiting for confirmation and marks only the
acknowledged revision saved. Later edits stay pending. Closing the window or
pressing Ctrl+W during Save waits for the active request. Sheets closes only
after confirmation if the workbook is clean. A later edit or missing
confirmation keeps the document open. A stale or expired grant requires
reopening the workbook or Save As. Consecutive saves on UFS tolerate deferred
inode timestamp changes. Scrinium checks workbook contents before replacement,
including when an outside edit preserves the file’s size and modification time.

If Limen restarts, each open Sheets window drops its old picker or print
session and reconnects to the broker. The workbook remains editable during
the outage. An interrupted save must be requested again. Save As opens a
fresh picker for the current revision, including edits made while offline.

Save As sends a sealed native workbook to Scrinium’s Save picker. Scrinium
publishes the completed file under an unused name and returns a new save grant.
Once Sheets acknowledges the copy, ordinary Save uses that grant and writes the
native file. The original XLSX or ODS keeps its existing contents. An interruption
before publication leaves the previous workbook intact and can leave a hidden
staging file. Neither operation exposes the directory to Sheets.

The installed desktop gate `scripts/sheets-interop-vm-check.sh` with
`SHEETS_INTEROP_CASE=native_copy` exercises Save As with unsaved edits in XLSX
and ODS. It edits and saves each native copy, reopens both through the picker,
checks the saved formulas and workbook features, and compares the originals
byte for byte.

On Tessera, the same save boundaries preserve the prior and published file
in native filesystem generations under the volume’s retention policy
(scrinium.md §4.2). Sheets does not need access to the snapshot namespace.

### 4.3 Recent files and reopening

The Sheets launcher’s **Recent Workbooks** button opens a Scrinium picker
with up to eight personal workbooks. Opening a workbook or publishing it
with Save As records a bookmark when the file is in a supported home
folder. Ordinary Save advances that bookmark to the file Scrinium just
published. Scrinium keeps the bookmarks in its private per-person app
state across restarts (scrinium.md §9).

Selecting a row rechecks the file’s native handle and opens a fresh
document jail with a new grant. A rename within the accessible home
folders preserves the bookmark on the same filesystem. Deleting the file
and putting a copy at the same name leaves the bookmark unavailable.
Sheets receives the chosen file’s displayed name and descriptor.

### 4.4 Autosave, recovery and versions

- **Autosave journal.** Every edit is an op in `officina-core`'s log (§6). The log is
  appended to the document's private autosave folder every few seconds and on blur; a crash
  replays it over the last saved file. Nothing is written to the user's file without Save
  (§17 D9 asks whether to change that).
- **Versions.** Data ▸ Browse Versions… opens Scrinium for a workbook with a current
  save grant. Scrinium lists recent predecessors from its live grant cache whose bytes
  Tessera still retains. Selecting one opens a separate Sheets jail from a sealed,
  read-only historical copy. Editing or saving that view cannot replace either the
  historical generation or the current file. Save opens a picker for a new copy.
  The list follows Tessera retention and the lifetime of Scrinium's grant cache.
  UFS and other filesystems keep their current-file save flow and show an empty list.

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
| `.csv`, `.tsv` (encoding and delimiter detection) | Sheets delimited reader | UTF-8 values using the input separator | O2 |
| `.rtf`, `.txt`, `.md` | converter | writer | O3 |
| pasted / dropped HTML | `navigator-normalize` in a jail, then profile → rich text | Tabula offers HTML subset | O3 |
| `.xlsm`, `.docm`, `.pptm` | as their base format; macros reported, kept inert (§5.3) | with the kept macro parts, byte for byte (D7; `.xlsm` built in O4f, §16.8) | O4 |
| `.xls` (BIFF8) | converter (§17 D8) | native Save As | O7b |
| `.doc`, `.ppt` (binary, pre-2007) | converter (§17 D8) | native Save As | O7+ |
| `.pdf` | not edited; opens in `atrium-pdf` | `officina-pdf` export, PDF/A-2b | O3 |

Writers run in the document process because their input is our own validated model; their
output is checked by reading it back through the converter in CI.

In Sheets, **Import Notes** opens the full report when a workbook has import
notices. Previous and Next show each note in order, including approximations,
inert content, dropped content, read-only opens and delimited encoding
conversion. The button stays available while the workbook is open, including
after editing or saving. Save notices appear in the status bar. A reopened
file gets a new report from its current contents.

XLSX and XLSM import ISO calendar-date and timestamp cells as numeric dates
in the workbook's epoch. Explicit number formats are preserved. Cells in
General get a date or timestamp display format. Timestamps with offsets
are normalized to UTC. Invalid or out-of-range date strings remain text with
a fidelity notice. Ordinary Save writes the numeric value and its number
format. The installed `SHEETS_INTEROP_CASE=dates` gate checks XLSX, XLSM and
native Save As from XLS. [Excel date-cell format notes](https://learn.microsoft.com/en-us/openspecs/office_standards/ms-oi29500/b9d01770-b85b-4fb2-a387-f5811719a1af)

ODS preserves the 1904 null date, date serials and formula arithmetic across
ordinary Save and in native workbooks. Timestamp offsets are normalized to UTC.
Typed dates in General receive a date or timestamp format, while explicit
number formats are retained. Invalid or out-of-range typed dates keep their text. Other explicit null dates
outside the normal 1900-compatible and 1904 modes receive a fidelity notice.
The default mode retains Sheets' Excel-compatible handling from the start of
1900, including its historical difference from a Gregorian calendar before
March. [ODF calculation settings](https://docs.oasis-open.org/office/OpenDocument/v1.3/os/part3-schema/OpenDocument-v1.3-os-part3-schema.html#element-table_null-date)

#### Sheets fidelity triage

Prioritize changes that let invalid input through or alter saved values, then
changes that turn editable workbook features into static content. Cosmetic
differences come after those. The following groups cover the current notices
and their associated import or Save behavior.

| Priority | Formats | Group and user impact |
|---|---|---|
| 1 | XLSX, XLSM, XLS, ODS | **Input validation.** Sheets preserves Excel extension lists as exact choices, including commas inside a choice. BIFF8 validation stays active through native Save As. Missing or unsupported formulas, malformed BIFF8 rules and unsupported input method settings receive fidelity notices. |
| 2 | XLSX, XLSM, XLS, ODS | **Dates and formula results.** Excel and ODS preserve the 1904 epoch and valid typed dates within the supported range. Invalid or out-of-range dates keep their text and a notice. An unavailable direct formula call keeps a useful saved result as a static value with a notice. Supported formulas recalculate and discrepancies with the saved results remain visible in the import report. |
| 3 | XLSX, XLSM, ODS, XLS | **Other editing constraints.** Sheets keeps active XLSX, XLSM and ODS sheet locks and their cell styles. Foreign passwords and custom Excel permissions have fidelity notices. |
| 4 | XLSX, XLSM, ODS, XLS | **Editable analysis.** Top and bottom sheet filters by count and percentage stay live in XLSX, XLSM and ODS. The remaining static cases are grouped below. ODS retains live spill and table references for Sheets and supplies fixed compatibility formulas to other apps. |
| 5 | XLSX, XLSM, ODS, XLS | **Review and output.** Sheets restores intact ODS comment threads from a bounded extension and gives other apps a standard note. Legacy XLS keeps cell notes, supported links, rectangular print areas and both print-title axes on native Save As. Unsupported drawings, chart types, chart series, conditional formatting variants and print settings can still lose content or change output. |
| 6 | XLSX, XLSM, ODS, XLS | **Appearance.** Sheets draws turned or stacked text, shrink-to-fit, diagonal borders, patterns and supported linear gradients. Native files and applicable foreign formats retain these settings. XLSX and ODS import bound the width of style defaults on empty rows. Unsupported gradients and conversion to formats with fewer fill or border options receive fidelity notices. |
| Format boundary | CSV, TSV, XLS, macro-enabled formats | Delimited files carry values and may report encoding conversion or clipped long fields. XLS opens for import and can be saved as a native copy. Supported macro payloads stay inert and external workbook values stay cached. |

#### Installed desktop closeout matrix

On a private Tessera-root guest with current Sheets, its converter, and
Scrinium from the same source revision, run the `appearance`, `legacy`, and
`delimited` cases of the [desktop gate](../../scripts/sheets-interop-vm-check.sh)
against fresh fixtures in the sole picker place. Set `SHEETS_INTEROP_NOTES=1`
for the appearance and delimited runs.

| Formats | Desktop evidence |
|---|---|
| XLSX and ODS | Ordinary Save, picker reopen, pixel-identical styled regions and the import notice for bounded row defaults |
| XLSM | Ordinary Save, picker reopen, original VBA and code names, the persistent inactive-macro label, and both pages of a multi-note report |
| XLS | Read-only and unsupported-merge notices, the copy picker from ordinary Save, native Save As and picker reopen, and an unchanged source |
| CSV and TSV | Quoted fields and embedded line breaks in UTF-8 and Windows-1252 inputs, values-only Save, and picker reopen as UTF-8 |

The [visual checker](../../scripts/sheets-appearance-png-check.py) checks the
saved grid regions. The [notice checker](../../scripts/sheets-interop-notices-check.py)
checks the status and Import Notes screenshots with Pillow and Tesseract.
Use current Scrinium for the Windows-1252 cases so the named-delimited-text
check runs at the picker.

#### Advanced filters and pivot fidelity

| Group | Formats | What the user sees |
|---|---|---|
| Sheet rank filters | XLSX, XLSM, ODS | Sheets retains top and bottom numeric rules by count and percentage. **Data → Reapply Filter** recalculates the cutoff after edits. See §16.6 for rounding and ties. |
| Sheet text filters | XLSX, XLSM, ODS | Excel custom filters keep text comparisons and `*`, `?`, and tilde escapes. ODS keeps text comparisons, begins/ends/contains, regex matches, their negations, and case sensitivity. Open restores the last saved row visibility and shows filter buttons for ODS standard filters. **Data → Reapply Filter** reruns the rules after editing the source. |
| ODS compound filters | ODS | Worksheet filters and named database ranges retain cross-column OR and nested AND/OR. Reapply uses current values, while Save retains the last applied visibility. Single-column selections can narrow the compound rule. Show All clears the whole filter. Live compound rules are bounded to 16 levels and 1,024 nodes. |
| Excel calendar filters | XLSX, XLSM | Sheets keeps Gregorian year through second selections, including ordinary values and blanks in the same checklist. It retains supported non-Gregorian calendar tags and evaluates the group fields as Gregorian, matching Excel. It preserves relative day, week, month, quarter and year rules, Year to Date, and named months and quarters across years. Save keeps the last applied bounds and visibility. **Data → Reapply Filter** uses the current UTC date, like Sheets' TODAY formula. Weeks start on Sunday. Both Excel date systems are supported. |
| Excel appearance filters | XLSX, XLSM | Sheets keeps solid cell colors, cell-level font colors, No Fill, Automatic font color, and built-in icon selections, including Office 2010 icon sets and No Cell Icon. Pattern filters distinguish geometry and resolved foreground and background colors. Excel Save preserves the patterns, and the Sheets grid draws them. Office 2010 custom icon rules retain their threshold slots, including mixed icon sets and empty slots. **Data → Reapply Filter** uses the current cell style and conditional formatting. Save keeps the last applied row visibility. |
| Table-local filters | XLSX, XLSM, ODS | Each table owns its filter rules and last applied row visibility. The Data menu and header dropdown act on that table. Worksheet filters stay independent. A row is hidden while any applicable filter hides it. Excel table parts and ODS named database ranges keep supported rules across Save and reopen. |
| Static filter visibility | XLSX, XLSM, ODS | Unknown calendar tags, gradient filter targets, malformed custom icons and unsupported ODS conditions can leave only the saved row visibility. ODS regex uses the bounded Rust regex syntax. Rules that need unsupported syntax, such as backreferences or look-around, receive a fidelity notice. Excel export reports when an ODS filter cannot be carried as a live rule. ODS export reports when it keeps only the saved row visibility of Excel calendar or appearance rules, or omits Excel custom conditional icons. |
| Live pivot display calculations and groups | XLSX, XLSM, ODS | Sheets preserves Excel value display modes, including the Office 2010 modes. ODS retains field references, derived group chains and automatic aggregation. Excel retains discrete groups, fixed numeric bins and arithmetic calculated fields over local worksheet sources. Save keeps the definition and the last refreshed cells. Refresh recomputes the values and group membership from the source. |
| Recovered pivot caches | XLSX, XLSM | Sheets uses the workbook cache ID when a pivot cache link is broken. A damaged definition can be rebuilt when it still identifies a local source. Saved output stays visible until Refresh. |
| Static pivot results | XLSX, XLSM, ODS | Excel calculated items, unsupported calculated formulas, automatic numeric or calendar bins, and missing or damaged caches with an unknown or external source stay as saved cells. ODS groups with missing or circular dependencies, unsupported aggregates and external or database sources also keep their saved cells and carry a fidelity notice. |
| Pivot item order | XLSX, XLSM, ODS | Sheets preserves Excel manual order and value sorting by grand total or a selected opposite-axis member, along with ODS manual, source, label and value sorting. Previous, Next and Running Total follow the visible member order after refresh. |
| Pivot layout | XLSX, XLSM, ODS | Outline, per-field automatic subtotals and the innermost Values row axis stay live. Unsupported custom subtotals and a Values field before other fields retain their saved cells with a notice. |

The native table record keeps the last applied row mask with the filter and
uses table-relative coordinates. Older table records decode without a filter.
XLSX and ODS share worksheet row visibility across scopes. On import, Sheets
preserves the file's visible rows and attributes saved hidden rows to each
table predicate they still fail. XLSX has one hidden flag for the row. A
saved hidden row whose previous owner cannot be inferred stays hidden as a
worksheet row. ODS distinguishes rows explicitly marked as collapsed.
[Microsoft table and worksheet AutoFilter ownership](https://learn.microsoft.com/en-us/openspecs/office_standards/ms-xlsx/a3349ee1-4144-443c-9ced-e014a9474cc1) · [OpenDocument named database ranges](https://docs.oasis-open.org/office/OpenDocument/v1.3/os/part3-schema/OpenDocument-v1.3-os-part3-schema.html)

#### Sheet and cell protection in Sheets

XLSX and XLSM preserve active `sheetProtection` and cell XF protection,
including inheritance from a named style. ODS preserves `table:protected`
and `style:cell-protect`. A protected sheet's default cells are locked.
Explicitly unlocked cells, including styled blank cells, accept edits.
A paste, fill or other batch touching a locked cell is refused as a whole.
Ordinary paste and fill on a protected sheet keep the destination styles.
Sheets restricts structural changes and changes to workbook-wide settings
that can affect protected sheets.

Hidden formulas still calculate. The formula bar and grid editor hide the
formula, and Copy takes its calculated result. ODS cells marked
`hidden-and-protected` also hide their value in the grid and clipboard.
The underlying data and formulas remain in the workbook. [Excel cell protection](https://learn.microsoft.com/en-us/dotnet/api/documentformat.openxml.spreadsheet.protection?view=openxml-3.0.1) · [ODF 1.3 cell protection](https://docs.oasis-open.org/office/OpenDocument/v1.3/os/part3-schema/OpenDocument-v1.3-os-part3-schema.html)

Recognized password verifier fields and Excel's custom sheet permission
attributes are retained on same-format Save. Fidelity notes explain that
Sheets cannot remove imported password locks and applies its unlocked-cell
editing policy. The native WORKBOOK settings store per-sheet protection,
and native style flags store unlocked, hidden-formula and hidden-value
settings. The `SHEETS_INTEROP_CASE=protection` gate covers all three foreign
formats through ordinary Save and picker reopen.

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
  date system (including the 1900 leap-year quirk) and the workbook's 1904 system.
  The workbook epoch drives date formulas, number formats and typed dates.
  Time-only values and elapsed durations use the unshifted numeric value.
  XLSX and XLSM preserve `workbookPr date1904`. XLS imports the BIFF setting and
  native Save As stores it as an optional trailing byte in the WORKBOOK chunk.
  A native workbook without that byte uses the 1900 system. [Excel date systems](https://support.microsoft.com/en-us/excel/date-systems-in-excel)
- **Formula text.** `FORMULATEXT(reference)` returns the formula bar’s A1 text for
  the top-left cell of the reference. It observes formula edits even when the value
  stays equal. A direct self-reference can display its own formula. A cell without
  a formula, a formula hidden by sheet protection, or displayed source longer than
  8,192 UTF-16 code units gives `#N/A`. A number or text literal gives `#VALUE!`.
  XLSX and XLSM use `_xlfn.FORMULATEXT`. ODS uses OpenFormula’s `FORMULA`.
  [Excel FORMULATEXT](https://support.microsoft.com/en-us/excel/functions/formulatext-function)
  [OpenFormula §6.13.12](https://docs.oasis-open.org/office/OpenDocument/v1.3/OpenDocument-v1.3-part4-formula.pdf)
- **Volatile** functions (`NOW`, `RAND`, `INDIRECT`, `OFFSET`) recompute on every recalc and
  are flagged in the dependency graph.
- **External** functions (`WEBSERVICE`, stock and currency data, links to other workbooks)
  are off until the user allows them for this document (§11.2).

- **Ranked values and subtotals.** LARGE and SMALL select the requested
  numeric rank, keeping ties and propagating input errors. For an in-range
  fractional rank, LARGE rounds up and SMALL truncates. SUBTOTAL supports codes 1–11 and 101–111, skips nested
  subtotals and filtered rows, and excludes manual hides and collapsed native
  outlines for codes 101–111. Sheet and table filter changes, row sizes, and
  outline changes invalidate subtotal groups and their dependents. Older
  workbooks store worksheet-filter and manual hides together. A saved hide
  inside the active worksheet filter's data range is interpreted as a filtered
  row, consistent with the existing filter state.
  [Microsoft LARGE](https://support.microsoft.com/en-us/excel/functions/large-function)
  [Microsoft SUBTOTAL](https://support.microsoft.com/en-us/excel/functions/subtotal-function)
- **Date headers.** MONTH accepts English month labels followed by zero in
  Excel's `MONTH(header & 0)` idiom. DATEVALUE uses the workbook date system
  and recognizes explicit ISO, US numeric and English named dates, including
  `1-JAN2021`. An omitted year uses the current year. ISO time components
  are discarded by DATEVALUE. EOMONTH calculates the last day of an offset
  month in the same date system.
  [Microsoft DATEVALUE](https://support.microsoft.com/en-us/excel/functions/datevalue-function)
  [Microsoft EOMONTH](https://support.microsoft.com/en-us/excel/functions/eomonth-function)
- **Closed workbook references in XLSX and XLSM.** The converter keeps a
  formula that directly refers to another workbook as its last saved result,
  including shared fills and array results. A direct single-cell reference
  can use the package's embedded cell value when its formula result is absent.
  An absolute single-cell defined name becomes a constant in its original
  scope, so local formulas using the name stay live. References without a
  usable cached result show `#REF!` and a fidelity note. Archive reads are
  bounded and use only internal relationships. Saving writes the inert
  values and names.
  [SpreadsheetML external cell cache](https://learn.microsoft.com/en-us/dotnet/api/documentformat.openxml.spreadsheet.externalcell)
- **Conditional totals.** SUMIF and AVERAGEIF take the shape of their criteria
  range and begin at the top-left cell of the optional value range. SUMIFS and
  AVERAGEIFS require matching shapes. Errors in matched values propagate through
  sums and averages, including indexed equality queries. Ordered indexes fall
  back to the walk when sorting could change which error is returned. A blank
  criterion reference means zero. An explicit empty string can match blanks
  beyond the used extent in a bounded explicit range.
  [Microsoft AVERAGEIF](https://support.microsoft.com/en-us/excel/functions/averageif-function)
  [Microsoft AVERAGEIFS](https://support.microsoft.com/en-us/excel/functions/averageifs-function)
  [Microsoft COUNTIFS](https://support.microsoft.com/en-us/excel/functions/countifs-function)
- **TEXTAFTER and date text.** TEXTAFTER matches literal delimiters and supports
  an array of alternative delimiters, forward and backward instances,
  case-insensitive matching, boundary matching, and a fallback value. Array
  arguments other than delimiters broadcast over the result. XLSX writes
  the Excel future-function prefix `_xlfn.TEXTAFTER`.
  Arithmetic and scalar numeric coercion, including VALUE and date functions,
  recognize explicit ISO dates, US month/day/year dates, and English
  dates such as `1 January 2023` in the workbook's 1900 or 1904 system.
  Two-digit years use Excel's 2029 cutoff. Invalid dates give `#VALUE!` in
  arithmetic. Date strings remain text in comparisons and in cells.
  [Microsoft TEXTAFTER](https://support.microsoft.com/en-us/excel/functions/textafter-function)
  [Microsoft date interpretation](https://support.microsoft.com/en-us/excel/change-the-date-system-format-or-two-digit-year-interpretation)
- **Array coercion.** Explicit ranges retain trailing blank cells when converted
  to arrays. Whole-axis references still use the used extent on that axis.
  Binary operators broadcast singleton axes and pad unmatched positions with
  `#N/A`. IFERROR handles those errors per element and returns empty text for a
  selected empty cell. ODS writes spilled error cells as cached matrix members
  so they cannot block the anchor on reopen.
  [Microsoft IFERROR](https://support.microsoft.com/en-us/excel/functions/iferror-function)

### 8.3 Dependency graph and recalculation

- **Range edges, not cell edges.** A formula that reads `A1:A1000000` is one edge to a range,
  kept in an interval index per column; a formula group depends on a sliding range once,
  not a million times. Changing a cell finds its dependents by interval lookup.
- **Implicit conditional ranges.** SUMIF and AVERAGEIF footprints cover the
  effective value rectangle for static cell, range, and named references, even
  when the formula spells only its anchor. A longer written target does not add
  dependencies past the effective range. Relative fills keep sliding footprints
  when the criteria height is constant.
- **Dirty propagation, then ordered evaluation.** An edit marks dependents dirty
  transitively; evaluation runs them in dependency order (topological, by strongly
  connected components so circular references are found and reported, or iterated when the
  user turns iterative calculation on).
- **Vectorised groups.** A formula group is evaluated column at a time over the chunk
  arrays, not cell by cell, so `=A2*B2` over a million rows is a loop over two `f64` slices.
- **Parallel.** Independent groups and partitions of large groups run on a thread pool
  inside the document process.
- **Visible-first recalculation.** Interactive slices prioritize dirty independent
  scalar rows in the viewport, including split and frozen panes. The scheduler
  brings bounded precedent ranges forward in dependency order. Formulas with broad
  prerequisites wait for those inputs. Ordered fills, cycles, volatile formulas
  and dependencies on unsettled spills use their regular evaluation path.
  Each slice targets 12 ms and can finish the current wave (§16.70). Dirty cells
  show their previous values in grey and the status bar shows “Calculating…”.
  A new edit restarts the affected work.
- **Lookup indexes.** Repeated `XLOOKUP`/`MATCH`/`VLOOKUP` over the same column build a
  hash or sorted index of that column once, invalidated by edits to it.
- **Correctness bar:** results match Excel on a differential corpus of real workbooks
  (§15); LibreOffice's results are a second oracle where Excel's aren't available.

The focused gate in `officina-convert/examples/lo_check.rs` checks conditional
totals, blank and text coercion, error propagation, and unequal array shapes
before and after source edits. It verifies Sheets XLSX and ODS reopen, then
checks the caches written by Calc after forced recalculation. The gate puts
a wrong cache on a control formula and requires Calc to replace it. Calc 26.8
differs on a direct text argument to SUM and on an IFERROR result at a missing
position of a short array operand. Both differences are asserted separately.

### 8.4 Data features

Sort, filter (auto-filter and advanced), conditional formatting (evaluated for visible
cells only), data validation, named ranges, find and replace, freeze panes, split views,
grouping and outlines, pivot tables (computed over the column chunks, so a pivot of 10 M
rows is a scan, not a formula per cell), charts (§9.2), CSV import that streams into chunks
(a 2 GB CSV loads without holding the text).

**Find and Replace** opens from Data or Ctrl+F / Ctrl+H. It searches stored cell
inputs, including formula source, in row order through the current sheet or all
sheets. Searches wrap and offer case-sensitive and whole-cell matching. Find
moves to the matching worksheet and cell. Replace operates on the selected cell
and Replace All applies as one undo step. The replacement batch checks formula
syntax, sheet protection and data validation before recording changes. Generated
pivot and spill cells and hidden protected inputs are outside the search.

**Split views** divide the worksheet horizontally, vertically, or both from Data.
Each pane scrolls on its own. All panes show the same worksheet and share the
selection, while the active pane owns the editor. Removing the split retains the
active pane's scroll position. Split positions are view state for the open session.

**Outlines** group selected rows or columns. Nested groups can be collapsed or
expanded from their header buttons, and Data can group and ungroup the selection.
Collapse leaves original dimensions, manual hides, and filters in place. Groups
follow inserted and deleted rows or columns and participate in undo and crash
recovery. Native files carry an optional per-sheet `OUTLINES` chunk (27), and the
grid journal records the group change as operation 26. XLSX and XLSM Save
preserve row groups that SpreadsheetML can represent, including empty detail
rows. Import reads summary markers above or below the details. Export places
the markers below, preserving the grouped rows and collapse state. A row group
without a collapse marker keeps its individually hidden rows. Expanding a
collapsed group leaves independently filtered rows hidden. Column groups,
ambiguous group boundaries, and overlapping independent hides on folded rows
use native Save As. Other foreign formats also use native Save As for outlines.

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
- **Sheets HTML tables** use the existing Officina converter worker and Navigator’s HTML parser.
  The worker returns a bounded range of typed values and static styles. See §16.88.
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
  Built 2026-11-17 as the system font library ([fonts.md](fonts.md)): the stand-ins,
  IBM Plex Serif, Noto for 23 scripts, and Source, Fira, Roboto and Inter, matched by name in
  `officina-text::PinnedFonts`.
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
| **O5** | tracked changes, comments, footnotes, TOC; pivot tables, conditional formats, dynamic arrays, `LAMBDA`; recent files via Scrinium bookmarks | the corresponding corpus files |
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
- **CSV and TSV** (`officina-grid/src/csv.rs`). Detection compares the first
  records using commas, semicolons and tabs, counting line breaks inside
  quotes as part of the field. Sheets uses tabs for a `.tsv` name.
  Import accepts UTF-8 with or without a BOM and reports Windows-1252
  fallback. Formula-like input stays text. Ordinary Save uses UTF-8, CRLF
  record endings, the source delimiter and General number formatting.
  Formulas save as calculated values, and Sheets names the delimited format
  in the Save notice. The [desktop gate](../../scripts/sheets-interop-vm-check.sh)
  with `SHEETS_INTEROP_CASE=delimited` checks both encodings in both formats,
  including quoted delimiters, doubled quotes, embedded CRLF and the original
  formula-like text beside a saved formula result.
- **`atrium-sheets`**: the app. Toolbar (Open, Save, Save As, Undo, Redo), a formula bar
  (address and the active cell's formula), the grid, a status line. Files come from the
  system picker (scrinium.md §4.1): Open asks for one file read-write and Save writes
  back to that descriptor. **Save As is interim:** until Scrinium's Save As sheet (§4.2)
  exists it asks the picker for a folder with write access and writes `NAME.osheet`
  there (temp file, hard link, so nothing is ever replaced; `NAME 2.osheet` and so on
  when taken). Saving to CSV or TSV keeps formula results as values.
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
- **Styles** (`officina-grid::style`): a style includes number format, font, fill,
  alignment, wrap, indent, rotation, shrink-to-fit, four outer borders and two
  diagonal borders. Each column keeps runs of interned styles. Restyling uses
  `GridOp::Restyle`; native `STYLES` and `STYLE_RUNS` also keep these settings.
  Styles with a new field use style marker 3 alongside the older encodings.
- **Excel styles:** XLSX and XLSM keep both diagonal flags and their shared
  stroke, rotated and stacked text, shrink-to-fit, patterns and linear
  gradients with up to 16 stops. Unsupported gradients use the first colour
  and appear in the fidelity report. BIFF8 imports rotation, shrink-to-fit,
  patterns and diagonals through native Save As. Excel row defaults are
  materialized as compressed style runs within 262,144 styled cells per sheet.
  The import normally covers the first 256 columns and extends through the
  used columns when the budget allows. Cell styles take precedence.
  [SpreadsheetML diagonal borders](https://learn.microsoft.com/en-us/dotnet/api/documentformat.openxml.spreadsheet.diagonalborder)
  [BIFF8 cell formatting](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/671c8577-901f-4215-9ebf-6f5890e5896d)
- **OpenDocument styles:** the cell properties keep signed turns through the
  normalized angle, stacked direction, shrink-to-fit and independent diagonals.
  Row defaults also style trailing blanks in a bounded prefix. Plain ODS
  cell backgrounds provide a solid fallback for Excel patterns and gradients.
  [OpenDocument 1.4 schema](https://docs.oasis-open.org/office/OpenDocument/v1.4/OpenDocument-v1.4-part3-schema.pdf)
- **Sheets** shows values through their format (with the format's colour, `#` for a date
  out of range), bold, font colour, fills over the grid lines, outer and diagonal
  strokes, wrap, vertical alignment, rotated and stacked labels, and shrink-to-fit.
  Rotated labels use the bundled UI face. Dashed and double diagonals use a
  single stroke in the grid. The installed `appearance` gate opens, edits and
  reopens XLSX, XLSM and ODS and performs native Save As from BIFF8. It checks
  saved styles and captures the grid before and after each operation. The
  [appearance checker](../../scripts/sheets-appearance-png-check.py) tests
  visible formatting and compares the styled cells pixel for pixel.

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
  rotation and other formula namespaces (kept as values). Direct unavailable
  function calls retain their cached values when available. Explicit string
  results keep their type even if they spell a spreadsheet error.
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

Sheets offers stable single-column sorts in Data and value filters by column.
To sort by several keys through the desktop, sort the least significant
column first and finish with the primary column.

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
  on shown text ignoring case, one or two numeric tests joined by and/or, or a
  top/bottom numeric rule by count or percentage) and hidden
  row ranges, swapped as one undoable `GridOp::Filter` (journal kind 4, native chunk 8);
  hidden rows have height 0 in the view. `.xlsx`: `row hidden`, `autoFilter` with
  `filters`/`customFilters`/`top10`, the hidden `_xlnm._FilterDatabase` name Excel expects.
  `.ods`: row `visibility` (`filter` / `collapse`) and anonymous database ranges with
  their conditions (LibreOffice leaves numeric order tests untyped and repeats the first
  set item; both handled). Imported rank filters and the last applied visibility
  survive ordinary Save. Reapply ranks the finite numeric cells in the whole column
  range below the header, then intersects the result with other column conditions.
  Percentage counts round up in Sheets, and every tie at the cutoff stays visible.
  Other specialized filters can retain the saved hidden rows as described in §5.2.
  [XLSX rank filters](https://learn.microsoft.com/en-us/dotnet/api/documentformat.openxml.spreadsheet.top10?view=openxml-3.0.1) · [ODF filter operators](https://docs.oasis-open.org/office/OpenDocument/v1.3/os/part3-schema/OpenDocument-v1.3-os-part3-schema.html#attribute-table_operator)
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
| Sheets desktop | [Interoperability gate](../../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=sort_filter` saves both sort passes and both filters in XLSX and ODS. It reopens the original format and checks stable ties, both filter predicates, visible rows, the manually hidden row outside the filter, and recalculated row and cross-sheet formulas. |

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

The `tables` profile of `scripts/sheets-interop-vm-check.sh` runs O4d
and O4e together in the installed desktop. An edit to the chart source
updates the workbook and sheet-scoped formulas, table total, and chart.
Changing the filter updates the chart’s visible categories. The test saves
each state, including undo and redo, and reopens both XLSX and ODS through
the converter. Sheets reports ODS save approximations for structured cell
references, table styles and totals, and typed chart series names when
those features are present.

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
- **Sheets** shows "Macros are inactive" in the status bar independently of the
  workbook name and Save feedback. The Remove Macros button journals the removal
  immediately and applies it to the file at the next Save. `.xlsm` files save as
  `.xlsm`.

Gates, on the 4-core cloud machine:

| gate | result |
|---|---|
| LibreOffice (`examples/macros_lo.rs`, LibreOffice 24.2) | our `.ods` with a Basic module → LibreOffice → `.ods`: the module's code is kept. Two real `.xlsm` files from Apache POI's test data: VBA project and code names kept; LibreOffice opening the original and opening our re-written `.xlsm` finds the same 5 VBA modules with the same code |
| round trips | `tests/roundtrip.rs`: `.xlsm` parts and code names equal through write and read and through a native file; `.xlsx` gets none; parts outside `xl/` and the other family's parts are not written; `.ods` Basic and Scripts parts equal |
| hostile input | the `.xlsx` and `.ods` fuzzers now run on packages with macros, with code-name, relationship-target and manifest-path mutations; clean |
| app | `atrium-sheets` test: removing macros survives a crash |


The [desktop interoperability gate](../../scripts/sheets-interop-vm-check.sh)
with `SHEETS_INTEROP_CASE=legacy` opens a workbook based on Apache POI's
[SimpleMacro.xlsm](https://github.com/apache/poi/blob/trunk/test-data/spreadsheet/SimpleMacro.xlsm)
fixture, edits a value, saves and reopens it. It checks the exact VBA parts
and code names, the expected cell values and recalculation, and the inactive
macro notice.

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

Not yet: chart editing (titles, kinds, ranges),
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
| O5h | recent files via Scrinium bookmarks | Tessera and UFS desktop restart gates |

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
  formula over its current spill. Excel's newer functions get
  `COM.MICROSOFT.` names. ODS cell formulas and defined names keep their live
  spill and table references in bounded Atrium attributes and include standard
  OpenFormula compatibility expressions. Sheets restores the live reference
  when the compatibility expression is unchanged. A different expression
  written by another app takes precedence. Other apps see the fixed range or
  a reference error when the source has no active spill. Save reports the
  compatibility behavior.
- **Desktop gate.** The [interoperability check](../../scripts/sheets-interop-vm-check.sh)
  with `SHEETS_INTEROP_CASE=formulas` exercises XLSX and ODS dates, times,
  named and cross-sheet references, spill anchors and errors. Each source
  contains a stale formula cache that triggers the import notice. The gate
  edits the input, saves, reopens and grows the spill again to verify the
  live spill references in both formats. It also checks the 1904 epoch, a
  defined name over the spill, a table reference, and the repaired
  cache on both saved files.
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
  above and its `dxf`s in the styles part. Rule priorities apply across
  the whole sheet. The importer splits a range block when rules from
  another block interleave, keeping each rule's original ranges and
  formula origin. Excel 2010's `x14` extension is reported and the 2007
  data bar rule is kept. `.ods`:
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
  "Sum of Units"), grand totals for rows and columns, and automatic subtotals
  with per-field visibility and placement.
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
  `pivotCaches`. The reader accepts sources on a sheet or a table's name.
  Whole-column sources end at the last row in use. Excel discrete groups preserve the
  values that map to each group, including typed members. Fixed numeric bins
  retain their range and cache labels. Refresh rejects a numeric grouping when
  its source has nonnumeric values and leaves the existing output in place.
- **Excel cache recovery.** A pivot's workbook cache ID can recover a
  definition when the direct relationship is missing or broken. When a
  damaged definition still identifies a local worksheet range, table or
  fixed defined name, Sheets rebuilds the pivot model and writes a fresh
  definition on Save. The saved worksheet output stays until the user runs
  Refresh in Sheets. Intact item indices can still supply hidden members,
  selected pages and display references. Sheets leaves the cells static
  when an item essential to the pivot cannot be identified, or when all
  source metadata is missing. Import reports the recovery or the reason the
  pivot stayed static. [Microsoft PivotTable package structure](https://learn.microsoft.com/en-us/office/open-xml/spreadsheet/working-with-pivottables)
- **Calculated value fields.** Excel worksheet-backed calculated fields
  retain numeric constants, field references, parentheses, arithmetic, powers
  and percent. Each result evaluates over the sums for its row, column, or
  grand-total slice. It can use another calculated field. The native pivot
  keeps the definitions. Unsupported functions, item references, invalid field
  references and cycles leave the saved cells as values with a fidelity notice.
  [Microsoft calculated field semantics](https://support.microsoft.com/en-us/excel/calculate-values-in-a-pivottable)
- **Display calculations.** XLSX retains `showDataAs` and the Office 2010
  `pivotShowAs` extension. Named base items resolve through the pivot field's
  item list and cache, so Save can rebuild their indices. Differences,
  percentages, index, running totals, parent totals and ranks use the
  aggregates of the relevant value field. Cells keep their saved results
  until Refresh. Percentage calculations give newly generated cells a
  percentage format when they have the General format.
- **ODS.** `table:data-pilot-tables` retains selected pages, members, value
  captions and grand totals. `table:data-pilot-field-reference` keeps the
  reference calculation and its base field or item. `table:data-pilot-groups`
  keeps named item groups, numeric intervals and calendar groups, including
  fixed or automatic bounds. The grouped field can have its own name and
  appear beside the original source field. A group can take another derived
  group as its input. An automatic measure uses Sum for numeric source data and
  Count when the source contains nonnumeric values. ODS preserves the automatic
  setting. Excel export writes the currently selected aggregate. External and
  database sources keep their saved cells. When exporting to another format,
  unsupported groups and calculations retain their output cells with a Save
  notice.
- **Layout.** XLSX and ODS preserve outline row fields, automatic subtotal
  visibility per field, and subtotals above or below each row group. In outline
  form the parent gets its own row. Tabular form keeps a column for each row
  field. Values can be the innermost row or column field. Refresh puts each
  vertical measure on its own row and formats percent measures independently
  of numeric rows, preserving explicit formats. Saved cells stay static with a
  fidelity notice for custom subtotal functions or a Values field placed
  before other fields on its axis.
- **Item order.** Excel manual order comes from the pivot field's item
  list, whose indices address shared cache values. Save keeps manual
  positions for missing members and places new members after the saved
  ones. ODS keeps the `table:data-pilot-sort-info` mode and direction.
  Manual order comes from its member list. No sort uses source order.
  Value sort uses the chosen measure's aggregate under each parent and
  uses label order to break ties. Previous, Next and Running Total use the
  same order as the visible items. Excel `autoSortScope` resolves a measure
  and the selected row or column members through the pivot field items
  or the shared cache. A measure without selected members sorts by its
  grand total. Refresh sorts by that measure's aggregate at the selected
  opposite-axis cell or subtotal. Save rebuilds indices and keeps a
  temporarily absent selected member in the cache. An Excel scope can select
  several items within an opposite-axis prefix. The sort aggregates the
  selected records, including the original counts for Average. Absolute
  positions follow the visible label or manual order within each parent.
  Relative positions, offsets and sorts that depend on a value-sorted
  opposite axis carry fidelity notices. Grand-total value sorting translates between
  XLSX and ODS. ODS export reports selected-member sorts as label sorts.
- **Native file and recovery.** Pivot definitions carry display modes,
  group definitions, item sort and field layout settings, and the last output area. The pivot operation journals
  the definition, cell edits and percentage format changes together.
- **Remaining gaps.** Relative and offset sort references, custom subtotal
  functions, non-innermost Values fields, automatic numeric bins and Excel calendar bins, calculated
  items, calculated fields with worksheet functions, GETPIVOTDATA and pivot
  charts.
  [Excel display modes](https://learn.microsoft.com/en-us/dotnet/api/documentformat.openxml.spreadsheet.showdataasvalues?view=openxml-3.0.1) · [Excel extended modes](https://learn.microsoft.com/en-us/openspecs/office_standards/ms-xlsx/f5b294f2-a168-46c3-b9a9-8e4f0132b78d) · [OpenDocument data pilots](https://docs.oasis-open.org/office/OpenDocument/v1.3/os/part3-schema/OpenDocument-v1.3-os-part3-schema.html)

The installed Sheets desktop gate `SHEETS_INTEROP_CASE=pivot_display`
covers ordinary Save and picker reopen in XLSX and ODS, a source edit that
leaves the cached output visible, and Refresh followed by another ordinary
Save. Its XLSX fixture verifies that ranking changes and a difference still
uses the named base after Save rebuilds the cache. The ODS fixture verifies
the named groups, percentages and a reference to a grouped member.
The paired `pivot_order` desktop gate uses a manually ordered XLSX pivot
and an ODS pivot sorted by an aggregate. After Save and picker reopen, a
source edit changes the ODS order while the XLSX order stays fixed. Both
recompute the adjacent-member differences and running totals on Refresh.
The `pivot_scope` desktop gate imports Excel grand-total and selected-member
scopes on both axes. It checks the saved order, Previous, Next and Running
Total after picker reopen, a source edit, Refresh, a second Save and a
second picker reopen. [Microsoft pivot references](https://learn.microsoft.com/en-us/openspecs/office_standards/ms-oi29500/8e92a816-2bb4-4938-9f1c-1a9800714ede)
The `pivot_cache` desktop gate imports a missing cache part with an
alternate workbook relationship and a truncated cache with a local named
source. It checks the saved output after Save and picker reopen, then edits
the source and verifies that only Refresh changes the output. It saves and
reopens each refreshed workbook again.

The paired `pivot_sources` desktop gate covers a chained ODS group with
automatic aggregation and an Excel workbook with discrete groups, fixed
numeric bins and a calculated ratio. It checks the saved output, opens from the
picker, edits the source, saves the unchanged results, then refreshes both
formats and saves and reopens again.

The `pivot_layout` desktop gate follows outline and subtotal layout and the
Values row axis through Save and picker reopen in XLSX and ODS. A source edit
leaves existing output visible until Refresh, then changes Previous in both
workbooks. In XLSX it also changes the order and Running Total of a second
pivot sorted by an absolute position on the opposite axis. The last Save and
picker reopen keep the updated cells. Host fixtures also cover a selected-record
average, bottom subtotals, percentage formats and native undo.


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

The installed Sheets desktop runs O5f and O5g together in
`scripts/sheets-interop-vm-check.sh`. The paired XLSX and ODS fixtures open
through the jailed converter. Editing a source cell recalculates the spill,
`LET`, and `LAMBDA` and changes the matching conditional fills. The test
saves the stale pivot, refreshes it from Data, saves undo and redo results,
and checks the formulas, rule, and pivot after a fresh picker reopen.

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
  Editing, thumbnails, presenting and PDF will all draw from it. For
  left-to-right lists, the renderer measures the shaped label and widens a
  narrow hanging indent before wrapping the paragraph. The label keeps its
  saved position and the paragraph wraps within the adjusted text margins.
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
  widths and hidden columns, row heights and hidden rows, and default sizes.
  Hidden sheets are shown. The 1904 date system is retained for calculation,
  display and native Save As.
- **Data validation.** BIFF8 rules retain their ranges, comparisons, literal or
  formula-backed lists, custom formulas, prompts, alerts and blank and drop-down
  settings. Native Save As keeps them live. Malformed rules and unsupported
  formulas receive loss notices. IME settings receive an approximation notice.
- **Notes and hyperlinks.** Notes keep authors, text and supported inline formatting from the associated text object, including text split across continuation records. Hyperlinks keep workbook locations, URL monikers, textual targets and supported Windows file paths. They remain inactive until the user follows them. Unknown or malformed targets and notes without text receive fidelity notices. [MS-XLS HLink](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/f0b9de34-b147-4f96-a1a3-ac21f6fd48fa)
- **Print.** The worksheet-local built-in names keep a single absolute print rectangle and independently specified title rows and columns. Sheets uses these settings for PDF output after native Save As. Disjoint areas and unsupported built-in names receive fidelity notices. [MS-XLS Lbl](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/d148e898-4504-4841-a793-ee85f3ea9eef)
- **Reported, not carried:** merged cells, charts, pictures, shapes and form controls, conditional formats, frozen panes, pivot tables (their values kept), macros, chart and macro sheets. **Refused** in words: Excel 95 and older (BIFF5 and
  before, in a compound file or bare) and XML or HTML spreadsheets saved as
  `.xls`.
- **Sheets** opens `.xls` through the converter (`officina-convert xls`,
  `--password` as for the others). It doesn't write `.xls` (§16.40, D8): a
  document opened from one has no save target, so Save asks where to keep a
  copy. The workbook status shows the import fidelity notice and explains
  that the source opened read-only.

The [desktop interoperability gate](../../scripts/sheets-interop-vm-check.sh)
with `SHEETS_INTEROP_CASE=legacy` checks this flow using a BIFF8 workbook with
an unsupported merged range. Ordinary Save opens the copy picker. Save As
writes a native `.osheet` that reopens with the edited value and recalculated
formula. The source workbook's bytes remain unchanged.

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

Open: the missing functions above (merged cells: §16.79; comments: §16.80; hyperlinks: §16.81); `.xls` charts and pictures; Excel 4 macro sheets.

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

Open: ~~floating pictures~~ (§16.46), drawn shapes and groups, ~~EMF/WMF
pictures~~ (§16.45), form fields' text and check boxes, legal and chapter list labels,
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

Open: ~~EMF/WMF pictures~~ (§16.45), formula-driven custom shapes (guide formulas), more
preset types, picture and pattern fills, tables (drawn as groups by
PowerPoint), embedded objects and sounds, comments.

### 16.45 Old Windows pictures (2026-11-15): WMF, EMF, BMP

The gap §16.43 and §16.44 left: `.doc` and `.ppt` keep most pictures as
Windows metafiles, and many `.docx`/`.pptx` files do too (the corpus holds
171 WMF and 53 EMF parts, 768 with duplicates). They were dropped or shown as
boxes.

**`officina-pict`** (new crate, MIT OR Apache-2.0) draws them. Its headers
half (`sniff`, `natural_size`, no dependencies) is all the converter uses;
the `render` feature plays the file into a `tiny-skia` canvas (BSD-3,
already in the tree through resvg):

- `dib`: BMP files and the DIBs inside metafiles: 1/4/8/16/24/32 bits,
  run-length 4 and 8, bit fields, top-down, core headers, 32-bit alpha for
  AlphaBlend, colour tables of palette indices (`DIB_PAL_COLORS`) and DIBs
  with no table that take the selected palette.
- `gdi`: the device context: object table (WMF lowest free slot, EMF by
  index, stock objects), save/restore, mapping modes (text, metric,
  English, twips, isotropic, anisotropic), world transforms, pens (cosmetic
  and geometric, dashes, caps, joins), brushes (solid, hatched, DIB
  patterns, monochrome patterns in the text and background colours),
  polygons, Béziers, arcs, chords and pies in GDI's direction, rounded
  rectangles, paths (begin/end, fill, stroke, clip), clipping by rectangles,
  regions and paths, bitmaps with the common raster ops (copy, AND, paint,
  invert, NOT copy, AlphaBlend, TransparentBlt), gradients, text.
- Text is drawn with Liberation Sans, Serif and Mono (regular and bold,
  italic slanted), picked by face name and pitch family; escapement, the
  four alignments, update-current-position, opaque and clipped rectangles,
  underline and strikeout. Per-character advances from the file are kept,
  and a face wider than the file's own is narrowed to them. 8-bit text is
  read in the code page of the font's charset (1250–1258); the Symbol font
  maps to Unicode Greek and maths; dingbat fonts draw nothing.
- Office's way of clipping a fill to a shape (fill with XOR, the shape in
  black, the fill with XOR again) is played as layers: the first fill masked
  by the shape.
- A WMF's frame is its placeable header; else the window it sets before it
  draws (unless a text mapping set afterwards voids it); else the box its
  drawing covers. An EMF's is its header's frame in hundredths of a
  millimetre. EMF+ records are skipped; an "EMF+ only" file is refused.
- Bounded: 64 MB file, 8,192 px a side and 32 Mpx drawn, 2^20 records,
  2^20 points per call, 65,535 objects, 2^18 glyphs, 1,024 saved states, and
  a painting budget of 64 canvas areas; coordinates far outside the canvas
  are pulled in before they reach the rasteriser (its fixed point panics on
  them).

**Where it is drawn.** `navigator-image` gains a feature, `office`, adding
`Format::Wmf`, `Emf` and `Bmp`: metafiles are drawn at the painted size (up
as well as down, like SVG), bitmaps decoded and reduced like the rasters.
Slides, Write, `pergola-richtext` and `officina-pdf` turn it on; Navigator
never does, so web content never reaches these parsers. Slides takes BMP,
WMF and EMF from Insert Picture too.

**Converter.** `.doc` and `.ppt` blips: EMF and WMF inflated from the blip
(a WMF given back the placeable header the blip keeps apart: its bounds and
size), DIBs made BMP files; PICT and TIFF still noted. `.docx` and `.odt`
pictures in these formats are sized from their headers and kept; `.pptx`
and `.odp` show them instead of boxes. Writers name and type `.emf`, `.wmf`
and `.bmp` parts.

**Gates.** 147 distinct pictures (one per corpus document) drawn here and by
LibreOffice, compared side by side cropped to content: equal apart from
fonts (Liberation for Calibri, Tahoma), one ActiveX button drawn with its
real monochrome pattern colours (LibreOffice ignores the pattern), CJK text
(no CJK face yet) and a starburst LibreOffice leaves unclipped. All 768 corpus
metafiles draw in 3.6 s. 40,000 mutated files: no panics (the first run
found the rasteriser's), slowest 246 ms. `.doc` corpus pictures 73 → 193;
`.ppt` 195 → 368 (3 PICT/TIFF notes left). Unit tests build WMFs and EMFs
record by record (fills, text colour, the XOR clip, every truncation) and a
deflated WMF blip; `navigator-image` tests the feature (drawn at the target,
BMP). LibreOffice gates unchanged: `.doc` 31/31, `.ppt` 19/19 without the
POI deck.

Open: a CJK face, PICT and TIFF, SVM (LibreOffice's own metafile, in
`.odp`), EMF+ drawing, glyph-index text (`ETO_GLYPH_INDEX`), embedded fonts.

### 16.46 Floating pictures in `.doc` (2026-11-16)

- **The drawing** (`officina-convert/src/doc/art.rs`). `fcDggInfo` in the
  table stream holds the OfficeArt content: the drawing group with its
  picture store (`0xF007` entries, each holding its picture or naming where
  it lies in the WordDocument stream, `foDelay`), then a drawing for the main
  document and one for the headers. Each drawing's shapes are read by id:
  their picture (property `pib`), whether they are a group, and where the
  shape itself says it goes (`posh`, `posrelh`, `posv`, `posrelv`,
  `fBehindDocument`). Records, entries and shapes are bounded.
- **Anchors.** Each `PlcSpaMom` (and `PlcSpaHdr`) entry that isn't a text box
  becomes a `wp:anchor` at its `0x08` character: the box in twips from
  `SPA`, measured from what the shape names (margin, page, column, character;
  margin, page, paragraph, line) or else from `SPA`'s `bx`/`by`, aligned
  when `posh`/`posv` say so; `wr` gives the wrap (top and bottom, none in
  front or behind, tight, through, else square). The `.docx` reader then
  places it like any floating picture (§16.30).
- **Table cells grow** to hold the pictures their text wraps round
  (`officina-text` row layout), as Word lays them out in the cell; pictures in
  front of or behind the text don't. This applies to `.docx` and `.odt` too.
- **Not yet**: drawn shapes, WordArt and groups (noted as dropped; Write has
  no shapes yet), cropping, pictures in headers (Write's headers keep only
  their text).

Corpus: `.doc` pictures 193 → 246; LibreOffice's renders of the floating
picture test files (POI 58804, Bug47731, LibreOffice tdf132094, tdf160049,
tdf162541) place them the same. 20,000 mutated files with floats clean
(slowest 12 ms). LibreOffice gate 31/31.

### 16.47 Each section's own page (2026-11-16)

- **Model.** `Section.page: Option<PageSetup>`: a section's own size and
  margins, `None` for the document's. The native file carries it after the
  section (paragraph extension bit 3). A section on a page of another size
  starts a new page even when continuous.
- **Pagination.** Each page takes its section's setup (`Page.setup`); a
  section's paragraphs are laid out at its own column width. The painter,
  PDF export (a media box per page) and the editor view (pages stacked at
  their own sizes, centred on the document's; hits and caret per page) read
  it from the page.
- **Files.** `.docx`: each `w:sectPr`'s `pgSz`/`pgMar` read into its section
  (the last section's is the document's), written back per section with
  `w:orient`. `.odt`: a paragraph starting a master page of another size
  starts a section with that page; each section page is written as its own
  page layout and master page (`Section_20_Page_N`), the document's again
  where a section returns to it. The body's first paragraph is now found
  inside sections and lists too. `.doc` gets it through `.docx` (§16.43).
- **In Write**: Insert menu Portrait, Landscape, A4 Paper, Letter Paper,
  Normal and Narrow Margins act on the caret's section (one undo step): a
  one-section document changes its own page; in one of several the section
  gets a page of its own, dropped again when it matches the document's.
- **Not yet**: headers and footers stay the document's.

Gates: LibreOffice both ways across formats 8/8 with a landscape section;
Write's other LibreOffice gates unchanged (docx 26/26, odt 52/52, doc 31/31,
floats 8/8, contents 16/16); 2,546 corpus documents paginate with no panic,
85 of them with pages of more than one size, and sample files' portrait and
landscape pages run in LibreOffice's order.

### 16.48 Preset shapes from the standard's definitions (2026-11-16)

- **Definitions.** All 187 of ECMA-376's preset shapes (Part 1 §20.1.9,
  `presetShapeDefinitions.xml`) are drawn from their definitions, kept
  compact in `officina-slides/src/presets.txt` (made by
  `tools/gen_presets.py`). LICENCE: the definitions are the standard's,
  taken from the copy Apache POI ships under Apache-2.0; the evaluator is
  ours.
- **Evaluator** (`officina-slides::presets`): adjust values (the file's
  override the defaults), guides by the 17 shape-guide formulas over the 42
  built-in variables, then paths of moves, lines, quadratic and cubic
  Béziers and arcs (angles as seen, turned into the ellipse's parameter,
  Béziers of at most a quarter turn). At most 1,024 guides and 4,096 path
  commands a geometry. The hand-drawn shapes stay only as a fallback.
- **`.pptx` custom shapes with guides**: `a:custGeom` with `a:gdLst` or
  `a:avLst` goes through the same evaluator at the shape's size; plain
  custom paths are read as before. Written back as fixed paths.
- **`.ppt`**: every MSO shape type with a DrawingML counterpart maps to it
  (all but WordArt, which stays text, and freeforms); connectors of every
  kind draw as lines, not filled. MSO's curved connector 2 was mapped to 3.
- **Not yet**: `.ppt` shapes with their own guide formulas (one corpus deck,
  22 shapes) still draw as rectangles; `.ppt` adjust values are not carried
  over (defaults used); text rectangles of presets are the shape's box.

Gates: all 187 presets rendered and checked by eye; `.ppt` corpus shapes
that drew as rectangles (flowchart collate, braces, arcs, striped and
curved arrows, magnetic disks and others, 113 shapes) now draw as
themselves; slides corpus 739 files, 0 panics, slide counts equal to
LibreOffice's 666/666, 99.38 % of its words, round trips unchanged; `.ppt`
LibreOffice gate 19/19.

**Gradients on turned shapes** (same day): ours turn with the shape. A
`.pptx` gradient with `rotWithShape="0"` (as LibreOffice writes them) is
on the page, so the shape's turn is taken off its angle; `.odp`
`draw:angle` is on the page too as LibreOffice reads and writes it, so the
turn is added on writing and taken off on reading. The slides LibreOffice
gate, which had fallen to 82/84 on a 30° turned box, is 84/84 again.

### 16.49 Headers and footers of their own paragraphs (2026-11-16)

- **Model.** `HeaderFooter.paras`: a header's or footer's own paragraphs
  (formatted text, several lines, pictures in line), at most 64; `text`
  and `align` stay as its one-line form (the page-number footer Write makes
  still uses it). `{page}` and `{pages}` stand for the page number and
  count in either; filled in per page with the runs moved to match, so a
  bold page number stays bold.
- **Pages.** The header is laid out at the content width from half an inch
  below the page's top edge down, the footer up to half an inch above its
  bottom. One taller than its margin pushes the text down (or up), as
  Word and LibreOffice do: the page's margin grows to hold it (at most half
  the page). Painting, PDF export and the editor view all follow.
- **Native file.** A MARGINS chunk (12) holds the paragraphs; the page
  setup operation in the journal carries them too.
- **Files.** `.docx`: the header and footer parts read with their own
  relationships (pictures in line, anchored ones in line too), page fields
  as placeholders; written as paragraphs with `PAGE`/`NUMPAGES` fields in
  the run's own format and the pictures beside the part. `.odt`: the master
  page's header and footer read the same way; written into the master page
  with their automatic styles and pictures in `styles.xml` (names prefixed
  `MH`/`MF` so they don't meet the body's). `.doc` gets them through
  `.docx` (§16.43).
- **Tables in headers**: headers lay out no tables, so each row becomes one
  line, its cells apart by tabs (a cell a paragraph stacked a three-column
  row three lines high and pushed the text far down).
- **First and even pages** (same day): `TextDoc.first_page` and
  `even_pages` hold their own header and footer (either none: blank there);
  `margins_of(page)` picks: the first page's on page 1, even pages' on even
  pages, else the default. Pagination grows each page's margins for the
  header it shows and won't take old pages whose number moved by one when
  even pages differ. `.docx`: `w:titlePg` with the `first` references and
  `w:evenAndOddHeaders` with the `even` ones, both ways (a kind with none
  written as a blank part). `.odt`: `style:header-first` (and
  `loext:header-first`) and `style:header-left`, both ways (a blank one as
  an empty paragraph, as LibreOffice writes it; `style:display="false"`
  there would turn the region off). Native file: after the MARGINS chunk's
  paragraphs. Gates: LibreOffice both ways across formats 4/4 with a
  blank-header title page and even pages' own; LibreOffice's PDF and ours
  show the same header and footer on pages 1–3; 2,551 corpus documents, 149
  with a first page's own, 34 with even pages' own, all through the native
  file unchanged, no panics.
- **Editing in Write** (same day): Insert ▸ Header and Footer… opens a
  two-row bar: the header and footer as text (paragraphs marked ¶), All
  Pages / First Page / Even Pages, Different first page and Different even
  pages (starting blank, as Word does), a Page Number button and Done. Each
  change is one undoable `SetMargins` operation (journal tag 15). An edit
  keeps the format round the change (`Para::retext`: unchanged start and
  end keep their runs, new text takes the format before it, never a
  picture's or field's), new lines take the last line's; a header that was
  one plain line stays one. Pagination now starts again when a header or
  footer changes, not only the page setup.
- **Not yet**: anchored pictures and drawn shapes in headers drawn in line or left out;
  per-section headers (the first page's is the document's first page only);
  formatting inside a header from the bar (it keeps what the file had).

Gates: our files through LibreOffice both ways across formats 4/4 (lines,
picture and its size, bold, page fields keeping their format); round trips
through `.docx`, `.odt`, both in turn and the native file; LibreOffice's PDF
of a tall header matches our page (the text pushed down alike); Write's
other LibreOffice gates unchanged (docx 26/26, odt 52/52, doc 31/31,
sections 8/8, floats 8/8, contents 16/16); 2,551 corpus documents read,
paginate and paint with no panic, 379 with headers or footers of their
own paragraphs, 26 with pictures in them (checked against LibreOffice's
renders on samples), 180 with margins grown for them.

### 16.50 Drawn shapes and text boxes in documents (2026-11-16)

- **Shared outlines.** `officina-shapes` (new, no dependencies) holds the
  outline model (`Geom`, `Path`, `PathCmd`), the 187 preset definitions
  (§16.48) and their evaluation, moved out of `officina-slides`, which
  re-exports them. The presets now carry each definition's text rectangle
  (`Def::text_rect`, 182 shapes; from the same Apache POI data), so an
  arrow's or ellipse's text sits where Word and LibreOffice put it.
- **Model.** An object run with `ObjectRef.shape` points into
  `TextDoc.shapes` (a `Store<ShapeData>`, at most 65,536) instead of the
  pictures: outline, fill (RGB), line (colour, width, solid / dot / dash /
  dash-dot / long dash, arrowheads at each end: triangle, open, stealth,
  diamond, oval), turn and flips about the middle, insets, text anchored
  top / middle / bottom, its own paragraphs (at most 256) and a group's
  shapes (frames as fractions of the group's, at most 8 deep and 1,024 in
  one). Placed in line or floating exactly as a picture is (§16.30).
- **Pages.** The page painter emits `Item::Path` (fill, then line with
  dashes; arrowheads as their own marks) for each outline and lays the text
  out upright in the outline's text rectangle less the insets; groups draw
  their shapes in order. The screen (`pergola-richtext`) and PDF export draw
  the paths (`folium-content` gained the dash operator).
- **Native file.** A SHAPE chunk (13) per shape; attribute ext bit 5 marks a
  shape object. Loading refuses a shape reference the file lacks, a shape
  inside a shape's text and groups past the limits.
- **`.docx`.** `wps:wsp` and `wpg:wgp` both ways: preset and custom
  outlines (through the `.pptx` reader's drawing code, theme colours from
  the document's theme), solid fills (a gradient's middle stop), lines with
  dashes and arrowheads, `bodyPr` insets and anchor, `txbx` paragraphs,
  group child frames (`chOff`/`chExt`). Shapes in headers, notes and other
  shapes' text stay pictures-or-nothing as before.
- **`.odt`.** Read: `draw:custom-shape` (enhanced geometry, `ooxml-…`
  presets kept), `draw:rect`, `ellipse`, `circle`, `line`, `connector`,
  `polygon`, `polyline`, `path`, text-box frames and `draw:g`, with
  `draw:transform` turns and mirroring, graphic styles (falling back to the
  default graphic style), markers mapped to arrowheads, dash styles by
  kind. Written: custom shapes with `ooxml-…` types and the preset's text
  area, `draw:line`/`draw:connector` for lines, `draw:g` for groups, an
  automatic graphic style per shape, dashes and markers in `office:styles`;
  floating objects now say `style:flow-with-text="false"` so LibreOffice
  places them where Word does (a group left of the margin stayed inside
  without it).
- **Not yet**: text turned with its shape (drawn upright); paragraph
  shading inside shapes; WordArt as plain text; picture and gradient fills
  as one colour; pictures inside groups; transparency; shapes in headers;
  no shape drawing or editing in Write yet.

Gates: our files through LibreOffice both ways across formats 4/4
(`shapes_lo`: a turned callout with dash-dot line and bottom text, a
connector with oval and triangle ends, a custom outline, a group of an
ellipse with text and a flipped arrow); `.docx` and `.odt` round trips keep
every field (`tests/shapes.rs`); five LibreOffice test documents (text
boxes, dash kinds, nested groups, an organisation chart, a circuit drawn
from 30 shapes) render as LibreOffice renders them from the original and
from our `.docx` and `.odt`; corpus: 1,893 documents read, paginate and
paint with no panic, 308 with 928 shapes, every one kept through our own
`.docx` and `.odt`; Write's other LibreOffice gates unchanged (docx 26/26,
odt 52/52, floats 8/8, headers 4/4, slides 84/84).

### 16.51 Drawing shapes in Write (2026-11-17)

- **Operation.** `TextOp::SetShape { id, shape }` puts a shape in, changes
  it or takes it out (inverse: the old one; journal tag 16). It refuses
  shapes past the limits, paragraphs with attributes or styles the
  document lacks, and any object in a shape's words (no shape in a shape);
  the anchor paragraph's page is painted again.
- **Editor.** `insert_shape(preset)` draws a preset (or `"textBox"`,
  `"line"`, `"arrow"`) floating beside the caret's paragraph with square
  wrap, in one undo step with its anchor run: shapes come blue with a
  darker line and words in the middle, text boxes white with a thin black
  line and words at the top, lines blue (the arrow with a triangle end).
  `shape_at_caret`, `set_shape(label, f)` (one undo step a change),
  `shape_text` / `set_shape_text` (a line a paragraph; the format round an
  edit kept by `TextDoc::shape_paras`; new words white and centred in a
  dark shape) and `resize_object` (pictures too).
- **Write.** Insert ▸ Text Box and Insert ▸ Shape…; the shape bar (two
  rows, where the header bar goes) offers twelve shapes when the caret is
  on none; on a shape it shows its words (paragraphs marked ¶), Fill and
  Line menus (none or nine colours), Thinner / Thicker (Office's widths),
  Dashes, Arrowheads (none, end, both), Turn Left / Right (15°), Wider /
  Narrower / Taller / Shorter (a fifth of an inch) and where the words sit.
  The words go in before any other change; the bar follows the caret's
  shape. The wrap and position items of the Insert menu work on shapes as
  on pictures. Crash recovery replays shapes from the journal.
- **Text turned with its shape**: not on screen yet. Fresco draws glyphs
  as upright atlas bitmaps (a run moves with its group but does not turn),
  so turned words wait on the Fresco renderer; Write draws them upright
  in the turned shape's middle, on screen and in PDF alike.
- **On the page** (same day): a press on a floating shape or picture
  picks it (the caret goes to its anchor; behind-text ones only where no
  line is); the picked one gets a frame and a corner handle. Dragging it
  moves it (a ghost frame follows; on release one undoable Move: placed
  from the margin across, from its paragraph down when it was, else from
  the margin); dragging the corner sizes it (a picture keeps its
  proportions, the top-left stays put). With the shape bar open, a click on
  another shape offers Edit This Shape.
- **Turning and objects in line** (same day): a picked shape has a turn
  handle on a stalk above its middle; dragging it turns the shape by the
  angle swept about its middle, to the whole degree, snapping to 15° steps
  within 3° (one undoable Turn). A picture or shape in line with the text
  is picked by a click (framed) and dragged to where it is let go (a drop
  caret shows the place; one undoable Move; not while changes are tracked,
  and not between a table and the body).
- **Turned words** (2026-11-17, on Fresco's turned glyph runs, 1f7273b):
  a shape's words are laid out upright about its middle, then turned with
  it: each run's origin turned about the middle and the run given the
  turn (`GlyphRun::turn`, Pergola's `Node::Glyphs { turn }`), underlines
  and highlights becoming turned outlines. A flip doesn't mirror words;
  flipped upside down they turn half round, as Office draws them. The PDF
  writes the turn in the text matrix. Slides' turned frames turn their
  runs the same way (a mirrored group keeps them upright).
- **Adjust handles** (same day): the standard's handle definitions
  (`ahXY`/`ahPolar`, 243 handles over the presets) are read with the
  shapes; a picked floating shape shows them as yellow dots. Dragging one
  sets the shape's adjust values so the handle lands as near the pointer
  as the shape allows: each value searched across its range (64 samples,
  then halving steps), handles that move two values twice over; a turned
  or flipped shape's pointer is read back into its own box first. One
  undoable Adjust; kept in `.docx`/`.odt` as the preset's adjust values.
- **In Slides too** (same day): the selected drawn shape shows the same
  yellow handles on the canvas; a drag shows the outline live and lands as
  one undoable Adjust (inside groups and on turned shapes as well).
- **Not yet**: adjust handles on shapes in line with Write's text, moving
  the anchor with the shape, grouping in Write.

Gates: every preset's every handle lands on itself when dragged there,
an arrow's head and a callout's pointer dragged (also turned a quarter);
engine (set, undo, refusals), native journal op round trip, editor
(insert, words keeping their format, fill, turn, size, undo, text box,
arrow), Write's bar (both modes, every command, undo a step each, bars
sharing their place), and a crash recovery that keeps a shape's words
and fill; the view: a click picks a shape, a drag moves it 40 px left and
30 px down within 2 px, undo puts it back, its corner sizes it 50 px wider
and 20 px shorter with its corner fixed, its turn handle pulled round to its right turns it 90°; a picture in line dragged into another paragraph moves there and one undo puts it back; officina-text 68, officina-convert, pergola-richtext 28 and
Write tests all pass; twelve shapes drawn by the editor look right in our
PDF.

### 16.52 Paragraph styles in Write (2026-11-17)

Write's Style menu offered five fixed names, so a document's own styles
(a `.docx` full of them) could not be picked, changed or added to.

- **Engine**: `TextOp::SetStyle { id, style }` changes a style, adds one
  one past the last, or takes the last out (`None`, only when nothing
  uses it: body, headers and footers, notes, shapes, deleted or
  conflicting text, other styles' parent or next). Refused: an empty or
  taken name, a reference past the table, a loop through parents, a
  paragraph style turned character style, Normal based on something.
  Paragraphs of the style or styles based on it are laid out again
  (layouts are keyed on the resolved format already). Journal tag 17;
  the autosave reader counts a style added earlier in the same step, so
  New Style then the paragraph on it replays.
- **Editor**: `para_styles`, `style_at_caret`, `change_style` (one undo
  step), **Update to Match** (the caret's style takes the selection's
  paragraph settings and the characters' font, size, weight, slant,
  underline, strike, colour and highlight; the selected paragraphs drop
  their own look and keep lists, cells and sections) and **New from
  Selection** (based on the caret's style, next itself, the selection
  takes it); each one undo step.
- **Write**: the Style menu lists the document's paragraph styles, then
  *Edit Styles…*, which opens a two-row style bar on the caret
  paragraph's style: its name (Rename), New from Selection, Update to
  Match; size a point at a time, bold, italic, underline, alignment,
  space above and below; its font (the document's, IBM Plex Sans or
  Mono: the pinned set), what it is based on (Normal on nothing) and the
  style Enter gives after it. Moving to a paragraph of another style offers
  to switch to it. Styles are saved in `.owrite`, `.docx` and `.odt` as
  before.
- **Refusals leave no trace**: a change the engine would refuse (a loop
  through parents, a taken name) is checked first (`style_fits`), so it
  adds no empty undo step and the bar says why.
- **Character styles** (same day): the bar switches between Paragraph
  and Characters. On characters it edits the character style of the text
  at the caret (rename, size, bold, italic, underline, font), makes one
  from the selection (the selection takes it and drops its own font,
  size, weight and colour) or updates one to match, and applies one or
  none from a menu; each one undo step. Runs carry it as before
  (`CharAttrs::style`, `w:rStyle`, ODF text styles).
- **Not yet**: deleting a style (its index is in every
  paragraph), a styles pane with previews, more fonts than the pinned set.

Gates: engine (change, add, remove, every refusal), journal op round
trip, editor (update to match moves direct format into the style, new
style, rename refused when taken, undo/redo), Write's bar (every command,
four steps four undos, the menu lists a new style), and a crash recovery
that keeps a new style and the paragraph on it.

### 16.53 Moving and editing charts in Sheets (2026-11-17)

A click on a chart picks it: a ring and a corner handle show. Dragging
the chart moves it, dragging the corner handle of a picked chart sizes
it (the top-left stays), and the new place is stored as a cell anchor
(cell plus offset in points), so it follows row and column sizes as
before. Each move or size is one undo step ("Move Chart", "Size
Chart"), journaled like every grid op.

While a chart is picked a chart bar sits over the formula bar: kind
(column, bar, line, area, scatter, pie, doughnut), title, legend place
(right, bottom, top, left) and stacking (side by side, stacked,
percent; not offered for pie, doughnut or scatter), Delete and Done.
Each change is one undo step. Delete or Backspace deletes the picked
chart, Escape or a press elsewhere puts it down.

Pergola's `GridView` does the picking for any float marked `movable`
(`GridAction::PlaceFloat`, `GridAction::DeleteFloat`); tables' and
filters' floats stay fixed.

Edit Chart opens a series editor from the chart bar or Data. Each series takes a
one-row or one-column value range, an optional equally sized category or scatter
X range, and a label typed or linked to a cell. A range may refer to another
worksheet. The editor adds or removes series and sets category/X and value/Y
axis titles. Column, line and area series can share the same axes in a combo chart
with standard grouping. Apply records the complete chart edit as one undo step.

The chart round-trip gate exercises charts with quoted cross-sheet sources in
XLSX and ODS, including linked series labels and axis titles. XLSX reads series
order independently of the chart type groups. ODS uses one shared categorical
range and preserves cell-linked series labels, with a Save notice for typed
series names or differing category ranges.

### 16.54 Choosing filter values from a list in Sheets (2026-11-17)

Data ▸ Choose Values in Column … (the cursor anywhere in a filter
column, its header too) opens the column's value list at the window's
right, over the grid: each distinct shown text once (sorted, "(Empty)"
for empty cells) with how many rows show it, ticked when the filter
keeps it now. A search field narrows the list; Select All and Clear act
on the values the search shows. Apply makes the ticked values the
column's condition, one undo step ("Filter"); ticking every value drops
the column's condition, ticking none is refused in words. Cancel leaves
the filter as it was. The list draws only the rows in view (Pergola
`VirtualList`) and offers at most 10,000 values; a column with more
says so and points to Show Only and Hide.

### 16.55 Deleting styles in Write (2026-11-17)

The style bar's Delete deletes the caret's paragraph style, or with
Characters picked its character style. Paragraphs of a deleted paragraph
style take the style it is based on (Normal if none); text of a deleted
character style takes its base character style, or none. Styles based on
it are then based on its base; styles that Enter followed with it keep
their own style after Enter. Normal can't be deleted. A style used in a
header, footer, note, shape or tracked deletion is refused in words until
those change. The whole deletion is one undo step and is kept by crash
recovery.

The engine op `TextOp::PlaceStyle { id, style, stale }` takes an unused
style out of the middle of the table or puts one back. Every style id
the document holds follows, wherever it is: paragraphs in the body,
headers, notes, shapes and deleted text, styles' base and next, and the
attribute table's character styles. The op inverts exactly, including
attribute entries no text uses that still named the style (`stale`).
Journal tag 18.

### 16.56 The styles pane in Write (2026-11-17)

Style ▸ Styles Pane shows a pane at the window's right, and the pages
make room for it. It lists the document's paragraph styles, then "No
Character Style" and the character styles. Each name is drawn in its
style's own look: weight, a monospace face for code fonts, and the size
kept between 11 and 20 px, with the size in points beside it. The caret
paragraph's style and the character style at the caret are highlighted.
A click on a paragraph style puts it on the selected paragraphs; a click
on a character style (or "none") puts it on the selection. Each is one
undo step. The list draws only the rows in view, so documents with up to
4,096 styles scroll smoothly. Close or the menu item hides the pane.
Editing styles stays in the style bar (§16.52, §16.55).

### 16.57 Asking before closing with unsaved changes (2026-11-17)

Write, Sheets and Slides no longer close silently with unsaved changes.
frescod only asks a window to close (`WindowCloseRequested`), and the
app decides. With unsaved changes a bar under the toolbar asks "Save the
changes to “name” before closing?" and offers Don't Save, Cancel and
Save.

* Save saves to the document's file and closes. A document without one
  (new, or read from a format the app doesn't write) opens the picker's
  Save As and closes once it is saved. Cancelling the picker leaves the
  window open.
* Don't Save closes and removes the autosave folder, so the changes
  don't come back at the next start.
* Cancel puts the bar away.

For Sheets, a pending Save or Save As follows §4.2, including on repeated
close requests. Otherwise a second close request while the bar shows (or
while the picker is up) closes as before. The changes stay in the autosave folder and come back
at the next start, so nothing is lost when the window has to go.

### 16.58 Paragraph direction in Write (2026-11-17)

Write's alignment menu sets the selected paragraphs' direction after the
four alignments: Direction from Text, Left to Right or Right to Left. It
sets `ParaProps.rtl`, which the bidi layout in `officina-text` uses and
which `.docx`, `.odt` and the native file keep. Each change is one undo
step and is kept by crash recovery.

### 16.59 Icon sets in Sheets (2026-11-17)

Conditional formats gain Excel's icon sets: all 20 (3 to 5 icons: arrows, grey arrows, flags, traffic lights, signs, symbols, ratings, quarters, boxes, triangles, stars). A rule holds the set, one threshold per icon after the first (number, percent, percentile or formula, each "at least" or "more than"), reverse order and icon only. A cell gets the highest icon whose threshold it meets; the icon is drawn at the left of the cell by `GridView` from a small set of shape kinds coloured per set, so no picture files are needed. Native files keep the rule (codec tag 10).

`.xlsx` reads and writes `<iconSet>` with `reverse`, `showValue` and `gte`. `.ods` reads and writes LibreOffice's `calcext:icon-set`. Gate (`examples/condfmt_lo.rs`, two icon columns added): every icon cell matches in .xlsx → .xlsx, .ods → .ods and .ods → .xlsx through LibreOffice. LibreOffice's ODF form keeps neither reverse order nor icon only (it writes show-value but does not read it back) and drops strict thresholds in both formats; those are its limits, written down in the gate.

### 16.60 The conditional-format dialog in Sheets (2026-11-17)

Data ▸ Conditional Formatting… opens a panel at the window's right on the rules whose ranges meet the selection, first applied first (with nothing there it opens straight on a new rule). A rule is picked by a click; New Rule, Edit, Delete, Up and Down work on it. The rule form has Apply To (cells, ranges, whole columns or rows, several split by commas), the kind (cell value, text, formula, top or bottom, average, duplicates, blanks, errors, colour scale, data bar, icon set), its comparison or choice (Excel's twelve colour scales, six bar colours, all twenty icon sets with reverse and icon only), one or two values, Excel's six ready-made looks with a sample, and Stop If True. A value typed for a comparison is a formula when it starts with `=`, a number when it reads as one, else text. What the form doesn't show is kept when a rule is changed: custom looks and colour scales show as "As it is", icon thresholds stay while the set does, an average rule keeps its standard-deviation line. New rules go first, as in Excel; a changed rule keeps its place and its format takes the new ranges. Errors are said in the panel and nothing changes.

Each change is one undo step: `GridOp::CondFormats` swaps in the whole list, held as the native file's bytes so the journal can replay it (a list naming a sheet no longer there changes nothing). Tests: every kind and choice saves, loads back into the form and saves to the same rule; a panel test makes, changes, picks by click, reorders and deletes rules and undoes each.

When recalculation finishes in the first deferred slice after an edit, the desktop schedules a fresh frame for the updated values and formats.

The installed desktop gate `scripts/sheets-interop-vm-check.sh` with `SHEETS_INTEROP_CASE=condfmt` checks fill colors in XLSX and ODS before and after editing a shared threshold and a calculated score. It verifies rule priority when both rules match, then checks the saved rule order, formula references and visual result on reopen. A further edit verifies that the reopened rules still respond.

### 16.61 The pivot field list in Sheets (2026-11-17)

Data ▸ Pivot Table Fields… (with the cursor in a pivot table or its source) opens a pane at the window's right: the source's fields, each marked with where it is, then buttons that put the picked field in Rows, Columns, Filters or Values, take it out, or move it earlier or later in its area; a line per area; each value field's summary function (all eleven); and switches for the grand total row, the grand total column and subtotals. A field going to Values is summed when most of its cells are numbers, else counted; a field leaving an axis and coming back keeps its hidden items. Each change refreshes the pivot and is one undo step (`Workbook::pivot_op`); a change that would leave the pivot with no row, column or value field, or that the refresh refuses (cells in the way, overlap), is said in the pane and nothing changes. Compact layout is still to come.

### 16.62 Compact pivot layout (2026-11-17)

`Pivot.compact` lays a pivot out in Excel's compact form: the row fields share one label column headed "Row Labels" (and "Column Labels" over the column items), each outer item gets a row of its own holding its subtotal (empty with subtotals off), and the items inside it follow, indented one step a level; the outer items' rows are bold. `Shape::indent` says how far a label cell is indented and `CellView.indent` draws it (12 px a step). The field list has a Compact Form switch; new pivots stay tabular. Native files keep it (flag bit 3); `.xlsx` writes compact form as Excel does (no `compact="0"` on the table or its fields) and reads it when the table and its row fields are compact (outline form is shown tabular, with a note); `.ods` writes and reads LibreOffice's `loext:layout-mode="compact-layout"`. LibreOffice gate (`COMPACT=1 examples/pivot_lo.rs`): the compact pivot comes back compact with the same cells in all four runs. The one difference left is LibreOffice's: its `.xlsx` export drops a pivot's "no grand total column" (`rowGrandTotals="0"`), with or without compact form.

### 16.63 Sections on odd and even pages (2026-11-17)

A section that starts on an odd (or even) page now gets a blank page before it when the page it would start on is the other kind, as Word and LibreOffice lay it out. The paginator inserts the blank page (`Page.blank`, no text, the section's start as its start, headers and footers as on any page); an edit that moves the page count moves the blanks, and pages only line up again with the same page parity. Write's Insert menu gains Section Break to Odd Page and to Even Page; a break put at the start of a paragraph that already starts a section changes how that section starts. `.docx` keeps them as before (no longer reported as approximated); `.odt` writes them as LibreOffice does, the section's master page on a page layout kept to right or left pages (`style:page-usage`) with the plain master after it, and reads them back. LibreOffice gate (`examples/sections_lo.rs`, an odd/even document added): 14 of 16 checks; the two misses are LibreOffice's own `.docx` export writing the last section's even-page start as a next-page one.

### 16.64 Outline pane in Write (2026-11-17)

Write's Style menu gains Outline Pane, shown beside the pages in the styles pane's place (one or the other). It lists the document's headings (paragraphs with an outline level, from their style or their own setting; table cells and contents entries left out), set in by level, the heading the caret is under marked. A click on one takes the caret there and scrolls to it. Up and Down swap the caret's heading, with everything under it, with the one before or after it at the same level; not across a higher heading, a section break, or while changes are tracked. Promote and Demote move the heading and those under it a level, to the "Heading n" style when the document has one and by the paragraph's own outline level when not (a heading back at its style's level drops that setting); never past levels 1 and 9. Each change is one undo step and is replayed by crash recovery like any editor action. Tests: `pergola-richtext` `outline_moves_and_shifts_headings`, `atrium-write` `outline_pane_lists_moves_and_shifts_headings`.

### 16.65 Continuous view and read mode in Write (2026-11-17)

Write's Tools menu gains Continuous View and Read Mode, the last two of §7.1's views. Continuous view keeps the pages as laid out but shows only the part of each between its top and bottom margins, the next page's text following a thin seam, so a draft reads as one column; headers and footers are hidden, sections with their own page sizes keep their own widths, and clicks, selection, the caret and scrolling work as in page view (the view's page geometry carries what each page cuts). Read mode gives the window to the pages: no toolbar, bars, panes or status line, the page fitted to the window, Page Down, Space, the right and down arrows turning on, Page Up, Shift+Space, Backspace, the left and up arrows back, Home and End to the ends; nothing edits (keys and pointer drags are not passed to the editor and no caret shows). A small page count and Leave Read Mode stay; Escape also leaves, and the caret goes to the top of the page being read. Both are view settings, not saved with the document. Tests: `pergola-richtext` `continuous_and_read_views`, `atrium-write` `continuous_view_and_read_mode`.

### 16.66 Handles on shapes in line in Write (2026-11-17)

A picked shape that sits in line with the text now shows its yellow adjust handles as a floating one does (an arrow's head, a callout's pointer), and dragging one reshapes it the same way, one undo step; the editor finds a shape's box from the page's floats or, for one in line, from where its character sits on the line. Pictures and shapes in line also gain the corner handle that floating ones have: dragging it sizes the object (pictures keep their proportions), one undo step, the object staying in line. Test: `pergola-richtext` `a_shape_in_line_takes_its_yellow_and_corner_handles`.

### 16.67 East Asian language in Write (2026-11-18)

Write's alignment menu, after the paragraph direction, sets the East Asian language of text (the model's `lang_ea`, added with fonts.md §5): Asian Text as Document, Chinese (Simplified), Chinese (Traditional), Japanese, Korean. It picks which Han forms the same characters are drawn in. With a selection it sets the selected text; with none, the caret's paragraph and what is typed next. One undo step; the menu marks the language at the caret. `.docx` and `.odt` keep it (`w:lang w:eastAsia`, `style:language-asian`). Test: `atrium-write` `east_asian_language_is_set_from_the_menu`.

### 16.68 Compare workbooks in Sheets (2026-11-18)

The first step of spreadsheet change review (§16.22's "spreadsheets get the same idea later"): Data ▸ Compare with… asks for another copy (`.xlsx`, `.xlsm`, `.xls`, `.ods`, `.csv`, `.osheet`; read-only) and a Compare pane lists what would turn this workbook into it. Compare, tracking, review, and merge appear together at the top of Data.

- **Engine** (`officina-grid::compare`). Sheets match by native worksheet identity when available, then by case-insensitive name. In a sheet, rows match by their contents: each row is hashed over its cells, values by value and formulas in relative form (printed for a fixed row), so a row that moved still matches; a longest common subsequence of row hashes runs between the common start and end (up to 4 M row pairs, else rows go in order). In each gap between matched rows, rows are paired by how many cells they share (a dynamic programme over the gap, up to 10,000 pairs) and compared cell by cell; the rest are rows added or removed, in runs. Changes include cells, row additions and removals, worksheet additions, removals, and identified renames, and the workbook parts listed below. Spilled cells belong to their formula. At most 100,000 changes are listed. 200,000 rows with a fill compare in 0.6 s.
- **Taking.** Take applies one entry. For row additions and removals, Take All can replace that sheet’s cells, formula fills, and styles together. Take All applies the accepted changes as one undo step. Added tabs bring their sparse contents, settings, rules and placed objects. A set of added tabs is staged before their formulas are installed so they can refer to each other. Deleting a tab uses the ordinary worksheet deletion edit, including references rewritten to `#REF!`. The last tab stays in place.
- **The review pane.** Compare with…, Review Tracked Changes, and Merge Conflicts use the same lane beside the grid. Each opens with collapsed groups by sheet and change type, with counts and a Workbook group for changes without a local sheet. Expand a group to see its individual items. The list renders only nearby rows, even when a group contains thousands of changes. Selecting an item navigates to its location and opens its full summary in a separate, scrollable detail area. Cell edits show the full values or formulas under Current and Incoming, or Before and Current when tracking. Conflicts show both sides with separate labels, including the competing operations or the alignment failure for structural conflicts. Take and Keep Mine act on the selected item. Compare offers Take All across collapsed and expanded groups as one undo step.
- **Worksheet identities and order.** Native `.osheet` files preserve an optional `SHEET_IDENTITIES` chunk (26). New tabs have a stable identity. Grid journal operation 25 records it for newly added tabs and restores it when undo brings a tab back. Older files and copies without shared identities match by name. Rename preserves formula, defined-name, chart and rule references by sheet identity and updates qualified cell hyperlinks. Worksheet reordering is one workbook entry with the current and incoming order. Taking it reorders shared tabs while preserving local-only tabs. Take All applies ordering after the other changes.
- **Styles, charts and defined names.** Compare with… compares sparse static style runs on matched rows, including styled blank cells. Taking inserted rows brings their styles, and row and column shifts move styles outside the data extent. Charts are matched by host sheet and top-left anchor, with existing identical charts matched first. A move to a different anchor appears as a removal and an addition. Chart references are remapped by sheet name. Defined names match by case-insensitive name and scope, and review includes their stored formulas. Take, Keep Mine and Take All apply to these entries. Take All combines chart and name changes in one undo step and leaves row changes pending on sheets with kept cell, style or row differences.
- **Conditional formatting and data validation.** Compare with… lists separate changes for each sheet’s ordered conditional formats and validation rules. Take replaces the selected set, including its ranges, formulas, formats or validation prompts. Rules keep their order so overlapping formats and validations keep their priority. Ranges use their stored sheet coordinates, and formula references follow sheet names. If an incoming formula refers to a sheet missing from the current workbook, that set stays pending. Keep Mine retains the selected set. Take All combines accepted sets with other changes in one undo step.
- **Tables and pivot tables.** Compare with… matches these objects by case-insensitive name. Table changes include the range, headers, totals, columns, and stored style. Pivot changes include the source, destination, fields, filters, and layout. Ranges and destinations remap by sheet name. Taking a pivot regenerates its output from the current source data, and removing it clears its output. Generated values stay with their pivot during review. Take All groups accepted objects as one undo step and preserves kept definitions and their areas. Conflicting names and occupied destinations stay pending. A bulk review leaves row replacements pending on sheets with pivot output. Run Refresh Pivot after taking separate changes to its source data.
- **Embedded pictures.** Compare with… pairs drawings by host sheet name and top-left cell, matching identical pictures first. The review flags changes to encoded image bytes, anchor geometry, crop, and rotation, along with additions and removals. Moving a picture to a different cell appears as a removal and an addition. Take preserves the stored picture and remaps its host sheet by name. Link metadata stays inert. Take All applies the selected pictures together within the workbook picture limits, and Undo restores the previous collection.
- **Comment threads and cell hyperlinks.** Compare with… lists added, removed, and changed comments and stored hyperlinks on matched rows, including annotated blank cells. A comment change includes formatting, author and reply details, resolved state, and mention metadata. Take copies the complete comment or the stored hyperlink target. A `HYPERLINK()` formula is reviewed as a cell formula. Taking an inserted row brings its annotations. Keep Mine preserves the selected annotation and leaves bulk row replacement pending on that sheet. Take All groups the accepted changes into one undo step.
- **Merged cells and sheet sizes.** Compare with… flags added, removed, and resized merged ranges by their top-left cell. Merges and row heights follow aligned rows, and column widths follow column positions. Review includes sheet-wide defaults, explicit overrides on blank rows and columns, and the width remembered for a hidden column. Taking inserted rows brings their explicit heights and merges wholly inside them. Merges that cross unresolved row edits are revisited after the rows are taken. Take All keeps overlapping incoming merges pending if they would replace a retained merge and applies accepted layout changes in one undo step. Grid journal operation 24 preserves the axis default and overrides together.
- **Sheet settings.** Compare with… reviews tab color, frozen panes (including their scroll origin), and the auto-filter with its saved hidden rows. Filters and row visibility share one review entry because the workbook stores filter results and manually hidden rows together. Filter ranges and pane boundaries use the stored row and column positions on the matching sheet. Take and Keep Mine apply to each entry independently, including during a bulk row replacement.
- **Tracked cell changes.** Data ▸ Start Tracking Cell Changes captures a sparse baseline and records the change set creator’s name. Data ▸ Review Tracked Changes lists the net edits against that baseline under the saved creator’s name. Edits reverted by undo disappear when the list is reopened. Native `.osheet` files keep the baseline and author in optional chunk `TRACKED_CELLS` (23), and autosave journal record 8 preserves the snapshot when tracking starts between saves. Reopened workbooks continue the same change set. Save routes tracked workbooks from other formats to an `.osheet` copy. The baseline is limited to 16 MiB of estimated memory.
- **Three-way workbook merge.** With the local copy open, choose Data ▸ Merge Copies… and pick the common base and the incoming copy. Sheets aligns worksheet names and axes first, then compares cells and the same workbook parts as Compare. It applies independent incoming edits together in one undo step and preserves local-only edits and changes both sides made identically. Styles split at the three copies’ run boundaries so an independent range can apply alongside an overlapping conflict. Thread and hyperlink changes are keyed by cell, drawings by host and anchor, tables and pivots by name, defined names by scope and name, and conditional formats and validations by ordered rule set on a sheet. Taking a pivot definition regenerates its output from the merged source. Generated spill and pivot values do not appear as separate cell edits.
- **Conflicts.** Competing versions of a workbook part open under its sheet and change type. Current and Incoming are shown in the selected detail. Take Incoming applies that entry as its own undo step and Keep Mine clears its review entry. A pending resolution checks the current state before writing. Worksheet deletion conflicts if the local side changed that tab or a dependent item the deletion would rewrite. Competing edits on different objects at the same drawing anchor, incompatible destinations, unresolved references, and ambiguous or incompatible sheet changes can need manual resolution. Two differently edited, independently added sheets sharing a name are flagged for Keep Mine or manual reconciliation.
- **Inserted and deleted rows and columns.** Native `.osheet` files store sparse identity runs for both axes in optional `AXIS_IDENTITIES` chunks (24). An untouched axis has implicit identities, including blank coordinates. Insertions allocate fresh IDs and deletions remove IDs. Grid journal operation 22 swaps the runs along with the shift, so recovery, undo and redo keep the same identities. An axis is bounded to 4,096 runs.
- **Structural alignment.** Merge Copies compares each branch's surviving identities with the common base. It combines several disjoint shifts on either axis, including shifts of blank rows and columns. Incoming inserts retain their identities. The ordinary sheet shift operation aligns references and metadata in all three copies before comparing the aligned addresses. Shifts share the same undo step as incoming edits. Competing insertions at the same gap, overlapping shifts, and deletions that cover opposing edits are flagged before writing.
- **Merge scope.** XLSX and ODS copies of a common workbook use the same parts merge and match tabs by name. Use native `.osheet` branches for rename tracking and repeated structural shifts. When all three copies lack recorded axis identities, unique unchanged cell inputs and a change in used extent can identify one contiguous splice. Without that evidence the engine compares by address or flags a detected ambiguity. Spill followers and occupied merged cells are guarded. The review plan is limited to 100,000 changes.

Tests: `officina-grid` `compare::tests` (cells, rows and sheets; formulas taken for our row; taking rows and undo; 300 random pairs where taking every change gives the other copy; the 200,000-row timing), `atrium-sheets` `compare_with_lists_and_takes_changes` `tracked_cell_changes_and_three_way_merge_use_undo_and_flag_conflicts`, `reopened_workbook_reviews_tracked_changes_under_the_saved_author`, `autosave_recovers_tracking_started_between_cell_edits`, `structural_merge_combines_both_axes_and_flags_overlapping_edits`, and `reopened_axis_ids_merge_multiple_blank_shifts_and_flag_conflicts`. Grid gates: `blank_axis_identities_survive_native_and_undo` and `styles_follow_matched_rows_and_take_new_row_styles`. App gates: `workbook_review_flags_and_takes_or_keeps_styles_charts_and_names` `workbook_review_flags_and_resolves_conditional_formats_and_validations`, `workbook_review_flags_takes_and_keeps_tables_and_pivots`, `workbook_review_flags_takes_and_keeps_embedded_pictures`, `workbook_review_flags_takes_and_keeps_threads_and_hyperlinks`, `workbook_review_flags_takes_and_keeps_merges_and_sizes`, and `workbook_review_resolves_sheet_settings_across_row_replacement`.

The [desktop review gate](../../scripts/sheets-review-vm-check.sh) opens Compare and both Merge Copies pickers through Scrinium. It checks the collapsed groups and selected details, applies Take and Keep Mine, and reopens the native results to check tracked author and values and the resolved merge.
With `SHEETS_REVIEW_FORMAT=ods` or `xlsx`, the same gate compares 1,202 edits
and style changes across two sheets. Captures show the collapsed groups, the
full selected edit and both values or formulas in the merge detail. Ordinary
Save verifies the independent edits and row-height change before conflict
resolution. The gate takes one cell, keeps another, and takes the conflicting
fill. It saves, undoes and redoes the fill resolution, then reopens via the
picker and verifies the saved values, recalc, size and fill. The comparison,
base and incoming files remain byte-for-byte identical.

The workbook merge regressions also cover 1,200 cell conflicts across collapsed
groups, style changes that partially overlap, worksheet rename/add/remove and
tab order with live cross-sheet formulas, and one-step undo followed by native
Save and reopen.

### 16.69 Inserting and deleting rows and columns in Sheets (2026-11-18)

Sheets had no way to insert or delete rows or columns. `officina-grid::shift::shift_op(wb, sheet, axis, at, n)` now builds that edit (`n` > 0 inserts before `at`, `n` < 0 deletes from it) out of the ops that already exist, so undo, redo, the journal and crash recovery need nothing new:

- The sheet's cells are replaced with themselves moved; formula fills are rewritten row by row and joined again where their relative form is the same (a fill the insert cuts through becomes two pieces that still read `=A1+1`).
- Every reference to the sheet, on it or from other sheets, follows: a reference into deleted cells becomes `#REF!`; a range loses its deleted part, and grows by rows inserted inside it (inserted at its first row, it moves); whole-column ranges don't move with rows nor whole-row ranges with columns.
- Styles move with their cells (inserted rows start plain); tables (a table whose header row is deleted goes; columns inserted inside get new names), charts (data and anchor), pivot tables (source and output), conditional-format ranges, the auto-filter and hidden rows move along.
- Defined names follow as formulas do, and row heights and column widths move with their rows and columns (added the same day: two new ops, `GridOp::Names`, journal tag 9, the names as formula text read against the workbook when applied, and `GridOp::Sizes`, tag 10, a sheet's row or column size overrides swapped whole; test `names_and_sizes_move_too`, which also round-trips the inverse through the journal encoding).
- Conditional-format rules' formulas (cell-is values, expressions, colour-scale, data-bar and icon points) follow too, on the sheet and from other sheets, re-encoded for the format's new first cell; an absolute reference into deleted cells becomes `#REF!` (added 2026-11-18, test `conditional_format_formulas_follow_what_they_read`).

Sheets' Data menu gains Insert Rows Above, Insert Columns Left, Delete Rows and Delete Columns, as many as the selection spans, each one undo step. Compare with… (§16.68) now takes rows added as rows inserted with the other copy's cells, and rows removed as rows deleted, instead of the whole sheet (Take All still takes a sheet with rows added or removed whole).

Gates: `shift::tests` (insert and delete with references on and from other sheets, columns with a table, a fill cut in two, 200 random inserts and deletes undoing exactly, a row inserted into 200,000 rows with a fill: built in 89 ms, applied in 121 ms), `compare::tests` now taking changes one at a time until none are left, `atrium-sheets` `rows_and_columns_are_inserted_and_deleted`.

### 16.70 Responsiveness and a memory budget in Sheets (2026-11-18)

Shankar asked whether each sheet has a memory budget or only the workbook, and that Sheets stay responsive with a huge sheet or many sheets. Measured first (`atrium-sheets/examples/stall_probe.rs`, `stall_heavy.rs`, 4 cores): an edit read by 2 M rows held the UI 54 ms, an edit read by 4,000 sheets 41 ms, but `SUMIF(C:C,C1,B:B)` filled down 100,000 rows held it 132 s and `COUNTIF` 44 s, every row walking the whole column, and recalculation ran on the UI thread (against §8.4). There was no budget at all for typing, pasting or filling; only the file readers had caps.

- **-IF range index.** `officina-grid::eval::ifcache`: while a formula group is evaluated, a criteria range read with an equality criterion (number, text, logical, empty; one column) is walked once into a map from the cells' keys to count and sums, shared by the group's threads and slices; later rows look it up. Keys follow `Criterion::test` exactly (numbers and numeric text by value, text case-folded), and sums are added in the walk's order, so results are the same to the bit (`tests/ifcache.rs`, 13 formulas against the walk). SUMIF down 100,000 rows: two edits 132 s to 0.1 s; COUNTIF 44 s to 36 ms. Ranges that move with the row (`$C$1:C1`) and wildcards still walk; ordered numeric criteria: §16.77.
- **Recalculation in slices.** `Workbook::recalc_for(budget)` stops between groups, or between slices of a long group (8 rows a slice per thread, waves doubling while under 4 ms), and leaves the rest pending; `recalc_pending` and `is_stale` say what is left, and edits in between join the work. Sheets recalculates an edit for 12 ms and its event loop does the rest a slice per turn, so the window keeps answering; cells not yet done show their old value greyed. Saving and Data commands (sort, filter, pivots, compare) finish recalculating first. Slices rather than a thread: the UI reads the workbook directly, and a slice ends within one wave past its budget (8 ms budget: worst call 16 ms over 318 calls in `tests/budget.rs`, ending equal to one full recalculation with an edit made midway).
- **One budget per workbook, not per sheet.** A per-sheet cap would refuse one legitimately large sheet while thousands of small ones could still exhaust memory. `Workbook::memory_bytes()` estimates the cells' memory (chunks, strings, styles, sizes, and every formula cell's result whether computed yet or not) in about 0.2 ms at 20 M cells (167 MB estimated, 197 MB resident). Sheets' budget is half the machine's memory, between 1 and 16 GiB; a change (typing, paste, fill, insert, redo) that takes the workbook past it is undone at once and the status bar says so, so a formula filled down two billion rows is refused before anything computes it.
- Opening a file (converter, load and its recalculation) and reading one for Compare with… run on a thread of their own since the same day (`files::Loading`): the window keeps answering with “Reading …” in the status line, and a password, an error or the document arrives when the thread is done (`a_slow_file_is_read_while_the_caller_carries_on`: a 20,000-row volatile SUMIF file reads for 6 s, each poll under 5 ms).
- At start the window shows at once: a document restored from its autosave (`files::Loading::restore`) or a file named on the command line is read on its thread too (`an_autosave_is_restored_on_its_own_thread`).
- Not yet: the Sheets process has no memory limit of its own until documents run in their own jails, and wildcard and ordered-text criteria still walk their range per row (native files load within the budget: §16.76; ordered number criteria: §16.77).

Gates: `officina-grid` `tests/ifcache.rs`, `tests/budget.rs`; `atrium-sheets` `a_slow_edit_hands_the_window_back_and_greys_what_is_not_done`, `a_change_past_the_memory_budget_is_not_made`.

**Large-workbook check (2026-11-24).** The live
`atrium-sheets/examples/recalc_responsiveness.rs` fixture has 1,048,576 rows and
2,097,152 formula results in an approximately 24.4 MiB native file. Every trial
begins from the same saved copy. It enters 2, scrolls while a dependent result
is stale, then enters 7 before the old work completes. Every cached result is
checked against 7 once recalculation settles. The installed
`scripts/sheets-interop-vm-check.sh` responsiveness profile uses Scrinium to
open, edit, save and reopen the same fixture. It checks the file's stored values
without recalculating them and checks the new document jail's recovery baseline.

Building the entire group's partitions before every slice cost more than the
slice budget on this workbook. The scheduler now partitions only the current
wave and carries the unvisited row runs into the next slice. The full calculation
path still partitions the group once. `tests/budget.rs` verifies disjoint dirty
runs and that the slice counters add up to the rows actually evaluated.

Three baseline and three updated trials ran consecutively with optimized
FreeBSD/arm64 binaries on the same four-vCPU HVF Tessera-root VM. Typing and
scrolling timings begin at event dispatch and end when the app submits its
scene update. Commit timing also includes the first calculation slice.

| Measurement | Baseline median / max | Updated median / max |
|---|---|---|
| Idle typing | 5.65 / 6.16 ms | 5.67 / 5.86 ms |
| Typing with calculation pending | 5.90 / 6.26 ms | 5.79 / 5.90 ms |
| Idle scrolling | 5.83 / 6.32 ms | 5.94 / 6.30 ms |
| Scrolling with calculation pending | 6.51 / 7.65 ms | 6.06 / 7.01 ms |
| Commit | 36.58 / 38.61 ms | 20.54 / 24.27 ms |
| Background calculation slice | 26.16 / 33.62 ms | 14.51 / 25.17 ms |
| Second edit to settled | 762.71 / 868.41 ms | 434.74 / 499.53 ms |

The measured slices can still exceed the 12 ms target. Timing covers app scene
submission in the VM, while the installed check covers input and file lifecycle.

**Visible-first check (2026-11-24).** The grid reports visible cell ranges when
it paints, so the next slice follows the scroll position. Recalculation builds
a bounded priority subset of the same dirty graph, evaluates ready ranges, and
leaves the remaining rows for the regular pass. Whole-workbook recalculation
and Save use the regular dependency pass.

The `visible` mode of `recalc_responsiveness` scrolls twice into stale rows of
the million-row native fixture. In three live Tessera-root trials, all 29
visible rows refreshed in the next slice at both positions while an earlier
offscreen witness stayed stale. The probe checks each visible result and both
formula columns after the calculation finishes. The installed Save and picker
reopen gate checks the cached results again. Grid tests cover shifted
cross-sheet precedents, a wide aggregate waiting for its source, running
totals in both directions, and cycles. Pergola tests cover viewport clipping,
hidden cells, and frozen bands.

**Excel-saved workbook check (2026-11-24).** Four workbook originals come
from the [SpreadsheetBench archive at revision 49b73a9](https://github.com/RUCKBReasoning/SpreadsheetBench/blob/49b73a94775fb489063f60ca1865e3a650079a79/data/spreadsheetbench_912_v0.1.tar.gz).
The paths below are relative to `all_data_912_v0.1/spreadsheet` in that
archive. The packages identify Microsoft Excel in their application
properties and have a cached result for every source formula, including
empty strings.

Run `atrium-sheets/examples/excel_scale.rs` in release mode with
`officina-convert` beside it. Give the probe an original XLSX and an empty
output directory. It takes three trials on the same bytes and checks each
formula cell's result after reopening each export with the Sheets loader.
Open and reopen include the workbook read, subprocess conversion, and
initial calculation. Recalculation is a separate full pass on the loaded
workbook. Save measures the same XLSX encoder as the app followed by a write
and sync to Tessera. The picker, document jail setup, and Scrinium's commit
protocol are outside those timings. The timings are medians from a private
arm64 HVF guest running FreeBSD 16-CURRENT on Tessera with four vCPUs and
4 GiB RAM.

| SpreadsheetBench input | Source formulas | Live formulas after reopen | Open | Recalculate | Save | Reopen |
|---|---:|---:|---:|---:|---:|---:|
| `55427/3_55427_answer.xlsx` | 1,418 | 1,418 | 2,599 ms | 1.0 ms | 64 ms | 2,536 ms |
| `55392/1_55392_input.xlsx` | 16,669 | 16,642 | 6,920 ms | 389 ms | 91 ms | 7,110 ms |
| `31745/1_31745_answer.xlsx` | 42,042 | 42,042 | 3,289 ms | 9.3 ms | 65 ms | 3,097 ms |
| `59932/1_59932_answer.xlsx` | 36,344 | 15,950 | 8,386 ms | 262 ms | 286 ms | 6,537 ms |

All twelve trials kept the Sheets-computed results of live formulas through
reopen. On the installed desktop, ordinary Save through Scrinium and picker
reopen in a new document jail also kept all 76,052 live formula results across
the four cases. The probe's `check` mode compares the cached native recovery
baselines from before Save and after picker reopen without recalculating.
Compare original and exported packages with
`python3 scripts/sheets-xlsx-cache-check.py ORIGINAL.xlsx EXPORT.xlsx …`.
This checks what the packages actually stored before any reader can
recalculate and treats a frozen formula as a fidelity difference even when
its value matches:

| Case | Matching live formulas | Different live results | Excel values kept in static cells |
|---|---:|---:|---:|
| Lookup (55427), including 106 saved errors | 1,418 | 0 | 0 |
| Schedules (55392), including volatile dates and external references | 9,979 | 6,663 | 27 |
| Shared fills (31745), using `--TEXTAFTER(…)` | 0 | 42,042 | 0 |
| Ranking (59932), with external workbook links | 142 | 15,808 | 20,394 |

Sheets reported the differences when opening the three affected workbooks.
These original timings used a release probe with a development converter.
The outputs and trial logs are under `/private/tmp/atrium-sheets-excel`.

**TEXTAFTER and date-text rerun (2026-11-24).** With the §8.2 changes,
all 42,042 live formulas in the shared-fill case now match Excel's original
cached results after each of three XLSX saves and reopens. The source uses
`--TEXTAFTER(A2,", ")` to convert labels such as `Sunday, 1 January 2023`
to date serials. The other three workbooks retained the same cache results.
All twelve trials kept the Sheets-computed results on reopen. Installed
Sheets also saved all four via Scrinium and reopened them in new document
jails with the same 76,052 live formula results. Both date
systems passed a separate XLSX regression that also changes the input after
reopening and checks the dependent formulas. All 124 TEXTAFTER formula
cells from three independent Excel-recalculated reference files also
matched Excel's caches, including fractional reverse instances, an empty
fallback cell and implicit intersection of a legacy delimiter range.
[Excel-recalculated TEXTAFTER fixtures](https://github.com/GRID-is/spreadsheet-benchmark/tree/7b6278e39e38453a0420d5c0b3786ca860f3064d/suites)

With the release converter and release probe, median timings on the same
four-vCPU Tessera configuration were:

| Case | Open | Recalculate | Save | Reopen |
|---|---:|---:|---:|---:|
| Lookup | 75 ms | 1 ms | 49 ms | 96 ms |
| Schedules | 579 ms | 406 ms | 85 ms | 552 ms |
| Shared formulas | 109 ms | 9 ms | 47 ms | 101 ms |
| Ranking | 1,147 ms | 340 ms | 494 ms | 761 ms |

Evidence for the TEXTAFTER run is under `/private/tmp/atrium-sheets-textafter`.

**Closed-workbook cache check (2026-11-24).** In the ranking workbook, 6,793
direct formula anchors agreed with their embedded source cell. Twelve more
had a consuming formula result and no corresponding cached source cell.
After preserving the consuming results on import, all 36,344 Excel results
survived XLSX Save and reopen. That includes 7,582 live local formulas,
25,363 linked formulas converted to their saved values, and 3,399 unsupported
LARGE formulas already imported as saved values.

In the schedules workbook, 13 linked names now use their embedded cell
values. A name that points to a range without usable cached cells remains
a reference error. Excel's additional table accessibility metadata had
also been interpreted as a second table declaration, discarding the actual
table. Sheets now keeps all three tables and the structured references
resolve. The schedules workbook had 527 differing results, down from
6,663. Remaining examples include the current year versus the saved year
and empty text versus saved zero. Its 27 SUBTOTAL results still import as
static values.

Three trials per workbook on the private Tessera guest covered Open, full
recalculation, XLSX Save and reopen. Lookup and shared formulas matched
Excel in every trial. Installed Sheets saved all four via Scrinium and
reopened the same 67,684 live formula results in fresh document jails.
The focused converter tests exercise a missing
consuming cache, quoted names, direct and shared links, array values,
a missing link, and local recalculation after an edit and a reopen.
Evidence is under `/private/tmp/atrium-sheets-external`.

**Rank, subtotal, and date-header check (2026-11-24).** The ranking corpus
uses LARGE in 3,399 formulas that previously imported as static values.
They now recalculate through XLSX Save and reopen. The schedules workbook
uses SUBTOTAL on its budget tables and uses DATEVALUE and EOMONTH for date
headers. Its monthly SUMPRODUCT formulas depend on `MONTH(header & 0)`.
Resolving the month label lets the calculation produce numeric zero for
empty months and retain nonzero totals and dependent results.

Three Save and reopen trials per input preserved the Sheets-computed values.
Ranking matched all 36,344 Excel results, including 10,981 live formulas and
25,363 static external-link values. The schedules workbook now keeps all
16,669 formulas live. At the guest's current date, its 25 year and date
header results followed the clock. With the isolated guest set to June 15,
2021, the source workbook's saved year, all 16,669 matched the Excel caches
in all three trials. The lookup and shared-formula cases retained their
previous exact matches. Installed Save and picker reopen in fresh document
jails preserved all 71,110 live formula results across the four workbooks.

The two 26-case Excel rank-function references and a 27-case subtotal
fixture matched. The second subtotal reference contains a five-row group in
Sheet3!16:20. All five rows have outline level 1 and the hidden bit. The file
has no collapsed summary marker, and its worksheet filter ends at row 9.
XLSX Save and reopen now retain the group and the five hidden rows.

For Sheet3!A16:A26, the values 1 through 11 total 66. The visible values 6
through 11 total 51. Sheet2!A26 (code 109) yields 51. Sheet2!A27 (code 9)
yields 66, which matches the fixture’s expected value in B27. The source
caches 51 in A27 and FALSE in the three dependent cells Sheet2!C27 and
Sheet1!B8 and B3. Sheets recalculates all three as TRUE. A fresh Excel
recalculation of this file remains unverified. Microsoft documents both
codes’ treatment of hidden and filtered rows.
[Excel SUBTOTAL](https://support.microsoft.com/en-us/excel/functions/subtotal-function)

Three arm64 Tessera-root guest trials per reference retained all 54 live
results in the grouped workbook and all 27 in the control. The grouped
workbook matched 50 live Excel caches and kept 26 FORMULATEXT caches as
static values. Installed Sheets edited Sheet1!A1 to 8, saved through
Scrinium and reopened the grouped XLSX in a fresh document jail. The saved
XML retained the group, the edit, the subtotals and all three dependent
comparisons. The converter’s row-outline tests also exercise an explicitly
collapsed nested group, a summary above filtered rows, manual hides and
native fallback. Evidence is under `/private/tmp/atrium-sheets-outline`.

The focused checks cover rank arrays, invalid ranks, ties, filter and
outline changes, undo, nested subtotals, cross-sheet dependents, edits
after XLSX and ODS reopen, and both date systems. Evidence is under
`/private/tmp/atrium-sheets-aggregates`.

**Live formula-text check (2026-11-24).** Reimporting the grouped subtotal
reference now brings in all 80 formulas. In each of three full recalculation,
XLSX Save and reopen trials on a private arm64 Tessera-root guest, all 26
formula strings in Sheet2!D2:D27 matched the Excel caches. Across the workbook,
76 caches matched and the four subtotal-related differences above remained.

In installed Sheets, replacing Sheet2!A2 with `=11/2` kept its numeric result
at 5.5 and changed D2 to the new formula text. Ordinary Save replaced the
granted XLSX. A fresh picker jail reopened all 80 live results. Independent
inspection of the saved XML confirmed the new formula and its displayed text,
the other 25 strings, and the five grouped rows. Evidence is under
`/private/tmp/atrium-sheets-formulatext/guest/final`.

### 16.71 Sheet tabs in Sheets (2026-11-18)

Sheets showed only a workbook's first sheet. A tab strip now sits under the grid: a press on a tab shows that sheet (at its top left; the filter and conditional-format panels about the last sheet close), **+** adds a sheet at the end and shows it, and ‹ › move the strip a tab at a time. Only the tabs that fit are measured and drawn (`atrium-sheets::tabs::visible`, `reveal`), so 3,000 sheets render as a few do. The Data menu gains Add Sheet and Rename Sheet…, which opens a bar in the strip's place; a taken name (case aside) or one with `[ ] : * ? / \` is refused with the reason, and formulas follow a rename because they name sheets by number.

Three new undoable ops, journaled like the rest: `GridOp::AddSheet(name)` (tag 11), its inverse `GridOp::DropSheet` (tag 12, takes away the last sheet only when it holds nothing and isn't the first), and `GridOp::RenameSheet { sheet, name }` (tag 13); a refused one changes nothing. Undo past a sheet's making moves the view to a sheet that exists. Deleting and moving sheets followed the same day (§16.72). Not yet: tab colours.

Gates: `officina-grid` `ops::tests::sheets_are_added_renamed_and_undone`; `atrium-sheets` `sheets_are_added_switched_renamed_and_tabbed`, `tabs::tests::only_the_tabs_that_fit_are_laid_out`.

Fix (2026-11-24): a press on a tab's name or colour bar now shows that sheet. The name and bars were drawn as siblings on top of the tab, so they took the press and dropped it; only the margin around the name worked. They are now drawn inside the tab, and a press on any of them reaches it. Gate: `a_press_on_a_tab_label_edge_or_colour_bar_shows_that_sheet` (fails on the old drawing).

### 16.72 Deleting and moving sheets (2026-11-18)

- **Moving.** `Workbook::move_sheet(from, to)` renumbers in place: formula groups come out and go back on their sheets' new places with their references renumbered (`remap_sheets`), and tables, charts, pivot tables, conditional formats, names (read back from their text, which names sheets), spills, macro code names and pending recalculation follow. Cells don't move or recalculate, so the cost is the workbook's groups and objects, not its cells. Journaled as `GridOp::MoveSheet { from, to }` (tag 14), its inverse moving it back.
- **Deleting.** `officina-grid::sheetops::delete_sheet_op(wb, s)` is one undoable batch of existing ops: formulas elsewhere that read the sheet get `#REF!` there (as Excel does), names scoped to it go and others reading it break, tables, charts, pivot tables and conditional formats on or reading it go, its filter, cells, styles and sizes are emptied, then it moves to the end and `DropSheet` takes it away. Undo brings each back in turn; the first sheet can't go when it is the only one.
- Sheets' Data menu gains Move Sheet Left, Move Sheet Right and Delete Sheet (the sheet that takes its place shows; deleting the last shows the one before).

Gates: `sheetops::tests::a_sheet_moves_with_everything_that_reads_it` and `deleting_a_sheet_breaks_what_read_it_and_undo_brings_all_back` (three sheets reading each other with names, a table, a chart and a spill: moved and back, and deleted and undone, each save byte for byte the same native file as before); `atrium-sheets` `sheets_are_added_switched_renamed_and_tabbed`.

### 16.73 Sheets and Slides as installable apps with a jailed converter (2026-11-18)

Write already ran its converter as a worker (dev-tools-plan.md M5). Sheets and Slides now have their manifests too (`atrium-sheets/atrium.toml`, `atrium-slides/atrium.toml`): Fresco graphics, no network, no folders, the `convert` worker (`bin/officina-convert`, 2 GB, 2 CPU-minutes) in a jail of its own as a uid of its own, so `launch::convert` runs it there once installed (a child process in development and tests, as before). Sheets opens `.xlsx`, `.ods` and CSV for changes and `.xls` to view (it doesn't write `.xls`); Slides opens `.pptx` and `.odp`. Both now take files handed over from Scrinium on the document channel (`atrium_open::Channel`, opening-files.md §6.2), as Write does. Gate: `portcullis-toml` `tests/app_manifests.rs` `sheets_and_slides_convert_in_a_worker_and_read_no_folders`. VM run (Mac VM, 2026-11-18, Scrinium on a folder instead of a stick): each opened a file from Scrinium in its own jail and uid (Sheets 50000, Slides 50006), and the converter ran as a separate jailed worker (50005, 50007).

### 16.74 The app cores parse nothing that came from outside (2026-11-18)

For the platform SDK move (`cargo atrium check`'s parser-in-core). Write's, Sheets' and Slides' own processes now link no picture, HTML, SVG, Markdown or XML parser (`cargo tree -e normal`):

- **Fonts.** Navigator's pinned font set is its own crate, `navigator-fontset` (navigator-render re-exports it), and `pergola_document::Faces` is usable without the document engine (feature `view`, default on). officina-text and the apps took all of navigator-render for them.
- **Converter readers.** `officina-convert`'s readers and quick-xml are behind its default feature `read`. The apps link it without that: writers, password protection (ODF encryption now rewrites our own manifest with a tag scanner) and the launcher. Tests turn `read` on to make sample files.
- **Pictures** are decoded in the `convert` worker. `officina-convert pictures` is a long-lived mode (one process for the session, up to 3 when several threads ask; a request is a size or a decode at a target size, answered in RGBA). `officina_convert::pictures::Remote` is the client, installed at startup as `officina_text::picture`'s decoder. pergola-richtext, officina-pdf and Slides ask that decoder; pergola-richtext and officina-pdf keep an in-process fallback (`local-decode`, default on, for other apps such as Mail), which Write turns off. Slides has it only for tests and `big_deck` (`--features local-decode`). `picture::mime` sniffs the first bytes in-process. Gate: `officina-convert/tests/pictures.rs`.

Left: Slides links `png` only to encode its own previews, and the checker lists fonts (allsorts, rustybuzz, ttf-parser) that only ever read the shipped faces.

### 16.75 Tab colours (2026-11-18)

A sheet has a tab colour (`Sheet::tab_color`, 0xRRGGBB): set from Data ▸ Tab Colour…, a bar in the tab strip's place with eight colours, None and Cancel (`TabCmd::Coloring`, `Color`, `ColorCancel`). It is drawn as a bar along the bottom of the tab, thicker on the sheet on screen. `GridOp::TabColor` (tag 15) sets it and is its own inverse; it moves with its sheet, a coloured sheet isn't bare (so `DropSheet` keeps it), and Delete Sheet takes the colour off first so undo brings it back. Native chunk 15 (TAB_COLORS). `.xlsx`: `<sheetPr><tabColor rgb>`, read also as a theme or indexed colour with its tint. `.ods`: a table style's `tableooo:tab-color`. Gates: officina-grid `tab_colours_are_set_undone_and_saved`, officina-convert `tests/tabs.rs`, atrium-sheets `sheets_are_added_switched_renamed_and_tabbed`.

### 16.76 Native files loaded within the memory budget (2026-11-18)

`native::load_within(data, budget)` begins with the workbook memory estimate
after reading strings. For each column chunk it checks the required row-index
slots and formula index space before allocating, then accounts for the actual
column and chunk storage. It also checks the projected memory for formula
fills before materializing their results and rechecks the workbook after the
formula phase and at the end. When the estimate crosses the limit, it returns
`LoadError::TooBig`. The same loader without a limit is `load`. Sheets uses
the bounded loader off the window thread for native open and recovery and for
native output from the converter. The refusal names the workbook and explains
the memory limit. The budget measures the workbook estimate, while process RSS
also includes the encoded file and other allocations.

**Boundary gate (2026-11-24).** On an arm64 Tessera-root guest with four vCPUs
and a 2,132,725,760-byte app budget, `atrium-sheets/examples/native_memory.rs`
ran each case in a fresh process three times. The dense case used the 1 GiB
opening floor from §16.78.

| Native fixture | Stored bytes | Estimated workbook bytes | Loader time, three runs | Outcome |
| --- | ---: | ---: | --- | --- |
| Dense, 131,072,000 numeric cells | 1,065,968,177 | 1,066,755,256 | 2,416 / 2,495 / 2,465 ms | Opened and sampled all 125 columns |
| Sparse, 507 columns through row 2,147,483,647 | 23,412 | 2,126,585,512 | 576 / 572 / 578 ms | Opened and checked every populated far-row cell |
| Oversized sparse, 515 columns | 23,780 | 2,160,141,096 | 501 / 506 / 500 ms | Refused before allocating additional slots at a projected 2,134,974,288 bytes |

Dense reads took 1,051 / 973 / 1,056 ms separately from the loader. These
are timings for the guest. The installed Sheets app refused the oversized
workbook through Scrinium. From that same window, the picker opened the
near-limit sparse workbook in a fresh jail. Ordinary Save and picker reopen
preserved the A1 edit and all 507 far-row cells. The oversized file's hash
stayed the same. The dense workbook passed the engine boundary check.
The original desktop attempt failed because grant verification outlasted the
Open press. Portcullis now spends the press before Scrinium verifies the
grant and bounds the rest of the handoff on that connection
(opening-files.md §6.2). On the same Tessera desktop fixture, Sheets opened
the dense file, edited A1 to 7, saved via the original Scrinium grant and
reopened it through the picker in a fresh jail. The saved file retained A1
and the reference values sampled in all 125 columns. An earlier Save with
both a full encoded buffer and a memory file exhausted the 4 GiB guest.
The plain native snapshot now streams into the sealed memory file (§4.2).

The later picker read error reproduced on the stock Nov 18 arm64
Tessera-root guest. Both a direct fsync/rename replacement and Scrinium's
`workbook::grant` and `commit` passed three immediate and delayed digest
checks per path under ordinary metadata activity. Raising the data-zone GC
pressure threshold from 12% to 50% advanced the scan count from 1 to 9.
All six subsequent reads of each file failed after 1,056,964,608 bytes
(1,008 MiB). GC had queued only the first 63 child manifests. Each child
covered a 16 MiB append window in this file, so GC could reclaim the tail
while the workbook was still live.

On a fresh clone with the corrected Tessera module (tessera-fs.md §11.1),
three alternating replacements per path passed both immediate and delayed
full-file BLAKE3 comparisons. After the pressure change, `gc_scans` and
`gc_manifest_stack_grows` each advanced from 11 to 12 and `gc_reclaimed`
from 4,993 to 4,999. All six SHA-256 reads per path matched the edited
1,065,968,177-byte revision. Both workbooks reopened with A1 equal to 8 and
the sampled values intact in all 125 columns. Both SHA-256 digests and
workbook checks passed again after a clean reboot. The reproducer is
`scripts/tessera-workbook-gc-check.sh`. Baseline logs are under
`/private/tmp/atrium-sheets-delayed-read/guest/` and the final run is under
`/private/tmp/atrium-sheets-delayed-read/final/`.

The dense desktop trial confirmed that the 128 MiB workbook recovery volume
cannot hold the 1,065,968,177-byte native base. Sheets shows the autosave
quota error and still opens the file. A failed base write removes the
incomplete temporary file. If this was the first base, Sheets removes the
failed autosave directory so closing the workbook frees its slot. A fresh
dense trial reused that slot in a new jail and confirmed a second saved A1
edit. Portcullis now gives new workbook slots a bounded 3 GiB recovery volume
(Portcullis §6.5). On the private arm64 Tessera-root desktop an empty legacy
slot upgraded from 128 MiB. Its first native checkpoint held all
1,065,968,177 bytes. After an unsaved A1 edit from 1 to 8 reached the
journal, the document process was killed. Recover opened a new jail.
Save As published the replayed 8 through Scrinium, ordinary Save published
a subsequent 9, and picker Open in a third jail produced a new checkpoint
containing 9. Each revision retained the sampled values in all 125 columns.
The desktop check is `scripts/sheets-dense-recovery-vm-check.sh`.

The FreeBSD gate passed all six Portcullis Open tests, including the bounded
grant transfer, and all six runnable Sheets app tests. The Sheets library
suite passed 107 of 109 tests. The two timing failures also occurred in
alternating runs of the unchanged baseline on the same VM.

`officina-grid/tests/budget.rs` covers exact-budget sparse acceptance,
early refusal and retry, and a compact million-row formula fill. Guest logs,
screenshots, and small workbook snapshots are under
`/private/tmp/atrium-sheets-memory/guest/`. The dense desktop retry and
Save evidence are under `/private/tmp/atrium-sheets-large-open/guest/`.

### 16.77 -IF criteria such as ">"&A1 from a sorted index (2026-11-18)

- **Ordered criteria** (`<`, `<=`, `>`, `>=` a number) on one range in SUMIF, COUNTIF(S), SUMIFS and AVERAGEIF(S) are answered from that range's numbers sorted once per recalculation of a group, grouped by the keys of any equality criteria on other ranges (`COUNTIFS(D:D,">"&D1,D:D,"<"&D2,A:A,A1)`). It uses the same cache as the equality index (see "-IF range index" above: seen twice, at most 8 indexes, 4M rows).
- **Counts** come from a binary search. Sums come from prefix sums only when every number summed is whole and their magnitudes add to under 2^53, so the bits match the walk whatever the order. Otherwise sums are walked as before.
- **Not covered:** ordered text criteria (`">b"`) and wildcards still walk.
- **Timing:** `=COUNTIF(D:D,">"&D1)` down 20,000 rows went from seconds to 25 ms.
- **Gate:** `officina-grid/tests/ifcache.rs` checks every 97th row bit-for-bit against a walk.

### 16.78 Workbooks can't be made bigger than every computer opens (2026-11-18)

- **Limit.** Editing in Sheets refuses a change that grows a workbook past `MAKE_LIMIT` (1 GiB of `memory_bytes`). That is the floor of the opening budget, which is half the machine's memory, from 1 to 16 GiB. So a workbook made on a large machine still opens on a small one.
- **Larger workbooks from elsewhere.** They open where memory allows. Changes that don't grow them, such as clearing or deleting, are still made.
- **Gate:** `doc::tests::a_change_past_the_memory_budget_is_not_made`.

### 16.79 Printing from Sheets and Slides (2026-11-18)

- **Shared writer.** `officina-pdf` writes every export through one page writer (`out.rs`): faces subset once, pictures once, colour glyphs as pictures, opacity as graphics states. Write's export moved onto it unchanged.
- **Sheets.** `officina_pdf::sheet::export` pages the used range down, then across (Excel's default), 0.75 pt to the screen's pixel, with optional fit to width. Cells come from the grid's display values and appearance. Text uses the grid's 12 px face and preserves wrapping, line breaks, alignment, rotation, stacked graphemes and shrink-to-fit. Numbers that still exceed the cell show hashes and other single-line text can be elided. Printing also carries solid, patterned and gradient fills, edges and diagonals, font emphasis, conditional data bars and icons. Cell text and page artwork are clipped to their printable bounds. Hidden axes contribute no size.
- **Slides.** `officina_pdf::slides::export` draws each slide from `officina_slides::draw::draw_slide` (hidden slides excluded by default), at the slide's size or fitted on the paper. Gradients are 128 bands of solid colour (Folium skips shadings, and Prelum rasterises with Folium); pictures go through the installed decoder (the apps build `officina-pdf` without `local-decode`).
- **Apps.** Print… on the toolbar and Ctrl+P in both, through `pergola::print` (printing.md §5.1); the print sheet's Relayout gets a new layout for the paper and orientation chosen.
- **Gates:** `officina-pdf/tests/print.rs` (pages, look and text read back and drawn by Folium); `atrium_sheets::print` tests; `scripts/prelum-r4-vm.sh` stages `sheets` and `slides`.

Sheets prints a saved XLSX or ODS print area when it specifies one cell or
rectangle. Explicit rows and columns repeat on their respective page axes,
including when the title lies outside the body. An explicit band takes
precedence over the frozen pane on its axis. An axis without explicit titles
can repeat its frozen band inside the print area. Title bands use the page's
existing space limit, and overlapping title cells appear once per page. Fit
to width accounts for repeat columns outside the body.

The workbook stores these settings as sheet-local names so edits to the sheet
structure and native saves carry them with other references. XLSX serializes
both repeat bands as one `_xlnm.Print_Titles` union. ODS writes the print area
as `table:print-ranges` and splits row and column runs at the header wrapper
boundaries. Disjoint ODS print areas receive a fidelity notice.
[ODF 1.3 table schema](https://docs.oasis-open.org/office/OpenDocument/v1.3/os/part3-schema/OpenDocument-v1.3-os-part3-schema.html)

Anchored charts print as PDF paths and text. XLSX and ODS can store and print
embedded package PNG and JPEG pictures. ODS writes sheet-anchored frames with
the picture bytes in package parts. ODS external and inline image data and
frame transforms remain outside this path. XLSX accepts one-cell, two-cell
and absolute anchors. XLSX source crops and center rotations survive native
and XLSX saves and are applied when printing. The PDF omits source pixels
entirely outside a crop. XLSX picture links survive native and XLSX saves.
Printing uses the workbook cache or a PNG or JPEG the user selects in the
system picker. Sheets leaves the external target inert.

Slides prints full slides, speaker notes with slide thumbnails and continuation pages, or handouts with two, three, four, six, or nine slides per page. Include Hidden Slides in the Print menu opts all print layouts into hidden slides, including their speaker notes in the notes layout.


The [desktop print gate](../../scripts/sheets-interop-vm-check.sh) with
`SHEETS_INTEROP_CASE=print` edits, saves and reopens XLSX and ODS workbooks and
exports through Prelum and Scrinium. The
[PDF checker](../../scripts/sheets-interop-pdf-check.py) checks both repeat
bands, their intersection exactly once per page, the recalculated formula and
the picture inside the print area. The `comments_print` desktop case also checks ODS thread persistence and BIFF8 rich notes, links and print settings in a native copy, then exports both PDFs. Its PDF check verifies the same four-page layout, without expecting an image in the BIFF8 fixture. The XLSX and ODS print workbooks also retain a second picture
outside the print area, which the PDFs omit. The converter tests cover JPEG
bytes and geometry, blank title bands, both XLSX axes and quoted sheet names.

The `print_fidelity` desktop profile opens, edits, saves and reopens matched
XLSX and ODS workbooks, then saves each with and without comments through
Prelum and Scrinium. The PDF gate checks text geometry and the link underline
in the rendered output, the in-area picture, the four worksheet pages and
both title bands. The comment mode adds an appendix and omits hidden and
out-of-area comments. A separate Folium rendering test exercises a long note
that runs across multiple appendix pages.

### 16.79 Merged cells (2026-11-18)

- **Engine** (`officina-grid::merges`). A sheet keeps merged areas: disjoint, at least two cells each, sorted. `Workbook::merges`, `merge_at`, `merges_in` and `grow_to_merges` read them. `GridOp::Merges { sheet, list }` (journal tag 16) swaps them in whole as one undo step, and the native file keeps them in chunk `MERGES` (16). Cells under a merge keep what they hold, as Excel's and LibreOffice's files do. Inserted and deleted rows and columns move, grow or shrink a merge. One left as a single cell is no longer merged. A merge counts toward a sheet not being "bare", and Delete Sheet clears its merges first.
- **Files.** `.xlsx` `<mergeCells>` and `.ods` `table:number-columns-spanned` / `number-rows-spanned` are read and written. Before this they were reported as "merged cells shown unmerged". Overlapping or one-cell areas in a file are dropped and reported. LibreOffice opens ours in both formats, and we read back what it saves (`officina-convert/tests/merges.rs`).
- **Sheets.**
  - **Drawing:** the grid draws a merged area as one cell, with the top-left cell's fill, borders, icon and text across the whole area and no grid lines inside. This is a layer above the tiles, so an area crossing tiles draws whole.
  - **Moving around:** a press inside one puts the cursor on its top-left, and the arrows step over it as one cell. A selection that meets part of one shows all of it.
  - **Commands:** Data ▸ Merge Cells merges the selection (grown to whole merges). As Excel does, it keeps the top-left value, clears the others (the status says how many) and centres it. Data ▸ Unmerge Cells undoes merges the selection meets. Each is one undo step.
  - **Hooks:** `GridSource` has `merge_at` and `merges_in`, defaulting to none.
- **Sorting** a range with merged cells works when every row is merged across the same columns, one row high and inside the range (`sort::merges_allow_sort`); the merges stay put as the rows move. Anything else is refused with "To sort, every row needs the same merged cells; unmerge them first." Excel also sorts blocks of rows merged to the same height; we don't yet.
- **Printing** merged areas as one cell: done by the printing thread (c7af758b).
- **Center Across Selection** without merging: §16.86. (The editor already spans the merged area while typing.)
- **Gates:** officina-grid `merges::tests`, `merges::op_tests`; officina-convert `tests/merges.rs`; pergola `a_merged_area_is_one_cell_to_the_pointer_and_the_keys`; atrium-sheets `merge_keeps_the_top_left_value_and_unmerge_and_undo_bring_cells_back`.

### 16.80 Cell comments (2026-11-18)

- **Engine** (`officina-grid::comments`). A cell may have a comment with an author, text with lines, and optional bold, italic, underline and strike ranges. Plain comments retain their existing encoding. Styled comments use entry marker 2 in the same native chunk and journal operation, with ordered, bounded UTF-8 byte ranges after the text. `Workbook::comments`, `comment` and `set_comments` read and set them. `GridOp::Comments { sheet, set }` (journal tag 17) sets or removes them as one undo step, and the native file keeps them in chunk `COMMENTS` (17). Clearing or editing a cell keeps its comment, as Excel does. Inserted and deleted rows and columns move a comment, and one on a deleted cell goes. The limits are 32 KB of text, 255 bytes of author and 1M comments a sheet.
- **Files.**
  - **.xlsx:** we read and write `xl/commentsN.xml`. Writing also produces the VML drawing Excel needs to show the notes (a hidden yellow box per comment, linked by `legacyDrawing`). Matching bold "Author:" prefixes are taken off on read. Bold, italic, underline and strike runs are read and written.
  - **.ods:** we read and write `office:annotation` (`dc:creator` and one `text:p` a line). Bold, italic, underline and strike use text spans. Empty commented cells are written so they can carry the comment.
  - **LibreOffice:** it reads both our formats. It drops every author when it reads any `.xlsx`, its own included, so that round trip keeps cells and text only (`officina-convert/tests/comments.rs`).
- **Sheets.**
  - **Seeing them:** a commented cell has a small red triangle in its top-right corner (`CellView::note`). With the cursor on it and no edit open, the comment shows in a pale yellow box beside the cell or its merged area, author in bold, with the body’s inline styles wrapped to ten lines. This is a grid float from `SheetSource::note_at`.
  - **Writing them:** Data ▸ New Comment… / Edit Comment… opens a bar where the tabs are. Line breaks show as ¶ in the one-line editor. Shift+Enter inserts a break and Enter saves. The bar’s four formatting controls and Ctrl+B/I/U or Ctrl+Shift+U apply bold, italic, underline or strikethrough to the selection or set the style for typing. Text edits carry the unaffected style ranges with them. Undo and redo in the bar restore text and formatting together. Save writes the comment as `$USER` (an edit keeps the original author), and an empty one deletes it. Data ▸ Delete Comment(s) removes those in the selection. Each save or deletion is one workbook undo step.
- **Sorting** moves comments with their rows (`sort::tests::comments_move_with_their_rows`).
- **Threaded comments** (Excel 365). The native file and journal keep the root and replies in parent-first order (comment entry marker 3). Messages retain their GUIDs, parent links, author identities and provider IDs, timestamps, resolved state and text. The Sheets balloon shows the conversation with author labels for replies. Data ▸ New Comment Thread… starts one on an unannotated cell. Data ▸ Open Comment Thread… opens the thread bar, where Previous and Next select a message for editing and Reply appends a message to the root. New messages are attributed to the local user and timestamped in UTC. Editing keeps the original author, timestamp and message ID. Posting, editing, replying and deleting a thread each take one workbook undo step. ODS writes the visible conversation as a standard annotation for other apps. Sheets also writes a versioned extension holding the native thread, including message order, authors, timestamps, resolved state and mentions. On reopen, the visible author, text and formatting must match the thread's note before Sheets restores it. An edited note takes precedence, and invalid extension data receives a fidelity notice. Threads larger than 4 MiB in the native encoding save as standard notes with a notice. Apps that rewrite the file can discard the extension. [OpenDocument extended conformance](https://docs.oasis-open.org/office/OpenDocument/v1.3/os/part3-schema/OpenDocument-v1.3-os-part3-schema.html)
- **XLSX thread parts.** Read the author directory from the workbook relationship and the conversation from the worksheet relationship, including replies without a cell reference. A linked legacy placeholder supplies the authoritative cell. Save writes the shared `persons` part and each worksheet’s `threadedComments` part, plus the `tc={id}` author and matching `xr:uid` for each legacy placeholder. Copies get distinct message IDs with their parent links remapped. [MS-XLSX §2.3.7.3](https://learn.microsoft.com/en-us/openspecs/office_standards/ms-xlsx/6383f002-c90b-401c-a1d7-66b97b14cb3e)
- **Mentions.** The native file and journal use comment entry marker 4 for threads containing mentions. A mention stores its ID, the referenced person and a bounded UTF-8 byte range. XLSX maps these ranges to UTF-16 indices and includes people referenced only by mentions in the workbook person directory. Copied threads keep the referenced people and receive distinct mention IDs on export. Edits before a mention move it, edits that touch its text remove the metadata, and editor undo restores the original range. [MS-XLSX CT_Mention](https://learn.microsoft.com/en-us/openspecs/office_standards/ms-xlsx/b03ed619-e307-4d3e-9c67-b68612274128)
- **People picker.** In a thread comment or reply, click **@ Mention** to search workbook participants by name or ID. A draft `@prefix` next to the caret starts the search and is replaced on selection. Enter an email address to mention someone new to the workbook. The picker uses the workbook’s authors and previously mentioned people. It stores the selected person and inserts the visible token as one editor undo step. Posting or saving includes the mention in the normal workbook undo step.
- **System clipboard.** Copying a cell range between Sheets workbooks includes its comments, including styled comments on otherwise blank cells. Copying a text selection between rich comment bars carries bold, italic, underline and strikethrough. Text and styles paste together as one editor undo step. Saving the edited comment is one workbook undo step.
- **Printing.** Data ▸ Print with Comments… marks the visible comment cells and appends pages keyed by cell address. Legacy notes keep their text emphasis. Threads print the authors and replies in order, with resolved roots labelled. A note on a repeated title has one appendix entry. Hidden and protected values and the print area govern which cells contribute. Ordinary Print keeps the comment contents private.
- **Gates:** officina-grid `comments::tests`; officina-convert `tests/comments.rs` (native and XLSX thread round trip with authors, replies and mentions, Excel's author prefix, LibreOffice both ways); atrium-sheets `comments_are_written_shown_beside_the_cell_edited_and_deleted`, `rich_comment_edits_render_and_undo_through_the_comment_bar`, `threads_can_be_posted_replied_to_edited_and_undone`, `mention_picker_inserts_in_threads_and_replies_and_round_trips`, and `imported_threads_can_be_deleted_and_undone_without_flattening`.

### 16.81 Cell hyperlinks (2026-11-18)

- **Engine** (`officina-grid::links`). A cell may carry a link: an address (`https:`, `mailto:`, ...) or, starting `#`, a place in the workbook (`#Sheet2!B4`, `#B4`). `GridOp::Links` (journal tag 18) sets them as one undo step; the native file keeps them in chunk `LINKS` (18). Links move with inserted and deleted rows and columns and with sorted rows, as comments do. Limits: 8 KB a target, 1M links a sheet. `HYPERLINK(address, [label])` works too: `Workbook::link_at` reads either.
- **Files.**
  - **.xlsx:** `<hyperlinks>`; an address is an external relationship (`TargetMode="External"`), a place is the `location` attribute.
  - **.ods:** `text:a xlink:href` in the cell's paragraph; places are written `#Sheet.A1` and read back.
  - **LibreOffice** reads both and writes both back to us unchanged (`officina-convert/tests/links.rs`).
- **Sheets.**
  - A link cell is drawn in blue, unless it has a colour of its own.
  - Data ▸ Open Link follows the cursor's link. A place shows its sheet and selects the cell. An address goes to Monstrator with the activation of that press (`open-uri` in the manifest), from a thread, and the status line says what happened. Sheets never opens an address without a press.
  - Data ▸ Insert Link… / Edit Link… opens a bar where the tabs are. It takes `Sheet2!B4` or `B4` as a place, a bare host as `https://`, and an address as given; anything else is refused with a reason and the bar stays. An empty cell gets the target as its text, in the same undo step. An empty field removes the link, as does Data ▸ Remove Link(s) for the selection.
  - Ctrl+click (or Cmd+click) on a cell selects it and follows its link, with that click's activation. Pointer events carry no modifiers, so `pergola::input::pointer_modifiers()` keeps those of the last button event, and the grid sends `GridAction::Follow`.
  - With the cursor on a link cell and no edit open, the target shows beside the cell ("Go to Sheet2!B4" for a place) with "Ctrl+click to open". It shares the comment box when the cell has both.
- **Printing.** Link text uses the grid colour (or its explicit colour) and is underlined. Prelum's sanitized PDF keeps the styled label as static page content. ODS saves links even when the cell has no value or shares the cell with a comment.
- **Gates:** officina-grid `links::tests`; officina-convert `tests/links.rs` (round trips, LibreOffice both ways); atrium-sheets `links_are_inserted_followed_and_removed`; pergola `ctrl_click_selects_and_asks_to_follow`.

### 16.82 Frozen panes (2026-11-18)

- **Engine.** `Sheet::freeze` holds the rows and columns frozen at the top and left ((0, 0): none). `GridOp::Freeze` (journal tag 19) sets it as one undo step, and the native file keeps it in chunk `FREEZE` (19). A frozen sheet isn't bare (DropSheet leaves it), and deleting a sheet undoes its panes with it.
- **Files.**
  - **.xlsx:** `<sheetView><pane xSplit ySplit topLeftCell activePane state="frozen"/>`. Split (not frozen) panes are noted as dropped.
  - **.ods:** LibreOffice's `settings.xml` view settings, per sheet (`HorizontalSplitMode`/`VerticalSplitMode` 2 and their positions). LibreOffice reads them only when the root declares its `xlink` and `ooo` namespaces too, which we do.
  - **LibreOffice:** it keeps our panes both ways (`officina-convert/tests/freeze.rs`). Headless conversion has no view, so LibreOffice writes no view settings at all and the panes are lost. The test therefore opens each file with a view: `tests/lo_view.py` runs under Xvfb through Python's `uno`, and the test skips without them.
- **Grid** (`pergola` `GridSource::frozen`).
  - Bands drawn over the body keep the frozen cells in view. The frozen rows' band moves only across, the frozen columns' band only down, and their corner not at all. Each band draws its cells, merged areas, the selection and the editor. The frozen labels stay in the headers, and a darker line marks the freeze.
  - A press in a band means the unscrolled cell. Keys, drags and the wheel work there as on the body.
  - Moving the cursor scrolls so that it clears the frozen rows and columns, and moving into them doesn't scroll.
  - The frozen area is at most three quarters of the view.
- **Sheets.** Data ▸ Freeze Panes keeps the rows above the cursor and the columns left of it in view. Freeze Top Row keeps the first row. With panes frozen, the item reads Unfreeze Panes. Each is one undo step.
- **Floats in frozen panes (2026-11-19).** Charts, filter buttons, comment boxes and validation marks meeting the frozen rows or columns are drawn in their band too, so they stay with those cells. The part of a chart past the freeze line scrolls in the body, as in Excel. Presses on them in a band reach them. Gate: pergola `a_float_in_the_frozen_rows_stays_with_them`.
- **From the top of the view (2026-11-19).** As in Excel, Freeze Panes freezes from the row and column at the top left of the view, and Freeze Top Row keeps the row at the top of the view. `Sheet::freeze_from` holds the first frozen row and column (0 on an axis with none frozen, else less than `freeze`); rows and columns above and left of it are out of reach until unfrozen. `GridOp::Freeze` carries it (8 more bytes when not (0, 0), so older records still read), and the native file keeps it in chunk `FREEZE_FROM` (22).
  - **Grid:** `GridSource::frozen_from`. The bands start there, and `scroll::Physics` gained a `min` (`set_min`), so the body can't scroll above it.
  - **.xlsx:** `<sheetView topLeftCell>` is the first frozen cell, and `xSplit`/`ySplit` count the frozen columns and rows from it. **.ods:** `PositionTop`/`PositionLeft`. LibreOffice keeps it both ways (`tests/freeze.rs`, sheet "Scrolled").
  - The status bar names the frozen rows and columns, e.g. "Rows 11 to 12 and columns B to C stay in view".
- **Printing:** the frozen rows and columns repeat on every printed page that starts below or right of them, as print titles (not when they would take more than half the page; printing.md §11.4).
- **Gates:** officina-grid `frozen_panes_are_set_undone_and_saved`; officina-convert `tests/freeze.rs`; pergola `frozen_rows_and_columns_stay_in_view`, `rows_frozen_from_the_top_of_the_view_hide_those_above`; atrium-sheets `panes_freeze_at_the_cursor_and_unfreeze`.

### 16.83 Data validation (2026-11-18)

- **Engine** (`officina-grid/src/validation.rs`).
  - A rule covers ranges of one sheet and allows any value, a whole or decimal number, a value from a list, a date, a time, a text length, or a formula that must be TRUE. Numbers, dates, times and lengths compare against one formula or two (between).
  - A rule also has an input prompt and an error message. Its error style says whether a disallowed value is stopped, warned about or only told.
  - A list can use a range, a comma-delimited string or an array of literal choices. Arrays keep punctuation and whitespace inside each individual choice.
  - Formulas are written for the first range's top-left and move with rows and columns as conditional formats' do.
  - `check_cell` runs after an entry goes in, because a custom formula may read the cell itself. A refused entry is undone.
  - `GridOp::Validations` (journal tag 20) sets the rules as one undo step, and the native file keeps them in chunk `VALIDATIONS` (20). Deleting a sheet takes its rules with it, and undo brings them back.
  - Limits: 65,536 rules, 1,024 ranges a rule, 1 KiB a message, 4,096 list items.
- **Files.**
  - **.xlsx and .xlsm.** Read ordinary `<dataValidations>` and formula-backed rules in the [Excel worksheet validation extension](https://learn.microsoft.com/en-us/openspecs/office_standards/ms-xlsx/07d607af-5618-4ca2-b683-6a78dc0d9627). Both use the same rule limits and preserve ranges, formulas, prompts and alerts. A rule with a direct reference to another sheet is saved in the extension. Other supported rules can be saved in the standard container. Missing required formulas and invalid ranges are reported. Excel's `showDropDown="1"` hides the list button.
  - Excel's [x12ac:list](https://learn.microsoft.com/en-us/openspecs/office_standards/ms-xlsx/de43f3b8-5c1b-4e5c-96a7-7f18e7bcdb6c) can provide individually quoted choices. A supported compatibility choice takes precedence over the fallback formula. Save writes the exact choices and includes a fallback only when a standard comma-separated formula expresses the same options.
  - **.xls.** [BIFF8 Dv records](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/862bbc7c-009a-4fd6-a93e-3e32e591c2f8) provide worksheet validation on import and keep it active through native Save As. Formula references are relative to the first range. Sheets retains NUL-delimited literal list items as exact choices.
  - **.ods:** `table:content-validations` before the tables, and each covered cell names its rule (`table:content-validation-name`). Conditions use OpenDocument's text, such as `cell-content-is-whole-number() and cell-content-is-between(1,100)`, `cell-content-is-in-list("Yes";"No")` or `is-true-formula(…)`. Their formulas are relative to the base cell.
  - Writing names at most 10,000 rows past a sheet's last used row (a whole-column rule would otherwise name a million cells), and 500,000 cells per file.
  - **LibreOffice:** it reads our .xlsx and writes .ods, and reads our .ods and writes .xlsx, with every rule coming back the same (`officina-convert/tests/validation.rs`). LibreOffice writes a second formula of `0` for one-formula rules, and we drop it.
- **Sheets.**
  - Typing, paste, and fill check the affected cells after the edit has been applied and recalculated. Custom formulas see every change in the batch. An edit can cover up to 100,000 cells with enabled validation.
  - A Stop rule rejects the entire edit before it reaches undo history or autosave. The status line gives the rule's message and identifies the offending cell for a batch.
  - A Warning rule keeps the edit and offers Keep or Undo in a bar under the grid. Undo takes back the entire edit.
  - An Information rule keeps the edit and gives the rule's message in the status line.
  - The cursor cell's input prompt shows beside it, as a comment does. A list cell adds the hint "Data ▸ Choose from List…", which opens the list at the right of the grid, with a search. Choosing an item uses the same validation as typing.
- **Gates.** The grid and converter validation tests cover rule evaluation and format persistence. The Sheets tests cover tentative edits and undo. The installed desktop gate `scripts/sheets-interop-vm-check.sh` with `SHEETS_INTEROP_CASE=validation` covers typing, system clipboard paste through Tabula, fill, undo and redo, and Save and reopen in XLSX and ODS. The `validation_legacy` case exercises the same edits with Excel extension lists and BIFF8 rules, verifies that both reject invalid entries after picker reopen, and checks that native Save As leaves the XLS original intact.

### 16.84 Data validation panel (2026-11-19)

- **Sheets** (`atrium-sheets/src/validation.rs`, drawn by `app.rs`).
  - Data ▸ Data Validation… opens a panel at the right of the grid. It shows the rule over the cursor's cell ("Change Validation"), or a new rule for the selection ("New Validation", a list by default).
  - The panel has:
    - Apply To.
    - Allow (any value, whole number, decimal, list, date, time, text length, custom formula).
    - The comparison and one or two values, for kinds that compare.
    - Allow empty cells, and Offer the list.
    - The input prompt (shown or not, title, text).
    - Check values typed in, with the error style and its title and message.
  - **Values:** a number, `=formula`, a date as 2026-01-31 or a time as 09:30. A list is items split by commas, or `=range`. A value the rule can't use is refused with why, and nothing is saved.
  - **Saving:** a cell has one rule, as in Excel. A saved rule's cells leave every other rule, and a rule left with no cells goes. A new rule goes first; a changed one keeps its place.
  - **Deleting:** Delete Rule removes the rule being changed. Data ▸ Clear Validation takes rules off the selection's cells only.
  - Each change is one undo step.
- **The list in the cell.** The cursor's cell shows a ▾ button at its right when its rule offers a list; a press on it, or Alt+↓, opens the list. A filter column's ▾ button opens its value list the same way. pergola: `GridFloat::pressable` floats report `GridAction::PressFloat` instead of choosing cells, and Alt+↓ reports `GridAction::Choose`.
- **Circle Invalid Data** (a Data menu toggle) rings in red each cell on screen whose value its rule doesn't allow, and the status line counts them on the sheet's used cells. Rings follow edits and go when the toggle is turned off; it isn't saved. Engine: `Workbook::invalid_cells(area, limit, budget)`, which checks each covered cell against its first rule, with empty cells counting only when blanks aren't allowed.
- **Selection.** While a rule panel (validation or conditional formats) is open, the grid's left part stays usable. Its Selection button puts the grid's selection into Apply To.
- **Not Excel's way:** Excel keeps one rule per range and offers to "apply these changes to all other cells with the same settings"; here a rule holds all its ranges, so a change always reaches all of them.
- **Gates:** atrium-sheets `rules_are_made_changed_and_cleared`, `the_validation_panel_makes_changes_and_deletes_rules` and `a_list_cell_s_button_and_alt_down_open_its_list`, `invalid_data_is_circled_until_turned_off`; officina-grid `invalid_cells_are_found_in_an_area`; pergola `a_button_in_a_cell_and_alt_down_reach_the_app`.

The `annotations` profile of `scripts/sheets-interop-vm-check.sh` edits
a rich note, an internal hyperlink, a validation rule, and a comment thread
in the installed desktop. XLSX keeps the thread and its mention metadata.
ODS keeps the conversation as a plain note. The edited ordinary rule rejects
and accepts the expected values after reopening either format. The XLSX fixture
also verifies that a cross-sheet extension list survives Save and reopen and
still rejects values outside the list. The converter tests cover multiple
target ranges, two formula bounds, specific loss notices for missing bounds,
and the XLSM round trip with inert VBA.

### 16.85 Long text runs over empty cells (2026-11-19)

- **Grid** (pergola `grid_view.rs`, `spill`/`emit_spills`). Text wider than its cell runs over empty neighbours, as spreadsheets show it:
  - Left-aligned text runs right, right-aligned text runs left, and centred text runs both ways.
  - It runs up to 32 visible neighbours per side and stops at a visible cell that shows anything or is merged. What still doesn't fit is cut with an ellipsis.
  - Numbers never run over; they show `###`. Text with line breaks doesn't run over either.
  - The grid lines under the text are hidden, except where a neighbour has a fill.
  - The text is drawn in a layer over the cell tiles, so a run that crosses tiles or frozen bands draws once.
  - Text reaching in from cells beside the visible columns is found only through a blank edge cell, so full rows cost nothing more. `scroll_cost` p50 and p99 are unchanged.
- **Hidden columns:** a hidden column can neither contribute text nor block overflow from a visible cell. A visible cell beyond the hidden run can block the overflow. The Excel behavior is documented in the interoperability report. [LibreOffice bug 160011, comment 6](https://bugs.documentfoundation.org/show_bug.cgi?id=160011#c6)
- **Gates:** pergola `long_text_runs_over_empty_neighbours` and `text_runs_across_hidden_columns_from_visible_cells_only`.

### 16.86 Center Across Selection (2026-11-19)

- **Command.** Data ▸ Center Across Selection (needs two or more columns) gives the selection Excel's "centre across" alignment without merging. It is one undo step. With the selection's first cell already centred across, the item is checked, and choosing it again puts the cells back to General.
- **Grid.** A cell centred across is drawn centred over itself and the cells after it in its row, while those cells are empty, also centred across, and not merged (up to 32 columns). Text wider than the run is cut. pergola: `CellAlign::Across`.
- **Files.** .xlsx keeps it as `centerContinuous` (and .xls reads it). .ods has no such alignment, so it is written as `center`, which is what LibreOffice shows for it.
- **Printing** centres it across the same run (d39d04a8).
- **Gates:** atrium-sheets `center_across_selection_turns_on_and_off_without_merging`; pergola `text_centred_across_the_selection_spans_its_cells`.

### 16.87 The fill handle, Fill Down and Fill Right (2026-11-19)

- **Engine** (`officina-grid::autofill::fill(wb, from, to, mode)`): one undoable batch that fills the cells of `to` outside `from`, down, up, right or left. Each column (filling down or up) or row (across) of the selection is read in the fill's direction:
  - Two or more numbers carry on along their best-fit line (1, 3 → 5, 7; 0.1, 0.2 → 0.3 without drift: values are rounded to 15 digits). One number is copied, as Excel's handle does.
  - Dates (by their format): one date steps a day; dates on the same day of the month a whole number of months apart step by months, keeping the day where the month has it (31 Jan → 28 Feb).
  - Text whose last digits count ("Item 1", "Q3", "Room 008", keeping leading zeros) steps by one, or by the step two or more show.
  - Day and month names, short or long, in the same case.
  - Anything else, and any line holding a formula or mixed kinds, repeats in turn. Formulas move as a copy moves them (relative references shift, `$` ones stay), and a column of them is one formula fill.
  - Styles repeat with the cells.
  - Filling up or left, one cell counts down ("Item 5" → "Item 4", "Item 3").
  - `Mode::Copy` repeats without series. `Mode::Flip` (2026-11-19) is Excel's Ctrl: one number counts up by one (down when filling up or left), and everything else is copied.
  - Refused: areas meeting merged cells, and more than 4 M new cells.
- **Grid** (pergola): a source that says `fills()` gets a small square at the selection's bottom-right corner. Dragging it rings the area it reaches, down, up, right or left, whichever way the pointer is farther out. Letting go sends `GridAction::Fill { from, to, copy: false, flip }`, `flip` when Ctrl is held as it is let go, and selects the filled area. Ctrl+D and Ctrl+R send `copy: true`: the selection's first row (or column) into the rest, or with one row chosen, the row above.
- **Sheets:** `Doc::fill`, undone as one step ("Fill Series" or "Fill").
- **Double-click on the handle** (2026-11-19) fills the selection down as far as the data beside it goes: the column left of it, or else the one right of it, to the end of its unbroken run, stopping above anything already in the selection's first column (`autofill::to_data_end`, reading the column a growing window at a time). The filled area is selected. pergola reports `GridAction::FillToData`; two presses on the handle within 450 ms are a double-click, and a press that hardly moves fills nothing.
- **Fill choices** (2026-11-19, Data menu, while the last change is a fill): Fill: Copy Cells, Fill: Series (one number counts by one, `Mode::Count`), Fill: Formats Only, Fill: Without Formatting do the fill again that way, in its place as one undo step (`Doc::refill`, `Doc.last_fill` checked against `Doc.edits`).
- **Fill options button** (2026-11-19): after a fill, a small button sits under the right of the filled area until the next change (`source::KEY_FILL`, `fill_button_at`). A press opens a menu with Copy Cells, Fill Series, Fill Formats Only and Fill Without Formatting, as Excel's Auto Fill Options does. Gates: atrium-sheets `a_fill_offers_its_options_by_the_filled_area`, `a_double_click_on_the_fill_handle_fills_down_beside_the_data`.
- **Flash Fill** (2026-11-19; Ctrl+E, Data ▸ Flash Fill). Type how a cell or two beside some data should read, and Flash Fill fills the rest of that column the same way, as one undo step (`officina_grid::flashfill`, `GridAction::FlashFill`).
  - It learns a short program from the examples and checks it against every one as it grows. A program is made of words of the cells beside (counted from either end), their first letters, whole cells, each in the same case or upper, lower or proper case, with fixed text between.
  - It reads up to 8 columns on each side and fills up to 100,000 rows. A first row the examples don't explain is taken as a header and left alone.
  - When no program explains the examples it says so rather than guessing. It never writes over a cell that holds something.
  - Gates: officina-grid `programs_are_learnt_from_examples`, `a_column_is_filled_by_example`; atrium-sheets `flash_fill_fills_a_column_by_its_examples`.
- **Flash Fill by position** (2026-11-19): programs also take the first characters of a word (up to 7) and the first or last characters of a whole cell (up to 8): `Lovelace` → `LOV`, `AB12345` → `2345`.
- **Custom lists** (2026-11-19; Data ▸ Fill: Keep Selection as a List, Fill: Forget Custom Lists): a row or column of 2 to 256 different entries becomes a list. A fill from any of its entries goes on through the rest in turn, before the day and month names are tried, and in the typed entry's case. Up to 64 lists, kept for the person beside Sheets' autosaves (`atrium-sheets/lists`, one list a line, entries split by tabs; `atrium_sheets::lists`, `autofill::set_custom_lists`). A new list replaces one sharing an entry with it. Gates: officina-grid `custom_lists_fill_in_turn`; atrium-sheets `a_selection_kept_as_a_list_fills_on`.
- **Custom lists panel** (2026-11-19; Data ▸ Fill: Custom Lists…): lists each custom list with Forget, plus Forget all (`lists::forget`). Gate: atrium-sheets `the_custom_lists_panel_opens_and_closes`.
- **Flash Fill number formatting** (2026-11-19): from displayed decimal numbers, Flash Fill learns fixed decimal places, rounding, comma grouping, leading zeroes and percentage scaling. Fixed text can supply a currency symbol or other label. Results are text, as for other Flash Fill patterns. Dates can still be rearranged as words. Gate: `reformats_numbers_from_examples`.
- **Gates:** officina-grid `autofill::tests` (6, with `ctrl_flips_series_and_copy`, which checks `Mode::Count` too); atrium-sheets `a_fill_can_be_done_again_another_way`; pergola `the_fill_handle_and_ctrl_d_ask_to_fill`; atrium-sheets `the_fill_handle_continues_a_series_and_ctrl_d_copies`, `a_double_click_on_the_fill_handle_fills_down_beside_the_data`.

### 16.88 Copy, cut and paste within Sheets (2026-11-19)

- **Before:** copying put only the shown values on the clipboard, so a formula pasted as its result, and a cut emptied the cells at once.
- **Engine** (`officina-grid::clip::Clip`): `take(wb, rect, cut)` keeps the used part of the area (a whole column costs what it holds): values, formulas and styles. `paste(wb, sheet, row, col)` gives the edit and the area pasted.
  - A copy keeps formulas as expressions, so relative references move with them (`=A1*2` copied four rows down reads `=A5*2`; `$` ones stay). A column of them pastes as one formula fill.
  - A cut empties its source in the same step, so pasting moves the cells, as Excel does. Overlapping moves keep what lands.
  - References follow moved cells (2026-11-19): every formula in the workbook, the moved ones too, that reads a moved cell, or a range wholly inside the moved area, is rewritten to read it where it went, on whichever sheet. A range only partly inside stays. References from moved formulas to cells left behind keep reading them (named by sheet when the formula lands on another one).
  - Cells written over (2026-11-19): formulas elsewhere that read a cell the move writes over (one not itself moving), or a range wholly inside them, read `#REF!` instead, as Excel's do; a range only partly over stays.
  - Refused: an area that would run off the sheet, and pasting over merged cells.
- **System clipboard** (2026-11-19). Copy and Cut offer tab-separated `text/plain` and a versioned `application/vnd.atrium.sheets.cells` range through Tabula. Paste requests the Sheets MIME first. Between workbooks the range carries typed values, static cell styles, and comments with their inline runs or thread metadata. Formula results are captured at copy time. Commented ranges use version 2 of the payload, and version 1 offers remain readable. HTML and plain text alternatives carry cell contents. A matching local offer uses the original clip with formulas, comments and links. A cut waits for paste in the source document, moves once, and requires the source document to have had no intervening edits. An unrelated plain-text offer pastes as text even when its bytes equal an earlier copy.
- **Clipboard limits.** An offer is limited to 2 million cells and Tabula’s per-format byte limit. A copy whose rich payload exceeds the limit can offer text alone. A cut needs the rich payload to match the source operation. Invalid rich data is rejected before editing the sheet. Copy and paste each use one gesture.
- **Paste Special** (2026-11-19; Data menu, offered after a copy, not a cut): Paste Values (formula results as they were when copied, no formats), Paste Formulas (no formats), Paste Formats (the cells keep what they hold), Paste Transposed (rows become columns, with formats). `Clip::paste_as(…, What, transpose)`. A transposed formula (2026-11-19) keeps its formula: references into the copied cells turn with them, `$` ones stay, and other relative ones move as a copy moves them (`=A1+B1*$E$9+F1` in C1, with A1:C1 pasted transposed at A5, reads `=A5+A6*$E$9+D7`).
- **Arithmetic and skipping blanks** (2026-11-19; Data menu, after a copy): Paste and Add, Subtract, Multiply, Divide combine each copied number (a formula's value when copied) with the cell it lands on: a number there, or blank as 0, takes the result (dividing by 0 gives `#DIV/0!`); a formula there gains the operation (`=(C2*2)*10`); text there, and copied text or blanks, leave the cell alone. Paste Skipping Blanks pastes everything but lets blank copied cells keep what is under them (formats still come from the whole copy). `Clip::paste_special(…, Special { what, transpose, arith, skip_blanks })`; only the touched cells are rewritten, one edit a run down a column.
- **Into a bigger selection** (2026-11-19): with more cells selected than the copy holds, a paste repeats it across the selection in whole copies (a 1×2 copy into 4×4 fills it twice across, four times down), with formulas moved for each copy, as one undo step. A selection smaller than the copy, or not a whole number of copies, pastes it once. At most 10,000 copies and 4 M cells (`Clip::paste_into`, `Doc::paste_in`; pergola's `GridAction::Paste` carries the selection).
- **What comes along** (2026-11-19): copying brings cells' comments and links (Paste Special ▸ Paste Comments brings only the comments); a cut moves them. A cut also moves what points at the cells: defined names, conditional formats and validation rules (each area wholly inside the cut moves; onto another sheet only when all of a rule's areas do; their formulas follow as cell formulas do), and charts' data, category and series-name cells (`Clip::follow`).
- **Paste Special bar** (2026-11-19; Data ▸ Paste Special…): what to paste (All, Values, Formulas, Formats, Comments), an operation (none, +, −, ×, ÷), Skip blanks and Transpose, chosen together and pasted as one undo step (`data::paste_with`); a refusal leaves the bar open. Pasting all or formulas with an operation combines a copied formula with what is there as a formula, as Excel does: `=5+(F1/2)`, `=(F3*2)+(G1/2)`, `=0+(H1/2)` onto blank; pasting all also brings the formats. Gates: officina-grid `paste_special_adds_multiplies_and_skips_blanks`, atrium-sheets `the_paste_special_bar_pastes_its_choices_together`.
- **Rules partly cut** (2026-11-19): a cut that takes some of a conditional format's or validation rule's cells splits it, as Excel does. The cells taken keep a copy of the rule where they land, placed just after it, with its formulas moved as those cells' formulas move. The cells left keep the rule, with their areas cut around the hole (`clip::meet`, `minus`). A chart range only partly inside a cut stays where it was, as a formula's range does, and as in Excel. Gate: officina-grid `a_cut_of_part_of_a_rule_splits_it`.
- **HTML clipboard interchange** (2026-11-19). Sheets also offers a `text/html` table when it fits. Paste prefers the native Sheets range, then HTML, then plain text. Ctrl+Shift+V requests plain text directly. Excel documents HTML and text formats and Windows uses CF_HTML framing. Calc offers HTML with native formats, SYLK, DIF, RTF and text. Google Sheets clipboard samples show HTML with Sheets-specific value and format attributes. HTML is the shared rich representation used here. [Microsoft Excel formats](https://support.microsoft.com/en-gb/excel/file-formats-that-are-supported-in-excel) · [CF_HTML](https://learn.microsoft.com/en-us/windows/win32/dataxchg/html-clipboard-format) · [Calc transfer source](https://github.com/LibreOffice/core/blob/master/sc/source/ui/app/transobj.cxx) · [Google Sheets clipboard sample](https://support.google.com/docs/thread/297341739/copying-from-sheets-do-not-include-raw-value-anymore?hl=en)
- **HTML subset.** The converter reads the selected table or an annotated Google Sheets single cell, with 1 MiB of UTF-8 HTML and 16,384 cells at most. It recognizes numeric hints from Excel and Calc, Google typed values, basic fonts, fills, alignment, wrapping, and Excel and Calc number-format hints. Calc’s `sdval` preserves the underlying number when the offered Google-compatible numeric hint loses precision. It recognizes inline markup, literal cell defaults, and simple class rules. Unannotated values are literal strings. Formula markup, scripts, external resources, and other CSS have no effect. Merged cells and unsupported HTML are refused. Export writes a flat table with inline styles and literal values, including formula results and styled blanks. For Calc’s HTML importer it also writes `sdnum` for custom number formats, legacy cell fill and alignment attributes, and inline font tags. The import runs asynchronously and is canceled if the destination sheet changes before it finishes.
- **Gates:** officina-grid `clip::tests` (9, with `a_move_brings_comments_links_names_and_rules`, `paste_special_adds_multiplies_and_skips_blanks`, `#REF!` checks in `references_to_moved_cells_follow_them`, `a_transposed_formula_reads_the_turned_cells`, `pasting_into_a_bigger_area_repeats_the_copy`); atrium-sheets `copy_and_paste_here_bring_formulas_and_a_cut_moves`, `paste_special_is_offered_after_a_copy_and_pastes_values` (with Paste and Add), `a_copy_fills_a_bigger_selection_and_ctrl_flips_the_handle`.
- **System clipboard gates:** atrium-sheets `system_clipboard_round_trips_formats_between_workbooks`, Pergola’s `clipboard_tabula` integration test, and officina-convert’s `html_clipboard` worker round-trip test.

### 16.89 Hiding rows and columns by hand (2026-11-19)

- **Sheets** (Data menu): Hide Rows and Hide Columns hide the selection's; Unhide Rows and Unhide Columns show the hidden ones inside the selection (select the rows or columns around them, as in Excel). Each is one undo step, and the status line counts what changed.
  - Rows join the sheet's hidden ranges (`Sheet.hidden`, which the filter shares: rows hidden by hand outside a filter's range stay hidden when it filters again; inside it, filtering decides).
  - Columns get no width; shown again (2026-11-19) they take back the width they had when hidden in this session (`Doc.hidden_widths`), or else the default width. At most 16,384 at once.
- **Grid** (pergola `past_hidden`): arrow keys step over hidden rows and columns (filtered ones too) to the next shown one, or stay put if none. Since 2026-11-19 Home, Ctrl+Home, Page Up and Page Down do too, landing on the nearest shown row or column. The row and column labels draw a double line where hidden ones sit, as Excel does.
- **Files:** .xlsx columns with no width are written `hidden="1"` and read back hidden (they used to show 2 px wide, with a note); .ods columns get `table:visibility="collapse"` and are read back hidden. Hidden rows were kept already. LibreOffice keeps both ways (`officina-convert/tests/hidden.rs`).
- **Double-click on the double line** (2026-11-19): just right of where columns are hidden, a double-click on the label edge shows the hidden run again, as Unhide Columns would (`data::unhide_cols_at`, from `GridAction::FitCol` on a hidden column); just left of it still fits the shown column.
- **Width kept (2026-11-19):** a hidden column's width before hiding is kept in `Sheet::hidden_widths`, set as one undo step with the hiding by `GridOp::HiddenWidths` (journal tag 21) and saved in native chunk `HIDDEN_WIDTHS` (21). .xlsx writes it as the hidden column's `width` and .ods as its column style's width, and both read it back, so Unhide Columns shows the column as it was after saving and reopening, in LibreOffice and Excel too. Inserting and deleting columns move the list; deleting the sheet takes it, and undo brings it back.
- Hidden rows get the same double-click (§16.93).
- **Gates:** atrium-sheets `rows_and_columns_are_hidden_and_shown_again`; pergola `arrows_step_over_hidden_columns`; officina-convert `tests/hidden.rs`.

### 16.90 Row heights by dragging, and sizes undone in one step (2026-11-19)

- **Grid** (pergola): a press within 4 px of a row label's bottom edge drags that row's height (at least 6 px), as a column label's right edge drags its width; elsewhere on the label it still selects the row. It sends `GridAction::ResizeRow` while dragging and `GridAction::ResizeEnd` when let go (for columns too).
- **Sheets:** the drag shows live (`Doc::size_preview`, not journaled), and letting go makes it one undoable `GridOp::Sizes` ("Row Height" or "Column Width", `Doc::size_done`). Column widths used to change without undo.
- **Fit a column** (2026-11-19): a double-click on a column label's right edge sends `GridAction::FitCol`; Sheets sets the column to its widest shown entry (`SheetSource::fit_width`: shown rows, cells not merged across columns, at most 200,000 cells read; pergola `needed_width` measures a cell view's widest line with its indent, icon and padding), as one undo step. An empty column is left alone.
- **Several at once** (2026-11-19): with whole columns (or rows) selected, dragging or double-clicking the edge of one of them sizes all of them (each fitted to its own entries on a double-click), as one undo step; up to 16,384 at once (`app::sized_span`).
- **Exact sizes** (2026-11-19; Data menu): Row Height… and Column Width… open a bar holding the cursor's row height or column width in pixels; a number from 0 (hides) to 545 for rows or 1790 for columns sets every selected row or column (up to 16,384) as one undo step, and anything else leaves the bar open with a note. Sizes are in pixels, not Excel's points and characters.
- **Excel's units** (2026-11-19): the size bar also takes `15pt` (rows or columns, 0.75 pt a px) and, for columns, `8.43ch` (Excel's characters: 7 px each plus 5 px), and on opening the status bar gives the size in them (`app::typed_px`, `excel_size`). Fitting rows: §16.93.
- **Gates:** pergola `a_row_label_s_edge_resizes_the_row`, `a_double_click_on_a_column_edge_asks_to_fit_it`; atrium-sheets `a_dragged_edge_is_one_undo_step`, `a_column_fits_its_widest_shown_entry`, `an_edge_of_selected_whole_columns_sizes_them_all`, `the_size_bar_sets_exact_heights_and_widths`.

### 16.91 The grid scrolls on while a selection is dragged past its edge (2026-11-19)

- **Grid** (pergola): a selection or the fill handle dragged past the cells' edge takes the cell at that edge, and while the pointer stays out there each frame scrolls toward it (faster the farther out, 4 to 60 px a frame), the selection following. Before, a drag above the view jumped to row 1. `GridState.edge`, `past_edge`, `cell_at_edge`.
- **pergola App:** a drag's move now asks for frames, so `on_frame` handlers can animate while a press is held still; handlers that don't need them stop the frames as before.
- **Gate:** pergola `a_selection_held_past_the_edge_scrolls_on`.

### 16.92 The pointer in the cell editor (2026-11-19)

- **Grid** (pergola): while a cell is being edited, a press inside the editor (the cell, or as wide as its text runs) places the caret at the nearest character instead of committing; dragging selects text, Shift+press extends, and a double-click selects the word. A press anywhere else still commits and moves, as before. `editor_rect`, `index_at` (a binary search over measured prefixes).
- **Gate:** pergola `a_press_in_the_editor_places_the_caret_and_a_drag_selects` (press, drag, double-click word, Shift+press).

### 16.93 Wrapped text and rows fitted to it (2026-11-19)

- **Before:** a style's wrap flag was read and written with files but the grid ignored it, and text with line breaks showed on one line.
- **Grid** (pergola): `CellView.wrap` breaks text onto lines to the cell's width, at spaces, or between letters for a word wider than the cell (`text_lines` over `text::lines_with`, at most 200 lines). Text with line breaks shows its lines whether it wraps or not. The lines are centred down the cell when they fit; otherwise as many as fit whole are shown from the top, at least one. Wrapped text never runs over its neighbours.
- **Fit a row:** a double-click on a row label's bottom edge sends `GridAction::FitRow`. Sheets sets the row to its tallest cell's lines, at least the default height (`SheetSource::fit_height`; `needed_height` measures a cell view's lines in a column's width; hidden columns and cells merged across rows don't count, at most 200,000 cells). With whole rows selected, each is fitted, as one undo step.
- **Hidden rows:** just below the double line of hidden rows, the double-click shows them again, as Unhide Rows would (`data::unhide_rows_at`), as for columns (§16.89).
- **Sheets:** Data ▸ Wrap Text turns wrapping on or off for the selection (ticked when its first cell wraps), one undo step.
- **Line breaks:** Alt+Enter in the cell editor puts a line break at the caret and keeps editing, as in Excel (`TextEdit::insert_break`).
- **Rows grow as you type** (2026-11-19): when what is typed needs more lines than its row shows and nobody has sized the row, the row grows to fit, in the same undo step as the typing (journal record `Extend`, tag 7, `Doc::grow_row`). A row sized by hand keeps its height.
- **Vertical alignment** (2026-11-19): `CellView.valign` (Top, Center, Bottom) places a cell's lines in a taller row; Sheets follows the style's `v_align`, bottom by default as in Excel. Data ▸ Align Top, Align Middle, Align Bottom set it for the selection, one undo step each.
- **The editor grows** (2026-11-19): with line breaks it is as tall as its lines, draws each with its selection and caret, and a press picks the line under it.
- **Rows shrink back** (2026-11-19): a row grown for typed lines is fitted again when its text changes or is cleared, back down to the default height (`Doc.grown_rows`, this session only; sizing it by hand ends that).
- **Up and Down** in an editor holding several lines move between them at about the same place across; past the first or last line they end editing and move, as before.
- **The editor wraps** (2026-11-19): in a cell that wraps its text, the editor breaks its lines as the cell does (after spaces, or inside a word too long for the line), and Up and Down move between them (`editor_wrap`, `editor_lines`). Gate: pergola `the_editor_wraps_as_its_cell_does`.
- **Printing:** wrapped and explicit lines follow the cell width, height and vertical alignment in the print snapshot (§16.79).
- **Gates:** pergola `a_double_click_on_a_row_edge_asks_to_fit_it_and_wrapped_text_is_taller` (with `line_top`), `a_press_in_the_editor_places_the_caret_and_a_drag_selects` (Alt+Enter, the second line); atrium-sheets `a_row_fits_its_wrapped_text`, `wrap_text_toggles_and_a_hidden_row_s_line_shows_it` (with alignment), `rows_nobody_sized_grow_for_typed_lines`, `records_round_trip`.

The `layout` profile of `scripts/sheets-interop-vm-check.sh` verifies
these features together with frozen panes (§16.82), merged cells (§16.79),
and hidden axes and manual sizes (§16.89–16.90) in XLSX and ODS. It edits
a cell, drags a row and column edge, saves the undo and redo states, and
reopens through the installed picker and converter. Checkpoints assert the
visible and hidden dimensions and the wrapped and merged cells. The
scrolled screenshots show the pinned top row and left column after reopen.

### 16.94 Entry keys and typed dates (2026-11-19)

- **Typed dates and times:** typing `2026-11-19`, `14:30`, `14:30:05` or `2026-11-19 14:30` enters a date or time serial, and a cell still in General format gets `yyyy-mm-dd`, `hh:mm` (and so on) in the same undo step (`doc::typed_date`). Before, these stayed text. Impossible days (`2026-02-31`) stay text.
- **Ctrl+Enter** in the editor enters what is typed in every selected cell, as one undo step: formulas move as a copy moves them (`=A1+1` typed over B3:C5 reads `=B3+1` in C5), formats stay. Typing no longer collapses the selection to the active cell, so the selection is still there for it. At most 10,000 cells (`Doc::type_all`, `GridAction::CommitAll`).
- **Ctrl+;** and **Ctrl+Shift+;** put today's date or the time now (local) into the editor, starting it when needed; they become a date or time as above (`GridSource::now_text`).
- **Size bar:** 0 now hides, as it said: rows through Hide Rows, columns by width 0. Before, it set 1 px.
- **Entering and editing** (2026-11-19): an editor started by typing (or Ctrl+;) is entering, as Excel's Enter mode: an arrow key finishes the entry and moves that way. One started by F2 or Enter, or pressed into, is editing: arrows move the caret. F2 in the editor switches between the two.
- **Ctrl+'** puts the formula of the cell above into the editor as written; **Ctrl+Shift+'** its value as shown.
- **Enter or Edit in the status bar** (2026-11-19): while the editor is open, the status bar's right end says Enter or Edit, as Excel's does (`GridState::edit_mode`, `EditMode`).
- **Other date orders** (2026-11-19; `doc::typed_date_with`):
  - Numbers in the persona's order: `19/11/2026`, `19.11.26` and `19-11-2026` day first, or `11/19/2026` month first. The order comes from the persona's locale (`LC_ALL`, `LC_TIME`, `LANG`; `doc::day_first`). It is month first only in the United States and the places that follow it, and day first elsewhere and when no locale is set.
  - A month by name in any order: `19 Nov 2026`, `November 19, 2026`, `Sept 1 2026`, `Nov 2026`.
  - Without a year (`19/11`, `Nov 19`): this year.
  - A time may follow any of them.
  - Each is shown in the order it was typed (`dd/mm/yyyy`, `d mmm yyyy`, `mmm d, yyyy` and so on).
  - Two-digit years are 1930–2029. A dash needs a four-digit year, and a dot needs the year, so `1.5` stays a number and `26-11-19` stays text.
- **Gates:** pergola `ctrl_enter_enters_the_text_in_every_selected_cell`, `arrows_finish_a_typed_entry_and_f2_edits_in_place`, `a_press_in_the_editor_places_the_caret_and_a_drag_selects` (Up and Down); atrium-sheets `typed_dates_and_times_become_them`, `ctrl_semicolon_gives_today_and_the_time_as_typed`, `rows_nobody_sized_grow_for_typed_lines` (shrinking), `the_size_bar_sets_exact_heights_and_widths` (0 hides).

### 16.95 Everyday commands: Remove Duplicates, Text to Columns, AutoSum, Clear, Go To (2026-11-24)

The Data menu gains five commands. Each makes one undo step.

- **Remove Duplicates** works on the data around the cursor, or on the selection, with its header row kept. A row that matches an earlier row in every column is taken out, and the rows below move up. Text matches without regard to case, as in Excel. The rows' values, formulas, styles, comments and links move together through the same reorder that sorting uses (`officina_grid::sort::reorder_op`). It reads at most 1,000,000 rows. It refuses while a filter hides rows or the rows' merges differ, as sorting does.
- **Text to Columns** splits one column, the selection or the cursor's column of the data. It splits at the delimiter most of the cells use: tab, semicolon, comma, bar, then space. Pieces are typed as typing would read them, so numbers become numbers, and a piece that starts with `=` stays text. The columns to the right must be empty, or it says which ones are in the way. It splits at most 100,000 rows into at most 256 columns.
- **AutoSum** (Alt+=). On one cell it sums the run of numbers just above it, or else just left of it. On a range it puts each column's SUM in the empty row under it.
- **Clear Formats** puts the selection back to the default format. **Clear All** also empties the cells and takes off their comments and links.
- **Go To…** (Ctrl+G or F5) opens a bar under the grid. It takes `B4`, `B4:D9`, `C:D`, `4:6`, any of them after `Sheet2!` or `'My Totals'!`, or a defined name. It shows that sheet and selects the range. An unknown sheet or name is named in the status, and the bar stays open (`data::go_to_place`).

pergola sends `GridAction::AutoSum` and `GridAction::GoTo`. Gates: atrium-sheets `remove_duplicates_keeps_the_first_of_each_row_and_undoes`, `text_to_columns_splits_at_the_delimiter_the_cells_use`, `autosum_totals_the_numbers_above_left_or_under_a_selection`, `clear_formats_and_clear_all` and `go_to_selects_a_cell_range_sheet_or_name`; pergola's grid key test. Find and Replace was already built (§8.4). Not yet: a choice of delimiter or fixed widths for Text to Columns, typed dates among its pieces, and a column choice for Remove Duplicates.

### 16.96 Filter conditions and table total rows (2026-11-24)

- **Filter conditions.** A filter column's panel opens on a menu: Pick values, as before, or a condition. The conditions are Equals, Does not equal, Greater than, Greater than or equal to, Less than, Less than or equal to, Between, Top, Bottom, Top %, Bottom %, Begins with, Ends with, Contains and Does not contain. A number compares as a number and other text compares as text, ignoring case. Between takes two numbers in either order. Top and Bottom take a row count, and the % forms take 1 to 100. Apply puts the condition in place of the column's choice. A refusal is said in the status, and the panel stays open. The panel reopens on the condition it made. The engine already held these conditions (`Cond::Compare`, `Rank`, `Custom` with `TextOp`), and `.xlsx` already reads and writes them. Code: `data::FILTER_CONDS`, `filter_cond`, `cond_fields`, `set_condition`.
- **Table total row.** Data ▸ Total Row (checked when on) adds a row under a table, as one undo step. Its first column says Total, and each column holding numbers gets `=SUBTOTAL(109,Table1[Column])`, a sum of the rows the filter shows. The row under the table must be empty first. Turning it off empties the row and shrinks the table.

Gates: atrium-sheets `a_filter_column_takes_a_condition` (each condition applied, read back and refused) and `a_table_total_row_sums_the_shown_rows_and_goes_again`. Not yet: Above or below average, date conditions in the panel (the engine has `DynamicDate`), two conditions joined by and or or, and a total row function other than SUM.

### 16.97 Cell formatting: the Format menu (2026-11-24)

Before this, Sheets showed the formatting a file brought in but had no way to set it. A **Format** menu beside Data now acts on the selection, as one undo step per command (`atrium-sheets::format`). Bold, italic, underline and strikethrough follow the first cell's state: a command turns the effect on for every cell when that cell lacks it, and off otherwise.

- **Font:** Bold (Ctrl+B), Italic (Ctrl+I), Underline (Ctrl+U), Strikethrough (Ctrl+5). Larger Text and Smaller Text step through 8–72 pt; 11 pt is the default size.
- **Colours:** Text Colour… and Fill Colour… open a bar under the grid with fourteen swatches: the eight tab colours and six light fills. The bar also has Automatic (text) or No Fill, and Cancel. A fill takes off any pattern or gradient.
- **Alignment:** Align Left, Centre and Right, and Align by Value (General). Vertical alignment stays in Data.
- **Number formats:** General (Ctrl+Shift+~), Number `#,##0.00` (Ctrl+Shift+!), Currency `$#,##0.00` (Ctrl+Shift+$), Percent `0%` (Ctrl+Shift+%), Date `yyyy-mm-dd` (Ctrl+Shift+#), Time `hh:mm` (Ctrl+Shift+@), Scientific `0.00E+00` (Ctrl+Shift+^) and Text `@`. More Decimals and Fewer Decimals change each section of the first cell's format code (`format::decimals`); exponent digits and quoted or bracketed parts are left alone.
- **Borders:** All Borders, Outside Borders (built from disjoint corners and strips, so a corner gets both of its edges), Bottom Border, and No Borders. All are thin and automatic in colour.
- **Grid:** cells now show italic, underline, strikethrough and font size. pergola's `CellView` gains `marks` (the `inline::Emphasis` bits) and `size`. Fitting a column or row measures with them. pergola sends `GridAction::Format(char)` for the keys.

Gates: atrium-sheets `the_format_menu_styles_the_selection_and_the_grid_shows_it` and `format::tests::decimals_are_added_and_taken_in_every_section`; pergola `italic_and_size_reach_the_cell_style` and the grid key test. Not yet: a font name picker, custom colours, other border styles and colours, a format code box (Format Cells), Format Painter and rows growing for larger text by themselves. Printing draws italic, underline, strike and sizes as the grid does (officina-pdf `a_cell_prints_in_its_own_size_and_italic`).

### 16.98 Insert and delete cells, custom number formats, rows that follow text size (2026-11-24)

- **Insert and delete cells** (Data menu): Insert Cells, Shift Down or Shift Right, and Delete Cells, Shift Up or Shift Left, as Excel's Insert and Delete dialogs offer them. The cells from the selection (inserting) or past it (deleting) to the end of what is used, across the selection's columns or rows, are cut and pasted along by the selection's height or width as one undo step (`data::shift_cells`). Formulas elsewhere that read the moved cells follow them, as a cut does. Merged cells crossing the moving block are refused; at most 2,000,000 cells move. Deleting first turns references to the deleted cells (a cell, or a range wholly inside them) into `#REF!` in formulas outside them (`officina_grid::clip::kill_refs`), then moves the rest in, all as one step (§16.102). Merged cells anywhere in the cells that would move are refused, since a move doesn't carry merges.
- **Custom Format…** (Format menu) opens a bar with the cursor cell's format code. Apply sets the code on the selection as one step, and the status shows how the first cell now reads. A code with a part the formatter doesn't understand (`NumFmt::is_exact`) is refused and the bar stays open. Codes are at most 255 characters.
- **Row heights:** Larger Text and Smaller Text refit rows nobody sized (the first 1,000 selected rows that hold something), as part of the same step. Rows a person sized keep their height.

Gates: atrium-sheets `insert_and_delete_cells_shift_their_neighbours_and_formulas_follow`, `the_custom_format_bar_takes_a_code_and_refuses_what_it_cannot_read` and `larger_text_grows_rows_nobody_sized_and_smaller_brings_them_back`; all 123 Sheets lib tests pass.

### 16.99 Duplicate Sheet, Define Name, indent (2026-11-24)

- **Duplicate Sheet** (Data menu) puts a copy of the shown sheet right after it, named `Name (2)` (or the next free number), as one undo step (`data::duplicate_sheet`). The copy brings the cells with their formulas, formats, comments and links, plus merged cells, row heights and column widths, frozen panes and tab colour. Formulas in the copy that read their own sheet read the copy. At most 20,000,000 used cells are copied. Pivot tables are copied since §16.108. (Conditional formats, validations and sheet-scoped names are copied since §16.100; charts, pictures and tables since §16.106.)
- **Define Name…** (Data menu) opens a bar that names the selection for the whole workbook as one step (`data::define_name`), for example `Sheet1!$A$1:$B$2`. A name already in use now means the new place. Names that look like cells, or that break Excel's rules, are refused and the bar stays open. Formulas and Go To use the names at once. A Name Manager came in §16.100.
- **Increase Indent / Decrease Indent** (Format menu) step text in from the left by up to 15 steps. Indenting General or centred text moves it to the left, as Excel does. The grid now draws a cell style's indent for left-aligned and General cells, which also covers indents that files bring in.

Gates: atrium-sheets `duplicate_sheet_copies_cells_formats_sizes_and_merges_after_it`, `define_name_names_the_selection_and_formulas_and_go_to_use_it` and `indent_steps_text_in_and_the_grid_draws_it`; all 126 Sheets lib tests pass.

### 16.100 Name Manager, and a fuller Duplicate Sheet (2026-11-24)

- **Name Manager…** (Data menu) opens a panel at the right of the grid. It lists every defined name: workbook names first, then sheet names with their sheet, each with what it refers to. Excel's own names show without `_xlnm.`, for example Print_Area. **Go** selects what a name covers, on its sheet; a name for a constant or a formula says it isn't a place. **Delete** removes a name as one undo step, and formulas that used it show `#NAME?` (`data::name_rows`, `name_place`, `delete_name`). Not yet: editing what a name refers to in the panel. (Rename came in §16.104.) Define Name with the same spelling moves a name.
- **Duplicate Sheet** now also copies the sheet's conditional formats, data validations and sheet-scoped names (such as its print area) onto the copy. The sheet move comes last so that these follow the copy to its place.

Gates: atrium-sheets `the_name_manager_lists_goes_to_and_deletes_names` and the extended `duplicate_sheet_copies_cells_formats_sizes_and_merges_after_it`; all 127 Sheets lib tests pass.

### 16.101 Hidden sheets (2026-11-24)

Before this, a hidden sheet in an .xlsx file opened shown, with a "hidden sheets shown" import note. Sheets can now be hidden end to end.

- **Engine:** `Sheet::sheet_hidden`, and `GridOp::HideSheet { sheet, hidden }` (wire kind 27), which is undoable. Hiding the last sheet that shows changes nothing, so a workbook always has a sheet on screen (`Workbook::shown_sheets`). The native file keeps hidden sheets in chunk `HIDDEN_SHEETS` (28). A file that hides every sheet, names a sheet twice or names a missing sheet is refused. Deleting a hidden sheet shows it first, so undo brings it back hidden. A protected sheet can't be hidden or shown.
- **Files:** .xlsx reads `state="hidden"` and `"veryHidden"` as hidden and writes `state="hidden"`; a very hidden sheet comes back merely hidden, which Unhide can show. .ods reads and writes `table:display="false"` on the table style. An .xlsx that hides every sheet opens with the first shown, with a note.
- **Sheets:** Data ▸ Hide Sheet hides the shown sheet and opens the next one that shows. Data ▸ Unhide *Name* lists up to 32 hidden sheets by name; choosing one shows it and opens it. Tabs leave hidden sheets out. Go To refuses a hidden sheet and says how to unhide it. A file that opens on a hidden sheet opens on the first that shows.

Gates: officina-grid `sheets_are_hidden_shown_undone_and_saved_but_one_always_shows`; officina-convert `hidden_sheets_stay_hidden_through_xlsx_and_ods`; atrium-sheets `hide_sheet_takes_its_tab_away_and_unhide_brings_it_back`. All 121 officina-grid lib, all officina-convert and all 128 Sheets lib tests pass.

### 16.102 Delete Cells breaks references to the deleted cells (2026-11-24)

Delete Cells (shift up or left) now turns references to the deleted cells into `#REF!`, as Excel does, even when nothing moves in to replace them. `officina_grid::clip::kill_refs(wb, rect)` rewrites every formula outside `rect` that names a cell inside it, or a range wholly inside it. It uses the cut mover with nothing moving. Ranges that only overlap the deleted cells keep their ends. Excel would shrink them; that is not done yet. The rewrite and the clearing run first, then the cells past the deletion are cut and pasted in from the workbook as it now is, so the mover never sees a stale formula. The two halves are one undo step, and if the paste is refused the first half is undone.

Gates: atrium-sheets `deleted_cells_turn_formulas_reading_them_into_ref_errors`. It fails on the code before (`=D4` stayed `=D4` after D4 was deleted with nothing to its right). All 129 Sheets lib and 121 officina-grid lib tests pass.

### 16.103 Text rotation, more border styles, typed colours, Clear Comments (2026-11-24)

- **Rotation** (Format menu): No Rotation, Angle Counterclockwise (45°), Angle Clockwise (−45°), Vertical Text (stacked, 255), Rotate Text Up (90°) and Rotate Text Down (−90°), as Excel's Orientation button offers them. The grid already draws `Style::rotation`.
- **Borders:** Top and Bottom Border (thin), Thick Outside Borders, Thick Bottom Border and Double Bottom Border join All, Outside, Bottom and None. Thick outside uses the same disjoint corner and strip rectangles as Outside, so the inside cells keep their own edges.
- **Typed colours:** the text and fill colour bar has a `#RRGGBB` field (`#RGB` and a bare `RRGGBB` also work, via `format::parse_hex`). Set applies the colour as one step and closes the bar. A colour the field can't read keeps the bar open and says how to write one.
- **Clear Comments** (Data menu) takes the selection's comments off as one step, leaving contents and formats.

Gate: atrium-sheets `rotation_border_styles_hex_colours_and_clear_comments`; all 130 Sheets lib tests pass. Border lines and colours came in §16.104, and a typed rotation angle in §16.105.

### 16.104 Border pens, and renaming names (2026-11-24)

- **Border pen** (Format menu): Border Line: Thin, Medium, Dashed, Dotted or Hair, and Border Colour: Automatic or one of the eight tab colours. Both are radio items and stay set for the session (`Doc::border_pen`). All, Outside, Bottom, and Top and Bottom draw with the pen. Thick and Double keep their own line and take the pen's colour.
- **Rename** in the Name Manager opens the name bar filled with the name. `officina_grid::names::rename_ops` renames the definition and rewrites every formula and other name that uses it, as one step. A workbook name isn't rewritten on a sheet whose own name of that spelling hides it, and a sheet name is rewritten only on its sheet. A formula that binds the spelling itself with LET or LAMBDA is left unchanged and counted in the status. Clashing and invalid names are refused, and the bar stays open. Conditional formats and validations follow since §16.106.

Gates: atrium-sheets `border_pens_draw_their_line_and_colour` and `renaming_a_name_rewrites_the_formulas_and_names_using_it`; all 132 Sheets lib and 121 officina-grid lib tests pass.

### 16.105 Custom Sort, and a typed rotation angle (2026-11-24)

- **Custom Sort…** (Data menu) opens a bar that sorts the data at the selection or cursor by several columns, first to last, as one step (`data::custom_sort`). Each column is a letter or a header (case does not matter), optionally followed by `desc` or `Z to A`, or `asc` or `A to Z`. Columns are separated by commas or `then`, for example `Region, Sales desc`. The bar starts with the cursor's column, by its header when the data has one. Columns outside the data, a column given twice, and more than 64 columns are refused, and the bar stays open. The checks Sort A to Z makes (hidden rows, merges, the header row) are shared (`data::sort_rect`).
- **Rotation Angle…** (Format menu, a radio item that is on for angles the presets don't cover) opens a bar for degrees from −90 to 90, counter-clockwise, as Excel's Format Cells takes them. A trailing `°` is fine.
- Opening a bar now closes the others through one helper (`Sheets::close_bars`).

Gates: atrium-sheets `custom_sort_orders_by_several_columns_named_by_letter_or_header` and `a_typed_rotation_angle_turns_the_text`; all 134 Sheets lib tests pass.

### 16.106 Duplicate Sheet brings charts and tables; renames reach rules (2026-11-24)

- **Duplicate Sheet** now copies the sheet's charts and pictures onto the copy. A chart's series ranges, and its title or series names taken from cells, follow to the copy when they were on the original sheet; ranges on other sheets stay. Tables are copied too, each under a fresh workbook-wide name `Name_2` (or the next free number, clashing with neither tables nor defined names). Formulas in the copy that name a copied table name the copy instead (since §16.107), as Excel's do. Pivots are copied since §16.108.
- **Renaming a name** also rewrites conditional-format rule formulas and data-validation formulas, on the sheets where the spelling means that name, with the same LET and LAMBDA exception as cell formulas (`names::rename_ops`).

Gates: atrium-sheets `duplicate_sheet_brings_charts_and_tables_under_fresh_names` and `renaming_a_name_reaches_conditional_formats_and_validations`. Both fail on the code before. All 136 Sheets lib and 121 officina-grid lib tests pass.

### 16.107 Copied formulas follow copied tables; Freeze First Column (2026-11-24)

- After Duplicate Sheet, formulas on the copy whose structured references name a table that was copied (`Sales[Qty]`) are rewritten to name the copy (`Sales_2[Qty]`), as part of the same step (`data::retable`). Formulas on other sheets keep naming the original.
- **Freeze First Column** (Data menu, beside Freeze Top Row) keeps the view's first column in view, as Freeze Top Row does for the top row.

Gates: the extended atrium-sheets `duplicate_sheet_brings_charts_and_tables_under_fresh_names` (the copy's `=SUM(Table[Qty])` names the copy and sums to 3) and `panes_freeze_at_the_cursor_and_unfreeze`; all 136 Sheets lib tests pass.

### 16.108 Paste Link, Merge Across, and pivots in Duplicate Sheet (2026-11-24)

- **Paste Link** (cell menu, after Paste Special…, enabled after a copy, not a cut) puts one formula per copied cell at the cursor, each reading its source cell (`=B2`, or `='Sheet 1'!B2` from another sheet), as one undo step. At most 1,000,000 links; it refuses when the block would run off the sheet, covers merged cells, or the source sheet is gone.
- **Merge Across** (cell menu, before Center Across Selection, two or more columns) merges each selected row on its own. As in Excel, merges the selection touched are replaced, and only each row's leftmost value stays; the status line says how many other values were cleared. At most 100,000 rows; one undo step.
- **Duplicate Sheet** now copies pivot tables whose output is on the sheet, each under a fresh name `Name_2` (or the next free number). A pivot reading data on the same sheet reads the copy's data; one reading another sheet keeps its source.
- Tests: `paste_link_points_at_the_copied_cells_and_merge_across_merges_each_row`, `duplicate_sheet_copies_a_pivot_table_reading_the_copy`. All 138 Sheets lib tests pass.

### 16.109 Goal Seek, Show Formulas, AutoFit from the menu (2026-11-25)

- **Goal Seek…** (cell menu after Go To, enabled on a formula) opens a bar "Set B2 to [goal, cell]" prefilled with the formula's value and the first cell on the sheet it reads that holds a number (`data::goal_seek_start`). Seek changes that cell by Newton steps on a measured slope until the formula is within 0.001 of the goal (Excel's default), at most 100 steps and 3 s, then prefers a round number that does as well (20 rather than 20.0000000003). The answer goes in as one undo step; with no answer the cell is left as it was and the bar says the closest value. It refuses a cursor without a formula, an input cell holding a formula or text, and a goal that isn't a number. `goal, cell` may also be typed `goal by changing cell`.
- **Show Formulas** (checked item, not saved) shows each formula as typed in its cell instead of its value; hidden formulas stay hidden.
- **AutoFit Row Height** and **AutoFit Column Width** (after Row Height… and Column Width…) fit every selected row or column in the used area to its contents, as one undo step, like a double-click on the header edge. Hidden rows and columns stay hidden; at most 1,000 at a time.
- Tests: `goal_seek_finds_roots_and_leaves_the_cell_alone_without_one`, `goal_seek_show_formulas_and_autofit_from_the_menu`. All 140 Sheets lib tests pass.

### 16.110 Excel's formatting keys, Accounting and Comma Style (2026-11-25)

- Keys as in Excel: **Ctrl+`** turns Show Formulas on and off, **Ctrl+1** opens the Custom Format bar, **Ctrl+Shift+&** draws outside borders and **Ctrl+Shift+_** takes borders away. The menus show them. The keys are mapped in pergola's grid (`format_key`) and in `format::for_key`.
- The Format menu's number formats gain **Accounting** (`_($* #,##0.00_);…`), **Comma Style** (the same without the currency sign: negatives in brackets, zero as a dash), **Long Date** (`dddd, mmmm d, yyyy`) and **Fraction** (`# ?/?`), after Text. The `*` fill is drawn to the column width since §16.112.
- Tests: the format-menu test checks the new keys and that each new format shows a value as Excel does; pergola's grid key test checks the four keys. All 140 Sheets lib tests pass.

### 16.111 Excel's row, column and selection keys (2026-11-25)

- **Ctrl+Space** selects the selection's whole columns and **Shift+Space** its whole rows, as a header click does (pergola grid, so every grid app gets them).
- **Ctrl+9** / **Ctrl+0** hide the selected rows / columns; **Ctrl+Shift+(** / **Ctrl+Shift+)** unhide them.
- **Ctrl+-** deletes and **Ctrl++** (or Ctrl+Shift+=) inserts: whole rows or columns when those are selected, otherwise cells, shifting them up or down. Excel asks in a dialog; Sheets takes the dialog's first choice, and the menu offers the others (`data::key_cmd`).
- The menu shows these keys. Tests: `excel_row_and_column_keys_pick_the_command_for_the_selection`, and pergola's grid key test. All 141 Sheets lib tests and 229 pergola lib tests pass.

### 16.112 Number formats' `*` fill drawn to the cell's width (2026-11-25)

- A number format's `*x` fill (Accounting, Comma Style, `0*-`, …) now does what Excel's does. The parser marks where the fill goes, and `numfmt::Formatted::fill` gives the byte offset of the fill character in the text. The text still holds that character once, so copies, CSV and print are unchanged.
- The grid (`CellView::fill_at`) draws the text before the fill at the cell's left edge and the text after it at the right edge. A visible fill character is repeated between them, at most 512 times. Accounting's `$` therefore sits at the left and the number at the right. A cell too narrow for both shows `#`s like any number. An offset that isn't on a character boundary is ignored.
- Sheets passes the fill through for cell styles and for conditional-format number formats.
- Tests: `a_star_fill_is_shown_once_and_its_place_kept` (officina-grid), `a_star_fill_puts_the_sign_left_and_the_number_right` (pergola), and the Sheets format-menu test. Results: officina-grid 122, pergola 230 and Sheets 141 lib tests pass.

### 16.113 F4 anchors references; Ctrl+T, Ctrl+Shift+L, Ctrl+K (2026-11-25)

- **F4** while editing a formula cycles the reference at the caret through A1 → $A$1 → A$1 → $A1 → A1, as Excel does. Both ends of a range change together. A reference after a sheet name (`Sheet2!A1`) counts. Function names (`LOG10(`), defined names (`Tax_A1`) and text outside a formula are left alone (`pergola::widgets::grid_view::cycle_ref`, so every grid editor gets it).
- **Ctrl+T** (or Ctrl+L) is Format as Table, **Ctrl+Shift+L** turns Filter on or off, and **Ctrl+K** opens Insert/Edit Link. The menu shows these keys (`data::key_cmd`).
- Tests: `f4_cycles_the_reference_at_the_caret`, the grid editor's F2/F4 key test, the grid key test, and `excel_row_and_column_keys_pick_the_command_for_the_selection`. Results: pergola 231 and Sheets 141 lib tests pass.

### 16.114 F4 repeats; Ctrl+A and Ctrl+Shift+* select the current region (2026-11-25)

- **F4** outside the editor repeats the last command on the current selection, as Excel's Repeat does. That covers format commands, a colour typed as hex, and data commands that act on cells in place: insert, delete, hide, merge, align, clear, the Paste Special family, sort, AutoSum and fill (`format::repeatable`, `data::repeatable`). Commands that only open a bar, such as Fill Colour… or Custom Format…, or that set the border pen, don't replace what F4 repeats. Before any command, F4 says there's nothing to repeat yet. Inside the editor, F4 still anchors references (§16.113).
- **Ctrl+Shift+*** selects the current region: the rectangle around the cursor, grown while any cell just outside it, diagonals included, holds something. **Ctrl+A** does the same first. Pressed again, or on a cell with nothing around it, Ctrl+A selects everything (`pergola::widgets::grid_view::current_region`). Each side is grown as far as it goes in one pass, and at most 4,000,000 cells are looked at.
- Tests: `f4_repeats_the_last_command_on_the_new_selection` (Sheets) and `ctrl_a_and_ctrl_shift_star_select_the_data_around_the_cursor` (pergola). Results: Sheets 142 and pergola 232 lib tests pass.

### 16.115 Sheet and comment keys (2026-11-25)

- **Ctrl+PageDown** and **Ctrl+PageUp** show the next or previous sheet. Hidden sheets are skipped, there is no wrap at either end (as in Excel), and the cursor stays where it was (`tabs::step_sheet`).
- **Shift+F11** adds a sheet and shows it. **Shift+F2** opens the comment bar for the cursor's cell, where Excel opens its note editor. Plain F2 still edits the cell.
- pergola's grid sends `GridAction::Sheet(-1 | 0 | 1)` and `GridAction::Comment`. Tests: `ctrl_page_keys_step_through_shown_sheets_only` (Sheets) and the grid's region and key test (pergola). Sheets 143 and pergola 237 lib tests pass. pergola's lib tests now build only in debug, since the UI inspector's `Link` is `debug_assertions`-only.

### 16.116 Active-cell, screen and Paste Special keys (2026-11-25)

- **Ctrl+.** moves the active cell to the selection's next corner, clockwise, and keeps the selection. **Shift+Backspace** leaves only the active cell selected. Neither clears anything.
- **Ctrl+Backspace** scrolls back to the active cell. Plain Backspace and Delete still clear the selection.
- **Alt+PageDown** and **Alt+PageUp** move one screen right or left. The step is as many columns as fit at the active column's width, so the two keys undo each other. Hidden columns are stepped over, and Ctrl+PageUp/PageDown still change sheet.
- **Ctrl+Alt+V** opens the Paste Special bar (pergola's grid sends `GridAction::PasteSpecial`). The menu item shows the key.
- Tests: `excel_keys_for_the_active_cell_corners_and_screens` (pergola) and the Paste Special test (Sheets). Sheets 143 and pergola 239 lib tests pass.

### 16.117 Window keys: save, open, new, Save As, Name Manager (2026-11-25)

- **Ctrl+S** saves, **F12** opens Save As, **Ctrl+O** opens the file picker, and **Ctrl+N** opens a new workbook. **Ctrl+F3** opens the Name Manager. Ctrl+P, Ctrl+F and Ctrl+H work as before, now through the same path.
- `Sheets::window_key(usage, modifiers)` maps the window's raw key (USB HID usage and Fresco's modifier bits) to a `Cmd`. It enters a cell being edited first, as Excel does. It answers nothing while the password bar, the close question or a picker is up, or with Alt or the logo key held. On the launcher only Ctrl+O and Ctrl+N work.
- The menus show Ctrl+F for Find and Replace and Ctrl+F3 for the Name Manager.
- Test: `window_keys_save_open_find_and_enter_the_cell_being_edited`. All 144 Sheets lib tests and the bin tests pass.

### 16.118 Calculation keys, Insert Function and Paste Name (2026-11-25)

- **F9** (and **Ctrl+Alt+F9**) calculates every formula again, and **Shift+F9** the sheet on screen (`Workbook::mark_all`, then the usual sliced recalculation). Sheets always calculates automatically, so these matter for volatile functions such as RAND and NOW. As in Excel's automatic mode, those update on every sheet with any calculation. The Data menu has **Calculate Now** and **Calculate Sheet**. F9 does nothing while a cell is being edited, where Excel would work out part of the formula.
- **Shift+F3** opens a searchable list of the functions Sheets has (**Insert Function…**). **F3** lists the names the sheet can use: the workbook's and this sheet's own, without Excel's built-in `_xlnm.` ones (**Paste Name…**). A pick goes into the open editor at its caret (a function as `NAME(`), or starts a formula with `=` when no cell is being edited. F3 with no names says so. The lists reuse the validation list panel (`ListKind`).
- Test: `f9_calculates_and_f3_lists_go_into_the_formula`. All 145 Sheets and 122 officina-grid lib tests pass.

### 16.119 AutoComplete and Pick From Drop-down List (2026-11-25)

- **AutoComplete.** Typing text in a cell finishes it when exactly one entry of the column's list begins with what was typed, ignoring case. The guessed rest is selected, so the next key replaces it, Backspace drops it, and Enter keeps it. The entry's own case is used, as in Excel. Formulas (`=`) and numbers are never completed. pergola's grid asks `GridSource::complete` after each typed character at the end of the editor; the default offers nothing.
- **The column's list** (`data::column_entries`) is the filled cells next to the cell, above and below until the first empty one, at most `MAX_ENTRY_LOOK` (10,000) each way. Only text counts, each entry once ignoring case.
- **Alt+Down** on a cell with no validation list now offers that list, sorted, in the list panel (Excel's Pick From Drop-down List). The menu item is now **Pick from List…** (Alt+Down), enabled for either kind of list.
- Tests: `typing_completes_from_the_source_and_the_next_key_replaces_it` (pergola) and `autocomplete_and_pick_from_list_read_the_column_around_the_cell` (Sheets). All 146 Sheets and 240 pergola lib tests pass.

### 16.120 Go to precedents and dependents: Ctrl+[ and Ctrl+] (2026-11-25)

- **Ctrl+[** selects what the cursor's formula reads directly: the first cell or area on the sheet on screen, or else on another sheet, which is then shown. **Ctrl+]** selects the first formula cell that reads the cursor's cell directly, from any sheet. The status bar names them all, for example "Precedents (2): A1:A2, Sheet2!B3." It lists the first 8 and gives the count of the rest.
- Excel selects every precedent at once. The grid has one selection range, so Sheets goes to the first one and names the others. Pressing the key again traces on from there.
- `data::precedents` reads the formula group's footprints at the cursor's row, so names and tables are included. `data::dependents` checks every group's footprints for the row that reads the cell, so a filled-down `=A1*2` gives only the row reading it. At most `MAX_DEPENDENTS` (1,000) are listed. The menu has **Go to Precedents** and **Go to Dependents**.
- Test: `ctrl_brackets_go_to_precedents_and_dependents`. All 147 Sheets and 240 pergola lib tests pass.

### 16.121 Function argument tips (2026-11-25)

- While a formula is being typed, a tip under the formula bar shows the arguments of the function the caret is in. For example, `ROUND(number, num_digits)` appears with the current argument in bold. Optional arguments are in brackets, and `...` marks a list that can go on. The `...` becomes current once the caret is past the listed arguments.
- `fnhelp::call_at` finds the innermost function call before the caret. It skips commas inside nested calls, plain parentheses, array constants, strings and quoted sheet names. Inside a string there is no tip. At most `MAX_TIP_SCAN` (8 KiB) of the formula is read.
- `fnhelp::ARGS` holds Excel's argument names for each of Sheets' 128 functions. A test checks that every function has an entry and that its required and total arguments match the evaluator's `min` and `max`, so a new function without names fails the build's tests.
- Tests: `every_function_has_its_arguments_and_they_match_its_counts`, `the_caret_s_function_and_argument_are_found`, `the_tip_marks_the_current_argument` and `a_formula_being_typed_shows_its_function_s_arguments`. All 151 Sheets lib tests pass.

### 16.122 Formula AutoComplete, and Tab reaches the grid (2026-11-25)

- **Formula AutoComplete.** While typing a formula, the name being typed is finished when exactly one function or defined name begins with it. A function's guess ends with `(`. For example, `=vlo` becomes `=VLOOKUP(` with `OKUP(` selected. **Tab** takes the guess and editing goes on. A typed key replaces the guess, and Backspace or Delete removes it. Enter, the arrows and the other keys drop it before they act, so a formula is never committed with an unwanted guess. Text AutoComplete (§16.119) keeps its guess on Enter, as Excel does.
- There is no guess inside a string or a quoted sheet name, after `!`, `:` or `$` (a reference such as `Sheet1!XL`), or for a word that starts with a digit (`fnhelp::complete_formula`). The grid marks a guess it made (`GridState::guess`), so a tail that was selected by hand is never dropped.
- **Tab now reaches the grid.** The App used to take Tab for focus traversal, so the grid's Tab handling never ran. The grid now claims Tab (`takes_tab`): Tab moves one cell right and Shift+Tab one cell left, and in the editor Tab enters the cell and moves right. Ctrl+Tab still moves focus out of the grid.
- Tests: `formula_autocomplete_finishes_one_function_or_name` (Sheets) and `typing_completes_from_the_source_and_the_next_key_replaces_it` (pergola, extended). All 152 Sheets and 240 pergola lib tests pass.

### 16.123 Tab-then-Enter rows and F8 Extend Selection (2026-11-25)

- **Tab, then Enter.** After entering a row of cells with Tab, Enter goes to the next row in the column where the Tabs began, as in Excel, so a table can be typed row by row. Tab outside the editor counts as part of the run. Typing and editing keep the run going. An arrow key, a click, Shift+Tab or Escape ends it (`GridState::tab_from`).
- **F8** turns Extend Selection on. Arrows, Page keys and Home then extend the selection as if Shift were held. F8 again or Escape turns it off, and typing into a cell ends it. The status bar shows "Extend Selection" in the place where Excel shows it (`GridState::extending`).
- Excel also moves the active cell *inside* a selection with Enter and Tab. The grid's active cell is always a corner of its one selection range, so that is not done. Ctrl+. (§16.116) visits the corners instead.
- Tests: `enter_after_tabs_goes_to_the_next_row_where_they_began` and `f8_extends_the_selection_until_pressed_again_or_escape` (pergola), and `the_status_bar_shows_extend_selection_while_f8_is_on` (Sheets). All 153 Sheets and 242 pergola lib tests pass. `doc::tests::a_slow_edit_hands_the_window_back_and_greys_what_is_not_done` failed once while two builds ran at the same time. It is timing-based and passed on the rerun.

### 16.124 Selection summary in the status bar (2026-11-25)

- With two or more cells selected, the status bar shows **Average**, **Count** and **Sum** at its right, as Excel does, for example "Average: 2   Count: 3   Sum: 6". Count is the cells holding anything. Average and Sum cover the numbers only. With no numbers only the count shows, and nothing shows for one cell or an empty selection.
- `data::selection_stats` reads the selection with `Workbook::each`, which skips empty stretches unseen, so a whole column costs only its filled cells. The result is kept until the document's revision, the sheet or the selection changes, so it is computed once per change, not once per frame.
- Numbers use the General format (up to 15 significant digits). Excel uses the first cell's format and also leaves out rows a filter hides; Sheets does neither yet.
- Test: `the_status_bar_sums_up_the_selection`. All 154 Sheets lib tests pass.

### 16.125 Status bar summary: formats, filtered rows, and a choice of summaries (2026-11-25)

- This closes the two gaps noted in §16.124. The results show in the number format of the top-left number in the selection, so a currency column sums as currency. If that format gives nothing or only `#`, General is used instead. Rows hidden by a filter (an AutoFilter or a table's) are left out, as in Excel. `Sheet::row_filtered` is now public in officina-grid, and `Workbook::formatted_as` shows any value through a cell's format.
- A press on the summary opens a menu, as Excel's right-click on the status bar does. It offers **Average**, **Count**, **Numerical Count**, **Minimum**, **Maximum** and **Sum**, each with a check mark. Average, Count and Sum are on by default (`data::STATS_DEFAULT`). The choice holds for the window's session and is not saved in the workbook.
- Test: `the_status_bar_sums_up_the_selection`, extended to cover the format, a filter and the menu. All 154 Sheets and 122 officina-grid lib tests pass.

### 16.126 Format Painter (2026-11-25)

- **Format Painter** is in the cell menu. It takes the selection's formats; the next cells selected with the pointer (a press, a drag, or a column or row header) get them. Over a bigger selection the formats repeat as many whole times as fit, as Excel does. Values and formulas stay. The paint is one undo step and turns the painter off.
- Choosing Format Painter again while it is on keeps it on (Excel's double-click). It then paints each selection until **Escape** turns it off.
- pergola's grid now sends `GridAction::Selected(range)` when a press in the cells or the headers ends. Sheets turns that into `Cmd::Selected`, which paints only while the painter is on. `data::paint_formats` uses the clipboard's Paste Formats path on a private copy (`Clip::take`), so the user's clipboard is untouched. At most `MAX_PAINT_TILES` (4,096) copies are made.
- Tests: `format_painter_paints_the_next_selection_once_or_until_escape` (Sheets) and the drag and header tests (pergola), which now check that `Selected` comes once at the end of a press. All 155 Sheets and 242 pergola lib tests pass.

### 16.127 Zoom, stepping through a selection, End mode, Format Painter button (2026-11-25)

- **Zoom**, 10–400 %, per window (not saved with the workbook yet). Ctrl+wheel and a trackpad pinch zoom; **Ctrl+Alt+=** and **Ctrl+Alt+-** (Excel's) and the cell menu's Zoom In and Zoom Out go in 10 % steps to the next step up or down. Zoom to 100% and **Zoom to Selection** (fits the selection in the grid and puts it at the top left) are in the cell menu. The status bar shows the percent between − and + buttons at its right; pressing the percent goes back to 100 %.
- Rows, columns, cell text and charts scale; headers keep their size. Each pane (split or not) keeps its top-left cell when the zoom changes. Dragged column and row edges and fitted sizes are kept in 100 % units, so a size made at 150 % is the same at 100 %.
- How: `SheetSource::zoom` multiplies `row_top`/`col_left` and divides `row_at`/`col_at`; `cell` scales the text size, and `cell_at_100` keeps the unscaled view for fitting. pergola's `GridSource::zoom()` sizes the in-cell editor, and the grid sends `GridAction::Zoom(factor)` for Ctrl+wheel or a pinch instead of scrolling; Sheets turns it into `Cmd::Zoom(percent)`.
- **Enter and Tab in a selection of several cells** step the active cell through it, as Excel does: Enter down the column and on to the next column, Tab across the row and on to the next row, round to the first after the last; Shift goes back. The selection stays, so a block can be typed in cell by cell: typing enters the active cell, and Enter or Tab enters it and goes on. Any other move drops the selection. On a single cell Enter still opens the editor. pergola keeps the selection in `GridState::held` while the active cell steps (`step_in_selection`).
- **End mode**: End, then an arrow goes to the data's edge as Ctrl+arrow does (Shift extends); End then Home goes to the last used cell. The status bar shows "End Mode" while it is on; any other key or a press ends it.
- **Ctrl+Shift+Space** selects everything.
- **Format Painter** is now a toolbar button too, shown pressed while the painter is on. Pressing it again while on keeps it on, as Excel's double-click.
- Tests: `enter_and_tab_step_through_a_selection_end_mode_and_ctrl_wheel_zoom` (pergola; `keys_move_extend_and_edit` now expects Enter to stay in the selection) and `zoom_scales_rows_columns_and_text_and_keeps_the_top_left_cell` (Sheets). All 156 Sheets and 245 pergola lib tests pass.
- Not yet: headers scaling with the zoom. (Zoom saved per sheet came in §16.128.)

### 16.128 Zoom kept per sheet, editing a name's reference, Above and Below Average, total row functions, Text to Columns at… (2026-11-25)

- **Zoom per sheet, kept in the file.** Each sheet has its own zoom (`Sheet::zoom`, percent, 10–400, 100 as made), as in Excel; switching sheets shows each at its own. It is a view setting, not an undo step, and doesn't mark the workbook changed. `.xlsx` reads and writes `<sheetView zoomScale>` (values outside 10–400 are ignored on read); the native file has a new `ZOOM` chunk (29: count, then each sheet's index and percent; the reader refuses an out-of-range or repeated zoom, or 100). `SheetSource::zoom_factor` reads the sheet on screen. `.ods` doesn't keep it yet (its view settings live in settings.xml).
- **Name Manager: Edit** under each name opens the name bar with what the name refers to. A place, a constant or a formula is taken, with or without its `=`; formulas using the name follow, as one undo step (`data::edit_name_refers`).
- **Filter: Above average and Below average** in the filter panel's condition list, with nothing to type. The engine has `Cond::Average { below }` (wire kind 10): it keeps numbers strictly above (below) the average of the column's numbers in the filter range, worked out each time the filter is applied, as Excel does. `.xlsx` reads and writes `<dynamicFilter type="aboveAverage">` and `"belowAverage"`; `.ods` has no such filter and leaves it out.
- **Total row functions.** With the cursor on a table's total row, the cell menu lists Total: None, Average, Count, Count Numbers, Max, Min, Sum, StdDev and Var, the current one marked. Each writes `SUBTOTAL(code, Table[Column])` with Excel's codes that skip filtered rows (101, 103, 102, 104, 105, 109, 107, 110); None empties the cell (`data::TOTAL_FUNCTIONS`, `total_fn`).
- **Text to Columns at…** opens a bar that asks what to split at: a name (comma, tab, space, semicolon, bar, singular or plural) or up to 8 characters typed, such as `-` or ` / `. Empty or `auto` finds one as plain Text to Columns does. Cells without the delimiter stay as they are; a delimiter in none of them is refused and the bar stays (`data::text_to_columns_at`).
- Tests: `zoom_scales_rows_columns_and_text_and_keeps_the_top_left_cell` (now also per sheet), `a_name_s_reference_is_edited_in_the_name_manager`, `a_filter_column_takes_a_condition` (average, and its `.xlsx` round trip), `a_table_total_row_sums_the_shown_rows_and_goes_again` (functions), `text_to_columns_splits_at_the_delimiter_the_cells_use` (the bar) in Sheets; officina-grid `average_filter_keeps_numbers_above_or_below_and_is_saved`; officina-convert `each_sheets_zoom_round_trips_through_xlsx_and_the_native_file`. All 157 Sheets, 123 officina-grid and 65 officina-convert lib tests pass, and the convert ods, roundtrip and freeze tests.
- Not yet: zoom in `.ods`; consecutive delimiters as one and fixed widths for Text to Columns; a drop-down on the total row cell itself. (All three came in §16.129.)

### 16.129 Remove Duplicates by columns, two filter conditions, Text to Columns runs and fixed widths, zoom in .ods, the total row cell's list (2026-11-25)

- **Remove Duplicates by…** (cell menu) opens a bar that asks which columns make a row a duplicate: letters or headers, separated by commas (`Name, City`), as Excel's column check boxes. The first row with each combination stays; the rest close up, as one step (`data::remove_duplicates`, `columns_named`). Plain Remove Duplicates still compares every column.
- **Two filter conditions.** When the panel's condition is a comparison or a text test, And and Or buttons and a second condition (with its own value) appear under it, as Excel's Custom AutoFilter. The two become one `Cond::custom` (numbers stay `Compare`); opening the panel again shows both (`data::PanelCond`, `filter_cond2`, `panel_cond`). Top, Bottom, Between and the averages don't join.
- **Text to Columns at…** now also takes a `+` after the delimiter, so a run of it counts as one (`-+`, `comma+`; spaces always do), and `fixed 3, 8` for pieces cut after the 3rd and 8th characters (Excel's fixed width), each trimmed. Cells no longer than the first cut stay as they are (`data::parse_split`, `Split`).
- **Zoom in `.ods`**: `settings.xml` writes `ZoomType` 0 and `ZoomValue` for each sheet not at 100 % and reads `ZoomValue` back (10–400), next to the frozen panes it already kept.
- **The total row cell's own list.** A table's total row cell under the cursor shows a drop button at its right, as a list validation does; a press (or Alt+Down, or Choose from List) opens the list of totals, and a pick sets the function (`ListKind::Total`, `source::total_cell`).
- Tests: `remove_duplicates_keeps_the_first_of_each_row_and_undoes` (by columns), `a_filter_column_takes_a_condition` (two conditions), `text_to_columns_splits_at_the_delimiter_the_cells_use` (runs, fixed widths), `a_table_total_row_sums_the_shown_rows_and_goes_again` (the list) in Sheets; officina-convert `each_sheets_zoom_round_trips_through_xlsx_ods_and_the_native_file`. All 157 Sheets and 65 officina-convert lib tests pass, and its ods, roundtrip and freeze tests. The Sheets timing test `a_slow_edit_hands_the_window_back_and_greys_what_is_not_done` can miss its 5-slice bound when four test threads share four cores; it passes alone and with `--test-threads=2`.
- Not yet: more than two conditions per column in the panel. (Headers scaling with the zoom came in §16.130.)

### 16.130 Date filters, Font: Mono, dates from Text to Columns, headers with the zoom (2026-11-25)

- **Date is** in the filter panel's conditions, as Excel's Date Filters: a menu of Today, Yesterday, Tomorrow, this, last and next week, month, quarter and year, Year to date, Quarter 1 to 4 and January to December. It makes the engine's `Cond::DynamicDate` (already read from and written to `.xlsx` as `<dynamicFilter>`); relative ones are worked out each time the filter is applied. Only cells in a date format count. Opening the panel again shows the choice (`data::DATE_COND`, `DATE_RULES`).
- **Font: Sans and Font: Mono** in the Format menu. Mono gives the cells the font Courier New, which files keep; Sans takes the name off (the workbook's own font). pergola's `CellView::mono` draws a cell in the monospace face (`font::MONO`), and a file's monospace fonts (Courier, Consolas, Menlo, names with Mono or Code: `source::is_mono_font`) draw mono too; other names draw in the sans face as before. Printing still draws sans (officina-pdf, the printing thread's).
- **Text to Columns: dates.** A piece that reads as a date the way typing it would (ISO, or the persona's day order) becomes a date, and a cell still in General gets the date format typing gives, in one restyle per run down a column (`Doc::typed_date`).
- **Headers scale with the zoom**, kept to half to twice their size (`grid_view::header_scale`, `header_height`, `GridView::header_width_at`). Sheets places the fill options button and Zoom to Selection with the scaled headers.
- Rows growing for larger text (the §16.84 "Not yet") was already built: Larger and Smaller Text refit rows nobody sized, in the same step.
- Tests: `font_mono_draws_in_the_monospace_face_and_files_keep_the_name`, `a_filter_column_takes_a_condition` (Date is) and `text_to_columns_splits_at_the_delimiter_the_cells_use` (dates) in Sheets; pergola `headers_scale_with_the_zoom_within_half_to_twice`. All 158 Sheets (with `--test-threads=2`, see §16.129) and 246 pergola lib tests pass.
- Not yet: font families beyond sans and mono (the shell ships those two); a CSV's ISO dates read as text, so date filters need dates typed or from `.xlsx`/`.ods`.

### 16.131 Gridlines and headings, cell styles, colour filters, ISO dates from CSV (2026-11-25)

- **Gridlines** and **Headings** in the Data menu, checked while shown: each sheet's own, as Excel's View tab. They are view settings, not undo steps, but they mark the document changed because files keep them: `Sheet::show_grid` and `show_headings` (both true as made); the native chunk `VIEW_HIDDEN` (30, a flags byte per sheet that hides either); `.xlsx` `<sheetView showGridLines="0" showRowColHeaders="0">`; `.ods` settings.xml `ShowGrid` and `HasColumnRowHeaders` per sheet, and for the whole view when every sheet has them off (LibreOffice's own view-wide `false` turns them off on every sheet). LibreOffice keeps our per-sheet gridlines both ways (`libreoffice_keeps_our_frozen_panes`). pergola's `GridSource::show_grid` and `show_headings`; without headings the cells start at the view's top left (`GridView::headings_size`, also used to place the fill options button and Zoom to Selection).
- **Cell styles** in the Format menu: Excel's built-in Normal, Good, Bad, Neutral, Title, Heading 1 to 4, Total, Input, Output, Calculation, Check Cell, Note and Warning Text, with the Office theme's colours (`format::CELL_STYLES`). A style sets the font (colour, bold, italic, size), the fill and the borders, in one undo step; the cells keep their number format, alignment, wrapping, indent, rotation and protection. Normal takes those off; Excel's Normal also sets General, which we leave so dates stay dates. The menu marks the style the first cell has. Files keep the look, not the style's name.
- **Fill colour is** and **Text colour is** in the filter panel's conditions, as Excel's Filter by Color: a choice of the colours shown in the column (at most 16, from its first 100,000 rows; patterned fills match no colour), including No fill or Automatic. Conditional formats' fills and colour scales count, as Excel's do. It makes the engine's `Cond::Appearance` colour filter, already read from and written to `.xlsx`; the shown colour is `filter::shown_color`, which the filter's own test uses too.
- **CSV: ISO dates and timestamps** (`2026-11-25`, `2026-11-25T09:30:00Z`), quoted or not, open as dates in the ISO date format, as Excel opens them, with one restyle per run down a column; text that isn't a day (`2026-02-31`) stays text. Writing a CSV writes a number in a date or time format as the format shows it, so the dates read back.
- **Remove Duplicates by…** refuses a name that could mean two columns before any cell changes: a header two columns share, or one column's header that is another's letter (column B headed "A": type B, or column A's header). Before, the header won silently.
- Tests: `gridlines_and_headings_turn_off_per_sheet_and_files_keep_them`, `cell_styles_set_font_fill_and_borders_and_keep_the_number_format`, `a_filter_column_keeps_rows_by_fill_or_text_colour` and `remove_duplicates_keeps_the_first_of_each_row_and_undoes` (ambiguous names) in Sheets; pergola `gridlines_and_headings_can_be_turned_off`; officina-grid `iso_dates_read_as_dates_and_write_back_as_written`; officina-convert `gridlines_and_headings_off_round_trip_through_xlsx_ods_and_the_native_file`. All 161 Sheets (`--test-threads=2`), 247 pergola and 124 officina-grid lib tests pass.
- Not yet: headings off in LibreOffice is view-wide, so a file with some sheets' headings off shows them all there; cell style names (`cellStyleXfs`) aren't kept, only their look.

### 16.132 Forty-four more functions (2026-11-25)

Sheets had 128 functions; Excel's everyday ones it lacked now work, in `eval/more.rs`, appended after the first batch so function ids stay put:

- **Rounding to a multiple and integers:** CEILING, FLOOR, CEILING.MATH, FLOOR.MATH, MROUND, QUOTIENT, EVEN, ODD, FACT, COMBIN, GCD, LCM, SUMSQ. Quotients within 1e-12 of a whole number count as whole, so CEILING(1.5, 0.1) is 1.5 as in Excel. GCD and LCM take ranges, refuse negatives and numbers past 2^53, and LCM refuses a result past 2^64 (#NUM!).
- **Trigonometry:** SIN, COS, TAN (refusing angles of 2^27 or more, as Excel), ASIN, ACOS, ATAN, ATAN2 (x first, as Excel), RADIANS, DEGREES.
- **Times and dates:** TIME (wrapping past a day, #NUM! below zero), HOUR, MINUTE and SECOND (to the nearest second), EDATE (the month's last day when the day is past it), DAYS, DATEDIF (Y, M, D, YM, YD and MD, whose odd results for a day past the month's end are Excel's), NETWORKDAYS and WORKDAY (Monday to Friday, with up to 100,000 holidays; whole weeks at once when there are none), WEEKNUM (return types 1, 2, 11 to 17 and 21) and ISOWEEKNUM. All use the workbook's date system.
- **Text:** PROPER (a letter after a non-letter is capital, so 76BudGet is 76Budget), CHAR and CODE (Windows-1252, as Excel on Windows; CODE gives 63 for a character it lacks), UNICHAR, UNICODE, CLEAN.
- **Logic and information:** SWITCH (with a default; #N/A without a match), ISEVEN, ISODD (#VALUE! for a logical), N, T.
- One-value arguments lift over arrays as the first batch's do (`EVEN(A1:A3)` is three values). NETWORKDAYS' and WORKDAY's holidays are a set, and SWITCH lifts only its expression.
- Files: `.xlsx` writes UNICHAR and UNICODE with `_xlfn.`, as it already did CEILING.MATH, FLOOR.MATH, DAYS, ISOWEEKNUM and SWITCH. `.ods` writes CEILING, FLOOR and their .MATH forms in Microsoft's namespace, as LibreOffice does, since OpenFormula's CEILING and FLOOR differ for negative numbers.
- Sheets shows argument tips for all of them (`fnhelp::ARGS`), and AutoComplete offers them.
- Tests: 106 new conformance cases with the results on Microsoft's function pages (`tests/conformance.rs`, 326 in all, each printed and parsed back); the `.xlsx` and `.ods` name tests; Sheets' `every_function_has_its_arguments_and_they_match_its_counts`. `tests/budget.rs`' `sliced_recalc_stops_in_time_and_ends_where_one_call_does` fails here the same way before this change (a 64 ms slice in a debug build on a busy cloud box).
- Not yet: MAXIFS and MINIFS, XMATCH, TEXTBEFORE and TEXTSPLIT, OFFSET and INDIRECT, LOOKUP, ADDRESS, RANK, PERCENTILE, QUARTILE, MODE, the financial functions (PMT, FV, PV, NPV, IRR, RATE, NPER), YEARFRAC and the rest of the long tail.

### 16.133 MAXIFS, ranks and percentiles, XMATCH, and the loan and cash-flow functions (2026-11-25)

Twenty-three more functions, after §16.132's so function ids stay put:

- **MAXIFS and MINIFS** (0 when nothing matches; an error among the matching values is the result). They share SUMIFS' criteria and its equal-key index, which now keeps each key's largest and smallest number too (`ifcache::Agg::max`, `min`); the sorted index keeps only running totals, so they don't use it.
- **RANK, RANK.EQ and RANK.AVG** (Excel takes only a reference for the numbers; #N/A when the number isn't among them), **PERCENTILE, PERCENTILE.INC and PERCENTILE.EXC**, **QUARTILE, QUARTILE.INC and QUARTILE.EXC** (interpolated as Excel does; .EXC refuses what the data doesn't reach), **MODE and MODE.SNGL** (the first of equally frequent numbers; #N/A when none repeats).
- **XMATCH**, sharing XLOOKUP's search (`funcs::xfind`): match modes 0, -1, 1 and 2, search modes 1 and -1, and the binary modes 2 and -2 searched the same way, which gives Excel's result on sorted data. A search mode outside those is #VALUE!, for XLOOKUP too (it was taken as 1).
- **PMT, IPMT, PPMT, FV, PV, NPER, RATE, NPV and IRR** (`eval/fin.rs`) on Excel's sign convention, payments at the end of a period or, with type 1, its start. RATE and IRR use Newton's method from the guess (0.1 by default): at most 100 steps, #NUM! when it doesn't settle or a rate reaches -100 %. IRR needs a positive and a negative flow; NPV and IRR take at most 2^20 flows.
- Argument tips for all of them. `.xlsx` writes the dotted statistics names (RANK.EQ, PERCENTILE.INC, …, MODE.SNGL) with `_xlfn.`; `.ods` writes them and MAXIFS and MINIFS in Microsoft's namespace, as LibreOffice does.
- Tests: 49 conformance cases with the results on Microsoft's function pages (money rounded to the cents the pages show), and `maxifs_and_minifs_agree_with_and_without_the_index`.
- Not yet: TEXTBEFORE and TEXTSPLIT, OFFSET and INDIRECT, LOOKUP, ADDRESS, YEARFRAC, the depreciation functions, MODE.MULT and the rest of the long tail.

### 16.134 TEXTBEFORE and TEXTSPLIT, FIXED, LOOKUP, ADDRESS, YEARFRAC and depreciation (2026-11-25)

Eleven more functions, after §16.133's so function ids stay put:

- **TEXTBEFORE**, the mirror of TEXTAFTER: both now go through one search (`textafter::around`) that keeps the chosen delimiter's start and end, so instance numbers from either end, case-insensitive matching (sliced back on the original text), match_end and if_not_found behave the same in both. It lifts over every argument but the delimiter set.
- **TEXTSPLIT(text, col_delimiter, [row_delimiter], [ignore_empty], [match_mode], [pad_with])** returns an array: the text cut into rows, then each row into columns, where the longest delimiter that matches at a place wins. Short rows are padded with #N/A unless pad_with says otherwise; an empty delimiter, or neither delimiter, is #VALUE!; nothing left after ignoring empties is #CALC!. Delimiter sets hold at most 256 entries and the search shares TEXTAFTER's work budget (#LIMIT! past it); the result is bounded by the array limit before it is built.
- **FIXED(number, [decimals], [no_commas])**: rounded the way ROUND rounds (negative decimals round left of the point), with thousands commas unless asked not to; decimals over 127 is #VALUE!.
- **LOOKUP**, vector form and array form (searching the first row of a wider-than-tall array, else the first column, and answering from the last row or column). It takes the last place whose value of the same type is at most the one sought, which is where Excel's binary search lands on sorted data and makes `LOOKUP(2,1/(A:A<>""),A:A)` find the last filled cell. A one-cell or short result range reads on past its end as Excel's does.
- **ADDRESS** in A1 and R1C1 style with the four absolute/relative kinds and an optional sheet name, quoted exactly as formulas quote sheet names (`print::push_sheet_name`, now shared); rows and columns beyond the sheet's limits are #VALUE!. **ISREF** is TRUE only for a reference.
- **YEARFRAC** on all five bases: US 30/360 with its February month-end rules, actual/actual (a year of 366 days when a 29 February falls in a span of up to a year, else the average length of the years spanned), actual/360, actual/365 and European 30/360. Dates out of order are swapped; another basis is #NUM!.
- **SLN, SYD, DDB and DB**. DDB never takes the book value below salvage; DB rounds its rate to three places and splits the first year by `month`, with the remainder in period life + 1. Both take closed forms, so a large life costs nothing.
- Argument tips for all of them. `.xlsx` writes TEXTBEFORE and TEXTSPLIT with `_xlfn.`.
- Tests: 85 conformance cases with the results on Microsoft's function pages.
- Not yet: OFFSET and INDIRECT (§16.135), MODE.MULT and the rest of the long tail.

### 16.135 INDIRECT and OFFSET, regression and spread statistics, and number bases (2026-11-26)

Fifty-three more functions, after §16.134's so function ids stay put:

- **INDIRECT(ref_text, [a1])** reads A1 text through the formula parser (sheet names, ranges, defined names and tables) and accepts only a reference; R1C1 text (`R2C3`, `R[-1]C`, ranges, a sheet prefix) is turned into A1 first. Anything else is #REF!. **OFFSET(reference, rows, cols, [height], [width])** moves and resizes a reference; leaving the sheet or a size under one cell is #REF!. Both give references, so `SUM(OFFSET(…))` and `ISREF(INDIRECT(…))` work.
- **Recalculation.** Both are volatile, as in Excel. A group that calls either is marked `Group::dynamic`, and each pass adds an ordering edge from every dirty group that isn't downstream of a dynamic group to every dynamic one (`Workbook::order_dynamic`): no such edge can close a cycle, and the dynamic groups run after any cell they could read in that pass, with their own readers after them. What's left: an INDIRECT or OFFSET whose target is itself computed from another INDIRECT or OFFSET, or from its own group, may see that cell's value from before this recalculation. `indirect_and_offset_see_this_recalculation` checks the order over a 20-group chain, and fails without the edges.
- **Statistics**: AVEDEV, DEVSQ, GEOMEAN and HARMEAN (positive numbers only), STANDARDIZE; over pairs of lists (pairs where either side isn't a number are dropped, lists of unequal length are #N/A) CORREL and PEARSON, RSQ, SLOPE, INTERCEPT, FORECAST and FORECAST.LINEAR, STEYX, COVAR, COVARIANCE.P and COVARIANCE.S; MODE.MULT (a column of every most frequent number in first-seen order); MAXA, MINA and AVERAGEA, which count text in ranges as 0 and logicals as 0 or 1. SINH, COSH, TANH and their inverses.
- **Text and numbers**: REPLACE (by characters), DOLLAR (FIXED with `$`, negatives in parentheses), NUMBERVALUE (any decimal and group separators, spaces ignored, trailing % divides by 100), ROMAN with all five forms (each allows longer subtractions, so 499 is CDXCIX, LDVLIV, XDIX, VDIV or ID) and ARABIC, BASE and DECIMAL (radix 2 to 36, below 2^53), and the twelve BIN/OCT/DEC/HEX conversions with Excel's ten-digit two's complement for negatives and `places` padding.
- **Information**: TYPE (64 for an array or a range of more than one cell), ERROR.TYPE (Excel's numbers; #N/A for Officina's own #CIRC! and #LIMIT!), SHEET, SHEETS, ISFORMULA (which, like FORMULATEXT, is recalculated when the cell's formula changes) and ISNONTEXT.
- Argument tips for all of them. `.xlsx` writes the 2010-and-later names (FORECAST.LINEAR, COVARIANCE.P/.S, MODE.MULT, NUMBERVALUE, ARABIC, BASE, DECIMAL, SHEET, SHEETS, ISFORMULA) with `_xlfn.`; `.ods` writes the dotted statistics ones in Microsoft's namespace.
- Tests: 126 conformance cases with the results on Microsoft's function pages, and the recalculation order test above.
- Not yet: the rest of the long tail (statistical distributions and LINEST came in §16.136; bond and security functions, CELL and INFO).

### 16.136 Statistical distributions, tests and regression (2026-11-26)

Seventy-nine more functions, after §16.135's so function ids stay put:

- **Special functions** (`eval/special.rs`, written from the textbook definitions): ln Γ by Lanczos' approximation (exact factorials for whole numbers), the regularized incomplete gamma P and Q (power series below a + 1, Lentz's continued fraction above, each computed directly so tails keep their digits), the regularized incomplete beta (continued fraction with the symmetry swap), erf and erfc through P(½, x²), and inversion of a monotone function by bisection with a growing bracket (at most 200 doublings and 300 halvings). Every inverse distribution uses that bisection, so it lands within a few units in the last place.
- **Distributions**, new and legacy names: NORM.DIST/NORMDIST, NORM.INV/NORMINV, NORM.S.DIST/NORMSDIST, NORM.S.INV/NORMSINV, PHI, GAUSS, LOGNORM.DIST/LOGNORMDIST, LOGNORM.INV/LOGINV, T.DIST, T.DIST.2T, T.DIST.RT, TDIST, T.INV, T.INV.2T/TINV, CHISQ.DIST, CHISQ.DIST.RT/CHIDIST, CHISQ.INV, CHISQ.INV.RT/CHIINV, F.DIST, F.DIST.RT/FDIST, F.INV, F.INV.RT/FINV, BINOM.DIST/BINOMDIST, BINOM.DIST.RANGE, BINOM.INV/CRITBINOM, NEGBINOM.DIST/NEGBINOMDIST, POISSON.DIST/POISSON, HYPGEOM.DIST/HYPGEOMDIST, EXPON.DIST/EXPONDIST, GAMMA.DIST/GAMMADIST, GAMMA.INV/GAMMAINV, GAMMA, GAMMALN/GAMMALN.PRECISE, BETA.DIST/BETADIST, BETA.INV/BETAINV (with the optional [A, B] bounds), WEIBULL.DIST/WEIBULL, ERF (one or two bounds), ERF.PRECISE, ERFC/ERFC.PRECISE, FISHER and FISHERINV, CONFIDENCE.NORM/CONFIDENCE and CONFIDENCE.T. Degrees of freedom and counts are truncated, and out-of-range arguments are #NUM! as Excel documents. A cumulative hypergeometric sum takes at most 10^6 terms (#LIMIT! past it).
- **Tests**: T.TEST/TTEST (paired, equal variances, Welch), Z.TEST/ZTEST, CHISQ.TEST/CHITEST (a table's degrees of freedom are (r−1)(c−1), a single row or column's n−1), F.TEST/FTEST.
- **Regression** (`eval/linest.rs`): LINEST and LOGEST with one to 64 predictor variables, with or without a constant, and with Excel's five-row statistics block (standard errors, r² and the standard error of y, F and the degrees of freedom, the two sums of squares, #N/A filling the rest); TREND and GROWTH at new xs, or at the known ones in the known ys' shape. With a constant the data are centred before the normal equations are solved (Gauss–Jordan with partial pivoting), so year-valued xs keep their digits. Collinear predictors are #NUM!, where Excel drops one of them and reports a zero coefficient.
- Argument tips for all of them. `.xlsx` writes the 2010-and-later names with `_xlfn.`; `.ods` writes the dotted ones in Microsoft's namespace and CHIDIST, CHIINV, CHITEST, FDIST, FINV, NORMSDIST, NORMSINV and TDIST as OpenFormula's `LEGACY.` names, and reads both back.
- Tests: 139 conformance cases with the results on Microsoft's function pages (every one also checked against SciPy and NumPy outside the build), unit tests of the special functions, and an `.ods` round trip of the legacy names.
- Not yet: bond and security functions, CELL and INFO, and dropping collinear predictors in LINEST (all in §16.137).

### 16.137 Bonds, more cash-flow functions, CELL and INFO (2026-11-26)

Thirty-eight more functions, after §16.136's so function ids stay put, and one fix:

- **Coupon schedule** (`eval/bonds.rs`): coupon dates step back from maturity by 12 / frequency months, keeping its day of the month or, when maturity is a month's end, every month's end. COUPPCD, COUPNCD and COUPNUM come from that walk (bounded: a guess from the month difference, then at most eight steps); COUPDAYS is the period's actual length on basis 1, 365 / frequency on basis 3 and 360 / frequency otherwise; COUPDAYBS counts from the previous coupon as the basis counts (30/360 US or European, else actual days); COUPDAYSNC is the period less that on the 30/360 bases, else actual days.
- **PRICE, YIELD, DURATION and MDURATION** on Excel's documented formula (a single remaining coupon has its own closed form). YIELD inverts PRICE by bisection. At most 32,000 coupons. **ACCRINT** sums each quasi-coupon period's share of the schedule through first_interest, from issue or (calc_method FALSE, settled after the first coupon) from first_interest; **ACCRINTM**, **DISC**, **PRICEDISC**, **YIELDDISC**, **RECEIVED**, **INTRATE**, **PRICEMAT** and **YIELDMAT** take their year fractions from YEARFRAC's day counts (now shared as `more::year_frac` and `days_360`); **TBILLPRICE**, **TBILLYIELD** and **TBILLEQ** (the semiannual form past 182 days) refuse a maturity more than a year after settlement.
- **Cash flows and rates** (`eval/fin.rs`): XNPV and XIRR on dated flows (365-day years from the first date, which must be the earliest), MIRR, CUMIPMT and CUMPRINC (summing IPMT's periods, at most 10^6), EFFECT, NOMINAL, FVSCHEDULE, PDURATION, RRI, ISPMT, DOLLARDE and DOLLARFR, and VDB (declining balance between two possibly fractional points, switching to straight line over the remaining life when that is larger unless no_switch; a partial period gets its share of that period).
- **CELL** answers "address", "col", "row", "contents", "type" and "filename" (always empty: a workbook never learns where it is stored); **INFO** (volatile) answers "numfile", "recalc", "release", "system", "osversion" and "directory" (empty). Other info types, such as "width" and "format", are #VALUE!.
- **LINEST and LOGEST drop collinear predictors** as Excel does: elimination runs in column order, and a column whose pivot falls to almost nothing of its own diagonal is dropped with a zero coefficient and standard error, and the degrees of freedom and F count only the kept ones. A constant predictor beside the intercept no longer gives #NUM!.
- Argument tips for all of them; `.xlsx` writes PDURATION and RRI with `_xlfn.`.
- Tests: 81 conformance cases with the results on Microsoft's function pages. Three of the pages' figures are their own rounding and the cases check the true value instead: XIRR's 0.373362535 (the root is 0.37336253352), MIRR's third example, and MDURATION's 5.73567842, where the textbook modified duration for that bond is 5.73566981; MDURATION is checked as DURATION / (1 + yld / frequency) on the DURATION page's bond, which matches its page to the last digit shown.
- Not yet: the ODD-period bond functions (ODDFPRICE, ODDFYIELD, ODDLPRICE, ODDLYIELD), AMORDEGRC and AMORLINC, and CELL's formatting info types.

### 16.138 Matrices, complex numbers, AGGREGATE and the .INTL working days (2026-11-26)

Seventy-six more functions, after §16.137's so function ids stay put:

- **Matrices** (`eval/matrix.rs`): MMULT, MINVERSE and MDETERM by Gauss–Jordan elimination with partial pivoting (a pivot under 10^-15 of the largest entry makes the matrix singular: MINVERSE is #NUM!, MDETERM 0), and MUNIT. A determinant within 10^-9 of a whole number is that number, so integer matrices give integer determinants. Numbers only (anything else is #VALUE!); at most 2^30 multiply-adds a call, and the result within the array limit.
- **Complex numbers** (`eval/complex.rs`): COMPLEX and the 25 IM functions (IMREAL, IMAGINARY, IMABS, IMARGUMENT, IMCONJUGATE, IMSUM, IMPRODUCT, IMSUB, IMDIV, IMPOWER, IMSQRT, IMEXP, IMLN, IMLOG10, IMLOG2, IMSIN, IMCOS, IMTAN, IMCOT, IMSEC, IMCSC, IMSINH, IMCOSH, IMSECH, IMCSCH). Text is read as `a+bi` or `a+bj` (a number or blank is real; a logical is #VALUE!; anything else #NUM!); results are written with 15 significant digits, E notation outside 10^-9 to 10^15, a unit imaginary part as plain `i`, and the arguments' suffix (mixing `i` and `j` is #VALUE!).
- **Engineering and maths** (`eval/mathx.rs`): BITAND, BITOR, BITXOR, BITLSHIFT and BITRSHIFT on whole numbers 0 to 2^48 − 1 (shifts up to 53 places); DELTA, GESTEP; PERMUT, PERMUTATIONA, COMBINA, FACTDOUBLE, MULTINOMIAL (a product of binomials, exact while it fits), SERIESSUM, SQRTPI; SUMX2MY2, SUMX2PY2 and SUMXMY2 (arrays the same size or #N/A, skipping pairs that aren't both numbers); COT, COTH, CSC, CSCH, SEC, SECH (angles under 2^27), ACOT, ACOTH; CEILING.PRECISE, ISO.CEILING and FLOOR.PRECISE (toward +∞ or −∞ whatever the signs).
- **Statistics** (`eval/spread.rs`): KURT, SKEW and SKEW.P; PERCENTRANK, PERCENTRANK.INC and .EXC (interpolated between neighbours, cut to `significance` digits, #N/A outside the data); TRIMMEAN (the count trimmed rounded down to even); FREQUENCY (a column with one more row than the bins, which may be in any order); PROB (probabilities in (0, 1] summing to 1); STDEVA, STDEVPA, VARA and VARPA on MAXA's reading of text and logicals.
- **AGGREGATE**: functions 1–13 over references and 14–19 (LARGE, SMALL, PERCENTILE.INC, QUARTILE.INC, PERCENTILE.EXC, QUARTILE.EXC) over an array with k. Options 0–3 skip nested SUBTOTAL and AGGREGATE cells, 1/3/5/7 skip hidden and filtered rows, 2/3/6/7 skip errors, so `AGGREGATE(14,6,A1:A5/(A1:A5<3),1)` works. AGGREGATE groups are marked like SUBTOTAL's, so hiding rows recalculates them and an outer SUBTOTAL skips them.
- **Dates**: DAYS360 (Excel's US rule: a start on a month's last day is the 30th, an end on the 31st is the 30th when the start is; or European), WORKDAY.INTL and NETWORKDAYS.INTL (weekend codes 1–7 and 11–17 or a seven-character Monday-first mask; a week with no working day is #VALUE!; WORKDAY and NETWORKDAYS now share that code), TIMEVALUE (`h:mm[:ss]` with optional AM/PM, after an optional date that must parse).
- Argument tips for all of them; lifting over arrays for the scalar ones; MMULT, MINVERSE, MUNIT and FREQUENCY are array results. `.xlsx` writes the 2010+ names with `_xlfn.` and `.ods` the dotted ones and AGGREGATE as `COM.MICROSOFT.`.
- Tests: 144 conformance cases with the results on Microsoft's function pages (IMPOWER is checked through IMREAL, since the page's `9.00000000000001i` is its own rounding noise), and a recalc test that AGGREGATE's options follow hidden rows and nested AGGREGATE cells (it fails without the subtotal marking).
- Not yet: AGGREGATE's handling of rows hidden by a filter versus by hand (both count as hidden), complex numbers in polar text, and TIMEVALUE's localized AM/PM words.

### 16.139 Database functions, regular expressions, CONVERT and Bessel functions (2026-11-26)

Thirty-eight more functions, after §16.138's so function ids stay put:

- **Database functions** (`eval/database.rs`): DSUM, DAVERAGE, DCOUNT, DCOUNTA, DGET, DMAX, DMIN, DPRODUCT, DSTDEV, DSTDEVP, DVAR and DVARP. The field is a header (any case) or a 1-based column; DCOUNT and DCOUNTA count records when it is left out. Each criteria row's conditions must all hold and any row may; a blank criterion is no condition, and plain text matches text that begins with it (`Apple` takes `Apple pie`; `=Apple` is exact), as Excel's database criteria do. DGET is #VALUE! with no match and #NUM! with several. Excel's computed criteria (a formula under a label that isn't a field) are #VALUE! here.
- **Regular expressions** (`eval/textmore.rs`, the `regex` crate already used by Find): REGEXTEST, REGEXEXTRACT (0 the first match, 1 every match as a column, 2 the first match's groups as a row; #N/A without a match) and REGEXREPLACE (every match, the nth, or the nth from the end; `$1` and `${name}` in the replacement). case_sensitivity 1 ignores case. Patterns the engine doesn't take (look-around, back-references) or over 2 MiB compiled are #VALUE!; 64 compiled patterns are kept per thread.
- **Text**: ARRAYTOTEXT (concise `1, 2, 3` or strict `{1,"a";TRUE,4}`, at most 32,767 characters), VALUETOTEXT, ENCODEURL (UTF-8 percent-encoding of all but letters, digits and `-_.~`), ASC and DBCS/JIS (full-width ASCII and the ideographic space; half-width katakana is not mapped), and the B forms FINDB, SEARCHB, LEFTB, RIGHTB, MIDB, LENB and REPLACEB, which count characters as Excel does when the default language isn't a double-byte one.
- **CONVERT** (`eval/convert.rs`): Excel's units of mass, length, time, pressure, force, energy, power, magnetism, temperature, volume, area, information and speed, with metric prefixes on the units Excel allows them on (raised to the unit's power for m2, m3 and the like) and binary prefixes (ki … Yi) on bit and byte. Factors come from the units' definitions; an unknown unit or two kinds is #N/A.
- **BESSELI, BESSELJ, BESSELK and BESSELY** (`eval/special.rs`): I by its power series; J by the series for small x and otherwise its integral form by the trapezoid rule (geometric on that periodic integrand); K and Y from their integral forms by composite 16-point Gauss–Legendre. Microsoft's pages give BESSELJ(1.9, 2) as 0.329925829 and BESSELK(1.5, 1) as 0.277387804, Excel's polynomial approximations; the true values (0.3299257277, 0.2773878005, checked against SciPy) are what we return.
- **AMORLINC** and **AMORDEGRC** (`eval/fin.rs`): the French depreciation functions on YEARFRAC's bases 0, 1, 3 and 4. AMORDEGRC's coefficient is 1, 1.5, 2 or 2.5 for lives under 3, under 5, to 6 and over 6 years, every amount is rounded to a whole unit, and the period that would take the value below salvage and the one after each take half of what is left.
- **AREAS** (1 for any reference, since references here have one area), **PERCENTOF** and **TRIMRANGE** (a reference without its blank outer rows and columns; #REF! when it is all blank).
- Argument tips for all of them; lifting over arrays for the scalar ones; REGEXEXTRACT is an array result. `.xlsx` writes the new names with `_xlfn.`, `.ods` with `COM.MICROSOFT.`.
- Tests: 80 conformance cases (Microsoft's examples for the database functions, CONVERT, AMORLINC, AMORDEGRC and the Bessel functions), a unit test that every CONVERT name is found once and to its own unit, and Bessel checks against SciPy including large orders and small arguments.
- Not yet: the ODD-period bond functions, FORECAST.ETS, GROUPBY and PIVOTBY, and the cube, web and RTD functions.

### 16.140 Odd-period bonds, GROUPBY and PIVOTBY, and function names as values (2026-11-26)

Nine more functions, after §16.139's so function ids stay put, and one language feature:

- **Function names as values** (`eval/arrays.rs`): where a LAMBDA is expected, a built-in function's bare name (`BYROW(A1:B9, SUM)`, `GROUPBY(…, PERCENTOF)`) is that function as a lambda with as many parameters as it requires. Only a name that isn't a LET or defined name is taken so. `.xlsx` reads Excel's `_xleta.` prefix and writes it for a function name given as an argument of MAP, REDUCE, SCAN, BYROW, BYCOL, MAKEARRAY, GROUPBY or PIVOTBY.
- **ODDFPRICE, ODDFYIELD, ODDLPRICE and ODDLYIELD** (`eval/bonds.rs`) on Microsoft's formulas for odd first and last periods, short or long. An odd period is cut into quasi-coupon periods on the schedule through the first coupon (or from the last interest date), each counting its days (DC), days accrued before settlement (A) and days from settlement (DSC) as fractions of its normal length on the chosen basis. ODDFYIELD inverts ODDFPRICE by bisection; ODDLYIELD has a closed form. At most 1,000 quasi periods and 32,000 coupons.
- **GROUPBY** (`eval/groupby.rs`): the function is applied to each group's values column (a two-parameter function such as PERCENTOF also gets all the values), per value column. Keys are compared as SORT compares them (text without case). field_headers 0–3, or by default headers when the first value row is all text and the second has a number; total_depth 0, ±1 (grand total after or before), ±2 and deeper (subtotals by the leading fields); sort_order one column index, negative for descending, by key or by a value column's result, kept within the hierarchy; filter_array. field_relationship is checked (0 or 1) but both give the hierarchy.
- **PIVOTBY**: row and column fields with the same totals and key sorting on each axis; an empty intersection is blank; several value columns sit side by side under each column key. A two-argument function (PERCENTOF, or a LAMBDA) gets as its second argument the values relative_to names: 0 the column total (default), 1 the row total, 2 the grand total, 3 the parent column total (the same column within the row's parent group), 4 the parent row total (§16.144).
- **BAHTTEXT** (Thai words, ล้าน for millions, เอ็ด for a trailing one, สตางค์ or ถ้วน), **PHONETIC** (cells carry no readings here, so the cells' text), and **EUROCONVERT** (the 19 legacy euro-area currencies at their fixed rates, rounded to the target's decimals unless full_precision, triangulating through the euro with triangulation_precision ≥ 3).
- Argument tips; lifting for the bond functions, BAHTTEXT and EUROCONVERT; GROUPBY and PIVOTBY are array results. `.xlsx` writes GROUPBY, PIVOTBY and BAHTTEXT with `_xlfn.`.
- Tests: 50 conformance cases, including Microsoft's examples for the four odd-period functions (all match to eight places), and `.xlsx` read and write of `_xleta.`.
- Not built, on purpose: FORECAST.ETS and its kin (Excel's AAA exponential smoothing chooses its parameters by an undocumented optimisation, so results would not match), WEBSERVICE, FILTERXML, STOCKHISTORY and IMAGE (they fetch or parse outside content in the formula engine, which §16.74 rules out), the cube and RTD functions, CALL and REGISTER.ID, and GETPIVOTDATA (until pivot tables are addressable from formulas).

### 16.141 Iterative calculation, Subtotal, Advanced Filter, Consolidate and Data Table (2026-11-26)

With the function list essentially complete, the Data menu's remaining what-if and list tools. Each opens the typed bar under the grid (as Remove Duplicates by… does) and runs as one undo step; `atrium-sheets/src/whatif.rs`.

- **Iterative Calculation…** (`100, 0.001`, or `off`): a workbook setting (`book.rs` `Iteration`: 1–32,767 passes, change ≥ 0), `GridOp::Iteration` (tag 28) so it undoes, kept in the native file (settings flag bit 2 with the count and change), `.xlsx` `calcPr iterate/iterateCount/iterateDelta` and `.ods` `table:iteration` both ways. When it is on, each circular component that recalculation finds (`recalc.rs` Tarjan pass) is evaluated in row order up to the count, stopping once no number moved more than the change and nothing else changed; off, the cells show #CIRC! as before. A self-counter such as `=A1+1` advances by the count each recalculation, as in Excel. Sheet protection doesn't stop it (it is a calculation setting).
- **Subtotal…** (`Region; Sum; Sales, Qty`, or `remove`): the data needs a header row and isn't a table. At each change in the key column a row is inserted holding "East Total" and `=SUBTOTAL(9,C2:C3)` per listed column, then a Grand Total row over everything (SUBTOTAL skips the inner subtotals); an outline groups each block inside one group of all but the grand total. Functions Sum, Count, Average, Max, Min, Product, Count Numbers, StdDev(p), Var(p). The columns default to those holding numbers. At most 1,000 groups and 100,000 rows. `remove` deletes the rows holding SUBTOTAL formulas and the outline groups in the data.
- **Advanced Filter…** (`G1:H2[, J1][, unique]`): criteria as the D functions read them (`eval/database.rs` `picks`, now shared): headings naming fields, each row below one alternative whose conditions all hold, plain text matching as a prefix. In place, the data's other rows are hidden (rows hidden outside the data stay); with a cell to copy to, the header and the matching records are written there as values. The criteria and the copy must be outside the data; criteria may be on another sheet (`Sheet2!A1:B3`). At most 1,000,000 data cells and 4,096 criteria cells.
- **Consolidate…** (`Sum; A1:B5, Sheet2!A1:B5; top, left`): up to 64 ranges, combined cell by cell by position, or by the labels in their top row and/or left column (matched without regard to case, in the order first met). The result is written as values at the cursor, which must not overlap a source. Not built: Excel's links to the sources.
- **Data Table…** (`B1, B2`; `, B2` for a column input only; `B1,` for a row input only) on the selected table: with one input, the values to try run along the first column (or row) and the formulas sit beside the corner; with two, the corner holds the formula. Each cell is filled by setting the input cell(s), recalculating and reading the formula, as Goal Seek does; the inputs are put back. Excel keeps a live `{=TABLE()}`; here they are values, so run it again after a change. At most 10,000 cells and 10 s.
- Tests: `officina-grid/tests/iteration.rs` (cycle converges, self-counter, undo, native round trip, invalid settings), `officina-convert/tests/iteration.rs` (.xlsx and .ods round trip), six `whatif` tests (each tool, its refusals, undo) and an app test driving Subtotal and Iterative Calculation through the bar.

### 16.142 Scenario Manager, Series and Justify, Error Checking, Trace Error and Evaluate Formula (2026-11-26)

More of Excel's What-If and Formulas-tab tools, each from the typed bar of §16.141.

- **Scenario Manager…** (`atrium-sheets/src/scenarios.rs`): `add Best: B1=5, B2=7` (or `add Now: B1:B2` to keep what the cells hold; the same name again edits it), `show Best` (types the values in, one undo step), `delete Best`, and `summary B3, B4`: a new sheet "Scenario Summary" listing the changing cells and the result cells, with the current values and each scenario's (the results are recalculated with each scenario's values in place and the sheet is put back). Scenarios are kept with the sheet (`officina-grid/src/scenario.rs`: at most 256 a sheet, 32 cells each as Excel allows, names distinct without case; `GridOp::Scenarios` tag 29; native chunk 31; `.xlsx` `<scenarios><scenario><inputCells r val>` both ways, a malformed list dropped with a note). Changing cells move when rows or columns are inserted or deleted; a deleted one leaves its scenario. Changing cells must hold values, not formulas. `.ods` keeps scenarios as scenario sheets, a different model, so they aren't written there.
- **Fill ▸ Series…** (`src/series.rs`): `linear 2`, `growth 3`, `date day|weekday|month|year N`, `linear trend` or `growth trend` (least squares over the numbers already at the start of each line), with `stop 100`, and `rows` or `columns` (by default down when the selection is at least as tall as wide). Each column (or row) starts from its first cell; a month step keeps the day, or the month's last when shorter (31 Jan → 29 Feb → 31 Mar); weekdays skip Saturday and Sunday. At most 1,000,000 cells. The older Fill: Series (the last fill done again as a series) stays.
- **Fill ▸ Justify**: the text down the selection's first column joined and broken again into lines that fit the selection's columns (about 7 px a character of the default font), written down from the top and the rest of the selection emptied; refused when a cell holds a number or formula or the lines need more rows than selected.
- **Error Checking** (`src/audit.rs`): the next cell after the cursor, by rows and wrapping round, that holds an error value or a number stored as text, the cursor moved there and the status saying what the error means. **Trace Error**: from the error at the cursor, back through each precedent area's first error cell (at most 64 steps, 100,000 cells an area) to the source, shown as `A4 ← A3 ← A2` with the source's reason.
- **Evaluate Formula…** (`officina-grid/src/eval/steps.rs`): the bar shows the formula; each Next works out the leftmost innermost part (a reference, operation or call whose inputs are values) and shows the formula again, until the value; then it starts over. A range becomes an array constant up to 64 cells. Parts that can't stand as a value are worked out with their parent: an empty cell (it reads as 0 or "" by use), a bigger range, and the arguments of functions that read references or evaluate only some arguments (IF, CHOOSE, ROW, OFFSET, INDEX, the *IF family, SUBTOTAL, the lookups, the D functions, the lambda helpers…).
- Tests: `officina-grid/tests/steps.rs` (steps for arithmetic, SUM over a range, IF and ROW whole, an empty cell, errors, and five formulas ending on their cells' values), `officina-convert/tests/scenarios.rs` (.xlsx round trip with escaping, a duplicate name refused with a note), `scenarios` (add, show, undo, summary, edit, delete, row insert, native round trip), `series` (linear, growth with stop, across, refusals, month, weekday, year, trend, Justify at one and two column widths) and `audit` unit tests, and an app test stepping Evaluate Formula and running Error Checking and Trace Error.

### 16.143 Sparklines, Protect Workbook, sort by colour and Fill Across Worksheets (2026-11-26)

- **Sparklines…** (`officina-grid/src/sparkline.rs`, bar in `atrium-sheets/src/sparkbar.rs`): select a column (or row) of cells and type `line B2:F9`: each cell gets a sparkline of the matching data row (or column). `column` and `winloss` are the other kinds; `markers` marks the highest and lowest points; `color 00B050` sets the series colour (negative points stay red). Typed with no range over cells that have sparklines, it restyles them and keeps their data; `clear` removes them. Each sparkline reads one row or one column, at most 1,000 points, any sheet; non-numbers are gaps. A sheet keeps at most 10,000, one per cell, as one list (`GridOp::Sparklines` tag 30, native chunk 32). They follow inserted and deleted rows and columns: the cell moves, and so does data on the same sheet (data on other sheets isn't adjusted). Drawn by Pergola's grid (`Spark`, `spark_shapes`: segments with gaps, columns from zero, win/loss blocks above and below the middle), including in empty cells. `.xlsx` reads and writes Excel 2010's `x14:sparklineGroups` extension, a group for each distinct look; a sparkline naming an unknown sheet or a bad range is dropped with a note. Not built: axis options, first/last point markers, and printing them (the printing thread owns `print.rs`).
- **Protect Workbook Structure** (Data menu, a check item): while it is on, adding, deleting, renaming, moving, hiding or showing sheets and tab colours are refused with their own message (`sheet_protection::refusal`); cells still edit. It is `GridOp::Structure` (tag 31), so it undoes, kept in the native file (settings flag bit 3). `.xlsx` reads and writes `<workbookProtection lockStructure="1">` with Excel's verifier kept as is; `.ods` `table:structure-protected` on `office:spreadsheet`. A lock with a password from another app can't be taken off here (its hash isn't checked), so the command says to unlock it there.
- **Sort by colour**: a Custom Sort level can be `Region fill FFC7CE` (cell colour), `Qty font 9C0006` (font colour), `fill none` or `font auto`, with `bottom` to put those cells last; the other levels and the rest of the rows keep their order (`sort::order_levels`, a stable sort; the colour matched is the one shown, conditional formats included, as colour filters match).
- **Fill ▸ Across Worksheets…**: the selection copied to the same place on every other sheet (`all`) or on the sheets named, with `contents` or `formats` for only those, as one undo step. Formulas keep their text, so they read the same cells on each sheet, as Excel's does.
- Tests: `sparkbar` (one per row, restyle, refusals, column insert, native round trip, clear, the grid's view of an empty cell), Pergola's `spark_tests` (segments, gaps, markers, columns inside the cell, win/loss, nothing to draw), `officina-convert/tests/sparklines.rs` (round trip across sheets with a quoted name, grouping, an unknown sheet noted), `tests/structure.rs` (.xlsx with the verifier, .ods), an app test of the structure lock (add, delete, rename and move refused, cells edit, unlock, a password lock kept, native round trip), and unit tests of colour sorting and Fill Across Worksheets.

### 16.144 PIVOTBY relative_to and sparklines that follow their sheet (2026-11-26)

- **PIVOTBY relative_to** now decides what a two-argument aggregator's second argument holds, as Excel documents it, instead of always the grand total. With A/X/10, A/Y/30, B/X/20, B/Y/40, PERCENTOF for A/X is 1/3 by default (column X), 1/4 for row totals and 1/10 for the grand total. Parent totals (3, 4) use the line's parent group on that axis (its key fields but the last); a top-level line's parent is everything. Work over these sets is bounded with the rest of PIVOTBY's. Conformance: LAMBDA(x,t,SUM(x)/SUM(t)) and PERCENTOF over all five modes.
- **Sparklines follow their source sheet**: moving tabs remaps each sparkline's data sheet with the formulas, charts and pivot tables, so a sparkline on Report reading Rates still reads Rates when Costs moves in front, through undo, redo, `.xlsx` save and reopen (tests/sparklines.rs in officina-convert). Deleting a sheet takes away the sparklines that read it (as charts go) and the sheet's own sparklines and scenarios, which delete previously left behind and so refused.

### 16.145 Manual calculation, Watch Window, names from labels, Apply Names, Auto Outline and Workbook Statistics (2026-11-26)

More of Excel's Formulas tab, in the Data menu; `atrium-sheets/src/formulas.rs`. Each change is one undo step.

- **Manual Calculation** (a check item; Excel's Calculation Options): edits stop recalculating the cells that read them. A formula typed or pasted is still worked out itself (`Workbook::recalc_cells` evaluates the formulas in the cells changed since the last recalculation, the last 64 areas, without their readers), so what was entered shows its value while the rest keeps its old one. Calculate Now (F9) and Calculate Sheet (Shift+F9) work everything out at once; nothing runs in the background (`Doc::settle` waits), and `Doc::needs_calculate` says edits wait (Excel's "Calculate" on the status bar). Switching back to automatic catches up at once. `GridOp::Calculation` (tag 32), native settings flag bit 4, `.xlsx` `calcPr calcMode="manual"` both ways (Excel's "automatic except tables" reads as automatic, since data tables here are values). Not built: `.ods` (LibreOffice keeps AutoCalculate in settings.xml, not the document), recalculating before saving, and Shift+F9 limited to one sheet's chain (it works out every pending formula).
- **Watch Window…** and **Add Watch**: a panel at the right listing each watched cell as `Sheet!A1 = value` with its formula, Go and Delete, and Add Watch for the selection (one cell, or the cells with something in them of a larger selection). Watches are kept with their sheet (`officina-grid/src/watch.rs`: at most 1,000 a sheet, none twice; `GridOp::Watches` tag 33; native chunk 33; `.xlsx` `<cellWatches><cellWatch r>`), follow inserted and deleted rows and columns, go with a deleted sheet, and are allowed on a protected sheet (they aren't its contents).
- **Create Names from Selection…** (Ctrl+Shift+F3, a bar: `top, left`, `bottom`, `right`; it starts with Excel's guess from which edges hold text): a workbook name for each column (labels in the top or bottom row) and each row (labels in the left or right column) of the selection, referring to the rest of it with absolute references, the corner left out when both edges are used. Labels become names as Excel makes them: characters a name can't hold turn into `_`, `_` goes first before a digit (`2024 sales` → `_2024_sales`) and last after a cell's spelling (`Q1` → `Q1_`). An existing name gets the new place; empty or repeated labels are skipped and counted. At most 4,096 names a run.
- **Apply Names**: in the selection's formulas (the sheet's when one cell is selected) each reference that is exactly a name's place, relative or absolute alike as Excel's default, becomes the name; the sheet's own names win over the workbook's. At most 100,000 formula cells.
- **Auto Outline** (with the Group commands): over the selection or, for one cell, the sheet, each formula reading a column of cells just above or below it groups those rows, and one reading a row of cells just left or right of it groups those columns; widest first, so a total of totals is the outer group, and a span that would cross another is left out. The sheet's outline on each axis that had totals is replaced.
- **Workbook Statistics…**: a panel with the sheet's end cell and its cells with data, formulas, tables, PivotTables, charts, pictures, comments and sparklines, then the workbook's sheets, cells, formulas, tables, PivotTables, charts and names.
- Fixed: since §16.143 the Sheets app (which links officina-convert without its readers) didn't build, because the `.xlsx` sparkline reader wasn't behind the `read` feature; the tests, which turn it on, hid it.
- Tests: `formulas` unit tests (manual calculation with F9, a typed formula, the native file and back to automatic; watches added, moved by a row insert, saved, deleted and undone; names from top and left labels with Excel's spelling and Apply Names with undo; Auto Outline's nested row groups and a column group; statistics), `officina-convert/tests/watches.rs` (`calcPr` with iteration, `<cellWatches>` round trip, automatic writes nothing).

### 16.146 Allow Edit Ranges, Error Checking's formula rules, Ignore Error, Convert to Number and the Document Inspector (2026-11-26)

Review-tab tools, in the Data menu; `atrium-sheets/src/checks.rs`, with the lists in `officina-grid/src/sheetareas.rs`. Each change is one undo step.

- **Allow Edit Ranges…** (a bar: `add Inputs: B2:B9, D1`, `delete Inputs`, `list`): titled ranges of the sheet whose cells can be changed while it is protected, locked or not (`Workbook::can_edit_cells` counts rows a range covers as open). Changing the ranges is refused while the sheet is protected, as Excel greys the dialog. At most 1,024 a sheet, 256 areas each, titles distinct without case; they follow inserted and deleted rows and columns and go with a deleted sheet. `GridOp::EditRanges` (tag 34), native chunk 34, `.xlsx` `<protectedRanges><protectedRange sqref name>` both ways. A range from Excel with a password keeps its verifier (`password`, or `algorithmName`/`hashValue`/`saltValue`/`spinCount`) and is written back as it was, but stays locked here, since the verifier isn't checked. Not built: setting a range's password, and per-user permissions.
- **Error Checking** (§16.142) now also applies Excel's formula rules, each with its own message: a formula that differs from the ones above and below (or left and right) of it when those agree; a formula whose range leaves out a number just past either end (`=SUM(A1:A3)` with a number in A4); and a formula in an unlocked cell. It looks at up to 200,000 formula cells a sheet (`audit::problem_at`).
- **Ignore Error** (on the cursor's cell): Error Checking passes over it for the rule it broke. **Reset Ignored Errors** forgets every sheet's. The areas and rules are kept with the sheet (at most 4,096; `GridOp::IgnoredErrors` tag 35; native chunk 35; `.xlsx` `<ignoredErrors><ignoredError sqref>` with Excel's nine rule attributes, all kept, though only five are checked here), follow row and column moves, and may change on a protected sheet.
- **Convert to Number**: the selection's numbers stored as text (not formulas) become numbers; at most 1,000,000 cells.
- **Inspect Document…**: a panel listing what a reader may not see, with Remove All for each kind: comments; hidden rows and columns within what each sheet uses (Remove All deletes them and what they hold, as Excel does); hidden worksheets (deleted); defined scenarios; active AutoFilters (taken off, their rows shown); watched cells; and macros (removed as Remove Macros does, which isn't undone). PivotTables are listed, as they keep copies of their source data, but stay. Every other removal is one undo step, however many sheets, rows or columns it touches (at most 10,000 runs of hidden rows and columns).
- Not built: Excel's Remove Hyperlinks was already here (Remove Links). Accessibility checking waits for alt text on pictures and charts.
- Tests: `checks` unit tests (edit ranges opening cells on a protected sheet, refused while protected, moved by a row insert, saved; the inconsistent and omitted-cell rules, Ignore Error passing a cell over, Reset; an unlocked formula; Convert to Number; the inspector finding and deleting a hidden sheet, a hidden row and a hidden column, with undo), and `officina-convert/tests/sheetareas.rs` (`<protectedRanges>` with an Excel verifier and `<ignoredErrors>` round trips).

### 16.147 Trace Precedents and Dependents arrows, Show All Comments, the Data Form and Quick totals (2026-11-26)

Formulas- and Data-tab tools, in the Data menu; `atrium-sheets/src/arrows.rs` and `atrium-sheets/src/dataform.rs`, drawn as grid floats from `source.rs`.

- **Trace Precedents** / **Trace Dependents**: arrows on the grid from each cell (or range, boxed) the cursor's formula reads to the cursor, or from the cursor to each formula reading it. Each use adds one more level, as Excel's does; a level adds nothing new says so. Arrows are blue, red when the source is an error value, and a source on another sheet is drawn as a sheet mark at the cell. At most 500 arrows. **Remove Arrows** clears them; any edit clears them too, since they would go stale. They are not saved, as in Excel.
- **Show All Comments**: every comment and note on the sheet shown at once, not only the one pointed at (at most 200 drawn); a menu tick, kept for the window, not saved.
- **Data Form…** (a bar on the list around the cursor, whose first row names the fields): the bar shows the record at the cursor as `Field = value; …` (formula fields left out, as they can't be typed). Changing values and pressing Apply sets them; `next` and `prev` step through the records; `new Field = value; …` adds a record under the list, carrying the record above's formulas into it, and refuses when the row under the list isn't empty; `delete` removes the record, the cells below moving up within the list; `find Region = East, Qty > 10` goes to the next matching record, wrapping (numbers compare by value, text without case, `=` on text matches its start, as Excel's criteria do). The cursor follows the record. At most 32 fields. Each change is one undo step.
- **Quick Totals…** (Quick Analysis' Totals tab, a bar): `sum`, `average`, `count`, `percent` (each line's share of the whole selection, formatted `0.0%`) or `running` (totals so far across the lines), `below` (a row under the selection, one per column) or `right` (a column to its right, one per row). They are formulas; a cell in the way that isn't empty refuses the lot. At most 16,384 lines. One undo step.
- Not built: alt text on pictures and charts, and with it the Accessibility Checker, since the picture and chart records are shared with printing (`print.rs`), whose thread owns them.
- Tests: `arrows` (one level, then the next, an error source in red, a removed arrow set, edits clearing them), `dataform` (a field edit, finds by text start and by number with wrap, no such field, a new record, delete with undo; the bar's next/prev/delete moving the cursor; sum, percent and running totals below and right, a cell in the way).

### 16.148 Go To Special, Locked and Hidden cells, Show and Hide Detail, Clear Outline, FORECAST.ETS and the Forecast Sheet (2026-11-26)

`atrium-sheets/src/special.rs` and `officina-grid/src/eval/ets.rs`. Each change is one undo step.

- **Go To Special…** (a bar): `blanks`; `constants` or `formulas`, optionally narrowed to `numbers`, `text`, `logicals`, `errors`; `comments`; `validation`; `conditional`; `row differences` and `column differences` (each row or column of the selection against the cursor's column or row, formulas compared by their shape); direct `precedents` and `dependents` on this sheet; `visible` (the selection without hidden rows and columns); `last cell`, `current region`, `current array`. It looks in the selection, or the sheet's used area when one cell is selected (at most 4,000,000 cells). Excel selects every match at once; the grid here has one selection, so the matches are marked with a tint (at most 10,000 areas, joined down each column), the first is selected, and the bar then says `next` to step through them in reading order, wrapping. The last cell, region and array select their one area. Any edit clears the marks.
- **Locked** and **Hide Formulas When Protected** (the Format menu, Format Cells ▸ Protection): toggle the selection from its first cell's state. Both are refused while the sheet is protected, as Excel greys them. The engine already honoured both (§16.79's protection, `hide_formula`); this adds the commands.
- **Show Detail** / **Hide Detail**: the innermost outline group around the selection (rows first, then columns), or the group whose summary row or column is selected, expanded or collapsed. **Clear Outline** takes away every row and column group on the sheet, showing what they hid.
- **FORECAST.ETS**, **FORECAST.ETS.CONFINT**, **FORECAST.ETS.SEASONALITY**, **FORECAST.ETS.STAT**: additive triple exponential smoothing (ETS AAA) over a timeline with a steady step. Times are sorted; duplicates are combined by the aggregation argument (1 AVERAGE … 7 SUM); missing steps and blank values are interpolated (data completion 1) or zero (0), at most 30% of the points, as in Excel; a time off the step's grid or too few points is #NUM!, sizes that differ #N/A. Seasonality 1 (default) detects the season from the detrended series' autocorrelation, 0 turns it off, 2 to 8,760 sets it; SEASONALITY returns the season found or 0. The parameters minimise the squared one-step errors (a coarse grid, then coordinate refinement). CONFINT uses the AAA model's h-step variance and the normal quantile; STAT returns alpha, beta, gamma, MASE, SMAPE, MAE, RMSE and the step (1–8). Excel doesn't publish its fitting, so results agree exactly where the model fits a series exactly (a line, a line plus a repeating pattern) and closely, not to the digit, on noisy data. At most 100,000 points. Written with `_xlfn.` in `.xlsx` and `COM.MICROSOFT.` in `.ods`.
- **Forecast Sheet…** (a bar, `12 ahead, 95%, season auto`): from a date or time column and a value column (selected, or the region at the cursor, a header row if the first isn't numbers), a new sheet "Forecast <values>" laid out as Excel's: Timeline, Values, Forecast, Lower and Upper Confidence Bound, the forecast columns starting at the last known point so the lines join, then each step ahead as `FORECAST.ETS` and `± FORECAST.ETS.CONFINT` formulas over the copied history, with the timeline's number format, and a line chart of the four. The window moves to it. At most 10,000 steps ahead.
- Tests: `special` (each Go To Special kind with next and wrap, visible cells around a hidden row, Locked and Hidden with undo and refused while protected, Show/Hide Detail from inside the group and from its summary row, Clear Outline with undo, a Forecast Sheet over a straight line with its chart and undo), `eval::ets` (a line plus a season of 4 forecast exactly with a zero interval, season detection, the timeline sorted, combined, filled and refused, the interval widening with the horizon and the confidence), and conformance cases (exact fits, the step from STAT, #NUM! and #N/A).

### 16.149 Protect Sheet with a password and its allowances, Zero Values and Shrink to Fit (2026-11-26)

`atrium-sheets/src/lock.rs`, `officina-convert/src/protect.rs`, the allowances in `atrium-sheets/src/sheet_protection.rs`. Before this, a sheet could only arrive protected in a file, and one with a password could never be unprotected here.

- **Protect Sheet…** (a bar whose text is hidden as it is typed): an optional password. It isn't kept. The sheet carries Excel's verifier instead (`algorithmName="SHA-512"`, a fresh 16-byte salt, `spinCount="100000"`, `hashValue`), so `.xlsx` saves it as Excel protects a sheet. `GridOp::Protection` (tag 36) puts protection on or takes it off, one undo step. At most 255 characters.
- **Unprotect Sheet…**: works at once when there's no password, otherwise asks for it. It checks the password against the verifier the sheet carries: Excel 2010 and later (SHA-512, 384, 256 or SHA-1; H0 = H(salt + UTF-16LE password), then each spin hashes the last hash followed by the spin number, at most 10,000,000 spins), older Excel files' 16-bit `password` hash, and LibreOffice's `table:protection-key` (SHA-256 or SHA-1 of the UTF-8 password). A wrong password changes nothing. The bar's field is emptied once the password is used, right or wrong. An unknown algorithm or a damaged verifier says so and leaves the sheet protected. Not checked against a sheet Excel itself protected: no Excel here.
- **Protection Options…** (a bar, only while the sheet is unprotected): what the next Protect Sheet still allows, as Excel's settings (`formatCells="0"` and so on). Sheets enforces **format cells** (any style change), **format rows** and **format columns** (sizes, hiding by hand), **insert hyperlinks**, **AutoFilter** (using a filter already there; its range and the rows outside it stay as they are), **objects** (charts, pictures, comments) and **scenarios**. Insert and delete rows and columns, sort and pivot tables are kept and saved for Excel but not done here: sorting needs unlocked cells anyway. Unprotecting hands the sheet's allowances back to the options. Excel's settings in a file are kept and enforced the same way.
- **Zero Values** (a menu tick, a view setting per sheet as Gridlines are, not an undo step): when it's off, cells whose value is 0 show blank, except under Show Formulas. Saved as native flag 4 of chunk 30, `.xlsx` `<sheetView showZeros="0">`, and `.ods` `ShowZeroValues` per sheet (and for the whole view when every sheet has it off), both ways.
- **Shrink to Fit** (the Format menu): toggles the style on the selection (the grid already drew it for cells from files). One step; refused on a protected sheet unless it allows format cells.
- Tests: `lock` (protect with a password, the wrong one refused, the right one unprotects, undo both ways, no password; allowances letting formatting through but not hiding a row, kept while protected, handed back; Zero Values blanking a 0 in the grid, Shrink to Fit with undo), `officina-convert` `protect` (a new verifier checks only its password; the iterator's place in the spin; the old 16-bit hash; too many spins and unknown algorithms refused; ODF SHA-256) and `tests/protect.rs` (the verifier and an allowance round trip through `.xlsx` and still unprotect; zero values through `.xlsx` and `.ods`).

### 16.150 Protect Workbook and edit ranges with passwords, Tab between unlocked cells, Sort Left to Right (2026-11-26)

`atrium-sheets/src/lock.rs`, `officina-convert/src/protect.rs`, `Workbook::next_unlocked` and `unlock_ranges` in `officina-grid/src/protection.rs`, `order_cols`/`reorder_cols_op` in `officina-grid/src/sort.rs`, the `tab_target` hook on Pergola's `GridSource`.

- **Protect Workbook…** (a hidden-text bar; **Unprotect Workbook…** while the structure is protected): an optional password for the structure lock. Like Protect Sheet it keeps only Excel's verifier, as `workbookAlgorithmName`, `workbookHashValue`, `workbookSaltValue` and `workbookSpinCount` (SHA-512, fresh salt, 100,000 spins), which `.xlsx` already wrote on `<workbookProtection>`. Unprotecting checks the password the same way, so a structure locked with a password by Excel (new or old hash) now unlocks here with it. With no password, unprotecting needs no bar. The plain **Protect Workbook Structure** tick still toggles a lock without a password, and points at Unprotect Workbook… for one with. One undo step (`GridOp::Structure`).
- **Range Password…** (a hidden-text bar: `TITLE: password`, or `TITLE:` to take it away): gives an Allow Edit Range a password, kept as its verifier's `algorithmName`, `hashValue`, `saltValue` and `spinCount` on `<protectedRange>`. Refused while the sheet is protected, as every change to the ranges is. One step.
- **Unlock Range…** (a hidden-text bar): opens every edit range on the sheet whose password it is, until the workbook closes, as Excel asks for a range's password on the first edit. Unlocked ranges are remembered as their verifiers in the open workbook only (at most 1,024), never saved and not an undo step. `can_edit_cells` opens a range with a password only once it is unlocked; before, such a range was opened by anyone.
- **Tab on a protected sheet** goes to the next unlocked cell in reading order (Shift+Tab the previous), wrapping round, as Excel does; cells unlocked by style and open edit ranges both count. Found from style runs and range rectangles, never cell by cell. A new `GridSource::tab_target` hook (default none) lets Pergola's grid ask, both while editing and while moving; an unprotected sheet keeps plain Tab. Selecting locked cells is still allowed (Excel's `selectLockedCells` is kept, not enforced).
- **Sort Left to Right…** (a bar: `1, 3 desc`): reorders the columns of the selection, or of the data at the cursor, by the values in the listed sheet rows, first to last, as Excel's Sort Options "left to right". Values, formulas (rewritten for their new column), styles, comments and links move with each column. At most 16,384 columns and 64 rows to sort by; refused when merged cells are in the data. One step.
- Tests: `lock` (a workbook password refusing the plain toggle and the wrong password, then unlocking; a range password opening its range only once unlocked, a range without one open at once, taking it away; Tab's order, wrap and back, through the sheet's source, plain once unprotected; Sort Left to Right by a row and descending, a row outside untouched, undo), `officina-grid` `sort` (`left_to_right_moves_whole_columns`), `officina-convert` `protect` (workbook and range verifiers) and `tests/protect.rs` (both passwords through `.xlsx` and still checking).

### 16.151 Superscript, subscript, double underline, Fill, Justify and Distributed alignment, Next and Previous Comment (2026-11-26)

`officina-grid/src/style.rs` (`Font::double`, `Font::script`), `officina-convert` `xlsx/styles.rs`, `ods/read.rs` and `ods/write.rs`, `xls/mod.rs`, Pergola's `inline::Emphasis` and `grid_view::Spread`, `atrium-sheets/src/format.rs`.

- **Superscript** and **Subscript** (the Format menu; each a toggle, one replacing the other): for the whole cell, as Excel's Format Cells font effects. Drawn at 62% of the size, raised or lowered by 0.3 em of the full size. Kept as native style flag bits 3 and 4 of the extended style byte (both at once is refused on load), `.xlsx` `<vertAlign val="superscript|subscript"/>` (and `.xls` FONT escapement on reading). In `.ods` the cell style carries `style:text-position="super 58%"` and its text a span with the same, because LibreOffice ignores the first and keeps only the second. A cell whose text is one such span, as LibreOffice writes it, reads back as a superscript or subscript cell.
- **Double Underline** (the Format menu): two lines under the text. Plain Underline turned off takes the double away too. Native flag bit 2, `.xlsx` `<u val="double"/>` (`doubleAccounting` reads as double), `.ods` `style:text-underline-type="double"`, `.xls` underline 2 or 0x22.
- **Fill the Cell**, **Justify** and **Distributed** (the Format menu, beside the other alignments). Files already kept them, but they were drawn as Left until now. Fill repeats the whole text as many times as it fits and never runs over its neighbours. Justify wraps the text and stretches every line except the last of each paragraph to both edges. Distributed wraps and spreads every line's words evenly, with a lone word centred. In `.ods`, Fill is `style:repeat-content="true"` and Distributed is `fo:text-align="justify"` with `css3t:text-justify="distribute"`, both as LibreOffice writes them. Printing still draws them as before: `print.rs` belongs to the printing work.
- **Next Comment** and **Previous Comment** (the Data menu): move the cursor to the sheet's next or previous comment in reading order, wrapping round. The status shows where it is, which comment of how many, the author and the first line, for example `B2 (1 of 3): Ana: check this`.
- LibreOffice check (`officina-convert/examples/fonteffects_lo.rs write|read`, LibreOffice 24.2 in the cloud container): our `.xlsx` and our `.ods` converted by LibreOffice keep all six effects and alignments, and LibreOffice's `.ods` of our file reads back with all of them.
- Tests: `officina-grid` `style` (effects through the native encoding; super and sub together refused), `officina-convert` `tests/fonteffects.rs` (`.xlsx` and `.ods` round trips), Pergola `fill_justify_distributed_and_script_cells_draw_as_spreadsheets_do` (repeats, word positions, a paragraph's last line, script size and offset, one rule or two), Sheets `format` (the toggles reaching the grid's cell, undo; next comment order and wrap) and `app` (Next Comment moving the cursor and its status).

### 16.152 Document properties, the rest of Paste Special, accounting underlines, Insert Symbol (2026-11-26)

`officina-grid/src/docprops.rs`, `What` in `officina-grid/src/clip.rs`, `Font::accounting`, `officina-convert` `xlsx/read.rs`, `xlsx/write.rs`, `ods/read.rs`, `ods/write.rs`, `atrium-sheets/src/checks.rs` (`properties`), `atrium-sheets/src/symbols.rs`.

- **Properties…** (a bar, for example `title: Budget 2027; author: Ana Rao`): the workbook's title, subject, author, keywords, category and comments, as Excel's File > Info keeps them. `NAME:` on its own clears one property and `clear` clears them all. Each property is trimmed, has its control characters dropped (line breaks are kept), and holds at most 4,096 bytes. `GridOp::Properties` (tag 37) makes this one undo step, allowed on protected sheets and structure. Saved as native chunk 36 PROPERTIES, `.xlsx` `docProps/core.xml` (written only when one is set, `dc:title`, `dc:subject`, `dc:creator`, `cp:keywords`, `cp:category`, `dc:description`), and `.ods` `meta.xml` (`dc:title`, `dc:subject`, `meta:initial-creator`, one `meta:keyword` for each keyword, `dc:description`, and the category as the user-defined property `OOXMLCorePropertyCategory`, as LibreOffice keeps `.xlsx`'s). The **Document Inspector** lists the properties set, second after comments, and Remove All clears them.
- **Paste Special** gains Excel's remaining choices, each in the Paste menu and on the Paste Special bar:
  - **Values and Number Formats** and **Formulas and Number Formats**: the contents with the copied cells' number formats, each landing cell keeping the rest of its own style.
  - **All Except Borders**: everything, with each landing cell keeping its own borders and diagonals.
  - **Column Widths**: only the copied columns' widths, kept with the copy when it was taken.

  Each is one step.
- **Single Accounting Underline** and **Double Accounting Underline** (the Format menu): the line runs under the whole cell's width rather than the text's, for one-line text. Native flag bit 5, `.xlsx` `singleAccounting`/`doubleAccounting` (and `.xls` underline 0x21/0x22). `.ods` has no accounting underline, so it saves as a plain single or double underline, as LibreOffice does.
- **Insert Symbol…** (a bar): a symbol by its name (44 of them: degree, euro, rupee, plus minus, not equal, check, arrows, fractions…), by its code (`U+2122`), or typed as itself (at most 8 characters, no control characters, no surrogates), put after the cursor cell's text as one step. Refused in a formula cell.
- LibreOffice check (`officina-convert/examples/fonteffects_lo.rs`, LibreOffice 24.2): all six properties survive our `.xlsx` to LibreOffice's `.ods` and our `.ods` to LibreOffice's `.xlsx`. LibreOffice writes keywords to `.xlsx` joined by spaces (`budget plan`) and drops accounting underlines to plain ones.
- Tests: `officina-grid` `docprops` (cleaning, bounds, the native file and undo) and `clip` (each new paste choice: what lands and what stays, undo), `officina-convert` `tests/fonteffects.rs` (properties through `.xlsx` and `.ods`, no `core.xml` when none are set; accounting underlines through `.xlsx`), Pergola (an accounting underline spans the cell), Sheets `checks` (the Properties bar, the inspector line, Remove All and undo), `symbols`, `format` (the accounting toggles) and `data` (the paste commands from the menu).

### 16.153 Font Size…, Move or Copy Sheet…, Standard Width and Height, the 1904 date system, vertical justify (2026-11-26)

`atrium-sheets/src/sheetbars.rs`, `GridOp::DateSystem1904` in `officina-grid/src/ops.rs`, `CellView::vspread` in `pergola/src/widgets/grid_view.rs`, `atrium-sheets/src/data.rs`.

- **Font Size…** (a bar, in the cell menu): any size from 1 to 409 points in whole or half points (`10.5`, `13 pt`), as Excel's size box takes; anything else is refused. 11 pt is kept as no size, as Larger and Smaller keep it. One step.
- **Move or Copy…** (a bar, in the sheet tab menu): `before NAME`, `after NAME`, `first` or `end`, with `copy` first to copy the sheet there instead (Duplicate Sheet's copy, then moved, one undo step). Sheet names match ignoring case and may be quoted. Refused while the workbook's structure is protected, and when the named sheet is the one being moved. The bar starts at the sheet's own place (`before` the next sheet, or `end`).
- **Standard Width…** and **Standard Height…** (bars, in the cell menu): the sheet's default column width (`8.43 ch` or pixels) or row height (`15 pt` or pixels), up to 8,192 px. Columns and rows someone sized keep their sizes, as in Excel. `GridOp::LayoutSizes`, so sheet protection's format columns and format rows switches apply.
- **1904 Date System** (a checked item in the Data menu): the workbook's dates counted from 1 January 1904, as Excel for Mac's were, or from 1900 again. `GridOp::DateSystem1904` (tag 38, one byte 0 or 1) marks every formula dirty, so date functions recalculate: `DATE(1904,1,2)` is 1463 in the 1900 system and 1 in the 1904 one. Allowed on protected sheets and structure, as Excel's Options are. The native file, `.xlsx` (`workbookPr date1904`) and `.ods` (`table:null-date`) already kept the setting.
- **Justify Vertically** and **Distribute Vertically** (the cell menu, after Align Bottom): wrapped lines spread from the top of the cell to the bottom, the first line at the top edge and the last at the bottom; a single line sits in the middle. Files already kept `justify` and `distributed`; they are now drawn (`CellView::vspread`).
- Tests: `officina-grid` `ops` (the op's encoding, refusals of bad bytes, recalculation and undo), Pergola (vertical spread, top to bottom and one line centred), Sheets `sheetbars` (font sizes and refusals, moves and copies to each place, structure protection, standard sizes keeping sized columns, undo) and `source` (the vertical commands set the style and draw spread; the 1904 switch recalculates and undoes).

### 16.154 The Borders bar, side, inside and diagonal borders, dashed lines drawn (2026-11-26)

`border_ops` and `borders_typed` in `atrium-sheets/src/format.rs`, `Dash` and `dashed_edge` in `pergola/src/widgets/grid_view.rs`.

- **More border commands** in the Format menu: **Top Border**, **Left Border**, **Right Border**, **Inside Borders** (the lines between the selection's cells, not round it), **Diagonal Down Border** and **Diagonal Up Border**, each in the border pen's line and colour. Inside sets each cell's inner sides, so either neighbour shows the line, as Excel's files do. **No Borders** now takes diagonals off too. Every command builds disjoint parts of the selection against the same styles, so one undo step takes it all back.
- **Borders…** (a bar, from the Format menu), Excel's Format Cells > Border as words: the edges (`all`, `outside`, `inside`, `inside horizontal`, `inside vertical`, `top`, `bottom`, `left`, `right`, `top and bottom`, `diagonal down`, `diagonal up`, `none`), then a line (`thin`, `medium`, `thick`, `double`, `hair`, `dashed`, `dotted`, `medium dashed`, `dash dot`, `medium dash dot`, `dash dot dot`, `medium dash dot dot`, `slanted dash dot`) and a colour (a tab colour's name, six hex digits, or `automatic`). A part left out comes from the pen. Clauses are separated by `;` (`outside thick red; inside horizontal dashed 1F4E79`) and make one step together. At most 512 bytes; an unknown word is refused with what it should be.
- The Format menu's **Border Line** pen also offers Thick, Double, Medium Dashed and Dash Dot (nine lines in all).
- **Dashed, dotted, double and hair lines are drawn** as they are saved (`CellView::dashes`): dashed three widths on and one off, dotted (and hair) one on and one off, the dash-dot styles as a dash, gap, dot, gap, and double as two one-pixel lines the width apart. An edge needing more than 512 pieces is drawn solid.
- LibreOffice check (`officina-convert/examples/fonteffects_lo.rs`, LibreOffice 24.2): all 13 line styles, their colour and both diagonals survive our `.xlsx` to LibreOffice's `.ods` and our `.ods` to LibreOffice's `.xlsx`, except slanted dash dot, which LibreOffice does not have (it comes back as medium dashed or medium dash dot).
- Tests: Pergola (dashed, dotted, dash-dot and double pieces), `officina-convert` `tests/fonteffects.rs` (every line style and both diagonals through `.xlsx` and `.ods`), Sheets `format` (inside, side and diagonal borders, No Borders, the double pen, the bar's clauses, colours, one undo, refusals, dashes reaching the drawn cell).

### 16.155 Fill Up and Left, Pattern Fill…, Special number formats, gridline colour (2026-11-26)

`fill_way` in `atrium-sheets/src/series.rs`; `pattern_typed`, `SPECIAL_FORMATS` and `grid_color_typed` in `atrium-sheets/src/format.rs`; `Sheet::grid_color` in `officina-grid/src/book.rs`; `officina-convert` `xlsx/read.rs`, `xlsx/write.rs`, `ods/settings.rs`; `GridSource::grid_color` in Pergola.

- **Fill: Up** and **Fill: Left** (the Data menu, with **Fill: Down** and **Fill: Right** beside them, as Excel's Home > Fill): the selection's last row (last column) copied over the rest of it. A selection one row (column) deep takes the row below (the column to the right), as Ctrl+D takes the row above; at the sheet's edge it is refused. One step each; F4 repeats them.
- **Pattern Fill…** (a bar, from the Format menu), Excel's Pattern Style with Pattern Color and Background Color: `PATTERN [COLOUR] [on COLOUR]`, the pattern by Excel's name (`75% gray`, `50% gray`, `25% gray`, `12.5% gray`, `6.25% gray`, `thick` and `thin` `horizontal stripe`, `vertical stripe`, `reverse diagonal stripe`, `diagonal stripe`, `thick diagonal crosshatch`, `diagonal crosshatch`, `thin horizontal crosshatch`, `thin diagonal crosshatch`) or the files' (`darkTrellis`), and colours as names or hex. `none` takes the pattern off. A pattern replaces a plain or gradient fill, as Excel's Fill tab does. The bar starts with the first cell's pattern. Patterns were already read, drawn and saved.
- **Special number formats** (the Format menu, Excel's Special category): Zip Code `00000`, Zip Code + 4 `00000-0000`, Phone Number `[<=9999999]###-####;(###) ###-####` (seven digits without the area code), Social Security Number `000-00-0000`.
- **Gridline Colour…** (a bar, in the View part of the Data menu): the sheet's gridlines drawn in a colour, or `automatic`. A view setting, as Gridlines is: no undo step, but the file keeps it: native chunk 37 GRID_COLORS; `.xlsx` `<sheetView defaultGridColor="0" colorId>`, which takes only Excel's 56 palette colours, so another colour is saved as the nearest of them; `.ods` `GridColor` in each sheet's view settings and, when every sheet has the same one, the whole view's (which is the one LibreOffice reads). LibreOffice's own default grey (C0C0C0) reads as automatic.
- LibreOffice check (`fonteffects_lo.rs`, LibreOffice 24.2): a red gridline colour survives our `.xlsx` to LibreOffice's `.ods`, and our `.ods` through LibreOffice back to `.ods`. LibreOffice does not write a gridline colour to `.xlsx` at all, even its own, and it reads a pattern fill as a single blended colour.
- Tests: `officina-convert` `tests/fonteffects.rs` (gridline colours through `.xlsx`, snapped to the palette, and `.ods`, LibreOffice's grey as automatic), Sheets `format` (pattern names and colours, none, undo, refusals, the bar's start; the four special formats as shown; the gridline colour bar, drawn, saved natively, automatic again) and `series` (Fill in each direction, one-row and one-column selections, the sheet's edge).

### 16.156 Font…, cells drawn in their own font, Find with wildcards and in values (2026-11-26)

`font_name` in `atrium-sheets/src/sheetbars.rs`, `drawn_family` in `atrium-sheets/src/source.rs`, `CellView::family` in Pergola, `Search::excel` and `find::values` in `officina-grid/src/find.rs`, `atrium-sheets/src/authoring.rs`.

- **Font…** (a bar, in the cell menu before Font Size…): any font name, as Excel's font box takes one (`Calibri`, `Times New Roman`), at most 31 characters, with no control characters, quotes, angle brackets, `&`, `;` or commas. `automatic` goes back to the sheet's own font. The name is kept as typed, so the files carry it. The status says which face draws it. One step.
- **Cells draw in their font**, whether set here or read from a file: the name goes through `atrium_fonts::Library::resolve` to the face Atrium ships for it (Calibri as Carlito, Cambria as Caladea, Arial and Helvetica as Liberation Sans, Times New Roman as Liberation Serif, a family we ship as itself). Plex, the generic names and sans faces we don't know keep the grid's own sans, and monospace names keep the grid's monospace (Font: Mono). Italic text still draws in Plex Italic, since the renderer opens a named family's upright face only.
- **Find uses Excel's wildcards**: `*` for any run of characters, `?` for any one, and `~` before `*`, `?` or `~` for the character itself. Other characters are plain. Replace takes the whole run a wildcard matches (`o*` in `coat` replaced by `ow` gives `cow`), as Excel's does.
- **Values** (a new Find option beside Match case, Whole cell and All sheets), as Excel's Look in: Values: finds what cells show (a formula's result through its number format, `50%`) rather than what was typed. Replace is refused while it is on, as Excel's Replace looks in formulas only. Cells whose value protection hides are skipped.
- Tests: `officina-grid` `find` (wildcards, `~`, whole cell, case, the greedy replacement, refusals; values as shown), Sheets `sheetbars` (font names and their faces, Plex, unknown and monospace names, refusals, automatic, undo) and `authoring` (wildcard finds, `~*`, a value found only in values, Replace refused there, a wildcard replacement).

### 16.157 Spelling (F7), Find All, Find in comments (2026-11-26)

`atrium-sheets/src/spelling.rs`, the Spelling bar in `atrium-sheets/src/whatif.rs` and `app.rs`, `GridAction::Spelling` in Pergola's grid view, `atrium-sheets/src/authoring.rs`.

- **Spelling…** (Data menu, F7), as Excel's Spelling: walks the sheet's typed text from the cursor, row by row, wrapping once to the cells before it, using `officina-spell`'s en-US dictionary. Each word it doesn't know moves the cursor to its cell and opens a bar: `Not in the dictionary: "wrold" in A1. Suggestions: world, …`. It passes over words with digits, words in capitals, and addresses (`@`, `://`, `www.`), as Excel's defaults do. Formulas and numbers are not checked. At most 200,000 text cells.
- In the bar: an empty answer or `ignore` (Ignore Once) goes on; `ignore all` skips that word for the rest of the run; a word changes this occurrence to it (at most 255 characters, no control characters); `change all WORD` changes every occurrence on the sheet in one undo step. Edits go through protection as typing would. At the end the status says `The spelling check is complete.` and the bar closes.
- **Find All** (a Find button): goes to the first match in sheet order and lists the matches in the status, `Found 3 cells: Sheet1!A1, Sheet1!A2, Sheet1!B3.`; past 20, the rest are counted (`and 10 more`).
- **Comments** (a Find option, exclusive with Values), as Excel's Look in: Comments: searches each comment's text and every reply in its thread instead of cells. Replace is refused while it is on.
- Tests: Sheets `spelling` (walks from the cursor and wraps, passes digits and capitals, Ignore All, Change All in one step), `app` (the bar steps through the sheet, refuses a 256-character word, closes at the end), `authoring` (Find All's list and count, comments found only in Comments, the exclusive options, Replace refused) and Pergola's grid key test (F7).

### 16.158 Mark as Final, AutoCorrect, Custom Views (2026-11-26)

`DocProps::marked_final` in `officina-grid/src/docprops.rs`, `officina-grid/src/views.rs` (GridOp tag 39, native chunk 38 CUSTOM_VIEWS), `atrium-sheets/src/autocorrect.rs`, `atrium-sheets/src/customviews.rs`, `toggle_final` in `atrium-sheets/src/checks.rs`, the final refusal in `atrium-sheets/src/sheet_protection.rs`, the `_MarkAsFinal` reading and writing in `officina-convert` (`xlsx/read.rs`, `xlsx/write.rs`, `ods/read.rs`, `ods/write.rs`).

- **Mark as Final** (a checked menu item beside Properties…), as Excel's: the workbook becomes read only. Every edit is refused with "This workbook is marked as final. Turn off Mark as Final to edit it." Only property changes go through, and turning the mark off is one of those. Marking and unmarking are undo steps. Opening a final workbook says so in the status. Files keep the mark: in `.osheet` it is a byte after the six properties, written only when set, so older files read as before. `.xlsx` writes Excel's custom property `_MarkAsFinal` in `docProps/custom.xml` plus `cp:contentStatus` Final, and reads the custom property back. `.ods` keeps it as LibreOffice keeps an `.xlsx`'s custom property, a boolean `meta:user-defined` named `_MarkAsFinal`.
- **AutoCorrect** on typed text, with Excel's defaults where they apply to cells:
  - About 50 common misspellings (`teh`, `adn`, `recieve`, `seperate`), with the typed case kept (`Teh` becomes `The`, `TEH` becomes `THE`).
  - Contractions typed without the apostrophe (`dont`).
  - `(c)`, `(r)`, `(tm)` and `(e)` become ©, ®, ™ and €. `-->`, `<--`, `==>` and `<==` become arrows.
  - TWo INitial CApitals keeps one capital. Only words with at least two lowercase letters after the capitals count, so `IDs` stays as typed.
  - Day names take a capital.

  It leaves formulas, numbers and dates alone, along with words containing digits and anything in an address (`@`, `://`, `www.`). The correction is a second undo step after the typing, so one undo gives back what was typed, as in Excel, and the status names each change. Ctrl+Enter into a selection is corrected the same way.
- **AutoCorrect Options…** (a bar): `on` or `off`; `add WORD REPLACEMENT` (the word is 1 to 64 bytes with no spaces, the replacement up to 255, at most 1,000 entries of one's own); `remove WORD` takes out one's own entry or a built-in one. As in Excel, these settings belong to the app, not the file: they last while the document is open and are not saved.
- **Custom Views…** (a bar):
  - `add NAME` saves the current sheet's view: hidden rows and AutoFilter, column widths and hidden columns, zoom and the selected cell. A name used again replaces that view.
  - `show NAME`, or the name alone, puts rows, filter and columns back as one undo step, then goes to the view's sheet, zoom and cell.
  - `delete NAME` removes a view, and `list` names each view with its sheet.

  The views are saved with the workbook, at most 64, with names up to 64 bytes and unique ignoring case. Adding and deleting are undo steps. Structural edits don't move a view. A view whose sheet is gone is refused when shown. `.xlsx` and `.ods` don't carry views yet; Excel's are `customSheetView` elements.
- Tests: grid `docprops` (the mark round-trips and a bad byte is refused) and `views` (capture, show, undo, the op and the native file, duplicate names, a bad zoom); convert `fonteffects` (`_MarkAsFinal` in `.xlsx` and `.ods`); Sheets `checks` (edits refused while final, properties still set, the mark through the file and its status, undo), `autocorrect` (the list is sorted; words, symbols, capitals and days; what is left alone; the second step, formulas untouched, options and refusals) and `customviews` (add, show, undo, replace, refusals, delete); `app` (a commit corrected, and a view shown from another sheet at its zoom and cell).

### 16.159 Alt Text, Check Accessibility, checkboxes in cells, Find by format (2026-11-26)

`Chart::alt` and `clean_alt` in `officina-grid/src/charts.rs`, `Style::checkbox` in `officina-grid/src/style.rs`, `atrium-sheets/src/accessibility.rs`, `atrium-sheets/src/checkbox.rs`, the Format option in `atrium-sheets/src/authoring.rs`, `CellView::checkbox`, `checkbox_box` and `GridAction::ToggleChecks` in Pergola's grid view, and the chart `descr` and `svg:desc` handling in `officina-convert` (`xlsx/chart.rs`, `ods/chart.rs`, `ods/read.rs`).

- **Alt Text…** (beside Delete Chart, for the chart under the cursor or using its cell) is a bar that sets what the chart shows, for a screen reader; `none` clears it. It is one undo step. The text is trimmed, keeps line breaks, has control characters dropped, and is at most 4,096 bytes.
  - `.osheet`: bit 7 of the chart's legend byte says a string follows the series, so older files read as before.
  - `.xlsx`: Excel's `descr` on the chart frame's `xdr:cNvPr`, with a line break as `&#10;`.
  - `.ods`: the frame's `svg:desc`.
- **Check Accessibility** (beside Spelling…), as Excel's Review ▸ Check Accessibility. A bar steps through what it finds with Next, going to each place and saying what is wrong and how to fix it (`… (2 of 5; Sheet1!A4)`). It looks for:
  - charts without alt text;
  - tables without a header row;
  - merged cells;
  - text whose colour against its fill is under WCAG's 4.5:1 (the font colour, black when automatic, against the solid fill, white when none), checking up to 200,000 cells a sheet;
  - sheets with content still named SheetN.

  It lists at most 1,000 issues and counts the rest. Not checked yet: pictures' alt text (pictures don't carry it), and contrast against patterns, gradients and conditional formats.
- **Checkboxes in cells** (Insert Checkbox, a checked menu item), as Excel 365's Insert ▸ Checkbox:
  - A cell format that shows TRUE as a ticked box and FALSE as a clear one. Other values show as they are. The box is centred unless the cell is aligned left or right.
  - Inserting fills the selection's empty cells with FALSE, at most 100,000 cells.
  - From a cell that has a checkbox, the item takes the format off and the values stay.
  - A click on the box, or Space with the cursor on one, flips the selection's boxes: if any is clear they all tick, otherwise they all clear. Cells whose formula decides them, or that hold other values, are left alone. Each flip is one undo step.
  - Clear Formats removes the box like any format.
  - The format is bit 6 of the style's extra flags in `.osheet`.
  - `.xlsx` and `.ods` keep the TRUE and FALSE values but not the box yet (Excel keeps it in a feature property bag).
  - Print shows TRUE and FALSE.
- **Find by format** (a sixth Find option, Format), as Excel's Format… ▸ Choose Format From Cell. Turning it on takes the cursor cell's whole format, and Find, Find All and Replace then match only cells formatted exactly so. With nothing typed, Find and Find All list every cell of that format that has content. Turning it off again matches any format.
- Tests:
  - Grid: `charts` (alt text through the wire, one flag bit plus the string) and `style` (the checkbox round-trips).
  - Convert: `roundtrip` (alt text with quotes, an ampersand and a line break, in `.xlsx` and `.ods`).
  - Sheets:
    - `accessibility`: Alt Text set, cleaned, undone and cleared; WCAG contrast; the check finding a default name, merged cells and a pale font, and stepping through them.
    - `checkbox`: insert, flip, formula and text cells left alone, undo, removal, the native file, the size limit.
    - `authoring`: find by format with and without text; Replace All only in that format.
  - Pergola's grid: a click on the box, a click beside it, Space on a checkbox, Space elsewhere.

### 16.160 Alt text for pictures, Replace Format (2026-11-26)

`Picture::alt` and `PICTURE_ALT` in `officina-grid/src/charts.rs`, the picture paths in `atrium-sheets/src/accessibility.rs`, the Replace format option in `atrium-sheets/src/authoring.rs`, and picture `descr` and `svg:desc` in `officina-convert` (`xlsx/chart.rs`, `ods/chart.rs`).

- **Alt Text… for pictures.** The item now works for a picture as well as a chart. It looks first for a chart over the cursor's cell, then a picture over it, then a chart drawn from the cell's data, and is greyed when there is none. Setting, cleaning (the §16.159 rules) and `none` work as for charts, one undo step each.
  - `.osheet`: flag bit 13 of the picture says a string follows the link, so older files read as before.
  - `.xlsx`: `descr` on the picture's `xdr:cNvPr`, read and written, with a line break as `&#10;`.
  - `.ods`: the picture frame's `svg:desc`.
  - Check Accessibility now also lists "Missing alt text: picture N", which closes the gap §16.159 left.
- **Replace Format**, as Excel's Replace ▸ Format… for the replacement. A seventh Find option, Replace format, takes the cursor cell's whole format; turning it off again leaves formats alone. The options now sit in two rows of four.
  - With text to find, Replace and Replace All change the text as before and give each changed cell that format. The status says "Replaced text and format in N cells".
  - With nothing typed and a Format to find, Replace All restyles every cell of the found format that has content and leaves its text ("Replaced the format of N cells").
  - Either way it is one undo step, and at most 10,000 cells at once.
- `officina-convert/examples/sheets_protection_fixture.rs` builds again: it matches the Excel workbook protection format (§16.143).
- The new `Picture::alt` field broke two test-only `Picture { … }` literals in `atrium-sheets/src/print.rs`; they now say `alt: None`. Nothing else in that file changed.
- Tests:
  - Grid: `charts` (`picture_alt_text_round_trips_with_its_flag`).
  - Convert: `roundtrip` (`picture_alt_text_round_trips_in_xlsx_and_ods`).
  - Sheets: `accessibility` (picture alt text set, undone and reported by the check) and `authoring` (`replace_format_restyles_only_the_cells_it_changes`: a format-only replace, its undo, and text with format leaving unmatched cells alone).

### 16.161 Data Analysis: the Analysis ToolPak's tools (2026-11-26)

`atrium-sheets/src/analysis.rs`, the Data Analysis… item beside Forecast Sheet…, and its bar in `whatif.rs` and `app.rs`.

**Data Analysis…** is Excel's Analysis ToolPak as one typed bar: `TOOL RANGE, options…, to CELL`. It starts on `descriptive` over the selection, with `labels` when the selection's first row is text. `labels` means the range's first row names each column. Without `to`, the result goes on a new sheet named after the tool, and Sheets shows that sheet. With `to CELL` (on any sheet), the result goes there, but only when every cell it needs is empty; otherwise it names the first cell in the way and changes nothing. Either way it is one undo step. It reads at most 1,000,000 cells and writes at most 1,000,000.

- **Descriptive Statistics** (`descriptive`): for each column, Excel's 13 measures: Mean, Standard Error, Median, Mode, Standard Deviation, Sample Variance, Kurtosis, Skewness, Range, Minimum, Maximum, Sum and Count. `confidence 95%` adds the Confidence Level row (CONFIDENCE.T). It handles at most 256 columns.
- **Histogram** (`histogram`): counts the values that are at or under each bin and over the bin before it, with a More row for the rest.
  - `bins RANGE` gives the bins. Without it there are ⌈√n⌉ equal bins up to the largest value.
  - `cumulative` adds the running percentage.
  - `pareto` adds the bins again, sorted by count.
- **Rank and Percentile** (`rank`): for each column, the table runs largest first and shows each value's Point (its place in the data), the value, its Rank (ties share one) and its Percent, as PERCENTRANK.INC gives it (three digits, truncated).
- **Moving Average** (`moving average`, `interval N`, 3 by default): one column or row. Each output is `=AVERAGE(…)` of that point and the N−1 points before it, and the first N−1 outputs are #N/A.
- **Exponential Smoothing** (`smoothing`, `damping D`, 0.3 by default): the first output is #N/A and the second is the first value. After that, each is `=(1−D)·value before + D·smoothed value above`, written as formulas, as Excel does.
- **Correlation** and **Covariance**: the lower triangle of CORREL, or of COVARIANCE.P (the population covariance, as the ToolPak uses), for every pair of columns, with labels along both edges. The diagonal of the correlation matrix is 1.
- **Sampling** (`sampling`):
  - `periodic N` takes every Nth number.
  - `random N` draws N numbers with replacement, as the ToolPak does.
- **Random Number Generation** (`random ROWSxCOLS`): draws from one distribution:
  - `uniform FROM TO` (0 to 1 by default)
  - `normal MEAN SD`
  - `bernoulli P`
  - `poisson LAMBDA` (at most 1,000)

  `seed N` makes Sampling and Random draw the same numbers each time. The generator is SplitMix64. Without a seed, the clock seeds it.

Differences from Excel:
- Descriptive Statistics, Correlation and Covariance write formulas, so their results follow later edits to the data. Excel writes fixed values.
- Histogram, Rank and Percentile, Sampling and Random write values, as Excel does.
- Not built yet: ANOVA, the F-, t- and z-tests, Regression and Fourier Analysis, and the histogram's chart. LINEST and the T.TEST family cover them in formulas.

Tests (Sheets, `analysis`):
- Descriptive Statistics on a new sheet, following an edit to the data and undone in one step.
- A histogram with bins, cumulative and pareto, written in place.
- A refusal to write over cells.
- Rank and Percentile with ties.
- Correlation and covariance checked against worked values.
- Moving average and smoothing worked by hand.
- Seeded sampling and random draws that repeat with the same seed, with their bounds checked.

The unused `Rect` import that §16.160 left in `accessibility.rs` is moved into its tests.

### 16.162 The rest of the Analysis ToolPak, and Solver (2026-11-26)

Round 1 of the six that finish Sheets. The code is in `atrium-sheets/src/hypothesis.rs` (the new Data Analysis tools), `atrium-sheets/src/solver.rs`, and the Solver… item after Data Analysis… in `data.rs`, `whatif.rs` and `app.rs`.

**Data Analysis…** gains the ToolPak's remaining tools in the §16.161 bar. They take the same `labels`, `to CELL` and new-sheet rules. Tools that compare two variables take a second range after the first. Except for Fourier, every number is a formula, so the tables follow later edits to the data. Excel writes fixed values.

- **ANOVA** (`anova RANGE`): Anova: Single Factor, one group to a column, from 2 to 200 groups.
  - It has a SUMMARY (Count, Sum, Average, Variance) and the ANOVA table: SS, df, MS, F, P-value and F crit for Between and Within Groups and the Total.
  - Within is the sum of each group's DEVSQ. Total is DEVSQ over all the groups. Between is the difference.
  - P-value is F.DIST.RT and F crit is F.INV.RT at `alpha` (0.05 by default).
- **Two-factor ANOVA** (`anova2 RANGE`):
  - **Without replication**: one value for each row and column. `labels` reads both the first row and the first column. It gives a SUMMARY of each row and column, then Rows, Columns, Error and Total.
  - **With replication** (`rows N`, N rows to a sample): the first row and first column are labels, as Excel requires. It gives Sample, Columns, Interaction, Within and Total.
  - Its summary is shortened: each sample's average in each column, plus the totals. Excel lists Count, Sum, Average and Variance for every sample.
- **F-Test Two-Sample for Variances** (`ftest A, B`): Mean, Variance, Observations, df, F, and the one-tail P and F Critical. Like Excel, it uses the lower tail when F < 1.
- **t-Tests** (`ttest paired`, `ttest equal`, `ttest unequal`; `difference D` sets the hypothesized mean difference):
  - Mean, Variance, Observations, then Pearson Correlation (paired) or Pooled Variance (equal variances).
  - Then Hypothesized Mean Difference, df, t Stat, and the one-tail and two-tail P and t Critical (T.DIST.RT, T.INV, T.DIST.2T, T.INV.2T).
  - Unequal variances use Welch's df rounded to a whole number, as the ToolPak does.
  - Paired needs as many numbers in each range. Its t uses the variance of the differences, VAR₁ + VAR₂ − 2·COVARIANCE.S.
- **z-Test: Two Sample for Means** (`ztest A, B, variances V1 V2`): the known variances are required. Then z, and the one-tail and two-tail P and z Critical.
- **Regression** (`regression Y, X`, with up to 16 X columns; options `constant zero`, `confidence 95%`, `residuals`): Excel's SUMMARY OUTPUT.
  - Regression Statistics: Multiple R, R Square, Adjusted R Square, Standard Error and Observations.
  - ANOVA: df, SS, MS, F and Significance F.
  - Coefficients for the intercept and each X, with Standard Error, t Stat, P-value and the confidence bounds.
  - Each of these numbers is an `INDEX` into `LINEST(Y, X, const, TRUE)`.
  - `residuals` adds the predicted Y and residual for each row, for up to 100,000 rows.
- **Fourier Analysis** (`fourier RANGE`, `inverse`): the discrete Fourier transform of 2, 4, … 4,096 numbers or complex texts.
  - It is written as complex text, as IMSUM and the other complex functions write it (`-2+2i`).
  - Rounding noise smaller than 10⁻¹² of the largest part shows as 0.
  - `inverse` divides by n, so a transform and its inverse give back the input.

**Solver…** is in the Data menu, after Data Analysis…. It works as Excel's Solver add-in, typed in a bar: `max B10, by B1:B3, B4 <= 100, B1:B3 int, report`. With the cursor on a formula, the bar starts as `max CELL, by `.

- **Objective**: `max CELL`, `min CELL`, or `CELL = value`.
- **Cells to change**: `by` and ranges, at most 200 cells. They must hold numbers or be empty, not formulas.
- **Constraints**:
  - `CELLS <= | >= | = a value`, or `CELLS <= CELLS` (one cell on the right, or one for each cell on the left), at most 1,000.
  - `CELLS int` and `CELLS bin` (whole numbers, or 0 and 1).
  - The cells change only to values of 0 or more unless you add `unconstrained`. This matches Excel's default, "Make Unconstrained Variables Non-Negative".
- **Method**:
  - Solver first checks whether the model is linear. It measures the objective and every constraint at zero and at each unit step, then checks those coefficients at two other points.
  - A linear model is solved exactly with a two-phase simplex (dense tableau, Bland's rule).
  - Integer and binary cells are handled with depth-first branch and bound, up to 5,000 nodes.
  - Any other model uses Nelder–Mead on the objective plus a growing penalty for broken constraints. Like Excel's GRG Nonlinear, it finds a local optimum; it is not GRG, so the path and sometimes the answer differ.
  - `simplex` refuses a model that isn't linear. `nonlinear` skips the linearity check. Integer cells need a linear model.
- **Limits and checks**:
  - Solver stops after 20 seconds or 200,000 evaluations.
  - It checks the answer on the sheet itself to Excel's 10⁻⁶ precision before keeping it.
  - It says when the model is unbounded, infeasible, or stopped at a limit. The messages follow Excel's wording.
- **Undo**: the answer goes in as one undo step, and every cell is put back while Solver works.
- **Report**: `report` adds an Answer Report sheet in the same undo step. It lists:
  - the objective cell's original and final value;
  - each variable cell's original and final value, Contin or Integer;
  - each constraint's cell value, formula (`$A$4<=4`), Binding or Not Binding, and slack.
- **Not built yet**: Excel's Sensitivity and Limits reports, the Evolutionary method, and saving the Solver model with the sheet. In Excel the model lives in hidden names such as `solver_opt`. Here the model is not kept once the bar closes.

Tests:
- `hypothesis`:
  - single-factor ANOVA against worked values (SS 54 and 6, F 27, p 0.001, F crit 5.1433);
  - two-factor ANOVA with and without replication, worked by hand;
  - the paired, equal and unequal t-tests, the F-test and the z-test, including Welch's rounded df and the refusals;
  - Regression against a hand fit, with its residuals;
  - Fourier of 1, 2, 3, 4 and back.
- `solver`:
  - the textbook product-mix LP (x = 2, y = 6, profit 36) and its Answer Report, undone in one step;
  - an integer optimum that differs from the rounded LP answer;
  - a value-of goal and a minimum;
  - unbounded and infeasible models;
  - a nonlinear minimum, with and without non-negative cells;
  - the simplex on its own, including the infeasible and unbounded cases.

### 16.163 Charts: trendlines, labels, secondary axis, error bars and six new kinds (2026-11-26)

Round 2 of the six that finish Sheets. The model is in `officina-grid/src/charts.rs`, the drawing in `officina-draw/src/chart.rs`, the files in `officina-convert/src/xlsx/chart.rs` and `officina-convert/src/ods/chart.rs`, and the command in `atrium-sheets/src/chartformat.rs`.

**Model.** Each series gains a `SeriesLook`:

- `trend`: a trendline, linear, exponential, logarithmic, polynomial (order 2 to 6), power or moving average (period 2 to 255). It can show its equation and R², and extend 0 to 10⁶ periods forward and backward.
- `labels`: each point's value as a data label.
- `secondary`: plotted against a second value axis on the right.
- `errors`: Y error bars, fixed, percent of each value, N standard deviations of the series (around its mean, as Excel draws them) or standard error.
- `sizes`: a bubble chart's sizes, one row or column.
- `totals`: the waterfall points that are totals, ascending, at most 4,096.
- `bins`: a histogram's bin count (1 to 10,000) or width. Without it, Scott's rule, as Excel's automatic bins.

`ChartKind` gains Radar, Bubble, Stock (three series high, low, close, or four with open first), Waterfall, Histogram and BoxWhisker. `whole()` names the kinds that can't be in a combo: pie, doughnut, radar, stock, waterfall, histogram and box. `SeriesLook::valid()` states the ranges above. `add_chart` refuses a look outside them, and the wire decoder refuses the same, so nothing the model holds fails to load. On the wire, bit 0x80 of the grouping byte says a section of looks follows the alt text; a chart without looks is byte for byte what it was.

**Drawing.**

- The secondary axis has its own scale and labels on the right.
- Trendlines are drawn across the x range plus the extensions. The equation and R² are written at the top right in Excel's form (`y = 0.011x² - 0.3736x + 8 R² = 0.0684`). Exponential, logarithmic and power fits leave out points they can't take (zero or negative), and a fit that isn't possible is not drawn.
- Data labels go above each point or bar, for up to 500 points a chart.
- Error bars have caps at both ends.
- Radar: a spoke per category, rings at the value ticks, a polygon with markers per series.
- Bubble: area proportional to size, the largest radius an eighth of the plot's shorter side.
- Stock: a high-low line per category and a close tick; with four series, an up-down bar from open to close, hollow when it rose and filled when it fell.
- Waterfall: bars float from the running total, increases and decreases in two colours, totals stand on the axis in a third, with an Increase / Decrease / Total legend.
- Histogram: Excel's bins, the first closed (`[1, 3]`), the rest open on the left (`(3, 5]`).
- Box and whisker: Excel's exclusive quartiles, whiskers to the last value within 1.5 IQR, the mean as ×, outliers as dots.

**Files.**

- `.xlsx` read and write:
  - radar, bubble (`bubbleSize`) and stock groups (`hiLowLines`, up-down bars with four series);
  - combos on the secondary axes (a second category and value axis pair, values on the right, `crosses max`); a group whose axis ids differ from the first group's is read as secondary;
  - `dLbls` with `showVal`, `trendline` (type, order, period, forward, backward, `dispEq`, `dispRSqr`) and `errBars` (Y only; X bars and custom amounts are reported);
  - waterfall, histogram and box and whisker as Excel 2016's chart extension: `xl/charts/chartExN.xml` (`cx:` markup, its own content type and relationship type), in an `mc:AlternateContent` whose fallback is the shape Excel 2013 shows. Waterfall totals are `subtotals`, bins are `binCount` or `binSize`, box plots use exclusive quartiles. The reader skips `mc:Fallback` and reads `cx:` parts back; Pareto lines and other extension layouts are reported.
- `.ods` read and write:
  - `chart:radar` (also `filled-radar`), `chart:bubble` (the series' values are its sizes, its first domain the y values and the second the x values, as ODF has it) and `chart:stock`;
  - per series: `chart:data-label-number="value"`, `chart:attached-axis="secondary-y"` with a `secondary-y` axis, `chart:regression-curve` with ODF 1.3's regression properties (LibreOffice's `loext:` ones are read too) and `chart:equation`, and `chart:error-indicator` with `constant`, `percentage`, `standard-deviation` or `standard-error`.
  - ODF has no waterfall, histogram or box plot. They are written as bar charts with `officina:kind` on the chart and `officina:totals` and `officina:bins` on the series. Officina reads them back; LibreOffice shows plain columns and drops the attributes when it saves.
- Checked through LibreOffice 24.2 (`cargo run --example charts_lo`): in `.xlsx`, the combo with a trendline, labels, error bars and a secondary axis, radar, bubble with sizes and stock all come back unchanged; LibreOffice drops the three chart-extension kinds, as it has no `cx:` support. In `.ods`, everything comes back but those three, which LibreOffice saves as columns.

**Format Chart…** (Data menu, beside Alt Text…) works on the chart at the cursor, or the chart drawn from the cursor's cell, as Excel's Change Chart Type and Format Data Series. One line, one undo step, and the bar starts with the chart's settings so typing them back changes nothing:

`type waterfall; series 1: labels, totals 1, 12`
`series 2: line, secondary, trend poly 3 equation r2 forward 2; errors percent 5`

- `type KIND` sets the chart's kind (column, bar, line, area, scatter, pie, doughnut, radar, bubble, stock, waterfall, histogram, box). A series' own kind that can't stay is cleared, and the whole kinds group side by side.
- Parts split on `;`. A part led by `series N:` is for that series; otherwise for every series. Settings split on `,`; after `totals`, bare numbers are more points (1 is the first).
- Settings: `labels`, `secondary` / `primary`, a series kind for a combo (`line`, `column`, `area`, …), `trend linear | exponential | log | poly N | power | moving N` with `equation`, `r2`, `forward N`, `backward N`, `errors fixed N | percent N | stddev N | stderr`, `sizes RANGE`, `totals POINTS`, `bins N`, `bin width W`, and `no labels`, `no trend`, `no errors`, `no sizes`, `no totals`, `bins auto`.
- Refused, with the reason, changing nothing: a stock chart without three or four series, a secondary axis on a pie, trendlines on kinds without them, a polynomial order or period out of range, a radar or other whole kind in a combo, a series that isn't there.

The chart bar's type menu lists all thirteen kinds, and its stacking menu shows only for column, bar, line and area.

**Not built yet**: X error bars, custom error amounts from cells, several trendlines on a series, the trendline's intercept and label formatting, data label positions and number formats, Pareto lines on histograms, overflow and underflow bins, and the inclusive quartile method for box plots.

Tests:
- grid: a chart with every look rides the wire and every truncation of it fails cleanly; each out-of-range look is refused by `valid()`, `add_chart` and the decoder.
- draw: linear, quadratic, exponential and power fits recover the coefficients they were made from, with R² of 1; moving averages; box statistics on 1…10 and 100 (Q1 3, median 6, Q3 9, 100 an outlier, whisker at 10); waterfall bars and totals; histogram counts by count and width; percent and standard-deviation error bars. Every kind still lays out inside its box.
- convert: nine charts covering every new kind and every extra round-trip unchanged through `.xlsx` (with the `chartExN.xml` parts and their content type checked) and through `.ods`.
- Sheets: Format Chart sets each extra, its bar text reads back and retyping it changes nothing, one undo takes it back; type changes to waterfall, histogram, bubble and stock with their settings; eight refusals leave the chart as it was.

### 16.164 PivotTables: Show Values As, calculated fields, grouping, slicers, timelines and PivotCharts (2026-11-26)

Round 3 of the six that finish Sheets. Show Values As, calculated fields and grouping were already in the pivot engine (`officina-grid/src/pivot.rs` and `pivot/`), with no way to set most of them from the app. This round adds that command, slicers and timelines, PivotCharts, and the file support the grouping lacked in `.xlsx`.

**Model** (`officina-grid/src/pivot/slicer.rs`).

- `Pivot.filters`: a `PivotFilter` per field filtered from outside the layout. It holds the items hidden and an optional date range `between` (source values from `.0` up to but not including `.1`). A slicer's selection is its field's hidden items: on the layout field when the field is in the layout, else in a filter of its own. A timeline's selection is a filter's range. `set_hidden` and `set_range` put each where it belongs and drop a filter left with nothing to do.
- `Pivot.slicers`: up to 32 `Slicer`s, each a name (unique ignoring case, 1 to 255 bytes), a field and a sheet anchor, with `timeline` set to years, quarters, months or days for a timeline.
- `pivot_output` skips source rows a filter hides, or whose raw value is outside a range, before anything is counted. Nothing of a filter shows in the pivot's cells, as in Excel.
- `check_pivot` validates both: at most 16 filters, one per field, the field in the source or derived, a finite range with its start before its end on a source column, and an anchor inside the sheets.
- On the wire, bit 4 of the layout tag says a section of filters and slicers follows. A pivot without them is byte for byte what it was.
- Moving, deleting and inserting rows, columns or sheets moves slicer anchors as it moves chart anchors. Deleting a slicer's sheet deletes the slicer.

**PivotCharts.** A chart whose series all sit inside a pivot's output follows that pivot. Every `pivot_op` re-points its series with `pivot_chart_series(shape, compact)`: one series per value column (grand total columns left out), rows from the body's first row to the last that isn't a total, categories from the innermost label column, and the name from the header cell above. Each series keeps its look and kind by position, so filtering, regrouping or refreshing the pivot redraws the chart from what the pivot now shows. There is no separate chart type: a PivotChart is a chart over a pivot, which is also how it is saved.

**PivotTable Tools…** (Data menu, before Pivot Table Fields…, `atrium-sheets/src/pivottools.rs`). It works on the pivot at the cursor, or the pivot whose slicer is at the cursor. It takes one line, split on `;`, as one undo step, and starts with how the first value shows:

`show Sum of Amount as % of parent row; group Date by quarters, years`
`slicer Region at L3; timeline Date by months; select Region: West; dates Date from 2024-03 to 2024-05; chart column`

- `show VALUE as KIND`. KIND is Excel's Show Values As list: normal, % of total, row, column, parent row, column or `% of parent FIELD`, index, `running total in FIELD`, `% running total in FIELD`, `rank largest by FIELD` and `rank smallest by FIELD`, `difference from FIELD ITEM`, `% of FIELD ITEM` and `% difference from FIELD ITEM`. ITEM can also be `previous` or `next`. `percent` is the same as `%`.
- `calculated NAME = FORMULA` adds a calculated field and sums it (the formula names fields, as `=Amount*0.1`). `remove calculated NAME` removes it.
- `group FIELD by PARTS`, with PARTS any of seconds, minutes, hours, days, months, quarters and years. The finest part groups the field in place, and each coarser part becomes a new field placed before it on its axis, as in Excel. `group FIELD by 7 days` makes week buckets. `group FIELD from A to B by STEP` makes numeric intervals, and without `from … to` the bounds are automatic. `ungroup FIELD` removes the field's groupings, the fields they made, and the slicers and filters on those fields.
- `slicer FIELD [at CELL]` and `timeline FIELD [by years|quarters|months|days] [at CELL]` add one. It is named after the field ("Region", then "Region 1"…) and placed at CELL or right of the pivot. A timeline needs a field with dates. `remove slicer NAME` and `remove timeline NAME` take one away and clear what it filtered.
- `select FIELD: ITEM, ITEM` shows only those items, and `select FIELD: all` clears the selection. `dates FIELD from 2024-03 to 2024-05` keeps March through May (a year, a month or a day at each end), and `dates FIELD: all` clears it.
- `chart [KIND]` adds a PivotChart (column unless named) right of the pivot, or below it when a slicer is in the way.
- These are refused, with the reason, changing nothing: an unknown field or item, a base field not on an axis, a second grouping of a grouped field, grouping text, a timeline on a field without dates, a 33rd slicer, and a calculated field name already used.

**On the sheet** (`atrium-sheets/src/source.rs`). Each slicer is a framed panel with its name. Below the name is a button per item, up to 200, filled when shown and pale when hidden. A press shows only that item, and the × at the top right, present while the slicer filters, shows everything again. A timeline shows its level and a row of periods across, from the first to the last date in the source, up to the latest 400. It shows as many as fit, ending at the selection. A press keeps that period. Each press is one undo step labelled Slicer or Timeline. The panels are drawn from a cache keyed by the pivot and its output, so redrawing doesn't rescan the source.

**Files.**

- `.xlsx`:
  - Date grouping is written as Excel writes it. The source column's shared items are dates (`<d>`, `containsDate`). The in-place part is a `fieldGroup` with `rangePr groupBy="months"` (or whichever part) and its start and end dates, automatic or not. Each coarser part is a `databaseField="0"` cache field with its own `fieldGroup`, chained finest to coarsest with `par`. `groupItems` list every period, with the items below the start and above the end first and last, and the pivot fields' items point into them.
  - Interval grouping is a `rangePr` with `startNum`, `endNum` and `groupInterval`, or `groupBy="days"` with a `groupInterval` for week-style buckets.
  - On reading, Excel's own labels ("Qtr1", "1-Jan", "<1/5/2024") are matched by position to this engine's, so hidden items still name the right periods. Automatic bounds read as automatic, and fixed numeric bins read as before.
  - A slicer's selection on a field outside the layout is the hidden items (`h="1"`) of a pivot field with no axis, which is how Excel stores it. A timeline's range is a `dateBetween` pivot filter, with `greaterThanOrEqual` and `lessThan` custom filters. An inclusive `lessThanOrEqual`, as Excel's timelines write, is read as the next day's start.
  - The slicers and timelines themselves are an `extLst` extension of the pivot table (`urn:atrium:officina:pivot-slicers:1`) that Excel passes over. Excel opens the file with the pivot filtered as it was, but shows no slicer panels.
- `.ods`: filters are `hidden`-orientation data pilot fields with the hidden members `table:display="false"`. Ranges and slicers are `officina:date-range` and `officina:slicer` elements in the data pilot table, which LibreOffice ignores.

**Not built yet**:
- Excel's own slicer and timeline parts (`slicerCache`, `slicer`, `timelineCache`, and the drawing objects that place them): Excel shows the filtering but not the panels.
- One slicer connected to several pivots.
- A slicer's multi-select mode and its style.
- Calculated items.
- `c:pivotSource` on a saved PivotChart, which Excel uses to show its field buttons.

Tests:
- grid: a slicer on a field outside the layout and a timeline range filter the output without showing; both survive the wire, and every truncation of it fails cleanly. A PivotChart follows its pivot when an item is hidden and a value field removed, keeping its look; a chart elsewhere stays as it is.
- Sheets: Show Values As and calculated fields through the bar; dates grouped by quarters and years and numbers by intervals; slicers, timelines, `select`, `dates` and a PivotChart; a press on a drawn slicer filters its pivot.
- convert:
  - `.xlsx` round trip of months grouped in place with years and quarters, interval bins with an automatic end, a Region slicer selection outside the layout, a 2024 timeline range and both slicers, with the same pivots and cells after reading;
  - Excel's "Qtr1" labels and an inclusive timeline end, read as this engine's;
  - the same book through `.ods`.

### 16.165 Look and objects: themes, table styles, shapes and text boxes, Camera, Quick Analysis (2026-11-26)
Round 4 of the six-round finishing plan. The workbook's look is one grid value, `officina_grid::look::Look`: the theme, custom table styles, shapes and Camera pictures. One op, `GridOp::Look` (wire kind 40), replaces it, and a look that fails `Look::valid` applies as nothing. Native files keep it in a `LOOK` chunk, written only when it isn't the default. Inserting or deleting rows and columns moves shapes and cameras with their cells and a camera's source with its range; a camera whose source is deleted goes. Deleting a sheet takes its shapes and the cameras on it or reading it, and moving a sheet keeps them pointed at the right one. Sheet protection allows the look to change except shapes and cameras on a sheet that isn't open to objects.

**Themes** (`look.rs`). A theme is a name, Excel's twelve colours (dark 1, light 1, dark 2, light 2, six accents, two link colours) and heading and body fonts. Twelve are built in: Office, Office 2007, Grayscale, Blue, Blue Green, Green, Yellow Orange, Orange Red, Red Violet, Violet, Median and Paper. Cells hold plain RGB, so `Workbook::theme_op` is a restyle: each cell colour (fill, font, border) that was one of the old theme's colours becomes the new theme's colour in the same slot, and each font named as the old heading or body font becomes the new one. Other colours and fonts stay. It is one undo step. Unnamed fonts are drawn in the grid's own sans as before; the theme fonts rename only fonts set by name. Charts take their series colours from the theme's accents (and darker tints after six), and the chart cache key includes the theme, so a theme change redraws them.

**Table styles**. Excel's gallery is computed from the theme: Light 1–21, Medium 1–28 and Dark 1–11, each family seven styles (dark 1, then accents 1–6). A style is up to seven parts (whole table, header row, total row, row stripe, column stripe, first column, last column), each with a fill, text colour, bold and a border colour. `Workbook::table_cell_look` gives a table cell's look from its style and the table's options: banded rows (the table's `stripes`), banded columns, first column and last column (`TableBands`, kept in the tables' wire flags). Custom styles are named in the look, up to 64, and can't take a built-in name. The grid draws table cells with their style's look; a table with an unknown style keeps the plain tint it had.

**In Sheets** (`atrium-sheets/src/objects.rs`), five typed bars on the Data menu. Each line is one undo step, split on `;` where it takes several settings, and anything refused changes nothing and says why.
- **Themes…** `theme Green` applies a whole theme; `colors Violet` takes only its colours, `fonts Paper` or `fonts Georgia / Verdana` only fonts; `list` names the built-in themes.
- **Table Design…** (in a table) `style Medium 9` (or `TableStyleMedium9`, a custom name, `none`); `banded rows`, `banded columns`, `first column`, `last column`, each with `no` to turn it off; `new style Brand with header 1F4E78 text white bold, stripe DDEBF7, whole border 1F4E78` makes a custom style from parts (whole, header, totals, stripe, column stripe, first column, last column) and settings (a fill colour, `text COLOUR`, `bold`, `border COLOUR`); `delete style NAME` removes one, and tables using it keep the plain tint. Colours are `none`, a theme colour (`accent1`…`accent6`, `dark1`, `light1`, `dark2`, `light2`), a colour name or six hex digits.
- **Insert Shape…** `rounded "Next step" at D2 size 4x3 fill accent2 line none text white bold 14`. Friendly kinds are rectangle, rounded, oval, triangle, arrow, star, diamond, hexagon, heart, cloud and callout, and any DrawingML preset name works (the set `.pptx` and `.docx` shapes use, over 180) (`flowChartDecision`). `text box "Words"` makes a shape with no fill or outline. On a shape the same settings change it, and `delete`, `front` and `back` remove it or change its order. Shapes are drawn with the preset's outline (`officina-shapes`), filled and stroked, with the text centred and wrapped in the preset's text rectangle. They move and resize by dragging like charts.
- **Camera…** `A1:B6 at D1` (or `Sheet2!A1:C9 at D1`) places a live picture of the range: its cells with fills, gridlines and formatted text, stretched to the picture's box and redrawn as the cells change. On a camera, `source RANGE` points it elsewhere and `delete` removes it. A source is at most 10,000 cells, and a workbook holds at most 64 cameras.
- **Quick Analysis…** on the selection: `data bars [COLOUR]`, `color scale`, `icon set`, `greater than N`, `top N` or `top N%`, `clear format`; `chart KIND`; `totals sum|average|count|percent|running [below|right]` (as Quick Totals, a sum below by default); `table`; `sparklines line|column|win/loss`.

**Files**.
- `.xlsx`:
  - The theme is `xl/theme/theme1.xml`, written when it isn't Office's: the colour scheme, the major and minor Latin fonts and the plain format scheme DrawingML requires. Reading takes a file's theme name, colours and fonts; a theme with Office's colours and fonts (Excel's "Office Theme") reads as the default, so a plain file carries no look.
  - Table options are `tableStyleInfo`'s `showColumnStripes`, `showFirstColumn` and `showLastColumn`, both ways.
  - Custom table styles are written as Excel's `<tableStyles>` in `styles.xml`, each part a `tableStyleElement` pointing at a `dxf` after the conditional formats' (font bold and colour, solid fill, thin border), so Excel shows them. On reading we take them from our extension (below); a foreign file's own custom styles aren't read yet.
  - Shapes are `xdr:sp` in the sheet's drawing, after its charts and pictures: `prstGeom`, `solidFill` or `noFill`, an `a:ln`, the text as centred paragraphs with the size, bold and colour on each run, `txBox="1"` for a text box and `descr` for alt text. Reading takes explicit `spPr` settings first and the shape's `xdr:style` references (`fillRef`, `lnRef`, `fontRef`, theme colours resolved through the file's theme) for the rest. Grouped shapes, connectors and custom geometry are dropped and reported.
  - Camera pictures and custom styles ride in a workbook `extLst` extension holding an `officina:look` element (`urn:atrium:officina:look:1`) that Excel passes over. Excel shows no Camera picture.
- `.ods`:
  - Shapes are `draw:custom-shape` in the sheet's `table:shapes`, with the outline written as an enhanced geometry (`ooxml-…` type and path, as `.odp` writes it) and an automatic graphic style for fill, outline and text, so LibreOffice draws them. `officina:` attributes carry our exact settings and cell anchor. Reading prefers those, and otherwise takes LibreOffice's own shapes from their graphic style and outline type (Office presets by `ooxml-` name, LibreOffice's own types by the `.odp` mapping).
  - Table styles and options are `officina:table-style` and `officina:bands` on the named database range.
  - The theme, custom table styles and cameras are an `officina:look` element at the end of `office:spreadsheet`, which LibreOffice ignores.
- Reading our element is bounded: it is parsed as a small tree under the crate's XML limits, and each part is kept only if the look stays valid with it, so an unknown sheet, a bad colour, a built-in style name or an oversized camera source drops that part alone, with a note.

**Not built yet**:
- Reading a foreign file's custom table styles from `styles.xml`.
- Shape rotation, flips, adjustment handles, gradients, shadows, connectors and groups on sheets; rich text runs inside a shape.
- Excel's own Camera picture (a picture linked by a formula); Excel opens our file without it.
- Theme effects and the theme's colour variants; a theme saved as a `.thmx` file.

Tests:
- grid: a theme change recolours the old theme's colours and renames its fonts, and undoes; built-in and custom table styles with their options give the expected header, stripe, totals and first-column looks, and a custom style can't take a built-in name; shapes and cameras survive the wire (every truncation fails cleanly) and the native file, follow inserted rows, and go with a deleted sheet.
- Sheets: Themes, Table Design with a custom style, Insert Shape (shapes and text boxes, delete and order) and Camera, and Quick Analysis formats, charts, totals and sparklines through the bars.
- convert:
  - the theme, a custom table style with all four options, a callout and a text box on two sheets and a camera survive `.xlsx` and `.ods`, and a plain book carries no look;
  - the `.xlsx` parts are what Excel reads (theme name and accent, `tableStyle` elements, the table options, `prstGeom`, run size and bold, `txBox`);
  - an Excel-style shape takes its colours from its style references, and a grouped shape is dropped with a note;
  - hostile `officina:look` parts (an unknown sheet, an oversized camera source, a built-in style name, a bad colour) are dropped one by one;
  - LibreOffice opens both files and saves them again, and the callout's outline, text and fill and the text box come back (and the theme, through `.xlsx`).

### 16.166 Files and leftovers: checkboxes and Custom Views in files, AutoCorrect kept, Convert Notes to Comments, New Window (2026-11-27)
Round 5 of the six-round finishing plan.

**Checkboxes in cells** (§16.159) now go through files.
- `.xlsx` is written as Excel 365 writes them. Each checkbox style's `xf` carries an `xfpb:xfComplement` extension (`{C7286773-470A-42A8-94C5-96B5CB345126}`) that points into `xl/featurePropertyBag/featurePropertyBag.xml` (a Checkbox bag, the XFControls cell control, the XFComplement and its mapper). The part has its content type and a workbook relationship, written only when a style has a checkbox. Reading takes an `xf` with an `xfComplement` as a checkbox, which is the one feature Excel stores that way.
- `.ods` has no cell checkbox. The cell style gets `officina:checkbox="true"` on its `style:table-cell-properties`, and LibreOffice shows the TRUE or FALSE value.

**Custom Views** (§16.158) now go through files.
- `.xlsx` gives Excel its own elements: a `customWorkbookView` for each view (its name, a GUID derived from its position, and its sheet as `activeSheetId`), and in every sheet a `customSheetView` with that GUID and the zoom. The view's own sheet also gets the selected cell. Excel can list and show them by name with their sheet, zoom and cell. Each view's hidden rows, filter and columns ride in our workbook extension as `officina:views`: the views' wire form (`GridOp::CustomViews`) in hex, decoded under the wire limits. A malformed element gives no views and a note, and a view on a sheet the file doesn't have is dropped with a note.
- `.ods` keeps the same `officina:views` element at the end of `office:spreadsheet`.

**AutoCorrect** settings (on or off, entries added and built-in entries taken out) are kept in a file named `autocorrect` beside the custom lists, in the person's own Sheets state. Each document opened afterwards starts with them, as Excel's are the app's and not the file's. The file holds one setting a line (`on`/`off`, `add⇥WORD⇥WITH`, `remove⇥WORD`), at most 1 MiB is read, a line that fails the bar's own limits is skipped, and it is replaced by a write and rename.

**Convert Notes to Comments** (Data menu, after New Comment Thread…), as Excel's Review ▸ Notes ▸ Convert to Comments. Every note in the workbook becomes a comment thread. Its first message is the note's text, with a leading "Author:" line dropped as Excel writes one, by the note's author, or by the person for a note without one. Formatting in a note doesn't carry over, and existing threads stay as they are. It is one undo step labelled Convert Notes to Comments, and is refused when there are no notes.

**Windows** (Data menu, after the split commands), as Excel's View ▸ Window group, within Sheets' one window.
- **New Window** opens a second window on the same workbook. Each window has its own sheet, selection and scroll, and both show the live document. A caption reads "Book:1 — Sheet1" and "Book:2 — Plan", with the active window highlighted. A press in the other window makes it active where it is on screen and keeps its keyboard focus there. The switch is refused while a cell is being edited.
- **Arrange Windows Side by Side** and **Arrange Windows Stacked** place them left and right, or above and below. Split panes and frozen panes work inside the active window.
- **View Side by Side (Scroll Together)** toggles synchronous scrolling. Whichever window moved takes the other along.
- **Close Window** goes back to one.
- The second window is a view: typing and commands go to the active one. Zoom belongs to each sheet as before. A drag that starts in the other window switches on the press, and the rest of that drag isn't kept.

**Not built yet**:
- Excel's own Custom Views from another program's files. They are read only from our extension.
- Separate desktop windows for New Window, a Cascade arrangement, and comparing two different workbooks side by side (one workbook per Sheets window).
- Note formatting carried into the converted comment.

Tests:
- Sheets:
  - `autocorrect`: the settings round-trip as lines, and bad lines are skipped.
  - `comment`: notes on two sheets become threads with their authors, an "Ana:" line is dropped, a thread is left alone, a second run is refused, and the conversion undoes.
  - `app`: New Window, captions, a window on another sheet with its own cursor, switching both ways with the active window moving on screen, stacked, scrolling together, no switch while editing, and Close Window.
- convert `leftovers`:
  - checkboxes and two views (one with a zoom of 150 and a cell on the second sheet, one with an ampersand in its name) survive `.xlsx` and `.ods`;
  - the `.xlsx` holds the feature property bag, its relationship, the `xfComplement`, the `customWorkbookView` names and the `customSheetView` scale and selection;
  - a plain book has neither;
  - odd, non-hex and truncated `officina:views` give no views and a note.

### 16.167 Close-out: the real-workbook sweep (2026-11-27)
Round 6 of the six-round finishing plan, first part. The Mac VM check of installed Sheets and the as-built summary follow.

**The sweep.** `officina-convert/examples/corpus_sheets.rs` reads every `.xlsx`, `.xlsm`, `.xls` and `.ods` under the directories it is given. It writes each workbook as `.xlsx` and as `.ods`, reads both back, and compares every sheet's name and the shown text of each non-empty cell, up to 200,000 cells a file. Volatile formulas (RAND, NOW, TODAY, INFO, CELL, OFFSET, INDIRECT) are left out of the comparison. Each file gets 120 seconds and a 64 MiB stack, and a panic is caught and reported. The output is TSV: `file result ms sheets xlsx ods`.

```
cargo run --example corpus_sheets -- DIR [DIR ...] > report.tsv
```

The corpus is Apache POI's `test-data/spreadsheet` (835 files) and LibreOffice's `sc/qa/unit/data/{xlsx,ods,xls}`, 1,796 files in all. It is test data and stays out of the repository.

**Results**, before and after this round's fixes:

| | First run | After |
|---|---|---|
| read | 1,673 | 1,675 |
| refused, with a reason | 121 | 121 |
| panics | 2 | 0 |
| `.xlsx` round trip the same | 1,667 | 1,670 |
| `.ods` round trip the same | 1,594 | 1,664 |

The slowest read took 905 ms. Every refusal says why:
- 41 are Excel 95 or older, or not an Excel 97–2003 workbook at all;
- 4 are encrypted (3 need a password, 1 uses Excel 95's XOR);
- 5 are XML with undeclared entities (the entity-expansion test files) or broken syntax;
- the other 71 are damaged or malformed: a broken zip or compound file, a stream past the end of the file, records cut short, bad cell references, a missing part.

**Fixed:**
- FIND and SEARCH panicked when the text searched for was longer than the text searched (two files).
- `.xlsx` sheet names now decode `_xHHHH_` escapes, unless the result would hold a control character.
- `.ods` number styles:
  - three- and four-section codes come back as sections, not as `[<0]` conditions;
  - Excel's text section becomes the style itself, with maps for positive, negative and zero, as LibreOffice writes it;
  - `*x` fills go through `number:fill-character`;
  - `?` in a zero section is written as spaces;
  - a code of only `mmmm` is a month;
  - `[DBNum1]` and locale tags no longer turn General into a date;
  - `[ss].00` keeps its decimals, and `[mm]` and `[ss]` read back as elapsed.
  
  Together these fixed the accounting formats that came back as `" $-2883 "`.
- `.ods` tables keep their totals row (`officina:totals` on the database range) and their header's spaces in column names. `Table[#Totals]` and `Table[[Field 4 ]]` no longer give #REF!.
- `.ods` keeps an empty-string cell as text, so `=L12` shows nothing instead of 0.
- `.ods` keeps carriage returns. A cell's CRLF and line ends inside formula attributes are written as character references, because XML turns raw line ends in an attribute into spaces.
- The engine:
  - a filled column that reads its own rows through INDIRECT or OFFSET (a running count down a column) is evaluated top to bottom, where before every row could see the rows above it still uncalculated;
  - a format with no integer placeholder (`.0`) still shows the whole part (45.6, not .6);
  - a table column whose name has a line break matches a formula saved with CRLF.

**Left as they are** (10 files):
- An iterative circular reference converges differently.
- A sheet past 1,048,576 × 16,384 is refused for `.xlsx` and `.ods`.
- Outline groups that `.xlsx` can't hold are refused with a reason.
- A LibreOffice-only function gives #NAME?.
- In `.ods`:
  - the `@` implicit intersection and the space intersection operator (OpenFormula has neither);
  - an icon-set table filter, so SUBTOTAL(9) then counts the rows it hid;
  - text embedded in decimals (`0.000 000`), since ODF embeds text only in the whole part;
  - the Japanese era `ggge`;
  - trailing `?` padding after a fraction.

Tests:
- grid:
  - `conformance`: FIND with a longer needle gives #VALUE!, and SEARCH with one is an error;
  - `numfmt`: `.0` shows 45.6 and .6;
  - `recalc`: an INDIRECT running count over 5,000 rows, before and after an edit at its top (failed before: every row was 1), and a CRLF column name.
- convert `ods_numfmt`:
  - eleven codes (accounting with fills and blanks, euro, red negatives, months, fractions, `[=0]?;0%`) over positive, negative, zero and text values show the same after `.ods`;
  - `[ss].00`, `[mm]:ss`, `[DBNum1][$-804]General` and `[h]:mm:ss`, an empty string, CRLF text and a CRLF formula result round-trip;
  - LibreOffice reads the written accounting and four-section styles with the same text.

### 16.168 Close-out: installed Sheets on the Mac VM, and Sheets as built (2026-11-27)
Round 6 of the six-round finishing plan, second part. This ends the plan.

**The VM check** ran on the Mac's HVF VM (arm64, main at 228f55e10, everything built on the host). It used installed Sheets and Scrinium for a person on a headless 1280×720 frescod. The workbook had three sheets (one with a red tab), a table, a chart, checkboxes and an accounting format.
- Worksheet tabs (§16.71): presses on the Sheet2 label (including the old failure point at (193, 681)), the Data label, two tab edges and the colour bar each showed that sheet.
- The typed bars from rounds 2 to 4 each opened: Format Chart…, PivotTable Tools…, Themes…, Table Design…, Insert Shape…, Camera… and Quick Analysis….
- New Window showed two captioned windows. Arrange Side by Side, View Side by Side (Scroll Together) and Close Window worked.
- Save, then reopening through the picker in a new document jail, kept every setting:
  - the accounting text (`$ 1,234.50`, `$ (56.25)`, `$ -`);
  - the checkboxes, the tab colour, the three sheets, the table, the chart and the column width.
  
  A host read of the saved file matched the original cell for cell.

**Fixed from the check:**
- The Data menu (about 218 items, 6,300 px) and the Format menu now take at most the window's height below their button. Their items scroll by the wheel and the arrows, as in a pop-up frescod shrank. Before, the items below Zoom In could not be seen. Pergola's `Menu` and `MenuButton` gained `max_height`.
- Escape closes a typed bar (Themes…, Solver…, Sort…, Goal Seek…, a name) as its Cancel does. Before, only Find and the chart editor took Escape.
- A typed bar's hint is cut with an ellipsis where the window ends, and the whole hint is the tooltip.

**Found, not fixed here:**
- Scrinium's ordinary Save refuses a workbook the person owns ("Cannot preserve workbook ownership. Use Save As."). Scrinium writes the replacement as its own app user and can't give the file back to its owner (`scrinium/src/workbook.rs`, `prepare_replacement`). Files made in a shell, by another app or by an earlier install can only be saved with Save As. This belongs to Scrinium.
- When the arrows move a menu's highlight, the screen shows it only at the next input event. Pergola's own menu test sees the highlight move at once, so the missed frame is in the app's loop on the guest.
- The bar labels "Quick" and "Table" are short to fit the 100 px label column.

Evidence is on the Mac in `~/atrium-vm/evidence/sheets-closeout-2026-11-27/`:
- `RESULTS.md`;
- 46 screenshots;
- the original and saved workbooks;
- the build, deploy and guest logs.

**Sheets as built.** Sheets covers the everyday Excel desktop in one window. Each feature's section is in this chapter (§16.1–§16.168), and the dated log is in the appendix.
- The grid is the engine of §8. It has dynamic arrays, tables and structured references, the function library of §8.2, iterative calculation, recalculation within a time budget with visible cells first, and undo.
- Every document is opened in its own jail (§3). Files go through Scrinium's picker, with review and merge (§16.68) and versions.
- `.xlsx`, `.xlsm`, `.xls` (read), `.ods`, `.csv` and the native format read and write with a report of what changed (§5.2). Real workbooks round-trip as §16.167 measures.
- The rest is built as listed in the HANDOFF row and the §16 sections:
  - formatting, conditional formats, validation, filters and sorting;
  - names, comments and notes, protection and sharing marks;
  - charts with Format Chart…, PivotTables with PivotTable Tools…;
  - What-If (Goal Seek, Scenarios, Data Table, Solver), the Analysis ToolPak, sparklines;
  - themes, table styles, shapes, Camera, Quick Analysis, checkboxes, Custom Views;
  - spelling and AutoCorrect, Find and Replace, printing to Prelum, New Window.

**Out of scope** (decided 2026-11-26): Power Query and the Data Model, VBA, linked data types, online services and co-authoring, WEBSERVICE and IMAGE. Page Setup and headers and footers belong to the printing workstream.

Tests:
- Pergola `a_menu_with_a_most_height_scrolls_inside_the_window`: 200 items under a 240 px most height draw fewer than 20 and keep within it. The arrows reach item 29 and the wheel the last.
- Sheets `escape_closes_a_typed_bar_as_its_cancel_does`: Themes…, Format Chart…, Quick Analysis… and Solver… close on Escape, and Escape with nothing open does nothing.

### 16.169 Write finishing plan (proposed 2026-11-27)

After Sheets (§16.162–§16.168), Write gets a fixed plan of seven rounds, the same way: each round is one push and one report, and the plan ends with a close-out. It starts from a real-document sweep, so the order follows what real files lose.

**The sweep.** `officina-convert/examples/corpus_write.rs` reads every `.docx`, `.docm`, `.odt` and `.doc` under the directories it is given. It writes each document as `.docx` and as `.odt`, reads both back, and compares each paragraph's visible text and a count of what the document holds: paragraphs, table cells, pictures, shapes, floats, notes, comments, tracked edits, sections, headers and footers, list items, fields, links, contents entries, tab stops, styled paragraphs, indents and spacing, and the characters that are bold, italic, underlined, sized, coloured, in a named font or highlighted. Each file gets 120 seconds and a 64 MiB stack, and a panic is caught and reported. At the end it ranks the readers' fidelity notes by how many files have each.

```
cargo run --release --example corpus_write -- DIR [DIR ...] > report.tsv 2> notes.tsv
```

The corpus is LibreOffice's `sw/qa/extras/{odfexport,odfimport,ooxmlexport,ooxmlimport,ww8export,ww8import}/data` and `sw/qa/core/data`, and Apache POI's `test-data/document`: 2,695 files. It is test data and stays out of the repository. The first run, before any fix:

| | files |
|---|---|
| read | 2,532 |
| refused, with a reason | 163 |
| panics, time-outs | 0 |
| `.docx` round trip the same | 2,164 |
| `.odt` round trip the same | 1,999 |

The slowest file took 414 ms. The text itself comes back in all but 2 (`.odt`). What moves is structure: sections (316 files through `.docx`, 378 through `.odt`; a document that starts with a table gains a section break after it), tab stops (25, 121), indents (50 through `.odt`), list items (39 through `.odt`), sizes and fonts (16 and 11 through `.docx`), headers and footers (11, 5) and contents entries (7). The refusals are Word 95 and older, Word 2, fuzzed compound files, damaged ZIPs and XML, three with a password and one DTD.

What the readers keep only approximately or drop, by files (of 2,532):

| files | what |
|---|---|
| 293 | paragraph borders (dropped) |
| 222 | paragraph shading (dropped) |
| 214 | capitals and small capitals (shown as typed) |
| 159 | page breaks inside a paragraph (a line break) |
| 138 | VML pictures and embedded objects (dropped) |
| 135 | frames and drop caps (kept in line) |
| 121 | text boxes (their text kept after the paragraph) |
| 106 | WordArt (drawn as plain text) |
| 71 | hidden text (shown) |
| 69 | merged cells (one wide cell; 8,475 of them) |
| 53 | nested tables (their text kept in the cell) |
| 48 | tables in headers and footers (cells one under another) |
| 46 | equations (dropped) |
| 45 | cells merged down (each row keeps its own) |
| 45 | text effects (dropped) |
| 55 | TIFF, SVG, SVM and PICT pictures (dropped) |
| 32 | headers and footers of more than one line (joined) |
| 31 | tight wrap (wrapped square) |
| 30 | floating shapes in `.doc` (dropped) |
| 12 | charts and embedded objects; text boxes in headers and footers |
| 8 | deleted table rows (kept); grouped floating shapes |

**The everyday gap.** Beside the sweep, Write's window lacks much of Word's Home, Insert, Layout, References and Review tabs: font and size pickers, colour and highlight, a Font… and Paragraph… bar, indents and spacing, the ruler, borders and shading, table tools beyond rows and columns, hyperlinks and bookmarks, captions, an index, citations, Word's formatting keys, Format Painter, zoom, Go To, and most of Review's settings. The "Not yet" lists of §16.12–§16.30 and §16.46–§16.52 are folded into the rounds below.

**The rounds.**

1. **Characters, and the sweep's round-trip fixes.** Sections, tabs, indents, lists, sizes and fonts kept through both writers. Capitals and small capitals, hidden text (shown with a dotted underline, hidden in print), double strikethrough, superscript and subscript, character spacing and scale, outline and shadow text effects. Page breaks inside a paragraph as real page breaks. A font and size picker on the toolbar, font colour and highlight, a Font… bar, Clear Formatting, Change Case (Shift+F3), Format Painter, and Word's character keys (Ctrl+Shift+>/<, Ctrl+]/[, Ctrl+=, Ctrl+Shift+=, Ctrl+Space, Ctrl+D, Ctrl+Shift+A/K/H/W/D, Ctrl+Enter).
2. **Paragraphs and lists.** A Paragraph… bar (indents, first line and hanging, spacing, line spacing, keep with next, keep lines together, widow control, page break before) and Ctrl+M/Ctrl+T/Ctrl+Q/Ctrl+1/2/5. Increase and Decrease Indent. The ruler, with indents and tab stops dragged on it and a Tabs… bar (leaders). Paragraph borders and shading with a Borders bar. Drop caps and frames placed as frames. Lists: restart and start at, the bullet and number gallery, multilevel lists and their levels' settings. Line numbers and automatic hyphenation.
3. **Tables.** Cells merged across and down, nested tables, cell shading, border styles per side, cell margins and vertical alignment, table styles (Word's built-in gallery and the file's own), header rows repeated and rows split across pages, column widths dragged and AutoFit, tables in headers and footers, and deleted and inserted table rows as tracked changes. A Table Tools bar: insert above, below, left and right, merge and split cells, delete the table, distribute rows and columns, sort, text to table and back, and a formula field (SUM(ABOVE)).
4. **Objects and pages.** Text boxes as real floats with their own text, WordArt as shapes, VML pictures and shapes, floating shapes in `.doc`, groups of shapes, tight wrap round a picture's outline, dragging and resizing pictures by their handles, floats in headers, footers and notes. TIFF, SVG and PICT pictures (decoded in the converter). Charts in documents, drawn by Sheets' chart code and edited with its Format Chart… bar. Headers and footers of many paragraphs (and their tables). Per section: page size, margins and header and footer, columns of their own widths, column breaks, blank pages for odd and even starts. Watermarks, page colour and page borders.
5. **References and equations.** Captions (Figure, Table, Equation) and a table of figures; the contents' levels and styles, and clicking an entry to go there. Bookmarks, a hyperlink bar (Ctrl+K), cross-references to bookmarks, numbered items, notes and captions. An index (Mark Entry, Insert Index). Citations and a bibliography (APA, Chicago, IEEE) kept in the document's sources. Footnote and endnote settings (numbering, restart per page or section, symbols, separator). Equations: OMML and ODF MathML read, drawn and written, and an equation bar that takes a linear form (`x = (-b ± √(b^2-4ac))/(2a)`).
6. **Review, mail merge and files.** Tracked changes in notes, headers, footers and paragraph formatting; filtering the lane by author, Original and All views, review keys; compare side by side; three-way merge with a separate base. Mail merge from a sheet (Sheets workbook or CSV as the source, merge fields, preview, finish to one document or to PDFs). Document properties, Restrict Editing, Mark as Final, Check Accessibility, the Document Inspector, AutoCorrect kept, the word count bar, Go To (Ctrl+G), zoom, Find with wildcards and formatting. `.dotx` templates and New from template, `.rtf` open and save. PDF bookmarks from headings, tagged PDF, PDF/A-2b, export of a page range.
7. **Close-out.** The sweep again (with LibreOffice's text compared, as §16.18), a Mac VM check of installed Write (open, edit with each round's bars, save, reopen in a new document jail), the as-built summary, and the HANDOFF row moved to Recently finished.

**Out of scope:** master documents (§17 D17: they import as one document), document signing (waits on Sigillum), online and cloud features, co-authoring, VBA (kept inert, as now), Word 95 and older, dictation and translation, SmartArt editing (drawn from its saved picture), and input-method scripts (wait on the IME work).

Then Slides gets its own plan the same way, starting with a sweep of `.pptx`, `.odp` and `.ppt` and the presenting frame gaps on the Mac VM (84–99 ms worst, §16.38).

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

## Appendix: Sheets workstream log

Moved here from the docs/HANDOFF.md row on 2026-11-24, newest first, as written at the time. The §16 sections are the as-built record; this log keeps the dated notes, evidence and measurements that went with them.

- 2026-11-24 §16.105 Custom Sort bar (several columns by letter or header, asc/desc) and Rotation Angle bar; 134 Sheets lib tests pass.
- 2026-11-24 §16.104 border pens (line style + colour) and Rename in the Name Manager (formulas and names follow); 132 Sheets lib tests pass.
- 2026-11-24 §16.103 text rotation, thick/double/top-and-bottom borders, #RRGGBB colours in the colour bar, Clear Comments; 130 Sheets lib tests pass.
- 2026-11-24 §16.102 Delete Cells turns references to deleted cells into #REF! (clip::kill_refs); 129 Sheets lib tests pass.
- 2026-11-24 §16.101 Hidden sheets end to end (engine op + native chunk 28, .xlsx state, .ods table:display, Hide Sheet / Unhide Name, tabs and Go To skip them); officina-grid, officina-convert and 128 Sheets lib tests pass.
- 2026-11-24 §16.100 Name Manager panel (Go, Delete); Duplicate Sheet also copies conditional formats, validations and sheet names; all 127 Sheets lib tests pass.
- 2026-11-24 §16.99 Duplicate Sheet, Define Name… bar, Increase/Decrease Indent (grid draws style indents); all 126 Sheets lib tests pass.
- 2026-11-24 §16.98 Insert/Delete Cells with shift down, right, up or left; Format ▸ Custom Format… code bar; rows refit after Larger/Smaller Text; all 123 Sheets lib tests pass.
- 2026-11-24 §16.97 Format menu (font effects and sizes, text and fill colours, alignment, number formats and decimals, borders, Excel keys) with italic, underline, strike and size drawn in the grid; all 119 Sheets lib and 228 pergola tests pass.
- 2026-11-24 §16.96 filter conditions in the filter panel (compare, between, top/bottom N and %, text tests) and table Total Row on the host; all 117 Sheets lib tests pass.
- 2026-11-24 §16.95 everyday commands on the host: Remove Duplicates, Text to Columns, AutoSum (Alt+=), Clear Formats/All, Go To (Ctrl+G, F5); all 115 Sheets lib tests, officina-grid and pergola pass.
- 2026-11-24 Worksheet-tab hit testing fixed on the host (§16.71). The name and colour bar were drawn on top of the tab as siblings, so a press on them was dropped; they are now children of the tab. New `a_press_on_a_tab_label_edge_or_colour_bar_shows_that_sheet` fails on the old drawing; all 110 Sheets lib tests pass.
- 2026-11-24 FORMULATEXT brings all 80 formulas in the grouped subtotal reference back live (§16.70), including the 26 formula strings. A same-value source edit updated D2 through ordinary Save and fresh picker reopen. Row groups and hidden rows survived. Three trials matched 76 Excel caches with the four previously recorded subtotal-related differences.
- 2026-11-24 Dense crash recovery passed on a private Tessera-root desktop (§16.76). The 131,072,000-cell workbook recovered unsaved A1=8, saved the recovered copy through Scrinium, then ordinary Save and a fresh picker jail retained A1=9. Portcullis §6.5 gives new slots 3 GiB, preserves occupied legacy slots, and upgrades undersized empty slots. Tessera and UFS each held two full dense bases and the journal across detach/remount. Evidence under `/private/tmp/atrium-sheets-dense-recovery/`.
- 2026-11-24 Fixed dense desktop Open and ordinary Save (§4.2 and §16.76). Portcullis spends the picker press before the save-grant hash and gives Scrinium 60 seconds to send the grant and descriptor on that connection. Plain native snapshots stream into the sealed memory file. On Tessera the 131,072,000-cell workbook opened, saved the A1 edit through Scrinium and immediately reopened in a fresh jail with the edit and sampled values intact. A later reopen of the first published revision encountered EIO. A direct Tessera read also failed after 1,008 MiB. Forced data-zone GC reproduced the cutoff on direct staged replacements and Scrinium saves. Tessera now visits children beyond the old 63-hash limit. On a fresh arm64 clone the collector grew the stack and reclaimed packs. Both paths passed three immediate and delayed comparisons, six post-GC full-file reads each and a native reopen with the saved A1 and samples from all 125 columns. The regression is `scripts/tessera-workbook-gc-check.sh` (Tessera filesystem §11.1). Evidence is under `/private/tmp/atrium-sheets-delayed-read/final/`. Its 1.06 GB recovery base exceeded the current 128 MiB quota. Sheets displayed the failure and removed the incomplete recovery generation. A fresh dense trial verified slot reuse and a second immediate Save and reopen. Six Portcullis handoff tests and six runnable Sheets app tests passed. The two Sheets library timing failures also reproduced on unchanged main. Evidence under `/private/tmp/atrium-sheets-large-open/guest/`.
- 2026-11-24 Hardened the native loader’s memory checks for far-row slot allocation and compact formula fills (§16.76). Three Tessera-root trials each covered 131,072,000 dense cells at the 1 GiB opening floor, a 507-column sparse workbook at the guest limit, and early refusal of a 515-column workbook. Installed Sheets named the oversized file and explained the limit. Retrying from that window opened the near-limit workbook, and ordinary Save and picker reopen preserved the edit and every far-row value. All 145 host checks passed. The dense workbook hit an expired Open press in the installed picker before reaching Sheets. Evidence: `/private/tmp/atrium-sheets-memory/guest/`.
- 2026-11-24 Added LARGE, SMALL, SUBTOTAL, DATEVALUE and EOMONTH, and fixed month-header coercion (§8.2 and §16.70). Ranking matched all 36,344 Excel results, with 3,399 newly live LARGE formulas. Schedules now retains all 16,669 formulas. It matched every Excel result when the private Tessera guest ran at the saved 2021 date. At the current date its 25 year-dependent cells advanced. All four workbook checks ran three times, and installed Save and picker reopen preserved all 71,110 live results. Passed 143 grid and 85 converter tests, including subtotal visibility, nested subtotals, undo, both date systems and XLSX/ODS reopen. Evidence is under `/private/tmp/atrium-sheets-aggregates`.
- 2026-11-24 Preserved cached XLSX/XLSM values from closed workbooks (§8.2 and §16.70). Direct external formulas keep their saved results as static cells, with a bounded embedded-cell fallback. Absolute linked names with cached values become scoped constants, and local dependent formulas remain live. Ranking now preserves all 36,344 Excel results, including 25,363 linked results and 7,582 live local formulas. Skipping Excel table accessibility extensions restored all three schedules tables and reduced its cache differences from 6,663 to 527. Schedules recovered 13 linked names. All four workbooks ran three Save and reopen trials in a private Tessera guest. Installed Save and picker reopen kept all 67,684 live results. Passed 84 converter tests. Evidence is under `/private/tmp/atrium-sheets-external`.
- 2026-11-24 Added TEXTAFTER and workbook-epoch date-text coercion (§8.2). The affected Excel-saved workbook now matches all 42,042 Excel results with live formulas in every XLSX Save and reopen trial. Repeated all four workbook checks three times. The installed Save and picker reopen checks retained all 76,052 live formula results. The other three produced the same cache comparisons, with external-link and volatile-date differences still visible. Median open on the release probe and release converter took 75–1,147 ms, full recalculation 1–406 ms and encode/write/sync 47–494 ms. All 124 TEXTAFTER formula cells in three Excel-recalculated reference files matched their stored results. Evidence is under `/private/tmp/atrium-sheets-textafter`.
- 2026-11-24 Ran three Open, full recalculation, XLSX Save and reopen trials per workbook on four unchanged Microsoft Excel packages from SpreadsheetBench (§16.70). Median Open took 2.60–8.39 s, full recalculation 1–389 ms and encode/write/sync 64–286 ms in the private arm64 Tessera guest. All twelve trials preserved every live Sheets formula result on reopen. Installed Open, ordinary Save and picker reopen retained all 76,052 live formula results from the four workbooks. The raw XLSX comparison matched every Excel result in the lookup case. The three other cases exposed 42,042 TEXTAFTER errors, differences around volatile dates and external links, and 20,421 formulas kept as static values with their Excel results intact. The Sheets probe, source paths, and independent cache checker are in §16.70. Evidence is under `/private/tmp/atrium-sheets-excel`.
- 2026-11-24 Visible-first recalculation shipped in §8.3 and §16.70. The grid reports the painted cell ranges from the main, split, and frozen panes, and status shows Calculating… while work remains. Each slice can advance visible independent scalar formulas and bounded precedents in dependency order. Wide prerequisites, ordered fills, cycles and unsettled spills follow their normal evaluation path. In three live Tessera trials all 29 visible rows refreshed in one slice after each of two far scrolls, while an earlier offscreen witness stayed stale. The installed Save and picker reopen gate verified the latest edit and all 2,097,152 cached results. Passed 141 optimized grid and 227 FreeBSD Pergola tests, including shifted cross-sheet dependencies, wide aggregates, running totals in both directions, cycles, and viewport clipping. Evidence is under `/private/tmp/atrium-sheets-visible`.
- 2026-11-24 The large-workbook responsiveness gate passed in §16.70. On one 1,048,576-row workbook with 2,097,152 formulas, three paired trials per build confirmed that typing and scrolling submit scene changes while calculation is pending. Each second edit superseded the first while the tail was still stale, and every computed result matched the latest input. Partitioning only the current wave cut the median background slice from 26.16 to 14.51 ms and the median edit commit from 36.58 to 20.54 ms. With recalculation pending the updated VM saw 5.79 ms median typing and 6.06 ms median scrolling from dispatch to scene submission. The slowest updated slice took 25.17 ms against a 12 ms target. The installed Tessera desktop gate verified ordinary Save and picker reopen from a new document jail, with every cached formula value checked before recalculation. Passed 141 optimized grid tests including 100 seeded edit histories. The debug comparison timing gate missed its limit during concurrent VM work and passed in 2.58 s with the VM shut down. Evidence is under `/private/tmp/atrium-sheets-responsive`.
- 2026-11-24 Formula correctness and recalculation shipped for conditional totals and array shape handling in §8.2–8.3. SUMIF and AVERAGEIF track the effective value rectangle, including named anchors and edits beyond the written target. SUMIFS and the new AVERAGEIFS propagate matched errors with equality indexes and on the walk. Empty criterion references mean zero and finite explicit blank ranges retain their shape. Array arithmetic pads unequal shapes with #N/A and ODS keeps spill errors as cached matrix members. The 19-cell fixture matched every Sheets expectation before and after a source edit in XLSX, ODS and Calc-authored XLSX. Calc 26.8.3 hard recalculation replaced the poisoned cache and agreed on 34 of 38 checkpoints. The gate records the two repeated differences in direct text SUM coercion and IFERROR at a missing position of a shorter array. Passed 141 grid tests across the selected suites, including 100 edit seeds and the 20,000-row conditional-index comparison, and all 135 converter tests. The installed XLSX and ODS formula profile passed edit, Save and picker reopen on a private Tessera-root desktop. Evidence is under `/private/tmp/atrium-sheets-formulas`.
- 2026-11-24 Grid and chart authoring shipped in §8.4 and §16.53. Find and Replace searches cell inputs and formula text by sheet or workbook, with Replace All as one validated undo step. Split views scroll each pane independently. Nested row and column outlines follow structural edits and keep collapse state in native files. Save offers a native copy for outlines on imported workbooks. Chart authoring covers series ranges, cell-linked or typed series labels, axis titles, and column, line and area combos on shared axes. XLSX preserves interleaved series order and ODS reads quoted cross-sheet ranges. ODS Save reports typed series names or category ranges it consolidates. The private Tessera-root desktop check verified Replace All and Undo, independent split scrolling, and the chart and outline controls after picker reopen. Passed 119 FreeBSD Sheets tests across four executables, 225 FreeBSD Pergola tests, and 120 grid, 65 converter and 4 chart drawing tests on the host. Evidence is under `/private/tmp/atrium-sheets-authoring`.
- 2026-11-24 Sheets print and PDF fidelity shipped in §16.79–81 and §16.93. Grid text layout (wrap, line breaks, rotations, stacked text, shrink-to-fit), cell styling and clipped pagination carry into print. Data ▸ Print with Comments… marks visible notes and threads and appends rich note text, authors and replies. Prelum keeps hyperlink colour and underline as static page content. ODS round-trips links on blank commented cells. The installed XLSX and ODS matrix in a private Tessera guest verified Save, picker reopen, four body pages with explicit repeated titles and a bounded print area, and PDFs with and without the comment appendix. Poppler checked geometry and link styling and the rendered pages were inspected. Passed 105 FreeBSD Sheets tests and 19 PDF tests, including long comment continuation, plus the focused ODS link and print fixture tests. Evidence is under `/private/tmp/atrium-sheets-print/guest/verified`.
- 2026-11-24 Workbook-wide review and three-way merge shipped in §16.68. Take and Take All apply tab additions, removals and native-identity renames. Merge applies independent cell, axis and workbook-part changes in one undo step, and groups conflicts by sheet and type with both versions in the selected detail. Native worksheet identities retain renames and references through reopen. A same-name addition from both branches with different contents stays pending for manual reconciliation. The 1,200-conflict regression checks collapsed groups, partial style overlap, Take and Keep, undo and native Save and reopen. All 119 grid and 104 FreeBSD Sheets tests passed. The native desktop gate verified CSV review and merge through native Save As and picker reopen. The installed XLSX and ODS gates on the same private Tessera guest verified independent values, formulas and row heights, competing cells and fills, Undo and Redo of the fill, ordinary Save, picker reopen, and hashes of the unchanged source copies. Evidence is under `/private/tmp/atrium-sheets-review/guest`.
- 2026-11-23 Sheets interoperability closeout passed in a private Tessera-root guest with current Scrinium. The installed appearance, legacy and delimited profiles covered XLSX, XLSM, ODS, XLS, CSV and TSV through Save or native Save As and picker reopen. The new Import Notes dialog exposes all import notices. Verified the second XLSM note and inactive-macro label, legacy read-only and unsupported-merge notices, bounded-row-style notices, Windows-1252 conversion and values-only Save in CSV and TSV. The UI screenshots, saved workbooks, and checks are in `/private/tmp/atrium-sheets-agent/interop-closeout.HYpk5Y/verified`. Earlier work draws and retains rotated and stacked text, shrink-to-fit, diagonal borders, patterned and bounded linear fills, and a bounded set of blank cells covered by row defaults. The private Tessera-root `appearance` gate checked XLSX, XLSM, ODS and native Save As from BIFF8. Positive colour and geometry probes passed and the styled regions matched pixel for pixel after picker reopen. VBA bytes and the original XLS were unchanged. Path gradients, styles on distant blank cells, and ODS conversion of Excel fills retain fidelity notices. Evidence: `/private/tmp/atrium-sheets-agent/appearance.P4Wz6Z`. Earlier work retained ODS comment threads, including replies, authors, timestamps, resolved state and mentions, while keeping a standard note for other apps. An edited visible note takes precedence. BIFF8 imports rich notes, supported link targets, a rectangular print area and both print-title axes through native Save As. The private Tessera-root `comments_print` desktop gate exercised Save and picker reopen, confirmed the original XLS stayed byte-identical, and exported two four-page PDFs through Prelum and Scrinium. Poppler checked their print bounds, repeat bands, recalculated edit and the ODS picture. Evidence: `/private/tmp/atrium-sheets-agent/comments-print.9YBs0D`. Earlier work preserved exact Excel validation choices through ordinary Save and ODS and native round trips. BIFF8 validation survives native Save As, including multi-range comparisons, literal lists, relative custom formulas, prompts and alerts. Unsupported or malformed rules are reported. The private Tessera-root desktop `validation_legacy` gate verified allowed and rejected typing, Tabula paste and fill, undo and redo, picker reopen, and the byte-identical XLS original. See [§16.83](spec/officina.md#1683-data-validation-2026-11-18) and Mac evidence `/private/tmp/atrium-sheets-agent/validation-legacy.OBK6if`. Earlier Sheets work preserves the ODS 1904 null date, date serials and date arithmetic. Typed timestamps normalize offsets, retain subsecond values near midnight, and get a date format in General. XLSX and ODS keep saved values and array followers for unavailable direct calls. ODS distinguishes explicit error text from error values. Cell formulas and defined names using spills or structured references stay live on Sheets reopen, and the written OpenFormula remains available to other apps. An edited standard expression takes precedence over the retained formula. The installed Tessera-root desktop passed XLSX and ODS Save, picker reopen, source resize and final reopen. The spill total advanced from 21 to 33 to 46, including through the defined name. Passed 230 optimized host tests and six FreeBSD Sheets binary tests. Custom null dates outside the supported modes and dates outside the supported range carry fidelity notices. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=formulas`.
- 2026-11-23 Sheets preserves outline row fields, per-field automatic subtotals and the innermost Values row axis in XLSX and ODS. The installed Tessera-root desktop passed Save, picker reopen, source edit, Refresh, another Save and final picker reopen in both formats. Existing output stayed visible until Refresh. Previous followed the new detail order in both formats, and XLSX Running Total followed the order of a second pivot sorted by an absolute opposite-axis position. Excel scopes can also select multiple members and use the selected records for Average. Passed 125 optimized grid and 96 converter and fixture tests, including bottom subtotals, percentage formats, native persistence and undo, and the saved-cell fallback for custom subtotal functions. Relative or offset sort references and a Values field before other fields remain outside live support. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=pivot_layout`.
- 2026-11-23 Sheets retains worksheet-backed Excel discrete groups, fixed numeric bins and arithmetic calculated fields, plus chained ODS groups and automatic Sum/Count. The installed Tessera-root desktop tested XLSX and ODS through ordinary Save, picker reopen, source edit, Refresh, another Save and final picker reopen. The saved totals stayed visible until Refresh. Excel uses the source sums at each output slice to calculate ratios. ODS Refresh follows the named group chain. The optimized suites passed 125 grid and 90 converter and fixture tests, including native persistence, undo and redo, numeric bins with a pending text edit, and static fallback for cycles and unsupported formulas. Excel date and automatic bins, calculated items and formulas with unsupported functions stay static with a fidelity notice. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=pivot_sources`.
- 2026-11-23 Sheets retains ODS cross-column OR and nested AND/OR for worksheet filters and named database ranges. XLSX retains supported non-Gregorian calendar tags with Excel's Gregorian date-group evaluation, patterned fill criteria, and custom Office 2010 icon slots. Both formats passed ordinary Save, picker reopen, independent source edits, Reapply Filter, a second Save, and a final picker reopen in the installed Tessera-root desktop. Reapply kept the manually hidden control row and hid only the newly excluded rows. Patterned fills keep their Excel geometry and colors and use a solid preview in the Sheets grid. Passed 132 optimized grid tests and 98 converter and fixture tests, including native persistence, undo and redo, and nested table filters across column insertions. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=advanced_filters`.
- 2026-11-23 Sheets gives each table its own filter rules and saved row mask. The installed Tessera-root desktop passed XLSX, XLSM and ODS with two tables sharing columns and a separate worksheet filter. Ordinary Save and picker reopen kept all three rules. Editing and reapplying West changed only West. Clearing and undoing that filter left East and the worksheet filter intact, and a final Save and picker reopen kept the resulting visibility. Host checks cover overlapping row masks, structural shifts, native persistence and undo. Foreign row flags without an inferable owner stay hidden. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=table_filters`.
- 2026-11-23 Sheets preserves worksheet filters by solid cell color, cell-level font color and built-in Excel icons. The installed Tessera-root desktop passed XLSX ordinary Save and picker reopen with both 3 Arrows and Office 2010 3 Stars. Independent changes to fill, font and icon source cells left the rows visible until Data → Reapply Filter. All three edited rows became hidden, while a manually styled matching row stayed visible. A second Save and reopen kept the rules and new visibility. Passed 131 optimized grid tests and 97 converter and fixture tests, including defaults, indexed DXFs, conditional priority, extension fallback, formula thresholds, native persistence and undo. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=appearance_filters`.
- 2026-11-23 Sheets preserves Excel calendar selections through second precision and dynamic date filters, including Year to Date and months or quarters across years. The installed Tessera-root desktop passed XLSX ordinary Save and picker reopen in both the 1900 and 1904 date systems. Two source dates were then edited in separate filter columns. Saving kept their rows visible. Data → Reapply Filter hid both rows, and a second Save and picker reopen retained the rules and new visibility. The gate also kept a literal value, a blank, a selected timestamp and a manually hidden row. Passed 124 optimized grid tests and 88 converter and fixture tests, including leap days, period boundaries, stale ISO bounds, native persistence and undo. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=date_filters`.
- 2026-11-23 Sheets preserves Excel wildcard filters and ODS literal and regex text filters. In the installed Tessera-root desktop, both formats kept their rules and visible rows through ordinary Save and picker reopen. A source edit saved without changing visibility. Data → Reapply Filter hid the edited row, and a second Save and picker reopen kept the result. Rows manually hidden outside the filter stayed hidden. Host checks cover case sensitivity, escaped wildcards, ODS standard filters and partial patterns, static fallback, native persistence and undo. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=text_filter`.
- 2026-11-23 Sheets recovers a missing Excel pivot cache through the workbook cache ID and rebuilds a damaged definition when it identifies a local range, table or fixed defined name. The installed Tessera-root desktop passed ordinary Save and picker reopen for a missing direct cache and a truncated definition. A source edit left the saved North total at 9 in both books. Refresh changed it to 13, and the refreshed files reopened with that result. Passed 121 optimized grid tests and 37 converter and fixture tests. The checks include malformed shared items, an intact alternate cache with indexed filters, missing source metadata, external sources and unresolved hidden items. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=pivot_cache`.
- 2026-11-23 Sheets preserves Excel pivot value sorts by grand total and a selected opposite-axis member. The Tessera-root desktop tested both scopes on both axes through ordinary Save and picker reopen, a source edit, Refresh, a second Save and another picker reopen. Each result used the selected measure and the correct row and column orders for Previous, Next and Running Total. Passed 121 optimized grid tests and 33 converter and fixture tests, including cache-index references, nested selections, absent members, measure reordering, ascending order and native undo and redo. In the FreeBSD Sheets suite 99 passed, the known recalculation timing test was skipped, and the file-open timing test failed identically on this build and clean main. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=pivot_scope`.
- 2026-11-23 Sheets keeps XLSX manual pivot member order and ODS source, manual, label and value sort settings. The Tessera-root desktop opened both formats, performed an ordinary Save and picker reopen, edited the source, refreshed and saved again. XLSX kept its saved manual order. ODS recomputed its order by the saved measure. Both pivots recalculated Previous, Next and Running Total in the visible order. Passed 121 optimized grid tests and 28 converter and fixture tests, including ODS sorts under separate parent columns, manual positions for absent members and native undo and redo. The FreeBSD Sheets suite passed 100 of 101 tests. The deferred-recalculation timing test failed on both this build and clean main in the same guest. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=pivot_order`.
- 2026-11-23 Sheets preserves XLSX pivot display calculations including percentages, named and relative comparisons, running totals, parent totals and rankings. ODS keeps field references and named, numeric and calendar groups. The installed Tessera-root desktop passed ordinary Save and picker reopen, a source edit, Refresh and another ordinary Save in both formats. XLSX ranks changed after Refresh, and the named base still worked after the cache was rewritten. ODS kept the named groups and recomputed the grouped shares and difference. Passed 121 optimized grid tests and 24 converter and fixture tests, including native undo and redo. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=pivot_display`.
- 2026-11-23 Sheets preserves top and bottom sheet filters by count and percentage in XLSX and ODS. Data → Reapply Filter recalculates row visibility after a score edit. Count and percentage modes keep ties, and rows manually hidden outside the filter stay hidden. The installed Tessera-root desktop passed ordinary Save, picker reopen, a second edit and reapply, and another Save in both formats. The converter check covers all four modes per format. Passed 114 optimized grid tests and 22 XLSX, ODS, pivot and rank-fixture tests. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=rank_filter`. [Fidelity triage](spec/officina.md#advanced-filters-and-pivot-fidelity) records static filter visibility, pivots left as values, and pivot results that change on refresh.
- 2026-11-23 The installed Sheets desktop passed active cell and sheet protection in XLSX, XLSM and ODS. Locked typing, Delete and Fill Right were refused. Unlocked cells and a styled blank stayed editable after Save and picker reopen, with recalculated formula results and destination formatting intact. Hidden formulas stayed out of the formula bar, and ODS hidden values stayed off the grid. The source formats retained their password verifier fields and Excel permission options, with fidelity notices for the narrower edit policy and imported password locks. XLSM VBA stayed inert. Passed 101 Sheets tests, 113 optimized grid tests and four converter style tests. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=protection`.
- 2026-11-23 Sheets keeps the Excel 1904 epoch in the workbook model. Numeric dates, date arithmetic and formatting, and typed dates now share the same epoch. ISO date and timestamp cells import as numbers with their explicit or default formats. XLSX and XLSM passed typing, ordinary Save and picker reopen in the installed desktop, including leap day, a fractional timestamp, and inert VBA preservation. The XLS desktop fixture passed native Save As and picker reopen with its 1904 formula result and a byte-identical original. All 113 optimized grid tests and 11 converter round-trip tests passed. A focused converter test verified ISO input and notices for unsupported dates in both epochs. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=dates`.
- 2026-11-22 [Sheets fidelity triage](spec/officina.md#sheets-fidelity-triage) groups notices by format and user impact. The first batch fixed loss of formula-backed Excel extension validation on XLSX and XLSM import and Save. Direct cross-sheet rules write back to the Excel extension. Missing bounds and invalid ranges retain specific notices. The installed XLSX desktop kept a list sourced from another sheet through Save and picker reopen, then rejected a value outside the list and saved a valid choice. The paired ODS annotation and validation regression passed. Nine focused host tests passed, including formula bounds, multi-range rules, XLSM rules and inert VBA. The optional LibreOffice run was skipped because the host lacks LibreOffice. The installed Sheets desktop passed native Save As for mixed-feature XLSX and ODS. Each published `.osheet` included an unsaved edit from the source window, and ordinary Save wrote a second edit to the native copy. Both copies reopened through the picker with their own recalculated formulas, defined name, frozen panes, merged heading, styles, conditional format, validation, rich note and internal link. Byte comparisons confirmed both originals were intact through publication, Save and reopen. Three native codec tests passed. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=native_copy`. The installed Sheets desktop passed paired XLSX and ODS conditional formatting checks. Screen assertions confirmed the first matching rule won on overlap, and the fill updated before Save when the shared threshold or an input to a calculated score changed. Ordinary Save and picker reopen preserved the rule formulas, priority and colors. A further edit changed the reopened colors and saved successfully. XLSX import now honors priorities across range blocks, and Sheets redraws when recalculation completes in its first deferred slice. Six grid and three converter conditional formatting tests passed. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=condfmt`. The installed Sheets desktop passed a paired XLSX and ODS validation gate for typing, system clipboard paste through Tabula, and Fill Down. Stop rules rejected mixed batches as a whole and showed the rule message with the failing address. Valid paste and fill each undid and redid in one step. Ordinary Save and picker reopen preserved the values, recalculated total, and active rules with their original message. The Sheets manifest now requests its Tabula capability. All 99 Sheets unit tests and both converter validation tests passed. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=validation`. The installed Sheets desktop passed paired XLSX and ODS sort and filter round trips. Two stable desktop sorts produced Region ascending and Rank descending, with the original order retained for equal keys. The North and Open predicates kept rows 2, 4 and 5 visible, and the manually hidden row outside the filter stayed hidden. Ordinary Save and converter reopen kept the row formulas and the updated cross-sheet result. Eight focused sort and filter tests passed. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=sort_filter`. The installed Sheets desktop passed CSV and TSV round trips with UTF-8 BOM and Windows-1252 inputs. Delimiter detection now samples complete quoted records, and the picker accepts Windows-1252 text for named CSV and TSV files. All four files kept their separators, doubled quotes, embedded CRLF, and literal formula-like text. Sheets showed the Windows-1252 notice on initial Open and the format-specific values-only notice on Save. The saved UTF-8 files reopened with the calculated number and the original text. Eighteen focused tests passed. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=delimited`. The installed Sheets desktop passed the XLSX and ODS formula and date round trips. Dates, date-times, time values, named and cross-sheet references, and both array anchors survived Save and reopen. Each source reported its deliberately stale C2 cache and recalculated it. Both formats preserved the error types and recalculated the dependent value as the edit introduced and then cleared a division error. After reopen, growing the spill changed the XLSX spill sum from 33 to 46. ODS kept the three-row sum at 33 and now says that Save replaced the dynamic reference. Ten host converter and date tests and three FreeBSD Save notice tests passed. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=formulas`. The installed desktop passed XLSX and ODS Compare with and Merge Copies. A 1,202-change comparison opened as three collapsed groups across two sheets. The selected long edit wrapped in the detail pane and both conflicting versions were readable. Ordinary Save kept the independent incoming and local edits before resolution. Take Incoming retained the incoming formula and recalculated a dependent formula on the other sheet. Keep Mine retained the other local value. Both decisions survived ordinary Save and converter reopen in the original format, and the comparison, base and incoming files stayed byte-for-byte unchanged. Fifteen grid tests and three FreeBSD Sheets tests passed after the visual test was updated for the current plus markers. [Desktop review gate](../scripts/sheets-review-vm-check.sh) with `SHEETS_REVIEW_FORMAT=ods` or `xlsx`. Sheets now preserves ODS package PNG and JPEG pictures and rectangular print areas. XLSX and ODS explicit repeat bands survive Save and converter reopen and print on both axes. In the installed desktop, both four-page A4 PDFs repeated the title corner exactly once per page, showed the edited value and recalculated formula, and included the picture inside the print area. Both saved files kept the second picture, and both PDFs excluded it and the out-of-area cells. The converter test also checks JPEG bytes and geometry with blank repeat bands and the native round trip. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=print` and [PDF assertions](../scripts/sheets-interop-pdf-check.py). The previous desktop gate checked embedded pictures, print areas and repeated titles through Save, converter reopen, Prelum and the PDF save picker. XLSX kept both PNGs byte-for-byte with their anchors, the rectangular print area and the explicit row title definition. The four-page PDF kept the image inside the print area and excluded the second image and out-of-area cells. ODS reported dropped drawings on import. Save omitted the image, print area and explicit row and column title settings. Its four-page PDF included cells outside the requested area and had no picture. Both formats kept the edited value and recalculated formula. Frozen rows and columns repeated along their page axes, and the explicit titles printed once with their cells. [Desktop gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=print` and [PDF assertions](../scripts/sheets-interop-pdf-check.py). The installed Sheets desktop passed XLSM ordinary Save and reopen and XLS import and native Save As. The real VBA project bytes and workbook and sheet code names survived XLSM Save. On open and reopen, cell values matched the expected data and the status bar showed "Macros are inactive" alongside save feedback. XLS reported that merged cells were shown unmerged and opened read-only. Ordinary Save offered a copy, and Save As produced an `.osheet` that reopened with the edited value and recalculated formula. The original XLS checksum stayed the same. Fourteen focused converter and FreeBSD rendering tests passed. [Desktop interoperability gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=legacy`. XLSX and ODS annotations passed through the installed Sheets picker and converter. Both kept the edited rich note and author, retargeted internal link, external link, and validation rule with prompts and error messages. The reopened rules rejected 9 and accepted 8. XLSX kept the edited thread, reply authors and mention metadata and reported one unsupported Excel extension rule. ODS kept both messages as a plain note and now reports that conversion on Save. Six focused converter tests and four FreeBSD Sheets binary tests passed. [Desktop interoperability gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=annotations`. XLSX and ODS layout passed through the installed Sheets picker and converter. The fixture reopened with both frozen panes, a merged heading across a hidden column, wrapped text, hidden rows and columns, and the hidden axes’ stored dimensions. Dragging a visible row and column, saving, undo, redo, and a fresh reopen preserved the expected dimensions and edited cell. Screenshots confirmed the frozen bands after scrolling on both axes. Eighteen focused converter tests and fifteen Sheets tests passed. The wider FreeBSD suite passed 93 of 96 on a serial rerun, with failures in the collapsed-review label and the slow edit and load timing tests. [Desktop interoperability gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=layout`. O4d and O4e passed together in the installed Sheets desktop. For XLSX and ODS, editing the chart source updated both name scopes, the table formula, and the chart. A filter edit changed visible rows and chart categories, undo and redo restored each state, and a fresh converter reopen verified the saved results. XLSX retained structured formulas, table style metadata, and the typed chart legend. ODS kept the table and formula results using cell ranges. Sheets now reports table and chart approximations on ODS Save. Fifteen converter tests and three Sheets binary tests passed. [Desktop interoperability gate](../scripts/sheets-interop-vm-check.sh) with `SHEETS_INTEROP_CASE=tables`. O5f and O5g passed together in the installed Sheets desktop. XLSX and ODS opened through separate converter workers. Editing a source cell recalculated the spill, LET and LAMBDA and changed which cells were filled. Pivot output held until Refresh All, undo and redo reversed and reapplied the totals, and each format saved in place and reopened through the picker. All 30 focused grid and converter tests passed. [Desktop interoperability gate](../scripts/sheets-interop-vm-check.sh). Recent Workbooks ships in the Sheets launcher. Scrinium stores personal home-folder bookmarks privately and Portcullis checks native FreeBSD handles. Tessera handles survive rename and writes and rebind to the mounted volume UUID after restart. The native probe reopened after a full reboot and returned ESTALE for a byte-identical replacement. The desktop restart gate passed through Tessera and UFS nullfs home exports. It reopened the workbook moved between Documents and Downloads, refused a second workbook’s replacement, and updated the bookmark through ordinary Save. UFS Save As registered and reopened a separate copy. Scrinium passed 51 FreeBSD tests and Limen passed 3 picker tests. [Recent Workbooks gate](../scripts/sheets-recent-vm-check.sh), [native handle probe](../scripts/tessera-file-handle-check.c). Browse Versions ships in Data. Scrinium lists predecessors from its session save-grant cache while Tessera retains their generations. A host lookup verifies the existing descriptor and seals the selected historical bytes before opening a separate read-only Sheets jail. The desktop gate reopened the live workbook through a Tessera nullfs place, opened the two prior saves, and verified Save routes each historical view to a copy. Through UFS nullfs the picker showed an empty history and an ordinary Save still updated the live file. The Tessera fixture used a 128-generation retention window. [Versions gate](../scripts/sheets-versions-vm-check.sh). Desktop Compare, tracked changes, and Merge Copies share one acceptance gate through Scrinium. Compare opened 1,200 edits collapsed. Take and Take All applied 1,199 incoming values, Keep Mine retained B2, and reopening showed the saved tracked author and Before and Current values. Merge picked the base and incoming workbook, showed both conflict versions, and saved and reopened both independent edits and the chosen conflict values. Source files matched their original digests. The four review commands sit together at the top of Data, and the group markers render in the fixture font. [Desktop review gate](../scripts/sheets-review-vm-check.sh). Protected workbook recovery is complete. The desktop gate covers long names in narrow windows, password retry and cancellation, recovery ID and journal preservation, saving the plain sibling while Restore waits, queued Save retry, and publication before Limen confirms. An external revision after publication is preserved by ordinary Save, and Save As keeps the pending plain edits in a separate copy. Unlocking then restores C1=8 and saving the protected copy preserves both plain files. The gate passed end to end on UFS. Tessera verified the same sequence in two steps while the conflict screenshot was captured. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`stale-refresh interleave external-edit`). [Test results](reviews/2026-11-20-sheets-jail-recovery.md). Ordinary Save on the conflict copy retained B1=4 and added C1=7. The desktop reopen gate checks all three files through Open in fresh jails. Fixed the shared type check that refused a password-protected .osheet in the picker. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`stale-refresh interleave external-edit`). After Limen reconnected, an outside writer cleared B1 in Proof.osheet. Save showed the conflict and preserved the external bytes. The protected sibling saved independently, and Save As wrote the pending B1=4 to Proof (2).osheet. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`stale-refresh interleave external-edit`). Restarted Limen after Scrinium published B1=2 and before Sheets received confirmation. Both recovered workbooks stayed open. The sibling saved first, then an ordinary Save published the plain workbook’s later B1=4 edit through its existing grant. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`stale-refresh interleave published-save`). Interrupted a plain workbook’s pending Save before Limen delivered it to Scrinium. After reconnect, the protected sibling saved first and the plain workbook retried its original edit without re-entry. Independent reads and digest checks verified both destinations. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`stale-refresh interleave pending-save`). Restarted Limen after both recovered workbooks published their Save As copies. Both original Sheets processes and Scrinium reconnected. Four alternating ordinary saves used the expected caller UID and changed only that workbook’s file. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`stale-refresh interleave restart-limen`). Released the held recovery with the replacement sibling open and alternated four ordinary saves between both live jails. Each saved its own edits to its Save As destination and preserved the other file’s digest. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`stale-refresh interleave`). Verified that pressing Recover after a stale selection refreshes the sibling row while the first workbook is still starting. The refreshed row opens the replacement ID and saves independently. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--delayed-start-sibling`, broker path, `stale-refresh`). Changing a listed sibling’s recovery ID during a delayed launch made its stale row show the retry notice. It started no sibling jail. The first workbook recovered under its original ID, and a fresh Recover opened the replacement ID. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--delayed-start-sibling`, broker path, `stale-id`). With the first recovery held, taking the listed sibling’s image offline produced a launcher error naming the sibling. The first workbook recovered and saved while that image was still offline. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--delayed-start-sibling`, broker path, `unavailable`). The recovery chooser keeps a closed sibling selectable under the startup notice. Verified the protected sibling opens, saves, and closes while the first jail launch is held, including after refreshing Recover. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--delayed-start-sibling`). A second Recover press keeps the same named startup notice while its document process is delayed. The desktop test compares the two launcher frames and counts runtime jail starts. Pressing Recover again during the delay produced one jail and retained the original recovery ID and edits. [Delayed-start desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--delayed-runtime-start`). Recover checks that Sheets starts in the selected document jail. A refused exec names the workbook and offers retry. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--retry-runtime-start`). The Sheets launcher names the workbook and offers retry when document jail preparation fails after mounting the image. The desktop gate asserts the rendered error for both an explicit selection and the only pending recovery. Verified that a failed Sheets jail setup detaches the selected recovery image and preserves both markers. The protected sibling can save while setup is blocked, and the original workbook can retry in the same slot. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--retry-jail-setup`). Portcullis mounts the selected recovery image before acknowledging Sheets. A failed mount names the workbook in the launcher and preserves the pending recovery. [Desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--retry-unmountable-image`). Sheets reports a missing recovery image when the workbook is selected, including after the chooser cached its name. Verified the protected sibling can recover and save independently and restoring the image recovers the original edits and identity. [Offline-image desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--retry-missing-image`). Tested a missing autosave name and invalid UTF-8 with two pending workbooks. The original chooser label opened the edited workbook. After offline name repair, Recover displayed the published filename with the same recovery ID. [Desktop retry gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--retry-recovery-label`). After a missed Save As label update and reboot, the Sheets chooser repairs the name from the detached autosave. Verified two named recoveries, stable IDs, preserved edits and collision-suffixed Save As defaults on Tessera and UFS. Markers cached by the occupancy-only broker are checked on upgrade. [Two-stage chooser gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--prepare-reboot-labels`, `--verify-reboot-labels`). After a guest reboot, Sheets New reuses a cleanly closed slot once Portcullis checks its detached recovery image. A second workbook kept its recovery ID and unsaved edit. Verified on native Tessera and bounded UFS volumes, including markers written by the previous broker. [Two-stage desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--prepare-reboot-reuse`, `--verify-reboot-reuse`). Sheets document jails now end on clean close across broker restart. New reused the closed slot with a fresh recovery ID and an empty workbook while a sibling retained its edits. The gate also covers upgrading a legacy persistent jail. [Clean-close desktop gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--clean-close-reuse`). Sheets Recover reclaims mounts left by a broker restart while a workbook was open. The workbook accepted another edit after restart. After its crash, Recover detached the volume, displayed the published name and restored both workbooks with their own edits. [Live restart gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--live-broker-restart`). Portcullis reconciles missed Save As labels when a document jail releases its recovery volume. The isolated desktop reproduced a stale Proof.csv entry before the fix. With the worker socket unavailable during publication, the corrected chooser showed Proof (2).osheet after broker restart on both Tessera and UFS. Both recovered copies retained their edits. [Interrupted-label gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--interrupted-labels`). Sheets recovery labels follow confirmed Save As names. Two jails opened Proof.csv and published Proof.osheet and Proof (2).osheet. After both crashed, the chooser, recovered edits and next suggested filenames matched each copy. Cancel preserved the old labels and both recovery IDs stayed stable. [Desktop recovery gate](../scripts/sheets-recovery-choices-vm-check.sh) (`--save-as-labels`). The Sheets launcher offers a choice when several workbooks await recovery. Canceling the password prompt releases the slot and preserves the pending workbook. Tested both recovery orders, rejection of a stale selection, Save As and subsequent Save through the new grant. [Two-workbook desktop gate](../scripts/sheets-recovery-choices-vm-check.sh). The earlier Sheets launcher change replays a pending workbook inside a fresh document jail. Dedicated recovery volumes survive teardown. Verified the edited cell, denial from a live sibling, explicit discard and an intact source CSV on a private Tessera-root desktop. [Desktop recovery gate](../scripts/sheets-jail-recovery-vm-check.sh). [Results and original reproduction](reviews/2026-11-20-sheets-jail-recovery.md). A committed unsaved edit reached the journal. Document jail teardown removed its tmpfs, and a fresh launcher reopened the original CSV with an empty journal. Prior [Initial autosave profile and recovery](reviews/2026-11-20-sheets-autosave-base.md). Eight measured Opens per source filesystem. Serialization and buffering took 168 ms at the median, writes 23–24 ms, and syncs under 0.01 ms at the median on the document jail’s tmpfs upper layer. All 18 live generation copies recovered byte-identical CSVs. Prior [Sheets Open stage timings](reviews/2026-11-20-sheets-open-stages.md). Eight warm Opens per filesystem. From load completion to populated scene submission, UFS took 229 ms and Tessera 232 ms at the median. Initial autosave setup accounted for 190–191 ms, and sending the batch took 2.8 ms. First visible cells followed submission by 392 ms on UFS and 370 ms on Tessera in the paced headless compositor. Prior [desktop Open through first workbook cells](reviews/2026-11-20-sheets-desktop-open.md). Eight warm opens per filesystem in a private headless desktop VM, each through a fresh document jail. Median picker confirmation to first values was 562 ms on UFS and 1,001 ms on Tessera. Both columns appeared at 1,002 and 1,001 ms respectively. Seven UFS opens showed the numeric column a frame earlier. Verified the selected filename, document PID/JID and rendered cell pixels. Prior [first Open after remount](reviews/2026-11-20-sheets-remount-open.md). Twelve pairs per filesystem on published 64 MiB workbooks. First/warm Open medians were 197/193 ms on UFS and 291/269 ms on Tessera. Tessera’s paired cost was 18.5 ms, mostly in grant creation. Guest device counters recorded about 64–65 MiB on first Open and zero reads on every warm reopen. All parsed CSVs matched. Prior [consecutive Tessera saves](reviews/2026-11-20-scrinium-consecutive-saves.md). A published 64 MiB workbook saved twice on the same grant in all 12 paired rounds. With the 1 MiB buffer, median Save times were 364 and 379 ms. The second Save improved over 64 KiB in every pair, by a median 100 ms. Both complete on-disk revisions matched on every run. Prior [1 MiB workbook verification reads](reviews/2026-11-20-scrinium-verification-buffer.md). Eight paired runs per filesystem and size. On a 64 MiB Tessera workbook the larger buffer saved 539 ms on Open and 532 ms on Save. Median UFS changes stayed within 1.1 ms. All 51 Scrinium tests passed on each filesystem, including refusal of a same-size external edit in the short final chunk after its mtime was restored. Tessera retained both revisions. Prior [Tessera verification read profile](reviews/2026-11-20-scrinium-tessera-reads.md). Across 12 measured passes per case, a freshly written 64 MiB CSV spent 555 ms in 64 KiB positioned reads and 55 ms in BLAKE3. The production grant took 610 ms. The same bytes after Sheets Save took 123 ms to read and 56 ms to hash. Read-only 1 MiB requests took 36 and 43 ms respectively. Prior [workbook verification measurements](reviews/2026-11-20-sheets-verification.md). Twelve warm-cache pairs per filesystem and size compared release builds from before and after verification. At 16 MiB, median paired Open/Save overhead was 17/30 ms on UFS and 54/64 ms on Tessera. At 64 MiB it was 55/121 ms on UFS and 722/904 ms on Tessera. Every saved CSV matched the expected one-cell edit byte for byte. Prior UFS save grants. Reproduced the second-save refusal on run 19 on a private soft-updates UFS volume through nullfs. Scrinium now verifies the pinned workbook bytes and records the staged replacement digest before rename. The interrupted-overwrite and consecutive-save regressions passed together 1,000 times on direct UFS and 1,000 through nullfs as nobody. An external in-place edit with the original size and restored mtime was refused and its exact bytes survived. All 51 Scrinium library tests passed on UFS and Tessera through nullfs. The Tessera retained-generation test also passed. Prior Limen recovery. Open Sheets windows reconnect and discard interrupted picker and print requests. In a private Tessera-root VM, stopped Limen with the Save As picker open after entering B1=7, entered C1=8 during the outage, then restarted Limen. The original document (PID 3174, JID 2) saved Personal.osheet through a new picker. Reopening the copy in JID 3 showed A1:C1 = 2,7,8. The source CSV checksum stayed identical. Prior Tessera-root save gate. Verified / was Tessera on the private guest. A jailed Sheets Save changed Personal.csv from 11 to 2. Generations 148–150 held 11, generations 151–154 and the live file held 2. The pre-rename fsync and post-rename directory sync use native Tessera generations directly and through nullfs, subject to retention. The FreeBSD integration test tessera_workbook reads the complete old and new bytes from retained history. All 49 Scrinium tests passed on the Tessera-backed nullfs grant, including interrupted overwrites. The unchanged UFS control had one false stale-grant rejection on the second save; ten immediate focused retries passed. The UFS save-grant fix above resolves this. Prior close-during-save gate. Sheets waits for the active Save when the window is closed or Ctrl+W is pressed. In a private FreeBSD desktop VM, a matching confirmation closed the document, a later edit survived repeated close requests, and a missing confirmation kept the edited workbook open. Tested Scrinium loss before publication and Limen loss after publication, plus Save As recovery after Scrinium restart and native reopen. The gate scripts/sheets-pending-close-vm-check.sh exercises the picker loss, late edit and confirmed close. Scrinium began syncing both directories after rename. All 96 Sheets and 49 Scrinium library tests passed. Prior desktop gate. Fixed ordinary Save swallowing grid input while awaiting Scrinium. In an isolated arm64 FreeBSD VM, opened Budget.csv via the launcher into a document jail. Paused Scrinium, saved A1=4, and typed B1=5 before releasing the service. The first acknowledgement left the title edited and only 4 on disk. The second Save wrote 4,5. Restarted Scrinium and edited C1=6. Save reported an expired grant and preserved the CSV. Save As created Budget.osheet, a subsequent Save added D1=7, and opening it in a fresh jail showed 4,5,6,7. Scrinium-managed Sheets Save and Save As publish sealed snapshots atomically. Save rejects stale grants and preserves edits made after staging. On arm64 FreeBSD the interrupted writer left the original intact, including under an unprivileged UID; reopen, two later saves, stale windows, replaced destinations and Save As name collisions were exercised. Passed 96 Sheets library, two Sheets launch, 49 Scrinium and 21 Limen tests. Repeated the unprivileged interrupted overwrite and consecutive saves 50 times. Compare with… shows worksheet tab order as one collapsed workbook entry. Taking it reorders shared sheets by name and preserves the slots of local-only tabs. Take All applies tab moves last and undoes together. Kept edits, formulas, merged ranges, and filters follow their sheets. All 96 Sheets library tests on FreeBSD and 110 grid unit tests passed, including Take, Keep Mine, Take All, save/reopen, undo, and stale tab order. Compare with…, Review Tracked Changes and Merge Conflicts open with collapsed groups by sheet and change type. Expanding renders only nearby items. Selecting a change opens its untruncated summary and available version details in a scrollable section. Conflicts show Current and Incoming separately. A 10,001-change app fixture starts with three summary rows and verifies expansion, virtual rendering, final-line scrolling, and Keep Mine. Cell and structural conflict tests verify both versions in the pane. All 95 Sheets library tests on FreeBSD and 109 grid unit tests passed. The FreeBSD build check passed. Compare with… reviews sheet tab colors, frozen panes with their scroll origin, and auto-filters with stored hidden rows. Each setting can be kept or taken independently during bulk row replacement. The FreeBSD app regression verifies reordered sheets, Take, Keep Mine, Take All, save/reopen and undo. All 94 Sheets library tests on FreeBSD and all 109 grid unit tests passed, including clearing settings and rejecting stale review entries. Compare with… flags merged ranges, row heights and column widths, including axis defaults and remembered hidden widths. Take and Keep Mine are independent, and Take All preserves kept layout and undoes together. Inserted rows bring explicit heights and merges wholly inside them. Overlapping incoming merges stay pending when the local merge is kept. Native sheets reopen zero-width hidden columns. All 93 Sheets library tests on FreeBSD and all 108 grid unit tests passed. Compare with… flags added, removed and changed comments and cell hyperlinks on aligned rows, including blank cells. Take preserves rich comment formatting and complete thread metadata, or copies the stored link target. Inserted rows bring their annotations. Keep Mine preserves annotations during Take All, and accepted changes undo together. The FreeBSD Sheets suite passed all 92 library tests, and the grid suite passed the alignment, inserted-row, undo and stale-sheet checks. Compare with… flags added, removed and changed pictures by sheet name and top-left cell. Take and Take All preserve encoded bytes, anchor geometry, crop and rotation, and Keep Mine survives picture index shifts. Take All respects workbook picture limits and undoes together. All 91 Sheets library tests on FreeBSD and all 105 grid unit tests passed. Compare with… flags tables and pivot tables by name, including additions, removals and changes with reordered sheets. Take and Take All regenerate pivot output from the current source, clear removed pivot output, and leave overlapping tables or occupied pivot destinations pending. Keep Mine preserves the selected objects and pivot totals, and Take All undoes as one step. All 89 Sheets library tests on FreeBSD, 104 grid unit tests, and seven pivot integration tests passed. Whole-sheet row replacements on pivot-output sheets remain pending in bulk review. Compare with… flags changes to conditional formatting and data validation as separate ordered rule sets per sheet. Take remaps formula references by sheet name, Keep Mine survives a refresh, and Take All preserves kept sets and undoes as one step. Verified all 88 Sheets library tests on FreeBSD and the grid rule-reference regression. The Sheets launcher and workbook windows route New and picker Open through Portcullis to a separate document jail. Scrinium sends a picker-selected descriptor directly to the broker with its own confirmation press. Save As now creates an .osheet through the system picker and keeps the returned read-write descriptor for Save. In an isolated headless Atrium VM, the desktop launcher opened New in JID 2 and its workbook window opened New in JID 3. Set A1 to 111 and 222 and saved both via the on-screen picker. Reopened the first from the launcher with Open and the second from the first workbook with Enter. Verified both cells on screen and both fresh jails live together (JIDs 4 and 5, distinct UIDs and roots). An in-place Save from the second reopened workbook changed only its file. Focused Scrinium and Limen regression tests passed. Compare with… flags static style runs including styled blank cells, charts, and workbook or sheet-scoped defined names. Take, Keep Mine and Take All work for all three. Chart and name references remap by sheet name, and bulk takes preserve kept entries and undo together. Taking inserted rows brings their styles, and shifts also move styled blank cells outside the data extent. Verified 85 Sheets tests, 102 grid tests, and the arm64 FreeBSD build check. The prior batch shipped rich comment clipboard transfers. The temporary X11–Tabula bridge was solely for the Calc test. Sheets has no bridge dependency and the project has no plans to ship one.
