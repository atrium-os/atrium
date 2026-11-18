# Fonts Atrium ships

Every file here is an unmodified upstream release, under a licence that lets
us ship it with the system. Each folder holds its licence next to the fonts.
None of them is GPL, LGPL or AGPL, and there are no Microsoft or Apple fonts.
How they're installed, checked and matched is in `docs/spec/fonts.md`.

| Folder | Families | Licence | Source |
|---|---|---|---|
| `ibm-plex/` | IBM Plex Sans, IBM Plex Mono (the system typeface) | OFL 1.1 (`OFL.txt`) | see `ibm-plex/README.md` |
| `phosphor/` | Phosphor icons | MIT (`LICENSE`) | phosphor-icons |
| `liberation/` | Liberation Serif, Sans, Mono 2.1.5 | OFL 1.1 (`OFL.txt`) | see `liberation/README.md` |
| `carlito/` | Carlito | OFL 1.1 (`OFL.txt`) | `github.com/google/fonts` `ofl/carlito` |
| `caladea/` | Caladea | OFL 1.1 (`OFL.txt`) | `github.com/google/fonts` `ofl/caladea` |
| `ibm-plex-serif/` | IBM Plex Serif | OFL 1.1 (`OFL.txt`) | `github.com/google/fonts` `ofl/ibmplexserif` |
| `noto/` | Noto Sans, Noto Serif, Noto Sans Mono, Noto Sans Symbols and Symbols 2, Noto Sans Math, and the script families below | OFL 1.1 (`OFL.txt`) | `github.com/notofonts/notofonts.github.io`, `fonts/<Family>/unhinted/ttf/` |
| `source/` | Source Sans 3, Source Serif 4, Source Code Pro | OFL 1.1 (`LICENSE-*.md`) | `github.com/adobe-fonts/source-sans`, `source-serif`, `source-code-pro`, branch `release`, `TTF/` |
| `fira/` | Fira Sans, Fira Mono | OFL 1.1 (`OFL.txt`) | `github.com/google/fonts` `ofl/firasans`, `ofl/firamono` |
| `roboto/` | Roboto 2 | Apache 2.0 (`LICENSE`) | `github.com/googlefonts/roboto`, `src/hinted/` |
| `inter/` | Inter 3.019 (Regular, Bold) | OFL 1.1 (`OFL.txt`) | npm `inter-font` 3.19.0 `ttf/`, a repackaging of the upstream build: the name table reads "Version 3.019;git-0a5106e0b", Rasmus Andersson |

Noto Emoji 3.006 (`NotoEmoji-Regular.ttf`, `NotoEmoji-Bold.ttf`): the
monochrome outline emoji, static instances of Google Fonts' build as carried
by the npm package `@expo-google-fonts/noto-emoji` 0.4.8 (name table:
"Version 3.006", "Copyright 2013 Google LLC", OFL 1.1), files renamed only.

Noto script families: Devanagari, Bengali, Gurmukhi, Gujarati, Oriya, Tamil,
Telugu, Kannada, Malayalam, Sinhala, Thai, Lao, Khmer, Myanmar, Georgian,
Armenian, Hebrew, Ethiopic, Thaana, Syriac, Cherokee (Noto Sans), Tibetan (Noto
Serif) and Arabic (Noto Naskh Arabic). Each has Regular and Bold, except Syriac,
Symbols, Symbols 2 and Math, which are Regular only. Every Noto font carries
its own copyright line in its name table ("The Noto Project Authors").

## Not here, on purpose

- **Arimo, Tinos, Cousine** (Apache 2.0): Liberation 2 is built from the same
  designs with the same metrics, so they would be second copies.
- **CJK** (Noto Sans/Serif CJK): about 6 MB a weight as static TrueType.
  Installed on demand as font packages (`docs/spec/fonts.md` §5).
- **Colour emoji**: the drawing path has no colour glyph tables yet; Noto
  Emoji (outlines, in the text's colour) stands in.
- **Liberation 1.x**: GPL with a font exception. 2.x is OFL.
- Variable-only releases (Gelasio, current Roboto and Inter): the library maps
  static files, so a family comes in only when it ships static TTFs.

## Updating a font

Replace the file, then regenerate `atrium-fonts/src/catalog.rs` (family from
the name table, weight and italic from OS/2, BLAKE3 of the file). The test
`catalog_matches_files` fails on any row that no longer matches its file.
