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
