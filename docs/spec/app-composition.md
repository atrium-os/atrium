# Composing an app: workers, services and the SDK that makes it easy

**Status:** accepted 2026-11-17 (Shankar: "yes, accepted", all §10 decisions
C1–C10 as recommended). Nothing built yet. Direction from Shankar, 2026-11-17: "artifex is probably the best way to do
all of this without overwhelming the developer - and we ensure through the
fuzzing that we point out unsecure build patterns...with some practice and
experience, command-line users can adapt to it to" (§6.1). Companion to [dev-tools.md](dev-tools.md). Nothing here is built yet.

**Asked by Shankar, 2026-11-17:** "just building a program is not enough for
atrium..there are complicated ipc paths - when we built the navigator, you can see
how many parts composed together to build it securely..so a third party developer
needs to compose their apps in a similar way..this is where it becomes difficult
for a developer from the unix world; they would just build monolithic apps and link
to libraries"

Related: [atrium-navigator-backend.md](atrium-navigator-backend.md) §2 (the rule);
[navigator-ui.md](navigator-ui.md) §2.1; [mail.md](mail.md) §4; [atrium-player.md](atrium-player.md)
§2; [portcullis.md](portcullis.md) §6.5.2 (one-shot jails, worker rules);
[limen.md](limen.md); [nuntius.md](nuntius.md); [arca.md](arca.md) §9;
[aqueduct.md](aqueduct.md), [aqueduct-services.md](aqueduct-services.md);
[atrium-bundle-format.md](atrium-bundle-format.md); [insula.md](insula.md) §2.3, §6.

---

## 0. In short

A Unix developer writes one process and links libraries: `libcurl` for the
network, `libpng` for pictures, `libxml2` for documents, a keyring library for
passwords. In Atrium that one process would hold the app's grants **and** parse
whatever the network and files hand it, which is the combination our own apps are
built to avoid. The Navigator is nine kinds of process for that reason.

We can't expect third parties to rediscover that design. So the proposal is to make
the safe shape the easy shape, at three levels, most apps needing only the first:

1. **Use a system service instead of a library** (§3). Fetching, file picking,
   secrets and sign-in, showing web content, documents, PDFs, media: Atrium already
   runs each of these in its own jails. An app that uses them parses nothing
   hostile itself and needs no workers at all.
2. **Declare workers in the manifest** (§4). For the parsing an app must do itself
   (its own file format, a protocol), it lists `[[workers]]` in `atrium.toml` and
   calls them through the SDK as if they were functions: `parse.call(fd)?`. Portcullis
   runs each call in a throwaway jail with no capabilities. Today this needs a
   separately signed app id per worker plus root-owned policy, which no third party
   can do.
3. **Provide a service or a role** (§5). An app that serves others (a Limen role, an
   Aqueduct service) declares it and gets typed messages from a schema, not
   hand-written opcodes.

Around that: templates for the common app shapes, `cargo atrium check` that tells a
developer *which* code is in the wrong process, a composition view in Artifex, and
fuzz harnesses for free, because every worker is a function from bytes to a message
(§6). Monolithic ports still run, jailed as one app, and are labelled
honestly (§7).

## 1. How our own apps are built

The rule, from atrium-navigator-backend.md §2: **holding capabilities and parsing
hostile input never happen in the same process.** Every app we built follows it:

| App | Trusted core (holds grants, parses nothing hostile) | Workers (parse, hold nothing) | System services used |
|---|---|---|---|
| Navigator | navigatord (policy, store, sessions), navigator-ui (chrome, no capabilities) | document worker per document, converter per page, image decoding | Nuntius fetcher, Arca couriers and jars, Limen `doc-viewer`, frescod renderers, media-player |
| Mail (spec; partly built) | mail-engine (store, index), the UI | `mail-parse` per batch of 64 messages | Arca mail courier (TLS and login), navigator-normalize + `doc-viewer` for HTML, Nuntius for remote pictures |
| Player | supervisor (Limen, Fresco, Lyra connections) | decode worker, `cap_enter` after setup, rlimits, watchdog | Limen `media-player` jail per embed |
| Officina | the app | `officina-convert` per file, `cap_enter`, CPU and memory limits | Scrinium picker |
| Folium PDF | the app | parser with a read-only descriptor, no files, no network | Limen `pdf-viewer` |

And how they do it today, which is the problem for anyone else:

- **Separate bundles and root's policy.** Each Navigator part is its own signed
  bundle with its own app id; the install scripts make system uids with `pw` and
  append `[workers."…"]` rules to the root-owned system policy
  (`scripts/navigator-*-bundle.sh`). A third party can do neither.
- **Hand-made sandboxes.** Officina, Mail and the Player start a child with
  `Command::new`, and the child calls `cap_enter` and sets rlimits itself; about 25
  binaries repeat that code. Mail's parse "jail" is a Capsicum child, not a jail
  (mail.md §16 M2 says so).
- **Hand-made protocols.** Every pair invents its framing (line protocols, postcard
  frames, length prefixes). Aqueduct has no schema or code generation; service
  opcodes are written by hand (aqueduct.md §10).
- **Mechanisms not reachable from a manifest.** `ExecInstance` needs portcullisd's
  socket, which only restricted capabilities mount. No ordinary capability mounts
  limend's socket (only `navigator-broker` does), so a third-party app can't ask for
  a `doc-viewer` or a picker. `[roles] implements` isn't parsed; limend takes role
  providers from an environment variable. Arca's app API has no manifest capability.
  Nuntius has no C ABI yet. The manifest parser rejects every section beyond `app`,
  `bundle`, `capabilities`, `setup`, `resources`, `supervision`, `opens` and `links`.

## 2. The model for developers

One sentence for the guide: **an Atrium app is a small core that holds the grants,
plus workers that touch untrusted bytes, plus system services for everything the
system already does.**

- **Core.** The process the manifest's capabilities apply to: the window, the app's
  own files, the app's state. It should read only data it wrote itself or that a
  worker has turned into checked, bounded messages.
- **Worker.** A function in its own jail: bytes in (a descriptor or a message),
  a typed result out. No capabilities, no files, no network, no display, CPU and
  memory limits, killed when done. A crash or a hang is an error value in the core,
  not a compromise.
- **Service.** Something the system or another app runs, reached by a capability
  or a Limen role. The app never sees how it is jailed.

What counts as untrusted: anything from the network, any file the person picked
(it may have come from anywhere), anything from another app, and anything a past
version of the app wrote that a newer one parses.

## 3. Level 1: a service instead of a library

The single biggest lever. The guide's first page is this table:

| On Unix you'd link | In Atrium you use | Capability or role | Built? |
|---|---|---|---|
| libcurl, reqwest, an HTTP client | **Nuntius**: the app asks for bytes, a jailed fetcher with address policy fetches them | `net-fetch` (+ `net-fetch-sites`) | yes (Rust); C ABI not yet |
| `open()` on a path the user typed | **Scrinium picker**: the person picks, the app gets a descriptor | Limen `picker` | yes; not reachable from a third-party manifest yet |
| libsecret, a keyring, storing a token | **Arca**: couriers make the authenticated request; keys never leave Arca | Arca API (§4.2) | couriers yes; app capability no |
| an OAuth library, a login form | **Vestibulum / Arca** sign-in role | Limen `sign-in`, `credential` | partly |
| WebKit, an HTML or Markdown renderer | Limen **`doc-viewer`** (navigator-normalize in a jail) | Limen role | yes |
| ffmpeg, a video player | Limen **`media-player`** | Limen role | yes |
| poppler, pdfium | Limen **`pdf-viewer`** | Limen role | yes |
| libpng, libjpeg, image crates | the **image decoder** `org.atrium.image-decode` (navigator-image in a jail of its own per call, as `_imagedec`); the SDK's `atrium_assets::image(name, w, h)` / `decode(file, w, h)` | `decoders = ["image"]` | yes (C6, dev-tools.md §3.5 step 3) |
| a terminal widget | Terminal as a Limen embed | role | partly |
| a fonts / text-shaping library | the system text stack (server-side shaping in frescod) | `graphics` | yes |

An app built only from these, with its own logic in the core, is already composed
the Atrium way without the developer drawing a single process boundary. A mail
client, a feed reader, a chat client, a podcast player, a document viewer are all
mostly this.

To make it reachable (all small, all needed before anyone can follow the guide):

- an ordinary capability **`embeds`** that mounts limend's socket, with the roles the
  app may request listed (`embeds = ["picker", "doc-viewer", "media-player"]`); the
  consent diff names them (C7);
- **`arca`** as an app capability for its API, scoped to the app's own items (C8);
- **`decoders`** (C6, built 2026-11-25): `decoders = ["image"]` mounts portcullisd's
  worker-call socket, where `SystemWorker { name }` starts the system's decoder for
  that name (`portcullis_ipc::SYSTEM_WORKERS`) in a one-shot jail with no capability,
  as the system worker rule's uid, never the app's. It asks no consent: the decoder
  holds nothing, and the app gives it only the file it sends. portcullisd refuses it
  to a worker and to an app whose installed manifest doesn't name it. Checked by
  `scripts/vm-image-decode-check.sh`;
- the Nuntius **C ABI** (nuntius.md step 5), and `libatrium` gaining fetch, picker,
  embed and Arca calls, so C apps have the same level 1.

## 4. Level 2: workers declared in the manifest

### 4.1 The manifest

```toml
[app]
id = "com.example.photos"

[bundle]
entry = "bin/photos"

[capabilities]
graphics = "fresco"
net-fetch = true
embeds = ["picker"]

[[workers]]
name   = "decode"              # called by the core as "decode"
entry  = "bin/photos-decode"   # in the same bundle, same signature
input  = "fd"                  # fd | message
lifetime = "call"              # call: a fresh jail per call
                               # session: one jail, many calls, bounded
memory = "512M"
cpu    = "10s"                 # per call
# capabilities: none, always (C3)

[[workers]]
name   = "index"
entry  = "bin/photos-index"
input  = "message"
lifetime = "session"
```

- **One bundle, one signature, one consent.** Workers are entries in the same
  bundle; the person consents to the app, and workers add nothing to consent because
  they get nothing (C1, C3).
- **Portcullis derives the worker rule from the manifest**, as it now reads
  `[workers."id"]` from root's system policy for our bundles: only this app's core
  may start its workers, each worker runs as its own uid, allocated per (person,
  app, worker) like an app's (C2), so a compromised worker can't read the core's
  overlay or another worker's output.
- **Limits** come from the manifest, clamped by the per-app pool already in the
  schema (`[resources] pool`) and the `ExecInstance` rate limits (32 per person, 8
  per second).
- `ExecInstance` stops accepting arbitrary app ids from callers: an app may start
  only its own declared workers, and system bundles keep their system-policy rules.

### 4.2 The SDK side (Rust)

```rust
use atrium::worker;

// In bin/photos-decode: the worker. Bytes in, a checked result out.
#[worker::main]
fn decode(input: worker::Fd) -> Result<Image, DecodeError> {
    let bytes = input.read_bounded(64 << 20)?;
    Ok(Image::decode(&bytes)?)          // any crate you like; it's caged
}

// In the core:
let decode = atrium::workers::get::<Decode>("decode")?;
let img: Image = decode.call(picked_fd)?;   // a jail started, used, gone
```

The crate (`atrium-worker`, in the SDK) does what each of our apps does by hand
today: framing (postcard, sizes bounded, `Image` checked on arrival by a generated
`check()` like mail-schema's), starting the jail through portcullisd, passing the
descriptor, the timeout, turning a crash, a limit or a timeout into an `Err`, and,
inside the worker, `cap_enter` after setup and the rlimits. Types shared by core
and worker live in one crate, so the boundary is a function signature.

- **Same code, three modes.** `ATRIUM_WORKERS=inproc` runs workers as plain
  function calls (unit tests, debugging with breakpoints across the boundary);
  `capsicum` forks a Capsicum child as Officina and the Player do now (macOS Insula
  host, and the cheap path for many tiny calls, C4); `jail`, the default on Atrium.
- **C.** `atrium_worker_call(w, "decode", fd, &reply, &len)` and
  `ATRIUM_WORKER_MAIN(decode)`, with messages described by the schema of §5.2.

### 4.3 Patterns the guide teaches (each with a template)

| Pattern | Shape | Our example |
|---|---|---|
| **Viewer** | core + parse worker per file, picker for opening | Officina, Folium |
| **Networked client** | core + Nuntius for bytes + Arca courier for logins + parse worker per response | Mail |
| **Document with rich content** | core + `doc-viewer` embed for HTML/Markdown | Mail's HTML, help pages |
| **Media** | core + `media-player` embed, or supervisor + decode worker | Player |
| **Long-running indexer** | core + `session` worker that receives messages and returns results | Mail's engine (to be split) |
| **Plugins** | each plugin a worker or a Limen child; the core never loads plugin code | Artifex extensions |

## 5. Level 3: providing services and roles

### 5.1 Declarations

```toml
[roles]
implements = ["share-target"]       # limen.md §3.1, parsed at last

[ipc]
provides = ["com.example.photos.library/1"]   # an Aqueduct service
uses     = ["org.atrium.notify/1"]
```

limend learns providers from installed manifests instead of `LIMEN_DEFAULTS`;
third-party role providers get the system worker rule they need generated, as in
§4.1. Vendor roles (an app defining a new role) stay later (limen.md).

### 5.2 Typed messages instead of hand-written opcodes

Aqueduct moves frames; every service today writes its opcode table by hand, and
there's no answer for other languages (aqueduct.md §10). Proposed (C5):

- **Schema in Rust types** with a derive (`#[derive(atrium::Message)]`) that gives
  postcard encoding, a bounds-checking `check()` (lengths, counts, recursion depth),
  a version number, and an exported schema file.
- **`atrium-schema`** generates the C header and encoder/decoder from that file, so
  a C app or a C service speaks the same messages; other languages can follow from
  the same file.
- Worker calls (§4) and services (§5) use the same messages, so a developer learns
  one thing.

## 6. Tooling that makes it tractable

- **Templates.** `cargo atrium new --viewer | --client | --media | --service` and
  the C equivalents produce a working core, worker and manifest, already split.
- **`cargo atrium check` finds the wrong process.** It reads the crate graph of the
  core binary (for C, the linked libraries) and flags parsers of hostile input
  linked into a process that holds capabilities: image, archive, XML, HTML, PDF,
  font, media and serialization crates from a curated list, plus `net` crates when
  the app uses `net-fetch`. It says *where* to move them: "`photos` holds
  `net-fetch` and links `image` 0.25: decode in a worker (template: viewer)". It
  also lists capabilities declared but never used. Warnings, not errors (C9).
- **Composition view in Artifex.** For a running dev build: the core, its workers,
  their uids, capabilities, limits and channels, from portcullisd's launch registry;
  calls, crashes and limit hits as they happen.
- **Fuzzing for free.** Every worker is a function from bytes to a result, so
  `cargo atrium fuzz decode` generates a cargo-fuzz target for it. That's the
  payoff of the shape: the code that touches hostile input is exactly the code that
  is fuzzable.
- **Run handles workers.** W5's dev bundle carries worker entries and derives their
  rules and uids like an installed bundle, so a developer sees the real composition
  from the first Run.
- **The guide**: the rule, the level 1 table, the patterns, and three worked examples
  (a viewer, a networked client, a service), taken from Officina, Mail and the Player.

### 6.1 Artifex is the front door (Shankar, 2026-11-17)

The developer meets all of this through Artifex, not through the manifest and the
tool list. The command line (`cargo atrium`, `atrium-bundle`, the manifest by hand)
stays the same engine underneath, for people who prefer it and learn the shape with
practice; Artifex never has a capability the CLI lacks.

- **New project** asks what the app does (opens files, talks to a server, plays
  media, serves others) and starts from the matching template, already split into
  core, workers and services.
- **The composition is visible while editing**: a panel shows the core, its
  workers and the system services it uses, with the capabilities each holds; the
  manifest is edited through it, with the consent text the person will see.
- **Fuzzing runs in the background** in the workspace (`WorkspaceExec`, low
  priority, within the workspace's limits), and its findings are how insecure
  patterns get pointed out:
  - every worker gets a fuzz target generated from its signature and is fuzzed
    continuously; a crash is a bug report with the input, but contained;
  - **the core's input surfaces are fuzzed too**: what it reads from picked files,
    from Nuntius responses and from other apps. A crash or a limit hit *there* is
    flagged as the insecure pattern itself, not just a bug: "this hostile input
    crashed the process that holds `net-fetch` and your files", with the code path
    and a **Move into a worker** action that creates the worker, its message types
    and the call from the template;
  - the crate-graph check of §6 runs on each build and shows its findings in the
    same panel, so a parser linked into the core is flagged before any fuzzer
    finds it.
- **Run, debug and fuzz share one view**: a dev build's workers appear in the
  composition panel as they start, crash or hit limits.

Build order effect: Artifex v0 (dev-tools.md T9) gains the composition panel and
the template-backed New project; the fuzz panel follows A6. The CLI pieces land
first because Artifex runs them.

## 7. Monolithic ports

A ported Unix program that links everything still runs: the jail is its boundary,
and it gets only the capabilities in its manifest. What it loses compared with a
composed app is containment *inside* the app: a parser bug gives the attacker that
app's grants and data. Proposed (C10):

- Allowed, with no penalty in the store or at install.
- The app's details page (Curia, Opifex) says plainly which of the app's grants
  are held by the same process that reads downloaded or opened files, from the same
  analysis as `cargo atrium check`.
- The cheapest wins for a port are level 1 shims: a `libcurl`-shaped API over
  Nuntius, and open/save dialogs that map to the picker, so a port moves its
  network and file reach out without restructuring.

## 8. Build steps

| Step | What | Gate |
|---|---|---|
| **A1** | Manifest: parse `[[workers]]`, `[roles]`, `[ipc]`; `embeds` and `arca` capabilities in the consent diff | old manifests unchanged; new keys round-trip; unknown keys still refused |
| **A2** | portcullisd: workers from the bundle's manifest, per-(person, app, worker) uids, generated rules, `ExecInstance` limited to own workers | a third-party test app starts its worker; can't start another app's worker or a system bundle; the worker can't read the core's overlay |
| **A3** | `atrium-worker` crate (jail, capsicum, inproc), moving Officina's converter and Mail's parser onto it | Officina and Mail pass their gates with workers as real jails |
| **A4** | limend reachable by `embeds`; providers from manifests | a third-party app opens a picker and a `doc-viewer`; one not declaring them is refused |
| **A5** | `atrium::Message` derive, `atrium-schema` C generation, C worker calls | a C app calls a Rust worker and back |
| **A6** | Templates, `cargo atrium check` (crate-graph lint), `fuzz`, Run with workers | the lint flags `image` in a networked core; the viewer template builds, runs and fuzzes |
| **A7** | Arca app capability; Nuntius C ABI; libatrium level 1 calls | a C client fetches through Nuntius and logs in through a courier |
| **A8** | Artifex composition view; the developer guide | |

A1–A3 are the core of it and follow dev-tools.md's T5 (Run closure).

## 9. Things noticed while surveying

- portcullisd's `ExecInstance` (`portcullisd/src/main.rs` around line 318) refuses
  only Nuntius and the CDM host by name; for any other app id it applies that app's
  manifest with the caller's uid unless a system worker rule says otherwise. Only
  restricted capabilities reach the socket today, so this isn't open to third
  parties, but A2 should close it on purpose rather than by that accident.
- `portcullis-toml`'s comment says unknown top-level sections are accepted while
  the struct denies them; limen.md's status header still says nothing is built.
- nuntius.md writes `net.fetch` in §2 where the key is `net-fetch`.

## 10. Decisions for Shankar

| | Decision | Recommendation |
|---|---|---|
| C1 | Workers: entries in the app's own bundle, or separate signed bundles as ours are now | in the app's bundle, one signature and one consent; our system bundles may move to it later |
| C2 | Worker uid | its own per (person, app, worker) |
| C3 | May a worker hold capabilities? | no, never; anything with reach is the core or a system service |
| C4 | Default isolation for a worker call | a jail; Capsicum-only fork allowed per worker (`isolation = "capsicum"`) for many tiny calls |
| C5 | Typed messages | Rust types with a derive, a schema file, C generated from it; no separate IDL language |
| C6 | Image decoding as a system service | yes, from navigator-image, as a role or service any app can call |
| C7 | Third-party access to Limen | `embeds` capability naming the roles; providers declared by `[roles] implements` |
| C8 | Arca for third-party apps | an `arca` capability for the app's own items, couriers for requests |
| C9 | `cargo atrium check` and fuzz findings | warnings to the developer, shown in Artifex (§6.1), not install blockers |
| C10 | Monolithic ports | allowed, with the details page saying which grants sit beside the parsers |
