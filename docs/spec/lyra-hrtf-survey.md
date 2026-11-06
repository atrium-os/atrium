# Lyra: HRTF datasets and their licences (for S3)

Status: survey, 2026-11-06; licences re-checked at the source the same day (the sources
are named in each row). Feeds [lyra-spatial.md](lyra-spatial.md) S3, the HRTF upgrade
for headphone virtual surround (S1) and object audio (S2).

## What we need

- **Redistribution inside Atrium:** we ship the data (or a resampled, trimmed derivative)
  in the OS image. The licence must allow redistribution and modification, including
  commercial use.
- **One good generic set:** a dummy head (KU100, KEMAR, FABIAN), measured densely, at
  48 kHz or resampled to it. A choice among many people (personalisation) comes later.
- **No share-alike if we can avoid it:** CC BY-SA would make our processed set BY-SA too.
  That is workable for a data file, but it adds an obligation that we don't need.

## The sets

Each licence below was read from the data file's own SOFA `License` attribute, the
repository's record, or Debian's copyright file (which ships samples of several sets in
`main`, so Debian judged them free). Where two sources disagree, the stricter one counts.

| Set | What | Licence | Fit |
|---|---|---|---|
| **FABIAN** (TU Berlin) | Head-and-torso simulator, very dense grid, several head-above-torso orientations, SOFA | **CC BY 4.0**: the DepositOnce record's `dc.rights.uri`; its version history says "Changed license to CC-BY" (earlier versions were CC BY-NC-SA 4.0). The item's `license.txt` is the depositor's submission agreement, not terms for users | **Generic candidate**: dummy head, attribution only |
| **HUTUBS** (TU Berlin) | 96 people (with head meshes), SOFA | **CC BY 4.0** (repository record and the documentation agree) | Good for later personalisation |
| **3D3A** (Princeton) | People plus scans, SOFA, still growing | **CC BY 4.0** (lab's database page, via search; the site refuses automated fetches) | Good for personalisation |
| **SONICOM** (Imperial) | 300 measured + 200 synthetic people, SOFA at 44.1/48 kHz | **MIT** per the Extended SONICOM paper (2025). The download server (transfer.ic.ac.uk:9090) isn't reachable from the cloud, so the files' own field is unchecked | Most permissive, biggest; personalisation |
| **SADIE II** (York) | KU100 (D1, 8802 directions, diffuse-field equalised, 48 kHz 256-tap SOFA ready to use), KEMAR (D2) and 18 people | **Apache 2.0**, confirmed: Zenodo record v2-2 licence field, and the D1 48 kHz SOFA file's `License` attribute ("Copyright 2018, University of York, Licensed under the Apache License, Version 2.0") | **Best generic candidate**: KU100, permissive, already at 48 kHz |
| **TH Köln KU100** (Bernschütz) | KU100 on full-sphere grids (Lebedev 2702, Gauss 2°), far field, near field, and a headgear set | **Conflicting, so treat as CC BY-SA 3.0**: the Zenodo records say CC BY 3.0 (far field) and CC BY 4.0 (near field, headgear), but the SOFA files themselves say "CC 3.0 BY-SA", and Debian lists the far-field set as CC-BY-SA-3.0 | Excellent data, but share-alike unless the authors confirm CC BY |
| **ARI** (Austrian Academy of Sciences) | More than 250 people, 1550 directions, SOFA | **CC BY-SA 3.0** (database pages, via search; they have since moved) | Share-alike |
| **IRCAM LISTEN** | 51 people | "The Listen database is public and available for any use"; an acknowledgment is appreciated (Debian `listen-ircam`) | Permissive, but old and small |
| **MIT KEMAR** (Gardner & Martin, 1994) | KEMAR dummy head, 710 directions | "Provided free with no restrictions on use, provided the authors are cited when the data is used in any research or commercial application" (Debian `mit-kemar`) | Permissive (cite the authors); old, weak low end (small speaker) |
| **CIPIC** (UC Davis) | 45 people | © 2001 UC Regents; "permission to reproduce and/or use materials … for any purpose — educational, research or commercial", keeping the copyright notice; written acknowledgment requested for commercial products (Debian `cipic`) | Usable, but a custom licence with a notice-and-letter request |

## Recommendation

1. **Ship SADIE II's KU100 (D1, Apache 2.0) as the default HRTF.** The licence is confirmed
   in the file itself, Apache 2.0 needs only the notice in the system's acknowledgements, the
   KU100 is the usual reference head for binaural rendering, and the 48 kHz, 256-tap,
   diffuse-field-equalised SOFA file fits the partitioned convolver as it is.
2. **FABIAN (CC BY 4.0) is the second candidate**: compare the two by listening before S3
   ships, and keep whichever sounds better outside the head.
3. **Avoid share-alike sets (TH Köln as its files are labelled, ARI)** for anything we ship.
   MIT KEMAR, LISTEN and CIPIC are usable but older and weaker; no reason to prefer them.
4. **Later, personalisation:** let the user pick a closer match from SONICOM (MIT) or
   HUTUBS/3D3A (CC BY), downloaded on demand rather than shipped.
