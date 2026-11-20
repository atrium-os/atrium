# Navigator live code: the island engine (tier 4)

Status: **accepted 2026-11-19** (Shankar: all §9 decisions as recommended; D1 and D3 as amended). Shankar asked for site
scripts to run live in the jailed environment (2026-11-19) and chose to plan first, then
build. Sign-in is the second case that needs it (Shankar, same day): single sign-on pages
(Okta, Entra ID) and WebAuthn with fingerprints and security keys run scripts, so the VPN's
SSO sign-in (vpn.md §9.3 and §14 V4, "sign-ins that need scripts wait") and Mail's Microsoft account
wait on this engine. This spec turns the settled design into build steps. It decides nothing that
[atrium-navigator-legacy-web.md](atrium-navigator-legacy-web.md) §8.1 and §11 already
settled, and only names what those sections left open.

## 1. Where things stand

**What runs today.** A page's scripts run once, at conversion, in prerender's one-shot jail
(Boa and our minimal DOM, legacy-web §5.3 tier 2). The person reads the result. Nothing
runs while they read: no script runs at view time anywhere in the Navigator.

**What is settled** (legacy-web §8.1, Shankar 2026-11-04 and 2026-11-13):

- The default shape is a **document with jailed islands**. The document lane serves the page,
  and only a region that needs an engine runs live, in a jail scoped to its origin.
- Islands start from embedded frames (D14, list-free): the normalizer turns each
  content-sized `<iframe>` into a box labelled with its real origin. **Nothing is contacted
  until the person presses it.**
- On a press it becomes a **Live code** island: its own jail, a throwaway cookie jar, the
  amber light, and a frame and tag ("live code · media.example") that frescod draws.
- The document drives an island only through a **typed channel** (for video: play, pause,
  seek, current time). The island never sees the document's DOM.
- A whole page goes live only when it is a fat client throughout (§11.8.3: Figma, WhatsApp
  Web, editors, banking).
- Engine: **Ladybird in I0**, Servo with SpiderMonkey deferred (§11.3, §9 D1), **JIT off** (§11.5), **session recycling**
  (§11.9.1), **in a jail on this machine** (§11.9.3: never an operator's rendering proxy).
- Video plays in **Limen's native player**. The engine's `<video>` and `MediaSource` are
  backed by a `media-player` embed (§8.1, mapping built in `navigator-media`, §8.1.1). The
  engine never decodes.

**What is built around the engine** (navigator-ui §14 step 6, 24/24 in
`scripts/island-probe-vm.sh`):

- frescod's `ISLAND` slot kind takes no pointer input or keys while anything covers it.
- Limen has the `web-island` role (provider `org.atrium.navigator.engine`, slot `Island`).
- The press rules (500 ms still and uncovered) and the tier chip's words are in place.
- The normalizer's island boxes are in place.

**What is missing:** the engine itself. Nothing produces islands, so the Navigator has no
`web-island` provider. The spec deferred the engine until the corpus measured the need
(legacy-web §5.4), and no step was planned after the measurement.

## 2. The engine jail

One jail per island, as the person's own uid for that persona (the page children's
per-instance worker rule), started by limend for the `web-island` role.

| it has | it does not have |
|---|---|
| its island window (frescod, an `ISLAND` slot in the UI's window) | the document, its DOM or its recording |
| one Nuntius fetcher of its own, for that origin, with a throwaway jar (§3) | the person's cookies, Arca, files or other personas |
| the typed island channel to navigatord (§5) | any socket of its own: Capsicum after start-up |
| a `media-player` embed per `<video>`, through Limen | decoders (they stay in the player's jail) |
| memory and CPU limits (rctl) | a GPU device (threat model §2.5.1), at first |

- **Network** goes only through Nuntius, as every fetch in Atrium does. Servo's own network
  layer is replaced at its fetch boundary by a Nuntius client, so the address rules (no
  private or loopback addresses), pins and the per-origin jar apply as for any page.
  Third-party requests the island makes follow legacy-web §5.4.1c.
- **Cookies**: a jar that lives as long as the island and is never written down. Signing in
  inside an island (the jar kept per persona, §7.7) is a later, separate decision.
- **Drawing**: for an island or whole-page app, a painter inside the confined engine
  translates its display list into native retained Fresco scenes on its assigned
  embed connection. Fresco owns composition, clipping to the granted area and the
  trusted chrome. Stable node identities, pinned glyph runs and scroll groups
  preserve incremental updates. Bound raster leaves to the affected content
  (for example a canvas), and charge their textures to the same client. Hostile
  commands, resource handles and damage hints must be validated by the receiver.
  The engine has no GPU descriptor, window-management grant or cross-window readback.
- **Input**: frescod gives the island pointer and keys only while it is unobscured. The
  engine receives them as ordinary events.
- **Lifetime**: the island ends when its page leaves the trail's view, when the person
  stops it, or at a memory or time limit (session recycling: §11.9.1). It restarts from the
  same address on the next press.
- **Fork before taint** (§7.5): later, an engine zygote per persona that forks an island
  before it loads any page, so islands start fast without sharing a session.

## 3. Fetching

The island's fetcher is a Nuntius instance started by portcullisd, as for the sign-in
window, for the island's origin only. Every request carries the island's own origin as the
initiator. The jar is in Nuntius, never in the engine (threat model §6: no engine holds a
cookie). The engine sees responses, not `Set-Cookie`.

## 4. What the person sees

As navigator-ui §6.5 already specifies:

- The box reads the frame's origin with the registrable domain emphasised, and a press
  starts it.
- While it runs, the tier chip reads "Replayed · 1 live part" and the island carries
  frescod's frame and tag.
- A **Stop** control on the frame ends the island and leaves the box.

Whole-page live code (§9 D5) would use the chip's **Live code** state, already drawn.

## 5. The typed channel

Between navigatord (for the document) and the engine, on the island's Limen connection.
Bounded frames, a fixed message set, fuzzed, and the document side never trusts a value
from the island (threat model §2.9).

- First set: `Started { origin }`, `Title`, `Ended { why }`, `Resize { w, h }` (the island
  asks; the document lane decides).
- Video adds `Play`, `Pause`, `Seek { s }`, `Time { s, duration }`, so chapter links in the
  document can seek a YouTube island.

## 6. Media

The engine's `HTMLMediaElement` and `MediaSource` bind to `navigator_media::mse::Media`
(built and tested against a fake player, legacy-web §8.1.1). Segments come through the
island's fetcher and their bytes are appended to the native player. The player decodes,
presents and draws its controls in its own jail. Any media backend Servo brings with it is
left out of the build (its usual one is GStreamer, which is LGPL).

## 6a. Sign-in: SSO and WebAuthn

An identity provider's page is a whole page of script: a redirect chain, a password step,
push approval, then a passkey or a security key. Islands don't fit it, so sign-in runs the
**whole page live** in the engine jail, in a **sign-in window**. The Navigator already has
windows of this kind (the Wi-Fi sign-in window; the VPN's `vpn` persona window), each its
own Navigator instance in a reserved persona. What changes is that its pages run in the
engine rather than in the document lane:

- **Fetching goes through Arca's couriers** in the window's own jar (vpn.md §9.3 and §14 V4: one
  partition per sign-in, `vpn-<profile>.<site>`), never a throwaway Nuntius jar. The jar
  is what keeps the person signed in next time. Cookies stay in the courier, and the
  engine never sees `Set-Cookie`. Declared token fields (the VPN gateway's
  `acSamlv2Token`) go to Arca and never reach the page.
- **WebAuthn**: the engine's `navigator.credentials.create` and `get` cross the typed
  channel to navigatord as requests, never as results. navigatord is the WebAuthn client
  (sigillum.md §9.1). It knows the origin from the courier's TLS endpoint, not from the
  engine. It checks the RP ID, builds `clientDataJSON` with `sigillum-webauthn` (built,
  S3a) and asks `sigillumd`. Sigillum's sheet takes the press, the fingerprint or the
  key's touch (`sigillum-ui`, passcodes never reach the Navigator). The engine gets back
  the `PublicKeyCredential` that navigatord packaged. A compromised engine can ask for an
  assertion for its own origin only, and only with the person's press on Sigillum's sheet.
- **Passwords** are typed into Arca's secure field (arca.md), not the page's own field,
  where Arca has an item for the site.
- **The window ends** at the flow's final address (the VPN's assertion consumer service,
  an OAuth redirect URI a native app registered) or after ten minutes, as the Wi-Fi
  sign-in window does.
- **Everyday browsing** keeps signing in through the document lane's forms where a site
  allows it. A site whose sign-in needs script opens a sign-in window for it on the
  person's press, rather than turning the persona's whole trail live.

## 7. Build steps

Each step runs in the VM with a gate before the next, as the Navigator's other steps did.

1. **I0. Qualify Ladybird on required pages** (D1). Test Ladybird on
   FreeBSD (amd64 and arm64), using its embeddable engine, JIT off, with its network
   and media layers stubbed. Use native Fresco scenes and Atrium's pinned fonts
   (`navigator-fontset`). Desktop shells and their graphics or system font-discovery
   stacks are outside the comparison. The qualifying run uses a real Atrium jail
   with Capsicum entered and only the fixture broker and display capabilities.
   Use these workloads:
   - the fixture site and the [whole-page editor](../../test-assets/navigator-i0/editor.html)
     with a native insertion caret, an editable paragraph and an opaque scroll viewport;
   - three corpus widgets (an Infogram chart, a map, the YouTube embed);
   - two scripted sign-in pages (a fixture identity provider, and a real one's page as far
     as its password step).
   For Ladybird, measure:
   - correctness against a reference rendering;
   - start time and memory;
   - binary and build size;
   - the build's cost in disk, memory and hours;
   - FreeBSD patches needed;
   - how far its `MediaSource` is from what the YouTube player needs;
   - how its network and media layers can be replaced (§3, §6).
   Record the results here. Reopen the Servo port if a required page fails in upstream
   Ladybird and a reduced case works in upstream Servo.

   **Editor measurement.** Use the same pinned fonts, 1× device scale, window size,
   initial selection and quiescent baseline in each arm. Focus line 4 before
   `beta`, observe a native blink transition, type a real lowercase `x` through
   the focused island, then scroll the viewport down 24 CSS pixels. Record the
   observed selection, text and offset. Capture each interaction independently
   of coincident blink or scrollbar animation. Repeat from a fresh fixture at
   least three times. Record engine paint work/time, Fresco node sets, clears
   and placements, serialized bytes, damage bounds, tiles rasterized, and any
   raster-leaf bytes. Compare the presented region to a full redraw of the same
   state and caret phase. A changed screenshot alone cannot supply these counters.
   Check capability mode in each page-handling process, confirm the permitted
   display and fixture endpoints work, and exercise denied direct file/network
   access and cross-slot operations. Verify JS and Wasm JIT are disabled.

2. **I1. One live island.** The `org.atrium.navigator.engine` bundle as Limen's `web-island`
   provider:
   - in its jail, under Capsicum after start-up;
   - fetching through its own Nuntius with a throwaway jar;
   - drawing into its island window and taking input from frescod.
   Gate on the confined script's fixture network: a fixture page embeds a fixture widget (a
   counter with a button). Pressing the box starts it, and clicks count. A click while
   another window covers it does not count. The island can reach neither the fixture's
   private addresses nor the document. It ends with its page.
3. **I2. Channel, label and lifecycle.** The typed channel, the tier chip's live part, Stop,
   and session recycling (a memory limit and an idle restart). Fuzz the channel.
4. **I3. Sign-in windows run live** (§6a), ahead of video because the VPN and Mail wait on
   it. The engine serves a whole page in a sign-in window, fetching through Arca's couriers
   in the window's jar. WebAuthn bridges to navigatord and Sigillum. Gates:
   - A fixture identity provider whose sign-in needs script (a redirect chain, a scripted
     password step, then WebAuthn `get` with the soft key through Sigillum's sheet)
     completes. The gateway token lands in Arca, not the page.
   - With the VPN thread: `VpnSignInWindow` against its SSO gateway in the VM.
   - On the Mac VM: a real YubiKey, then a fingerprint where the hardware has one.
5. **I4. Video: the YouTube prototype** (legacy-web §8.1). `MediaSource` goes to the native
   player. The document's chapter links seek through the channel. The gate is a real
   YouTube embed, which needs the open internet (see §8).
6. **I5. Hardening and the zygote.** JIT-off verified in the binary, rctl limits, crash
   handling (an island that dies leaves its box and a note), and fork-before-taint start-up.
   Then measure the time to first frame.
7. **I6. Whole-page live code** for fat clients in everyday browsing, if §9 D5 says so.

## 8. Costs and risks, stated

- **Servo is the largest dependency Atrium would take,** and SpiderMonkey is C++ with a
  tracing collector (§11.9). The jail, JIT off and recycling bound the harm. They don't
  remove it.
- **Build host.** A Servo build won't fit this cloud workspace (about 2 GB free). I0
  cross-builds on the Mac (D3), as `.cargo/config.toml` already does for the rest of Atrium
  with clang and a FreeBSD sysroot:
  - The Rust crates cross-compile like ours.
  - The risk is the C and C++ underneath. SpiderMonkey has its own build system.
    Servo's stock FreeBSD font backend depends on Fontconfig even when building the
    embeddable library alone. Adapt its font enumeration and fallback to the pinned
    font set before testing it. FreeType and HarfBuzz are separate font-processing
    dependencies to assess at that boundary. Ladybird uses CMake with vcpkg and needs
    a FreeBSD toolchain file and the pinned fonts and a retained-scene painter.
  - Cross-build on the Mac. Use the VM to run the resulting arm64 binaries.
- **FreeBSD support.** Servo targets Linux, macOS, Windows and Android. FreeBSD may need
  patches, upstreamed where we can.
- **Compatibility.** Servo renders fewer sites correctly than Blink. Islands are small
  regions, which helps, but YouTube's player may need APIs Servo lacks. I0 measures this
  before we promise it.
- **Real sites in gates.** I0 and I3 need the open internet in the VM. The hermetic fixture
  network covers I1 and I2.

### I0 progress

- Servo `c6a16079c28c` on the Mac with its pinned Rust 1.99.1 toolchain.
  The stock `servoshell` FreeBSD arm64 cross-build stopped at the missing
  `fontconfig.pc`. An embeddable `servo --lib` cross-build with default
  features off, `bundled` enabled and the stock Atrium sysroot reproduced
  the Fontconfig failure. No engine binary or runtime measurement resulted.
  Its `SoftwareRenderingContext` opens a Surfman connection and creates a software
  OpenGL adapter. CPU readback depends on establishing that context inside the jail.
- The shared [scripted sign-in fixture](../../test-assets/navigator-i0/sign-in.html)
  tests two navigations, a rejected public fixture password, and a successful
  callback. Its script passed a local smoke check with mocked DOM and navigation.
  Ladybird passed the jailed, JIT-off gate below. The equivalent Servo run is deferred.
- Ladybird at `ead47ae6ef846b910ac509f51b12281523d7c2c3` (source inspection).
  WebView's FontService scans installed directories. LibGfx's SharedFontProvider
  already accepts a mapped catalog and brokered face descriptors. Supply pinned
  face IDs, bytes, family and code-point fallback to both content and compositor.
  Replace the provider's automatic resource-font load too. HarfBuzz shapes the
  text and the compositor can construct a Skia typeface from those bytes.
  The FreeBSD CMake and vcpkg profiles require changes to omit Fontconfig,
  the desktop UI, GPU dependencies and bundled media.
- LibWeb records a display list. The compositor plays it through Skia and its
  CPU backing store wraps bitmap memory with an explicit pitch. This supplies a
  pixel reference and a possible bounded raster leaf. I0 targets a native
  retained painter on the island connection.
- At this pin Ladybird's JS lexer, parser, bytecode compiler and VM are in
  `Libraries/LibJS/Rust`. Its C++ API uses the embedding ABI. The hot dispatch loop
  is generated from `interpreter.flap` into target assembly at build time.
  `ENABLE_JS_JIT=OFF` controls LibJS. Upstream LibWasm still depends on its own JIT
  compiler, although its runtime OS/CPU allowlist already excludes FreeBSD arm64.
  The [arm64 port](../../patches/ladybird/freebsd-arm64-wasm-interpreter.patch)
  omits that compiler, uses the base FreeBSD reservation flags and reads the arm64
  saved PC for interpreter memory traps.
- On the Mac, [the component build](../../scripts/navigator-i0-ladybird-build.sh)
  generates the C++ and Rust Web IDL bindings from the upstream IDL inventory
  and cross-builds the Rust LibJS, LibWeb layout/paint, compositing and bindings
  libraries. The bindings' normal target dependency tree excludes both JIT crates.
  The linked editor measurements below use this interpreter configuration.
  On the base arm64 sysroot the upstream GUI-off CMake configure reached its
  required JPEG decoder after staging mimalloc, FastFloat, LLVM libc headers,
  simdutf, zmij and HarfBuzz. Its global checks also require media, Fontconfig
  and ANGLE. Separate those from the editor target, with image and media
  requests crossing their Atrium service boundaries.
- Component probes on the arm64 VM (FreeBSD ABI 1600030) used signed Portcullis
  one-shot jails. The entries verified jail membership, uid 1011, Capsicum mode 1
  and `ECAPMODE` on a global pathname open. A LibJS frontend probe read the
  editor over stdin and compiled its 408-byte script into 297 bytes of top-level
  bytecode with the JIT feature absent. This used LibJS’s Cargo test host and
  did not execute the script against a DOM. Two upstream LibCompositing tests
  passed for replacing a chunk between unchanged neighbors and changing a scroll
  offset. The interpreter-only Wasm test trapped an out-of-bounds load and then
  returned 42 from an interpreted addition in the same store.
- Editor fixture control on macOS, headless Chrome with `--jitless`, 640 × 360
  at 1×, three fresh profiles. All three gave the same screenshot differences.
  The native blink changed 19 pixels within 1 × 19. Typing `x` before `beta`
  on line 4 changed 814 pixels within 106 × 19 and moved the selection from
  offset 17 to 18. A wheel event moved the viewport by 24 CSS pixels and changed
  2,435 pixels within 410 × 183. These counts include the host's scrollbar
  rendering. The Fresco measurements below count scene operations and raster tiles.
### Ladybird editor on FreeBSD arm64, 2026-11-19

The Mac linked the [LibWeb host](../../navigator-ladybird/EditorPage.cpp) with
the generated bindings, ICU, and one Rust archive containing the interpreter,
layout, recording and [Fresco painter](../../navigator-ladybird/painter.rs).
The target dependency graph excludes the JS and Wasm JITs. Portcullis verified
the bundle manifest. The editor checked jail membership, uid 1011, Capsicum
mode 1 and an ambient pathname open returning `ECAPMODE` on FreeBSD
16.0-CURRENT arm64 `main-n290542-cfbf11308b58`.

Limen grants a separate embed connection per web island. The measurement
launcher also transfers a preconnected Nuntius fixture channel. LibWeb's
ResourceLoader fetched the 1,003-byte document through that channel and
verified it against the checked-in fixture before parsing. The broker serves
one fixed GET. The page ran its script, produced 128 editable paragraphs and
placed the caret at offset 17. The [pinned font provider](../../navigator-ladybird/PinnedFonts.cpp)
exports the canonical SFNT bytes for the typeface used by LibGfx. The painter
reads the published glyph positions, clips, caret and scroll state and sends
native nodes to the assigned Fresco window. Each renderer runs in its own
Portcullis jail. Unsupported paint operations fail the fixture.

Three fresh private displays at 640 × 360 and 1× gave identical results.
Bytes count outbound Aqueduct envelopes and payloads on the editor's display
connection, including content-addressed resources. Tiles count unique 16 × 16
raster tiles in the damage plan, including exposed scroll strips. The verification
redraw is excluded from that count.

| Frame | Changed nodes | Wire bytes | Raster tiles |
| --- | ---: | ---: | ---: |
| Initial scene | 136 | 4,469,466 | 920 |
| Caret hidden | 1 | 102 | 2 |
| Caret restored | 1 | 102 | 2 |
| Focused `x` press and release | 2 | 19,091 | 204 |
| Wheel scroll, 24 CSS pixels | 3 | 560 | 35 |

Blink samples two phases of LibWeb's recorded caret clock. Keeping its scene
slot while hidden preserves sibling paint order. Fresco dispatched the key and
wheel through the placed island. The editor verified the changed text and caret
offset 18, resolved the wheel hit against the recorded scroll tree, and verified
`scrollTop == 24`. Every incremental image matched a full redraw byte for byte.
The key touched 204 of 920 tiles even though it changed only two nodes.

**Reproduction.** The Mac staging directory is `/private/tmp/atrium-i0`.
Use the pinned source in `ladybird-full` with the patches in `patches/ladybird`.
The stage contains generated headers and Ninja inputs in `page-headers` and
`cpp-engine`. The editor-specific C++ units use `ATRIUM_I0_EDITOR`.
Build the CMake targets `font-check` and `editor-host` in `font-rebuild`
and build its staged painter manifest with `--features editor` for
`aarch64-unknown-freebsd`. Link the staged objects with
`python3 scripts/navigator-i0-editor-link.py /private/tmp/atrium-i0`.
Build Limen's `editor_fixture` example, headless frescod and the split renderer
with the `i0-raster-check` renderer feature. Copy host-built executables to
guest-local storage. The [guest gate](../../scripts/navigator-i0-editor-gate.sh)
takes the display staging directory and signed editor bundle. On the test guest
these are `/tmp/i0display.UYjBD3` and `/tmp/navigator-i0-editor.UreA3F`.
The stripped editor SHA-256 was
`33aab8426708d239b582e42ab4e0800f8c155625205c28a06935a3c7fea38f10`,
and the renderer was
`378e34febe2dba007b6bb5b5b907dc1c3b2aa36d83d536500c15b7873453914c`.

The common sign-in results follow the Tessera editor baseline. Production per-origin
fetch delegation, credential mediation and broader paint coverage remain open.
This gate measures work per interaction. It does not measure latency.

### Editor glyph damage follow-up, 2026-11-20

Inserting the first `x` on line 4 replaced the shaped glyph run and moved the
caret. Pergola rasterized a new 11 × 9 coverage glyph at atlas position (254, 0).
The old slot update sent nine complete 2,048-byte rows, or 18,432 raw bytes.
The packed update sends the 99 coverage bytes and occupies 119 bytes on the
wire. A trace of the resulting frame counted 514 bytes for the shaped run, 50
for the caret, 42 for their placements and 31 for frame boundaries and present.

The compute packer expands the run into individual glyph operations. The old
tile signature mixed the entire atlas into each glyph, so adding `x` changed
all signatures covering visible text, a 17 × 12 tile rectangle starting at the
origin. The new signature hashes the glyph's atlas rectangle including the
bilinear sample halo, shared by repeated uses within the frame. The two
remaining damage rectangles in half-open tile coordinates are (10,4)–(13,5)
and (10,5)–(17,6). They cover the caret and shifted letters on line 4.

| Arm64 Capsicum gate | Key nodes | Key wire bytes | Key raster tiles |
| --- | ---: | ---: | ---: |
| Original editor and whole-atlas damage | 2 | 19,091 | 204 |
| Packed atlas update alone | 2 | 756 | 204 |
| Packed update and glyph-local damage | 2 | 756 | 10 |

Three trials per arm used an HVF clone of the same FreeBSD arm64 guest,
kernel `main-n290542-cfbf11308b58`, and the same signed, JIT-off fixture.
Every incremental frame matched a full redraw. Final blink and restore each
changed one node, sent 102 bytes and touched two tiles. Scrolling 24 pixels
changed three nodes, sent 560 bytes and touched 35 tiles. Initial upload
fell from 4,469,466 to 4,265,693 bytes for 136 nodes and 920 tiles.
Final stripped editor SHA-256 was
`bdc19692da477979fe57d496de64854e704399bccabd76cc9f6e2bf354398a3e`,
renderer `8c0abdf21b92802dcfb5d6dfe5421871227b499dae656190450bb68ce5317f5f`.
These are work counts at 1×. Interaction latency remains unmeasured.

### Tessera-root arm64 HVF baseline, 2026-11-20

The host ran `qemu-system-aarch64 -accel hvf -cpu host` with four vCPUs and
4 GiB RAM. UEFI loaded the Atrium kernel and matching Tessera module from a
separate boot disk. In the running guest, `mount -p` reported
`/dev/vtbd1p1 / tessera rw`. The editor bundle also resolved to this mount.
The kernel was `16.0-CURRENT atrium-os-9f2eed03a36c` (ATRIUM, arm64).
The powered-off Tessera disk was the read-only backing file for a fresh
qcow2 overlay. A second boot used a new overlay over the retained base
(SHA-256 `f9a327dfaff407d7ce0e85f7ccad9273a43ef1611c9d2b093b2179d89d46f6a1`).
Each boot ran three independent editor launches with private displays. The editor and renderer SHA-256 values matched the glyph damage
follow-up above.

| Frame at 640 × 360, 1× | Changed nodes | Outbound wire bytes | 16 × 16 raster tiles |
| --- | ---: | ---: | ---: |
| Initial scene | 136 | 4,265,693 | 920 |
| Caret blink, each phase | 1 | 102 | 2 |
| Focused `x` press and release | 2 | 756 | 10 |
| Wheel scroll, 24 CSS pixels | 3 | 560 | 35 |

All six trials gave these counts, matching the UFS fixture baseline. Each
incremental raster matched its full redraw. Each signed editor verified uid
1011, jail membership, Capsicum mode 1 and `ECAPMODE` on ambient open. It
verified the broker's 1,003-byte response, caret offset 18 after inserting
`x`, and final scroll offset 24. The console emitted a WITNESS lock-order
warning involving nullfs over Tessera and unionfs over tmpfs during jail
setup. All trials finished and `jls` showed no remaining jails. Shutdown
synced all buffers. These trials measured rendering work, not elapsed time.

The Mac keeps the private base and evidence under `~/atrium-vm/navigator-i0-tessera`
(`root.sha256`, `gate.log`, `snapshot.txt`, `hvf.txt` and
`boot-measurement.log`, plus `replay-gate.log` and `replay-boot.log`). To repeat from a fresh disk snapshot after reserving
local SSH port 24223, use a new directory on the same APFS volume:

```sh
base="$HOME/atrium-vm/navigator-i0-tessera"
run=$(mktemp -d /private/tmp/i0-tessera-run.XXXXXX)
cp -c "$base/firmware-boot.qcow2" "$run/firmware-boot.qcow2"
cp -c "$base/vars.fd" "$run/vars.fd"
chmod 600 "$run/firmware-boot.qcow2" "$run/vars.fd"
cp "$base/boot" "$run/boot"
qemu-img create -f qcow2 -F raw -b "$base/root.raw" "$run/measurement.qcow2"
sh "$run/boot" measurement.qcow2 qcow2 measurement
```

Check the live mount before running the guest-local
`/tmp/i0display.UYjBD3/navigator-i0-editor-gate.sh /tmp/i0display.UYjBD3 /tmp/navigator-i0-editor.UreA3F`.
Power off the private guest cleanly after collecting the gate output.

### Ladybird brokered sign-in on Tessera, 2026-11-20

Three trials used a fresh copy-on-write snapshot on the shared arm64 HVF VM.
The guest reported FreeBSD 16.0-CURRENT kernel `9f2eed03a36c` and a
read-write Tessera root on `/dev/vtbd1p1`. Portcullis verified the bundle
manifest. Each engine reported uid 1011, jail membership and Capsicum mode,
and an ambient path open returned ECAPMODE. The cross-build's runtime Cargo
dependency tree excluded both the JS and Wasm JITs.

The preconnected Nuntius fixture broker served four ordered GETs to
`https://idp.example/sign-in.html` with successive query strings of
(empty), `?step=authorize`, `?step=password` and `?step=callback`.
Each response supplied the same 1,381 bytes, SHA-256
`8784c98941dba0864feb51e90878d36a7e856607697676d3ab1095659b15d733`.
The fixture host followed the top-level navigation requested by the running
page into a fresh LibWeb document on the same Limen embed. The driver set
the real password control and clicked the submit button. The wrong value
showed the refusal and issued no request. The correct public fixture value
cleared the field and reached the scripted completion message. JITless
Chrome reproduced the same DOM outcomes. Production origin grants, Arca
credential delivery, cookies and an external IdP are outside this fixture.

| Scene at 640 × 360 | Changed nodes | Fresco wire bytes | Rasterized tiles |
|---|---:|---:|---:|
| Password form, initial scene including resources | 10 | 4,203,510 | 920 |
| Password refused | 8 | 1,927 | 32 |
| Callback complete | 10 | 1,468 | 86 |

These counts repeated in all three trials. The first trial also rasterized
8 tiles again for the initial commit. Frame serials identify this additional
work independently of the refusal. Every incremental raster matched a full
redraw pixel for pixel.

| Trial | Form (ms) | Refusal (ms) | Callback (ms) | Engine peak RSS (KiB) |
|---|---:|---:|---:|---:|
| First on this boot | 418.852 | 494.273 | 536.986 | 82,816 |
| Second | 107.407 | 175.581 | 262.770 | 83,664 |
| Third | 127.450 | 169.703 | 210.238 | 83,340 |

Times are cumulative from the parent requesting the embed through observing
the corresponding renderer report. They include IPC, polling and the gate's
raster checks. The stripped engine was 164,979,848 bytes, SHA-256
`8129d28604d927ae47bc5f0e01c02c1ef18098b75526f40e2501018f3219f894`.
The renderer SHA-256 was
`55dd86e14daa8822dd69a7cdc31dcd423b778a3f458dde82212c3393044f91b9`.

Reserve port 24223 with the Coordinator and boot a fresh overlay using the
preceding recipe. On the Mac configure the editor host with
`-DI0_SIGN_IN=ON`, build the painter with the editor feature and link the
host with `scripts/navigator-i0-editor-link.py`. Stage the stripped host
at the fixture bundle's `bin/editor-check`, Limen's `editor_fixture` example
at `editor-parent` and the `i0-raster-check` build of `fresco-renderer` at
`renderer/bin/fresco-renderer` in the gate directory. Run the guest gate above
with `I0_SIGN_IN=1`. For the editor configure `I0_SIGN_IN=OFF` and unset
the guest variable. Power off cleanly and release the slot to the Coordinator.

### Mail Microsoft sign-in attempt, 2026-11-20

The first blocker is in Mail account setup. Discovery sets the OAuth provider
to `microsoft`. In `mail/atrium-mail/src/settings.rs`, `shown` displays the
missing-sheet message and `can_add` disables the button for any OAuth account.
There is no Microsoft launch from this screen. This finding comes from tracing
the Mail code. The guest test below starts at the engine entry point.

On a fresh arm64 HVF overlay, the guest reported kernel `9f2eed03a36c` and
`/dev/vtbd1p1` mounted read-write as Tessera. The unmodified JIT-off control
completed all four fixture GETs, password refusal and callback in each of three
trials. For the diagnostic build we substituted the initial URL in
`SignInFixture.h` with `MICROSOFT.authorize` from
`arca/arca-courier/src/oauth.rs` and logged the URL immediately before
`fetch`. This tests the origin boundary without inventing a client registration
or sending account data. The URL had no OAuth query parameters. The engine
otherwise used the same interpreter, loader and broker as the control.

Portcullis verified the manifest. The probe reported uid 1011, jail membership,
Capsicum mode 1 and ECAPMODE on the ambient open. LibWeb attempted to load
`https://login.microsoftonline.com/common/oauth2/v2.0/authorize`.
In `navigator-ladybird/fixture_fetch.rs`, the exact-URL check precedes
`client.open`. The broker logged zero GETs, the worker reported
`broker did not return the common fixture`, and Limen observed `ChildLost`
before the first page frame. Microsoft supplied no bytes, so page parsing,
script execution, password UI, callback and page timings remain untested.
Probe SHA-256: `9d9efb0aa4374d1b82d1d3c61b435ba36b419327f2ec3286d72a185293311d9a`.

Next, connect Mail's provider action to an Arca-owned PKCE session and
Navigator sign-in sheet. Supply Atrium's registered Microsoft client ID and
route navigation, subresources and the session cookie jar through the scoped
broker. The fixture transport checks the response against embedded HTML and
cannot serve as the real IdP loader. Retry to the first visible sign-in step;
record the last successful fetch and first failing stage. Full account setup
also needs Arca to intercept the registered redirect, verify state, exchange
the code and store the initial grant as described in mail.md §5.3.

### Development registration and Mail's first screen

**Decision, 2026-11-20.** Defer a project Microsoft registration. An operator
with an Entra test tenant can later supply its public application ID with the
native-client redirect in mail.md §5.3. Development uses a loopback TLS
provider, a test-only application ID and no personal credentials. No other
mail client's ID is shipped. A live provider render and mailbox authorization
remain separate gates.

Mail's Microsoft button calls arcad with an empty `MAIL_SIGN_IN` request.
The configured Mail publisher and an unlocked user store are required. Arca
owns the S256 verifier and state and gives Mail a 16-byte random ticket.
The signed Navigator broker can claim the URL once for the same user. Lock
invalidates outstanding tickets. The ten-minute session is memory-only.
The deployment settings are `ARCA_MICROSOFT_CLIENT_ID` and
`ARCA_MAIL_PUBLISHER` on arcad and the existing signed `ARCA_WEB_BROKER`
identity on arcad, courierd and limend. An unconfigured development install
reports the missing registration on a press.

Mail requests the `mail-sign-in` Limen sheet. Limen matches the configured
Navigator publisher, owner and persona. Navigator claims the ticket and
routes the authorization GET and redirects through the Arca cookie courier
in a fresh Unrecorded partition. The first-screen grant admits HTTPS on
port 443 and excludes the native-client callback from fetching. The sheet
hands the returned HTML in a sealed descriptor to its nested
`org.atrium.navigator.mail-page` worker. The engine receives the document
URL and bytes; the session ticket and cookie jar stay outside the engine.
Cross-build the worker with `I0_MAIL_PAGE=ON`, link it with the interpreter
and native painter, and sign the mail-page manifest. Deployment needs a
Limen per-instance worker rule for `org.atrium.navigator.mail-page`, with
`_limen` as caller. Navigator opens the trusted sheet with its Fresco grant.

Navigator draws **Cancel sign-in** in the sheet outside the nested page. A press there
disposes of the sheet and its page and tells Mail to cancel the Arca ticket. The
[revoked-account cancel/retry gate](mail.md#cancelling-and-retrying-a-revoked-account)
checks both the cancelled attempt and the subsequent successful exchange.

The development provider requires a Secure HttpOnly session cookie on its
redirect before serving a page whose inline script labels an account field.
The TLS test checks the cookie at the provider, its absence from script
access, one-use ticket claiming and refusal before a ticket is issued for
an unrelated origin or callback. The guest gate
`scripts/navigator-mail-sign-in-gate.sh BROKER_TEST SIGNED_ENGINE_BUNDLE`
uses the cross-built navigator-cookies broker test and the `I0_SIGN_IN=ON`
engine. It transfers only the response body from that test into the jail.

On 2026-11-20 the development gate passed on kernel `9f2eed03a36c`
with a fresh read-write Tessera root. The broker used the TLS fixture and
in-process Arca and courier handlers. Portcullis verified the engine bundle.
In the jail, uid 1011 entered capability mode and an ambient open returned
ECAPMODE. The response created the expected account input and script label.
A separate Limen routing test checked publisher, owner and persona. Three
render trials passed the 153-byte response as a sealed descriptor to the
JIT-off worker. Every initial frame measured 3 changed nodes, 4,198,501
wire bytes including initial resources, and 920 tiles at 640 × 360. The
worker SHA-256 was `dc89eef333707065c036aa3ba49487a8e6712b34537377d973ff0abb08362873`.
For the visible test, stage the `I0_MAIL_PAGE=ON` binary as the signed
gate worker and run the editor gate with `I0_MAIL_DOCUMENT` pointing to
the public response saved by the first gate.

**Continuous local UI gate, 2026-11-20.** On the shared arm64 HVF baseline,
the writable overlay mounted `/dev/vtbd1p1` as Tessera. In each of three cold
app launches, the test entered `ada@outlook.com` in Mail settings, pressed
**Find settings**, then **Sign in with Microsoft**. Arca audited Mail's
`0x14` and Navigator's `0x77` under the same fixture user. The signed
Navigator instance was already running for that user. The courier reached
only the fixture over TLS. The provider checked the S256 request and
required its Secure HttpOnly cookie at the redirect before returning HTML.
The rendered account field and its inline-script label **Enter your account**
were visible inside the system-labeled Navigator sheet on all three presses.

Mail anchors the sheet before sending the ticket. Navigator paints an opaque
backdrop. Limen checks the requesting uid and window token against the
registered Mail sheet before starting its document. Fresco admits that
document as a clipped island inside the sheet. The non-root worker
(uid 50006) ran the interpreter build identified above. FreeBSD reported
Capsicum flag `C` and both jail IP families disabled. Split Fresco used a
separate renderer jail. Each initial 640 × 360 document frame submitted
3 changed nodes and 4,198,501 encoded bytes and rasterized 920 tiles.
The three desktop PNGs had SHA-256
`b0ef50c85095cd8c223ba0eb9a38a6e745e09513252c783f227c1d2a7c39146a`.

The local provider is the `courier-test-server` example. Its arguments for
this gate are `login.microsoftonline.com CA_FILE PORT_FILE MARKER_LOG
127.0.0.1:0`. Trust its ephemeral CA only in the test courier. Inside the
overlay map the provider name to 10.0.2.100. Run QEMU with restricted user
networking and a per-connection guest forward from 10.0.2.100:443 to that
loopback listener. This keeps the guest's standard PF host-address block in
force. Start arcad, courierd and limend with the fixture publisher settings
above, launch the installed Navigator and Mail bundles for the same user,
and require exactly `authorized`, then `first-screen-cookie-ok` per press.
Screenshots and redacted guest evidence on the Mac are under
`~/atrium-vm/evidence/navigator-mail-button-20261120/`.

This milestone covers the first HTML response and inline script. External
script and stylesheet loading from LibWeb, interactive continuation, redirect
redemption and first-grant storage still need separate verification. The
development gate does not establish Microsoft's behavior in an embedded engine.

### Local Mail continuation fixture

The local provider accepts a final `mail-continue` argument after its
loopback listen address. The first fixture document then contains a
read-only demo account, an external stylesheet, an external script and a
Continue button. Its click handler requests the next step as same-origin
JSON and displays the returned message. The provider requires the redirect
cookie on all four GETs and records only stage names.

Set `NAVIGATORD_MAIL_CONTINUATION_FIXTURE=1` in the test broker bundle.
Navigator enables the three exact resources only after the ticket-bound
navigation returns `https://login.microsoftonline.com/first`. It uses
the existing sheet view and Arca partition for every GET. Redirects are
refused, response type and size are checked, and only a sealed body and
its resource URL go to Ladybird. The worker pumps the engine event loop,
dispatches pointer input through LibWeb and submits updated Fresco scenes.

The gate starts in Mail and requires `authorized`, `first-screen-cookie-ok`,
`stylesheet` and `script` before pressing Continue. After that press it
requires the provider's `continued` marker and “Demo account confirmed” on
screen. The public demo uses no user credential. OAuth callback redemption
and token storage are separate milestones.

On 2026-11-20 this gate passed twice from cold Mail and Navigator launches
on a private arm64 HVF overlay with Tessera mounted at `/`. Each press of
Mail’s Microsoft action produced the four pre-Continue markers above. On
the first run the sheet had 21,814 pixels in the external stylesheet’s
heading color (`#b4cfee`). A Fresco pointer press and release on Continue
produced exactly one authenticated continuation request and displayed
“Demo account confirmed”. Pressing the disabled button again left the
request count unchanged. The two post-click screenshots had identical
SHA-256 hashes. All four unauthenticated control requests returned 403.

The installed worker matched the host build at SHA-256
`227c740c194219ad01c6b8df0821a6b505f97eb8cca9043d4c6306535c7b1c7d`.
FreeBSD reported uid 50006, Capsicum flag `C` and both IP families disabled.
The arm64 build dependency tree contained the Rust JS and Wasm engines and
neither JIT crate. Screenshots, provider stages and confinement evidence
are in `~/atrium-vm/evidence/navigator-mail-continue-20261120/`.

### Direct Navigator inbox fixture

On 2026-11-20 the installed Navigator app opened a 640 × 360 whole-page
fixture in its own window, from the opt-in setting
`NAVIGATORD_INBOX_FIXTURE=https://inbox.example/inbox`. Two cold Navigator
launches passed on a fresh arm64 HVF overlay with `/dev/vtbd1p1` mounted as
Tessera root. That first gate used a startup launch. The Where-field gate below
now opens the same worker on a press.

Build the worker with `I0_INBOX_PAGE=ON`, the interpreter patch and generated
`LIBWEB_RUST_BINDINGS`. The host installs Ladybird's inline fetch executor
on the window agent before parsing the document. Navigator opens a cookied
view, retrieves the document through Arca and passes its body to Limen.
The resource loader requests two external assets and a same-origin JSON
endpoint. Navigator enforces the three exact resource URLs, single-hop
GET, expected MIME and a 64 KiB body limit. Each reply is a sealed descriptor.
The worker receives no cookie headers. JavaScript dispatch, layout, hit testing
and native scene updates all run in the worker.

Use the Mail gate's restricted forwarding setup with `inbox.example`. Start
`courier-test-server inbox.example CA_FILE PORT_FILE MARKER_LOG 127.0.0.1:0 inbox`.
First require a cookie-free GET of `/api/inbox` to return 403. Install the
inbox worker with its shipped manifest and allow only Limen to start it.
For the Where-field gate, install the Navigator UI worker, configure
`NAVIGATORD_UI=jail:org.atrium.navigator.ui`, the chrome and activation sockets,
and set the inbox fixture URL in the broker config. Start at the empty Where
field with `NAVIGATOR_UI_TRACE=1` and `NAVIGATOR_UI_SIZE=960x540` on a
960 × 540 headless display. Run this on the Mac:

```sh
python3 scripts/navigator-i0-inbox-check.py --where HOST_WORKER SSH_WRAPPER MARKER_LOG OUTPUT_DIRECTORY
```

The gate checks the installed worker SHA-256 against the host executable.
It enters the URL, waits for the prompt, presses Not now, reloads and presses
Run live. Before acceptance it requires zero new provider requests and no
worker. Navigator spends the compositor's guarded prompt token before
creating the separate window and requesting any document. The offer applies
to the exact fixture URL in a recorded, non-anonymous trail. Unrecorded
trails and the portal and VPN sign-in brokers do not offer it.

On a fresh arm64 Tessera snapshot the Where-field gate passed. FreeBSD
reported Capsicum flag `C` for the worker (uid 50006), with both IP families
disabled. Before/after frames, the prompt, and the guest proof are in
`~/atrium-vm/evidence/navigator-where-20261120/`. An earlier attempt exited
after the HTML request. The installed worker differed from both its staged
copy and the Mac binary. Restaging restored matching hashes and the full
flow passed. The cause of the initial installed-file mismatch is unverified.
This result covers the fixture. Live Gmail remains untested.

Both visits requested the document, CSS, JS and authenticated JSON. Each
before-frame showed “School field trip” and 22,631 heading pixels in the
stylesheet color. A Fresco pointer press and release on Compose at (53, 133)
made “New draft” appear. FreeBSD reported uid 50005, Capsicum flag `C`, and
both IP families disabled. The worker matched its host SHA-256
`9d0c19d09ff38310c290f4fd5117604cf25de30faee270205374cceebeee9581`.
The arm64 engine dependency tree included the Rust JS and Wasm engines and
neither JIT crate. The evidence is in
`~/atrium-vm/evidence/navigator-inbox-20261120/`.

### I1 embedded counter gate

On 2026-11-20 the counter passed twice on an arm64 HVF guest with Tessera at
`/`. Navigator’s normalizer converted the iframe in `counter-parent.html` into
an island. The document renderer emitted the slot at (40, 40), 640 × 360 CSS
pixels. A fixture parent used that rectangle for both hit testing and Limen
placement. An outside press left the placeholder visible and the broker log
empty. Pressing the box launched the signed `org.atrium.navigator.engine`
worker and made one GET of `https://widget.example/counter.html` through
a preconnected Nuntius protocol fixture.

The page’s inline JavaScript updated the counter in Ladybird. The composed
Fresco screen showed 0, then 1 on the first click. A covering window received
the next press. Uncovering showed 1, and a further visible click produced 2.
Closing the parent window and its Limen connection ended the engine process.
FreeBSD reported UID 1011, Capsicum flag C, IPv4 and IPv6 disabled. The worker
verified capability mode and a denied path lookup. The target dependency tree
contained the JS and Wasm interpreters with both JIT crates absent.

To repeat, cross-build the editor host with `I1_COUNTER=ON`, other fixture
options off, and the interpreter-only painter with the editor feature. Stage
the stripped binary as `bin/counter` with the counter manifest. Use the I0
split-renderer setup with a 720 × 420 headless screen. Normalize
`test-assets/navigator-i0/counter-parent.html` with `navigator-normalize` and
render its output with `nsg-render`. Write the resulting island rectangle
(x y width height, divided by 64) to `counter.rect` in a fresh guest socket
directory. Start the `editor_fixture` example with `I1_COUNTER=1`, that directory
and the signed bundle, logging to `parent.log`. Run the driver on the Mac with
Tesseract on PATH.

```sh
python3 scripts/navigator-i1-counter-check.py SSH_WRAPPER GUEST_SOCKET_DIR EVIDENCE_DIR PARENT_NSG
```

The document viewer now retains island keys and sources with each resident
scene band. A primary press on an island reports its key, source and visible
rectangle in viewer-local pixels to Navigator over Limen. The rectangle is
clipped to the band and viewport. The signed UI treats the message as advisory
and checks it against metadata for the same open request. Opening a page with
widget support asks the broker for its recorded iframe sources. The jailed
converter records the first 16 distinct eligible sources from the published
DOM, using the normalizer's iframe rules. The broker resolves web origins
against the recording URL and supplies the source/origin pairs to chrome.
A matching press on the current page opens a dismissible row identifying
the origin. Older recordings without the field and unmatched presses produce
no row. The row carries no engine or fetch grant.

On a fresh arm64 Tessera overlay the viewer event-handler test loaded the
counter parent and reported a single rectangle of (40, 40, 640, 360). A
secondary press, a release, an outside press and a press after document
replacement produced no island message. A separate document test checked
geometry at 2× zoom after scrolling. These tests injected viewer events.
The earlier compositor and JIT-off worker test used the fixture parent and
its bounded GET responder.

The widget-origin gate ran on a fresh arm64 HVF overlay with Tessera mounted
read-write at `/`. All five guest executable hashes matched staging. Against
the broker's fixture socket, the converter reported the counter iframe on a
fresh fetch and a cache hit. The link, forged video attribute, inline frame
and one-pixel frame in the negative page produced no source. An ordinary
open of the cached positive page also produced no source message. The broker
metadata and UI shell regressions passed, including rejection after navigation.
This gate used an ordinary converter process and exercised the UI shell in a
test binary. The installed window and jailed worker launch remain to be tested
together. Logs and executable hashes are in
`~/atrium-vm/evidence/navigator-i1-widget-origin-20261120/`.

Next, spend a guarded chrome activation for the selected source and current
document, maintain a whole slot across band, scroll and page changes, and
provide the origin-bound fetch channel through the production launcher.
Evidence for the two gates is in
`~/atrium-vm/evidence/navigator-i1-geometry-20261120/` and
`~/atrium-vm/evidence/navigator-i1-counter-20261120/`.

## 9. Decisions, with recommendations

*Pure Rust, later (Shankar, 2026-11-19):* no complete engine is pure Rust today; Servo's
only C++ part is SpiderMonkey. The study of binding Boa or Nova into Servo (legacy-web
§11.9.2: the collectors' unsafe-surface audit, then the DOM-hosting spike) comes later, not
alongside I0.

| | question | options | recommendation |
|---|---|---|---|
| **D1** ✓ | Engine | Servo + SpiderMonkey (MPL-2.0, Rust embedding) · Ladybird (BSD-2, Rust LibJS with a C++ API and mixed Rust/C++ LibWeb) · our Boa and minimal DOM, run live | **Updated by Shankar, 2026-11-20. Use Ladybird. Defer Servo unless it can handle a required page blocked by Ladybird.** Chromium is out: Blink still carries LGPL files from WebKit and KHTML. Gecko is MPL and WebKit is LGPL. Boa and our DOM have no layout or rendering, so they can't run a widget or a sign-in page |
| **D2** ✓ | MPL-2.0, if I0 picks Servo (LICENSING-POLICY: "evaluate per-component") | accept for the engine bundle only · refuse | **Accept for the engine bundle only.** MPL is file-level copyleft: we publish the source of any MPL file we change, and nothing of ours changes licence. toolkit-backends.md already accepted it for Servo. Nothing else links it |
| **D3** ✓ | Where I0 builds | cross-build on the Mac · natively in the Mac VM · a larger cloud build environment | **Decided (Shankar, 2026-11-19): cross-build on the Mac** with a FreeBSD sysroot, from a Mac session. Build one engine at a time on the Mac. The shared VM runs I0's measurements |
| **D4** ✓ | Order after the first island | sign-in windows (I3), then video · video first | **Sign-in first.** The VPN's SSO and Mail's Microsoft account wait on it, and it needs no `MediaSource`. A widget island (I1) still comes first, to prove the jail, fetching, drawing and input |
| **D5** ✓ | Whole pages live in everyday browsing (fat clients) | later, as I6, offered per page on the person's press · never | **Later, on a press.** Sign-in windows run whole pages live from I3, because a sign-in is a whole page. Everyday trails stay document-lane with islands until then |
| **D6** ✓ | Whose jar a live sign-in uses | Arca couriers in the window's own partition · a throwaway Nuntius jar | **Arca's couriers**, as vpn.md §9.3 already specifies, so the person stays signed in next time and no engine holds a cookie |

### Local Mail callback and account storage

On 2026-11-20 the local provider’s `mail-callback` mode passed on a fresh
arm64 HVF overlay. The guest mounted `/dev/vtbd1p1` at `/` as Tessera.
The Mail settings window started with zero accounts. Its Microsoft action
opened the development form in the JIT-off Ladybird worker. The worker
matched the SHA-256 above, had Capsicum active and had IPv4 and IPv6 disabled.

After one Continue click, the provider recorded the following sequence.

```text
authorized
first-screen-cookie-ok
stylesheet
script
callback-issued
pkce-redeemed
```

In this fixture mode Navigator intercepts the local continuation’s 302
before forwarding a response to the page. It passes the registered redirect
to the courier for the sheet’s partition. Arca checks the state and permits
one code exchange. The courier posts the verifier over TLS, checks the
token response and gives the refresh grant to Arca. Navigator signals
completion to Mail, which retrieves two opaque references from Arca.

The guest reopened a copy of Mail’s persisted store and verified one
Microsoft account for `ada@outlook.com`, distinct nonzero references,
and the Outlook IMAP (993) and SMTP submission (587) endpoints. Separate
arm64 controls rejected a foreign user, wrong state, duplicate callback
parameters, wrong redirect, incorrect verifier and replay. Background
sync could not reach Outlook on the restricted fixture network. Live
Microsoft sign-in and rotated refresh-token use remain unverified.

Screenshots, stage markers, confinement evidence and guest checks are under
`~/atrium-vm/evidence/navigator-mail-callback-20261120/` on the Mac.


### Stored Mail account refresh rotation

On 2026-11-20 a fresh writable arm64 HVF overlay booted from the completed
callback snapshot with Tessera at `/`. The fixture launcher restored the
original signed Mail UID binding (50005) in the guest’s volatile app
registry. The saved database retained its original ownership and account
references. No second sign-in was performed.

A fixture client under Mail’s identity used its production connection code
to authenticate four IMAP sessions and four SMTP submission sessions.
Each accepted XOAUTH2 login answered NOOP. The local provider rotated the
refresh token eight times. After the first two rotations, Arca restarted
and reopened its store. The next IMAP session used the stored successor,
and the original refresh token returned HTTP 400 `invalid_grant`.

The running Mail jail reported `vnet=new`, only `lo0`, and `allow.nomount`.
Portcullis supplied the launcher directory as a nullfs mount. Mail’s direct
connection to the fixture failed. The launcher logged separate couriers for
the provider on 443, IMAP on 993 and submission on 587. No live provider
or mailbox was used. Guest and provider evidence is under
`~/atrium-vm/evidence/mail-oauth-rotation-20261120/` on the Mac.


The subsequent local Mail gate used the saved callback account in the
production Mail window. IMAP synchronized the fixture message into the
inbox and reader. A Send press produced one authenticated SMTP delivery
with the expected envelope and MIME body. The guest ran the signed sync
worker. [Fixture and results](mail.md#local-inbox-and-submission-gate).
