# Navigator live code: the island engine (tier 4)

Status: **proposed 2026-11-19**, for Shankar's decisions in §9. Shankar asked for site
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
- Engine: **Servo with SpiderMonkey** (§11.3; now measured against Ladybird first, §9 D1), **JIT off** (§11.5), **session recycling**
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
- **Drawing**: the engine renders on the CPU into its island window's frames. GPU rendering
  waits for the architecture Shankar will design for apps that do their own GPU work
  (threat model §2.5.1).
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

1. **I0. Two engines, measured side by side** (D1). Build **Servo** and **Ladybird** for
   FreeBSD (amd64 and arm64), headless, JIT off, with their network and media layers stubbed.
   Load the same set in each:
   - the fixture site;
   - three corpus widgets (an Infogram chart, a map, the YouTube embed);
   - two scripted sign-in pages (a fixture identity provider, and a real one's page as far
     as its password step).
   For each engine, measure:
   - correctness against a reference rendering;
   - start time and memory;
   - binary and build size;
   - the build's cost in disk, memory and hours;
   - FreeBSD patches needed;
   - how far its `MediaSource` is from what the YouTube player needs;
   - how its network and media layers can be replaced (§3, §6).
   Write the results and the choice in this spec. The choice is made on those measurements,
   not on licence.
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
  - The risk is the C and C++ underneath. SpiderMonkey has its own build system, and Servo
    links fontconfig, FreeType and HarfBuzz, which the sysroot must carry from FreeBSD's
    packages. Ladybird uses CMake with vcpkg, which builds its dependencies (Skia among
    them) and would need a FreeBSD toolchain file.
  - Where a cross-build fails, that engine builds natively in the Mac VM instead, which is
    slower but sure.
- **FreeBSD support.** Servo targets Linux, macOS, Windows and Android. FreeBSD may need
  patches, upstreamed where we can.
- **Compatibility.** Servo renders fewer sites correctly than Blink. Islands are small
  regions, which helps, but YouTube's player may need APIs Servo lacks. I0 measures this
  before we promise it.
- **Real sites in gates.** I0 and I3 need the open internet in the VM. The hermetic fixture
  network covers I1 and I2.

## 9. Decisions, with recommendations

*Pure Rust, later (Shankar, 2026-11-19):* no complete engine is pure Rust today; Servo's
only C++ part is SpiderMonkey. The study of binding Boa or Nova into Servo (legacy-web
§11.9.2: the collectors' unsafe-surface audit, then the DOM-hosting spike) comes later, not
alongside I0.

| | question | options | recommendation |
|---|---|---|---|
| **D1** ✓ | Engine | Servo + SpiderMonkey (MPL-2.0, Rust embedding) · Ladybird (BSD-2, C++, young) · our Boa and minimal DOM, run live | **Decided (Shankar, 2026-11-19): measure Servo and Ladybird side by side in I0, and choose on the measurements.** Chromium is out: Blink still carries LGPL files from WebKit and KHTML. Gecko is MPL and WebKit is LGPL. Boa and our DOM have no layout or rendering, so they can't run a widget or a sign-in page |
| **D2** | MPL-2.0, if I0 picks Servo (LICENSING-POLICY: "evaluate per-component") | accept for the engine bundle only · refuse | **Accept for the engine bundle only.** MPL is file-level copyleft: we publish the source of any MPL file we change, and nothing of ours changes licence. toolkit-backends.md already accepted it for Servo. Nothing else links it |
| **D3** ✓ | Where I0 builds | cross-build on the Mac · natively in the Mac VM · a larger cloud build environment | **Decided (Shankar, 2026-11-19): cross-build on the Mac** with a FreeBSD sysroot, from a Mac session. An engine whose C++ side won't cross-build builds natively in the shared Mac VM, booked through the coordinator. The VM runs I0's measurements either way |
| **D4** | Order after the first island | sign-in windows (I3), then video · video first | **Sign-in first.** The VPN's SSO and Mail's Microsoft account wait on it, and it needs no `MediaSource`. A widget island (I1) still comes first, to prove the jail, fetching, drawing and input |
| **D5** | Whole pages live in everyday browsing (fat clients) | later, as I6, offered per page on the person's press · never | **Later, on a press.** Sign-in windows run whole pages live from I3, because a sign-in is a whole page. Everyday trails stay document-lane with islands until then |
| **D6** | Whose jar a live sign-in uses | Arca couriers in the window's own partition · a throwaway Nuntius jar | **Arca's couriers**, as vpn.md §9.3 already specifies, so the person stays signed in next time and no engine holds a cookie |
