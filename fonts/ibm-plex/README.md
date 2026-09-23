# IBM Plex — Atrium system typeface

The Atrium visual language (`docs/design/atrium-visual-language.md` §2) commits to
**IBM Plex Sans** (UI) and **IBM Plex Mono** (code/terminal) as the single type-family
pair across the whole OS.

## License

IBM Plex is licensed under the **SIL Open Font License, Version 1.1** (`OFL.txt`).
Copyright © 2017–2019 IBM Corp. The OFL governs only these font files, imposes no
obligation on Atrium's code, and is fully compatible with Atrium's permissive-runtime
charter (it is not GPL/LGPL/AGPL). The files here are **unmodified** upstream releases,
so the OFL Reserved Font Name clause does not apply.

## Files

- `IBMPlexSans.ttf` — IBM Plex Sans, **variable** font (weight + width axes; default
  instance is Regular 400). One file covers the 100–700 range the design uses.
- `IBMPlexMono-Regular.ttf`, `IBMPlexMono-Bold.ttf` — static IBM Plex Mono instances.
- `IBMPlexSans-Italic.ttf` — IBM Plex Sans Italic, **variable** (upstream
  `IBMPlexSans-Italic[wdth,wght].ttf`); one file covers italic 400 and 700.
- `IBMPlexMono-Italic.ttf`, `IBMPlexMono-BoldItalic.ttf` — static Mono italics. Both are
  needed: Mono has no weight axis, so instancing the regular italic at 700 returns the
  SAME bytes — a "bold" that is not bold, and the canonical address says so.
- `OFL.txt` — the license + IBM copyright (ship alongside the fonts).

## Source

Fetched from the Google Fonts OFL tree (`github.com/google/fonts/ofl/ibmplexsans`,
`.../ibmplexmono`), which mirrors the upstream IBM release (`github.com/IBM/plex`).

## Install / wiring

Installed in the VM at `/usr/local/share/fonts/ibm-plex/`; the fresco scene-server
font registry (`fresco-scene-server/src/text.rs`) resolves `system-sans` →
`IBMPlexSans.ttf` and `system-mono` → `IBMPlexMono-Regular.ttf` (DejaVu kept as
fallback). The Pergola `system-sans`/`system-mono` aliases therefore render Plex.
