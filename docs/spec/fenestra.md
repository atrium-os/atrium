# Fenestra: a remote compositor that receives the scene, not the pixels

Status: **accepted 2026-11-26** (Shankar: "agreed" to all §17 decisions as recommended, plus decision 16), nothing built. Shankar asked for "a remote compositor, which
can display the same scenegraph coming through the network", with research into what goes
wrong with RDP, VNC and the rest, and whether we can get low latency and high quality remote
viewing. Decisions are in §17. Reviewed adversarially by the review thread on 2026-11-26; its
findings are folded in (§21). Owner: a new Fenestra thread (spec), the Fresco thread
(frescod and renderer changes), Forum (the viewer's window and the host's indicator).

*Fenestra* is Latin for a window, the opening in a wall you look through into another room.
Names: `fenestrad` (the host's exporter), `fenestra-net` (the viewer's network jail),
`fenestra` (the CLI and the viewer app).

Builds on: [fresco-f4-split.md](fresco-f4-split.md) (broker and per-client renderer jails),
[atrium-scenegraph-hardening.md](atrium-scenegraph-hardening.md) (H1 to H10, H9 in
particular), [aqueduct-remote.md](aqueduct-remote.md) (SSH handoff, session capabilities),
[aqueduct.md](aqueduct.md) §6.6 and §7.2 (possession ledgers, tunnel do not embed),
[resolution-independence.md](resolution-independence.md) (points and per-monitor scale),
[fresco-video.md](fresco-video.md) (video layers), [fresco-surfaces.md](fresco-surfaces.md)
(external GPU surfaces), [fresco-scene-groups.md](fresco-scene-groups.md) (groups, SCROLL),
[fresco-recovery.md](fresco-recovery.md) (client replay), [stoa.md](stoa.md) §3 (state sync,
predictive echo), [tabula.md](tabula.md) (clipboard and activation tokens),
[concursus.md](concursus.md) (NAT traversal), [background-apps.md](background-apps.md)
(`listen` grant). It replaces the remote-desktop sketch in
[../subsystems/transport.md](../subsystems/transport.md), which predates the split and had
apps talk to a remote frescod directly (§3.1 says why that shape is wrong now).

## 0. The idea in one paragraph

Every remote desktop protocol in wide use today renders on the machine where the apps run and
ships pictures of the result: compressed bitmaps (VNC, RDP's older paths) or a video stream
(RDP's AVC modes, Parsec, Moonlight, Chrome Remote Desktop). Fresco already has what those
protocols lack: a retained, validated, content-addressed description of every window. Fenestra
sends that description to the viewer and the viewer draws it with the same renderer the host
uses, at the viewer's own resolution and refresh rate. Text is never compressed, a still screen
costs nothing, scrolling and animation run on the viewer's clock, and the host needs no GPU.
Where there is no scene to send (decoded video, a game's GPU surface, a window that asked for
raw pixels), Fenestra falls back to a video or lossless codec **for that region only**, and it
knows exactly which regions those are because the scene says so, instead of guessing from
pixels the way RDP and VNC must.

## 1. What people mean by "remote", and what v1 covers

| Shape | Example | In v1 |
|---|---|---|
| **Whole desktop** | Sit at the laptop, use the workstation's whole session (its bar, dock, windows) in one window or full screen. | Yes |
| **App or window** ("rootless") | Run Artifex on the build box; its windows sit among the laptop's own windows. | Yes, after whole desktop (R4) |
| **Shared view** | Show one window into a meeting, or let a colleague watch; view only. | Yes, as a restricted mint (aqueduct-remote.md §4) |
| **Headless host** | A server or cloud VM with no monitor and no GPU runs a session that only remote viewers see. | Yes, and it is the cheapest case (§4.4) |
| **Atrium viewing a foreign desktop** (Windows, Linux, macOS) | | No. That is an RDP or VNC client app, a separate project. |
| **A portable viewer** on macOS and Linux viewing an Atrium host | Shankar's Mac viewing the Atrium VM; a Linux cloud container viewing a cloud VM | **Yes, first** (§14, decided by Shankar 2026-11-26): it is how Fenestra is developed and tested while Atrium runs in a VM. Windows and a browser viewer later. |

## 2. What goes wrong with existing remote viewers

This is the research Shankar asked for. Each row is a known, documented failure; §16 maps each
one to the part of this design that avoids it.

### 2.1 Pixel protocols: VNC (RFB)

- **Everything is pixels, found by diffing.** The server compares framebuffers to find what
  changed, then picks an encoding (Raw, Hextile, ZRLE, Tight with JPEG). It cannot know that a
  region is text, so it either sends text losslessly (expensive) or through JPEG (smeared
  edges, coloured fringes around glyphs).
- **Pull model, one frame in flight.** The client asks for an update and the server answers
  with the current state. On a long link the screen updates at best once per round trip, so a
  100 ms link caps smooth motion near 10 updates a second.
- **Video and fast motion** are re-detected and re-encoded every frame; Tight's JPEG costs CPU
  on the server and still looks poor.
- **Security.** The classic VNC authentication is a DES challenge with an 8-character password
  limit and no transport encryption; encryption is bolted on (TLS or SSH tunnels, vendor
  extensions) and varies by implementation. Kaspersky's 2019 audit of LibVNC, UltraVNC, TightVNC
  and TurboVNC found 37 CVEs, many memory corruption, several reachable from a **malicious
  server against the client**: the viewer parses hostile bytes in an unconfined process.

### 2.2 RDP

- **Started as drawing orders, ended as video.** RDP's early "GDI orders" remoted drawing
  commands (this is the idea Fenestra returns to). They died because Windows apps moved to
  DirectX, WPF and browsers that draw into bitmaps the remoting layer cannot see into. Modern
  RDP's graphics pipeline (MS-RDPEGFX) sends surfaces with codecs: ClearCodec and a "text
  optimised" codec for text-like regions, the RemoteFX progressive codec, and H.264 in AVC420
  and AVC444 modes. AVC444 exists because 4:2:0 chroma subsampling blurs coloured text and
  sub-pixel anti-aliasing; it sends a second stream to recover the chroma.
- **Content classification is heuristic.** The server guesses which screen regions are text,
  which are images and which are video. Wrong guesses show as text smearing for a moment after
  scrolling (it was classed as motion), or as blocky refinement of images (progressive codec).
- **Latency stacks up:** capture, classify, encode, send, decode, compose. Hardware encoders
  help, but a WAN-quality encode still adds frames of delay; SPICE's developers measured at
  least 100 ms added by streaming at WAN bandwidth in their remote Virgl work.
- **Resolution and DPI** are the session's, not the viewer's. A 4K laptop viewing a 1080p
  session scales a bitmap; a session resized to the viewer re-lays out the whole desktop, and
  per-monitor DPI across a multi-monitor client is a long-standing pain.
- **Security.** RDP is a large pre-authentication attack surface: BlueKeep (CVE-2019-0708)
  was a wormable, pre-authentication use-after-free in a virtual channel. Check Point's
  "Reverse RDP" work found 25 flaws in mstsc, FreeRDP and rdesktop where a **malicious server
  attacks the client**, including a clipboard path traversal (CVE-2019-0887). The lesson
  matches VNC's: the viewer is attacked through every channel the server can write to
  (graphics, clipboard, file transfer, device redirection), and those parsers ran unconfined.

### 2.3 SPICE (QEMU/KVM)

- SPICE was designed to remote drawing commands (QXL) and let the client render, the same bet
  as Fenestra. It suffered from the RDP problem: guests stopped drawing through 2D commands, so
  QXL saw ever more bitmaps and switched to streaming them as video. Red Hat closed a bug about
  lagging cursor and text over high-latency WAN links as WONTFIX, planning to move to Virgl
  instead; remote Virgl then needed video streaming and its latency cost.
- Lesson: **a command protocol only works if the apps actually speak it.** On Atrium they do:
  Pergola, the Navigator, Officina and Forum emit Fresco scenes, and frescod refuses unknown
  vocabularies (H3).

### 2.4 X11 forwarding and NX

- X11 was network transparent at the protocol level, and that was its strength. It was killed
  by **round trips**: many requests wait for replies, and modern toolkits draw client-side into
  images (Cairo, Skia) and push them as pixmaps, so over a WAN an X app is both chatty and
  bitmap-heavy. NX recovered usability with caching and round-trip suppression in a proxy; LBX
  tried the same and was abandoned.
- Lessons: **no request on the drawing path may wait for the server**, and the protocol must
  carry the app's real content (glyphs, paths), not its rasterised output.

### 2.5 Wayland remoting: Waypipe

- Waypipe proxies the Wayland protocol and copies buffer contents (shm diffs, or video-encoded
  DMA-BUFs). Wayland has few round trips, so it beats X11 forwarding, but it is still **pixel
  shipping per window**: every client renders locally into buffers. Its video mode only works
  for some buffer formats, keeps one stream per buffer (so windows flicker as surfaces rotate
  between buffers), and hardware encode is described in its own man page as finicky.
- Lesson: per-window forwarding (rootless) is the nicer experience, but forwarding buffers
  inherits all of the pixel problems.

### 2.6 Game streaming: Parsec, NVIDIA GameStream / Moonlight / Sunshine, Steam Remote Play

- These get the lowest latency of anything pixel-based: hardware capture and encode on the GPU,
  UDP with custom congestion control and forward error correction, **no playout buffer** (show
  a frame the moment it decodes), low-delay encoder settings (no B-frames, intra refresh instead
  of keyframes), and the cursor drawn on the client. Parsec built its own UDP protocol (BUD)
  after finding nothing else gave TCP's reliability at UDP's latency.
- Costs: a GPU with a hardware encoder on the host, steady high bitrate (tens of Mbit/s for
  1080p60), text that is only as sharp as the bitrate allows, and Wi-Fi or mobile jitter that
  becomes visible stutter because there is no buffer.
- Lesson for Fenestra's **fallback** path: when we must send pixels, do it the way these
  products do (low-delay encoder, no playout buffer, region-limited, UDP datagrams), not the
  way VNC does.

### 2.7 Terminal-shaped lessons: mosh

- mosh (USENIX ATC 2012) synchronises **state, not a byte stream**: the server sends the
  difference between the state the client acknowledged and the current one, so a backlog never
  builds up and old frames are never replayed. It also predicts the effect of keystrokes and
  shows them immediately (70% of keystrokes, median under 5 ms versus 503 ms for SSH on a 3G
  link), correcting a wrong guess within one round trip. Stoa already does both (stoa.md §3.3,
  §3.4).
- Lesson: **sync state with acks and skip stale intermediate states**, and hide round trips with
  local prediction where a wrong guess is cheap to undo.

### 2.8 The common failures, summarised

1. Text is degraded because the protocol only knows pixels (VNC, RDP, game streams).
2. Latency is the sum of capture, encode, network, decode and compose, plus queues.
3. Bandwidth scales with screen area times frame rate, not with what changed.
4. The viewer re-scales pictures rendered for a different screen (DPI, HDR, refresh rate).
5. Command protocols died because apps stopped speaking them (RDP orders, SPICE QXL, X11).
6. Round trips on the drawing path (X11).
7. Queues of stale frames under congestion (TCP-based protocols).
8. Viewers parse hostile streams in unconfined processes, through many side channels
   (clipboard, files, devices), and authentication is weak or bolted on.

## 3. Architecture

### 3.1 Where the scene is taken from

transport.md's sketch had each app connect to a remote frescod. That no longer fits:

- Apps are jailed and most have **no network**; giving every app a socket to a remote host
  would break the network model and multiply the attack surface.
- frescod's split (F4b) means a client's scene lives in its **renderer jail**, already parsed
  and validated, and the broker only sees packed frames. The validated, canonical scene is the
  thing worth sending, and it exists in exactly one place per client.
- A remote session must survive the viewer going away. Apps keep running on the host; only
  the view is remote.

So Fenestra adds **remote outputs** to the host's frescod. A viewer that connects appears to
the host as one or more monitors (with the viewer's sizes, scales, refresh rates and HDR
capabilities). Forum lays windows out on them as on any monitor. Instead of rastering windows
on those outputs into scanout, each renderer jail emits its window's scene as a canonical
**scene stream**, and `fenestrad` carries it to the viewer.

```
 HOST (apps run here)                                          VIEWER (person sits here)
 ┌──────────────┐  fresco.sock  ┌─────────────────────┐
 │ app (jailed) ├──────────────►│ fresco-renderer      │ validates (H1–H6), holds the scene
 └──────────────┘               │ (jail, no net, no fs)│
                                │  emits canonical     │
                                │  scene stream ───────┼──┐ memfd ring, fixed records
                                └─────────────────────┘  │
 ┌─────────────────────────────┐ records                  ▼
 │ frescod broker (no network) │◄────────────────┐ ┌──────────────────┐ QUIC over UDP   ┌────────────────────┐
 │ windows, z, focus, outputs, │  input, acks,   └─┤ fenestrad +      ├────────────────►│ fenestra-net (jail,│
 │ input routing, chrome       │  output changes   │ fenestra-session │◄────────────────┤ own uid, connect   │
 └─────────────────────────────┘                   │ (jail/session)   │  input, acks    │ only)              │
                                                   └──────────────────┘                 └─────────┬──────────┘
                                                                                                  │ per window
                                                                                                  ▼
                                                                                ┌──────────────────────────┐
                                                                                │ fresco-renderer (jail,   │
                                                                                │ same bundle as the host's│
                                                                                │ validates again, H9)     │
                                                                                └─────────┬────────────────┘
                                                                                          │ packed frames (slots)
                                                                                          ▼
                                                                                ┌──────────────────────────┐
                                                                                │ viewer's frescod broker  │
                                                                                │ composes with local      │
                                                                                │ windows and chrome       │
                                                                                └──────────────────────────┘
```

### 3.2 The host side

- **frescod (broker)** gains remote outputs: `OUTPUT_ADD/REMOVE` records from `fenestrad`
  carrying the viewer's monitor geometry in points, scale, refresh range, HDR capabilities and
  colour space. It routes input arriving from `fenestrad` exactly as it routes Tactus input
  (F4c, F4c′), and it tells each renderer which of its windows are on remote outputs.
  **frescod gets no network**, and it never reads scene bytes from the network.
- **fresco-renderer** gains a scene-stream writer: for each window on a remote output it keeps
  a per-viewer acked version and writes the canonical difference (§5.2) into a sealed memfd
  ring that the broker hands to `fenestrad`. The writer serialises from the renderer's own
  validated structures, never by copying the client's bytes, so only canonical records leave
  the jail.
- **fenestrad** is a new jailed daemon, its own uid (`_fenestra`), with **no network** and no
  filesystem but its socket directory. It takes mints from `aqueduct-shell` (§11.2), keeps the
  session table, starts and ends the per-session network jails, runs the fallback encoders in
  child jails (§7.4), and passes input and acks to frescod. It reads only fixed-size records,
  from frescod, from renderers' rings and from session jails.
- **fenestra-session** is one jail per session, started by `fenestrad` through portcullisd for a
  successful mint, its own uid per session, no filesystem, a `listen` grant on **one** UDP port
  from a small range (background-apps.md: local network by default, wider only by the person's
  choice). It holds that one session's keys and nothing else, terminates QUIC, checks the
  handshake (§11.2), schedules what to send (§8), and turns the viewer's input and acks into
  fixed-size records for `fenestrad`. There is **no standing listener**: the port exists only
  while the session does (§11.9). A bug in quinn or rustls therefore gives an attacker one
  session's jail, which holds no other session's keys, cannot mint, and can inject only the
  bounded input records that session's capabilities allow.

### 3.3 The viewer side

- **fenestra-net** (jail, own uid, outbound UDP to the one host only, no filesystem) holds the
  QUIC connection and demultiplexes streams. It does not parse scene records; it hands each
  window's stream to that window's renderer over a socket pair.
- **fresco-renderer**, the *same signed bundle* the host runs, one per remote client (as on the
  host), in the same empty jail. It runs the full validator on the incoming stream (H9: an
  authenticated host may be compromised), holds the scene, rasterises at the viewer's scale with
  the viewer's GPU or the compute renderer, and hands packed frames to the viewer's frescod like
  any local renderer. A malicious host can therefore exploit, at worst, a zero-capability
  renderer jail: no network, no files, no other windows (contrast §2.1, §2.2's client-side
  CVEs).
- **The viewer's frescod** composes remote windows among local ones (rootless) or inside one
  local window (whole desktop). It draws **its own trusted marking** around remote content
  (§11.5) and owns the cursor.
- **The Fenestra app** (Pergola, unprivileged) is the connect dialog, the session list, the
  toolbar (full screen, view only, disconnect, stats) and the settings. Every one of those
  actions is also a `fenestra` CLI command (§15).

### 3.4 Two presentation modes

- **Whole desktop.** The viewer's frescod gives the session one local window (or full screen)
  whose size is the remote output. The host's Forum (bar, dock, wallpaper, window manager) runs
  on the host and its layout arrives as window placements in the stream. Moving a remote window
  is a round trip to the host's WM; the cursor, scrolling (§6.3) and animations are not.
- **Rootless.** The host's Forum does not manage remote outputs; instead each remote top-level
  window becomes a local window managed by the viewer's Forum. Moves, resizes (§6.4), focus
  and stacking happen locally at local latency; the host learns the size and focus so the app
  can lay out. Pop-ups and dialogs keep their parent relation (fresco-popups.md). The host's
  bar and dock are not shown; the viewer's dock shows remote apps with a host badge.

## 4. What the host has to change, and what it saves

### 4.1 Remote outputs are monitors

Treating a viewer as monitors reuses everything resolution-independence.md built: an app's
scene is emitted in the device pixels of the monitor its window lives on, so a window on a
viewer's 2x Retina-class panel is drawn by the app at 2x. **Text is shaped and rasterised by
the app for the viewer's actual pixel density** (glyph atlases at the viewer's scale), so it
is as sharp as if the app ran locally. No other remote protocol can do this; they all render at
the session's resolution and scale a picture.

A viewer with two monitors at different scales gives two remote outputs; moving a window
between them re-emits at the new scale (resolution-independence.md §7), exactly as locally.

### 4.2 The host keeps its own screen

A host with a local seat can keep showing its own session while viewed (decision 3): its own
monitors stay outputs, the remote ones are added. Whole-desktop mirroring of the *same* layout
uses the existing mirror machinery (`OP_WM_MIRROR_OUTPUT`) with the remote output as the mirror
target, re-emitted at the viewer's scale.

### 4.3 Version skew and missing features

The scene stream carries the wire version and a feature set agreed at connect. If the viewer
lacks a node kind (an older viewer, or a node a future host added), the host's renderer rasters
**that window** locally and it goes as pixels (§7.3), with the stats overlay saying why. A new
feature never breaks an old viewer; it only costs bandwidth until the viewer is updated.

### 4.4 Headless hosts need no GPU

In scene mode nothing on the host rasters a window that is only on remote outputs: the
renderer validates and keeps the scene and the viewer draws it. A cloud VM or a headless box
serves full-quality sessions with no GPU and a little CPU, and it rasters only fallback regions
(pixel-mode windows, GPU surfaces) when those exist. This is the opposite of RDP and game
streaming, where the host pays for rendering and encoding every frame.

## 5. The scene stream

### 5.1 What travels

Per window, in the vocabulary frescod already accepts (wire-format.md, fresco-protocol), after
validation and canonicalisation by the host's renderer:

| Class | Content | Size, typical |
|---|---|---|
| Window | create, destroy, title, role, parent, placement (whole desktop), size, scale | tens of bytes |
| Nodes | keyed node set/clear: rects, paths, fills (`ATRIUM_CORE_FILL`), glyph runs (`ATRIUM_TEXT_GLYPH_RUN`), textures, groups and places (`ATRIUM_CORE_GROUP`, `PLACE`) | ~32 bytes a glyph, tens to hundreds of bytes per other node |
| Slots | texture slots by CAS hash, and region updates (`OP_SLOT_UPDATE_REGION`), including glyph atlas pages and their growth | once per blob, then a reference |
| Animations | `OP_ANIMATION_START/CANCEL` tracks | one record per animation, not per frame |
| Video | layer create, place, colour (fresco-video.md §3), and frames by the path in §7.1 | §7 |
| Surfaces | external surface nodes; their pixels by §7.2 | §7 |
| Frame marks | `SCENE_FRAME_BEGIN/END`, so the viewer never shows half a frame | bytes |

transport.md's estimates still hold in kind: a session's first frame costs its fonts' atlases,
icons and wallpaper (a few MB, once, and cached across sessions, §5.4); typing costs a glyph
instance and a caret move (well under 1 KB a keystroke); an idle screen costs nothing. These
are estimates to be measured in R2, not results.

### 5.2 State sync, not a replayed log

Fenestra synchronises state the way mosh and Stoa do (§2.7):

- Each window's scene has a version. The viewer acks the versions it has fully applied (end of
  frame). The host's renderer keeps, per viewer, the acked version and sends the **difference
  between the acked scene and the current scene**: nodes set or cleared since, slots referenced
  that the viewer has not been sent.
- If the network is slow, intermediate states are **skipped**, never queued: when the next
  send opportunity comes, the difference is computed from the latest state. Ten fast edits to a
  node during a congested second become one record. Nothing ages in a queue (failure 7).
- The renderer keeps a per-node "changed at version" number so the difference is a walk over
  changed nodes, not a full compare. Memory: one version number per node per window and one
  acked version per viewer.
- A lost or corrupt datagram or a gap is not special: the viewer simply has not acked, so the
  next difference covers it. A viewer that cannot keep up (or reconnects) gets a full state
  from version 0, which is fresco-recovery.md's replay seen from the other side.

### 5.3 Ordering and atomicity

Records for one window travel on one QUIC stream, in frame order; a frame is applied on the
viewer only when its END arrives, so the viewer never draws a torn frame. Windows are
independent streams, so a large image upload in one window never delays typing in another
(QUIC has no head-of-line blocking across streams; TCP does).

### 5.4 Content-addressed blobs and the viewer's cache

- Blobs (images, glyph atlas pages, fill geometry blocks) are sent once and referenced by
  SHA-256 hash. The viewer verifies every hash on arrival (H2) and refuses a mismatch, and
  re-hashes any blob it reads back from its disk cache before use.
- **Glyph atlases are textures, and travel as textures.** As built, a `GlyphRun` node points at
  an R8 atlas the *app* rasterised and uploaded, grown by texture region updates
  (`frescod/src/render_backend.rs`); the renderer never receives fonts. So the stream carries
  atlas pages and their region updates like any other texture, and the viewer needs no font
  files. The cost is one atlas page per font and size the app uses (sent once, then only new
  glyphs' regions); region updates compress well with zstd.
- **Possession, not global dedup** (aqueduct.md §6.6): the viewer keeps a separate cache per
  (host, persona), where the host is identified by its **verified SSH host key** (the key sshd
  proved during the mint, not a name or address), and tells the host only which of *that
  host's* blobs it holds. A host cannot probe for blobs another host sent, and the viewer never
  says "I have H" for a hash it did not receive from that host. There is no cross-host
  exception.
- The cache holds pictures of the person's documents, so on disk it is **personal data at
  rest** and lives in the viewing persona's key group (AGENTS.md at-rest rule), in the Fenestra
  app's overlay, encrypted with the persona. A viewer without that directory (a kiosk, a guest)
  caches in memory only. The cache is bounded (default 1 GiB, Curia setting) and evicts least
  recently used. `fenestra cache clear [--host H]` empties it.

### 5.5 Images, progressively and by visibility

- Large textures are sent as a small preview first (a reduced copy the host makes in the
  renderer jail, carried in the same slot with a "preview" flag), then the full blob. The viewer
  shows the preview at once, then swaps. RDP's progressive codec does this for pixels it had to
  encode; here it applies only to images the app supplied, and the final image is exact.
- Blobs are queued by visibility: on screen first, then near the viewport (the band API's
  readahead), then off screen. A blob that stops being referenced before it is sent is dropped
  from the queue.

### 5.6 As built (R1, 2026-11-26)

`fresco-render-core/src/stream.rs`. Host tests: `tests/stream.rs`, `tests/stream_fuzz.rs`.

- **Records.** Each record is `len u32 | tag u8 | body`. There are four tags:
  - `BEGIN` carries the epoch, base, version and window size.
  - `OP` carries a client op's postcard payload.
  - `BLOB` carries raw bytes, hashed on arrival.
  - `END` carries the version.

  Only five ops may travel: `SLOT_SET`, `SLOT_CLEAR`, `SLOT_UPDATE_REGION`,
  `SCENE_NODE_SET` and `SCENE_NODE_CLEAR`. Any other op ends the stream. The reader applies a
  frame only at its `END`, through its own `EnvelopeFrontend::dispatch` (H9), and then packs
  with `pack`.
- **Change detection by scan, not hooks.** `Writer::update` runs after each frame. It encodes
  every node's canonical ops (a group as `group_now`, so a running animation travels sampled;
  a glyph run with its turn; a leaf with its place) and compares a 64-bit hash with the last
  one. So "changed at version" covers every mutation path in the frontend without touching
  them. The cost is one encode per node per frame, for windows on remote outputs only.
- **Slot changes.** `SceneState` remembers each slot's last whole upload and its last 64 region
  uploads (`changes_since`). A viewer that holds an earlier generation gets one
  `SLOT_UPDATE_REGION` with the bounding box of the regions since. Otherwise it gets the
  whole texture as a `BLOB` plus `SLOT_SET`, keyed by the SHA-256 of its current bytes.
- **Order within a diff:**
  1. Slots, because glyph runs check that their colour twin is bound.
  2. Clears of removed nodes.
  3. Changed leaves, each cleared and set afresh, so a place it no longer has goes with the
     old copy.
  4. Groups, parents first.
  5. Leaves.
  6. Places.
  7. Slot clears.
- **No cascades on the viewer.** A client's clear of a group takes its whole subtree. In a
  stream, the reader first lifts the group's children to the root, so the clear takes only
  the group. Without this, a subtree that moved on the host would vanish from the viewer: the
  diff names only what changed, and that subtree's deeper nodes had not.

  With the lift, every node that differs between the viewer's state (any version from the
  diff's base onwards) and the host's has changed since the base. Applying the diff in the
  order above therefore yields the host's state.
- **Lost acks.** A frame to the version the viewer already holds is acked again without
  being applied. The viewer acks `(epoch, version)`.
- **Whole states.** The writer sends a whole state when the viewer holds:
  - nothing,
  - another epoch, or
  - a version older than the last forgotten removal (`MAX_GONE` 4096).

  An op refused by the viewer's validator or budgets answers `Ack::Whole`, and frames until
  the whole state arrives are dropped. A malformed record, an unknown tag, a disallowed op, a
  frame over `MAX_FRAME_BYTES` (256 MiB) or `MAX_FRAME_RECORDS`, a record over `MAX_RECORD`,
  or a frame that goes backwards ends the stream with an error.
- **Tests (Linux host):**
  - Random client ops (rects, paths, fills, textures, glyph runs and turns, groups, places,
    clears, slot binds, region updates and clears, animations) for 40 seeds x 150 frames,
    with two viewers. One viewer misses 20% of sends, 30% of acks are dropped, and the bytes
    arrive in random pieces. Every caught-up frame packs to the same bytes as the host's
    (header, ops, textures, texels, LUT). 1000 seeds pass with `FENESTRA_STREAM_SEEDS=1000`.
  - Mutation checks: each of these makes the test fail:
    - dropping leaf clears,
    - dropping places,
    - dropping turns,
    - ignoring animations,
    - unsorted groups,
    - losing the newest region.

    The cascade bug above was found this way.
  - Fuzzer: real streams with records dropped, repeated, swapped or forged, lengths that lie,
    bit flips, truncation, and random bytes. 200 000 iterations ran with no panic, and the
    reader stayed within its budgets and bounds. A blob changed in flight is never drawn, and
    an endless frame is cut off at `MAX_FRAME_BYTES`.
- **Recordings.** A frescod built with the `fenestra-record` feature and run with
  `FRESCO_RECORD=<new file>` records what its frontend takes, in the order it took it, under
  the frontend's lock (`frescod/src/fenrec.rs`, format in `fresco-render-core/src/record.rs`):
  client ops, blobs, animation ticks and draws. No other build can record.

  `fenestra-stream-replay` plays a recording through the stream to two viewers per window.
  `scripts/fenestra-record-vm.sh` records the three sessions in a VM, and
  `scripts/fenestra-stream-check.sh` is the R1 gate (the tests above plus the three
  replays).
- **Not yet:**
  - The writer is not wired into `fresco-renderer`.
  - Window 0 (the shared screen) is not streamed.
  - A slot bound on the host whose blob missed the CAS is not sent, so a run's colour mark can
    differ there.

## 6. Latency

### 6.1 The budget

For an input that changes the screen (a click, a key):

| Step | Pixel streaming (RDP, Parsec) | Fenestra scene mode |
|---|---|---|
| Input to host | RTT/2 | RTT/2 |
| App reacts and redraws | app time | app time (emit only, no host raster) |
| Host capture and encode | 2 to 10 ms with hardware, more in software | none: validate and diff the changed nodes, microseconds to a millisecond |
| Network | RTT/2 plus serialisation of a video frame | RTT/2 plus a few hundred bytes |
| Viewer decode | 1 to 5 ms (hardware) | none |
| Viewer raster | none | the changed region only (incremental, fresco-incremental drawing), sub-millisecond to a few ms |
| Compose and scan out | next vblank | next vblank (VRR where available) |

Ranges for pixel streaming are typical published figures, not our measurements; ours are to be
measured in R2 and R6 on real links (TCG timings are never product numbers). Until then the
Fenestra column is **structural, not a number to quote**: Fenestra removes the encode and decode stages and their buffers, and it sends
small payloads, so on a given link its click-to-photon time is RTT plus app time plus about one
frame.

### 6.2 What never waits for the network

These happen on the viewer with no round trip at all, so they feel local at any RTT:

- **The cursor.** Drawn by the viewer's frescod at the local pointer position; the host's
  cursor shape (and hotspot) arrives as a blob. Pointer motion is sent to the host coalesced
  to the viewer's refresh rate.
- **Animations.** Server-side animations (`OP_ANIMATION_START`, tracks on groups) run on the
  viewer's frame clock at its refresh rate; only the start and cancel travel. A 120 Hz viewer
  gets 120 Hz animation from a host that has no display.
- **Video pacing.** Video frames carry their presentation time and the viewer's frescod
  presents them against its own vblank (fresco-video.md §4), so playback is smooth even with
  network jitter, within the buffer the player chose.
- **Window management in rootless mode**: moving, raising, focusing, minimising.
- **Hover and pressed states on trusted chrome** the viewer draws itself (its own title bars in
  rootless mode).

### 6.3 Scrolling on the viewer (decision 4)

Scrolling is the round trip people notice most. Today a SCROLL group's offset is set by the
app (fresco-scene-groups.md §4). Fenestra proposes a **viewer-scrollable** flag on a SCROLL
group: the app declares the scrollable range and has sent content (via bands) beyond the
viewport; the viewer's frescod moves the offset itself on wheel and touchpad input, at once,
and tells the host the new offset, which forwards it to the app as an ordinary scroll event.
The app then fills newly exposed bands. If the viewer scrolls past what it has, it shows the
group's background (as a browser compositor shows checkerboard) until the band arrives.

This is how browsers keep scrolling smooth when their main thread is busy, and it makes remote
scrolling feel local. Pergola's ScrollView and the Navigator's document views are the users;
an app that does not set the flag behaves as today. The same flag helps locally (scrolling no
longer waits on a busy app), which is a reason to build it in frescod rather than in Fenestra.

### 6.4 Resizing in rootless mode

A local resize of a remote window cannot wait for the app. The viewer shows the last frame
anchored at the top-left and clipped or padded with the window's background while the app
re-lays out, then swaps in the new frame atomically, the way macOS handles slow apps. Groups
let the viewer stretch nothing: content is never scaled, only clipped.

### 6.5 Typing (decision 5)

Predictive echo (Stoa §3.4, mosh) works because a terminal's next state after a printable key
is very predictable. A GUI text field is predictable in the same way if the app tells the
viewer enough: Fenestra proposes an optional **edit hint** on a text field's group: caret
position, line box, font size and colour, a glyph atlas already on the viewer, and the field's
"plain insert" behaviour. With it, the viewer draws a typed glyph at the caret immediately,
underlined faintly as tentative (mosh's convention), and drops the prediction when the host's
frame for that keystroke arrives. A wrong guess (autocomplete, a shortcut, a rich-text rule)
lasts one round trip. Without the hint, typing costs a round trip, as in every other protocol.

This is the one place Fenestra puts behaviour into the scene, so v1 ships without it and R7
adds it for Pergola's single-line and plain-text fields only, measured against a recorded
typing corpus as mosh did.

### 6.6 Frame pacing and the host's clock

The host does not pace frames to any display (a remote output has no vblank on the host). Each
window's renderer sends when its app ends a frame and the transport has room (§8.2); the viewer
composes on its own vblank. Frame callbacks to the app (when to draw next) run at the
viewer's reported refresh rate, limited by how many of that window's frames are unacked: up to
the number that fit in one round trip at the current send rate (at least 2). So an app animating
by itself draws at the rate the viewer displays, not faster, and a long RTT does **not** cut it
to one frame per round trip; only a link that cannot carry the frames slows it.

## 7. Content that cannot travel as a scene

### 7.1 Video layers

A player puts decoded NV12 or P010 frames into a video layer (fresco-video.md). Two paths, in
order of preference:

1. **Compressed passthrough (decision 6).** When the source is a codec the viewer can decode
   and the player is not doing anything to the frames, the best remote video is the original
   bitstream: no re-encode, no quality loss, and a fraction of the bitrate of re-encoding. This
   needs a new video-layer mode in which the player hands frescod compressed access units
   instead of pictures (the player already has them: it decoded them). On a local output
   frescod would have to decode, which it must never do, so this mode is offered **only on
   remote outputs**, and the viewer decodes in a zero-capability decoder jail (the player's
   decoder worker bundle, atrium-player.md §4: dav1d, libvpx). Codecs: AV1, VP9, VP8 in v1
   (permissive decoders already in the tree). H.264 and HEVC streams go by path 2 unless the
   viewer has OpenH264 through Hospes (hospes.md) or a hardware decoder.
2. **Re-encode the layer.** The host's `fenestra-enc` child jail reads the layer's ring and
   encodes it at the layer's own size and rate, low-delay settings (no B-frames, periodic intra
   refresh instead of keyframes), sent as datagrams. Encoders: a hardware encoder when the GPU
   driver gets one (VCN encode, not built), otherwise SVT-AV1 or rav1e in realtime presets.
   The viewer feeds decoded frames into a local video layer, so presentation timing, colour
   conversion and HDR tone mapping (fresco-video.md §5) run on the viewer for its own monitor.

Either way the player's controls and subtitles are ordinary scene nodes drawn over the video,
so they stay sharp and respond instantly while the video beneath them is compressed.

**Protected frames** never leave the host: a protected layer is black with a "protected video
is not shown remotely" label on the viewer, the same rule as capture exclusion
(fresco-video.md §6).

### 7.2 External GPU surfaces (games, 3D viewports)

A surface's pixels exist only as a GPU image on the host (fresco-surfaces.md). Fenestra
copies the surface on the host GPU and encodes it like a video layer (path 2), at the surface's
size and its push or pull rate. This is the game-streaming case of §2.6 and gets their
techniques: no playout buffer, low-delay encode, datagrams with forward error correction,
bitrate from congestion control (§8.3). Because the surface is one node in the window's scene,
only its rect is encoded; the app's toolbar and panels around a 3D viewport stay scene nodes.
A host with no GPU has no external surfaces to send.

### 7.2a Rendering 3D on the viewer's GPU instead (Shankar's question, 2026-11-26)

Could the viewer run the 3D itself, so a Tier-3 app's frames are drawn by the viewer's GPU
rather than sent as pictures? There are two ways to do it, and they behave very differently.

**Way 1: remote the app's Vulkan commands (aqueduct-gpu over the network).** The app's
`atrium-vk-icd` already records whole frames and sends them as one `OP_GPU_SUBMIT_FRAME`
envelope; IDs are assigned locally, device properties come once in the handshake, and shaders
and immutable textures are CAS blobs (aqueduct-gpu.md §3.5, §4.4). Carillon already carries that
wire across a VM boundary to MoltenVK on the Mac. So the protocol is close to remotable as is:
a `fenestra-gpu` jail on the viewer runs an `aqueduct-gpu-host` session against the viewer's
GPU and draws the surface there.

What it buys: exact pixels at the viewer's resolution, no encoder, no GPU needed on the host
(a cloud box could run a 3D viewer app), and CAS-cached shaders and assets after the first run.

What breaks, and why it decides the default:

1. **Fences become network round trips.** A Vulkan app waits on a fence before it reuses a
   frame's buffers. If the fence signals on the viewer, the app can have at most its frames in
   flight per RTT: 3 frames in flight at 100 ms RTT caps it at 30 fps, at 200 ms at 15 fps.
   Pixel streaming has no such cap, because the host renders at full rate locally. The cap can
   be lifted only by signalling fences on the host when a frame is *sent*, which is correct only
   while the app never reads GPU results back.
2. **Readbacks stall.** Occlusion and timestamp queries, GPU compute results (physics,
   culling), host-visible buffers the GPU writes, screenshots: every one is a full RTT, and
   engines do several per frame. Many engines are built on them.
3. **Bandwidth moves to the worst place.** Per-frame uniform and vertex streaming, dynamic
   textures and persistently mapped memory need dirty tracking across the network (the problem
   that made Venus slow). Open-world texture streaming is gigabytes. First launch must upload
   the game's assets to the viewer: minutes to hours on a WAN, where a picture stream starts in
   a second.
4. **The viewer's GPU is the wrong GPU.** The usual reason to view a machine remotely for 3D
   is that the host has the strong GPU (the R9700 box) and the viewer has a laptop iGPU or a
   phone. Features and limits also differ by viewer: an app that chose features on one viewer
   gets device-lost when a different viewer attaches, or when the session moves back to the
   host's screen.
5. **Security.** The viewer's GPU driver would run command streams written by the host. Under
   aqueduct-gpu.md §11 (option A), shader validation is only advisory against a hostile
   producer; the real boundary is the GPU MMU, the broker writing packets and the hang
   watchdog. That is acceptable for apps the person installed locally; for any host they
   connect to, it puts a remote party one driver bug from the viewer's kernel. It would have to
   be opt-in per host, and only on a driver that passes `gpu-isolation.md`'s attestation gate.

**Way 2: retained 3D in the scene.** If the app describes its 3D as a retained scene (meshes,
materials, transforms, a camera: wire-format.md's `NODE_MESH`, `NODE_CAMERA`, the
`0x0C00` 3D range, which is sketched but not drawn by today's renderers), then 3D remotes like
2D: meshes and textures go once as CAS blobs, and a frame is a few changed transforms. The
viewer renders it at its own resolution and refresh, and it can **orbit, pan and zoom the camera
locally with no round trip**, which no pixel protocol can do. This fits model viewers, CAD and
scientific views, Artifex previews and web 3D content. It does not fit game engines, which
wire-format.md already places outside Fresco's scope.

**Answer: pictures stay the default for Vulkan surfaces, and viewer-side 3D comes through the
scene, not through remoted Vulkan.**

| 3D content | On the viewer's GPU | As pictures |
|---|---|---|
| Model viewers, CAD, data viz, product previews | **Best, via retained 3D nodes** (local camera, exact pixels, tiny bandwidth) | works |
| Games, engines with readbacks and streaming | poor: fps capped by RTT, readback stalls, huge uploads, wrong GPU | **Best** (§2.6 techniques) |
| A modest Vulkan app on a LAN, strong viewer GPU, no GPU on the host | good, if it never reads back | works if the host has a GPU |

So the plan is: (a) external surfaces go as pictures (R6); (b) the retained 3D nodes, once
Fresco draws them, travel in the scene stream like everything else, which makes viewer-side
3D the normal case for 3D *UI* (R11, after the renderers draw them locally); (c) Vulkan command
remoting stays possible later as an opt-in per host and per app, for LAN use with a host
without a GPU, restricted to apps that declare no readbacks (decision 15). The compositor's own
drawing on the viewer already uses the viewer's GPU when it has one, as any local window does.

### 7.3 Pixel-mode windows and windows the viewer cannot draw

Windows in split pixel mode (`OP_SLOT_UPDATE_REGION` of RGBA pixels as their content), legacy
clients, and windows using a node kind the viewer lacks (§4.3) are rastered by the host's
renderer and sent as pixels, damage region by damage region (the renderer already knows the
damage rect, fresco incremental drawing). Encoding per region:

- Small or static damage (a caret, a hover, a dialog): **lossless**, the region's changed
  tiles with zstd. Text in these windows therefore stays exact.
- A region that changes on most frames for more than about half a second: switches to the
  video encoder for that rect, and back to lossless once it settles, with a final lossless
  refresh so nothing stays smeared (RDP's "lossy then refine", but with the refine guaranteed).

### 7.4 The encoder jail

Encoders are large C codebases and the GPU copy path touches the driver, so `fenestrad` never
encodes itself. Each encoded region gets a `fenestra-enc` instance in a one-shot jail (the
renderer's pattern): input from a sealed memfd ring, output a byte stream to `fenestrad`, no
network, no files, a memory and CPU limit. Decoders on the viewer are the player's decoder
worker bundle, same jail shape. Licences: dav1d (BSD-2), libvpx (BSD-3), rav1e (BSD-2),
zstd (BSD) are plainly permissive. SVT-AV1 is BSD-3-Clause-Clear plus the AOM patent licence,
which `scripts/licence-audit.py` may not list; it is added only after the audit is run on the
real dependency tree and Shankar accepts that pairing (rav1e is the default encoder until then).
Patent posture follows atrium-player.md §4 (royalty-free codecs native, H.264 through
Hospes only).

## 8. Transport, bandwidth and congestion

### 8.1 One QUIC connection

QUIC (RFC 9000) over UDP, through a permissive Rust stack: quinn with rustls (MIT or
Apache-2.0). rustls needs a crypto provider, chosen on purpose: aws-lc-rs carries
OpenSSL/SSLeay-licensed code, and ring's licence (ISC plus OpenSSL-derived parts) also needs
checking; the choice is made by running `licence-audit.py` on the real tree in R0 (decision 12):

- **Reliable streams** for each window's scene stream, for blob uploads (lower priority), for
  control (outputs, clipboard offers, session events).
- **Unreliable datagrams** (RFC 9221) for encoded video and surface frames (a late frame is
  dropped, not retransmitted), pointer motion (the latest position wins) and acks.
- **Connection migration**: the session survives the viewer changing networks (Wi-Fi to
  wired, a new address), as mosh does, without a reconnect.

Why not TCP or the Stoa datagram envelope: TCP queues stale data behind a lost packet (failure
7) and cannot carry datagrams; Stoa's raw envelope (aqueduct-remote.md §5) is the right shape
for a terminal but would mean re-inventing streams, priorities and congestion control that
QUIC already has. Why not WebRTC: it is a large stack, browser-shaped, and its strengths (SDP,
ICE) are what Concursus provides in Atrium's own idiom (§8.5).

### 8.2 Priorities

Strict priority, highest first, so a heavy class can never starve a light one:

1. Input from the viewer, and acks.
2. Scene records for the focused window, then other visible windows.
3. Cursor shape, clipboard offers, session control.
4. Video and surface datagrams (rate-limited by §8.3, not by priority alone).
5. Blobs on screen, then near the viewport, then off screen (§5.5).

Scene records for a window are only sent when the window has something newer than its acked
version, and at most once per viewer frame (§6.6), so priority 2 cannot flood.

### 8.3 Congestion control and the degrade ladder

- QUIC's congestion controller (BBR-style, delay-based rather than loss-based, so it does not
  fill router buffers; L4S where the path supports it) sets the send rate. Fenestra keeps the
  bytes in flight near the bandwidth-delay product: it never builds a queue that would add
  latency (bufferbloat), and it measures RTT continuously.
- Encoded regions get the rate left after scene traffic, with their encoder bitrate set from it
  each second; video and surfaces adapt, text never does.
- When the link gets worse, Fenestra steps down in this order and back up when it recovers:
  1. Delay off-screen blob uploads.
  2. Send previews instead of full images for new on-screen images.
  3. Lower the frame rate of encoded regions, then their bitrate.
  4. Coalesce scene frames of background windows to one per second.
  5. Last: the focused window's scene frames are coalesced to the rate the link sustains.
     Text is never made lossy; a slow link shows it later, never blurrier.

### 8.4 Measurements the viewer shows

`fenestra stats` and the toolbar's statistics panel: RTT, send rate, bytes per class, frames
skipped by state sync, blobs pending, encoded regions and their bitrates, click-to-frame time
(the viewer stamps an input and measures when the host's frame acking it lands). These are the
numbers R2 and R6 are judged by.

### 8.5 Finding the host

v1: an address or name the person types (`fenestra connect host`), over SSH (§11.2). Later:
hosts the person has paired appear in the Fenestra app through mDNS on the local network
(discovery only; no trust from it), and Concursus (concursus.md) for hosts behind NAT, with its
relay as the fallback when direct UDP fails.

### 8.6 Bad networks, case by case

The design's answer to a bad link is that **the viewer never waits for the network to stay
usable, and the host never builds a backlog**. Each case below says what the person sees.
Targets are to be met in the R2 and R6 `netem` profiles (§19), not measured yet.

| Condition | What Fenestra does | What the person sees |
|---|---|---|
| **High RTT** (100 to 300 ms, satellite 600 ms) | Cursor, animations, video pacing, viewer scrolling (§6.3), rootless window management are local. In whole-desktop mode a window drag is **predicted**: the viewer moves the window locally from the placement it already has and the host's WM confirms or corrects it one RTT later. Frame callbacks are not tied to acks (§6.6). Pointer motion coalesced, not queued. | Moving, scrolling, hovering the cursor feel local. A click that needs the app costs one RTT plus app time; typing costs one RTT until R7's prediction. |
| **Low bandwidth** (256 kbit/s to 2 Mbit/s) | Scene traffic is small and goes first (§8.2). On first connect: window structure and on-screen glyph atlases first, image previews next, full images and off-screen blobs last (§5.5). Encoded regions step down the ladder (§8.3); below their floor (a set bitrate, default 300 kbit/s per region) a video region shows its last frame with a lossless refresh every few seconds and a "paused on slow link" badge, and resumes when the rate allows. Audio drops to 24 kbit/s Opus before video loses more. | Text and UI stay exact and current; images sharpen as they arrive; video degrades, then pauses, never smears the text around it. |
| **Loss and jitter** (Wi-Fi, mobile: 1 to 10 % loss, 30 ms jitter) | Reliable streams retransmit, but state sync means a lost record is usually superseded by the next difference anyway (§5.2). Acks are sent redundantly (each carries the last few). Video and surface datagrams carry forward error correction sized from measured loss; the viewer reports lost frames so the encoder sends an intra refresh or switches reference frames instead of waiting for a keyframe. Pointer datagrams: latest wins. Congestion control is delay-based, so random loss is not read as congestion. No playout buffer for surfaces; video layers keep the player's own small buffer. | Occasional late UI updates by one RTT; video holds or drops a frame rather than freezing for seconds. |
| **Sudden drop** (seconds to minutes) | Keepalive every 250 ms; after 1 s of silence the viewer marks the remote area stale (dimmed, "reconnecting" with a timer) and keeps the last frames. **Input typed or clicked while disconnected is not queued and not replayed later**: it is discarded and the overlay says so (decision 14). The host keeps the remote outputs for the grace period (§12) and the apps see nothing. | A frozen, clearly marked screen, not a frozen-looking live one; no surprise burst of old keystrokes on reconnect. |
| **Reconnect** | First QUIC connection migration (same connection, new path); if that fails, a new handshake with the same pinned keys while the token is valid (no SSH needed), resuming each window from its acked version with the blob cache intact, so a reconnect costs the difference, typically kilobytes. Retries back off 0.25, 0.5, 1, 2, 4 s, then every 5 s, bounded by the token's expiry; after expiry, one SSH re-mint (which may prompt for the key). | The screen comes back current within about one RTT of the link returning. |
| **Address change** (Wi-Fi to wired, roaming between access points, NAT rebinding) | QUIC connection migration, path validation, congestion state reset for the new path. | Usually nothing; at worst a short "reconnecting". |
| **Flapping link** | Degrade ladder steps down at once and up only after 10 s of stable rate (hysteresis). Grace period means windows never move home on a flap. A reconnect never resends full state when acked versions are valid. | Quality settles instead of oscillating. |
| **Bufferbloat** (a full upload elsewhere on the same link) | Delay-based congestion control keeps bytes in flight near the bandwidth-delay product; Fenestra itself never adds a queue. RTT rise is the signal to step down. | Latency stays near the base RTT instead of growing to seconds. |
| **UDP blocked** (some corporate and hotel networks) | Fallback: the session's streams run over TLS 1.3 over TCP on the same per-session port, with the same pinned raw public keys and `K_sess` proof (or, last resort, tunnelled through the SSH connection itself, still inside that same pinned TLS), with media datagrams turned into a single latest-frame-wins stream. Head-of-line blocking returns, so the stats panel says "TCP fallback". Concursus's relay (R9) covers networks that allow only 443. | Works, with more latency under loss; marked. |
| **Tiny uplink** (asymmetric mobile) | The viewer sends only input, acks and stats: a few kbit/s. | No effect. |

## 9. Input

- The viewer sends **HID usages and positions**, not characters: key usage (page 0x07),
  modifiers, pointer position in points relative to the window, buttons, scroll deltas (pixel
  and line), touchpad gestures, gamepad state (atrium-input.md §4.1 shapes). The host applies
  its own keyboard layout, so shortcuts behave as on the host; text input methods run on the
  host.
- Option (decision 7): **use the viewer's layout** by sending the keysym the viewer's layout
  produced alongside the usage; the host uses it when the session is set to "viewer layout".
  This is what people expect from a laptop with a different layout than the workstation.
- Input becomes ordinary host input in frescod, injected as from a Tactus device with the
  session's device ids. It is routed by the host's compositor-owned geometry (H7). A view-only
  session sends no input at all; the viewer's frescod does not even forward it.
- **Remote presses count only for an allowlist of ordinary purposes.** frescod tags every
  input event with its source (seat or a session id). Activation tokens (tabula.md §2) carry a
  purpose, and a press from a full-input session mints tokens only for purposes on a fixed
  **allowlist**: paste, open a link or file in an app, a choice in the file picker, share,
  print, drag and drop, window focus and raise. Every other purpose, including every sheet added
  later unless it is put on the list in review, **denies remote presses by default**: Arca
  approvals, Curia and Portcullis grant sheets (`background allow`, `listen allow`, app
  permissions), the unlock sheet, Fenestra's own host settings, pairing, Sigillum PIN entry.
- **Authority sheets raised by a remote session need a security-key touch on the viewer.**
  Such a sheet is shown only on that session's remote outputs, never on the host's seat
  monitors, so nobody at the host's desk can press it for the remote person; a seat press only
  counts for sheets the seat itself raised. The sheet is drawn by the host's trusted chrome with
  "Requested from <viewer device>, remote" in its purpose text. The person satisfies it with a
  FIDO touch through Sigillum on the viewer, relayed as follows: the host sends only a sheet id
  and a 32-byte nonce; the viewer's sigillumd builds the relying-party id itself from the
  **pinned host identity** (`fenestra:<host key fingerprint>`, a per-host RP the viewer
  registered at first connect) and the client data from that RP, the nonce and the sheet id, and
  never signs for an RP id or client data the host supplied. A hostile host can therefore never
  obtain an assertion for the viewer's own unlock RP, for a web RP, or for another host. A
  headless host has no seat, so for it the key touch is the only path; a person without a
  security key cannot approve authority sheets remotely and gets a refusal that says why. A
  stolen viewer key gives the session's use, but not the power to widen it.
- **A scoped session's input stays in scope.** For `scope = "app:<id>"` or `"window:<id>"`,
  frescod routes the session's input only to windows in scope; it never reaches chrome, the WM
  or windows outside the scope, even if one is on top. Anything else is dropped and counted.
  What is in scope:
  - `window:<id>`: that window only. No popups of other windows, no system sheets at all (the
    file picker, share and print sheets are not shown and the app's request for them is
    refused for that viewer), no notifications, no audio. It is a viewing scope.
  - `app:<id>`: that app's windows and popups; its notifications and audio only. System sheets
    the app raises (picker, share, print) are shown in scope **only if the mint sets
    `sheets = true`** (default false), because the picker shows the persona's files beyond the
    app's own. Authority sheets follow the key-touch rule above.
- Every input record is fixed-size, bounded and rate-limited (at most one pointer motion per
  viewer frame, a key-event cap per second); `fenestrad` drops a viewer that exceeds the caps.
- Local shortcuts on the viewer (switch away, release the keyboard, leave full screen) are
  caught by the viewer's frescod first and never sent; a "send keys to host" mode in full
  screen sends everything except the **secure attention key**: one chord the viewer's frescod
  handles below every client and never forwards (the same key that brings up the viewer's own
  lock and switcher), so a host can never trap the person in a full-screen session.
- **Full screen keeps an edge strip.** Even in full screen the viewer's frescod draws a thin
  host-coloured strip along one edge (default top, 4 px, the host's colour, the name on hover),
  so a host painting a fake viewer lock screen or Arca sheet across the whole display is still
  visibly remote. The viewer's own lock screen and sheets are never shown without first leaving
  the remote area, and they appear only after the secure attention key or a local event.

### 9.1 Input devices on the viewer

The viewer's own Tactus owns every device plugged into the viewer (USB, Bluetooth through
Vinculum, the built-in trackpad); Fenestra never redirects a raw USB or HID device to the host.
The viewer decodes each device locally and forwards **typed events** in Tactus's own shapes
(atrium-input.md §4), which the host's frescod injects as a virtual Tactus device per viewer
device. The host never parses a HID descriptor from the viewer, and the viewer never lets the
host write raw output reports.

| Device | Decoded on the viewer | Sent to the host | Back from the host | Notes |
|---|---|---|---|---|
| **Mouse** | Pointer stage (acceleration with the viewer's settings), buttons, high-resolution wheel (`value_120`) | Absolute position in the window (normal use), coalesced to one per viewer frame; buttons and wheel as events, never dropped | cursor shape | Under **pointer lock** (a game or 3D modeller on the host asked, after a press, atrium-input.md §4.1) the viewer hides its cursor and sends raw relative deltas, **summed, never dropped**, at up to 1000 Hz in datagrams with a sequence number, so no motion is lost to coalescing. The secure attention key (§9) and the usual Escape always release the lock on the viewer itself. |
| **Trackpad** (built-in or external) | Contacts, palm rejection, taps, two-finger scroll with phases, pinch, swipes, all in the viewer's Tactus | Scroll deltas with phases, `MAGNIFY` pinches, clicks; momentum is computed on the viewer | — | With viewer-scrollable groups (§6.3) the scroll and its fling run entirely on the viewer. Three- and four-finger swipes are the viewer's own system gestures; in "send keys to host" full screen they go to the host's WM instead. |
| **Keyboard** | HID usages, modifiers, repeat | Usages (and keysym in "viewer layout", §9) | LED state (caps lock) | Key repeat is generated on the host from press and release, so a dropped release cannot leave a key repeating: the viewer resends the full set of held keys with every key datagram, and the host releases anything missing from it. |
| **Gamepad** | Descriptor decode, `PadLayout`, buttons, axes, hat | `PadLayout` once on attach; then a **full state snapshot** (all buttons and axes) per change, latest wins, sequence numbered, up to 250 Hz | rumble: typed effects (strength, duration), bounded | A pad appears on the host as a remote pad in Forum's controller menu (fresco-capabilities.md C5) and follows the usual claim rules: exclusive only while the app is focused, the person can take it back. A lost snapshot is harmless, the next one carries the whole state. |
| **Racing wheel, pedals, flight stick, HOTAS** | Same as gamepads (HID axes at full resolution, 16-bit) | Full-state snapshots, up to 1000 Hz for wheels | **force feedback as effects**, not as raw reports | Force feedback is latency-critical, so the host sends *effect descriptions* (HID PID shapes: constant, spring, damper, friction, periodic, with gain and duration) and the **viewer's driver plays them on the device locally**. A spring or damper therefore reacts to the wheel at local speed; only changes of effect parameters cross the network. A constant force the game updates per frame is RTT-delayed, which is fine on a LAN and noticeable on a WAN (§18). The viewer caps every effect's strength and duration to a per-device limit the person sets, so a host can't drive a wheel harder than allowed. Device settings (rotation range, deadzones) stay on the viewer. |
| **Pen tablet** (later, with Tactus pen support) | Pressure, tilt, eraser, proximity | Typed pen events, not coalesced while the pen is down (strokes need every sample) | — | Absolute mapping to the remote window or the whole session, the person's choice. |
| **Touchscreen** (later, Tactus touch is deferred) | Contacts | Typed touch events | — | — |

How devices are offered to the host:

- **Keyboard and pointer** go to the session that has focus on the viewer, as for any local
  window. They need no extra step.
- **Gamepads, wheels and other game controllers** are offered per device, the first time a
  remote app asks for a pad: the viewer shows "Use <device> with <host>?" once, remembered per
  host. A controller in use by a local app on the viewer is not taken away.
- **Hot-plug** on the viewer sends attach and detach; the host's virtual device appears and
  goes with it, and the host app sees an ordinary connect or disconnect. A dropped connection
  detaches nothing during the grace period, but sends no input (§8.6); the host releases all
  held buttons and centres axes the moment the link is lost, so a car does not keep
  accelerating.
- Only the **controlling viewer's** devices are forwarded; view-only viewers forward none.

Timing and bounds:

- Every input datagram carries the viewer's timestamp; the host maps it to its own clock with
  an offset estimated from acks, so apps that use event times (gesture velocity, rhythm games)
  see consistent intervals.
- Each class has a rate cap (pointer motion 1 per viewer frame unlocked, 1000 Hz locked;
  gamepads 250 Hz; wheels 1000 Hz; keys 100 per second), at most 8 forwarded devices per viewer
  and 4 game controllers. Over a cap, `fenestra-session` coalesces (state snapshots) or drops the
  viewer (event floods), §11.10.
- Effects sent back to the viewer are fixed-size records (at most 16 active effects per device,
  bounded parameters), parsed in `fenestra-net` and checked again by the viewer's Tactus before
  any output report is built.

## 10. Audio, clipboard, files

- **Audio.** Lyra on the host gains a remote sink per session (lyra-external-device.md is the
  model); streams are mixed on the host and sent as Opus (BSD) datagrams with timestamps, and
  the viewer's Lyra plays them. Video layers carry presentation times on the same session
  clock so picture and sound stay in step. Microphone input from the viewer is a separate grant
  (decision 8), off by default.
- **Clipboard.** Through Tabula on both ends with per-direction switches (host to viewer,
  viewer to host), both on by default for a full session, both off for a restricted one. Only
  formats Tabula already handles cross, as CAS blobs; nothing is offered until the person
  pastes (the paste on one side asks the other side for the bytes), so the clipboard is not
  streamed. **Viewer to host** is the dangerous direction (a compromised host could claim a
  paste at any time and pull whatever the person copied on the viewer): the viewer releases its
  clipboard bytes only to a request that **cites the input sequence number** of an input event
  the viewer forwarded within the last second and itself classified as a paste gesture: the
  viewer's own paste chord in its own layout, or a press the host's trusted chrome marked as a
  paste control. One release per cited event. A key that is a paste chord to the viewer but
  something else to the host app (vim, another layout) can at most release the clipboard once,
  for that keystroke, which the person made; a request citing any other event, or none, is
  refused. Host to
  viewer needs a paste gesture on the viewer, as for any app. This is the channel Check Point used against RDP clients, so the viewer's Tabula
  treats remote clipboard data like any untrusted input and paths are never interpreted.
- **Files.** Not a Fenestra feature. Drag and drop of files between viewer and host goes
  through Scrinium's picker on the receiving side (a press there chooses where it lands), never
  by writing a path the sender names. v1 has no file transfer; R8 adds drag from viewer to a
  remote app as an upload into that app's grant.
- **No device redirection** (USB, smartcards, printers) in v1. Each of those is a large attack
  surface in RDP and needs its own spec.

## 11. Security

This section is the whole security design of Fenestra in one place. Other sections point here.
Shankar asked for it to be reviewed on its own (2026-11-26); the review log is §21.

### 11.0 What is protected, and the rules that protect it

Assets, in order of harm if lost:

1. **The host session's authority**: the person's apps, files, secrets in Arca, and the power
   to grant permissions. A remote viewer may *use* the session; it must not be able to *widen*
   it (new grants, unlock, pairing) without a local press or a security-key touch.
2. **The viewer machine**: its kernel, its GPU driver, its own sessions and secrets. A host the
   person connects to is untrusted; it must not reach anything on the viewer beyond pixels in
   the remote area and data the person deliberately pastes or drops.
3. **What is on screen**: documents, messages, passwords being typed. Only the authenticated
   viewer of that session may see them; the host's own desk must not see them either unless the
   person chose that (decision 3).
4. **Availability**: a hostile viewer or network must not degrade the host's local session or
   other viewers; a hostile host must not hang the viewer's desktop.

Rules (each one is checked in §11.18):

- Every byte from the other machine is parsed in a jail with nothing to lose (H8.1's "parse
  where the capability set is empty"), and the processes with capabilities read only
  fixed-size records.
- No new long-lived secrets: authentication rides SSH; per-session keys live in memory only.
- No standing network listener on the host.
- The other side's claims never grant anything: roles, sizes, offsets, hashes, versions are
  checked by the receiver against what it itself decided.
- A remote person is the person for *use*, never for *authority*.

### 11.1 Threat model

| Party | Can do | Contained by |
|---|---|---|
| Network attacker | Read, change, replay, drop packets | QUIC TLS 1.3, keys bound to the SSH handoff (§11.2); content hashes (H2) |
| **Hostile host** (a compromised workstation or cloud desktop you connect to) | Send any bytes on any stream | Every parser of host bytes runs in a zero-capability jail: renderer (full validator, H9), decoders, `fenestra-net` (framing only). Budgets (F4h) and cost limits (H6) per remote client. Remote content is marked and confined (§11.5). Clipboard and files only by the viewer person's press. |
| **Hostile viewer** (a stolen laptop with a valid key, a compromised viewer) | Send input and acks; see what the session shows | Session capabilities (view-only, app, window scope); input bounded (§9); `fenestrad` parses only fixed-size records; tokens expire and can be revoked from the host (§11.6); authority-granting sheets need a seat press or a security-key touch, and scoped input stays in scope (§9); the host shows that it is being viewed (§11.4). A compromised viewer sees what the session displays; that is inherent (aqueduct-remote.md §1). |
| A hostile app on the host | Draw anything in its own windows | Unchanged from local: H1–H8. Remote outputs do not give it more: it cannot learn that it is being viewed remotely beyond its output's properties (decision 9). |
| Another user or persona on the host | Try to view or join a session that is not theirs | Sessions are minted by the session's own user over SSH (§11.2); `fenestrad` maps a session to exactly one seat session and persona. |

### 11.2 Authentication and keys

- **Default: the SSH handoff** of aqueduct-remote.md §2, unchanged: `fenestra connect host`
  runs `ssh host aqueduct-shell mint fresco ...`, sshd authenticates the person with their
  key (from the viewer's Arca SSH agent: the key is used, never read, arca.md), and the mint
  returns `{address, port, session id, K_sess, caps, expiry}` inside the SSH channel.
- **Binding the QUIC connection to the mint.** rustls does not offer external TLS 1.3 PSKs,
  so the binding uses **raw public keys** (RFC 7250) pinned through the SSH channel instead.
  At mint time `fenestrad` makes a fresh Ed25519 key pair for the session (in memory only) and
  the mint returns its public key; the viewer makes its own and sends its public key in the
  mint request, inside SSH. The QUIC handshake is then ordinary TLS 1.3 (ECDHE, forward secret)
  with both sides presenting raw public keys, and each side accepts exactly the key pinned
  through SSH and nothing else. No certificates, no PKI, no trust-on-first-use prompt: a network
  attacker cannot get a key pinned without breaking SSH. The first application message is
  `SESSION_HANDSHAKE` with `AuthMethod::SshHandoff` (aqueduct-remote.md §3): an HMAC with
  `K_sess` over a server nonce and the TLS exporter value, so the token is tied to this exact
  connection. **0-RTT is off** on both ends, so no input can be replayed from early data.
- **Pairing without SSH (R9).** For a viewer that is not set up for SSH (a phone, a tablet),
  pairing mints a device key in the viewer's Arca and registers its public half on the host
  through a code shown on the host's own screen and typed on the viewer (or a QR code); the host
  keeps paired device keys per persona in Arca, revocable in Curia and `fenestra devices`.
  Paired devices mint the same session token, and the key pinning is unchanged (the device key
  is the pinned viewer key). Pairing details are in §11.2a.
- **What the host keeps.** The binding that matters is the pinned keys; the `K_sess` proof ties
  the aqueduct session token to this TLS connection. Checking an HMAC needs `K_sess` itself, so
  the session's `fenestra-session` jail holds `K_sess`, the two pinned public keys and its
  session private key **in memory only**, for the session's life, and nothing is written at
  rest (aqueduct-remote.md §6's "store only `H(K_sess)`" is about at-rest records; Fenestra has
  none). `K_sess` crosses the network only inside the SSH channel, at mint.
- **Reconnect** within the token's expiry is a new QUIC handshake with the same pinned keys and
  a fresh `K_sess` proof; after expiry the viewer re-mints over SSH. If `fenestrad` restarts,
  its session keys are gone with it, so the viewer re-mints over SSH and resumes from version 0
  (§12); the apps and the session on the host are not affected. The session on the host is
  never lost on expiry or restart, only the credential.
- **The mint is checked, not trusted.** `fenestrad` takes a mint request only from
  `aqueduct-shell` on its local socket, reads the caller's uid by `getpeereid`, and mints only
  for a seat or headless session owned by that uid, in the persona that session runs; a uid
  with no such session gets a refusal. A headless session needs its persona's key group
  unlocked (the same gap as background-apps.md G11: a session that cannot reach its key group
  is not offered for viewing until it can).
- **Bounds.** How many viewers, by kind (Curia settings; refused past them, with the reason):
  - **Whole desktop** (`scope = "session"`): at most **one viewer with control** at a time, plus
    up to **3 view-only** viewers (4 in all). Two people driving one desktop needs input
    arbitration (whose pointer, whose focus), which is out of scope (§18); a second full mint
    while one is attached is refused unless the person asks to take over, which disconnects the
    first.
  - **One app or one window** (`app:<id>`, `window:<id>`): each scoped session is separate and
    may have up to 4 viewers (one with control if `input = "full"`). Different apps can be
    shared to different people at once.
  - **Per host**: at most **16 sessions** and **32 viewers** in all, across every user and kind.
  - Why these numbers: in scene mode a viewer costs the host little (an acked version per window
    and the bytes it is sent), but each encoded region (video, GPU surfaces) is encoded once per
    viewer at that viewer's bitrate, and those encoders are the expensive part (at most 4 per
    session, §11.10). The defaults are guesses to be checked in R2 and R6; the host can lower
    them, and raise the per-host ones. Per viewer, the renderer keeps at most one acked version per window and a diff
  backlog bounded by the window's own scene budget (F4h); a viewer whose unacked data passes
  64 MiB, or that has not acked for 30 s while data waits, is dropped and must reconnect
  (from version 0).

### 11.2a Pairing a device without SSH (R9)

Pairing needs a listener before any key is pinned, which is the one exception to "no standing
listener", so it is narrow:

- It starts only from a **press on the host's own seat** ("Pair a device" in Fenestra's host
  settings, or `fenestra pair` run from a seat session, which raises that sheet). A remote
  session cannot start pairing (it is not on the allowlist, §9), so a stolen viewer cannot pair
  more devices. A headless host pairs only over SSH (a device that can do SSH), never by code.
- The sheet shows a pairing code **only on seat monitors**, never on remote outputs: 10
  characters of base32 (50 bits), valid for 2 minutes, and a QR code of the same.
- For those 2 minutes one `fenestra-session`-shaped pairing jail listens on one port and runs a
  PAKE keyed by the code (SPAKE2, from a permissive crate chosen by the licence audit), so the
  code itself never crosses the network and an offline guess is impossible. Three failed
  attempts close the port and void the code.
- On success the viewer's new device public key (its private half made in the viewer's Arca) is
  stored in the host persona's Arca with the name typed on the host's sheet (32 characters,
  sanitised as in §11.4), and the host's public key is pinned on the viewer. Later sessions from
  that device mint through a signed request to that device key instead of SSH, and are pinned
  exactly as in §11.2.

### 11.3 Jails, uids and grants

| Process | Machine | uid | Network | Filesystem | Parses |
|---|---|---|---|---|---|
| frescod broker | host | `_frescod` | none (unchanged) | none | fixed records from `fenestrad` |
| fresco-renderer | both | `_fresco-render` | none | none | client wire (host); scene stream (viewer) |
| `fenestrad` | host | `_fenestra` | none | its socket dir | fixed records only (mints, session jails, frescod) |
| `fenestra-session` | host | one per session | `listen` on one UDP port of the range, local network unless widened | none | QUIC and TLS framing, handshake proof, viewer input and ack records |
| `fenestra-enc` | host | `_fenestra-enc` | none | none | its memfd ring (pixels, no structure) |
| `fenestra-net` | viewer | per viewing persona | outbound UDP to one host address | none | QUIC framing; stream demux |
| decoder workers | viewer | player's decoder uid | none | none | compressed video (hostile) |
| Fenestra app | viewer | the app's uid | none | its overlay (cache, §5.4) | session list, settings |

`fenestrad`'s listen grant is a system service grant in its services.d manifest, not an app's
background grant; it listens only while at least one person on the host has allowed remote
viewing (Curia, `fenestra host enable`), and the port is closed otherwise. No code goes into
portcullisd for Fenestra: the jails use existing manifest grants (AGENTS.md review rule).

### 11.4 The host always shows that it is being viewed

When a viewer is attached to a session, the host's frescod draws a trusted indicator in the
chrome lane (F4d) on every local monitor of that session: who (the SSH key's comment or the paired device name, both from the host's own records, never
from the viewer at connect time, cut to 32 characters, printable characters only, shown as
plain text), view-only or full control, and a one-press Disconnect. The bar shows it too, but the
indicator is frescod's, so an app cannot cover or fake its absence (H7). A host with the screen
locked (decision 3) shows the indicator on the lock screen.

### 11.5 Remote content on the viewer cannot pose as local

H7 applies across the network:

- Remote windows can never take the viewer's Chrome, Notification or lock-screen roles; the
  viewer's frescod maps every remote role to Document, Dialog or Popup inside the remote area.
- In rootless mode every remote window gets a viewer-drawn **host strip** (the host's name and
  a colour the person chose for it), like the Navigator's origin strip, in the trusted lane.
  In whole-desktop mode the frame of the session window carries it. A remote host therefore
  cannot draw a convincing local Arca prompt or Vestibulum unlock sheet: those never appear
  without the strip on the viewer, and the viewer's own trusted sheets are never remote.
- Secure input (F4f) and credential slots on the host work for the remote person, but the
  viewer shows that the field is remote; the viewer's Arca never autofills into a remote window.

### 11.6 Sessions, capabilities and revocation

- Session capabilities from aqueduct-remote.md §4: `input = "full" | "none"`, `scope =
  "session" | "app:<id>" | "window:<id>"`, `clipboard`, plus Fenestra's `audio`, `microphone`
  `layout` and `sheets` keys (§9). A scoped session's remote outputs show only the windows in scope; the
  host's renderer for other apps never emits a stream for that viewer.
- `fenestra sessions` and `fenestra revoke <id>` on the host (and the indicator's Disconnect)
  end a session at once: `fenestrad` ends the session's `fenestra-session` jail, and its keys go with it; the remote
  outputs go and Forum moves the windows back to local monitors (or keeps them parked on a
  headless host).
- Chained remoting (viewing a host from a session that is itself remote) needs a fresh consent
  for a full mint, per aqueduct-remote.md §8's lean.

### 11.7 Residual risks, stated

- A compromised host shows the remote person whatever it likes inside the remote area, and can
  try to phish them there. The host strip is the mitigation; it cannot stop a person typing a
  password for the *host* into a fake host dialog, which is the same as any compromised machine.
- The scene stream reveals more structure than pixels would: window titles, the text of
  documents as glyph ids, which font. Both ends already hold this; the transport is encrypted;
  it matters only for a compromised viewer, who sees the screen anyway.
- Timing of acks and frame callbacks is a cross-session timing channel at the host (H10); it
  is no wider than the local one.
- A viewer's cache is a second copy of the person's on-screen data; it is encrypted in their
  key group and can be turned off (decision 10).
- The viewer's GPU runs the compositor's own raster of remote scenes (fresco-compute kernels,
  Atrium-shipped and attested, aqueduct-gpu.md §11.5), never host-supplied shaders. Remoted
  Vulkan (§7.2a) would change that and is therefore an opt-in with its own review.

### 11.8 Attack surface, parser by parser

Every place one machine parses bytes the other wrote, where it runs, and how it is tested.

| Parser | Runs on | In | Input from | Bounded by | Test |
|---|---|---|---|---|---|
| QUIC + TLS handshake (quinn, rustls) | host | `fenestra-session` (one per session) | anyone who can reach an open per-session port | rustls/quinn limits; port open only after a mint; handshake rate limit per source (§11.10) | upstream fuzzing; R3 handshake flood |
| QUIC + TLS handshake | viewer | `fenestra-net` | the host address the mint named | same | same |
| `SESSION_HANDSHAKE` proof | host | `fenestra-session` | authenticated peer | fixed size | unit tests, wrong-proof gate |
| Input, ack and stats records | host | `fenestra-session`, re-checked as fixed records by `fenestrad`, then frescod | the viewer | fixed size, every field ranged, rate caps (§9) | `fenestra-split-fuzz` style fuzzer on fenestrad's records |
| Scene stream records | viewer | fresco-renderer (full validator, H1 to H6, F4h budgets) | the host | per-client budgets, cost limits | stream fuzzer (R1), R3 hostile-host gate |
| Blobs (images, atlases, fills) | viewer | fresco-renderer | the host | size caps per class, hash checked (H2) | same |
| Image decoding (if the host sends encoded images) | viewer | the existing jailed image decoder (`org.atrium.image-decode`) | the host | its own caps | its own fuzzers |
| Compressed video | viewer | decoder worker jail (atrium-player.md) | the host | 150 MB feed cap, decoder limits | player's decoder fuzzing |
| Encoded regions (video codec, zstd tiles) | viewer | decoder worker jail; zstd in the renderer with an output cap | the host | frame size fixed by the region | same |
| Clipboard data | both | the receiving side's Tabula, as for any app | the other machine | Tabula's caps, formats it already parses | Tabula tests |
| Window placements, roles, titles | viewer | viewer's frescod via the renderer's fixed records (`fresco-split`) | the host | roles mapped (§11.5), titles 64 bytes | split fuzzer |
| Remote output descriptions | host | frescod via `fenestrad` records | the viewer | sizes and scales ranged as for EDID-derived outputs | fuzzer |

What is never parsed: the host never parses viewer-supplied scenes, pixels or files; the viewer
never interprets a path, a command line or a URL from the host (a link click in a remote app
opens on the host, never on the viewer).

### 11.9 Network exposure on the host

- No port is open while no session exists. A successful SSH mint makes `fenestrad` start that
  session's `fenestra-session` jail with one UDP port from the range, bound to the address
  family the mint came in on, and it accepts only a QUIC handshake presenting the viewer key
  pinned in that mint.
  The port closes when the session ends, is revoked, or its token expires with no connection.
- A minted port with no successful handshake within 30 s is closed and the mint is void.
- QUIC address validation (Retry tokens) is on, so a spoofed source cannot make the session jail
  send large responses (no amplification); stateless reset tokens are per session.
- The grant is local network by default. Widening it to "any network" is a Curia setting with
  a warning, and `fenestra host status` says which it is.
- The pre-authentication surface is therefore: the SSH server (already exposed, already the
  platform's remote entry point) and, only for a just-minted session, quinn and rustls up to the
  key check.

### 11.10 Denial of service and resource limits

Host:
- Sessions, viewers and backlog bounds as in §11.2. Handshake attempts are counted only **after**
  Retry address validation (so spoofed floods cannot close a real session's port): 10 failed
  validated handshakes a minute per source address, then that source is refused for the rest of
  the mint window; 30 from all sources and the port closes and the mint is void.
- Diffing for a viewer runs in that client's renderer jail and counts against the renderer's
  CPU limit, so a viewer that forces full resyncs slows only the app's own remote view, never
  the local seat's compose or other apps.
- Encoder jails: at most 4 encoded regions per session, each its own jail with memory and CPU
  limits; past that, further regions are sent as lossless tiles at a lower rate.
- Blob sends are capped per session per second by congestion control; the viewer cannot ask
  for blobs (there is no fetch-by-hash from the viewer), so it cannot make the host read
  arbitrary content.

Viewer:
- Each remote client's renderer has the local budgets (F4h: nodes, params, textures, atlas
  pages) and cost limits (H6, F4j), and it rasters under the same no-wait compose rule, so a
  hostile host's huge scene is refused and reported, and the viewer's own windows keep their
  frame rate (the noisy-neighbour gate covers it).
- The viewer's cache has its size cap; a host cannot fill the viewer's disk.
- Decoder jails have their own limits; a decoder that hangs is killed and that region shows its
  last frame.

### 11.11 Behaviour that runs on the viewer

Viewer-scrollable groups (§6.3), predicted window drags (§8.6), local camera control for
retained 3D (§7.2a) and, later, typing prediction (§6.5) put a little of the app's behaviour on
the viewer. Invariants for all of them:

- They change only **what the viewer draws**, inside the remote area. They never create input
  to the host beyond the person's own actions (a viewer scroll is forwarded as the scroll the
  person made; a predicted glyph is never sent as a key, the key itself is).
- They are bounded by values the viewer checks: a scroll offset stays inside the declared range
  and the group's clip; a predicted drag stays inside the session area; a predicted glyph uses
  an atlas the viewer already holds and is drawn only inside the hinted field's group.
- They can never apply to trusted chrome, the host strip or any viewer-local window.
- A host that lies in a hint can at most make the viewer draw a wrong tentative picture inside
  the remote area for one round trip, which it could do anyway by drawing it.

### 11.12 Outages, reconnects and migration

- Input during an outage is discarded (decision 14), so a reconnect never delivers old input.
- Connection migration uses QUIC path validation, so an attacker who spoofs the viewer's source
  address cannot redirect the host's stream to itself; it would also lack the keys to read it.
- A reconnect re-runs the handshake with the pinned keys and a fresh `K_sess` proof; a stolen
  session id alone is useless. After a `fenestrad` restart all session keys are gone and only an
  SSH re-mint works.
- The viewer refuses a reconnect answered by a different host key than the one pinned (a host
  that was reinstalled or replaced must be re-trusted through SSH's own host-key check).

### 11.13 Personas and multi-user hosts

- A session belongs to one host user and one persona; its windows, its clipboard (Tabula's
  persona scope) and its audio are that persona's only. Another persona's windows on the same
  seat are never emitted to that viewer.
- On the viewer, each viewing persona has its own `fenestra-net` instance, cache and paired
  hosts; a remote session cannot see or paste into another viewer persona's data.
- Two host users may each be viewed at once; their sessions share no renderer, port, cache or
  key.

### 11.14 Privacy

- The host's indicator (§11.4) and local blanking (decision 3) mean nobody is watched without
  knowing it, and nobody at the host's desk watches the remote person's work by default.
- Fenestra sends no telemetry. Statistics stay on the two machines.
- mDNS discovery (R9) is off by default; when on, it announces only that a Fenestra host
  exists, with a random per-boot instance name, never the user's name or persona; paired viewers
  recognise the host by its key, not its announced name.
- Logs (§11.15) record who connected and when, never what was shown or typed.

### 11.15 Audit

`fenestrad` logs to the system log (no content): mints and their caps, handshakes accepted and
refused (with reason), sessions ended and why, viewers dropped for exceeding bounds, revocations,
and authority sheets refused for lack of a local press. The same events go to vigil's validator
channel (atrium-scenegraph-hardening.md §4) so repeated refusals from one viewer show up as one
correlated alert. `fenestra sessions --history` shows the last 100 on the host.

### 11.16 Updates and version skew

- Wire version and feature set are negotiated only inside the authenticated connection, so a
  network attacker cannot force a downgrade.
- A feature the viewer lacks makes the host send that window as pixels (§4.3): a quality
  downgrade, never a security one, since the pixel path is parsed in the same kind of jail.
- The viewer runs the renderer bundle its own Opifex installed and signed; the host cannot ship
  code to the viewer.

### 11.17 Security tests that must pass before each step lands

- R1: the stream fuzzer (random, truncated and mutated records, hash mismatches, budget
  overruns) against the viewer-side renderer, with ASan where the renderer links C.
- R2 and R3: a hostile host (fuzzed streams, wrong roles, oversized titles, huge scenes), a
  hostile viewer (fuzzed input and ack records, floods, view-only sending input, scoped input
  aimed outside scope, authority sheets pressed remotely), handshake floods and wrong keys, a
  port that closes on time, jails and uids checked from the kernel (`jls`, `procstat`), the host
  indicator and the host strip present (screen capture), clipboard released only after a paste
  gesture.
- R6: decoder and encoder jails killed on a hang, with the region recovering.
- Every gate sources `scripts/lib/gate.sh` and fails if fewer checks ran than declared.

### 11.18 Invariants, for reviewers

1. frescod has no network and never reads network bytes.
2. Every parser of the other machine's bytes runs in a jail with no network it does not need,
   no files and no secrets (§11.8).
3. The host has no listening port without a live mint, except a seat-started pairing port for
   2 minutes (§11.2a); the process that parses network bytes holds one session's keys only.
4. The QUIC peer key is pinned through SSH; no certificate, no trust on first use, no 0-RTT.
5. `K_sess` crosses the network only inside SSH, lives only in the memory of that session's
   `fenestra-session` jail and the viewer's `fenestra-net`, and is never written at rest.
6. Remote presses mint tokens only for allowlisted purposes; authority sheets raised remotely
   are shown only to that viewer and need a key touch whose RP and client data the viewer built;
   scoped input never leaves its scope.
7. Viewer clipboard bytes leave only to a request citing a forwarded paste gesture's input
   sequence number, once per gesture.
8. Remote content can never take a viewer trusted role and always carries the host strip, in
   full screen too; the secure attention key is never forwarded.
9. The host always shows that it is being viewed, drawn by frescod.
10. Personal data at rest on the viewer lives only in the viewing persona's key group.
11. Every count, size and rate from the other side is bounded before it is buffered.
12. Viewer-local behaviour (§11.11) changes only drawing inside the remote area.
13. Input made during an outage is never delivered.
14. The viewer's GPU runs only Atrium-shipped kernels for remote content unless the person
    opted into remoted Vulkan for that host and app.

## 12. Persistence, roaming and reconnect

- Apps live on the host; a dropped connection only removes remote outputs after a grace period
  (default 2 minutes, decision 11) during which windows stay where they were and apps see no
  change. Reconnecting within it resumes at the acked versions: typically a few KB of
  difference, not a full redraw.
- After the grace period the outputs go; windows move to the host's local monitors, or on a
  headless host to a parked virtual output that keeps the layout for the next viewer.
- QUIC connection migration covers address changes without a reconnect (§8.1).
- If the host's frescod restarts, its clients replay (fresco-recovery.md §3) and the streams
  resume from version 0; the viewer keeps showing the last frames meanwhile, marked stale.

## 13. Several monitors, DPI, colour

- A viewer with several monitors offers each as a remote output, with its geometry, scale,
  refresh range and HDR capabilities. In whole-desktop mode the person picks "all my monitors"
  or "this window"; in rootless mode windows go wherever the viewer's Forum puts them.
- Colour: scene content is drawn on the viewer for the viewer's monitor, so an HDR viewer gets
  HDR video tone mapping and SDR UI in its own colour pipeline (fresco-video.md §5), and the
  host needs no knowledge of the viewer's panel beyond what the output declares.
- Span groups (fresco-span.md) are not offered on remote outputs in v1.

## 14. The portable viewer (macOS and Linux first), and later Windows and browsers

Shankar (2026-11-26): the non-Atrium viewer is needed **from the start**, because during
development Atrium runs in a VM while the person sits at macOS (the Mac and its shared VM) and
the cloud threads sit in a Linux container (with `scripts/cloud-vm.sh`'s VM). So the first
viewer is `fenestra-view`, a portable Rust program for macOS and Linux; the Atrium viewer of
§3.3 reuses its core and follows.

### 14.1 What it is made of

- **Network**: the same `fenestra-net` code (quinn, rustls, the pinned-key handshake, stream
  demux), built for macOS and Linux.
- **Renderer**: the scene-stream reader and the window raster. Today that code sits in
  frescod's `fresco-renderer` binary, which does not build off FreeBSD (kqueue, the split
  channel). R1 therefore moves the renderer's scene state, validator, stream apply and raster
  calls into a **portable crate** (working name `fresco-render-core`) used by both the FreeBSD
  renderer jail and `fenestra-view`. `fresco-compute` already builds on Linux and macOS (its
  CPU path through atrium-spv on arm64 and x86_64).
- **Presentation**: one native window per session (whole desktop) through `winit` and
  `softbuffer` (both MIT/Apache-2.0, licence-audited in R0), drawing the compute renderer's
  pixels; a GPU path through Metal or Vulkan later. Rootless mode on macOS (one native window
  per remote window) comes after the Atrium rootless step.
- **Input**: the adapters of §14.6 (AppKit and GameController on macOS; evdev-free Linux input
  through the windowing system, plus `hidraw` via the `tactus` crate for wheels and pads).
- **Audio**: Opus decode and CoreAudio or ALSA/PipeWire output through a permissive crate
  (`cpal`, Apache-2.0), audited in R0.
- **CLI**: the same `fenestra connect|list|stats|disconnect ...` commands (§15), and the same
  `--json`.

### 14.2 Headless mode, for gates

`fenestra-view --headless --png-every N --input-script FILE` renders without a window, writes
frames as PNGs, plays scripted input and prints stats as JSON. The cloud gates use it: the
Atrium host runs in `cloud-vm.sh`'s VM, the viewer runs in the Linux container, and a gate
compares the viewer's frames bit for bit against the host's own local raster of the same
scene. This replaces the "two cloud VMs" rig R2 had planned, so Fenestra gates need only one VM
under TCG.

### 14.3 Containment off Atrium

Off Atrium there are no jails, so the viewer splits itself into processes and confines each
with what the OS has, keeping §11's rule that host bytes are parsed only where nothing is held:

- **Linux**: `fenestra-net` and one renderer process per remote client, each with Landlock (no
  filesystem), seccomp (no socket creation in the renderer, no `exec`), its own network
  namespace for the renderer (no network), and memory limits by `setrlimit`; the presenting
  process holds the window and the input devices and reads only fixed-size records from the
  renderers (the `fresco-split` record format).
- **macOS**: the same split, with each helper under a sandbox profile (`sandbox_init` with a
  deny-default profile, or XPC services with App Sandbox entitlements when it is packaged as
  an app): no network for renderers, no file access, no other app's data.
- Until the confinement gate (R3's hostile-host checks run against `fenestra-view`) passes on
  a platform, the viewer prints and shows "development viewer: connect only to hosts you
  control" on every connection there. Connecting to your own VM is the development case and is
  fine without it.
- Secrets on the viewer: the SSH key stays in the person's own agent (macOS Keychain-backed
  `ssh-agent`, or the Linux agent); `fenestra-view` never reads key files. Its cache on disk
  (§5.4) goes in the OS's per-user cache directory, encrypted with a key held in the macOS
  Keychain or the Linux Secret Service, or stays in memory where neither exists.

### 14.4 Reaching a VM

- **Mac VM** (`scripts/run-vm.sh`, QEMU user networking): SSH is already forwarded on 2222.
  Fenestra adds a UDP forward for a small fixed session port range (default 24300 to 24303;
  `FENESTRA_PORTS` in `run-vm.sh` and `cloud-vm.sh` via `NETDEV_EXTRA`), and the mint returns
  the address the client used for SSH with the same port, so no address rewriting is needed.
  Changing `run-vm.sh` touches the shared VM, so it goes through the coordinator like any VM
  change.
- **Cloud VM** (`scripts/cloud-vm.sh`): the same forward on 127.0.0.1, and the viewer in the
  container connects to it.
- Under TCG every timing is a functional check, never a product number (AGENTS.md).

### 14.5 Later: Windows and a browser

A Windows build of `fenestra-view` (the same crates; AppContainer for confinement) and a
browser viewer (WebGPU for the compute kernels, WebTransport for QUIC, the browser's process
model for confinement) are plausible because nothing in the stream is FreeBSD-specific. Not
in v1.

### 14.6 Input adapters

Input on non-Atrium viewers needs no Tactus *daemon*. The contract with the host is the typed event
set of §9.1 (Tactus's event shapes), so each platform needs an **input adapter** from its own
APIs to those events:

- **macOS**: keyboard, mouse and trackpad from AppKit events (the OS's own palm rejection,
  momentum and gestures, which are better used than redone); controllers from the
  GameController framework; wheels and other HID devices through IOHIDManager raw reports,
  decoded with the pure `tactus` crate (descriptor parser, decoders), which has no FreeBSD
  dependency. Force feedback through IOHIDManager output reports built by the same crate.
- **Windows**: Raw Input for keyboard and mouse, Windows.Gaming.Input for pads and wheels
  (including force feedback); the `tactus` crate for anything else that exposes HID.
- **Browser**: Pointer Events with Pointer Lock, keyboard events (no HID usages for every key,
  so some keys degrade), the Gamepad API (rumble only, no wheel force feedback, coarse
  polling). A browser viewer is fine for desktop work and limited for games.

The adapter maps into the same events, rate caps and bounds, and the host cannot tell which
platform the viewer runs. Only the controlling viewer's devices are sent, as on Atrium.

## 15. CLI: everything the GUI does

Shankar's rule: every GUI action has a command. `fenestra` on both ends:

| GUI | CLI |
|---|---|
| Connect dialog | `fenestra connect HOST [--user U] [--view-only] [--app ID \| --window ID] [--rootless \| --desktop] [--fullscreen] [--monitors all\|current]` |
| Session list (viewer) | `fenestra list [--json]` |
| Toolbar: disconnect, full screen, send keys | `fenestra disconnect ID`, `fenestra fullscreen ID on\|off`, `fenestra keys ID host\|local` |
| Statistics panel | `fenestra stats ID [--watch] [--json]` |
| Host: allow remote viewing | `fenestra host enable\|disable`, `fenestra host status [--json]` |
| Host: who is viewing, disconnect (indicator) | `fenestra sessions [--json]`, `fenestra revoke ID` |
| Restricted share ("share this window") | `fenestra share --window ID --view-only [--expires 1h]` (mints and prints a connect line) |
| Paired devices | `fenestra devices [--json]`, `fenestra pair`, `fenestra unpair DEVICE` |
| Settings (cache size, keyboard layout, clipboard directions, audio, microphone, grace period) | `fenestra config get\|set KEY VALUE` (the same Curia keys) |
| Clear cache | `fenestra cache clear [--host H]`, `fenestra cache status` |
| Host colour and name on the strip | `fenestra host-label HOST --name N --colour C` |

`--json` output is stable and documented, so scripts and the VM gates use it.

## 16. How the design answers each failure in §2.8

| Failure | Where it shows up | Fenestra |
|---|---|---|
| 1. Text degraded | VNC JPEG, RDP 4:2:0, game streams | Text is glyph runs rastered on the viewer at its own scale (§4.1). Never lossy, at any bandwidth (§8.3). Pixel-mode windows use lossless for static regions with a guaranteed final refresh (§7.3). |
| 2. Encode, decode and queue latency | RDP, VNC, SPICE streaming | No encode or decode in scene mode; state sync means no queue (§5.2, §6.1). Encoded regions use game-streaming low-delay settings with no playout buffer (§7). |
| 3. Bandwidth scales with pixels | all pixel protocols | Scales with changes; idle costs nothing; blobs once per host, cached (§5). Encoded regions are only the rects that need them. |
| 4. Re-scaled pictures, wrong DPI, HDR, refresh | RDP, VNC, Waypipe | Viewer monitors are host outputs; apps draw at viewer scale; animations at viewer refresh; colour on the viewer (§4.1, §6.2, §13). |
| 5. Apps stop speaking the command protocol | RDP orders, SPICE QXL, X11 | Atrium apps already speak Fresco and nothing else; frescod refuses foreign vocabularies. The fallback is per region, so one 3D viewport does not turn the whole window into video (§7.2). |
| 6. Round trips on the drawing path | X11 | No request in the stream waits for the viewer; text measurement and shaping are in the app (resolution-independence.md, pergola). Cursor, animation, scrolling and rootless window management are local (§6.2, §6.3). |
| 7. Stale frames under congestion | TCP protocols | Diff against acked state, skip intermediates; QUIC streams per window; datagrams for media (§5.2, §8). |
| 8. Unconfined viewers, weak auth, side channels | VNC and RDP CVEs, Reverse RDP | Every host-supplied byte is parsed in an empty jail on the viewer; peer keys pinned over SSH, no passwords; clipboard by paste only, no file paths, no device redirection; trusted host strip and host indicator (§11). |

## 17. Decisions for Shankar

1. **Take the scene from the host's renderer jails and present viewers as remote outputs of
   the host's frescod**, rather than letting apps connect to a remote frescod
   (transport.md's sketch). *Recommend yes.* Apps keep no network, the session survives the
   viewer leaving, apps draw at the viewer's scale for free, and the validated canonical scene
   is the only thing that crosses.
2. **Whole desktop first (R2), rootless second (R4).** *Recommend yes.* Whole desktop needs no
   change to the viewer's Forum and proves the stream; rootless is the nicer daily experience
   and comes right after.
3. **When a viewer attaches to a session that also has a local seat, blank the host's local
   screen** (lock screen with the "being viewed" indicator and a Disconnect, §11.4), with
   showing it live as a per-host opt-in. *Recommend: blank by default.* Otherwise anyone at the
   host's desk watches the remote person's work, which they may not know; live is easy to turn
   on for your own two machines.
4. **Viewer-scrollable SCROLL groups (§6.3), built in frescod so local scrolling benefits
   too.** *Recommend yes,* in R5. It is the biggest perceived-latency win after the cursor.
5. **Typing prediction through edit hints (§6.5): not in v1; R7 for Pergola plain-text fields
   only, judged on a typing corpus.** *Recommend yes.* It is the only behaviour in the scene and
   should earn its place with numbers.
6. **Compressed video passthrough for remote outputs (§7.1, path 1)**, a new video-layer mode
   the player uses only when its window is on a remote output. *Recommend yes,* in R6. It is the
   difference between sending a film at its own bitrate and re-encoding it at a worse one.
7. **Keyboard: host layout by default, "viewer layout" as a per-session option (§9).**
   *Recommend yes.*
8. **Audio in v1 (output only); microphone as a separate grant, off by default.** *Recommend
   yes.*
9. **Apps are not told that an output is remote** (they see its scale, size and refresh like
   any monitor). *Recommend yes:* nothing an app should do differently depends on it, and
   telling them invites apps that behave differently when watched. The one exception is
   the player's passthrough choice (decision 6): frescod offering compressed passthrough for a
   window *does* tell the player that window is on a remote output. That is a named exception,
   limited to apps holding the media-player role.
10. **Viewer cache on disk in the persona's key group, 1 GiB default, with a switch for memory
    only.** *Recommend yes.* Reconnects and repeat sessions are near free with it.
11. **A dropped viewer keeps its outputs for 2 minutes** before windows move home.
    *Recommend yes.*
12. **QUIC (quinn + rustls with raw public keys pinned over SSH) rather than TCP or Stoa's raw
    envelope**, with the rustls crypto provider (aws-lc-rs or ring) picked by the licence audit
    in R0. *Recommend yes,* subject
    to the licence audit of the crates and their dependencies.
13. **The name Fenestra** (`fenestrad`, `fenestra-net`, `fenestra`). *Recommend yes;* NAMING.md
    gets the row once accepted.
14. **Input made while the link is down is discarded, not replayed on reconnect** (§8.6).
    *Recommend yes.* Replaying keystrokes and clicks into a screen the person could not see is
    how you delete the wrong thing; mosh replays because a terminal line is forgiving, a GUI is
    not.
15. **3D: Vulkan surfaces go as pictures by default; viewer-side 3D comes through retained 3D
    scene nodes (R11); remoting Vulkan commands to the viewer is a later opt-in per host and per
    app, LAN only, for apps that declare no GPU readbacks** (§7.2a). *Recommend yes.* Remoted
    Vulkan caps frame rate by RTT and stalls on readbacks, and it puts the host one driver bug
    from the viewer's kernel; retained 3D gets the benefits without either.
16. **Decided by Shankar (2026-11-26): the portable viewer for macOS and Linux comes first**
    (§14), because development runs Atrium in a VM. R1 moves the renderer core into a portable
    crate; R2's first viewer is `fenestra-view`, headless in the cloud gates and windowed on
    the Mac; the Atrium viewer follows on the same core.

## 18. Out of scope, and honest limits

- **App round trips stay round trips.** A click whose result the app must compute (opening a
  menu that the app builds on demand, a search) costs one RTT plus app time, like every other
  protocol. Fenestra removes everything else from that path, but it cannot beat the speed of
  light. Over 150 ms links, rootless mode, local scrolling and (later) typing prediction carry
  the experience.
- **Pixel-heavy apps** (games, 3D, video editors' previews) are as good as the fallback
  encoder: game-streaming quality, not better.
- **First connection to a new host** costs the session's blobs (estimated a few MB); on a slow
  link the first screen is slower than RDP's first frame, and every later one is faster.
- **Remote Vulkan command streams** as a default (§7.2a explains why not; opt-in later), **foreign
  desktops**, **USB and smartcard redirection**, **multi-viewer collaboration** with several
  people controlling one session at once (a shared view is supported; shared control needs
  input arbitration), and **file transfer** beyond the R8 drag are not designed here.

## 19. Build plan

Each step has a gate script (sourcing `scripts/lib/gate.sh`) and runs in cloud VMs first; Mac
VM runs only with the coordinator's go.

| Step | What | Gate |
|---|---|---|
| R0 | This spec reviewed and decided. New crates licence-audited. **Done 2026-11-26** (§19a). | review thread sign-off; audit 0 failing |
| R1 | **In progress:** portable crate, stream writer and reader, randomised equality test and fuzzer done (§5.6); recorder and gate next. The renderer's core moved into a portable crate (§14.1). Scene stream writer in fresco-renderer (canonical records, versions, acked diff) and a stream reader that applies it in a second renderer; no network. Fuzz the reader with the split fuzzer's harness. | `fenestra-stream-check.sh`: a recorded Pergola, Navigator and Officina session replayed through writer and reader draws bit for bit the same as local (compute renderer), with random drops of acks |
| R2 | `fenestrad`, `fenestra-session` and `fenestra-net` with QUIC, pinned keys over SSH, remote outputs in frescod, whole-desktop mode, input, cursor, stats. **The first viewer is `fenestra-view` on Linux and macOS** (§14): the gate runs it headless in the cloud container against `cloud-vm.sh`'s VM, with `netem` on the forwarded path; Shankar runs it windowed on the Mac against the Mac VM. | `fenestra-desktop-vm.sh`: connect, type, click, scroll, idle (zero bytes asserted), disconnect, reconnect within grace (bytes asserted small), revoke; click-to-frame at 0, 50, 150, 300 ms added RTT, 3 runs each; the §8.6 profiles under `netem`: 1 Mbit/s, 2 % and 10 % loss, 30 ms jitter, a 5 s and a 3 min blackout (no input replayed, resume bytes asserted small), an address change mid-session, UDP blocked (TCP fallback) |
| R3 | Security gate: hostile host (fuzzed streams into the viewer's renderer and `fenestra-net`), hostile viewer (bounded input, view-only sends none), jails and uids checked from the kernel, host indicator present, host strip present, clipboard by paste only. | `fenestra-security-vm.sh` |
| R4 | Rootless mode: remote windows as local windows, local move and resize (§6.4), popups, host strip, dock badge. | `fenestra-rootless-vm.sh` |
| R5 | Viewer-scrollable SCROLL groups in frescod (local and remote), Pergola ScrollView and Navigator documents set the flag. | `scroll_bench` locally; scroll latency at 150 ms RTT in the R2 rig |
| R6 | Fallbacks: pixel-mode regions (lossless and video switching), video layers (re-encode, then compressed passthrough), external surfaces; `fenestra-enc` jail; audio. | encoded-region quality (final lossless refresh asserted), AV sync, bitrate follows `netem` rate changes |
| R7 | Typing prediction for Pergola plain-text fields. | typing corpus: share of keystrokes shown before the host frame, misprediction rate |
| R8 | Clipboard both ways via Tabula, drag of files into a remote app via Scrinium. | `fenestra-clipboard-vm.sh` |
| R9 | Pairing without SSH (Arca device keys), mDNS discovery, Concursus for NAT. | pairing and revocation gate |
| R10 | Real hardware: two Atrium machines on a LAN and over a real WAN; R9700 host for surfaces. | numbers for §6.1, 3 runs per arm |
| R11 | Retained 3D nodes in the scene stream (after the local renderers draw them), with local camera control on the viewer. | a model viewer orbited at 300 ms RTT with no round trip per frame (asserted by stream bytes) |

## 19a. R0 result: licences of the new crates (2026-11-26)

`scripts/licence-audit.py` run on probe crates holding exactly Fenestra's planned
dependencies (latest compatible versions on 2026-11-26), then the bundled C and assembly
sources read by hand, because crates.io's SPDX field does not describe vendored code.

| Group | Crates | Audit | By hand |
|---|---|---|---|
| Transport, ring provider | quinn 0.11.12 (quinn-proto 0.11.19, quinn-udp 0.5.16), rustls 0.23.46 (ring), rustls-webpki 0.103.17, ring 0.17.14, tokio, ed25519-dalek 2, spake2 0.4.0, hkdf, hmac, sha2 | 97 versions, 0 failing | ring: new code ISC, BoringSSL-sourced code Apache-2.0, once_cell polyfill MIT/Apache; no file carries the old OpenSSL or SSLeay licence. |
| Transport, aws-lc provider | quinn, rustls (aws_lc_rs), aws-lc-rs 1.19.0, aws-lc-sys 0.47.0 | 74 versions, 0 failing | aws-lc: OpenSSL- and SSLeay-derived code is now under Apache-2.0 (its LICENSE), the rest ISC, Apache-2.0 or MIT; bundles jitterentropy (BSD-3-Clause, dual with GPL-2.0: we take BSD). Permissive, but larger, needs CMake for the C build, and carries more third-party code. |
| Viewer window and audio | winit 0.30.13, softbuffer 0.4.8, cpal 0.18.2 (coreaudio-rs 0.14.2 on macOS) | 222 versions, 0 failing | **cpal on Linux always links the system `libasound`, which is LGPL-2.1** (alsa-sys is MIT, the library it binds is not). Not acceptable for anything we distribute. |
| Media | opus 0.3.1 (audiopus_sys 0.2.2 bundling libopus), zstd 0.14.5 (zstd-sys bundling zstd 1.6.0), rav1e 0.8.1 | 82 versions, 0 failing | libopus BSD-3-Clause; zstd BSD-3-Clause and BSD-2-Clause; rav1e BSD-2-Clause plus the Alliance for Open Media Patent License 1.0 (royalty-free, with the usual defensive termination). |

Choices taken from this:

1. **rustls crypto provider: ring.** Both pass; ring is smaller, has no CMake step for the
   Tier 3 FreeBSD cross-build, and has fewer bundled third parties. aws-lc-rs stays the
   fallback if ring lacks something we need (it has everything Fenestra uses: X25519, Ed25519,
   AES-GCM, ChaCha20-Poly1305, HKDF).
2. **Linux viewer audio: not cpal.** `fenestra-view` on Linux plays audio through PipeWire
   directly (libpipewire is MIT), and headless mode has no audio at all. macOS uses cpal or
   coreaudio-rs (CoreAudio is the OS's own). The licence of the `pipewire` Rust crate and its
   dependencies is audited when R6 adds audio.
3. **Video encoder: rav1e** (BSD-2 plus AOM patent licence), as §7.4 says; SVT-AV1 is not
   added.
4. **Pairing PAKE: spake2** (RustCrypto, MIT or Apache-2.0), for R9.
5. Still to check when each step adds it: whether ring and quinn-udp cross-build for
   `aarch64-unknown-freebsd` with the repo's sysroot (R2, first build), the `pipewire` crate
   (R6), dav1d and libvpx bindings (already used by the player, R6).

## 20. Sources

- Microsoft, *Graphics encoding over the Remote Desktop Protocol* (Azure Virtual Desktop docs):
  https://learn.microsoft.com/en-us/azure/virtual-desktop/graphics-encoding
- Microsoft, MS-RDPEGFX, RemoteFX progressive codec:
  https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/f8c3c134-bf99-45eb-be7e-d22c8178255f
- Microsoft, *RDP 10 AVC/H.264 improvements* (AVC444 and text):
  https://techcommunity.microsoft.com/t5/microsoft-security-and/remote-desktop-protocol-rdp-10-avc-h-264-improvements-in-windows/ba-p/249588
- Palo Alto Unit 42, *CVE-2019-0708 (BlueKeep)*: https://unit42.paloaltonetworks.com/cve-2019-0708-bluekeep/
- Check Point Research, *Reverse RDP attack*: https://research.checkpoint.com/2019/reverse-rdp-attack-code-execution-on-rdp-clients/
- Kaspersky, *37 vulnerabilities in open source VNC systems* (2019):
  https://www.kaspersky.com/about/press-releases/kaspersky-uncovers-37-vulnerabilities-in-open-source
- SPICE: *Spice for Newbies*: https://www.spice-space.org/spice-for-newbies.html; Red Hat bug
  1158029 (WAN latency, WONTFIX): https://bugzilla.redhat.com/show_bug.cgi?id=1158029;
  spice-devel, *Remote 3d support - streaming and lag* (2017):
  https://lists.freedesktop.org/archives/spice-devel/2017-February/035578.html
- K. Packard, J. Gettys, *X Window System Network Performance* (FREENIX 2003):
  https://static.usenix.org/events/usenix03/tech/freenix03/full_papers/packard/packard.pdf;
  Linux Journal, *The Arrival of NX*: https://www.linuxjournal.com/article/8480
- Waypipe man page: https://www.mankier.com/1/waypipe; announcement:
  https://lists.freedesktop.org/archives/wayland-devel/2019-June/040687.html
- Parsec, *A networking protocol built for the lowest latency interactive game streaming*:
  https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007
- K. Winstein, H. Balakrishnan, *Mosh: An Interactive Remote Shell for Mobile Clients*, USENIX
  ATC 2012: https://mosh.org/mosh-paper.pdf
- RFC 9000 (QUIC), RFC 9221 (QUIC datagrams), RFC 7250 (raw public keys in TLS), RFC 8446 (TLS 1.3).

## 21. Review log

2026-11-26, review thread, on ff028ac. All folded in:

- High: rustls has no external TLS 1.3 PSK; the binding is now raw public keys pinned over SSH
  plus a `K_sess` proof tied to the TLS exporter, 0-RTT off (§11.2). Remote presses no longer
  satisfy authority-granting sheets, and scoped input cannot leave its scope (§9). Viewer
  clipboard released only right after a forwarded paste gesture (§10). Glyph text travels as
  app-made atlas textures, not fonts (§5.4).
- Medium: mint checks caller uid, session owner and persona; headless needs the key group
  (§11.2). Licence questions on aws-lc-rs, ring and SVT-AV1 made explicit (§7.4, §8.1,
  decision 12). Viewer, session and backlog bounds (§11.2). Cache keyed by the verified SSH host
  key and re-hashed from disk (§5.4). Local screen blanked by default while viewed (decision 3).
- Low: what `fenestrad` stores and reconnect after its restart (§11.2); passthrough named as
  decision 9's exception; §6.1 marked as structural, not a measurement.

2026-11-26, review thread, security-only pass on 95c7d92 (asked by Shankar). Verdict: sound once
the three high findings are fixed. All folded in:

- High: network parsing moved out of `fenestrad` into a per-session `fenestra-session` jail
  holding one session's keys (§3.2, §11.3, §11.8, §11.9). Remote presses count only for an
  allowlist of ordinary purposes, so new sheets deny by default; headless hosts need a key touch
  (§9). The FIDO relay has the viewer build the RP id and client data from the pinned host
  identity, taking only a nonce and a sheet id (§9).
- Medium: full screen keeps an edge strip and a never-forwarded secure attention key (§9).
  Pairing starts only from a seat press, the code shows only on seat monitors, 50-bit code with
  a PAKE and 3 attempts (§11.2a). Scopes spell out sheets, popups, notifications and audio (§9).
  `K_sess` handling corrected: held in memory by the session jail, crosses only inside SSH
  (§11.2, invariant 5). Clipboard released only to a request citing a forwarded paste gesture's
  input sequence (§10).
- Low: handshakes counted after Retry validation (§11.10); names in host chrome bounded and
  sanitised from the host's own records (§11.4); TCP and SSH fallback keep the same pinned TLS
  and per-session port (§8.6); remotely raised authority sheets show only on that viewer's
  outputs (§9).
