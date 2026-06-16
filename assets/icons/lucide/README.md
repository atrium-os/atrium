# Lucide icons — Atrium's vector icon set

The Atrium visual language is vector-native (`docs/design/atrium-visual-language.md`
§7: "crisp at any DPI", "1px logical strokes", "real curves"), so icons are SVG
vectors too — resolution-independent, rendered through Fresco's scene graph like
everything else.

## Why Lucide

[Lucide](https://lucide.dev) (the community successor to Feather) is the best fit:
clean, geometric line icons on a 24px grid with a uniform 2px stroke and rounded
caps — exactly the "calm, confident, geometric clarity" the visual language calls
for, and pure stroked `<path>` data (no fills, no raster) that maps straight onto a
vector renderer.

## License

**ISC** (`LICENSE`) — a permissive, MIT-equivalent license. It governs only the icon
files, imposes nothing on Atrium's code, and is fully compatible with the
permissive-runtime charter (it is not GPL/LGPL/AGPL). Files are unmodified upstream.

Considered alternatives, all also permissive (kept here for the record): Phosphor
(MIT, 6 weights), Tabler (MIT), Heroicons (MIT), Material Symbols (Apache-2.0).

## Files (the dock set)

Renamed to their dock role; each is an unmodified Lucide icon:

| file | Lucide source | role |
|---|---|---|
| `editor.svg`   | `file-pen-line` | text editor |
| `terminal.svg` | `terminal`      | terminal |
| `files.svg`    | `folder`        | files |
| `settings.svg` | `settings`      | settings |
| `browser.svg`  | `globe`         | browser |

## Source

`github.com/lucide-icons/lucide` (`icons/*.svg`).
