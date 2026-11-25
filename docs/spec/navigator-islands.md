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
- While it runs, the chip keeps the main document’s tier and adds “1 live part”.
  A replayed document reads “Replayed · 1 live part”. Frescod frames and tags the island.
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

### I2 installed island reports

For a broker-fetched web island, Limen accepts lifecycle reports from the
bound child. A session opens with one `Started { origin }`. While running,
it may report `Title` and `Resize { width, height }` and can finish once with
`Ended(Closed | Failed)`. Frames use postcard, consume the whole payload,
and fit in 512 bytes. The HTTPS origin field is limited to 255 ASCII bytes,
the title to 256 UTF-8 bytes without control characters, and each dimension
to 1 through 16384. A running worker may also issue one numbered
`BeforeUnload` question at a time. Its matching unload answer is the only
parent-to-child payload. Limen refuses descriptors. Its existing per-embed message rate limit applies.
The older editor measurement fixture uses a separate request without a fetch
channel and retains its exact child `frame` and parent `advance` markers.

The installed Ladybird counter takes its initial origin and title from the
loaded LibWeb document after first paint. Navigatord checks report order
and matches the claimed origin against its own grant. The compositor
attachment still determines the live badge. Title is informational and
the parent keeps the widget at its assigned rectangle. Stop and process
loss retire the worker even when it cannot send an Ended report.

On the arm64 Tessera fixture, two guarded launches each reported
`https://widget.example` followed by the seven-byte document title. Stop
and worker loss cleared the chip, and the UI and parent viewer kept their
PIDs. The Limen integration test exercised Resize and Ended, including
rejection of later reports. To repeat the installed gate, stage matching
Limen, navigatord and counter builds, enable `NAVIGATOR_UI_AX=1`, and run
`scripts/navigator-i2-stop-check.py SSH_WRAPPER GUEST_STAGE NAVIGATOR_LOG EVIDENCE_DIR --reports`.
Evidence is in `~/atrium-vm/evidence/navigator-i2-channel-20261120/`.

### I2 generated channel and lifecycle tests

Limen’s `island_channel` integration test uses a fixed seed to generate
32,768 cases for the five report variants and the unload answer. It tests
each report against a lifecycle state, checks rejection preserves that state,
and checks matching, stale and duplicate answers. Mutations cover truncated
reports, trailing bytes, oversized frames and single-bit changes. A mutated
report that is still valid must survive re-encoding with the same transition.
Both Stay and Leave and all five accepted report variants must be exercised.

The installed gate uses the pinned `counter-unload.html` fixture, UI
accessibility tracing and a fresh window without a forced start URL. Pass
`--channel-seed 7331` to `scripts/navigator-i2-stop-check.py` to reproduce
the live sequence. The seed varies edits and unload questions across four
workers. Each session exercises Stay before ending on either a guarded
Leave or compositor Stop with a question pending. The gate checks the
painted counter, worker identity and Capsicum state, broker decisions,
fetch count and the return to the static tier.

On arm64 Tessera all 32,768 generated wire cases passed. The live run with
seed 7331 recorded four distinct JIT-off counter processes in four jails,
each with the kernel’s Capsicum flag. Across 18 increments and eight unload
questions, four Stay replies preserved the count and PID, two Leave replies
produced one voluntary close each, and two compositor Stops retired a worker
with a question outstanding. All four sessions cleared the prompt and live
part. Guest logs, frames and binary hashes are in
`~/atrium-vm/evidence/navigator-i2-channel-corpus-20261121/`.

### I2 worker resize and voluntary close

The embedded counter reports changes to its laid-out document’s scroll width
and height. Navigatord retains the placement supplied by the parent. In the
installed gate, Increment widened the body to 884 CSS pixels plus 32 pixels
of padding. Ladybird reported 916 × 360. The two captured frames had the
same compositor borders around the 640 × 360 slot, and the counter PID
stayed the same. This gate exercises layout-driven resize requests. In the
pinned LibWeb revision the JavaScript `window.resizeTo()` method returns
immediately.

Finish calls `window.close()`. The single-document host runs LibWeb’s
before-unload check and close traversal. Once the page closes, the worker
reports `Ended(Closed)`. The broker retired the worker, the chip returned
to Static and the UI showed no crash prompt. A guarded Start reached Count 0
with a different PID on the same parent and viewer.

To repeat on the arm64 Tessera fixture, serve `counter-lifecycle.html` as
`/counter.html` and copy it as `counter.html` into the counter build’s
`--embed-dir`. Compile both `editor-check.cpp` and `EditorPage.cpp` with
`ATRIUM_I1_COUNTER`. Launch the installed parent with UI AX tracing and run
`scripts/navigator-i2-stop-check.py SSH_WRAPPER GUEST_STAGE NAVIGATOR_LOG EVIDENCE_DIR --lifecycle`.
This check reads compositor frames with Pillow. The Limen unit tests also
check all 7,776 length-five sequences over six representative reports and
title lengths 0–513, including truncated frames. Evidence is in
`~/atrium-vm/evidence/navigator-i2-worker-lifecycle-20261120/`.

### I2 cancelling a widget unload

After placement and brokered fetch, the single-document host initializes the
counter’s LibWeb page as visible. LibWeb applies its own eligibility checks,
including prior user interaction, before it requests a before-unload dialog.
The engine sends only a question number. Navigator’s signed prompt row uses
the attached widget’s broker-confirmed origin and offers **Leave** and **Stay**.
The broker requires a fresh compositor press for Leave. The answer is scoped
to the document, the original Start receipt and the outstanding question.
Limen rejects unsolicited answers and duplicate or reused question numbers.

Stay resumes the pending LibWeb unload check with a negative answer. An
unavailable prompt or 30 seconds without an answer also chooses Stay.
The engine bounds its wait at 45 seconds. Leave resumes the same check
with an affirmative answer and the normal close traversal sends `Ended(Closed)`.

The installed arm64 Tessera gate used a dirty counter whose real
`beforeunload` handler called `preventDefault()`. Stay preserved Count 1,
the unanswered prompt preserved Count 2, and a subsequent Increment reached
3 on the same PID. Leave retired the worker once. The page used one widget
GET, the tier returned to Static, and the parent and viewer kept their PIDs.
To repeat, pin and serve `counter-unload.html` as `counter.html` using the
worker lifecycle setup above and pass `--unload` to the installed gate.
Evidence is in `~/atrium-vm/evidence/navigator-i2-unload-cancel-20261121/`.

The Stop-and-replacement gate runs with the same worker and production broker.
Stop withdrew the first worker while its unload question was visible. Both
answer buttons disappeared from the accessibility tree and the tier returned
to Static. Guarded Start opened a fresh counter on the same document. With
query 1 pending again, a gate-only UI sent Leave for the retired Start using
a fresh compositor press, then sent Stay for the replacement. The replacement
kept its PID and advanced to Count 2. Its own Leave on query 2 closed it once.
The parent and page PIDs stayed unchanged across the two widget fetches.

To repeat, apply `test-assets/navigator-i0/retired-unload-ui.patch` to a
disposable copy of the UI source before cross-building and staging the UI
bundle. The patch captures the first pending question and writes the delayed
answer over the usual broker connection. Use the unload fixture above and
pass `--pending-stop` to `scripts/navigator-i2-stop-check.py`. Guest logs,
frames and binary hashes are in
`~/atrium-vm/evidence/navigator-i2-pending-stop-20261121/`.

The navigation-away gate uses the production UI. With the counter at 1 and
its unload question visible, following **Visit garden** opened a second static
page and retired the worker. Back showed the original viewer again without a
second parent GET. The question was absent from both the frame and the
accessibility tree on the destination and after Back. The widget stayed idle
until a new Start press. Its new worker opened at 0, incremented to 1 and
closed on its own Leave.

To repeat, serve `away.html` alongside `counter-parent.html` on the parent
origin, serve the unload fixture on the widget origin, and run the installed
gate with `--navigate-away` and UI accessibility tracing. Frames, guest logs
and fixture requests are in
`~/atrium-vm/evidence/navigator-i2-navigation-away-20261121/`.

The parked-viewer gate follows **Visit garden** with the unload prompt open,
then **Visit pond**. Navigator keeps the visible page and its immediate parent
live, so the second link makes the original counter page eligible to park.
With `NAVIGATOR_UI_IDLE_S=5`, the viewer acknowledged Park and exited.
Two Back presses returned to the counter page. A new viewer laid it out from
the broker’s cache, and the fixture recorded just one parent GET across the
run. The restored page showed no unload question on screen or in the
accessibility tree. Even with the new Start prompt open, there was no
counter process or additional widget GET. Confirming Start launched a fresh
counter at 0. It incremented to 1 and closed on its own Leave.

To repeat, serve `pond.html` on the parent origin alongside the navigation
fixtures above, set the five-second UI idle timeout and pass `--parked-viewer`
to the installed gate. Evidence is in
`~/atrium-vm/evidence/navigator-i2-parked-viewer-20261121/`.

To exercise saved-record restore, generate the sixteen linked fixture pages
with `python3 test-assets/navigator-i0/make-flowerbeds.py DIRECTORY` and serve
them as `/bed-1.html` through `/bed-16.html` on the parent origin. Use
`--saved-record TLS_LOG` in the parked-viewer setup. The gate visits all
sixteen pages after the park acknowledgement to fill the broker’s 16-entry
cache. On Back it requires the store as the source, a new viewer, and no
second parent GET in the fixture’s HTTP request log. It then checks the
restored prompt and Start consent through the production UI.

The installed saved-record run passed on arm64 Tessera with JIT off. The
original viewer exited after parking. After sixteen distinct fixture pages
were fetched, Back created a new viewer and Navigator reported the store
as the source. The parent had one GET for the run. The rebuilt page had no
unload question on screen or in the accessibility tree. Its widget stayed
idle through the Start prompt. Confirming Start produced the second widget
GET and a fresh counter that advanced from 0 to 1 and closed on its own
Leave. Evidence is in
`~/atrium-vm/evidence/navigator-i2-saved-record-20261121/`.

The broker-restart gate starts a fresh Personal trail by typing the counter
URL in the installed UI. Follow **Visit garden** with the widget’s unload
question visible. Before stopping navigatord, the gate checks the persisted
trail for both page record addresses, the parent link and the current page.
It sends TERM to the broker, waits for the broker and UI to exit, and
relaunches Navigator through Portcullis as the same user.

On arm64 Tessera the new broker restored Garden from the store. Back opened
the saved counter page in a new viewer. The TLS fixture received one GET for
each page across both processes. The old question was absent from the frame
and accessibility tree. The widget stayed idle through the Start prompt,
then opened at 0, incremented to 1 and closed on its own Leave.

To repeat, use the counter unload fixture (`counter-unload.html`) and the
same garden link as the navigation-away gate. Start with an empty app overlay,
omit `NAVIGATOR_UI_OPEN`, enable UI accessibility and trace logging, and set
`NAVIGATORD_TRAILS=/var/db/navigatord/trails.json` and
`NAVIGATORD_STORE=/var/db/navigatord/store` in the broker bundle. Pass
`--broker-restart TLS_LOG` to the installed gate. Both broker logs, the
pre-restart trail file and frames are in
`~/atrium-vm/evidence/navigator-i2-broker-restart-20261121/`.

The two-trail restart gate adds `private-counter.html` on the parent origin.
Use the broker-restart setup with `--unrecorded-restart TLS_LOG`. The gate
opens an Unrecorded trail with Ctrl-Shift-N while the Personal Garden trail
remains open. It renders the private page, presses its counter placeholder,
and leaves its widget-unavailable notice visible while stopping the broker.
The current counter fixture offers Start only in the ordinary Personal context.

On arm64 Tessera the saved trail file contained only the Personal pages.
A search of the broker’s on-disk page store found the Garden URL and found
neither the private URL marker nor its distinctive page text. The replacement
broker restored one trail at Garden. The private page had one GET before
restart and none afterward. Its notice and worker were absent. Back reopened
the Personal counter page from disk, and fresh Start consent launched a new
working counter. Both broker logs, frames and the saved Personal trail are
in `~/atrium-vm/evidence/navigator-i2-unrecorded-restart-20261121/`.

With multiple Personal trails, saving while Unrecorded is selected makes
the first recorded trail current on restart. Each recorded trail keeps its
own page cursor. The installed gate opened Embedded counter → Garden notes
in the first trail and Pond notes → Flowerbed 1 in the second, then went
Back to Pond before opening and selecting Unrecorded. The saved file
contained the two Personal trails, with Garden and Pond current respectively.

After restart the UI showed Garden (page 2 of 2). Switching trails showed
Pond (page 1 of 2). Forward opened Flowerbed 1, Back returned to Pond, and
switching back showed Garden. These steps made no fixture requests.
The private page, widget notice and worker stayed gone.

Use the Unrecorded restart setup with `--multiple-personal TLS_LOG` and
serve `/pond.html` and `/bed-1.html` from the garden fixtures described
above. The saved trail file, screenshots and broker logs are in
`~/atrium-vm/evidence/navigator-i2-multiple-personal-20261121/`.

The complementary gate uses `--second-personal TLS_LOG` with the same
fixtures. After opening Unrecorded, switch to the second Personal trail at
Pond. Before stopping the broker, the gate checks the selected Pond rail
entry, the still-open Unrecorded entry, and both Personal cursors in the
saved trail file. After launch it checks the first displayed page before
sending any navigation input, then exercises Forward in the Pond trail and
switches to the Garden trail.

On arm64 Tessera the saved file selected recorded trail 2 and contained
only the two Personal trails. The replacement broker opened Pond (page 1
of 2) from the store. Forward reached Flowerbed 1, Back returned to Pond,
and switching to the first trail showed Garden (page 2 of 2). These actions
made no fixture requests. Unrecorded’s private page, notice and worker
stayed gone. Evidence is in
`~/atrium-vm/evidence/navigator-i2-second-personal-20261121/`.

## 6. Media

The engine's `HTMLMediaElement` and `MediaSource` bind to `navigator_media::mse::Media`
(built and tested against a fake player, legacy-web §8.1.1). Segments come through the
island's fetcher and their bytes are appended to the native player. The player decodes,
presents and draws its controls in its own jail. Any media backend Servo brings with it is
left out of the build (its usual one is GStreamer, which is LGPL).

### I4 video prototype

The [watch document](../../test-assets/navigator-i4/watch.html) embeds the
YouTube player demo and links to two local time fragments, `#t=0.5` and
`#t=1.5`. The normalizer and renderer host test checks that the document
produces one privacy-enhanced YouTube island and two chapter hit regions.
The Navigator MSE test checks seeks in both directions, the pending seek
state, the native time reply, and rejection of malformed or out-of-range
targets.

The arm64 `atrium-media-probe` now drives the native Limen player through
Navigator's MSE adapter. The deterministic input is `red-blue.webm`. The
probe asks the player about VP9 support, appends the clip, waits for native
acknowledgements, then activates the rendered chapter targets while paused.
It checks the Fresco slot for red at 0.5 s and blue at 1.5 s. On arm64 Tessera, an isolated probe run captured RGB (254, 0, 0) at 0.5 s
and (0, 0, 255) at 1.5 s. Independent sampling of both saved PNGs at
desktop point (300, 204) matched the probe. The private guest used the
jailed Fresco renderer and shut down cleanly. The probe drove the MSE
adapter directly. Installed Ladybird DOM playback remains the next gate.
Frames, binary hashes, player events and shutdown evidence are in
`~/atrium-vm/evidence/navigator-i4-native-chapters-20261123/`.

The [deterministic MSE page](../../test-assets/navigator-i4/local-player.html)
loads its own script, fetches the VP9 clip, appends it to a DOM SourceBuffer
and ends the stream after updateend. A localhost Chrome control decoded
RGB (255, 24, 0) at 0.5 seconds and (0, 15, 255) at 1.5 seconds after
each seeked event. The arm64 probe accepts `PROBE_CHAPTER_FRAMES` to save
the Fresco frame for each target when running the guest comparison.

A host GET of the real embed on 2026-11-23 returned HTTP 200 in both
cases. Its preview playability status reported error 153
(`EMBEDDER_IDENTITY_MISSING_REFERRER`) with no Referer and OK with the
served watch document's loopback origin as Referer. This checks the
provider configuration only. The installed viewer sends chapter seeks for an on-screen recording only after
the island reports media readiness. The broker checks the document ticket
and Start receipt. Limen delivers the typed seek only while that instance
is running. The video worker applies it to the fixture element's DOM
`currentTime`. The host verified the transport and linked the arm64 worker.

For the [local watch document](../../test-assets/navigator-i4/local-watch.html),
build the worker with `I4_VIDEO` and enable the exact iframe URL with
`NAVIGATORD_I4_VIDEO`. The anonymous fetch grant covers its HTML, external
script and VP9 WebM in order, with a 64 KiB limit per response, an exact
media type and zero redirects. A loopback TLS test of the host fetch service
compared all three bodies byte-for-byte and verified refusal of a wrong MIME
type, a redirect and a 65,537-byte response. The counter retains its single-document grant.

The local I4 build applies `patches/ladybird/native-mse.patch` to the engine
and compiles the changed translation units with `ATRIUM_I4_VIDEO`. Build
EditorPage and the entry point with identical definitions. A stale EditorPage
object from the counter gate crashed before the first resource request.
The DOM element retains its CSS box and the native player owns its pixels and
controls. The broker clips its slot to the island. A private socket accepts
open, one VP9 track, bounded append, end and seek. Native buffered ranges
and decoder acknowledgements feed the DOM responses. The broker retires
the player when the channel closes.

Installed Navigator on arm64 Tessera confirmed the trusted Start press, a
non-root Capsicum engine, and broker GETs for the HTML and external script.
The guest's jaild mount policy refused the declared decoder-worker socket
for `org.atrium.player.embedded`. Native placement failed before the clip
GET. The separate native probe received correlated decoder acknowledgements
for 0.5 and 1.5 seconds while paused and verified both Fresco frame colours.
See `~/atrium-vm/evidence/navigator-i4-installed-chapters-20261123/`.
On 2026-11-23 the installed gate passed after adding the single writable
mount source `/atrium/sockets/workers` to the private overlay's jaild
policy, validating it and restarting jaild. The installed browser fetched
the local watch document, island HTML, external script and 1,672-byte VP9
clip. Ladybird and the native player ran in separate jails with IPv4 and
IPv6 disabled. With playback paused, pressing Next chapter, Opening and
Next chapter again produced blue, red and blue frames. The native playhead
moved to x=471, 371 and 471 on the two-second timeline, matching 1.5,
0.5 and 1.5 seconds. The installed gate observed the compositor output;
the earlier probe recorded the decoder acknowledgements separately.
See `~/atrium-vm/evidence/navigator-i4-installed-chapters-worker-20261123/`.
### YouTube provider playback control

On 2026-11-23 a fresh signed-out Chrome profile loaded the project watch
document for each referrer policy. The same Play action produced:

| Identity visible to the player | Player result after 15 s | Decoded frames |
| --- | --- | ---: |
| No referrer | Error 153, time 0 | 0 |
| Loopback watch origin only | Playing, time 13.97 s | 423 |
| Navigator package ID, separate desktop WebView-style navigation control | Playing, time 13.77 s | 417 |

The last control supplied `https://org.atrium.navigator/` on navigation
to the embed. The ID comes from Navigator's installed broker bundle.
YouTube documents the installed app ID as a desktop client identity.
See [YouTube's embedded-player requirements](https://developers.google.com/youtube/terms/required-minimum-functionality#api-client-identity-and-credentials).
The signed-out control received separate VP9/WebM and Opus/WebM format
offers and fetched media from a Google Video CDN. A player script measured
1,957,976 decoded bytes. These observations establish compatibility with
the provider in Chrome. They do not confer a network grant on Ladybird.

The local playback fixture grants four GETs: the HTML, its script, the VP9
WebM clip and the Opus WebM tone. Each resource is limited to 64 KiB.
The private player channel accepts at most 64 KiB of append data in total. Its ordinary fetcher
omits Referer. For the opt-in I4 entry test, the broker admits only the exact demo URL in
`NAVIGATORD_I4_YOUTUBE`. Start must spend a compositor press for the
current document. The island's first typed GET launches
`org.atrium.navigator.youtube-entry` over a private socket. The worker
enters Capsicum and sends `Referer: https://org.atrium.navigator/` on one
GET for the fixed embed over TLS. It accepts one HTML response up to
512 KiB and refuses redirects. The ordinary fetch worker, subresource
requests and the reader's page URL are outside this exception.
Build the entry island with `I4_YOUTUBE` and install the dedicated network
worker with `scripts/navigator-youtube-entry-bundle.sh`. The initial HTML-only gate measured the cost of opening the embed.
On arm64 Tessera the installed watch page and the open Start prompt made
zero provider connections. The first run exposed an uninitialized LibWeb
fetch executor while parsing the embed. After initializing it, two trusted
Starts fetched 144,490 and 144,511 HTML bytes, one provider TLS connection
each. The JIT-off worker reported the provider origin in Capsicum, UID
50007, IPv4 and IPv6 disabled. Stop retired it. Choosing Not now caused
zero further connections. The player area stayed black: the inline
bootstrap reached `writeEmbed`, which was undefined with the external
player scripts unavailable. The HTML-only results are at
`~/atrium-vm/evidence/navigator-i4-youtube-entry-20261123/`.

The installed startup gate reuses one Capsicum fetch worker for the lifetime
of the island. After the entry GET it admits same-origin versioned player
scripts and styles over TLS, up to 12 assets, 3 MiB per body and 10 MiB
delivered in total. The worker checks the path and response MIME type.
Asset requests omit the referrer and follow no redirects.

On arm64 Tessera the final Start delivered 143,823 HTML bytes, two styles
and three scripts to jailed JIT-off Ladybird. The bootstrap defined
`writeEmbed` and created a 640 × 360 player with 67 API methods. The
provider remained in state −1 and the island stayed black. Chrome with
the same resource policy displayed a 72 px “Play video” button. Timers,
messages and animation-frame callbacks fired in the Ladybird diagnostic.
A separate explicit `cueVideoById` probe reached a Fresco conversion
error on `DrawRepeatedTile` with an inline clip or transform. This run left the cause of the missing initial button open. The final frame, network trace and diagnostic are at
`~/atrium-vm/evidence/navigator-i4-youtube-startup-20261123/`.

The follow-up readiness gate ran with a 640 × 360 viewport, the same
provider fetch policy and a signed-out page. Chrome displayed its 72 px
Play button in state 5 when its capability probe accepted both exact WebM
types, VP9 video and Opus audio. Either codec alone left it in state −1.
In the JIT-off jail Ladybird reported VP9 supported and Opus unsupported.
Its player was fully intersecting the viewport, had the video title and
duration (1,344 s), and stayed in state −1. In a diagnostic build, adding
only the Opus answer produced the provider's own Play button in the DOM
without an explicit cue. That probe made no audio buffer or playback
claim. The two-track bridge now associates each LibWeb SourceBuffer with its own
native track and returns buffered ranges for that track. The fixture appends
the audio header first and checks that the video media append leaves the
audio range empty. After the audio media append, its shorter range must
remain distinct. A Chrome control passed in both buffer creation orders
and decoded both tracks after seeking to 0.5 s.

The installed arm64 Tessera gate passed with JIT off and Capsicum active.
Both creation orders kept the audio range empty after the video append and
reported video through 2.000 s and audio through 1.835 s after both appends.
The external script set `data-resume-on-seek` on the video for both runs
and `data-audio-first` for the reverse order, keeping the pinned HTML identical. A compositor press on **Load with sound** activated the page
before it attached the native player. Both runs sought to the blue chapter
at 1.5 s, sought back to red at 0.5 s, then resumed into blue. Lyra's stereo
mix to FreeBSD's dummy sound device contained the fixture's 440 Hz tone
across 27 consecutive 50 ms windows in each arm.
The guest had the fixture persona on the active seat, a Portcullis playback
grant for Navigator and a Limen attribution ticket for the embedded player.
Its jaild mount-source allowlist included the playback and workers directories.

LibWeb now calculates element readiness and buffered ranges from the native
tracks. Previously a seek reset readiness to HAVE_METADATA and left the
play promise pending. The native player now waits for Lyra's drain event
before declaring playback ended. Previously the device played only the
first queued portion of the post-seek tone. Frames, PCM, hashes, resolved
LibJS features and jail evidence are in
`~/atrium-vm/evidence/navigator-i4-opus-20261123/`.

For the restricted YouTube entry, navigatord now passes the same native
player descriptor used by the two-track fixture. On trusted Start the
worker receives the descriptor before entering capability mode. The
provider continues to use its existing restricted fetch path. Two trusted Starts on arm64 Tessera exercised the real channel with JIT off.
The page reported VP9 and Opus support, fetched two styles and three scripts,
and laid out the provider’s Play button at (284, 144) with a 72 × 72 CSS px
box in the 640 × 360 viewport. Each time, the native painter rejected
`DeclareMaskContent` at chunk 4, record 3 before submitting that frame.
Both the engine and player had IPv4 disabled. The engine reported Capsicum
mode and ECAPMODE on a path lookup. Limen reported no audio attribution
ticket during this run. Audible playback and chapter seeks on the real
provider still need verification. The corrected fixture used the multi-asset
helper and verified the installed public CA and hostname map.
See `~/atrium-vm/evidence/navigator-i4-youtube-channel-20261124/`.

The native painter collects mask declarations by effect ID. For a single
solid rectangular source, it keeps the mask geometry as a Fresco rectangle
or fill, colours it with the source RGB, multiplies the coverage by the
source alpha, and intersects the source and mask bounds. Opaque rectangular
masks can also clip overlapping source nodes. Translucent overlapping
sources and multi-paint masks still require isolated composition.
The host pixel fixture records a mask and draws through Fresco’s reference
renderer. It checks clear, half-alpha and opaque coverage across both clip
edges, plus the interior and transparent corner of an SVG triangle.
On Tessera, the isolated 192 px red ramp over white matched the reference
samples, including the transparent exterior at both edges. The provider's
title uses a horizontal fade over glyphs. The painter clips retained glyph
runs into disjoint alpha bands and supports the translated vector paths,
rational conics and single-path drop shadow in that frame.

The installed JIT-off worker submitted the frame. The centre of the Play
control remained black. Its settled DOM uses a red 72 × 72 div masked by
`https://www.gstatic.com/youtube/img/icons/mweb/youtube_fill/logo/v2/67px.svg`.
The embed grant admits scripts and styles on the embed origin only. A
trusted press at framebuffer (577, 312) reached the provider button. Its
first post-press frame stopped at chunk 13, record 1, `PaintTextShadow`.
The click establishes input delivery. Visible Play and playback are open.
Evidence is in `~/atrium-vm/evidence/navigator-i4-mask-20261124/`.

#### Play icon and post-press rendering on Tessera

For this embed, the fetch worker admits one additional anonymous GET for
the exact Play SVG. It checks TLS for www.gstatic.com, sends the embed
Origin, requires HTTP 200, SVG MIME and a matching or wildcard CORS grant,
and caps the body at 16 KiB within the existing startup budget. LibWeb
receives the verified CORS permission. The renderer has no network access.

In the installed JIT-off arm64 jail, the broker delivered 478 bytes. The
72 × 72 CSS button painted 2,866 red pixels with a white triangle at the
centre. The prior capture had 5,184 black pixels in the same rectangle.
A compositor press at (577, 312) produced a trusted click and replaced
the logo with the provider spinner. Text shadows use the pinned glyph atlas
and a bounded separable Gaussian with the recorder's layer order and offsets.
Fresco composites overlapping children in an isolated opacity group.
The spinner's sampled rotation preserves the drawing plane even when its
unused z-axis scale differs from one by floating-point roundoff.

The provider moved from cued (5) to buffering (3). The spinner continued
painting at the 11-second capture. The restricted loader refused the
`POST /youtubei/v1/player` request. No media delivery was verified.
The subsequent player POST gate below exercises that boundary. The private guest exited cleanly.

Evidence is in `~/atrium-vm/evidence/navigator-i4-play-icon-20261124/`.

#### Bounded player POST on Tessera

A trusted Start opens the fixed demo embed. Its private Nuntius channel
accepts at most two player POSTs in the first 120 seconds, within the existing
resource count and byte budget. The destination is the exact same-origin
`/youtubei/v1/player` with an optional `?prettyPrint=false`. LibWeb accepts
JSON with identity or gzip encoding. Both compressed input and inflated output
are capped at 32 KiB. The renderer inflates a single gzip stream before IPC.
The broker and TLS worker validate the video ID and embedded client, and
rebuild the body using the app embed identity and bounded playback fields,
including the provider's own visitor and integrity values when present.
No page-supplied headers, cookies or authorization are forwarded.
The courier accepts HTTP 200 JSON up to 512 KiB and does not follow redirects
or retry the POST.

On the installed arm64 Tessera gate, the first attempt exposed the startup
socket closing before Play. The interactive timeout fixed that. The next
attempt established that the provider sent gzip-compressed JSON. With both
fixes, the minimal player body received 4,136 bytes. The body including
embed identity, playback context and provider values received 4,103 bytes.
Both responses reported a playability error and no adaptive formats. The page’s player API reported
“This video is unavailable”.
Time, decoded frames and buffered ranges remained zero. Native VP9 and Opus
buffer creation and append probes recorded no calls. The error UI attempted
`PaintOuterBoxShadow` after the response, which stopped scene submission.
The guest exited cleanly. Evidence is in
`~/atrium-vm/evidence/navigator-i4-player-post-20261124/`.

#### Signed-out player comparison and error-screen shadow

Fresh signed-out Chrome and installed JIT-off Ladybird used the same demo
(`M7lc1UVf-VE`) and Atrium app referrer. Normal Chrome received 23 adaptive
formats and decoded 329 frames and 139,328 audio bytes. Re-encoding the
complete request and narrowing its page-controlled headers separately
preserved playback. Substituting Navigator’s bounded body reproduced the
provider error in Chrome. Keeping only the embed’s `encryptedHostFlags` in
the tested content-playback context restored playback in three fresh
profiles (319–328 frames). Omitting that one field from the complete
content-playback context returned zero formats, frames and audio bytes
in all three controls.

The broker and worker now preserve that optional field on the existing
fixed player POST, subject to a 2,048-byte token limit. The installed
Ladybird gate received 39,008 bytes, playability OK and 23 adaptive formats
in the arm64 Capsicum jail. Playback time, buffered ranges and decoded
frames remained zero. The next media gate should identify the first
format-selection or delivery failure before extending any network grant.

The native painter now blurs the offset rounded shadow and removes the
original rounded box interior. The pixel test also checks texture reuse.
The real provider error control advanced past the shadow and an elliptical
radial gradient. Its next failure was applying the mask to multiple leaves
that require isolated composition. The complete error frame remains open.
The painter suite passed 11 tests and the player request suite passed three.
Evidence, including both guest controls and the Chrome comparison, is in
`~/atrium-vm/evidence/navigator-i4-shadow-compare-20261124/`.

#### First YouTube media delivery and guest audio

Fresh signed-out Chrome used `--mute-audio`. Its successful player reply
listed 23 formats without individual stream URLs. Chrome sent a binary POST
to the response’s HTTPS `serverAbrStreamingUrl` on the media CDN.
Jailed Ladybird accepted the bare VP9 and Opus MIME strings but rejected
the qualified profile-0 eight-bit VP9 strings and capability parameters used
for selection. With the native capabilities fixed, the page offered 144p
through 720p, attached its MediaSource and selected separate native buffers.

The broker and TLS worker each bind delivery to the current successful player
response. They check the exact host and path and preserve the issued query
fields, allowing the embed’s transport nonce and four bounded playback
parameters. The grant permits 12 media POSTs during the existing 120-second
window with 8 KiB request bodies, 2 MiB replies and the shared 10 MiB budget.
The worker forwards no credentials or page headers. It verifies the response
type, exact CORS origin and credentials-mode permission before returning the
bytes to Fetch. A request for an unissued delivery host fails individually.

The first media response was 546,877 bytes. Native VP9 and Opus initialization
appends succeeded. The first real media appends of 65,825 and 125,150 bytes
exceeded the local fixture limit. The provider gate now accepts bounded
segments and drives LibWeb readiness when the tracks have a common buffered
range. The original local fixture retains its 64 KiB session budget.
The painter also handles rounded rectangles and axis-aligned images transformed
by the playing page’s mask frame.

On arm64 Tessera the JIT-off engine ran in Capsicum and both the renderer
and the embedded player had IP disabled. A test-only counter in the native
player observed at least 512 decoded frames through PTS 17.033 s. A guest
frame showed the presenter, and the page reported playing at 20.7 s.
LibWeb’s DOM frame counter and the headless presentation counter both stayed
zero. With the headless test seat set to `mailgate`, Choragus attributed a
48 kHz stereo stream to Navigator. The 13.488-second Lyra device capture
peaked at 21,087 (signed 16-bit PCM). The same capture with no active seat
was silent. Evidence is in
`~/atrium-vm/evidence/navigator-i4-media-selection-20261124/`.

#### Sustained YouTube playback and chapter seeking

On the same signed-out embed the first buffer ended at 20.0145 s. LibWeb
kept reporting enough data even after the native clock passed that point.
The clientless playback manager never started LibWeb’s playback-position
timer. The attached native MediaSource now starts and stops that timer with
playback, so LibWeb updates readiness and runs its normal media event
algorithm. The engine allows 15 s for a bounded CDN reply. At 5 s the
engine could close the channel while the broker was writing a successful
response. The provider grant, byte limits and exact delivery origin still
apply.

The installed demo links to Opening (5 s) and One minute (60 s). After
metadata arrives, the provider island reports media readiness and forwards
each typed chapter seek to its video element. The broker accepts seeks
during an open stream. Starting a decoder epoch clears the old clock
sample. Sequential decoding waits at a gap until more samples arrive,
including after seeking back into the first of two buffered ranges.

Three observations from the final JIT-off arm64 Capsicum runs on Tessera:

- Without a chapter press, the video reached 58.633 s with 60.0145 s
  buffered. The native counter reached frame 1,024 at PTS 34.100 s. The
  12-second guest Lyra capture peaked at 21,945 in signed 16-bit stereo.
- The 60 s link was pressed at 2.4 s with 3.200 s buffered. The new
  range started at 56.733 s. The player acknowledged 60 s and decoded
  frames at 60.633 s and 64.900 s. Guest PCM peaked at 21,271.
- The 5 s link returned to the first range. The player decoded a frame
  at 8.600 s and continued past 24 s as the opening range refilled.
  Guest PCM peaked at 21,087. Before the gap fix playback jumped from
  the opening section into the previously fetched one-minute section.

The run used the existing 12-POST grant and reached its limit. Full-video
completion and continuous frame presentation remain unverified. The
next playback gate should exercise bounded grant renewal and measure
presentation cadence against Lyra’s clock. The redacted event sequences,
guest WAVs and frames are in
`~/atrium-vm/evidence/navigator-i4-sustained-20261124/`.


The provider recorded a 1 × 180 vertical black-to-transparent gradient
tiled across a 640 px region under two rectangular clips. The painter
now retains same-space intersecting rectangular clips and represents a
gapless horizontal repetition of a full-tile vertical sRGB gradient as
one Fresco fill. A jailed control painted a 320 px wide ramp against white.
At x=310 and 540 the five sampled channel values matched (61, 142, 186,
234, 255), and the pixel beyond the right edge was white. The frame sent
464 bytes for five changed nodes and the next sample sent zero. The real
provider advanced to `DeclareMaskContent` at chunk 4, record 3. Mask
declaration and application in Fresco remain open. End-to-end Play,
real-provider media traffic and chapter seeking remain unverified.
See `~/atrium-vm/evidence/navigator-i4-youtube-readiness-20261123/`.

Host control scripts and observations are at
`~/atrium-vm/evidence/navigator-i4-youtube-provider-20261123/`.
Fingerprint verification is deferred.

#### Bounded delivery renewal and frame presentation

The broker and TLS worker account for startup resources and media delivery
separately. Startup retains its 10 MiB budget. The broker and courier must authorize
the player POST within the initial 120-second window. Media POSTs must
match the destination and signed fields from the authenticated player reply.
Each media window allows 12 requests and 8 MiB for up to 120 seconds. When
another maximum-sized reply would exceed that window, the broker must renew
before sending the request. Renewal on the private channel consumes a fresh
indication that native playback advanced in the last five seconds. A pause,
seek or end clears that indication. Both endpoints enforce an absolute limit
of 128 windows, 128 MiB and one hour per session. Each reply is capped at
2 MiB or the remaining session allowance, whichever is smaller.

Fresco imports a video buffer before its scheduled display time and can
release the buffer immediately. The supervisor now holds early decoded
frames in the existing decoder buffer queue until they are within the
sink’s 60 ms submission lead. Polling still services worker records and
audio while video waits. The headless compositor schedules queued video at
its frame cadence and samples diagnostic PNGs separately.

Installed Navigator on arm64 Tessera played the same signed-out embed in
JIT-off Ladybird. The engine reported Capsicum mode, and both the engine
and player jails had IPv4 disabled. The final broker trace recorded 18
successful media responses on the issued destination, including six
after the first renewal. Both chapter presses landed (5 s and 60 s).
A guest Lyra capture after renewal peaked at 22,172 in signed 16-bit stereo.

Three trials per configuration used the same 1280 × 840 headless display,
player fixture, sound device and sampling schedule. A test build recorded
native frame submissions, their Lyra-mapped deadlines and Fresco's landed
events; matching sequence numbers gave the signed landing error. The
player-only and original builds were alternated before the compositor change.

| Configuration | Frames landed per trial | Median submission lead per trial | Median landing minus Lyra target per trial |
|---|---|---|---|
| Original player and compositor | 1, 1, 0 | 42.8, 26.3, 76.1 s | No matched audio-clock landings |
| Paced player and original compositor | 146, 152, 152 | 58.0, 58.4, 56.6 ms | +42.7, +30.3, +42.0 ms |
| Paced player and updated compositor | 515, 521, 527 | 57.6, 57.7, 58.6 ms | +1.6, +2.3, +1.4 ms |

The final trials reported 1, 6 and 9 dropped frames. Their 95th-percentile
absolute landing errors were 18.0, 39.0 and 20.3 ms over 39, 39 and 37
sampled matches. The guest's dummy device produced 25,237 stereo frames
per elapsed guest second during a 12-second observation at a nominal
48 kHz. Median landed intervals were about 62 ms, consistent with the
guest audio mapping. A hardware-display refresh measurement is still open.

The earlier request-count renewal trials and their raw timings are in
`~/atrium-vm/evidence/navigator-i4-renewal-cadence-20261124/`.

#### Full-clip continuation on Tessera

Installed Navigator played the public demo from zero to 1,343.567 s after
one Play press and zero chapter activations in JIT-off Ladybird. Over 453
media replies the restricted broker delivered 39,445,202 bytes and renewed
37 times. The final buffered range reached 1,343.6745 s. The last sampled
native video submission had PTS 1,343.467 s (every fifteenth submission
was sampled). A final-minute Lyra excerpt contained 144,000 stereo frames
with a signed 16-bit peak of 21,287. The provider entered state 0 (ended).

The full run exposed a mismatch at the end. The DOM still reported
`ended=false` because LibWeb consulted its unused playback manager.
The broker now exposes the native player's drained end state on the clock
channel. LibWeb runs its existing end-of-media steps once on that transition,
even when the last timestamp has already been reported. A fresh installed
run sought to 1,325 s, filled the final range and finished at 1,343.567 s
with the provider in state 0, the element paused, `ended=true` and exactly
one DOM end event. The end-event fix was verified with that final chapter.

The virtual HDA sink negotiated 48 kHz stereo S16. Three 12-second guest
intervals produced 47,637, 47,328 and 44,352 frames/s. Of the first 4,096
landed-event records, 4,095 were parseable. Their median interval was
34.28 ms. The 280 matches with sampled audio deadlines had a median signed
landing error of +8.73 ms. The intervals measure one virtual-device run.
Physical display cadence remains open. The following gate addresses the DOM frame count.

Guest captures, the progress log and the focused end-event observations
are in `~/atrium-vm/evidence/navigator-i4-fullclip-20261124/`.

#### Native playback quality

For the attached native MediaSource, Ladybird reads the player’s latest
presentation counts through the existing private element channel.
`totalVideoFrames` is the sum of frames Fresco presented and frames it
reported dropping. `droppedVideoFrames` is the latter count. A decoded
frame can still be waiting for presentation or belong to a previous seek.
Each new player starts at zero, and a chapter seek keeps the same counters.
The ordinary LibWeb sink supplies the counts for other video elements.

Installed JIT-off Ladybird on arm64 Tessera started at zero and reported
407 total, four dropped at 13.5 s. The 1,325 s chapter seek preserved the
counters. At the end the DOM reported 983 total and four dropped. The native
trace recorded 1,011 decoder outputs, 983 submissions, 979 Fresco presentation
acknowledgements and four compositor drops. Of the 28 decoder outputs that
never reached submission, eight belonged to the old seek epoch and 20 preceded
the target in the new epoch. The element ended once at 1,343.567 s. A fresh
engine process reported zero for both counters before Play. Evidence is in
`~/atrium-vm/evidence/navigator-i4-frame-counts-20261124/`.

#### Error-screen mask

The provider error screen paints a dark rectangle and a horizontal colour gradient
under one radial mask. Previously Ladybird rejected the second child and Navigator
kept displaying the previous frame. Ladybird now sends the coverage fill and source
children as a native Fresco group. Fresco applies coverage after composing the children.

On arm64 Tessera the installed JIT-off engine rendered a horizontal mask over
overlapping red and blue rectangles. All 14,400 sampled RGB pixels matched the
independent linear-light blend exactly. Both adjacent columns remained white.
The provider-error control returned `ERROR`. After Play, the sheet showed
“This video is unavailable”, error code 152 - 18, and the purple gradient.
The engine ran as uid 50007 in capability mode. Screenshots, pixel measurements
and renderer tests are in `~/atrium-vm/evidence/navigator-i4-multimask-20261124/`.

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

### I3 VPN gateway on Tessera

On 2026-11-22 Portcullis accepted `VpnSignInWindow` for profile `win`
and launched the signed Navigator broker in the reserved `vpn` persona.
The installed UI opened **Sign in to corp** through separate Arca couriers
to the gateway and Keycloak 26.0.7. The broker now uses only the profile's
couriers for VPN sign-in. This run had no plain fetcher bundle installed.
All 22 broker unit tests passed on arm64 FreeBSD.

The initial run requested the Limen document viewer. A separate
JIT-off Ladybird rendering probe read the guest-fetched Keycloak login HTML,
found the username, password and submit controls, and built a native Fresco
scene with 13 node changes. The non-root worker verified its jail, Capsicum
mode and denial of a filesystem lookup. This exercised the initial document
only. At that point the installed window still needed a live page role, brokered resource
and form navigation, and the trusted Arca password field.

The host gateway control verified Keycloak's signed response and minted a
token, rejected a repeated signed response with HTTP 403, reused the IdP
session without a password, and asked for a password after clearing its
browser jar. The fixture now resolves Keycloak's key name to the configured
IdP certificate with xmlsec 1.3. These controls used the fixture browser.
Arca profile deletion, Crypta token consumption and replay refusal, and
traffic through ocserv were pending at that point. The VPN
team's separate ocserv backend was offline during this run.

Evidence on the Mac is in `~/atrium-vm/evidence/navigator-i3-vpn-sso-20261122/`.

The installed VPN window requests the fixed `vpn-sign-in-page` worker through
its delegated Limen connection. Limen verifies the Navigator broker's app
identity, publisher and reserved persona. Navigatord passes the bounded HTML
response and its final HTTPS address to the window. The window forwards the
sealed descriptor to the matching embed. Ladybird paints the page and reports
forms from the parsed DOM. The trusted window embeds Arca's credential field,
and navigatord permits its handle only on an HTTPS POST to the page's origin.
The converter recording still supplies the broker's form checks.

The installed gate subsequently passed on a private arm64 HVF Tessera root.
Fresco initially refused the submit press because Arca's embedded surface
overlapped the guarded form. The heading, ordinary fields and submit buttons
now occupy the guarded area, with Arca's surface immediately below it.
The broker reads the 16-byte Arca handle through the bounded application
reply reader. The worker can recover a fixed-value SAML POST and its
noscript Continue button from the sealed response when the parsed DOM has
no actionable form. Sending it still takes a separate guarded press.

The installed Keycloak window reused its session on reopening. The gateway
verified the signed assertion, accepted Crypta's token and refused the same
auth-reply when repeated over a new TLS connection. Fetching
`http://10.98.0.1/` through ocserv on `tun2` returned `crypta-ac-ok`.
Deleting `win` removed it from Crypta's list. Recreating the same name
brought back the username and Arca password fields. A fresh sign-in,
assertion and redemption opened `tun3` and returned the same sentinel.
The local gateway exercised the replay in memory. Evidence is in
`~/atrium-vm/evidence/navigator-i3-vpn-installed-20261122/`.

The optional external-script gateway fixture (`CRYPTA_SSO_EXTERNAL_JS=1`)
adds a page before the Keycloak redirect. Its Continue button runs an
external same-origin script that fills a per-session value and calls
`requestSubmit()`. The gateway refuses an empty POST, a different session
and a second submission. For this fixture the broker fetches the named script
through the profile courier, checks MIME, status, final URL and size, and
seals it with the HTML. Ladybird's proposed URL-encoded POST goes to the
trusted window for confirmation. Host fixture controls and framing tests
passed. On 2026-11-22 the installed JIT-off Ladybird page initially offered
zero plain forms. Clicking Continue on the page produced the trusted form.
Confirming it sent a 38-byte POST that the gateway accepted. Arca held the
typed Keycloak password, the gateway accepted the signed SAML response,
and Crypta redeemed the token. The arm64 Tessera guest recorded
`handshake=1` and routed `10.98.0.1` through `tun0`. Its HTTP GET
returned exactly `crypta-ac-ok`. Both VMs shut down cleanly. Redacted logs
and binary hashes are in `~/atrium-vm/evidence/navigator-i3-vpn-external-js-20261122/`.

The follow-up batched an abandoned proposal with a fresh sign-in. The
gateway's loopback stats count external script responses, attempted POSTs,
and accepted POSTs separately. Pressing the page's Continue button displayed
a proposal in trusted chrome with zero POSTs at the gateway. The guest had
no VPN handshake and could not fetch the sentinel. Disconnecting the profile
and closing its test window left the proposal unsent. Reopening fetched a
new gateway flow and again sent nothing until the trusted confirmation.
The retry recorded one script POST, one accepted assertion, and one token
redemption. The guest fetched `crypta-ac-ok` through `tun3`. Both dedicated
guests shut down cleanly.

Ctrl-R in the focused VPN page now goes to trusted chrome, which clears
the pending form immediately and fetches the current page in the same
sign-in window. The arm64 Tessera gate verified that the old proposal
disappeared, the gateway served a fresh script, and no POST, assertion,
or token redemption occurred before confirmation of the new proposal.
After confirmation the gateway accepted exactly one script POST and one
signed assertion. Crypta redeemed one token and fetched `crypta-ac-ok`
over `tun0` on the same engine start. The focused FreeBSD unit test also
passed. Both dedicated guests shut down cleanly.

A second gate pressed Ctrl-R twice in the same sign-in window, each time
with a new script-generated proposal pending. Each reload removed the
proposal and fetched a fresh script. The gateway received zero POSTs for
either discarded proposal, and the guest had no handshake or sentinel
response during either reload. The third proposal stayed unsent until
trusted confirmation. The gateway then recorded exactly one script POST,
one accepted SAML assertion, and one token redemption. Crypta fetched
`crypta-ac-ok` through `tun0` without another engine start. This run reused
the verified binaries from the shared-cache build.

The next installed gate disconnected after the two reloads while the
third proposal was pending. The profile went down and the gateway still
recorded zero POSTs, signed assertions, and token redemptions. The gate
closed the old test window and opened sign-in again after reconnecting.
The second engine and fresh gateway flow had no handshake or sentinel
response before trusted confirmation. After confirmation the gateway
recorded one script POST, one accepted SAML assertion, and one token
redemption, and the FreeBSD guest fetched `crypta-ac-ok` over `tun1`.
The gate reused the verified shared-cache binaries.

The automatic-dismissal gate repeated the two reloads and disconnected
with a proposal pending. Navigator detected that its Crypta attempt had
ended and exited on its own. The gate waited for its jail to disappear,
saw zero POSTs, and then reconnected. Portcullis bound the reopened
window to a distinct Crypta attempt. The new proposal and IPv4 traffic
waited for trusted confirmation. The gateway recorded one POST,
one accepted assertion, and one token redemption. The guest fetched
`crypta-ac-ok` through `tun1`. Both dedicated guests shut down cleanly.

The immediate-reconnect variant briefly paused the old broker to ensure
Crypta could disconnect and begin a distinct attempt while the old window
still existed. Portcullis refused to reuse that window, and a press on its
old confirmation produced zero POSTs. The old window closed on its own
when the broker resumed. The fresh window waited for trusted confirmation
with no tunnel handshake or sentinel response. Confirmation produced
one script POST, one accepted assertion, and one redemption. The guest
then fetched `crypta-ac-ok` over `tun1`.

The queued Sign In gate made one request during that overlap. Portcullis
held it against the fresh Crypta attempt until the old jail finished
teardown, then launched a distinct jail without another request. A press
on the stale confirmation produced zero POSTs. The new proposal and
tunnel waited for trusted confirmation. The gateway recorded one POST,
one accepted assertion, and one redemption, and `tun1` returned
`crypta-ac-ok` through ocserv. Both dedicated guests shut down cleanly.

The cancellation gate disconnected again while the new Sign In request
was queued. Portcullis discarded it before the old broker resumed. The
old jail then exited without a queued launch, and the gateway counts
showed zero POSTs, zero accepted assertions, and zero redemptions.
A later reconnect opened sign-in for a distinct attempt. The gateway
received one POST after trusted confirmation, and the guest fetched
`crypta-ac-ok` through `tun2`.

The requeue gate made the next reconnect and Sign In request while
the original window still existed. Portcullis discarded the canceled
request and launched only the latest window after teardown. The gateway
served just one new sign-in page and recorded zero POSTs until trusted
confirmation. The first run exposed an Arca handle-revocation race.
Navigator now keeps the field attached until the broker replies to the
form. The rerun recorded one script POST, one accepted assertion, one
token redemption, and `crypta-ac-ok` through `tun5`.

The in-place replacement gate held the queue worker outside the lock in
a fixture-only build. Portcullis logged one enqueue and one replacement
while the original jail was still open. After teardown it launched one
window for the newest attempt. The gateway received zero stale POSTs,
and the new tunnel had no handshake or IPv4 sentinel response before
trusted confirmation. Confirmation produced one script POST, one accepted
assertion, one redemption and `crypta-ac-ok` through ocserv on `tun2`.

The follow-up used one VM session for two in-place replacements and
cancellation of the newest request. Portcullis discarded the request
while the original jail was still open. After teardown no queued window
launched, and gateway page, script and POST counters stayed unchanged.
A separate reconnect used a distinct Crypta attempt and a fresh
proposal. There was no handshake or IPv4 sentinel response before
confirmation. The confirmed proposal produced one POST, one accepted
assertion and one redemption. The guest fetched `crypta-ac-ok` through
ocserv on `tun8`.

The next gate held the original jail open across two replacements,
cancelled the newest attempt, then reconnected and requeued Sign In
while that jail still existed. Portcullis replaced the cancelled queue
entry in place. After teardown only the latest window launched, with no
additional Sign In press. The gateway saw zero POSTs and IPv4 stayed
blocked before fresh confirmation. The confirmed proposal produced one
POST, one accepted assertion, one redemption and `crypta-ac-ok` through
ocserv on `tun4`.

The follow-up disconnected while the latest requeued window still had
an unconfirmed proposal. The jail closed, the gateway saw zero POSTs,
and the IPv4 sentinel was unreachable. Reconnect used a distinct
Crypta attempt and required a fresh proposal and confirmation. Before
confirmation there was no handshake or IPv4 sentinel response. One
confirmed POST led to one assertion and one redemption, and the guest
fetched `crypta-ac-ok` through ocserv on `tun5`.

The overlap gate kept that requeued window open across disconnect and
queued the next Sign In against a distinct live Crypta attempt. A press
on the stale confirmation sent zero POSTs. After teardown Portcullis
launched the queued window without a second Sign In request. The new
proposal and IPv4 traffic waited for fresh confirmation. That press
produced one POST, one accepted assertion, and one redemption, and the
guest fetched `crypta-ac-ok` through ocserv on `tun5`.

The next gate disconnected again before the queued request could launch.
Portcullis discarded it while the old jail was still open. After that
jail closed, no new window opened and gateway page, script and POST
counters stayed unchanged. A separate reconnect used a distinct Crypta
attempt. Its new proposal and IPv4 traffic waited for fresh
confirmation. One confirmed POST produced one accepted assertion and
one redemption. The guest fetched `crypta-ac-ok` through ocserv on
`tun6`.

The next gate waited for the second disconnect to discard queued
Sign In, then reconnected and queued a new request while the old jail
was still open. After teardown only the newest request launched, with
no additional Sign In press. The stale confirmation sent zero POSTs.
The new attempt had no handshake or IPv4 sentinel response until fresh
confirmation. One confirmed POST led to one accepted assertion, one
redemption, and `crypta-ac-ok` through ocserv on `tun6`.

The follow-up disconnected while the newest requeued proposal was
pending. Its jail closed with zero POSTs or redemptions, and the
IPv4 sentinel was unreachable. A separate reconnect used a distinct
Crypta attempt. The gateway still recorded zero POSTs and IPv4 stayed
blocked until fresh confirmation. Confirmation produced one POST,
one accepted assertion, and one redemption, and the guest fetched
`crypta-ac-ok` through ocserv on `tun7`.

The overlap variant disconnected that newest requeued proposal, kept
its jail open and queued Sign In against the next Crypta attempt. A
press on the stale confirmation sent zero POSTs. After teardown the
queued window opened without another Sign In request. IPv4 and the new
proposal waited for fresh confirmation. One POST produced one accepted
assertion, one redemption, and `crypta-ac-ok` through ocserv on `tun9`.
The rerun restored the verified converter fixture after an installed
binary hash mismatch caused the initial page to fail.

The cancellation variant disconnected again while that request was
queued. Portcullis discarded it before the prior jail closed. No new
window launched, and the gateway served no additional page or script
and received zero POSTs. A separate reconnect used a distinct attempt.
IPv4 stayed blocked until fresh confirmation. The gateway then received
one POST, accepted one assertion, and redeemed one token. The guest
fetched `crypta-ac-ok` through ocserv on `tun8`.

The follow-up cancelled that queued request, reconnected on a distinct
Crypta attempt and queued Sign In before the old jail closed. After
teardown only the newest request launched, without another Sign In
press. The stale confirmation sent zero POSTs. IPv4 stayed blocked
until fresh confirmation sent one POST, and the guest fetched
`crypta-ac-ok` through ocserv on `tun8`.

The pending-proposal variant disconnected that newest requeued attempt
after its proposal appeared. The jail closed, the gateway received zero
POSTs, and the IPv4 sentinel was unreachable. The next attempt used a
fresh window and required confirmation before it sent one POST and
completed the ocserv handshake. The guest fetched `crypta-ac-ok` on
`tun9`.

The follow-up kept that latest requeued window open during disconnect
and queued Sign In for a distinct Crypta attempt. A press on the stale
confirmation sent zero POSTs. The queued request opened a fresh window
after teardown. Neither the tunnel handshake nor IPv4 sentinel
response was available before fresh confirmation. One confirmed POST
led to one redemption and `crypta-ac-ok` through ocserv on `tun9`.

The cancellation variant disconnected again before that queued request
could launch. Portcullis discarded it before the prior jail closed.
No new window opened, gateway page and script counts held steady, and
the gateway received zero POSTs. The separate reconnect required fresh
confirmation before its first POST and tunnel handshake. The guest
fetched `crypta-ac-ok` through ocserv on `tun10`.

The follow-up reconnected after that cancellation and queued Sign In
while the prior jail was still open. Portcullis launched only the
newest queued request after teardown, with no additional Sign In
press. The stale confirmation sent zero POSTs. The tunnel handshake
and IPv4 sentinel waited for fresh confirmation. The gateway received
one POST, and the guest fetched `crypta-ac-ok` through ocserv on
`tun10`.

The follow-up disconnected that newest requeued attempt with its
proposal pending. The jail closed with zero POSTs. The gateway page
and script counters stayed unchanged, and the IPv4 sentinel was
unreachable. A separate reconnect used a distinct Crypta attempt.
Fresh confirmation produced one POST, one redemption, and
`crypta-ac-ok` through ocserv on `tun11`.

The overlap variant disconnected that newest requeued proposal and
queued Sign In for a distinct attempt before the old jail closed. A
press on the stale confirmation sent zero POSTs. The queued window
opened after teardown without another Sign In request. The tunnel
handshake and IPv4 sentinel waited for fresh confirmation. Exactly
one POST led to one token redemption and `crypta-ac-ok` through ocserv
on `tun11`.

The cancellation variant disconnected while that request was queued.
Portcullis discarded it before the prior jail closed. No new window
opened, gateway page and script counts stayed unchanged, and the
gateway saw zero POSTs. A separate reconnect used a distinct attempt
and kept IPv4 blocked until fresh confirmation. Confirmation produced
one POST, one redemption, and `crypta-ac-ok` through ocserv on `tun12`.

The requeue variant reconnected after cancellation and queued a
distinct Crypta attempt before the old jail closed. The latest queued
request alone launched after teardown, with no additional Sign In
press. The stale confirmation sent zero POSTs. The new proposal,
tunnel handshake and IPv4 traffic waited for fresh confirmation.
Exactly one POST led to one redemption and `crypta-ac-ok` through
ocserv on `tun12`.

The follow-up disconnected the newest requeued window with its
proposal pending. The jail closed with zero POSTs, gateway page and
script counts stayed unchanged, and the IPv4 sentinel was unreachable.
The next attempt used a distinct Crypta identifier. Fresh confirmation
sent one POST, redeemed one token, and brought up the ocserv tunnel.
The guest fetched `crypta-ac-ok` on `tun13`.

### I3 installed passkey sign-in

On 2026-11-21 Mail opened its Microsoft action in Navigator on an arm64
HVF overlay with Tessera at root. The local TLS provider issued a fresh
challenge to the cookie-bound page. Ladybird's native Rust binding returned
a `PublicKeyCredential` with an `AuthenticatorAssertionResponse`. The
fixture checks both prototypes and submits their binary fields. The provider
checked the enrolled key and user handle, client-data origin, challenge,
RP hash, user presence and verification flags, counter and P-256 signature.
Only then did it return the OAuth redirect. The provider recorded
`assertion-verified` followed by `pkce-redeemed`. Reopening the new Mail
store found one account with separate nonzero IMAP and SMTP references.

The interpreter-only page ran under Capsicum as UID 50006, with IPv4
and IPv6 disabled and no Sigillum socket. Its sign-in embed carries a
bounded get request with a sheet-local ID. Navigatord derives the origin
from the courier's final document URL, checks the RP ID, and opens the
Sigillum client socket under its signed app identity. The page can request
cancellation, and dropping the sheet closes the ceremony connection. In
this gate the guest ran the real Sigillum daemon, trusted UI and ephemeral
software-key driver under their dedicated service UIDs. Fresco consumed
the device-selection press for the sheet's window token after checking
focus and visibility. Cancelling in a second trusted sheet returned
`NotAllowedError` to the page and produced no callback or code exchange.

For this build, apply `patches/ladybird/webauthn-get.patch` to the pinned
Ladybird source before regenerating the IDL bindings. Apply
`webauthn-submit.patch` after `fixture-fetch.patch`. Link the generated
public-key and authenticator response interfaces, window exposures and
intrinsics with the editor painter. The credential bridge is enabled for
the sign-in document's realm. Only the local sign-in fixture accepts the
assertion POST to its fixed continuation endpoint.

Cross-build the `navigator-softkey` example from the `sigillum-fido` crate.
Run the resulting guest binary as the Sigillum device UID.
Supply its public enrollment line to `courier-test-server` after the
`webauthn-sign-in` mode argument. Set the signed Navigator publisher for
both Limen and Sigillum. Point the trusted UI's `FRESCO_ACTIVATION` at the
running display service. In a headless gate, run the Forum example
`place_sigillum_fixture` from the directory containing the WM socket to
place the sheet below the trusted strip before choosing the device.

This gate covers local WebAuthn get with a pre-enrolled key. Registration
and live Microsoft remain separate. The subsequent cancellation gate is below. Screenshots, stage markers,
binary hash, confinement and shutdown logs are in
`~/atrium-vm/evidence/navigator-i3-webauthn-bridge-20261121/`.

### I3 cancellation and sign-in sheet geometry

On 2026-11-21 the installed JIT-off Ladybird worker ran in Capsicum in an
arm64 Tessera jail with IPv4 and IPv6 disabled. **Test page abort** called
the native get with an AbortSignal and aborted it after five seconds.
Sigillum saw the pending assertion end with `client gone`. The page
displayed `AbortError`. A fresh get in that same page reached the PIN
sheet, the fixture provider verified the signature and Arca redeemed the
code. Reopening the Mail store found one account with distinct references.

In a separate session, pressing Navigator's **Cancel sign-in** while
Sigillum was open retired its ceremony and the old page worker. The
provider log stayed byte-for-byte unchanged. Mail returned to its empty
account form. Adding the account again launched a new page (JID 46,
previously 41), and sign-in and the persisted-account check passed again.
Use a Mail build containing the sheet-cancellation handler (`403b5b3a`
or later). The original callback backing image predates that handler.

The headless gate places the 1040 × 720 Settings window at (120, 72)
on a 1280 × 840 desktop below the trusted strip. Run the Forum
`place_sigillum_fixture --settings` example while Settings is open,
then run it without arguments with the trusted sheet open to cross the
page's bottom edge. The 640 × 360 live page began at (304, 156) and the
440 × 270 trusted sheet began at (360, 320). The old chrome painted 263
amber pixels across the foreground sheet in the earlier overlapping layout.
The corrected device-choice and PIN screens each had 101 amber pixels on
the sampled exposed bottom edge and zero in the sheet interior. The title,
relying-party hostname and Cancel label were readable. A fresh sign-in
on the corrected layout produced a third verified signature and redemption.

Before and after screenshots, guest unit tests, provider markers and shutdown evidence
for cancellation and sheet geometry are in `~/atrium-vm/evidence/navigator-i3-cancel-layout-20261121/`.

### I3 read-only input and Mail sidebar

On arm64 Tessera, Ladybird's input value, default value and shadow text all
matched the fixture address. The styled input painted 16 glyphs with a
baseline at (128.203, 101.875) inside a clip beginning at (128, 88).
The native painter subtracted the clip origin before handing the run to
Pergola, whose placement encoder subtracted it again. The run landed
outside its clip. The painter now passes document coordinates for both
glyphs and rectangles and leaves the single conversion to Pergola.

On reopening the sheet from Mail with the corrected JIT-off worker in
Capsicum, OCR of the 172 × 18 field interior at (432, 244) read
`ada@example.test`. The worker's jail had IPv4 and IPv6 disabled.
The empty Mail sidebar also advances past its account label and action
before drawing Views. The four rows below Search had disjoint ink bounds.
Screens, field crops, sidebar bounds, worker identity and build evidence
are in `~/atrium-vm/evidence/navigator-i3-readonly-sidebar-20261121/`.

### I3 passkey registration and sign-in

The local fixture ran on arm64 HVF with Tessera mounted at root. The
installed Ladybird worker used the JIT-off engine, ran in Capsicum as UID
50006, and had IPv4 and IPv6 disabled. The software authenticator started
with an empty credential store. Pressing **Create passkey** in the page
called `navigator.credentials.create`. Navigator derived the origin from
the attached document and Sigillum presented its trusted device and PIN
sheet. The native Promise resolved with an `AuthenticatorAttestationResponse`.

The provider checked the creation challenge, origin, RP hash, user
presence and verification flags, credential ID, and ES256 key in the
attestation object. It kept the registered public key under the flow
cookie. **Use new passkey** supplied that ID in the allow list for an
independent challenge. The provider checked the returned ID, user handle,
counter and signature under the enrolled key. Only the assertion produced
the OAuth callback. Arca redeemed it and Mail displayed the fixture account.

For this gate apply `patches/ladybird/webauthn-create.patch` after
`webauthn-get.patch`, regenerate the bindings and also link the generated
AuthenticatorAttestationResponse object. Start `navigator-softkey --empty`
and select `webauthn-register` in the local courier test server. Keep the
fixture connection open after the registration POST returns 204 so the
same courier can submit the assertion. The provider markers in order are
`registration-verified`, `assertion-verified` and `pkce-redeemed`.
The soft-key ceremony ran independently of the Arca platform-passkey list,
whose lookup returned status 5 in this fixture. The local provider,
ephemeral key and fixture account cover a single run. Evidence is in
`~/atrium-vm/evidence/navigator-i3-registration-20261121/`.

### I3 registration cancellation and credential exclusion

On 2026-11-22 the installed JIT-off Ladybird worker ran in Capsicum on
arm64 Tessera with IPv4 and IPv6 disabled. Starting with an empty software
key, Cancel on Sigillum’s registration sheet returned `NotAllowedError`
to the page. The provider received no registration. Retrying from the same
page completed registration. **Try duplicate** sent an independent
challenge and the enrolled raw ID in `excludeCredentials`. Sigillum logged
`excluded` and Ladybird displayed `InvalidStateError`.

**Use new passkey** then put the original ID in `allowCredentials`.
The provider verified the assertion under the public key retained from
the sole registration, and Arca redeemed the PKCE code. The four Sigillum
ceremonies ended in order with cancellation, registration, exclusion and
assertion. The flow’s provider log contains one `registration-verified`,
one `assertion-verified`, and one `pkce-redeemed`.

An initial UI driver press landed on the adjacent sign-in button because
OCR grouped both button labels. The complete run above used a fresh soft
key and flow, and targeted the individual button bounds. Screenshots,
per-flow markers, ceremony log, worker hash, jail credentials and shutdown
evidence are in `~/atrium-vm/evidence/navigator-i3-exclusion-20261122/`.

### I3 Arca platform-passkey batch

The soft-key fixture started arcad directly without its production
`ARCA_SERVICES` mapping. Sigillum's `P_LIST` request then received
status 5 (`UNKNOWN_PEER`). The shipped arcad manifest already identifies
`sigillumd=65028`. In the private fixture, set
`ARCA_SERVICES="sigillumd=$(id -u _sigillum)"` on arcad and unlock the
fixture user's store. Run Ostiarius's verify socket with the PAM-enabled
Portcullis broker for platform user verification.

Select `webauthn-platform` on the local provider. The page requests platform
attachment and checks the returned attachment before it submits either
registration or assertion. The installed JIT-off Ladybird worker on arm64
Tessera ran in Capsicum as UID 50006 with both IP stacks disabled.
Jaild launched Ostiarius as UID 50093 in its service jail. Its verify socket
forwarded Sigillum's passcode checks to Portcullis, which loaded libpam and
used the atrium-login PAM stack. The fixture ran without a roaming key.

Cancel returned `NotAllowedError`. On retry the wrong passcode produced a
visible rejection and no enrollment. The correct passcode registered one
platform credential. Trying to register its ID again returned
`InvalidStateError` and Sigillum logged `excluded (arca)`. A separate
assertion required the passcode again. The provider verified the signature
against the original registered key and fresh challenge; Arca redeemed the
PKCE code and Mail displayed the fixture account.

The first provider rejected the valid assertion because its counter check
required an increase even when both counters were zero. Arca reports zero
for its platform passkeys. The fixture now accepts zero followed by zero;
a nonzero counter must advance. All five provider tests passed. A fresh
authorization flow on the same overlay produced exactly one each of
`registration-verified`, `assertion-verified` and `pkce-redeemed`.
The screenshots, logs, worker hashes and shutdown evidence are in
`~/atrium-vm/evidence/navigator-i3-platform-20261122/`.

### Sign-in pages: module scripts, IdP resources and the painter (2026-11-24)

A sign-in page (`ATRIUM_MAIL_PAGE`) now runs "the end" of parsing
(`HTMLParser::run(url)` in `SignInFixture.h`), so module and deferred
scripts run and DOMContentLoaded and load fire. Before this, an IdP page
built from module scripts (Keycloak's) never ran them. A VPN sign-in page
fetches its own origin's scripts and stylesheets through its window, and a
form its script submits goes to the window's panel. See vpn.md "Scripts in
the sign-in window". The native painter now accepts clips inside a
scrolling viewport's content (a text field's in a page taller than its
window). It also accepts stationary scrollers below the viewport, and
uniform scale and translate transforms (`pergola::Node::mapped` moves each
leaf; clips in transformed spaces move with it). It takes rounded clips as
their rectangles, draws per-side border widths, and leaves out an outer
shadow over its raster budget with a log line. Other transforms are still
refused, and errors name the spatial kind and the clip that did not fit.
A VPN sign-in page gets a 1024 x 600 viewport and window. A Mail sheet page
keeps 640 x 360.

### I3 physical YubiKey on Tessera

On 2026-11-23, installed Navigator's JIT-off Ladybird sign-in worker ran
WebAuthn create and get against the local TLS fixture using a USB YubiKey
(1050:0406). The arm64 guest mounted Tessera at `/`. The worker ran as uid
50006 in Capsicum mode with IPv4 and IPv6 disabled. Dispensator assigned
`/dev/u2f/0` mode 0600 to `_sigillum-dev` in the network-disabled
`atrium-sigillum-fido-0` jail. Sigillum received both requests from
`org.atrium.navigator`. Its trusted terminal client handled PIN and touch.

The provider's `webauthn-physical` mode requests a cross-platform credential,
discourages resident storage, and requires user verification. The sign-in
page passed the registration's raw credential ID in `allowCredentials`.
The provider verified the origin, RP hash, fresh challenge, presence and
verification flags, counter and ES256 signature. Arca redeemed the PKCE
code. The terminal client and local provider delimit the coverage of this run.
Evidence is in `~/atrium-vm/evidence/navigator-i3-physical-yubikey-20261123/`.

### I3 installed WebAuthn get probe

On 2026-11-21 the installed Navigator sign-in sheet opened a local TLS
provider through Arca on arm64 Tessera. The provider logged authorization,
the cookie-authenticated first screen, stylesheet and WebAuthn script.
Pressing **Use passkey** called `navigator.credentials.get({ publicKey })`.
The page displayed **Passkey failed: InternalError**. The engine logged
`FIXME: Unimplemented CredentialsContainer::get()`. The worker ran in jail
15 with Capsicum active and IPv4 and IPv6 disabled. Its SHA-256 matched the
JIT-off Ladybird binary from the earlier callback gate.

At the pinned Ladybird revision, `CredentialRequestOptions` lacks a
`publicKey` member. The Rust implementation of `CredentialsContainer::get`
ignores the options and rejects the promise. The request stopped there.
In the diagnostic build, navigatord's sign-in sheet accepted resource
requests on its page channel and lacked a WebAuthn request handler. No trusted Sigillum sheet
opened, the provider issued no callback, and Arca redeemed no code.

To reproduce this first hop, use the callback gate's local provider and
choose `webauthn-probe` as the final argument to `courier-test-server`.
The fixture explicitly refuses the earlier GET-only callback shortcut.
The separate `sigillum-webauthn --test e2e` suite passed all six tests in
the same VM, including RP signature verification with the software key,
an Arca passkey, and cancellation. Those tests use a scripted sheet peer.
At that point the installed gate still needed a native public-key request and response
binding, a cancellable request tied to the sheet and its courier-verified
document origin, and a provider-issued challenge and verified assertion
before OAuth completion. Screenshots and logs are in
`~/atrium-vm/evidence/navigator-i3-webauthn-20261121/`.

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
7. **I6. Whole-page live code** for fat clients in everyday browsing, on a press
   (§9 D5). Planned in [I6 plan](#i6-plan-whole-pages-live-on-a-press-accepted).

### I5 installed-worker hardening on Tessera

The local chapter gate exercises a fresh installed engine per guarded Start.
For the symbol audit, keep the unstripped link, strip a copy with the LLVM
toolchain used to package it, and compare its SHA-256 with the installed
worker's digest. Run `scripts/navigator-i5-worker-audit.sh UNSTRIPPED SHA256`
on the Mac. It checks both interpreter entry points and searches the defined
symbols for the LibJS and LibWasm JIT compiler crates. The audited VP9/Opus
worker and its native player must use the same media channel revision.

On FreeBSD run `scripts/navigator-i5-worker-check.sh PID 1024 100` while the
local video fixture is active. In this fixture the manifest specifies 1 GiB
resident memory and 100% of one CPU. Check the jail's kernel rules, Capsicum,
IP settings and executable mappings. In both sampled workers, 15 executable
mappings belonged to the installed bundle. The only other executable mapping
was the kernel shared page containing `AT_TIMEKEEP`. All 16 were read-only.
These are mapping samples during execution. The binary audit covers the linked
compiler crates separately.

On arm64 Tessera the forced exit of PID 6329 (jail 70) left the document and
the “The live widget stopped.” note on screen. Closing the note and pressing
Start created PID 6539 (jail 75). The broker, UI and parent viewer retained
their PIDs. The same pixel returned to red (254, 0, 0), then turned blue
(0, 0, 255) after Next chapter. The replacement had the same binary digest and
limits. The resource check also rejected a deliberately incorrect 512 MiB limit.

Three subsequent fresh-process Starts on the running guest took 3,176, 3,183
and 3,169 ms from issuing the Start press over SSH to observing the decoded red
pixel in a headless PNG (median 3,176 ms). Each run used a new worker PID and
the same fixture. This is an end-to-end observation with warm host and guest
caches, including input injection and PNG readback. The fork-before-taint
comparison is pending. Evidence is in
`~/atrium-vm/evidence/navigator-i5-worker-hardening-20261124/`.

### I5 fork-before-taint gate

The signed offline editor probe runs as an unprivileged user in Capsicum on
Tessera, with networking disabled, a 1 GiB memory limit and a 100% CPU limit.
It forks before admitting any site input. One arm initializes the pinned
typefaces in each child. The other initializes them once in the template.
The child creates its own event loop and JS agent, runs the scripted 128-line
editor, builds a native Fresco scene, types a character and scrolls 24 pixels.
The template checks that each child exited and its private write stayed local.
No child receives an island, fetch or player channel in this gate.

The initial font-warmed template held ten descriptors. The temporary-memory
LibGfx loader copied each built-in face into a shared anonymous buffer. The
[static-typeface patch](../../patches/ladybird/embedded-static-typeface.patch)
lets the pinned provider use the immutable bytes in the engine image directly.
Received and temporary fonts retain their existing ownership rules. Encoding a
static face for LibGfx IPC materializes the existing raw-font wire format.
The corrected template had zero descriptors above stdio and was single-threaded.
Constructing the Unix event loop opened its two wake-pipe descriptors. Both
were still open after JS agent initialization. The gate keeps that state local
to each child.

In three repeat launches with warm guest caches, all 18 children passed.
The following medians cover nine children per arm. Each launch ran the
child-initialized arm first. These timings end when the first scene conversion
returns successfully. The separate 3,176 ms installed baseline includes jail
creation, input injection, composition and headless frame observation.

| Pinned faces initialized in | Fork to child entry | Fork to first native scene |
| --- | ---: | ---: |
| Child | 305 µs | 12.124 ms |
| Template | 287 µs | 11.884 ms |

A plain fork inherits the current jail. Portcullis currently asks jaild to
create and attach a fresh island jail, drop privileges and exec the worker.
The offline gate therefore establishes a usable initialization boundary, while
the per-persona launch path still needs a design for assigning a child to its
own jail and confirming its uid, limits and descriptor set before Limen
delivers any page channels. Profile the installed Start stages before choosing
that design. The offline medians alone do not establish an installed speedup.
(Settled: the Start was profiled and D7 chose a standby pool over a fork
zygote; see "I5 warm start".)

To repeat, apply the static-typeface patch to the pinned engine. Rebuild
LibGfx and the pinned provider, including `Font-Typeface.o` in the staged link.
Configure the editor host with `I5_FORK_CHECK=ON`, build `editor-host`, and
use `scripts/navigator-i0-editor-link.py STAGE --fork-check`. Package the
arm64 binary in a signed offline bundle and run
`scripts/navigator-i5-fork-check.sh INSTALLED_BUNDLE` inside Tessera.
Evidence is in `~/atrium-vm/evidence/navigator-i5-fork-20261124/`.

### I5 Start trace

The 3,176 ms installed Start covers every hop from the press to the first
presented frame, and the fork gate covers only the engine's own part. Before
choosing how a warmed child gets its own jail, the installed path is traced
hop by hop. Each process on the path prints one line on stderr per stage,
and only for an engine launch:

```
start-trace WHO STAGE mono_us=N pid=P TAG
```

`mono_us` is `CLOCK_MONOTONIC` in microseconds, which is one clock for every
process and every jail on the machine, so the lines line up without any
clock exchange. A launch counts as an engine launch when the app is
`org.atrium.navigator.engine` (Limen, portcullisd, the one-shot lane) or the
jail name contains `navigator-engine` (jaild). Other launches print nothing.

| Who | Stage | Where |
| --- | --- | --- |
| navigatord | `widget-start` | `Widget::start`, before connecting to Limen |
| limend | `request-island` | `REQUEST_ISLAND` received |
| limend | `exec-instance` | `ExecInstance` sent to portcullisd |
| portcullisd | `exec-begin` | `handle_exec_instance` entry |
| portcullisd | `ready-for-fds` | policy, limits and route domain passed |
| limend | `ready-for-fds` | portcullisd's answer read |
| limend | `fds-sent` | island channel, fetch socket and stderr sent |
| portcullisd | `fds-received` | descriptors received |
| oneshot | `run-begin` | `run_inner` entry (manifest, signature, root) |
| oneshot | `jaild-call` | instance root built, `run_via_jaild` called |
| jaild | `create-begin` | `handle_create`, before validation |
| jaild | `exec-begin` | jail created, before the exec path |
| jaild | `pdfork-done` | the worker forked (its pid in the tag) |
| limend | `embed-handed` | frescod embed connection sent on the channel |
| navigatord | `island-granted` | `request_island` answered |
| navigatord | `island-fetch` | the fetch channel arrived |
| navigatord | `placed` | the island placed in the window |
| navigatord | `first-request` | the engine's first document GET |
| navigatord | `engine-started` | the engine reported its start for the origin |
| navigatord | `attached` | frescod attached the island's scene |
| oneshot | `jaild-returned` | the worker's jail ended (after the run) |

`scripts/navigator-i5-start-trace.sh [--last] LOG...` merges the logs of
navigatord, limend, portcullisd and jaild, orders the lines by `mono_us`,
prints each stage's time since the first and since the previous stage, and
fails when a hop that must appear is missing. `--last` keeps only the most
recent press. The engine's own work and frescod's first presentation are not
instrumented: the gap from `first-request` to `attached` is the engine
(execve, dynamic link, LibWeb start, layout, first scene), and the gap from
`attached` to the observed red pixel is composition and the headless
capture, which the existing I5 observation measures.

To run it, deploy main's navigatord, limend, portcullisd and jaild to the
arm64 Tessera VM, press Start three times as in the I5 gate, and pass the
daemons' logs to the script once per press. Record the three per-stage
tables beside the I5 evidence.

#### Results on arm64 Tessera, 2026-11-24

The daemons came from main `e4dc55230`, and the engine was the audited I5
worker (digest `036711b4…5a95f7`), on a disposable guest running the I5
local chapter fixture. All 21 stages printed on every press. The medians
below are milliseconds since `widget-start`, over three presses per arm.

| Stage | No audio service (as in the I5 gate) | choragusd answering |
| --- | ---: | ---: |
| limend `request-island` | 0.261 | 0.266 |
| portcullisd `exec-begin` | 1.523 | 1.558 |
| limend `fds-sent` | 1.837 | 1.869 |
| limend `embed-handed` | 2.136 | 2.158 |
| navigatord `placed` | 2.325 | 2.358 |
| oneshot `jaild-call` | 10.910 | 11.320 |
| jaild `create-begin` | 13.682 | 11.938 |
| jaild `exec-begin` | 23.119 | 16.867 |
| jaild `pdfork-done` | 23.456 | 17.156 |
| navigatord `first-request` | 2,025.432 | 62.587 |
| navigatord `attached` | 2,035.679 | 66.401 |
| navigatord `engine-started` | 2,270.950 | 366.227 |
| oneshot `jaild-returned` | 2,690.142 | 817.506 |

The daemon path from the press to a forked worker takes 16 to 29 ms. Of
that, about 2.4 ms covers Limen, portcullisd's policy step and the
descriptor handoff. The one-shot lane's manifest and root preparation
takes 6 to 21 ms, and jaild's jail creation takes 3 to 14 ms.

In the arm without an audio service, the engine's first request arrived
2.0 s after its fork, on every press. The I5 gate guest ran no choragusd.
While it starts the embedded player, limend waits up to 2 s for the absent
audio-attribution socket (`atrium_listen::connect` in `limen/src/audio.rs`).
With lyrad on `snd_dummy` and choragusd answering, the gap fell to 40 to
56 ms. choragusd still refused the ticket, because the Navigator grant has
no `audio`. A listener that accepts and never answers kept the 2.0 s gap,
which acts as the positive control (1 s read timeout, then one retry).
The 3,176 ms installed baseline was therefore measured with this wait
included. With the wait removed, the engine accounts for most of the
remaining Start time: about 45 ms from fork to its first request, and
about 300 ms from that request to its start report.

In every press the island then failed with "The live widget could not load
its page". Main's navigatord grant for the local video expects a fourth
fetch (`tone-opus.webm`, since `88704300b`), but the I5 fixture and worker
make three. No red frame was observed, so this run does not repeat the
press-to-pixel time. The press-to-failure note took about 2.9 s without the
audio service and about 0.95 s with it. Before the next Start measurement,
pair main's navigatord with a worker and fixture that make the Opus fetch.
Evidence is in `~/atrium-vm/evidence/navigator-i5-start-trace-20261124/`.

#### Follow-up: the audio wait and the Opus fetch

limend asked choragusd for the player's audio ticket inside `request`,
under its state lock, before replying to navigatord. navigatord's
`Video::start` waits for that reply before it starts serving the engine's
fetches, so an absent or stalled choragusd delayed the engine's first
request and, while it lasted, every other Limen request. limend now asks on
a thread of its own after the embed exists, and cancels a ticket whose
embed ended meanwhile; cancels at embed end also leave the state lock. The
player opens its sound again when a late ticket arrives
([lyra-v1.md §4](lyra-v1.md)). `limen/tests/daemon.rs` holds a 1.5 s
choragusd and checks that the embed and a second request still answer
within 1 s; on the old code it failed at 1.52 s.

Main's sources already agree on the four fetches: navigatord's grant,
`navigator-ladybird/fixture_fetch.rs` and `local-player.js` all name
`tone-opus.webm`. The audited I5 worker binary predated that, so the rerun
needs an engine rebuilt from main (with `fixture-fetch.patch` and
`native-mse.patch` as on main) and audited again with
`scripts/navigator-i5-worker-audit.sh`. A navigatord unit test now fails
when the grant, the engine's sequence and the page's fetches drift.

Rerun: rebuild and audit the engine from main, deploy main's navigatord,
limend, portcullisd and jaild, then run three Starts without choragusd and
three with it. Both arms should now reach `first-request` in tens of
milliseconds; record press to red pixel as in the I5 gate.

### I5 warm start: how a warmed engine gets its own jail (D7, accepted)

What the measurements say. On arm64 Tessera the daemons take 16 to 29 ms
from the press to a forked worker, and with the audio wait gone the engine
takes about 45 ms from fork to its first request and about 300 ms more to
report its start. The offline fork gate reaches a first native scene 12 ms
after fork from a font-warmed template. So nearly all of the remaining Start
is the engine's own exec and initialization, and the only way to remove it
from the press is to do that work before the press. The question is where
the warmed process lives, given that every island must run in its own jail,
as its persona's uid, under its own rctl limits and in Capsicum, before
Limen gives it any page channel.

Options:

1. **Fork zygote inside a per-persona parent jail.** One warmed template per
   persona, kept as root inside a parent jail with `children.max`. jaild
   creates a child jail per island under it; the forked child attaches to
   it, drops to the island uid, enters Capsicum and waits for channels.
   Children share the template's memory copy-on-write. Costs: a long-lived
   root process inside the jail tree running Ladybird's initialization code;
   whether jailed root may `jail_attach(2)` to its own child jails must be
   asked of the kernel, not assumed; every island shares the template's
   address-space layout (the known zygote weakness: one leak defeats ASLR for
   all of a persona's islands); and a second jail creator besides jaild,
   which breaks the "jaild is the only caller of `jail_set(2)`" rule unless
   jaild does the child creation and the template only attaches.
2. **Fork zygote on the host, root, attaching children to jaild's jails.**
   Simpler kernel story (host root may attach anywhere), but the warmed
   template is an unjailed root process running engine code. Rejected.
3. **Warm pool of ordinary one-shot workers (recommended).** limend keeps
   one engine already started per persona that has a live parent, through
   the same `ExecInstance` path as today: jaild jail, persona uid, rctl,
   Capsicum, audited binary. The worker initializes (fonts, event loop, JS
   agent) and then blocks on a standby channel to limend. On Start, limend
   passes the island channel, the fetch socket and the frescod embed
   connection over that channel (SCM_RIGHTS) and starts the next standby in
   the background. Each worker is used once and exits with its island.
   Nothing new holds privilege, jaild stays the only jail creator, every
   island has its own ASLR layout from its own exec, and the existing I5
   checks (`navigator-i5-worker-check.sh`, the symbol audit) apply as they
   are. Costs: the standby's memory is not shared (one idle engine per
   active persona, to be measured), a burst of Starts beyond the pool pays
   today's cold path, and the worker's launch protocol gains a standby phase
   in which it must accept only one `Assign` from limend.

Recommendation: option 3, one standby per persona with a live Navigator,
none when no Navigator runs. Gates: press-to-`first-request` under 20 ms with
a standby, the cold path unchanged without one, the I5 worker checks on an
assigned standby, a standby that receives anything but `Assign` exits, and
idle memory recorded per standby. Revisit the fork zygote only if standby
memory proves too high.

Shankar chose option 3 on 2026-11-24.

#### As built

- **limend** keeps at most one standby per Navigator uid (that is, per
  persona instance). After each island it asks for one, it starts the next
  standby on a thread of its own through `Launcher::launch_island_standby`:
  the same `ExecInstance` as a cold island (`serves` = the Navigator's uid,
  so the persona's island uid, jail and limits), with `/dev/null` where the
  fetch socket goes and no frescod connection yet. On the next
  `REQUEST_ISLAND` it takes the standby, sends the fetch socket first on the
  island channel, then the frescod embed connection and `Attach` as for a
  cold island; the island keeps the standby's embed id, so ending it ends
  the standby's jail. A standby that is older than `STANDBY_MAX_AGE`
  (10 minutes), or whose engine has gone (the send fails), is ended and the
  island starts cold. An unused standby is ended when it ages out. The first
  island of each Navigator is still cold.
- **The engine** (`navigator-ladybird/editor-check.cpp`) takes a `/dev/null`
  stdout as the sign of a standby. It enters Capsicum, installs the pinned
  faces, builds the event loop, the JS agent and the page client, then
  blocks in `navigator_standby_fetch` (`limen::client::Child::standby_fetch`)
  for exactly one descriptor. Anything else, or limend hanging up, ends it.
  It then continues as a cold island would.
- **Off by default.** limend starts standbys only with
  `LIMEN_ISLAND_STANDBY=1`, because an engine built before this change
  exits at once when it has no fetch socket. Turn it on with an engine
  rebuilt from main.
- **Tests.** `limen/tests/daemon.rs`: the second island takes the standby
  (same embed id, no new launch, fetch socket then frescod then `Attach`,
  the fetch socket reaches navigatord), a standby gets nothing before a
  press, the next standby follows, and a dead standby falls back to a cold
  start and is ended. The engine side is not built in the cloud.
- **Trace.** `limend standby-assigned` marks a warm Start, and
  `scripts/navigator-i5-start-trace.sh` then expects it in place of the
  portcullisd, one-shot and jaild stages.

Gate on the Mac VM, with the rebuilt engine and `LIMEN_ISLAND_STANDBY=1`:
three warm Starts (the second and later presses), press to `first-request`
and to the red pixel against the cold first press; `navigator-i5-worker-check.sh`
on an assigned standby; the standby's resident memory while it waits; and
a standby whose channel receives a data byte without a descriptor exits.

#### Results on arm64 Tessera, 2026-11-24 (rerun from main)

The engine was rebuilt from main `58f533c1b` with `fixture-fetch.patch` and
`native-mse.patch` (not `webauthn-submit.patch`): the seven patched media
and loader units, `editor-check.cpp` and `EditorPage.cpp` with
`ATRIUM_I4_VIDEO`, and the painter staticlib were rebuilt and relinked with
the rest of the audited I5 link inputs. `navigator-i5-worker-audit.sh`
passed for digest `6b2b35bb…5092eb`, and the installed engine had that
digest. navigatord, limend, portcullisd, jaild, the embedded player
supervisor, lyrad, choragusd and navigator-fetchd came from main. The
older navigator-fetchd refused the fourth fetch (`BadUrl` on
`tone-opus.webm`), so the fetcher must come from main too. The guest was
a disposable overlay of the newest surviving Navigator guest disk, since
the I5 run's own disk was gone. All three arms used one guest session.

Press to the decoded red pixel is measured as in the I5 gate (headless
PNG over SSH). The stage times are milliseconds since `widget-start`.

| | No choragusd | choragusd answering | Warm (standby assigned) |
| --- | ---: | ---: | ---: |
| Presses | 3 | 3 | 4 (presses 2 to 5) |
| `pdfork-done` | 13.0 / 30.7 / 19.3 | 10.2 / 20.6 / 43.4 | none (`standby-assigned` at 0.4 to 0.6) |
| `first-request`, median | 113.5 (67 to 156) | 69.0 (60 to 170) | 55.0 (41 to 172) |
| `engine-started`, median | 298 | 333 | 318 |
| Press to red pixel | 1,588 / 1,697 / 1,748 | 1,696 / 1,764 / 1,777 | 1,770 / 1,727 / 1,741 / 1,700 |
| Median to red | 1,697 | 1,764 | 1,734 |

The 2 s audio wait is gone: both cold arms reach `first-request` within
tens of milliseconds, and the press to red fell from 3,176 ms in the I5
gate to about 1.7 s, with the fourth (Opus) fetch now made and served.
choragusd still refused the ticket because the Navigator grant has no
`audio`.

The standby worked as built. The first press with `LIMEN_ISLAND_STANDBY=1`
was cold (1,723 ms to red), and each later press took the waiting standby
(same embed id, no portcullisd or jaild stages for the island) while the
next standby forked 42 to 63 ms after the press. `navigator-i5-worker-check.sh`
passed on assigned standby PID 11980 (jail 72): uid 50007, Capsicum, IP
disabled, `memoryuse:sigkill=1073741824` and `pcpu:deny=100`, and the
mapping checks. A waiting standby held 70,396 to 70,408 KB resident
(four samples, 7 to 8 s after its fork).

The warm path misses its two targets. `first-request` came 41 to 172 ms
after the press, not under 20 ms, and the red pixel did not move. The
trace does not cover the gap: navigatord starts the fetch thread only after
`Video::start` has asked Limen for the embedded player, and the next
standby's launch runs at the same moment. About 1.4 s of the 1.7 s lies
after `engine-started` (the four fetches, MSE appends, the native player's
start and first decoded frame, and the headless capture), which no stage
instruments. In one cold press with standbys on, the standby's jail was
created before the island's, delaying the island's fork to 141 ms. The
standby that receives a data byte without a descriptor was not tested on
the guest. Evidence is in
`~/atrium-vm/evidence/navigator-i5-rerun-20261124/`.

#### Follow-up: tracing past the engine's start, and two overlaps

The trace now continues to the first decoded frame. New stages, all
printed once per island:

| Who | Stage | Where |
| --- | --- | --- |
| navigatord | `player-request`, `player-granted` | the embedded player asked of Limen, and granted |
| navigatord | `request`, `fetched`, `served` | each of the first eight fetches: arrived, answered upstream (tag `step=N bytes=N url`), delivered to the engine |
| navigatord | `video-open`, `add-vp9`, `add-opus` | the engine's media element and source buffers |
| navigatord | `append-vp9`, `appended-vp9`, `append-opus`, `appended-opus` | the first append of each track, and its acknowledgement |
| navigatord | `player-canplay`, `player-ready`, `set-playing`, `playing` | the player's answers, Play, and the first advancing time |
| player | `main`, `ready` | the embedded player's process start and its embed token, for a Navigator embed only |
| player | `first-frame` | the first decoded frame |

Two changes take work off the press's path:

- **The player launches beside the engine.** `Video::start` used to ask
  Limen for the player and wait for it before handing the engine its video
  socket, and the engine could not make its first request until it had that
  socket. The socket is now handed over at once and the player is asked
  for on the session's own thread; the engine's media requests wait on the
  socket until the player is there, and a placement that arrives meanwhile
  is kept for it.
- **The next standby waits.** limend launches it `STANDBY_REFILL_DELAY`
  (3 s) after the island, so its jail, fork and warm-up do not compete with
  the island's start, its player and its first frame.

Next on the Mac VM: rebuild the player and navigatord from main, repeat
the three arms with the player's and navigatord's stderr in the trace
inputs, and read where the 1.4 s after `engine-started` goes before tuning
anything else.

#### Results on arm64 Tessera, 2026-11-24 (trace to the first frame)

Main `fe34b869c`: navigatord, limend and the embedded player rebuilt, the
engine unchanged (`6b2b35bb…5092eb`, the installed digest), on a disposable
guest. limend's log carries the player's stderr, so the four daemon logs
give every stage. Medians in milliseconds since `widget-start`, three cold
presses per arm and four warm (presses 2 to 5):

| Stage | No choragusd | choragusd | Warm |
| --- | ---: | ---: | ---: |
| `first-request` | 70 | 70 | 100 (42 to 155) |
| player `ready` | 264 | 247 | 164 |
| `served` step 0 (document) | 234 | 233 | 232 |
| `engine-started` | 354 | 354 | 336 |
| `served` step 1 (script) | 391 | 358 | 367 |
| `video-open` | 665 | 602 | 607 |
| `served` step 2 (VP9 clip) | 761 | 695 | 713 |
| `served` step 3 (Opus clip) | 1,027 | 963 | 987 |
| `add-vp9` | 1,222 | 1,158 | 1,181 |
| `appended-vp9` | 1,331 | 1,270 | 1,301 |
| player `first-frame` | 1,398 | 1,330 | 1,363 |
| `player-ready` | 1,462 | 1,375 | 1,425 |
| Press to red pixel | 1,744 | 1,724 | 1,740 |

Where the time goes, in the median cold press:

- **The engine's own work between fetches, about 700 ms.** From the
  script served to `video-open` takes 210 to 275 ms, from the VP9 clip
  served to the Opus request about 220 ms, and from the Opus clip served
  to `add-vp9` about 195 ms. In those spans the JIT-off engine runs the
  page script, which splits each clip by scanning its bytes with
  `findIndex`, and sets up MediaSource. Which of these dominates is not yet
  traced inside the engine.
- **The first fetch, about 160 ms** from `first-request` to served; the
  later three take 40 to 55 ms each, so about 300 ms of fetching in all.
- **Appends and the first frame, about 240 ms** from `add-vp9` to
  `player-ready`, of which the decode of the first frame is about 65 ms
  after the VP9 append is acknowledged.
- **Composition and capture, about 280 ms** from `player-ready` to the
  observed red pixel, which includes the headless PNG and the SSH poll.

The player now launches beside the engine and is ready at 160 to 260 ms,
long before the first append, so it is off the press's path. The next
standby forks 3.0 s after the island and no longer overlaps the Start.
The standby still saves nothing visible: the island's first request comes
40 to 155 ms after `standby-assigned` and the rest of the path is the
same engine and media work, so warm and cold reach red at about 1.74 s.

A standby fed one data byte without a descriptor ended 101 ms later with
"a standby expects exactly its fetch socket" (exit 1), and one whose
channel was closed ended with "peer closed before sending fds". As the
control, a standby sent one descriptor went on as an island and exited
8 s later when its stand-in fetch socket never answered. The probe
(`standby-probe.c`) ran the installed engine as uid 50007 in an engine
jail, on a socketpair with `/dev/null` as stdout, after a 3 s warm-up.
`navigator-i5-worker-check.sh` passed again on an assigned standby (PID
9109, jail 77), and a waiting standby held 69,720 to 69,728 KB. One press
on the jail that had hosted the probes took 3,745 ms, with the first fetch
alone taking 1 s; it is left out of the medians. Evidence is in
`~/atrium-vm/evidence/navigator-i5-trace-frame-20261124/`.

#### Follow-up: the 20 ms input poll, not the clip scan

The clip split is not the cost: the first Cluster starts at byte 468 of
`red-blue.webm` and byte 497 of `tone-opus.webm`, so `findIndex` calls its
callback fewer than 500 times per clip. The gaps between the page's steps
are each about 200 ms, which matches about ten turns of `drive_page`
(`SignInFixture.h`). Each turn pumped the Core event loop once, without
waiting, and then `navigator_page_pointer` waited up to 20 ms on frescod
for input. A resolved fetch, a promise reaction or a media event that
queued the next task therefore waited out a 20 ms poll at every hop.
`drive_page` now pumps until nothing is ready, up to 256 times, before it
polls for input.

This is a hypothesis until it is measured: the engine is not built in the
cloud. The change assumes `Core::EventLoop::pump` returns the number of
events it handled. Next on the Mac: rebuild the engine with this change,
re-audit it, rerun the three arms, and compare `served` step 1 to
`video-open`, step 2 served to step 3's `request`, and step 3 served to
`add-vp9` against the table above. If the gaps stay, add `start-trace`
stages inside the engine (painter `navigator_video_ready`, the fixture
loader's completion) before changing anything else.

#### Results on arm64 Tessera, 2026-11-24 (drained event loop)

The hypothesis holds. The engine was rebuilt from main `d4a26635e`
(`Core::EventLoop::pump` returns `size_t`, so the change compiled as
written) and audited, and the installed digest matched (`1000beb6…a7a0`).
The daemons, player and guest setup were as in the trace run. Medians in
milliseconds since `widget-start`, before (`97d4a9bdb`) and after:

| Stage | No choragusd | choragusd | Warm |
| --- | ---: | ---: | ---: |
| `served` step 1 | 391 → 473 | 358 → 430 | 367 → 362 |
| `video-open` | 665 → 478 | 602 → 434 | 607 → 388 |
| `request` step 3 | 980 → 547 | 913 → 528 | 928 → 467 |
| `add-vp9` | 1,222 → 620 | 1,158 → 592 | 1,181 → 545 |
| player `first-frame` | 1,398 → 683 | 1,330 → 678 | 1,363 → 629 |
| `player-ready` | 1,462 → 689 | 1,375 → 691 | 1,425 → 632 |
| Press to red pixel | 1,744 → 1,027 | 1,724 → 1,062 | 1,740 → 962 |

The three gaps the follow-up named are gone. Step 1 served to
`video-open` fell from 210 to 275 ms to 4 to 26 ms, step 2 served to
step 3's `request` from about 220 ms to 2 ms, and step 3 served to
`add-vp9` from about 195 ms to 1 to 3 ms. Press to red is now 1,444, 892
and 1,027 ms without choragusd, 1,067, 1,062 and 1,006 ms with it, and
938, 961, 963 and 1,001 ms warm (presses 2 to 5). The first cold press
was slower throughout (fork at 49 ms, first request at 327 ms).

What remains in the median warm press: about 55 ms to the first request,
the four fetches at 60 to 180 ms for the first and 60 to 80 ms for each
of the others (each the fetch worker's round trip upstream), about 90 ms
of appends and first frame, and about 330 ms from `player-ready` to the
observed red pixel (composition, the headless PNG and the SSH poll). The
standby now saves about 65 ms against the cold arms. Cold first-request
times varied from 70 to 327 ms with the jail fork (15 to 116 ms). Evidence
is in `~/atrium-vm/evidence/navigator-i5-drain-20261124/`.

#### Follow-up: splitting the last 330 ms

Headless frescod (`FRESCOD_START_TRACE=1`) now prints `frescod
frame-begin` (with the generation and how long it slept to keep the
capture floor), `composited` and `png-saved` for each frame. Two things in
the headless loop alone can account for much of the 330 ms: captures are
at least 200 ms apart (`HEADLESS_MIN_INTERVAL`) once no video is playing,
which the paused poster is not, and each capture is a full-screen PNG
encode before the SSH poll can see it. If the trace shows the time is
there, it is the measurement and not the Start; the product figure is then
`player-ready` to `composited`, and the end of the gate should be
`composited` rather than the pixel poll. Next on the Mac: run frescod with
`FRESCOD_START_TRACE=1`, repeat the three arms, and pass frescod's stderr
to `navigator-i5-start-trace.sh` with the other logs.

#### Results on arm64 Tessera, 2026-11-24 (frescod traced)

frescod from main `f0682dae8` with `FRESCOD_START_TRACE=1`; the engine
(`1000beb6…`), daemons and player were byte-identical to the drained-loop
run, so frescod was the only change. Times in milliseconds since
`widget-start`; "frame" is the first frescod frame after the player's
`first-frame`.

| Press | `player-ready` | frame `frame-begin` (slept) | `composited` | `png-saved` | Red observed |
| --- | ---: | ---: | ---: | ---: | ---: |
| No choragusd 2 | 623 | 794 (199) | 846 | 849 | 942 |
| No choragusd 3 | 692 | 838 (199) | 892 | 896 | 1,062 |
| choragusd 1 | 642 | 782 (199) | 835 | 838 | 959 |
| choragusd 2 | 694 | 837 (199) | 889 | 892 | 992 |
| choragusd 3 | 705 | 819 (199) | 872 | 875 | 1,015 |
| Warm 2 | 680 | 797 (199) | 850 | 854 | 994 |
| Warm 3 | 714 | 801 (199) | 855 | 858 | 988 |
| Warm 4 | 695 | 824 (199) | 875 | 878 | 977 |

The 330 ms is the measurement, as the follow-up expected. After
`player-ready`, the next frame waits 89 to 175 ms for the 200 ms headless
capture floor (every such frame slept 199 ms), composition takes 52 to
55 ms, the PNG 3 ms, and the SSH poll finds the red pixel 94 to 170 ms
after the PNG is written. The gate therefore now ends at frescod's
`composited` for the first frame after the player's `first-frame`, not at
the pixel poll. On that end the median press to `composited` is 869 ms
without choragusd (two presses), 872 ms with it and 855 ms warm. Without
the headless floor, which a real display does not have, composition would
follow the first decoded frame by about 55 ms, about 730 to 770 ms after
the press.

Two of the eleven presses took about 1.6 s to red, both before the engine
work: the first press after boot made its first request at 452 ms, and
warm press 5's first fetch took 765 ms upstream. They are left out above.

The same guest then took main `afdae0373`'s portcullisd (with the launch
registry check from `2bd8429`), frescod and navigatord. Three cold Starts
reached red at 1,050, 881 and 893 ms, and portcullisd logged no registry
error with `/var/run/atrium/app-registry` present. Evidence for both is in
`~/atrium-vm/evidence/navigator-i5-frescod-trace-20261124/`.

### I6 plan: whole pages live on a press (accepted)

Shankar asked for this plan on 2026-11-24 while the I5 timing waited on the
Mac, and accepted it with D8 to D13 as recommended the same day. D5 settled the shape: everyday trails stay in the document lane with
islands, and a whole page goes live only when the person asks, for the few
fat clients that need it (legacy-web §11.8.3: Figma, WhatsApp Web, Notion's
editor, Khan Academy's exercises, map-heavy pages, banking, checkout).

**What is already built.** Sign-in windows (I3) run whole pages live: the
JIT-off Ladybird engine in its own jail, a page fetched through Arca's
couriers in the window's own jar, forms the page's script submits turned
into a trusted Continue, WebAuthn through Sigillum, and module scripts. I5
gives a hardened, audited worker with a standby per persona and a Start of
about one second. I6 is mostly the sign-in window's machinery offered in an
ordinary trail, with the sign-in window's strictness loosened where an
everyday app needs it and kept where it protects the person.

#### What the person sees

- In the document lane the tier chip's popover offers **Run this page live**
  when the converter saw scripts that did not replay (tier 1 or 2 with
  losses). Nothing goes live by itself.
- After the press the chip reads **Live code** (navigator-ui §6.5, already
  drawn) and frescod frames the whole content area in amber with the
  origin's tag. The Where field stays navigatord's: the engine never draws
  the address, the chip or the frame.
- **Stop** in the popover returns the trail to its document-lane rendering
  and ends the engine. Leaving the origin ends it too.
- Whether the choice is remembered per origin is D8.

#### Where it runs

- One engine jail per (persona, origin), from the same
  `org.atrium.navigator.engine` bundle, as the persona's island uid, with
  the I5 limits, Capsicum after start-up and JIT off. A standby (D7) takes
  it when one is waiting.
- The engine gets a whole-window `Island` slot in the trail instead of a
  box. Input, clipping and the obscured rule are the island's.
- The engine is told its origin and fetches only through its own Nuntius
  (§3). What it may reach beyond its origin is D12.
- Session recycling (§11.9.1) applies: a memory or idle limit ends the
  engine and the trail falls back to the document lane until the next
  press (or the remembered choice) restarts it from the same address.

#### Navigation

The engine never navigates the trail on its own authority.

- A same-origin navigation (link, form, `location`, history API) stays in
  the engine and is reported on the typed channel, so navigatord adds a
  trail entry and the Where field follows. Back and Forward drive the
  engine's history for entries it made.
- A navigation to another origin is proposed on the channel and refused in
  the engine. navigatord opens it in the document lane, in the same trail
  (a link) or a new one (`window.open`, `target=_blank`). It never starts a
  second live engine for the other origin without its own press.
- Downloads go to Scrinium's save sheet, as in the document lane. Printing
  uses the existing Navigator print path (the page child writes the PDF in
  its jail).

#### What the page can reach

| need | I6 answer | later |
| --- | --- | --- |
| Cookies and sign-in | D9 | |
| `localStorage`, IndexedDB, Cache API | D10 | |
| Cross-origin `<iframe>` | D11 | |
| Network beyond the origin | D12 | |
| File inputs | Scrinium's picker through Limen, one read-only file per press | drag and drop |
| Clipboard | Tabula: copy any time, paste only on the person's Ctrl-V in that window | |
| Notifications, geolocation, camera, microphone, MIDI, USB, Bluetooth | refused, and the refusal shown in the chip's popover | per-origin grants through Curia |
| WebGL, WebGPU, `OffscreenCanvas` on a GPU | not available (no GPU descriptor, §2) | D13 |
| WebAssembly | the interpreter only (JIT off) | |
| Service workers, background sync, push | refused: nothing runs after the trail stops showing the page | |

#### Script-made requests

A sign-in window turns a form the script submits into a trusted Continue
because credentials are at stake. An everyday app sends requests all the
time, so I6 does not confirm them: the person chose to run the page live,
and the chip's words already say the page can send what is typed to its
own site. The protections that stay are the network scope (D12), Nuntius's
address rules (no private or loopback addresses), the jar's partition
(D9), and the obscured-input rule. A live page's script can read whatever
is typed or filled into its own fields, which is what the chip says, so
Arca fills a saved password only on the person's press and only for the
origin that saved it.

#### Build steps

1. **I6.0 Qualify.** Without accounts: Khan Academy's public exercises,
   an OpenStreetMap or MapLibre page, a public Notion page, and local
   fixtures for a fat client (a single-page app with IndexedDB, a login
   against the local Keycloak, a cross-origin frame, a file input and a
   download). Record what renders, what each refused request was, Start and
   memory. Figma is expected to fail without WebGL and is recorded, not
   chased.
2. **I6.1 Whole-window island and the press.** The chip's Run this page
   live, the whole-window slot, Stop, fallback to the document lane, the
   amber frame and tag. Gate: the fixture app runs live on a press, never
   without one, and Stop returns the document-lane rendering.
3. **I6.2 Navigation contract.** Same-origin navigations as trail entries,
   cross-origin proposals opened in the document lane, `window.open`,
   Back and Forward. Gate: a fixture with each kind of navigation, and an
   engine that tries to navigate cross-origin without proposing.
4. **I6.3 Jar and storage** (D9, D10). Gate: sign in to the fixture app,
   Stop, run live again and stay signed in; another persona and another
   origin see neither the cookie nor the stored data; removing the persona
   destroys both (Tessera E5).
5. **I6.4 Brokers.** File input through Scrinium, downloads, clipboard,
   printing, and the refusals listed above shown in the popover. Gate: each
   broker on the fixture, and each refused API reported, not silently
   dropped.
6. **I6.5 Frames and network scope** (D11, D12). Gate: a fixture page with a
   same-site API, a third-party script and a cross-origin payment frame.
7. **I6.6 Hardening.** The remembered choice (D8) in Curia with revoke,
   recycling under memory pressure, the worker audit and check on a
   whole-page engine, and the fuzzed channel's new navigation messages.

#### I6.0 qualification run

Until I6.1 builds the press, the vehicle is the VPN sign-in window, the one
built path that runs a whole page live: the installed JIT-off engine in its
jail, the page and its same-origin scripts and stylesheets through Arca's
couriers, everything else refused. Start it the way the Crypta Sign In
runners do (portcullisd's `vpn_sign_in_window`, or
`NAVIGATORD_VPN_SIGN_IN="<profile> <start> <final>"` with
`--unconfined-for-testing`), with the page under test as the start and a
final address the run never reaches. navigatord logs each refused resource
with its host and registrable site (`... refused: only the page's own
origin (host cdn.example.net, site example.net)`), which is the D12
measurement: how many refusals were sibling hosts of the page's site, and
how many were third parties.

The fixture sites are `test-assets/navigator-i6/`, served by
`scripts/navigator-i6-fixture.py`:

    python3 scripts/navigator-i6-fixture.py cert /tmp/i6-cert   # prints ca.pem
    python3 scripts/navigator-i6-fixture.py serve /tmp/i6-cert --port 443

It answers `app.example`, `api.app.example`, `pay.example`,
`cdn.thirdparty.example` and `elsewhere.example` by Host header. Point
those names at it the way the Crypta stand's `idp.crypta.test` is, and
trust its `ca.pem` for the couriers. The page reports each check as a line
in its own `#checks` list and as a JSON line in the server's log (`"check":
...`); every request the server saw is a line too, so a request the window
refused is one that never appears. Sign in on the page's form, add an item,
choose a file, press each link, and record the lines.

Run, each three times, cold:

1. `https://app.example/app.html` (the fixture).
2. A Khan Academy public exercise
   (`https://www.khanacademy.org/math/arithmetic/.../e/...`, any one that
   opens without an account).
3. An OpenStreetMap page (`https://www.openstreetmap.org/#map=12/...`) and a
   MapLibre demo page.
4. A public Notion page (a published `notion.site` page).

Record for each: what the window drew (a capture), the refused hosts from
navigatord's log grouped by same site or third party, Start (press to
first composited frame, from the I5 trace lines) and the engine's peak
resident memory (`ps -o rss` on the engine jail's process, every second).
Figma is opened once and recorded, not chased (D13).

What the run is expected to show, so a surprise is visible:

- The fixture's `module`, `local-storage`, `push-state` and `wasm` lines
  pass; `third-party` reads refused; `webgl` reads none.
- Script-made requests (`session`, `script-post`, `same-site-api`) fail,
  because the window fetches only scripts and stylesheets for a page: that
  is what I6.3 and I6.5 build, and the run records the shape of what fails.
- The `frame` line does not appear: no cross-origin frame runs before I6.5.
- Public apps that load their code from a CDN host on another site render
  little; the refused hosts list says which.

#### Results on arm64 Tessera, 2026-11-25 (I6.0)

The vehicle was the VPN sign-in window, asked of portcullisd as the Crypta
Sign In runner does, on a disposable overlay of
`~/atrium-vm/navigator-signin-base` with QEMU user networking unrestricted
for this run only. navigatord came from main `659476f56`; the JIT-off
`mail-page` engine and the other binaries were the VPN passkey run's set
(`mail-page` `0f917aff…`). A stand-in for cryptad's socket reported one
live attempt for profile `win`, so the window stayed open; the fixture
names were forwarded to `navigator-i6-fixture.py` on the host. Each page
was opened cold three times (the window's jail ended between runs). The
start is the press of Sign In (the portcullisd request) to the first
headless frame that differs from the empty desktop; "drawn" is the first
frame with page content. Peak RSS is the engine's (`mail-page`) largest
`ps` sample, once a second.

| Page | Window | Drawn | Engine peak RSS | What it drew |
| --- | ---: | ---: | ---: | --- |
| Fixture app | 576–617 ms | 1,023–1,046 ms | 88 to 91 MiB | the app and its checks |
| Khan Academy exercise | 546–556 ms | 11.6–12.0 s | 96 MiB | Fastly's "cookies are disabled" challenge |
| OpenStreetMap | 536–558 ms | nothing in 60 s | 130 MiB | an empty page; still loading at 60 s |
| MapLibre demo | 547–576 ms | nothing in 60 s | 106 to 112 MiB | an empty page; still loading at 60 s |
| Notion public page | 543–578 ms | blank after 5 to 9 s | 119 to 185 MiB | an empty page |
| Figma (once) | 542 ms | none | none | "figma.com could not be opened: the site answered 403" |

The fixture's checks: `module` pass, `third-party` refused,
`indexeddb` pass, `notifications` pass (default); `session` failed
("Resource unavailable from the VPN sign-in window"), `same-site-api`
failed (CORS tainting), and `local-storage` failed ("Unable to store more
than 5242880 bytes"). `push-state`, `wasm`, `webgl` and `frame` printed no
line within the run. Pressing Sign In raised the window's Continue bar;
pressing Continue sent nothing to the server, and Add, the file chooser and
the four links changed nothing and sent no request. One deviation from the
expected shape: `/api/me` reached the server on every load even though the
page was refused it, so a refused request can still appear in the
server's log.

Refused resources, from the engine's `VPN page: no <url>` lines:

| Page | Refused per run | Same origin | Same site, other host | Third party |
| --- | ---: | ---: | ---: | ---: |
| Fixture app | 3 | 1 | 1 (`api.app.example`) | 1 (`cdn.thirdparty.example`) |
| Khan Academy | 17 to 19 | all | 0 | 0 |
| OpenStreetMap | 59 to 105 | all | 0 | 0 |
| MapLibre demo | 99 to 124 | 74 to 93 | 0 | 25 to 31 (`fonts.googleapis.com`) |
| Notion | 1 to 2 | all | 0 | 0 |

On these public pages almost every refusal was the page's own origin,
for being neither a script nor a stylesheet (images, fonts, JSON and
fetches), not for its host: the window's script-and-stylesheet rule, not
D12's same-site question, is what stops them. The only cross-site
refusals were Google Fonts stylesheets on the MapLibre page. navigatord
logs a host and site only for the origin refusal, so the engine's lines
are the complete list. Khan Academy's edge served a cookie challenge
because the window gives the page no `document.cookie`
(`arcad: Operation not permitted`).

Two setup notes for the next run: the Crypta stand's atrium-net
(`--no-unbound`) empties `/etc/resolv.conf`, so couriers cannot resolve
real hosts until it is put back; and a stand-in on cryptad's socket must
read a whole line, because portcullisd writes the request in two writes.
Evidence is in `~/atrium-vm/evidence/navigator-i6-0-20261125/`.

**What I6.0 says for the build.**

- **Why `/api/me` reached the server.** The window's loader asked
  navigatord for every same-origin GET, including the page's `fetch()`.
  navigatord fetched it through the courier, with the jar's cookie, and
  only then refused it for not being a script. So a page could make
  credentialed same-origin GETs that the log called refused. The loader
  now asks the window only for a script or stylesheet load (the request's
  destination). Any other load fails in the engine and is never sent
  (`vpn-resources.patch`). The engine must be rebuilt for the next run.
- **Why no `wasm`, `webgl` or `push-state` line.** The fixture's checks
  ran in sequence, and the `geolocation` check waited on a callback the
  engine never called, so every check after it was held. Each check now
  reports `timeout` after 5 s. `push-state` needs its link pressed, and
  links did nothing in this window. `frame` was expected to be missing.
- **D12 holds.** On real pages, sibling hosts on the page's site were
  almost never asked for, and third parties were rare (Google Fonts on
  MapLibre). The blocker was the window's script-and-stylesheet rule,
  so I6.1 needs every resource kind (images, fonts, JSON, `fetch`) from
  the page's site, through its Nuntius.
- **The jar (D9) is needed before public apps.** Khan Academy's edge
  stops at a cookie challenge because the page has no `document.cookie`.
  The I6.3 jar gives cookies to the courier, not to the engine. Pages that
  read `document.cookie` need a view of the jar that leaves out HttpOnly
  cookies, which is the browser's normal rule. That is I6.3's design
  question.
- **Storage (D10).** `localStorage` refused even a few bytes against a
  5 MiB cap, so the engine has no storage backend in the jail. I6.3
  provides one per origin.
- **Cost.** Start was about 1.0 s to the first drawn frame, and the
  engine held 88 to 185 MiB. Script-heavy maps and Notion drew nothing in
  60 s with JIT off and most resources refused. They are re-measured in
  I6.1 with every resource kind allowed, before deciding whether JIT off
  is acceptable for them.

#### I6.1 as built (2026-11-25)

Host-tested in the cloud; the engine side is written but not yet built
(Ladybird builds on the Mac). Everything is off unless
`NAVIGATORD_LIVE_PAGES=1`, which navigatord also passes to the UI as
`NAVIGATOR_UI_LIVE_PAGES=1`.

- **The press.** The tier popover ends with a **Run this page live** row
  on an HTTPS page from navigatord (`shell::popover_action`). The row
  only asks: the prompt row says "Run this page live? Its scripts run on
  this computer, in a jail, and can send what you type here to
  <site>'s sites.", and its **Run live** is the counted press (500 ms
  still and uncovered, frescod's token). "Not now" is remembered until
  the person asks again from the popover.
- **Starting.** The UI sends `RunPageLive { url, rect, press }` with the
  page area. navigatord (`live_page::Live::start`) checks the address is
  the page it showed, spends the press, ends any widget, and asks Limen
  for a `web-island` in the UI's window (a standby takes it when one
  waits), placed over the page area.
- **The engine never names its page.** Its first request is for
  `limen::web_island::LIVE_PAGE` (`https://live-page.atrium.invalid/`);
  navigatord answers with the page, whose address is the answer's final
  URL, and ends an engine that asks for anything else first or gets
  something other than an HTML 200.
- **Resources.** Every later request is a whole GET checked by
  `live_page::scope` (D12): plain HTTPS in its normal spelling, the
  page's port, no credentials or fragment, and the same registrable site
  as the page (sibling hosts in, public suffixes respected). In scope it
  goes through fetchd's new `L` verb (one GET, no redirect, any status and
  type, at most 8 MiB, the content type passed back as one token); out of
  scope it is refused with the host and site logged and never reaches the
  network. POSTs, ranges and conditional requests are refused in I6.1.
  A page is ended after 4,096 requests or 512 MiB.
- **Live code and Stop.** When the engine reports `Started` with the
  page's origin, navigatord sends `LiveStarted`; the page's facts read
  tier Live (the chip says "Live code") until it ends. The popover's row
  becomes **Stop running live**. Stop (`PlaceLive { rect: None }`),
  leaving the page (any `Show` or `Blank`), or the engine's end
  (`LiveEnded` with why) retire the island, and the document-lane child,
  which stayed underneath, shows again. The island follows the page area
  as the window resizes (`PlaceLive { rect: Some }`).
- **Engine** (`navigator-ladybird`, unbuilt here): CMake option
  `I6_PAGE` defines `ATRIUM_I6_PAGE` (with the island fixtures'
  `ATRIUM_I1_COUNTER` wiring). `editor-check.cpp` attaches the island,
  gets the page from `navigator_live_page`, paints it, reports its origin
  and title, and drives it. `patches/ladybird/live-page.patch` (after
  `fixture-fetch.patch`; LibWeb must be built with `ATRIUM_I6_PAGE` too)
  sends every HTTPS GET to `navigator_live_get` in `fixture_fetch.rs` and
  fails everything else closed. The Rust side type-checks on its own.

**Known gaps, by step.** No cookies or storage yet (I6.3): the fixture's
`session` and `local-storage` checks stay failed. Responses carry only
their content type, so a cross-origin `fetch()` to a sibling host fails
Ladybird's CORS check until navigatord passes the CORS headers (I6.5).
Links became trail entries in I6.2 (below); `history.pushState` not yet.

**Gate (Mac, arm64 Tessera).** Build the `I6_PAGE` engine, main's
navigatord, navigator-ui and navigator-fetchd, and serve the I6 fixture
(`scripts/navigator-i6-fixture.py`, names forwarded as
`scripts/navigator-i6-window.py` does). With `NAVIGATORD_LIVE_PAGES=1`:

1. Open `https://app.example/app.html` in an ordinary trail: the document
   lane shows it, the fixture server logs only the converter's fetches,
   and no engine process exists.
2. Press Run this page live, then Not now: still no engine.
3. Press it again and Run live: the engine starts, the chip reads Live
   code, the amber frame and tag show over the page area, and the
   server's log shows `/app.js` from the engine and the `module`,
   `indexeddb` and `wasm` check lines; `third-party` reads refused and
   navigatord logs `cdn.thirdparty.example` refused.
4. Stop: the engine process ends within 2 s, the chip returns to the
   document lane's tier, and the document-lane rendering is on screen.
5. Run live again and navigate to another trail entry: the engine ends.
6. Record Start (press to `live-started`) and the engine's peak RSS, three
   runs each.

#### I6.1 gate results on arm64 Tessera, 2026-11-25

Built on the Mac from main `cc12e8534`: the `I6_PAGE` engine
(`fixture-fetch.patch` then `live-page.patch` on a pristine
`ResourceLoader.cpp`; the worker audit passed), and main's navigatord,
navigator-ui, navigator-fetchd and limend, on a disposable overlay of
`~/atrium-vm/navigator-signin-base` with `NAVIGATORD_LIVE_PAGES=1`. The
fixture names pointed at 1.2.3.100 on a `net=1.2.3.0/24` QEMU network,
because jailed fetchd refuses private addresses (10.0.2.102) and cannot be
given the private-network grant.

Five fixes were needed, all now on main:

- navigatord refused Run live on a page the UI had just opened ("that page
  is no longer on screen"): a first open sends `OpenWithRecordingTier`,
  never `Show`. navigatord now remembers the page opened last and accepts
  it; opening another page ends a live engine.
- navigatord could end a healthy engine ("the engine reported
  https://app.example, not the page's origin") when the engine's `Started`
  arrived before the serve thread recorded the delivery; it now waits up to
  2 s for the delivery.
- The engine aborted (`VERIFICATION FAILED: agent.fetch_executor`):
  `editor-check.cpp` installs the inline fetch executor for `ATRIUM_I6_PAGE`
  too.
- The engine aborted on the fixture's cross-origin frame (`PageClient::
  request_navigation_start`, `VERIFY_NOT_REACHED`): `EditorPage.h` now
  refuses a live page's navigations, loudly (`live page: navigation to ...
  refused`), until I6.2 and I6.5.
- Module scripts never ran and `DOMContentLoaded` never fired, and nothing
  below 360 px was painted: `paint_document` now runs "the end" of parsing
  for `ATRIUM_I6_PAGE` (as for the VPN page) and lays a live page out at
  1024 x 768 instead of an island's 640 x 360. The engine still does not
  learn its placed size.

Results (engine `dd887c64…7861`):

1. The document lane showed the page; no engine process existed.
2. Not now: still no engine.
3. Run live: the engine started, the amber frame and the "live code" tag
   covered the page area, and the page's checks read `module` pass,
   `third-party` refused, `session` pass (not signed in), `indexeddb` pass,
   `notifications` pass, `service-worker` pass (refused), `webgl` pass
   (none), `wasm` pass (interpreter ran), `done` pass; `local-storage`
   failed (5,242,880-byte cap), `same-site-api` failed (CORS tainting, as
   expected until I6.5) and `geolocation` timed out. navigatord logged
   `cdn.thirdparty.example` refused (another site) and served
   `api.app.example` (same site). The check lines did not reach the
   fixture server, since `/report` is a POST, which I6.1 refuses.
4. Stop: the engine process was gone within 108 ms and the document lane
   showed again.
5. Opening another page from the Where field ended the engine, and so did
   choosing another trail entry in the rail (gone within 93 ms).
6. Start, press to `live-started`: 235, 295 and 313 ms; engine peak RSS
   92,228, 92,196 and 92,216 KB (sampled each second for 12 s).

Seen but not fixed: a live page whose own background is transparent
(`second.html`) shows the document lane through it, slightly offset, so its
text appears doubled; the tier chip sat under the headless display's top
strip in the captures, so the chip's "Live code" was read from the island's
tag and navigatord's `LiveStarted`, not from the chip. Evidence is in
`~/atrium-vm/evidence/navigator-i6-1-20261125/`.

#### I6.2 as built (2026-11-25)

Host-tested in the cloud; the engine side is written but not yet built.
Same flag as I6.1.

- **A navigation is a proposal.** A top-level GET a live page starts (a
  link, a GET form, `location`) reaches `EditorPage::
  request_navigation_start`, which reports it as `Report::Navigate(url)`
  (`navigator_live_propose`) and never follows it. Limen checks the report
  on both sides (`web_island::reportable`: plain HTTPS, printable, at most
  2,048 bytes; reports now frame up to 2,304 bytes) and takes it only
  while the page runs. A POST, a frame's document, or a jump within the
  same document is refused, loudly, as in I6.1.
- **navigatord** (`live_page::navigation`) takes one plain HTTPS address in
  its normal spelling, no credentials, and sends `LiveNavigate { from, url }`
  to the UI. Any origin: where it opens is the UI's decision below.
- **The UI** (`Shell::live_navigated`) follows it as a new trail entry,
  exactly like a followed link, so Back and Forward work as for any page.
  When navigatord shows the new page (`Shell::live_follow`): at the live
  page's origin (after any redirect), the UI sends `ContinueLive { url,
  rect }` and the page runs live again without another press (navigatord
  checks it is the page it shows and the origin it ran live, D8's "this
  origin, this trail"); at another origin it stays in the document lane
  and the live run stops. Back, another tab, a typed address, or a failed
  open stop it too.
- **Page size.** navigatord tells the engine its page area with the new
  `web_island::PageSize` control message (`P`, two `u16`, 1 to 16,384),
  just after the engine reports its start and whenever a placement changes
  the size; limend passes it only while the page runs. The engine lays the
  page out at that size (`navigator_take_page_size`), replacing I6.1's
  fixed 1024 x 768 after the first frame.
- **`history.pushState`** (second part, host-tested, engine unbuilt). The
  engine's `drive_page` reports each change of the document's address as
  `Report::UrlChanged(url)`. navigatord (`live_page::address_change`)
  takes only a plain HTTPS address at the page's own origin, sends
  `LiveUrlChanged { from, url }`, and ends an engine that claims another
  origin's. The UI (`Shell::live_url_changed`) adds a trail entry for it
  with nothing to open: the engine stays over the page area and the Where
  field shows the new address. When the page goes back to the address of
  the entry just before or after (its own `history.back()`, or a route
  pushed again), the trail moves to that entry instead of adding one.
  `replaceState` adds an entry too, since the engine reports only the
  address.
- **Back and Forward among a run's entries.** Every entry the run made or
  went to (`LiveRun::entries`) runs live again when the person moves to
  it, with no new press: the entry is opened as a page (a route the app
  doesn't serve shows the server's answer), then `ContinueLive` starts a
  fresh engine at that address. The engine's own history is not driven:
  LibWeb sends traversals to the UI process, which this single-page host
  doesn't have, so the app reloads at the route and keeps no page state.
- **`window.open`** still returns `null` (`page_did_request_new_web_view`
  gives no page), which the fixture reads as refused. Proposing it as an
  entry opened beside needs the address, which that call does not carry;
  it waits for the engine work on new web views.

**Gate (Mac, arm64 Tessera),** after the I6.1 gate's setup:

1. Run `app.html` live, press **Same origin page**: a new trail entry
   `https://app.example/second.html`, the chip reads Live code again with
   no prompt, and the fixture server's log shows `/second.html` fetched by
   the engine through navigatord (the `same-origin-nav` POST stays
   refused).
2. Back: `app.html` shows in the document lane and no engine runs.
3. Run live again, press **Another site**: the trail shows
   `https://elsewhere.example/` in the document lane, the live run stops,
   and no engine is left.
4. Resize the window while live: the page lays out at the new size (the
   navigatord log shows no "size not sent" line).
5. Run `app.html` live, press **Route (pushState)**: the Where field reads
   `https://app.example/route/1` as a new trail entry, the same engine
   process keeps running (same pid), and no fetch of `/route/1` reaches
   the fixture server.
6. Back: `app.html` runs live again with no prompt (a new engine pid).
   Forward: `/route/1` opens (the fixture server answers 404) and runs live.
7. Press **Open another site**: the page reports `window-open` refused and the
   trail is unchanged.

#### I6.2 second part: gate results on arm64 Tessera, 2026-11-25

Main `067dced7e` with J2: the `I6_PAGE` engine (`62dafe26…18c6`, audit
passed; its sources unchanged since `2ef0a53f7`), navigatord and
navigator-ui from main, and main's portcullisd, atrium-jaild, portcullis
and opifex. Under J2 jaild refused every app launch (`path.class_root`:
"/var/lib/atrium/jails/org.atrium.navigator is not a app jail's root for
mailgate") until the guest's `jaild.policy.toml` took the repository's
`[apps]` section (`class_roots`, `class_rw_subtrees`, `class_ro_subtrees`,
`sockets_dir`, `sockets`); `navigator-signin-base` predates it.

5. Run `app.html` live, press **Route (pushState)**: the trail gained
   `https://app.example/route/1` (two entries), the same engine process
   kept running (pid 4245 before and after), and no request for
   `/route/1` reached the fixture server. Passed.
6. Back: `app.html` ran live again with no prompt, in a new engine (pid
   4538). Forward: `/route/1` opened and ran live (pid 5195). The fixture
   server answers `/route/1` with the app's page (200), not 404 as the step
   expected. Passed.
7. **Open another site**: the page reported `window-open: refused`, the
   trail stayed at two entries and the engine kept running. Passed.

The cross-site link sequence of the first part's step 3 (live link, Back,
Run live, Another site) drew `elsewhere.example` in all three repeats.
jaild logged 34 warnings while removing jails, all `rctl -r` "No such
process" but one devfs unmount `EBUSY`; launches were not affected.
Evidence is in `~/atrium-vm/evidence/navigator-i6-2b-20261125/`.

#### I6.2 first part: gate results on arm64 Tessera, 2026-11-25

Main `87785725f`, set up as for the I6.1 gate: the `I6_PAGE` engine
(`d7526241…6452`, audit passed), navigatord, navigator-ui and limend from
main. The engine needed one compile fix: `EditorPage.h` includes
`LibWeb/DOM/Document.h` and `LibWeb/HTML/LocalNavigable.h` for
`ATRIUM_I6_PAGE`.

1. Run live, press **Same origin page**: a new trail entry
   `https://app.example/second.html`, navigatord logged `live-start` and
   `live-started` for it with no prompt, the island and "live code" tag
   showed it, and the fixture server logged `/second.html` twice (the
   document lane's fetch and the engine's through navigatord). Passed.
2. Back: `app.html` in the document lane, no engine process. Passed.
3. Run live, press **Another site**: the trail shows `https://elsewhere.example/`,
   and the engine ended. Passed, but the first time the page area stayed
   blank white although the document lane had laid the page out: the slot
   hidden for the live page was not shown again for the new page. Nine
   later tries (the same sequence from a fresh Navigator, and from Back)
   drew the page. navigator-ui now shows a newly attached child in a hidden
   slot unless its own page runs live; three repeats with it drew the page.
4. Resizing the page area while live (the rail's collapse button, since the
   headless injector cannot resize the window): the island followed the new
   page area and navigatord logged no "size not sent" line. Passed.

c6de813e7 (the document lane hidden while live) is confirmed: `second.html`,
which has a clear background, no longer shows doubled text. Evidence is in
`~/atrium-vm/evidence/navigator-i6-2a-20261125/`.

#### I6.3 as built (2026-11-25, first part: the jar)

Host-tested in the cloud; the engine side is written but not yet built.
Storage (D10) is the second part.

- **One jar per (persona, site).** When navigatord has Arca, a live run
  opens a broker view with `Broker::show_live` (`LiveJar::open`): the
  page's site partition in the trail's jar (`partition_in`, the persona's
  own for a recorded trail, the trail's throwaway jar for an Unrecorded
  one), cookied from the start. It is the same partition the document lane
  uses for that site once the person lets it know them, so a sign-in in
  either lane holds in the other. D9 said "(persona, origin)"; the site is
  the unit because D12 already lets a page reach its sibling hosts. The UI
  passes the trail's jar with `RunPageLive` and `ContinueLive`, and the
  prompt row now ends "Its sign-in is kept for next time."
- **Every request through Arca's couriers.** The page itself and each GET
  go out with a ticket (`fetch_once`: no redirect, as for `L`), and the
  cookies stay in Arca; the engine never holds one (`document.cookie`
  reads empty, so a script that copies a CSRF cookie into a header fails
  until script cookies are given to live pages).
- **A script's POST** (`fetch`, XHR) inside the page's site is served
  (I6.1 refused it): the engine's loader hands it to `navigator_live_post`
  with its content type, the island channel carries it (at most 32 KiB,
  Nuntius's `BODY_MAX`), and the broker sends it with
  `Fetch::script_post`, its type printable and at most 128 bytes. Only a
  live view may; a document-lane view still may not. Ranges and
  conditional requests stay refused.
- **A form the page submits** (a sign-in) is `Report::Submit { url, body }`
  (`application/x-www-form-urlencoded`, printable, at most 4 KiB; reports
  now frame up to 6,208 bytes). navigatord checks the address against the
  page's scope (D12), sends it through the jar with `Fetch::live_form`,
  follows the answer's redirects as a browser does after a form (a 307 or
  308 is refused), and sends the answering page's address as
  `LiveNavigate`, so it opens as the next trail entry and runs live again
  (I6.2). A multipart form is refused. A form whose answer is a page
  rather than a redirect opens that address with a GET, which loses the
  answer: rare for sign-ins, recorded here.
- Closing the run closes its broker view; the cookies stay in Arca until
  the person forgets the site or removes the persona.

**Gate (Mac, arm64 Tessera),** as for I6.2, with navigatord connected to
Arca (`NAVIGATORD_ARCA` and `NAVIGATORD_COURIERD`, as for cookied pages; without them a live page has no jar and its POSTs are refused) and main's arcad, limend
and navigator-cookies:

1. Run `app.html` live in a recorded trail: the page reads "Not signed
   in", and its check lines now reach the fixture server (`/report` is a
   script POST): `session` reads not signed in.
2. Press **Sign in**: the server logs `sign-in` pass, the trail gains
   `app.html` as a new entry, it runs live with no prompt, and the page
   reads "Signed in as ana".
3. Stop, then Run live again: still signed in.
4. Run the same page live in an Unrecorded trail: not signed in.
5. Add an item: `/api/items` (a script POST with JSON) answers its count.

#### I6.3 first part: gate results on arm64 Tessera, 2026-11-25

Main `a44a1e9d2`: the `I6_PAGE` engine (`live-page.patch` re-applied on a
pristine `ResourceLoader.cpp` after `fixture-fetch.patch`; `3f738d5c…`,
audit passed), navigatord, navigator-ui, limend, arcad, arca-courierd and
the courier from main, and portcullisd and jaild deployed with
`deploy-portcullis.sh` (J2). navigatord ran with `NAVIGATORD_ARCA` and
`NAVIGATORD_COURIERD`; arcad had a fresh store with mailgate unlocked, and
arca-courierd trusted the fixture CA under the private-network test grant.

1. Run live in a recorded trail: "Not signed in", and every check line
   reached the fixture server as a script POST to `/report` (204);
   `session` read not signed in. Passed.
2. **Sign in**: the server logged `POST /login` (303) and `sign-in` pass,
   the trail gained `app.html` as a new entry, it ran live with no prompt,
   and the page read "Signed in as ana"; its later requests carried the
   session cookie. Passed.
3. Stop, Run live again: "Signed in as ana" (`/api/me` with the session).
   Passed.
4. The same page live in an Unrecorded trail (Ctrl+Shift+N): not signed
   in, no session cookie. Passed.
5. **Add**: `POST /api/items` with JSON answered "1 on the server"
   (`script-post` pass). Passed.

jaild logged no warnings in the whole run. `api.app.example` (the sibling
host) is fetched without the session cookie, as the cookie is the host's.

#### I6.3 as built (2026-11-25, second part: storage)

navigatord side host-tested; the engine's Rust side type-checks; the
LibWeb glue that makes `localStorage` use it is the Mac step.

- **Where.** `live_storage::Stores` keeps one store per (jar, site): for
  the persona's own jar a file `<NAVIGATORD_STORE>/live-storage/<site>.store`
  (0600, written whole to a fresh file and renamed), for an Unrecorded
  trail's jar memory only, dropped when the trail's jar is forgotten.
  Forgetting a site (`Forget`) removes its store with its cookies.
- **Bounds.** 5 MiB of keys and values (LibWeb's quota) and 65,536 items a
  site; a change batch is at most 32 KiB (Nuntius's POST bound), and a
  batch that is out of shape or would break a bound changes nothing. A
  damaged file starts the site empty, logged.
- **On the channel.** After its page is delivered, the engine GETs
  `limen::web_island::LIVE_STORAGE` (`https://live-page.atrium.invalid/local-storage`)
  for its site's store and POSTs batches of set, append, remove and clear
  there (`serve` answers 204, or refuses). A value too long for one batch
  goes as a set and appends; if one is refused the value is left partial.
  The site is the delivered page's, so a page reaches no other store.
- **Engine.** `fixture_fetch.rs` has `navigator_live_storage_load` (each
  item to a callback), `navigator_live_storage_set`, `_remove` and
  `_clear`. Still to do on the Mac, with the pinned LibWeb source: for
  `ATRIUM_I6_PAGE`, fill the page's local storage from
  `navigator_live_storage_load` before its scripts run, and forward
  `setItem`, `removeItem` and `clear` to the three calls, failing a
  `setItem` navigatord refuses as LibWeb does over its quota.
- **IndexedDB** stays per run: LibWeb keeps it in memory, and persisting it
  is its own step.

**Gate (Mac),** after the LibWeb glue, with `NAVIGATORD_STORE` set:

1. Run `app.html` live: `local-storage` reads 1 visit.
2. Stop, run live again: 2 visits. Restart navigatord, run live: 3.
3. The same page in an Unrecorded trail: 1 visit; close that trail and
   open another: 1 again.
4. `live-storage/app.example.store` exists, mode 0600, owned by the
   persona's Navigator user, and no file appears for the Unrecorded runs.

#### I6.3 second part: the LibWeb glue and gate results, 2026-11-25

LibWeb's storage areas are mirrors kept behind one
`StorageAPI::StorageAreaMirrorClient` per process; with none set, every
area stays detached and `setItem` throws `QuotaExceededError` (the I6.1 and
I6.2 gates' `local-storage` failure). `navigator-ladybird/LiveStorage.h` is
that client for `ATRIUM_I6_PAGE`, set by `editor-check.cpp` once the page
is delivered: the local area is loaded once with
`navigator_live_storage_load` and written through with `_set`, `_remove`
and `_clear`, a refused set answers `storage_area_did_refuse_write` with
navigatord's contents, a refused removal or clear is acknowledged and then
reset to them, loudly, and the session area lives in the engine only.
No LibWeb source is patched.

Gate on arm64 Tessera with the glue (engine `590a8f67…`, main `6033c3f49`)
and `NAVIGATORD_STORE=/var/db/navigatord/store`:

1. Run `app.html` live: `local-storage` pass, "visit 1". Passed.
2. Stop, Run live: "visit 2"; restart navigatord, Run live: "visit 3".
   Passed.
3. An Unrecorded trail: "visit 1"; closing it (Ctrl+W forgot its jar) and
   opening another: "visit 1" again. Passed.
4. `live-storage/app.example.store` (in the Navigator's overlay) is mode
   0600, owned by `atrium-app-50000`, holds `visits` = 3, and no other
   store file appeared. Passed.

Evidence for both parts is in `~/atrium-vm/evidence/navigator-i6-3-20261125/`.

#### I6.4 as built (2026-11-25, first part: downloads and refusals)

Host-tested in navigatord, Limen and navigator-ui; no engine source
changed. File input, printing and the clipboard are the second part.

- **A link or form that answers with a file.** A navigation a live page
  proposes within its own origin is first fetched through its jar
  (`LiveJar::peek`, off the watcher thread), and a form's answer is looked
  at the same way (`LiveJar::submit`). `files::live_file` decides: HTML
  (or XHTML) not marked `Content-Disposition: attachment` and not sniffed
  as a file is a page, everything else a file, since only a page can run
  live. A page is kept for 30 s for the run that shows it (one slot, keyed
  by jar and address), so it is not fetched twice and a form answered with
  a page, not a redirect, keeps its answer. A file is held in the UI
  connection's `files::Waiting` under an id from the top half of the u32
  range (the UI's own `Open` ids never reach it), named from the
  disposition's `filename` or the address and made safe, and the UI gets
  `Event::LiveFile`. It asks `Prompt::Download` on the live page's own
  entry while the engine keeps running; Save, Open and Cancel are the
  document lane's (`Save`, `OpenFile`, `Discard` with that id). A file it
  could not ask about is discarded at once.
- **The reporter.** navigatord puts one tag first in the delivered page,
  after any doctype and comments before it so the page keeps its mode:
  `<script src="https://live-page.atrium.invalid/reporter.js">`
  (`limen::web_island::LIVE_REPORTER`), whose body is
  `navigatord/src/live_reporter.js`. The engine has no backend for the
  refused APIs, so it is what refuses them; the reporter answers each at
  once (a page waiting on a prompt nobody sees just stops) and GETs
  `LIVE_REFUSED?api=<token>` once per API: notifications, geolocation,
  camera, microphone, MIDI, USB, Bluetooth, serial, HID, service worker,
  push, background sync, a clipboard read outside a paste, and a new
  window (`window.open` still proposes its navigation, I6.2, and returns
  `null`). A page that gets around the reporter is still refused, only
  not reported. A page whose CSP forbids the script loses the reports.
- **A file the page's script made.** The reporter takes a click on an `<a
  download>` whose address is `blob:` or `data:` (on the way out, so a
  page that prevented the default keeps it), and `a.click()` on such an
  anchor outside the document: it reads the bytes and POSTs them to
  `LIVE_DOWNLOAD?name=…&type=…` as `text/plain` (so `no-cors` allows it).
  navigatord holds it as above, its kind from its bytes or its type. The
  bound is one Nuntius body, 32 KiB (`DOWNLOAD_MAX`); a bigger one is
  reported as refused. `serve` answers the reporter's three addresses only
  after the page and only in their exact shapes; nothing on navigatord's
  own name reaches the network.
- **Refusals in the popover.** The UI collects `Event::LiveRefused` per
  run, each once, at most 12, and the tier chip's popover says "Refused
  here: …" while the live page is on screen. navigatord tells it each
  refused API once, each host it refused a request to once ("a request to
  cdn.thirdparty.example"), a form to another site, and a navigation it
  could not make. `scripts/navigator-live-reporter-check.js` runs the
  reporter against stand-in browser APIs under node.

**Gate (Mac)**, on `app.html` run live:

1. Download report: the prompt row asks "report.csv, … CSV file from
   https://app.example" while the page keeps running; Save writes it to
   Downloads with its origin recorded.
2. Download notes (the blob link): the same prompt for `notes.txt`, text
   file, and navigatord's log names `notes.txt` from the reporter; Cancel
   discards it.
3. The checks `notifications` (denied), `geolocation` (denied 1),
   `service-worker` (refused) and `window-open` (refused) settle at once
   rather than timing out, and the popover reads "Refused here:" with
   notifications, your location, a service worker, a new window and a
   request to cdn.thirdparty.example.
4. Sign in: the fixture answers 303 to `/app.html`, and the run that
   shows it is served the page navigatord already fetched: one GET of
   `/app.html` through the jar after the POST, where I6.3 made two (the
   document lane's own fetch of the trail entry is separate).

#### I6.4 as built (2026-11-25, second part: file input)

navigatord and Limen host-tested; the engine glue is written and builds
on the Mac.

- **The ask.** LibWeb asks its page client for a file picker
  (`page_did_request_file_picker`); `EditorPage` sends
  `Report::PickFile` (one file, read-only, whatever the input's `accept`
  or `multiple` said). If it can't, the input closes empty.
- **The picker.** navigatord, which holds the island's Limen connection,
  requests Limen's `picker` role over the UI's window (a Scrinium sheet
  frescod places and frames; the page under it gets no input meanwhile)
  and opens it with `Filter::file()`, titled "Choose a file for
  <host>". One picker at a time, and none within 2 s of the last one
  ending (a page can't hold the picker up over the window; a second ask
  is answered empty and named in the popover). The picker's `File`
  message brings a read-only descriptor; navigatord reads at most 8 MiB
  (one live resource) and takes only the first file.
- **The answer.** When the picker says `Done`, `Cancelled` or `Failed`, or
  goes away, navigatord sends the engine the control message
  `FilePicked::Chosen { name }` or `FilePicked::Cancelled` ('F'; Limen
  takes it only while the page runs). The bytes wait for one GET of
  `LIVE_FILE` (`https://live-page.atrium.invalid/picked-file`) on the
  fetch channel, then are gone. The engine never learns a folder or a
  path, and no file but the picked one reaches it.
- **Engine glue** (Mac: check against the pinned LibWeb, the signatures
  are written from memory): `EditorPage::page_did_request_file_picker`
  calls `navigator_live_pick_file`; `navigator_page_pointer` returns 6
  on `FilePicked`, and `drive_page` takes it with
  `navigator_take_file_picked`, GETs `LIVE_FILE` with
  `navigator_live_get`, and calls `Page::file_picker_closed` with one
  `HTML::SelectedFile { name, bytes }` or none.

**Gate (Mac)**, on `app.html` run live:

1. Press the file input: Scrinium's sheet opens over the window, titled
   "Choose a file for app.example". Pick a text file: `file-input` passes
   with its name, size and first bytes, and `#file-out` shows them.
2. Press it again and Cancel: no new check line, and the input is empty.
3. A script that clicks the input twice at once (in the console of the
   fixture, or a second press within 2 s of the sheet closing): the
   second is answered empty, and the popover names "a file picker asked
   for again at once".

#### I6.4 as built (2026-11-25, third part: printing)

Host-tested; no engine change.

- The reporter replaces `window.print()` and takes Ctrl-P (Cmd-P) in the
  page: each GETs `LIVE_PRINT` (`https://live-page.atrium.invalid/print`).
  navigatord passes it on as `Event::LivePrint` at most once every 5 s (a
  second ask sooner is named in the popover), and the UI prints only for
  the run on screen, once at a time, exactly as its own Ctrl-P: the trail
  entry's document-lane child (hidden under the island, still attached)
  lays the page out and writes the PDF in its jail, and Prelum's sheet
  takes it. So a page printed while it runs prints what its address
  shows, not its live state; printing the live state needs the engine to
  paint to a PDF, which is its own step. limend's print-sheet slowing
  (printing.md §5.2) still applies.

**Gate (Mac):** on `app.html` run live, Ctrl-P in the page opens Prelum's
sheet over the window with the fixture's document-lane rendering; Cancel,
then a script's `window.print()` twice at once opens one sheet and the
popover names "a print asked for again at once".

#### I6.4 as built (fourth part: keys and the clipboard)

A live page got only clicks before this part; it now takes the keyboard. The engine's
painter marks the scene with `navigator_live_keys`, and each key on the island goes to
LibWeb's `handle_keydown` and `handle_keyup`, except Ctrl-C and Ctrl-V.

- **Copy.** On Ctrl-C the engine reads the focused navigable's selected text and sends
  `Report::Copy { text, token }` (limen `web_island.rs`; text at most `COPY_MAX`, 16 KiB,
  token non-zero). navigatord turns it into `Event::LiveCopy`, and the UI writes it through
  Tabula with `pergola::clipboard::copy_text_on`, but only when that live page is the one
  on screen (`Shell::live_on_screen`).
- **Paste.** On Ctrl-V the engine sends `Report::Paste { token }`. The UI reads Tabula with
  `paste_text_on`, then sends `Request::LivePasted { url, text }` if the text is at most
  `COPY_MAX`, or else says in the prompt row that nothing was pasted
  (`Prompt::PasteTooLong`). navigatord passes it to the engine as `Pasted` ('V' plus the text), and the
  engine types it into the page as key presses.
- **The token.** It is minted for the press in the island. frescod lets the island's parent,
  the Navigator UI, spend it on `COPY` or `PASTE` only, and only when the
  press was the chord itself (tabula.md §2.6): never a click or Enter. The page never reads
  the clipboard on its own: the reporter still refuses `navigator.clipboard.read` and
  `readText`.
- **The presenter.** frescod names the parent by the owner of the UI's window, and
  navigatord opens that connection (§2.3a of navigator-ui.md), so the owner is the
  Navigator app's uid, not the UI's per-instance one. Tabula passes its peer's uid as the
  presenter, so navigatord opens the UI's Tabula connection too (`Hello { clipboard }`,
  answered by `Event::Clipboard` with the descriptor) and the UI keeps it
  (`pergola::clipboard::Tabula::on`, never reconnected as the UI). The Navigator app's
  manifest has `clipboard = true`; the UI's has no capabilities.

Not built: a page's own `copy` and `paste` events with clipboardData, and rich (HTML) copy.

#### I6.4 gate results on arm64 Tessera, 2026-11-25

All four parts ran on one disposable arm64 Tessera guest under J2, with frescod and the
engine built from e008f640e (engine sha256 `9dd07bef…94b5` at the end, JIT audit
passed), then arcad, the courier and navigatord from ac5ba59b9. Evidence is in
`~/atrium-vm/evidence/navigator-i6-4-20261125/` on the Mac. jaild logged no `rctl` WARNs.

- **Downloads and refusals, 4/4.** `report.csv` was asked in the prompt row while the page
  ran; Save wrote `~/Downloads/report.csv` with `user.atrium.origin` set to its address. A
  script's `blob:` file (`notes.txt`, 17 bytes) was asked, and Cancel discarded it. The
  refusals settled at once (notifications denied, geolocation denied, service worker and
  `window.open` refused), and the popover named the third-party host and all four. The form
  sign-in made one POST (303) and one GET through the jar.
- **File input, 3/3.** The picker sheet read "Choose a file for app.example", and the pick
  reached the page (`pick-me.txt`, 33 bytes). Cancel added nothing and the engine ran on;
  the input still shows the earlier pick, since LibWeb keeps it. A second press within 2 s
  was refused and named in the popover.
- **Printing, 1/1.** Ctrl-P in the live page opened Prelum's sheet with the entry's
  document-lane rendering; Ctrl-P twice gave one sheet and a logged refusal.
- **Keys and the clipboard.** Typing reached the page's fields, and Shift+Home selected.
  Ctrl-C then Ctrl-V copied "anaxy" from one field into the other. Ctrl+A does not select
  all yet.

Fixed on the way: navigatord ended the run when a finished picker detached (only the
island's own embed ends it now); the engine stopped after 15,000 input polls, about five
minutes (now a 12-hour bound); a few engine compile fixes against the pinned LibWeb
(`SelectedFile` and `FileFilter` live in LibWebCommon, the selection is read from
`hosted_focused_navigable()`); and the clipboard's presenter above, which Tabula refused as
`WRONG_UID` until navigatord opened the UI's connection. The guest also needed Prelum's
render worker installed, Scrinium run `--no-window` as its app uid, and `_tabula`'s user.

#### I6.5 as built (2026-11-25, first part: the page's own site answers its scripts)

D12 lets a page's scripts reach its sibling hosts, but until now their
answers failed LibWeb's CORS check: the request carried no `Origin`, and
the answer reached the engine with only its content type.

- **`Origin`.** Arca's courier takes a new client header, `Origin`
  (`Header::Origin` = 8 in `arca-courier/src/schema.rs`), only in the shape
  `https://host[:port]` (`origin_value`); anything else refuses the whole
  request. It and the ticket's initiator are one claim: arcad's `COOKIES`
  takes the request's origin and allows it only from a ticket whose
  initiator is on its site (DENIED, the ticket unspent, otherwise), and the
  courier then refuses the request unsent. `navigator_cookies::Fetch`
  carries it (`from_origin`), only from a live view, on the first hop only.
  navigatord's `LiveJar` sends the page's origin with a request to another
  host of the site, with every script POST and with a form.
- **The answer.** navigatord reads the server's one
  `Access-Control-Allow-Origin` (`*`, `null` or one origin) and
  `Access-Control-Allow-Credentials: true`, and sends them to the engine
  as a new Nuntius event, `Event::Cors`, after the head and before the
  body. Two such headers, a list, or a value out of shape pass nothing on.
  The engine's `navigator_live_cors` hands them to LibWeb's response
  headers (`live-page.patch`), and LibWeb's own check decides.
- **Not yet.** A preflighted request (a POST with a JSON body to a sibling
  host, or any custom header) needs `OPTIONS`, which neither the engine's
  loader nor the courier sends; it fails closed. The jar adds the site's
  cookies whatever the script's `credentials` mode, because the engine
  does not pass the mode.

Tests: `an_origin_is_sent_only_as_a_plain_https_origin` (arca-courier),
the live jar's origin checks in navigator-cookies' broker test,
`a_cors_answer_is_one_origin_or_none` (nuntius wire),
`a_servers_cors_answer_is_passed_on_only_in_its_one_shape` and the serve
test's sibling-host answer (navigatord). The fixture's `same-site-api`
line is the Mac check.

#### I6.5 as built (2026-11-25, second part: cross-origin frames)

D11 runs each cross-origin frame in its own engine jail, composed by Limen
over the page. It is built at navigatord and in the page script, with no
engine change:

- **Which frames run.** A frame whose address is a plain `https` address
  of another origin than the page's (`live_frames::frame_address`). Its
  site may be any site: D12 bounds what a page's scripts fetch, and a
  frame's document is the frame's own page, which gets D12 for its own
  site in its own engine. That is how the reading here takes D11's "third
  party frames refused by D12 are not started": a frame that isn't a plain
  https address, or past the limits below, is not started and the popover
  names it. A same-origin frame loads in the page's engine as before.
- **Reported, never loaded here.** The reporter, in the page, finds each
  cross-origin `<iframe>`, points it at `about:blank` (the page still reads
  its address from `src`), and reports its place in device pixels
  (`LIVE_FRAME`) whenever it moves, and when it goes. navigatord asks Limen
  for another island in the UI's window at `z` 2, over the page's island,
  places it at the page's place plus the frame's, cut to the page area
  (`frame_rect`), hides it while none of it shows, and moves it with the
  page. frescod's rule that a covered island takes no input leaves out
  another island of the same parent lying wholly inside it
  (`island_covered_in`), so the page keeps its clicks and keys outside the
  frame and the frame takes its own. Limits: 4 frames at once, 16 frame engines over a run, nothing
  smaller than 16 × 16 pixels (a tracking pixel is not started), 64
  reports waiting.
- **Its own engine.** The same engine, served by `serve` with the frame's
  address as its page, so its scope is its own site. Its jar is thrown
  away: a fresh in-memory Arca partition per frame, forgotten when it ends
  (`close_throwaway`), and no `localStorage`. It reports `Started` with its
  own origin or is ended. Its navigations, forms and file inputs are
  refused and named in the popover; its copy and paste go to the UI as the
  page's do.
- **postMessage.** The page's `iframe.contentWindow` for a frame is a
  stand-in with `postMessage`; the frame's `window.parent` is a stand-in
  whose only member is `postMessage` (anything else throws `SecurityError`,
  as across origins). A message is JSON text, at most 64 KiB, POSTed to
  `LIVE_MESSAGE` with its target. navigatord's `Mailbox` delivers it only
  to a receiver at the target's origin (`*` any, `/` the sender's own),
  stamped with the sender's real origin, at most 32 waiting per engine.
  Each side reads `LIVE_MESSAGES` every 250 ms while it has a frame or is
  one, and dispatches a `message` event. A message waiting for a frame is
  checked again when it is taken, so a frame that has moved to another
  origin doesn't get it. Messages and reports don't count toward a page's
  4,096 network requests; navigatord's own addresses are held to 20
  requests a second instead, past which they are refused and the page
  runs on.
- **Not yet.** `window.top` in a frame is the frame itself
  (`[LegacyUnforgeable]`, the script can't replace it); messages carry
  JSON, not transferables or ports; a frame's own frames are not run; a
  frame the page's script hides from the reporter (built before it runs,
  or in a closed shadow root) is never started. The page's engine may
  have asked for the frame's document before the reporter pointed it
  away, which shows as a refused request in the popover.

Tests: `live_frames` (address, placement, the mailbox),
`a_page_and_its_frame_run_apart_and_talk_only_through_the_mailbox` and the
reporter's parse checks (navigatord), and the frames half of
`scripts/navigator-live-reporter-check.js`. The fixture's `frame` line is
the Mac check.

**Mac check, 2026-11-25 (arm64 Tessera, ac5ba59b9).** The fixture's
`same-site-api` line passed ("api.app.example answered") and its `frame`
line passed ("pay.example running; parent DOM unreachable"), with the
frame in a second engine. Once the frame ran, the main page took no clicks
or keys: frescod's covered-island rule held the page island under the
frame island. 534c8262c lets a sibling island wholly inside it pass. Its
recheck on a fresh arm64 Tessera guest passed. With the frame running,
typing in the main page, then the frame, then the main page again landed
each time in the field clicked, and Ctrl-C/Ctrl-V in the main page still
worked. jaild logged no WARNs.

#### I6.6 as built (2026-11-25, first part: D8, the remembered choice)

- **Prompt.** In a recorded trail, the run-live prompt asks "Not now",
  "Always for this site" and "Run live". "Run live" stays the primary
  answer. An Unrecorded trail only gets "Not now" and "Run live". Either
  answer needs the counted press, the same as before.
- **What "Always" sends.** "Always for this site" sends `RunPageLive` with
  `remember: true`.
  - navigatord adds the page's origin to the persona's list once the run
    has started (the press spent and the engine asked for), and answers
    `LiveRemembered`.
  - It ignores `remember` for an Unrecorded jar and in a sign-in window.
- **The list.** It lives in `navigatord/src/live_remembered.rs`, in the
  file `live-origins` under navigatord's store directory.
  - That directory is on the persona's encrypted volume, so the list is
    per persona and is removed with the persona.
  - Each line is one `https://host[:port]` origin. The list holds at most
    256 (`REMEMBERED_MAX`); remembering one more drops the oldest.
  - It is written whole: a fresh 0600 file, synced, then renamed. This is
    the same writer as the localStorage stores.
  - It is read again on every check, so another window of the same
    persona sees a change at once, even when another navigatord serves
    it. Each change holds an exclusive lock on the sibling file
    `.live-origins.lock` from its read to its write, so a revoke in one
    navigatord and a remember in another can't lose either.
  - A file that is too long, has too many lines, or has any line that is
    not exactly such an origin counts as empty. An unreadable list never
    runs anything unasked.
- **A later visit.** After `Opened` for an HTTPS page of a remembered
  origin in a recorded trail, with live pages on and not in a sign-in
  window, navigatord sends `LiveRemembered { url }`.
  - The UI then asks `ContinueLive` for the page, if that page is the one
    on screen and nothing runs live.
  - navigatord accepts `ContinueLive` when the origin is the one run live
    in this window, as in I6.2, or when it is remembered and the jar is
    `Personal`. Either way it checks again itself.
- **Revoking.** While the page on screen is of a remembered origin, the
  chip's popover has the row "Ask before running this site live".
  - The row sends `ForgetLive { url }`, and navigatord takes the origin
    off the list.
  - A page that already runs live goes on running until Stop or until
    the person leaves it.
- **What vouches for the choice.** frescod's press proves that the person
  pressed an answer of the prompt. It doesn't prove which one: the UI's
  word picks `remember`, as it does for every other answer. The UI is the
  trusted side here; frescod does not bind the choice.
- **What "Always" allows.** With an origin remembered, every top-level
  page of that origin opened in a recorded trail runs live, with its
  cookies and no press. That includes a page another page linked to or
  sent the person to. This is the exception to "live needs a press"
  (threat model §2): the person granted it for that origin, the chip
  says Live code, and the chip takes it back.
- **Left for later.** Listing and revoking from Curia waits for
  Navigator's Curia integration, which does not exist yet. Until then the
  chip is the only place to revoke.
- **Tests.**
  - navigatord: `live_remembered` (kept across navigatords until
    forgotten, bounded with the oldest dropped, bad lists count as empty
    with a positive control, and eight writers at once lose no change,
    which fails with the lock taken out).
  - navigator-ui: `a_site_chosen_always_runs_live_unasked_until_the_chip_asks_again`.

#### I6.6 as built (2026-11-25, second part: recycling)

What already recycles a whole live page, from I2:

- **Idle.** frescod retires an island 30 minutes after the last input it
  routed there (`FRESCOD_ISLAND_IDLE_S`). Limen ends the engine, and
  navigatord answers `LiveEnded`.
- **Memory.** The engine's jail has an RCTL `memoryuse` SIGKILL rule,
  which portcullisd sets from the engine manifest and limend requires.
- **Either way,** the trail shows the document lane again at the address
  the live page was on. A remembered origin (D8) runs live again on its
  next open; any other origin needs the chip's "Run this page live"
  again. Unsaved page state is lost, as with a widget.

What this part adds (Shankar was asked for the limit; built as
recommended, 1 GiB for pages):

- **The limit by role.** A whole page asks Limen with
  `REQUEST_PAGE_ISLAND` (`Parent::request_page_island`) and gets the
  engine manifest's own limit.
  - `REQUEST_ISLAND`, used for widgets and a live page's frames, now
    passes `limen::web_island::PART_MEMORY_MB` (256) to portcullisd.
    portcullisd can only tighten the manifest's limit with it.
  - Every other start of the engine (a plain `web-island` request, as the
    I0 fixtures make) also gets the part's limit. Only
    `REQUEST_PAGE_ISLAND` gets the manifest's.
  - A standby engine (D7) is launched with the part's limit. A whole page
    therefore never takes the standby and always starts cold, which
    leaves the standby for the next widget.
  - The engine manifest says `memory = "1G"`
    (`navigator-ladybird/counter/atrium.toml`; the installed engine
    bundle on the Mac needs the same line).
- **One pool.** The manifest declares `[resources.pool]` (weight 1, floor
  256 MiB), so `memfed --pools-from` budgets every engine as one share.
  Frames and windows divide that share rather than add to it. Shipped
  memfed doesn't run `--pools-from` yet: its jail needs the apps directory
  mounted. Until then the pool is only declared.
- **What the person is told.** An engine that exits by itself (Limen's
  `ChildLost`, most often the RCTL kill; Limen can't tell which) ends with
  "the page's engine stopped; it may have used more memory than a live
  page may", in the UI's note.
- **Tests.** In limen,
  `island_fetch_channels_are_private_to_the_signed_navigator_request`
  checks that two parts ask for 256 MiB and a page for the manifest's
  limit. `a_navigators_next_island_takes_its_warmed_standby_engine` checks
  that a page leaves the standby waiting. The delegated UI is refused the
  new request too.
- **Left for later.**
  - The engine's own cooperative trim when memoryd sends SIGINFO (drop
    caches before it is killed). That is engine-side, a Mac build.
  - memfed `--pools-from` in the shipped service.
  - A Mac check: a fixture page that grows past 256 MiB keeps running up
    to 1 GiB and then falls back to the document lane, and a widget is
    still killed at 256 MiB.

#### Costs

- A live whole page is a large attack surface in a JIT-off engine: the jail,
  Capsicum, the origin-scoped fetcher and the persona's separate uid bound
  the harm, as for islands, but do not remove it (§8).
- JIT off makes script-heavy apps slow. I6.0 records how slow; making them
  fast is not in I6.
- Without WebGL some fat clients (Figma) will not run at all.

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
rectangle in viewer-local pixels to Navigator over Limen. The hit must land
inside the resident band. Placement uses the widget's full CSS-clipped bounds
intersected with the viewport, so adjacent bands agree on the rectangle.
The viewer tracks one selected key and source. It sends a placement message
only when the visible rectangle changes, and withdraws it when the widget
leaves view or a new document loads. Returning to view requires a fresh press.
Chrome updates only a matching row and drops widget rows on page switch,
history, reload or child loss. The signed UI treats the message as advisory
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

Four placement and withdrawal regressions passed on an arm64 HVF overlay
with Tessera mounted read-write at `/`. Adjacent layout bands agreed on the
CSS-clipped bounds. In the viewer event-handler test the initial counter
rectangle was (40, 40, 640, 360). Setting the scroll offset to 80 px produced
one update to (40, 0, 640, 320). At 500 px the viewer withdrew the widget
once. Scrolling back required another press, and replacing the document
withdrew the selection. The two shell tests checked the recorded origin and
preservation of unrelated prompts and their guard when a widget disappears.
The tests used the FreeBSD test binaries. Evidence is in
`~/atrium-vm/evidence/navigator-i1-placement-20261120/`.

The I1 fetch-channel gate passed twice on an arm64 HVF overlay with a
read-write Tessera root. Once the fixture parent received a press inside the
document's slot, Limen created a private socketpair for that embed. Ladybird
read the 460-byte counter page in one GET and ran its script with JIT off.
An outside press caused no fetch. The visible count went from 0 to 1, stayed
at 1 through a press on the covering window, then reached 2 after uncovering.
Closing the fixture window ended the worker.

The second run used a separate `_limen` daemon and Portcullis
`ExecInstance`. Portcullis verified the installed worker manifest and ran it
as `_navpage` (uid 65035). The worker reported capability mode and ECAPMODE
on a pathname lookup. The jail had IPv4 and IPv6 disabled. This gate supplied
a test launch-registry binding for the parent, a fixed worker uid rule and a
local responder for the counter URL. The 22 FreeBSD Limen broker tests passed,
including publisher refusal and endpoint isolation for two simultaneous
embeds. Frames, launcher logs and executable hashes are in
`~/atrium-vm/evidence/navigator-i1-fetch-channel-20261120/`.

Navigator now restricts its UI's delegated Limen connection before handing
over the descriptor. The UI can open its document viewer, credential field
and print sheet. The private island fetch transport requires a separate
broker connection. For guarded actions the UI registers its renderer-local ID for the origin
strip and its compositor-issued embed token for the prompt. Frescod resolves
the window token and checks the press against that compositor window before
spending it. The widget question retains the exact recorded source
alongside its origin and key. A replacement source retires the old question
even when the UI's rate limit delays the replacement or the replacement is
absent from the recording.

On 2026-11-20 the restricted-channel and widget-selection regressions passed
on a fresh arm64 HVF overlay with Tessera mounted read-write at `/`. The
guest ran all 23 Limen daemon tests, 22 navigatord tests and 65 Navigator UI
tests. The delegated socket opened a document viewer and refused engine
launches. A separate broker connection could request an island. With a test
compositor reply, the broker rejected a press from a different window and
accepted one from the registered window. UI tests exercised same-key source
replacement, withdrawal and the current recording's origin. The three guest
executables matched their staged SHA-256 hashes. Test logs and shutdown
evidence are in `~/atrium-vm/evidence/navigator-i1-delegation-20261120/`.

The earlier broker test supplied the same numeric window ID on both sides.
In split mode a renderer assigns local IDs and the compositor assigns global
IDs. The window-token request removes that numeric comparison. The live
activation probe submits a press for a second live window of the same uid,
then submits the same press for the intended window and checks replay. It
prints the local and compositor IDs for the successful receipt. On a fresh Tessera-root arm64 HVF overlay the guarded window was local
ID 1 and compositor ID 10. A press presented against another live window
of the same uid was refused. The intended window accepted the same press
once and refused replay. A covering window and an absent guard each caused
refusal. Uncovering the row restored acceptance. All 28 live probe checks
and all 22 Navigator broker tests passed. The three guest binary hashes
matched staging. Logs and a composed frame are in
`~/atrium-vm/evidence/navigator-i1-split-window-20261120/`.

The installed Navigator gate ran on arm64 HVF with Tessera at `/`. With
`NAVIGATORD_I1_COUNTER=https://widget.example/counter.html` in the broker
bundle, the recorded parent offered **Not now** and **Start** for that
source. The broker checked the recording ticket, source and compositor
press before requesting the island. A separate signed fetch fixture served
one anonymous 460-byte GET on the worker's brokered channel. Two presses
on Increment produced Count 1 and Count 2. Opening History withdrew the
island and the engine exited. Portcullis verified the engine signature
and allocated uid 50007 under a per-instance worker rule. FreeBSD reported
Capsicum mode and both IP families disabled. Screenshots, engine hash and
service logs are in `~/atrium-vm/evidence/navigator-i1-installed-start-20261120/`.

The guest fixture fetcher served local bytes. On the Mac the production fetch service
loop accepted a 200 HTML document from a local TLS server and refused a 302.
The server saw neither cookies nor authorization headers and received no
request for the redirect target. All 80 host fetch tests passed.

The installed TLS gate then ran on a fresh arm64 HVF Tessera overlay.
The fetcher bundle contained the fixture CA and host entries. An isolated
QEMU guest forward carried HTTPS to a loopback server on the Mac. The
server observed the parent GET first. Opening the Start prompt made no
widget request. Accepting it fetched 460 bytes from the widget origin and
Increment changed Count 0 to Count 1. On a 302 to the parent origin the
fetcher refused the response, the counter exited, and the server received
no request for the redirect target. Both origins saw no Cookie or
Authorization header. Restoring the 200 response and pressing Start
again in the same window produced a working counter without reloading
the parent. The guest hashes of the content-signed fetcher and interpreter
matched staging. Ladybird ran as uid 50007 in Capsicum mode with both IP
families disabled. In that build only the broker reported the refusal, and a later
placement could still address the exited worker.
Screenshots, server requests, hashes and service logs are in
`~/atrium-vm/evidence/navigator-i1-installed-tls-20261120/`.

Navigator now matches a widget failure to both its recording ticket and the
compositor receipt for that attempt. The broker retires its Limen connection
on a fetch failure, detach or child exit. The shell clears the matching
placement and shows a dismissible notice. Closing the notice allows a fresh
Start. Pressing the placeholder again during a pending Start leaves that
request in flight.

The installed arm64 Tessera gate exercised a TLS 302 and termination of a
running counter. The window showed “The live widget could not load its
page.” and “The live widget stopped.” respectively. For the replacement
case the fixture held the first GET open. Opening History withdrew that
worker. Returning to the same recording and pressing Start fetched the
counter again. The replacement reached Count 1 before the first GET
returned its 302, retained the same PID and reached Count 2 afterward.
The broker logged the old refusal and the window stayed on the counter.
The FreeBSD broker and UI suites passed, including the same-slot stale
receipt regression. Screenshots and logs are in
`~/atrium-vm/evidence/navigator-i1-widget-lifecycle-20261120/`.

Evidence for the two gates is in
`~/atrium-vm/evidence/navigator-i1-geometry-20261120/` and
`~/atrium-vm/evidence/navigator-i1-counter-20261120/`.

### I2 stopping a running island

Frescod draws Stop inside the upper-right edge of an uncovered island when
the visible frame has room for the control. Limen registers a dedicated
stream with frescod after checking the parent and child window owners.
The compositor consumes the primary press and release. Limen withdraws
the bound slot, tells the child to detach, and ends the worker through
its launcher. Navigator clears the placement for the matching Start
receipt and leaves the document viewer in place. Selecting the box
again requires a fresh guarded Start.

The installed gate passed on arm64 HVF with Tessera at `/`. Stop retired
the counter at Count 1. The UI and document viewer kept their PIDs. The
placeholder opened a new prompt without fetching. Start launched a
different engine PID at Count 0, and Increment advanced it to 1. The
fixture served the parent once and the counter once per accepted Start.
A compositor regression checked a covered frame, an engine-targeted
press, a press outside Stop, and consumption of the matching release.

To repeat on the staged 1280 × 840 TLS fixture, open the counter parent in
a fresh 1120 × 740 Navigator window and pass its fresh log to the driver:

```sh
python3 scripts/navigator-i2-stop-check.py SSH_WRAPPER GUEST_STAGE NAVIGATOR_LOG EVIDENCE_DIR
```

### I2 live parts in the tier chip

Navigatord reports an attachment when Limen confirms the expected embed. It
supplies the origin from the granted widget URL. Navigator applies the event
only to the document and Start receipt that requested the widget. The visible
chip, its details and its accessibility label use the same page-local facts.
An offered widget contributes zero. A confirmed attachment contributes one.
Stop, failure and withdrawal clear the attachment. Trail storage omits it.

In the installed arm64 Tessera gate the static parent gained one live part
from `https://widget.example`. The details named the parent as static and
identified the embedded code separately. Stop cleared the count. A fresh
Start restored one part with a different counter PID. Terminating that
worker cleared the count and showed the failure prompt. The UI and document
viewer kept their PIDs. To repeat, set `NAVIGATOR_UI_AX=1` in the fixture’s UI
configuration and append `--tier` to the command above. The host driver uses
Tesseract and ImageMagick to check the rendered fixture.


Guest service logs, executable hashes and composed frames are in
`~/atrium-vm/evidence/navigator-i2-stop-20261120/`.

### I2 Replayed parent

The converter writes the chosen document tier into the recording. On a
tier-aware open, navigatord reads its top-level `tier` and reports tier 2
to the requesting UI before delivering the page ticket. The UI applies
that result to the opened trail entry. A confirmed island attachment
changes only the live-part count. Older open requests keep their existing
wire replies.

The installed arm64 Tessera gate opened `replayed-parent.html` in a fresh
Navigator window. An inline script replaced “Waiting” with “Amaryllis blooms”
during conversion. The composed frame showed that output before Start and
after Stop. OCR read “Replayed” throughout and “1 live part” only while the
counter was attached. The accessibility label named `https://widget.example`
during attachment. The JIT-off Ladybird worker (PID 5454, jail 32, kernel
state `SCJ`) advanced from Count 0 to Count 1. Stop removed the worker and
the UI and viewer kept their PIDs. All 147 regression tests passed.

To repeat, serve the parent fixture on `parent.example` and the existing
counter fixture at `https://widget.example/counter.html`. Stage the
`forum-wm` example `place_navigator_fixture` in the guest test directory
beside the fixture WM socket. Start with an empty window and
`NAVIGATOR_UI_AX=1`, then pass `--replayed-parent` to
`scripts/navigator-i2-stop-check.py`. The placement helper moves the window
below Fresco’s trusted strip so both chip labels are visible. Screenshots,
logs and binary hashes are in
`~/atrium-vm/evidence/navigator-i2-replayed-parent-20261121/`.

### I2 idle retirement and the memory ceiling

A visible web island gets 30 minutes from attachment or the last input Fresco
routed to its window. Key, pointer, magnify and gamepad events renew that
window’s deadline. Painting and page messages cannot renew it. Both display
loops include the nearest island deadline in their sleep calculation. At
expiry Fresco uses the same private stream as frame Stop. Limen retires the
bound worker and Navigator clears its live part. A fresh Start reloads the
widget with a new process. Unsaved widget state is lost.

The engine manifest sets a 256 MiB resident-memory ceiling. Limen requires
Portcullis to apply the limit at launch. FreeBSD enforces it with a per-jail
RCTL SIGKILL rule. Navigator shows its ordinary worker-exit prompt when a
worker reaches the ceiling.

The installed arm64 Tessera gate shortened the compositor interval to ten
seconds. Two increments six seconds apart kept the first worker alive past
its original deadline. Leaving it idle retired it cleanly. Its replacement
started at Count 0. On its third increment the fixture retained and touched
8 MiB chunks. A diagnostic RCTL rule at the same threshold recorded the
counter PID at 268435456 bytes, and the worker exited. The live count cleared.
After closing the failure prompt, a guarded Start produced a third PID at
Count 0 and Increment reached 1. The UI and parent viewer kept their PIDs.

To repeat the installed counter gate, copy `test-assets/navigator-i0/counter-recycle.html`
into a private fixture directory as `counter.html`. Use that directory for
both the HTTP fixture’s widget response and `--embed-dir` when compiling
the counter’s `editor-check.cpp` with `ATRIUM_I1_COUNTER`. Link and install
the test binary with the normal counter manifest. Set `FRESCOD_ISLAND_IDLE_S=10`
on Frescod and `NAVIGATOR_UI_AX=1` in the installed UI configuration.
Run `scripts/navigator-i2-stop-check.py SSH_WRAPPER GUEST_STAGE NAVIGATOR_LOG EVIDENCE_DIR --recycle`
from a fresh Navigator launch. The gate queries the kernel rule and adds a
same-threshold logging rule only to the ephemeral worker jail. Guest logs,
hashes and frames are in `~/atrium-vm/evidence/navigator-i2-recycling-20261120/`.

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
| **D7** ✓ | How a warmed engine gets its own jail (I5 warm start) | fork zygote in a per-persona parent jail · fork zygote on the host · warm pool of ordinary one-shot workers | **Decided (Shankar, 2026-11-24): warm pool, one standby per persona with a live Navigator.** No new privileged process or jail creator, its own ASLR per island, and today's I5 checks apply unchanged; the cost is one idle engine's memory per persona |
| **D8** ✓ | Is "run this page live" remembered (I6) | ask on every visit · remember per (persona, origin) until revoked · remember for the session only | **Remember per (persona, origin), revocable in Curia and from the chip.** A fat client used daily should not need a press every visit, and a choice that is per persona and visible in the chip keeps it the person's. Never automatic for an origin not chosen |
| **D9** ✓ | Whose jar a live page uses (I6) | a jar per (persona, origin) kept by Nuntius through Arca couriers · a throwaway jar per run | **Kept per (persona, origin) through Arca's couriers**, as sign-in windows do (D6). Apps people run live are the ones they sign in to; a throwaway jar would sign them out at every Stop. The engine still never holds a cookie |
| **D10** ✓ | Where a live page's storage lives (I6) | a per-(persona, origin) directory on the persona's encrypted volume, mounted into the engine jail · in memory for the run only · kept by navigatord and served over the engine's channel | **Kept by navigatord, one store per (jar, site), on the persona's encrypted volume** (Shankar 2026-11-25, replacing the mounted directory accepted 2026-11-24). As Ladybird keeps storage in its browser process, the engine never holds a file and no mount authority is added; the engine loads its site's store and sends each change ([I6.3 second part](#i63-as-built-2026-11-25-second-part-storage)). Persona removal destroys it with the rest (Tessera E5) |
| **D11** ✓ | Cross-origin frames inside a live page (I6) | each its own engine jail, started with the page · a box the person presses, as in the document lane · not shown | **Its own engine jail, started with the page**, composed by Limen as a nested island. Payment and sign-in frames must work inside an app, and asking per frame would break it; the origin stays the isolation unit (legacy-web §7.6). Third-party frames refused by D12 are not started |
| **D12** ✓ | What a live page's scripts may reach (I6) | its own origin only (legacy-web §5.4.1c) · its registrable domain · its registrable domain plus origins it names in a declared list | **Its registrable domain**, measured in I6.0 before it is fixed. Fat clients keep their APIs and static assets on sibling hosts; third-party requests are refused and listed in the popover. The chip's words change to "send it to atlas.example's sites" |
| **D13** ✓ | GPU for live pages (I6) | none in I6 · a jailed GPU process later | **None in I6.** The threat model keeps GPU descriptors out of engines (§2). A jailed GPU broker for WebGL is its own spec, opened only if I6.0 shows that apps the person needs cannot run without it |

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

## 10. Progress log

The running record of each step's runs on the Mac and in cloud VMs, moved
here from the HANDOFF row on 2026-11-24 so the row stays short. Newest
entries are at the end of each part. Detailed results live in the step's own
section under §7; this log says what ran, when, and what it showed.

### 10.1 Sign-in windows (I3) and the VPN gateway

Installed JIT-off Ladybird ran the external script. Its Continue button proposed the form, trusted chrome confirmed the POST, Keycloak authenticated through Arca and Crypta redeemed the token. The Tessera guest fetched `crypta-ac-ok` from `10.98.0.1` over `tun0`. The follow-up verified zero POSTs on abandonment and a fresh retry that needed confirmation before one script POST, signed assertion and token redemption. The sentinel returned over `tun3`. Ctrl-R from the focused Ladybird page now discards the pending proposal and fetches a fresh script in the same window. The arm64 gate saw zero POSTs before confirmation, exactly one after, and `crypta-ac-ok` over `tun0`. A second installed gate discarded two pending proposals in the same window, confirmed only the third, and returned `crypta-ac-ok` on the same engine start. The next installed gate disconnected with the third proposal pending and recorded zero POSTs. After reconnect and a fresh sign-in window, only trusted confirmation released the new proposal and traffic through ocserv on `tun1`. The automatic-dismissal gate passed on arm64 Tessera. Disconnect closed the old window with zero POSTs, and reconnect waited for fresh confirmation before `tun1` returned `crypta-ac-ok`. Immediate reconnect with the old window still open also passed. Pressing its old confirmation sent zero POSTs, and only fresh confirmation allowed the sentinel. Both fixtures shut down cleanly. Queued Sign In passed on arm64 Tessera. One request opened a fresh window after the stale jail exited. The old confirmation sent zero POSTs, and only fresh confirmation allowed `crypta-ac-ok` on `tun1`. The next gate disconnected with Sign In queued. Portcullis discarded the request, the queued window never opened, and the gateway saw zero POSTs. A separate reconnect returned `crypta-ac-ok` through `tun2` after fresh confirmation. Both fixtures shut down cleanly. Requeueing before the old window closed launched only the newest request. After fixing premature Arca field detachment, the gate saw zero stale POSTs and fetched `crypta-ac-ok` on `tun5` after fresh confirmation. A fixture-only worker hold forced an in-place queue replacement before the old jail closed. Only the newest window opened, the gateway saw zero stale POSTs, and fresh confirmation released `crypta-ac-ok` through ocserv on `tun2`. The batched follow-up forced two replacements, cancelled the newest entry before teardown, and verified no queued window or POST. A separate reconnect waited for fresh confirmation and returned `crypta-ac-ok` over `tun8` in the same VM session. The next gate cancelled after two replacements and requeued before teardown. Only the latest window launched, with zero stale POSTs and `crypta-ac-ok` over `tun4` after fresh confirmation. The follow-up disconnected with the latest proposal pending. The window closed with zero POSTs, and a fresh reconnect waited for confirmation before `tun5` returned `crypta-ac-ok` through ocserv. The overlap gate queued the next Sign In while the disconnected requeued window was still open. The stale press sent zero POSTs, and the new window opened without a second request. Fresh confirmation returned `crypta-ac-ok` on `tun5`. The next gate disconnected again before that queued request could launch. Portcullis discarded it, no window opened, and the gateway saw zero POSTs. A separate reconnect waited for fresh confirmation and returned `crypta-ac-ok` on `tun6`. The next gate requeued Sign In after the second disconnect while the old window was still open. Only the newest request launched. Zero stale POSTs were sent, and fresh confirmation returned `crypta-ac-ok` through ocserv on `tun6`. The follow-up disconnected with that newest proposal pending. Its window closed with zero POSTs, and a fresh reconnect returned `crypta-ac-ok` through ocserv on `tun7` only after confirmation. The overlap variant queued Sign In before that newest window closed. Only the queued reconnect opened after teardown, with zero stale POSTs and fresh confirmation before `crypta-ac-ok` on `tun9`. The cancellation variant discarded that queued request before the older window closed. No new window or POST followed. A separate reconnect required fresh confirmation before `crypta-ac-ok` returned through ocserv on `tun8`. The follow-up requeued before the old window closed. Only the newest window launched, with zero stale POSTs and `crypta-ac-ok` on `tun8` after fresh confirmation. The pending-proposal variant disconnected that newest window with zero POSTs. A separate reconnect waited for confirmation before `crypta-ac-ok` returned on `tun9`. The follow-up queued the next Sign In before that disconnected window closed. The stale press sent zero POSTs, the new window opened after teardown, and fresh confirmation returned `crypta-ac-ok` on `tun9`. The next gate disconnected while that request was queued. It opened no window and sent zero POSTs. A new reconnect returned `crypta-ac-ok` on `tun10` after fresh confirmation. The follow-up requeued before the prior window closed. Only the newest window opened, the stale press sent zero POSTs, and fresh confirmation released `crypta-ac-ok` through ocserv on `tun10`. The follow-up disconnected that newest requeued proposal while pending. Its window closed with zero POSTs, and a separate reconnect returned `crypta-ac-ok` through ocserv on `tun11` after fresh confirmation. The overlap variant queued the next Sign In before that window closed. The stale press sent zero POSTs. The queued window opened after teardown, and its fresh confirmation returned `crypta-ac-ok` on `tun11`. The cancellation variant discarded that queued request before teardown, with no new window and zero POSTs. A separate reconnect returned `crypta-ac-ok` on `tun12` after fresh confirmation. The requeue variant queued Sign In before the prior window closed. Only the newest window opened, the stale press sent zero POSTs, and fresh confirmation returned `crypta-ac-ok` on `tun12`. The follow-up disconnected the newest window with its proposal pending. Zero POSTs were sent, and the next reconnect returned `crypta-ac-ok` on `tun13` after fresh confirmation. The consolidated installed Sign In runner passed all seven checks in one coordinated arm64 Tessera session with shared-cache artifacts. Duplicate requests kept one queue entry and did not extend its deadline. Reload, replacement, requeue and cancellation left stale POSTs at zero. Fresh confirmation sent one POST and returned `crypta-ac-ok` through ocserv on `tun18`. Evidence is at `/private/tmp/atrium-nav-vpn-batch1.6u32Fa/evidence-verified`. Physical YubiKey registration and sign-in passed from installed Navigator on arm64 Tessera. The JIT-off Ladybird worker ran in Capsicum. Sigillum used its jailed USB driver and trusted terminal PIN client. The local provider verified the new key's signature and Arca redeemed PKCE.

### 10.2 Video (I4)

[I4 installed playback and chapters](#i4-video-prototype) passed on arm64 Tessera. Installed Navigator started the JIT-off Ladybird island and fetched the external JS and VP9 clip. Three chapter presses moved the paused native player to blue at 1.5 s, red at 0.5 s and blue at 1.5 s. [Real YouTube host controls](#youtube-provider-playback-control) played with the watch origin (423 frames) and public Navigator app ID (417). No referrer produced error 153 and zero frames. Installed JIT-off Ladybird on Tessera fetched the real embed and reported the YouTube origin after two trusted Starts (144,490 and 144,511 bytes). Zero provider connections before Start or on cancellation. Fixed the LibWeb fetch executor initialization. The startup gate now delivers two styles and three scripts via one bounded worker. Ladybird created the player with 67 API methods, but its island stayed black. Restricted Chrome displayed the Play button. In a follow-up, reporting the exact WebM Opus MIME alongside VP9 made the provider create its Play button in jailed Ladybird. This was a diagnostic capability override. Installed JIT-off Ladybird passed both VP9/Opus buffer orders in Capsicum on arm64 Tessera. Video and audio buffered to 2.000 and 1.835 s independently. After the 0.5 s seek, both runs advanced from red to blue and emitted 440 Hz in Lyra’s device mix. Fixed LibWeb native readiness and premature player end before audio drain. A horizontally repeated gradient now uses one native fill. The isolated ramp passed pixel checks on Tessera.

### 10.3 Long playback, frame accounting, I5 hardening and the Start

Installed JIT-off Ladybird on arm64 Tessera played the public YouTube demo to 58.633 s without input. The 60 s chapter fetched a new range and decoded at 64.900 s. Opening returned to 5 s and refilled without skipping across the gap. Lyra captured audio in all three cases. The restricted courier renewed after 12 requests and delivered 18 media replies. Pacing native submissions and separating headless video from PNG capture yielded 515, 521 and 527 landed frames. Median landing errors against Lyra were +1.6, +2.3 and +1.4 ms. The guest mix and both chapter seeks passed after renewal. The full 22 min 24 s demo played uninterrupted to 1,343.567 s with 453 media replies, 39,445,202 bytes and 37 renewals. The guest HDA/Lyra mix remained audible in the final minute. The provider ended on the continuous run. After wiring native drain into LibWeb, a fresh final-chapter replay also delivered one DOM ended event. The installed DOM frame-accounting gate passed in JIT-off Ladybird on arm64 Tessera. After the 1,325 s seek the final 983 DOM frames and four drops matched 979 Fresco presentations and four drops. Of 1,011 decoded frames, 28 were discarded across the seek before submission. A fresh engine reset both counters to zero. All 24 host checks passed. The private guest shut down cleanly and port 24223 was released. The multi-leaf mask now composites in Fresco. On Tessera all 14,400 sampled pixels matched, and jailed Ladybird displayed the provider error message and masked gradient after Play. [Error-screen gate and evidence](#error-screen-mask). I5 installed-worker hardening passed on Tessera. The symbol audit matched the installed binary, both live workers passed mapping and rctl checks, and after worker loss the fresh Start decoded red and sought to blue. The ordinary Start median was 3,176 ms across three headless observations. The jailed offline fork gate passed all 18 repeat editor children. Immutable pinned faces eliminated ten inherited descriptors. Median fork to first native scene was 12.124 ms with child font setup and 11.884 ms with template font setup. The event loop opens two wake-pipe descriptors and stays child-local. The installed Start now prints a `start-trace` line per hop in navigatord, limend, portcullisd, the one-shot lane and jaild, on one monotonic clock, and `scripts/navigator-i5-start-trace.sh` merges them ([Start trace](#i5-start-trace)). The trace ran on arm64 Tessera with main's four daemons: the press-to-fork daemon path is 16 to 29 ms, but the engine's first request came 2.0 s after fork because limend waits 2 s for the absent audio-attribution socket. With choragusd answering, the gap fell to 40 to 56 ms, so the 3,176 ms baseline includes that wait. Every press then failed the island, because main's navigatord now expects the Opus fetch the I5 fixture and worker never make ([results](#results-on-arm64-tessera-2026-11-24)). limend now asks for the audio ticket off its state lock, so a missing or slow choragusd no longer delays the island (host test; the player reopens sound when a late ticket arrives), and a navigatord test pins the four video fetches to the engine's fixture sequence and page ([follow-up](#follow-up-the-audio-wait-and-the-opus-fetch)). Main's sources already make the Opus fetch; only the audited worker binary was older. Shankar chose D7, a standby pool: limend keeps one warmed engine per Navigator uid and hands it the next island, off unless `LIMEN_ISLAND_STANDBY=1` ([warm start](#i5-warm-start-how-a-warmed-engine-gets-its-own-jail-d7-accepted)). The rerun from main `58f533c1b` on arm64 Tessera passed the cold path: the rebuilt, audited engine (`6b2b35bb…`) made all four fetches, the 2 s audio wait is gone (first request 60 to 170 ms after the press with or without choragusd), and press to red fell from 3,176 ms to a median of 1,697 ms without choragusd and 1,764 ms with it. navigator-fetchd must come from main too, since the old one refused the Opus fetch. The standby worked: later presses took the waiting standby, the worker check passed on an assigned standby, and a waiting standby held about 69 MiB. It did not speed the Start: first request stayed 41 to 172 ms and press to red 1,734 ms (median of four), because navigatord asks Limen for the embedded player before serving fetches and about 1.4 s lies after the engine's start report ([rerun results](#results-on-arm64-tessera-2026-11-24-rerun-from-main)). The trace now runs to the player's first decoded frame (player launch, each fetch upstream and delivered, MSE appends, Play, first frame), the player is launched beside the engine instead of before its first fetch, and the next standby waits 3 s after the island ([follow-up](#follow-up-tracing-past-the-engines-start-and-two-overlaps)); host tests pass. The traced rerun on arm64 Tessera (main `fe34b869c`) shows where the 1.7 s goes ([results](#results-on-arm64-tessera-2026-11-24-trace-to-the-first-frame)): about 700 ms is the JIT-off engine's own work between fetches (page script, which scans each clip with `findIndex`, and MediaSource setup), about 300 ms the four fetches (the first one 160 ms), about 240 ms appends and the first frame, and about 280 ms composition and capture. The player is now ready at 160 to 260 ms, off the press's path, and the 3 s standby refill no longer overlaps; warm and cold still reach red at about 1.74 s. A standby fed a byte without a descriptor exits within 101 ms, and so does one whose channel closes (a descriptor proceeds as the control). The clip scan is too short to matter (split at bytes 468 and 497); the gaps are about 200 ms each, which fits `drive_page` pumping the event loop once per 20 ms input poll, so it now drains ready work first ([follow-up](#follow-up-the-20-ms-input-poll-not-the-clip-scan); engine side, unbuilt in the cloud). Measured on arm64 Tessera with the rebuilt, audited engine (`1000beb6…`, main `d4a26635e`): the three ~200 ms gaps fell to 1 to 26 ms, and press to red dropped from about 1.74 s to 1,027 ms without choragusd, 1,062 ms with it and 962 ms warm, with the player's first frame at 630 to 690 ms ([results](#results-on-arm64-tessera-2026-11-24-drained-event-loop)). What is left is mostly the four upstream fetches (60 to 180 ms each) and about 330 ms from `player-ready` to the observed pixel, which includes composition and the headless capture. Next: decide whether that observation lag needs its own trace stage in frescod before tuning further. Draining the engine's ready work before its 20 ms input poll cut press to red from about 1.74 s to 1,027 ms cold and 962 ms warm ([drain results](#results-on-arm64-tessera-2026-11-24-drained-event-loop)); the standby now saves about 65 ms. Headless frescod now traces each composited frame and saved capture under `FRESCOD_START_TRACE=1`, to split the last 330 ms between composition and the 200 ms capture floor plus PNG encode ([follow-up](#follow-up-splitting-the-last-330-ms)). Measured on arm64 Tessera ([results](#results-on-arm64-tessera-2026-11-24-frescod-traced)): the 330 ms was the measurement (the 200 ms headless capture floor, 52 to 55 ms composition, the PNG and the SSH poll), so the gate now ends at frescod's `composited`: median press to composited 869 ms cold without choragusd, 872 ms with it, 855 ms warm, and about 730 to 770 ms without the headless floor. Main's portcullisd (launch registry check, `2bd8429`), frescod and navigatord ran the same Starts cleanly (red at 1,050, 881, 893 ms). The [I6 plan](#i6-plan-whole-pages-live-on-a-press-accepted) (whole pages live on a press) was accepted with D8 to D13 on 2026-11-24. I6.0 qualification is next: the fixture app and its server are in `test-assets/navigator-i6/`, and the [run steps](#i60-qualification-run) use the VPN sign-in window as the whole-page vehicle until I6.1. Next (Mac): the I6.0 run. Per-persona launch is settled by D7: each Navigator persona instance gets its own standby, launched as that persona's island uid. [Fork gate and evidence](#i5-fork-before-taint-gate). [I5 gate and evidence](#i5-installed-worker-hardening-on-tessera). Physical display cadence is pending hardware. [Frame accounting and evidence](#native-playback-quality). Fingerprint verification deferred.

### 10.4 Whole pages live (I6)

I6.0 ran on arm64 Tessera ([results](#results-on-arm64-tessera-2026-11-25-i60)). [I6.1](#i61-as-built-2026-11-25) was built and its [gate passed](#i61-gate-results-on-arm64-tessera-2026-11-25) after five fixes. The [first part of I6.2](#i62-as-built-2026-11-25) (links as trail entries that run live again at the same origin, and the engine's page size) passed gate steps 1 to 4 on arm64 Tessera (0f3e97bba). The second part (`pushState` as trail entries, Back and Forward among a run's entries) passed gate steps 5 to 7 under J2. [I6.3's first part](#i63-as-built-2026-11-25-first-part-the-jar) (the jar: cookies through Arca, script POSTs, form sign-ins) and its second part (storage) passed their gates. [I6.4](#i64-gate-results-on-arm64-tessera-2026-11-25) (downloads and refusals, file input through Scrinium, printing, keys and the clipboard) passed its gates on arm64 Tessera after four fixes. I6.5 is built and [its Mac check passed](#i65-as-built-2026-11-25-second-part-cross-origin-frames), with the frame-input fix (534c8262c).
