# Nuntius: the system's sandboxed fetcher

> *Nuntius* (Latin: messenger). The Navigator's jailed fetcher (`navigator-fetch`,
> `navigator-fetchd`), made into a system component any app can use to fetch a URL.

**Status:** accepted by the user, 2026-11-06 (name and interface, all recommendations).
Steps 1 and 3 are built inside `navigator-fetch` (§6.1). Step 2, the `nuntius` crate and
`nuntiusd` on an Aqueduct channel, was built 2026-11-07 (§6.2). So was step 4's launch,
portcullisd's `OpenFetcher` and the `net-fetch` capability (§6.3), but it has not yet been
run on FreeBSD. Wiring the standalone player and step 5 (the C ABI) are not built. Owner:
the Navigator thread.

**Asked by the user, 2026-11-06:** "the navigator's sandboxed fetcher is the right one... used
as a widget, third party apps can use it too." The same day, the user chose that the media
player's jail gets **no network** (limen.md §2.2, `Source::Url` removed in 8ba526f). So the
Navigator, the standalone player and later third-party apps all need one fetcher they can
ask for bytes.

Related: [atrium-navigator-backend.md](atrium-navigator-backend.md) §2 (trust topology) and
§8 M1 (the fetcher gate); [atrium-navigator-threat-model.md](atrium-navigator-threat-model.md)
§5 (network reach); [arca.md](arca.md) §5 (courier-attached secrets);
[limen.md](limen.md) §2.2 (media-player feeds); [fresco-video.md](fresco-video.md) P3.

---

## 1. What exists, and what it already guarantees

`navigator-fetchd` today (`navigator-fetch/src`):

- **The only networked process in the Navigator, and one that cannot name a file.** On FreeBSD
  `sandbox::enter` puts it in Capsicum capability mode with a casper `cap_net` channel as its
  only way out.
- **Address policy checked between resolve and connect** (`addr::Policy`, `Conversation`):
  every DNS answer is classified and pinned before a socket is opened. Loopback, RFC 1918,
  link-local, CGNAT, ULA and the metadata endpoint are refused unless a private-network grant
  is given (never the metadata endpoint). That stops LAN probing and DNS rebinding.
- **Bounded work:** rustls only, at most 5 redirects, a 20 s timeout, 64 KiB of headers, 8 MiB
  of body per response, a byte budget for prefetch hints, at most 6 idle connections per origin.
- **Answers are checked, not believed:** a 206 must start where it was asked (`Ranged::from`).
- **No cookies or credentials.** A request that needs a bearer secret is made by Arca's
  courier, which presents it after TLS to the bound endpoint (arca.md §5); the fetcher never
  holds one.
- **One instance per conversion**, so pins and connection reuse never cross conversions.

The wire is a line protocol on stdin/stdout (`serve_with`: `GET`, `H`/`HH` hints, `P` peek,
`B` budget). That is fine for the converter, but it carries only whole bodies under 8 MiB,
with no streaming, no cancel and no descriptors.

## 2. The decision this note proposes

Nuntius is **a per-client jailed instance, not a shared daemon.** Each client (a Navigator
session, a player instance, an app) gets its own fetcher process. portcullisd spawns it in the
fetcher's jail and hands the client one Aqueduct channel to it. Two clients never share pins,
connections, caches or a compromise. This is today's "one per conversion" rule, generalised.

Apps never get raw sockets for this. A manifest capability `net.fetch` lets portcullisd
start a Nuntius for the app. An app that needs raw sockets asks for that separately, and the
user sees the difference (permissions UI).

Everything in §1 is kept as is. Changes are additive: streaming, ranges with resume,
descriptors, cancel, timeouts and retries.

## 3. Interface

Rust crate `nuntius` (client API) over an Aqueduct channel; a C ABI for non-Rust apps. One
channel, many concurrent requests, each with an id chosen by the client.

### 3.1 Requests

```
Open {
  id: u32,
  url: String,                       // http or https only; https by default for apps
  method: Get | Head,                // v1: no bodies upstream, so no POST
  range: Option<(u64, Option<u64>)>, // start, length; None = whole resource
  if_range: Option<Validator>,       // ETag or Last-Modified from an earlier Head
  deliver: Stream | Fd,              // see 3.3
  limits: { max_bytes, connect_ms, idle_ms, total_ms },  // clamped to the instance's caps
  retry: { max_attempts, backoff_ms },                   // clamped; 0 = no retry
  priority: u8,                      // 0 highest; media playhead > prefetch
  accept: Option<String>,
}
Cancel { id }
Budget { bytes }                     // the instance's remaining byte budget
```

### 3.2 Events

```
Head { id, status, final_url, redirects, content_type, length: Option<u64>,
       accept_ranges: bool, validator: Option<Validator> }
Data { id, offset, bytes }           // Stream delivery, at most 60 KiB per event
Fd   { id }                          // Fd delivery: the descriptor travels with this event
Progress { id, received }            // at most 4 per second
Retrying { id, attempt, reason }     // a timeout or reset was hit and a retry started
Done { id, bytes }
Error { id, code: Policy | Dns | Connect | Tls | Http(u16) | Timeout | TooBig | Cancelled | Malformed }
```

`Head` always comes first. `Data` offsets are absolute in the resource, so a resumed request
continues the same numbering.

### 3.3 Delivery

- **Stream** (the default): bytes arrive as `Data` events with flow control. The client
  acknowledges by sending more `Budget`, and the fetcher stops reading the socket when the
  budget is used up. This is how the Navigator feeds a media segment feed (`Append`) and how
  a document arrives.
- **Fd**: for a whole resource the client wants as a descriptor (the media player's
  `Load { source: Fd }`). The fetcher creates an anonymous file (`shm_open(SHM_ANON)` on
  FreeBSD, `memfd` elsewhere), fills it front to back, and sends a read-only descriptor with
  `Fd` as soon as `Head` is known. `Progress` tells the holder how far it is filled. A reader
  that reaches the fill point waits for more `Progress`. The file is sized to the
  `Content-Length` and capped by `max_bytes`; with no length, the client gets `Stream`
  instead. Seeking ahead of the fill point waits for the fill. Long media that needs random
  access beyond what is fetched goes through ranged `Stream` requests and a segment feed, not
  `Fd`.

### 3.4 Timeouts, retries and resume

- `connect_ms` covers DNS, connect and TLS. `idle_ms` limits time with no bytes received.
  `total_ms` is a hard cap.
- On a timeout or reset, the fetcher retries up to `max_attempts` with exponential backoff
  and jitter, sending `Retrying` each time. A retry resumes where the bytes stopped: a Range
  request from the last offset, with `If-Range` set to the validator from `Head`. If the
  resource changed (a 200 instead of a 206, or a different validator), the request fails
  with `Http(412)`-style `Malformed` rather than splicing two versions.
- Policy, DNS-refused, TLS and 4xx errors are never retried. Retries are counted against
  `max_bytes`.
- `Cancel` stops the request at once and frees its connection. A media seek cancels the
  stale requests this way.

### 3.5 Caps per instance (refused or clamped past them)

64 concurrent requests, 6 connections per origin, 256 MiB in flight, `max_bytes` at most
1 GiB for `Fd` and unbounded for `Stream` (flow control bounds it), `total_ms` at most 10 min
for `Stream` media and 60 s otherwise. Numbers to tune by measurement.

## 4. Who decides what a client may reach

- **Public internet:** any client with `net.fetch`.
- **Private network** (loopback, LAN, ULA): a per-client grant from the user, bound to the
  network it was given on (threat model §5 q2), passed to the instance at spawn. The
  environment-variable grant of today is the stand-in until that exists. The metadata
  endpoint is never reachable.
- **Credentials:** never through Nuntius. A request that needs one is made by Arca's courier
  instead, after TLS to the bound endpoint (arca.md §5), with the same `Open` shape. The
  client never sees the secret.
- **Origins:** an app's manifest may narrow its fetcher to listed origins. The Navigator's
  instances are unrestricted, since the reader chooses where to go.

## 5. Clients, first to last

1. **The Navigator** (documents, subresources, sprites, media feeds for fresco-video P3).
   P3 is the first user of `Stream` with ranges, resume, `Cancel` and retry.
2. **The standalone player** (atrium-player.md). It has no network of its own: its host app
   opens a Nuntius, uses `Fd` for files and `Stream` for feeds, and passes the bytes to the
   player over Limen.
3. **Third-party apps** via `net.fetch` and the `nuntius` crate or C ABI.
4. **Pergola widgets** that show remote content (an image view, DocumentView with remote
   images) take a Nuntius handle from their app rather than fetching themselves.

## 6. Build order

1. Inside `navigator-fetch`, add the request and event types, the `Stream` path with flow
   control, `Cancel`, timeouts, and retry with ranged resume. Use them in P3's media feed.
   Tests run against a fake server that is slow, stalls mid-body, resets and answers 5xx.
2. Split the client API into the `nuntius` crate and the Aqueduct channel. `navigator-fetchd`
   becomes `nuntiusd` with the line protocol kept for the converter, until the converter
   moves to the channel.
3. `Fd` delivery with progressive fill.
4. The portcullisd launch and the `net.fetch` capability. Then wire the standalone player.
5. The C ABI.

The jailed-gate tests (backend §8 M1 gate) run on every step, unchanged.

### 6.1 What is built (2026-11-06)

`navigator_fetch::nuntius` (in-process; no channel yet):

- `Nuntius::new(conversation, Sender<Event>, budget)`, then `request(Request)`, `live()` and
  `budget()`.
- The `Open`, `Cancel` and `Budget` requests, and every event in §3.2, with the field names
  above.
- **Stream** delivery is flow-controlled: reading stops when the budget is spent, and a
  ranged request finishes as soon as its length is in hand. `Data` events are at most 60 KiB
  and their offsets are absolute.
- **Fd** delivery (step 3, fetcher side): a read-only descriptor is sent with `Fd` as soon as
  `Head` is known, then filled front to back.
  - FreeBSD: `SHM_ANON`, with a `cap_rights_limit` read-only dup.
  - Linux: `memfd`, reopened read-only through `/proc`.
  - macOS (development only): an unlinked temp file.
  - With no length, the request falls back to Stream.
  - `navigator_fetch::nuntius::anon_file()` is public.
- Timeouts: `connect_ms`, `idle_ms` and `total_ms`.
- Retry: on a timeout, reset, short body, 5xx, 408 or 429, with doubling backoff and jitter.
  Policy, DNS, TLS and other 4xx errors are final.
- Resume: `Range` from the last byte, with `If-Range`. A 200 or a changed validator fails
  `Malformed`, never a splice.
- `Cancel` shuts the socket at once.
- Caps: 64 live requests, at most 1 GiB for `Fd`, `total_ms` at most 10 min. The connect,
  idle, attempt and backoff values are clamped.
- Every fetchd guarantee holds, because a request goes through the same `Conversation`: the
  address policy is checked between resolve and connect, addresses are pinned against
  rebinding, redirects are capped at 5 and refused on a downgrade, and 206 responses are
  checked. The converter's line protocol (`serve_with`) is unchanged.

**Not built:**

- 6 connections per origin, 256 MiB in flight, and the 60 s total for non-media requests.
- The `nuntius` crate, the Aqueduct channel, `nuntiusd`, the portcullisd launch, `net.fetch`
  and the C ABI.

The standalone player can wire to the in-process API today (`navigator-fetch` as a
dependency). The types are the ones §3 names, so moving to the channel is a transport change,
not an interface change.

**Tests.** `navigator-fetch/tests/nuntius.rs` has 15 tests against a scriptable loopback
server: slow, stalled mid-body, dropped, 5xx and 4xx, a changed resource, flow control, an
exact-credit resume, cancel, connect timeout, range checks, Fd read-only, the address and
redirect rules, and the concurrency cap. `navigator-media/tests/feed.rs` runs P3's segment
feed through it (fresco-video P3).

### 6.2 Step 2: the crate and the channel (2026-11-07)

- **`nuntius` crate** (`nuntius/`). It has no network code, so an app links it without TLS
  or a resolver. It holds:
  - the §3 types (`Open`, `Request`, `Event`, and so on) and the §3.5 caps;
  - the wire (`wire`): Aqueduct `CLASS_FETCH` (15), postcard payloads, `OP_REQUEST` from
    the client, `OP_EVENT` back, and `OP_EVENT_FD` for `Fd` with its descriptor attached;
  - `Client` (one channel; `split()` gives a sending and a receiving half for two threads);
  - `server::channel`, the fetcher's end.
- **Both ends check what they decode:**
  - URLs are at most 8 KiB, other strings at most 512 bytes, with no control characters;
  - `Data` is at most 60 KiB;
  - no trailing bytes;
  - an `Fd` must arrive with exactly one descriptor, under its own op.
  - Strings the fetcher took from a server are cut to fit, not refused. A client's request
    over the limits is refused before it is sent.
- **`navigator_fetch::nuntius` uses the crate's types**, so the in-process instance and the
  channel share one interface.
- **`serve_channel`** runs one instance for one channel, starting with 1 MiB of credit:
  - requests go in and events come back in order;
  - when the client hangs up, every request is cancelled;
  - a message out of shape ends the channel.
- **`nuntiusd`** (a binary in `navigator-fetch`) serves the channel on its standard input,
  which must be a socket.
  - On FreeBSD it reads the trust store, then enters capability mode with casper `cap_net`
    before its first request, as `navigator-fetchd` does.
  - Elsewhere it refuses unless run with `--unconfined-for-testing`.
  - `navigator-fetchd` and its line protocol stay for the converter.
- **Fixed on the way:** a whole resource of known length now ends at that length. With
  credit spent exactly, a `Stream` request had waited for more credit before it read the
  end of the body, until its deadline. Ranges already ended at their length.
- **Tests:**
  - `nuntius/tests/wire.rs` (3): round trips, refusals, and a lying peer.
  - `navigator-fetch/tests/nuntiusd.rs` (7) runs the client against `serve_channel` over a
    socket pair and a loopback server. It covers: no `Data` without credit; exact credit
    ending at the length; absolute offsets; `Fd` read-only; `Cancel` from a split client;
    refused oversized requests; garbage ending the channel; and hang-up.
  - The same file runs the `nuntiusd` process itself: loopback and the metadata endpoint
    are refused by policy without the grant, it exits cleanly on hang-up, and it refuses a
    standard input that is not a socket.

### 6.3 Step 4: the launch and `net-fetch` (2026-11-07)

- **The capability.** An app's manifest holds `net-fetch = true` (portcullis-toml). It
  goes through the usual grant flow, and the permissions prompt says "Fetch from the
  internet through the system's fetcher (no direct network access)". The jail gets
  portcullisd's socket directory and nothing else.
- **`OpenFetcher`** (portcullis-ipc) is a new portcullisd verb.
  - **Who may ask.** The caller is identified by the launch registry from its peer uid,
    never by anything it sends, as `Catalog` is. Its installed manifest must hold
    `net-fetch`.
  - **Uid.** The fetcher is always `org.atrium.nuntius`, run as the system worker rule's
    uid (`_nuntius`, 65023) and never as the caller. Without that rule, portcullisd
    refuses.
  - **Jail.** The handoff is `ExecInstance`'s: `ReadyForFds`, then three descriptors. Each
    fetcher gets a one-shot jail instance of its own, `u<uid>-<n>`, which is removed when
    the app hangs up.
  - **`ExecInstance` refuses `org.atrium.nuntius`.** Otherwise the fetcher's system-wide
    network grant would be anyone's raw fetch.
- **Client.** `nuntius::launch::open()` does the handshake and returns the `Client`. A
  guard keeps the jail alive: dropping it shuts the connection, so portcullisd removes the
  jail. The connection must be shut, not just dropped, because a thread holds a clone of
  it.
- **Install.** `scripts/nuntius-bundle.sh` builds the signed bundle in the guest:
  `nuntiusd`, the trust store, and the resolver's files. It also creates the system grant,
  `_nuntius`, and the worker rule (`callers = []`). `etc/jaild.policy.toml` admits uid
  65023.
- **Tests:**
  - `nuntius/tests/launch.rs` runs against a stand-in portcullisd: handshake, channel as
    stdin and stdout, hang-up on drop, and a refusal surfacing as `PermissionDenied`.
  - portcullis-toml, portcullis-policy and portcullis-ipc unit tests pass.
  - portcullisd and `nuntiusd` type-check for FreeBSD (`x86_64-unknown-freebsd`).
  - `nuntius-probe` (an example in the crate) is the app side of the jailed check.
- **Not done:**
  - A run on FreeBSD: an app holding `net-fetch` fetching through its own jailed fetcher,
    and one without it refused. This cloud container cannot download a FreeBSD image, so
    it needs the Mac.
  - The standalone player still uses its own `atrium-player-fetch`.
  - Origin narrowing from the manifest (§4) and the per-client private-network grant.
  - The Navigator's own instances, which stay in-process for now.

## 7. Open questions

- The name: Nuntius, accepted 2026-11-06.
- A shared HTTP cache across a client's own instances? Not in v1, which keeps one instance per
  client and no cache beyond connection reuse.
- HTTP/2 and HTTP/3. The fetcher is HTTP/1.1 today. Multiplexing would help media and pages,
  but it is a separate measured change (TTFD work is parked by the user's decision).
