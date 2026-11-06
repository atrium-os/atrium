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

## Quality

For a generic (not personal) HRTF, every set gives some front/back and elevation
confusion, because it isn't the listener's ears. What separates good generic sets is
clean measurement: a dense and even grid, an extended low end, a low noise floor, a
symmetric head, and true far-field distance. We measured the files we could download
(scratch script, 2026-11-06; left ear, source in front, levels relative to 1 kHz):

| | SADIE II KU100 (D1) | SADIE II KEMAR (D2) | TH Köln KU100 far field (L2702) |
|---|---|---|---|
| Directions | 8802; very dense horizontal ring, worst gap elsewhere 7.5° | same grid | 2702, even Lebedev grid, worst gap 2.6° |
| Distance | 1.2 m (slightly near field for lateral sources) | 1.2 m | 3.25 m (true far field) |
| Length at 48 kHz | 256 taps (5.3 ms) | 256 taps | 128 taps (2.7 ms) |
| Low end (30 / 100 Hz) | +1.0 / +0.7 dB (low-frequency extended) | +1.5 / +1.1 dB | +0.3 / +0.2 dB |
| Tail relative to peak (noise and windowing) | −45 dB | −43 dB | −48 dB |
| Left/right mirror difference, 0.2–12 kHz | 3.7 dB rms | 2.1 dB rms | 2.7 dB rms |
| ITD at 90° (low-passed cross-correlation) | 810 µs left, 789 µs right | 737 / 768 µs | 771 / 766 µs |
| Front/back spectral difference, 4–12 kHz | 5.8 dB rms | 6.0 dB rms | 6.4 dB rms |
| Processing | Diffuse-field equalised, low end extended | same | Raw; diffuse-field filter supplied |

- **TH Köln's KU100 is the cleanest measurement:** an even grid, far field, flat low end,
  lowest tail. But its files say share-alike.
- **SADIE II's KU100 is good but less even:** its grid is dense in the horizontal plane and
  coarser elsewhere, which interpolation handles. It is 1.2 m away, and its left/right
  mirror error is the largest of the three (a KU100 is built symmetric, so this is
  measurement asymmetry). Interpolation needs either set to be resampled to an even grid.
- **FABIAN** couldn't be measured here: its downloads come from api-depositonce.tu-berlin.de,
  which the environment doesn't allow yet. From its paper (Brinkmann et al., JAES 65(10),
  2017): 11,950 directions at about 2°, measured and numerically simulated, the two
  cross-validated with auditory models and found to agree, plus 11 head-above-torso
  orientations (±50°), useful when head tracking comes. On paper it is the best
  documented of the permissive sets.
- **Listening studies:** the SADIE II evaluation (Armstrong et al., Applied Sciences 8(11)
  2029, 2018) found that some non-individual sets were preferred over listeners' own
  measurements. A student thesis (Politecnico di Torino) preferred the TH Köln KU100 far
  field at 128 taps; treat that as preliminary. None compares FABIAN with the KU100.
- **Headphone equalisation matters as much as the set:** without it, any HRTF sounds
  coloured. S3 should ship a diffuse-field target and let the output carry a headphone
  correction.

## Recommendation

1. **Ship SADIE II's KU100 (D1, Apache 2.0) as the default HRTF.** The licence is confirmed
   in the file itself, Apache 2.0 needs only the notice in the system's acknowledgements, the
   KU100 is the usual reference head for binaural rendering, and the 48 kHz, 256-tap,
   diffuse-field-equalised SOFA file fits the partitioned convolver as it is.
2. **FABIAN (CC BY 4.0) may be the better measurement** (even 2° grid, cross-validated),
   so S3 loads any SOFA file and we compare SADIE II KU100 and FABIAN by listening on
   headphones before choosing which one to ship.
3. **Avoid share-alike sets (TH Köln as its files are labelled, ARI)** for anything we ship.
   MIT KEMAR, LISTEN and CIPIC are usable but older and weaker; no reason to prefer them.
4. **Later, personalisation:** let the user pick a closer match from SONICOM (MIT) or
   HUTUBS/3D3A (CC BY), downloaded on demand rather than shipped.
