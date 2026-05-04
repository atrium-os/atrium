# Atrium-RPC service author guide

Companion to [`aqueduct.md`](aqueduct.md). This document tells
you how to **build** a service on the substrate, not how the
substrate works internally.

## 0. Decide if aqueduct is the right substrate

It is the right substrate if your service:

- Is a long-running daemon that other Atrium processes call into.
- Carries non-trivial blob payloads (images, documents,
  manifests) that may be referenced from multiple channels.
- Should benefit from cross-jail capability scoping via
  Portcullis filesystem mounts.
- Wants its blobs to share a hash space with Tessera.

It is **not** the right substrate if:

- You're inside one process — use a Rust channel, mutex, etc.
- You're streaming millions of small events with µs latency
  budgets — go raw UDS or a shm ring; the envelope adds 10 B
  per message.
- You speak a foreign protocol (HTTP, FUSE, gRPC) — those
  ride on their own transports.

## 1. Reserve an opcode_class

Edit two places together:

- `docs/spec/aqueduct.md` §3.2 — the registry table.
- `aqueduct/src/classes.rs` — the `CLASS_*` constants.

Pick the next free class number in the 0..63 range (Atrium core).
If your service is third-party / experimental, use 64..255.

## 2. Define your opcode dictionary

Pick two ranges:

- `0x00..0x7F` — stable. Once shipped, never repurpose.
- `0x80..0xFF` — experimental / per-service-version. May change.

Document each opcode in your service's crate doc. Suggested
columns: op, name, direction (client→server / server→client / both),
flags expected, payload schema.

### Reference example: CLASS_DISPLAY (Fresco)

`fresco-protocol` is the canonical example service on this
substrate. Its op-number layout (in `fresco-protocol/src/lib.rs`)
demonstrates the conventions:

```text
0x0010..=0x001F    Reserved for future control extensions
0x0020..=0x002F    Slot table mutations (SLOT_*)
0x0030..=0x003F    Scene frame boundaries (SCENE_FRAME_*)
0x0040..=0x004F    Scene node mutations (SCENE_NODE_*)
0x0500..=0x05FF    Window management (WINDOW_*); 0x0580..=0x05FF for events
0x0600..=0x06FF    Reserved for D4.5 ANIMATION_* op family
```

Both control ops (client→server) and async events (server→client,
sent with envelope `ASYNC_EVENT` flag) live in the same class; events
are typically distinguished by living in a sub-range (e.g. `0x0580+`
for window events).

Reserve `0xFF` in your class for a `NEGOTIATE_CAPS`-equivalent
that lets clients query supported features.

## 3. Pick a payload marshalling format

aqueduct gives you bytes; what's inside is up to you.

**Defaults:**
- **Postcard** for Rust↔Rust messaging. Compact, deterministic,
  no_std-friendly, schema-evolution via `Option`/`enum`
  patterns.
- **Hand-rolled binary** for very small / hot opcodes (single u32,
  fixed structs). Faster, no dependency.

**Avoid:**
- JSON — wastes bytes, introduces a parser as attack surface.
- Protobuf / Cap'n Proto — overkill unless you're already cross-
  language. Reconsider when D6+ pulls in non-Rust clients.

Whichever you pick, **start the payload with a 1-byte schema
version**. You will regret not having it.

## 4. Sketch the server

```rust
use aqueduct::{Connection, MessageKind, classes, envelope::flag};
use std::os::unix::net::UnixListener;

fn main() -> std::io::Result<()> {
    // Standard service location: /atrium/sockets/<name>.sock.
    // Portcullis nullfs-mounts this into jails per atrium.toml.
    let _ = std::fs::remove_file("/atrium/sockets/myservice.sock");
    let l = UnixListener::bind("/atrium/sockets/myservice.sock")?;
    // mode 0600, owned by service UID — set permissions as
    // appropriate for your trust model.

    for s in l.incoming() {
        let s = s?;
        std::thread::spawn(move || {
            let mut c = Connection::wrap(s).unwrap();
            handle(&mut c).unwrap_or_else(|e| eprintln!("client: {e}"));
        });
    }
    Ok(())
}

fn handle(c: &mut Connection) -> std::io::Result<()> {
    loop {
        let m = c.recv_message()?;
        if m.opcode_class != classes::CLASS_MYSERVICE { continue; }
        match (m.op, m.kind) {
            (OP_FOO, MessageKind::Request) => {
                let response = handle_foo(c, &m.payload)?;
                c.send_message(
                    classes::CLASS_MYSERVICE,
                    OP_FOO_RESP,
                    flag::IS_RESPONSE,
                    &response,
                )?;
            }
            _ => {} // unknown ops silently dropped (per spec)
        }
    }
}
```

## 5. Sketch the client

```rust
use aqueduct::{Connection, MessageKind, classes, envelope::flag};

fn main() -> std::io::Result<()> {
    let mut c = Connection::connect("/atrium/sockets/myservice.sock")?;

    // Upload a payload via CAS (auto-dedups on the server side).
    let h = c.upload_blob(&document_bytes)?;

    // Send the request — payload is just the hash; server resolves
    // from cache (already loaded by upload_blob).
    c.send_message(
        classes::CLASS_MYSERVICE,
        OP_FOO,
        flag::RESPONSE_EXPECTED,
        &h,
    )?;

    // Wait for response.
    loop {
        let m = c.recv_message()?;
        if m.opcode_class == classes::CLASS_MYSERVICE
            && m.op == OP_FOO_RESP
            && m.kind == MessageKind::Response
        {
            // process m.payload
            break;
        }
    }
    Ok(())
}
```

## 6. CAS payload patterns

### Always-by-hash for non-trivial payloads

If your op carries a payload bigger than a few KiB, prefer:

```rust
let h = c.upload_blob(&bytes)?;
c.send_message(class, OP, flag::RESPONSE_EXPECTED, &h)?;
```

Server side:

```rust
let bytes = c.cache_get(&hash)
    .ok_or_else(|| io::Error::other("hash not in cache — \
                                     client did not upload"))?;
```

This gives you free dedup across requests (same content =
same hash = one cache entry on the server).

### Inline payload for tiny / hot ops

For control-plane ops with payloads ≤ 256 B (e.g., "increment
volume by 5", "set focus to window 7"), pass the bytes inline in
the message. No CAS dance needed; the envelope itself is the
transport.

### Mixed: small struct + large blobs by ref

For ops with both small structured fields AND a large blob (e.g.,
"create notification with this metadata + this icon"):

1. Upload the icon: `let h = c.upload_blob(&icon_png)?;`
2. Marshal the metadata (postcard or hand-rolled) with the hash
   embedded as a 32-byte field.
3. Set `flag::HAS_HASH_REFS` so the server knows to resolve
   referenced blobs from cache.

## 7. Async events

Set `flag::ASYNC_EVENT`. Don't expect a response. Receivers
deliver these via `recv_message` with `MessageKind::Event`.

```rust
// Server side: notify all clients about a state change.
for client in &mut clients {
    client.send_message(
        classes::CLASS_MYSERVICE,
        OP_STATE_CHANGED,
        flag::ASYNC_EVENT,
        &payload,
    )?;
}
```

For broadcast across many clients, the service is responsible for
maintaining a per-connection list. aqueduct doesn't do
many-to-many automatically.

## 8. Errors

Two layers:

- **Transport errors** — bubble up `io::Error` from `send_message`
  / `recv_message`. The connection is broken; tear down and
  reconnect.
- **Service errors** — define an OP_ERROR opcode in your
  dictionary, payload describes the error. Send with
  `flag::IS_RESPONSE` so the client correlates it with the
  failing request.

Don't use envelope flags for service-level errors; reserve
flags for things aqueduct itself defines.

## 9. Capability declaration (Portcullis)

Add your service to the policy (D2.5):

```toml
# atrium.toml in an app that uses your service
[capabilities]
myservice = true
```

Portcullis reads the manifest, sees the capability, and
nullfs-mounts `/atrium/sockets/myservice.sock` into the jail.
Apps that don't declare it can't see the socket — kernel-enforced
default-deny.

## 10. Trusted services: tessera-cas-read

System services that benefit from zero-copy hash lookups can
declare themselves trusted in their own service manifest:

```toml
# myservice/service.toml
[capabilities]
tessera-cas-read = true
```

Portcullis grants the service read-only nullfs of `/var/lib/
tessera/cas` at `/atrium/cas/`. The service can answer hash
lookups without going through the wire when content is in
Tessera storage.

**Apps cannot get this capability.** It's a service-only grant
in Portcullis policy. Otherwise the "sender must serve"
isolation property breaks.

## 11. Testing patterns

### Unit tests with `UnixStream::pair`

```rust
use std::os::unix::net::UnixStream;
let (a, b) = UnixStream::pair().unwrap();
let mut client = Connection::wrap(a).unwrap();
let mut server = Connection::wrap(b).unwrap();
// drive the protocol; assert on messages
```

This is what `aqueduct/src/connection.rs` uses. No filesystem,
no kernel scheduling — perfect for unit tests.

### Integration tests with the real server

`aqueduct-echo` is the canonical example: spawn the server as
a child process, run the client, assert exit code. See
`aqueduct-echo/src/bin/{server,client}.rs`.

## 12. Performance checklist

- Use `cache_get` before sending FETCH_REQUEST — the cache may
  already have it.
- For high-frequency ops, batch into one message rather than
  many small ones (each message has a 10-byte envelope cost +
  syscall round-trip).
- Set `cache_cap` based on your service's working set; the
  default 8 MiB is fine for most.
- Don't use aqueduct for streaming audio/video sample data —
  that's what shm + fd-passing is for.

## 13. Deployment

Sockets live at `/atrium/sockets/<service>.sock`. Convention:

- Service runs as a dedicated UID.
- Socket mode 0600, group 0700 if multi-user grouping is needed.
- Service is launched by `init` / `rc` (D2 era) or by the
  user-session supervisor.
- Crash recovery: on bind, `unlink` the stale socket if it
  exists (the example in §4 does this).
