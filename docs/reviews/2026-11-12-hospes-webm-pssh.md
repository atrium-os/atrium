# Hospes: Widevine WebM through the manifest's PSSH (2026-11-12)

This was an open item from H4 (`docs/reviews/2026-11-12-hospes-h4.md` §7): "a manifest-aware path
(the page or a DASH reader supplying the PSSH) for WebM-only content".

**Done.** Shaka's angel-one stream was played in the FreeBSD arm64 VM through the player as `nobody`,
with Widevine installed from Google's arm64 Chrome package. The VP9 WebM video and the MP4 video
now both play to the end with the Opus WebM audio:

| Clip | Frames | Last frame | Audio frames | Licence requests |
|---|---|---|---|---|
| `v-0144p-0100k-libx264.mp4` | 1,500 (1,473 distinct) | 59.96 s | 2,880,648 | 1 (200) |
| `v-0144p-0100k-vp9.webm` | 1,500 (1,473 distinct) | 59.96 s | 2,880,648 | 1 (200) |

In both runs the worker was sandboxed and no `hospes-cdm` process was left afterwards.
Before this work, H4 played the MP4 and made a second request, from a bare key id, which
the proxy refused with a 500. That request is gone now.

## Why WebM needed this

A WebM file names only a key id for each encrypted track; it has no place for a `pssh` box. Shaka's
licence proxy refuses a request made from a bare key id. Shaka's own player takes the Widevine
`pssh` from the DASH manifest (`<ContentProtection schemeIdUri="urn:uuid:edef8ba9-…"><cenc:pssh>`)
and passes it to EME as `initData`. Our player now accepts the same thing.

## What changed

- **`atrium-media-demux::dash`** (new) scans a manifest for the `<cenc:pssh>` of one DRM system.
  - It is not an XML parser, and it treats the manifest as untrusted.
  - The manifest is capped at 4 MiB, each box at 16 KiB, and at most 8 distinct boxes are kept.
  - Each box must be a whole `pssh` (version 0 or 1) for the system asked, with its size fields
    consistent. Anything else is dropped.
  - The fuzz target is `atrium-player/fuzz` `dash`. It ran clean for 1 h (14.1 M runs), and the weekly CI fuzz
    job now runs it too.
- **The player** (`CdmOptions::init_data`) sends these boxes to the CDM first, as `cenc`.
  - Two switches feed it:
    - `ATRIUM_PLAYER_CDM_PSSH`: base64 boxes, comma-separated, as a page would pass them.
    - `ATRIUM_PLAYER_CDM_MPD`: a manifest file.
  - Init data from the media that is identical to a box already sent is not sent again.
- **Held key-id init data** now waits until the licence's keys have arrived.
  - Before, it was sent as soon as nothing was outstanding. That happened between a session's
    creation and its licence request, so every WebM track made a doomed key-id request.
  - Now it waits for a licence and then a key change, or 2 s after the CDM resolved the update.
  - Once the key is usable, the held init data is dropped.

## What the first WebM run found

The licence came back, and then the video stalled for 70 minutes with nothing busy. Two causes:

1. **The VP9 profile was misread from WebM's `CodecPrivate`.**
   - WebM's `CodecPrivate` is a feature list (`01 01 00 02 01 0a …`), but `cdmvideo.rs` read it
     as MP4's `vpcC` (byte 4 = profile).
   - So the player decided the CDM could not decode this VP9 and fell back to decrypt-only.
   - Widevine answers decrypt-only *video* with "no key", even once the key is usable (audio
     decrypts fine), so the video waited for a key forever.
   - Fix: `vp9_profile` reads both forms. Its `vpcC` check is the three zero flag bytes; its WebM
     check is ID 1 in the feature list.
2. **Nothing ended the wait.**
   - The worker now fails encrypted video that still has no key 30 s after the keys last
     changed, with `the CDM still has no key`, rather than hanging.

`ATRIUM_PLAYER_TRACE_CDM=1` prints the player's sessions, licences, key changes and the worker's
no-key waits. `scripts/hospes-h4-vm.sh` passes it on with `TRACE=1`.

## The check

```
SRC=/home/user/freebsd-src ARCH=aarch64 sh scripts/hospes-h4-vm.sh   # TRACE=1, VIDEOS=…, PLAY_TIMEOUT=…
```

It now plays both clips by default. The WebM run is given the manifest (`dash.mpd`, downloaded with
the media); the MP4 run plays from the media's own `pssh`. Each run is bounded by `PLAY_TIMEOUT`
(default 900 s).

Harness fixes found on the way, all caused by the cloud disk filling up:

- **The kernel step (`hospes-k1-vm.sh kernel`)** now only runs `nextboot` when the guest already
  has this build's `kernel.atrium`. It recognises the build by a stamp file holding the kernel's
  sha256. This saves staging about 500 MB on the host.
- **The bundle copy** is checked: the Chrome package's sha256 in the guest must match the index.
  A full host disk once cut it short with no error.
- **A full host disk pauses qemu.** The VM's monitor reports `info status` as "paused
  (io-error)". Free space, then `cont`.

## Not covered

- **The amd64 VM.** It was not rerun for WebM. Nothing in the change is architecture-specific.
- **Fetching the manifest itself.** The player takes a file, or the boxes. A page or app that
  fetches the manifest through Nuntius passes what it found, as EME does.
- **The real CDM is not in CI.** Unchanged: Shankar's decision, manual until the licence.
