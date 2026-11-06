# Lyra: HRTF datasets and their licences (for S3)

Status: survey, 2026-11-06. Feeds [lyra-spatial.md](lyra-spatial.md) S3, the HRTF upgrade
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

Licences were checked against the publisher's own pages where search reached them. The
cloud session can't fetch york.ac.uk, zenodo.org, sound.media.mit.edu or Debian's
metadata, so the rows marked "to confirm" need a look at the download itself.

| Set | What | Licence | Fit |
|---|---|---|---|
| **FABIAN** (TU Berlin) | Head-and-torso simulator, very dense grid, several head-above-torso orientations, SOFA | **CC BY 4.0** since the 2020-01-22 version (earlier versions were CC BY-NC-SA 4.0). The download also has an item-specific `license.txt`: to confirm | **Best generic candidate**: dummy head, attribution only |
| **HUTUBS** (TU Berlin) | 96 people (with head meshes), SOFA | **CC BY 4.0** (repository record and the documentation agree) | Good for later personalisation |
| **3D3A** (Princeton) | People plus scans, SOFA, still growing | **CC BY 4.0** (lab's database page) | Good for personalisation |
| **SONICOM** (Imperial) | 300 measured + 200 synthetic people, SOFA at 44.1/48 kHz | **MIT** per the Extended SONICOM paper (2025); the 2023 original's data licence was not stated: to confirm on the download page | Most permissive, biggest; personalisation |
| **SADIE II** (York) | KU100 and KEMAR dummy heads + 20 people, WAV/SOFA | Copyright University of York, with Apache-style disclaimer. The first SADIE was **Apache 2.0**; SADIE II changed its licence metadata in v1.3/1.4 (2020): to confirm in the SOFA `GLOBAL:License` field | Strong if Apache 2.0: has a KU100 |
| **TH Köln KU100** (Bernschütz) | KU100 on a full-sphere Lebedev grid, far field (about 22,000 HRIRs) and near field | Creative Commons; the related headgear set is **CC BY-SA 4.0**. The far-field set's exact variant: to confirm | Excellent data, but probably share-alike |
| **ARI** (Austrian Academy of Sciences) | More than 250 people, 1550 directions, SOFA | **CC BY-SA 3.0** | Share-alike |
| **IRCAM LISTEN** | 51 people | "Public and available for any use", © IRCAM 2002, acknowledgment requested (quoted by third parties) | Permissive, but old and small |
| **MIT KEMAR** (Gardner & Martin, 1994) | KEMAR dummy head, 710 directions | Custom terms (Debian ships it as "mit-kemar"); free to use with acknowledgement, but not a standard licence | Old; weak low end (small speaker). Avoid as a default |
| **CIPIC** (UC Davis) | 45 people | Conflicting: announced as public domain in 2001, but the files carry "© 2001 Regents of the University of California. All Rights Reserved" | Avoid |

## Recommendation

1. **Ship FABIAN (CC BY 4.0) as the default HRTF**, resampled to 48 kHz, trimmed to the
   tap count the partitioned convolver needs, with attribution in the system's
   acknowledgements. First read its `license.txt` to confirm there is no extra term.
2. **If SADIE II's SOFA files say Apache 2.0, its KU100 is the alternative.** The KU100 is
   the usual reference head for binaural rendering. Compare the two by listening, and pick
   one.
3. **Avoid share-alike (TH Köln, ARI) and unclear terms (MIT KEMAR, CIPIC)** for anything
   we ship.
4. **Later, personalisation:** let the user pick a closer match from SONICOM (MIT) or
   HUTUBS/3D3A (CC BY), downloaded on demand rather than shipped.

To finish the licence checks from the cloud, allow zenodo.org, york.ac.uk and
depositonce.tu-berlin.de in the environment's network settings, or check them from the Mac.
