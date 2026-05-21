# Insula — implementation status

**Branch:** `claude/romantic-rubin-42085b`
**Last updated:** 2026-05-21
**Phase:** M1A (Foundation) complete; M1B (service catalogue MVP minus Pergola) substantially complete — five libatrium ABI surfaces, four backing daemons.

This document orients a reader landing fresh in the
branch. For the design corpus see [`spec/insula.md`](spec/insula.md)
and its eight siblings; for the phasing see
[`ROADMAP-INSULA.md`](ROADMAP-INSULA.md).

## What works today

Two commands, no daemon management, no env-var setup:

```
$ insula install bundle/
Installed com.atrium-os.insula-hello v0.1.0

$ insula launch com.atrium-os.insula-hello
[insula][libatrium] atrium_init(sdk=1.0)
[insula][libatrium] connected to platform log at /private/var/.../run/insula-logd.sock
[insula][INFO] hello from Insula on aarch64-macos (insula-hello v0.1.0)
[insula][INFO] container is /private/var/.../container
[insula][INFO] wrote hello-from-insula.txt
[insula][libatrium] atrium_exit(0)
```

What this exercises end-to-end:

- **Manifest parser** turns `manifest.toml` into typed sections.
- **Bundle reader** validates layout + locates the binary.
- **Host adapter** writes an SBPL profile from the manifest,
  spawns the app through Apple's `sandbox-exec` with
  canonical container path, env vars set for the platform
  services.
- **App** (a real Rust binary) statically links **libatrium**
  and calls its C ABI: `atrium_init`, `atrium_log`,
  `atrium_container_path`, `atrium_storage_open`,
  `atrium_exit` — plus `atrium_keychain_pubkey`,
  `atrium_keychain_sign`, `atrium_net_connect`,
  `atrium_notify_post` available for apps that need them.
- **libatrium** routes `atrium_log` via real **Aqueduct**
  framing on `CLASS_LOG=10` to **insula-logd** running in
  the background.
- **insula-logd** decodes the envelope, writes an
  ISO-8601-stamped line into its log file.
- **vestibulum-macos** on `CLASS_VESTIBULUM=11` mints
  ed25519 keypairs on demand, signs challenges; signatures
  verify under returned pubkeys (test-asserted), keys
  persist across daemon restart.
- **atrium-netd-macos** on `CLASS_NET=12` accepts CONNECT
  requests, validates against an allowlist, resolves DNS,
  opens TCP, byte-proxies between the app's local socket
  and the upstream.
- **praeco-macos** on `CLASS_NOTIFY=3` accepts
  POST_NOTIFICATION, mints monotonic ids, appends to its
  notifications log.
- **insula-cli** auto-spawns all four daemons under
  `<install_root>/run/`, manages their pid files, threads
  the socket paths into the launched child's environment.

## Crate layout

Ten crates at the repo root. Each is its own `Cargo.toml`;
the repo has no top-level workspace by convention.

| Crate | Purpose | LoC (src + tests) |
|---|---|---|
| [`insula-manifest`](../insula-manifest/) | TOML parser for the full Insula manifest spec | ~700 |
| [`insula-bundle`](../insula-bundle/) | On-disk bundle reader + validator | ~250 |
| [`libatrium`](../libatrium/) | Platform C ABI (cdylib + rlib + staticlib) | ~1100 |
| [`insula-host-macos`](../insula-host-macos/) | macOS host adapter: SBPL gen, install, launch | ~950 |
| [`insula-hello`](../insula-hello/) | Demo Insula app + manifest | ~200 |
| [`insula-cli`](../insula-cli/) | `insula install / launch / list / info / uninstall / daemons` | ~750 |
| [`insula-logd`](../insula-logd/) | Aqueduct log-forwarding daemon | ~300 |
| [`vestibulum-macos`](../vestibulum-macos/) | ed25519 keychain daemon (disk-backed) | ~450 |
| [`atrium-netd-macos`](../atrium-netd-macos/) | Network broker (allowlist + byte proxy) | ~400 |
| [`praeco-macos`](../praeco-macos/) | Notifications daemon | ~300 |

## Spec → implementation mapping

| Spec section | Implemented in |
|---|---|
| `insula.md` §2.3 (libatrium C ABI) | `libatrium/src/lib.rs` + `libatrium/include/atrium.h` |
| `insula.md` §3.1 (bundle format) | `insula-bundle/src/lib.rs` |
| `insula.md` §4 (sandbox + network) | `insula-host-macos/src/sbpl.rs` + `src/launch.rs` |
| `insula.md` §5.1 (manifest schema) | `insula-manifest/src/lib.rs` + `src/sections.rs` |
| `insula.md` §4.2 (network broker) | `atrium-netd-macos/src/main.rs` + libatrium `atrium_net_connect` |
| `insula.md` §11.5 (push delivery) | (no — deferred) |
| `insula.md` §13.3 (per-service keypairs) | `vestibulum-macos/src/main.rs` + libatrium `atrium_keychain_*` |
| `insula.md` §15.2 (per-app storage) | libatrium `atrium_container_path` + `atrium_storage_open`; container provisioning in `insula-host-macos/src/install.rs` |
| `insula.md` §20 (notifications) | `praeco-macos/src/main.rs` + libatrium `atrium_notify_post` |
| `insula-host-macos.md` §2 (SBPL generation) | `insula-host-macos/src/sbpl.rs` |
| `insula-host-macos.md` §10 (bundle format on macOS) | `insula-host-macos/src/install.rs` (no `.app` wrapping yet) |
| `aqueduct.md` opcode-class registry | `aqueduct/src/classes.rs` — `CLASS_LOG=10`, `CLASS_VESTIBULUM=11`, `CLASS_NET=12` added; `CLASS_NOTIFY=3` reused |

## Architecture realized

```
┌─────────────────────────────────────────────────────────────────┐
│                          insula-cli                              │
│                  (install / launch / daemons)                    │
└─────────────────────────────────────────────────────────────────┘
        │                          │                       │
        ▼ install                   ▼ launch                ▼ daemons
┌──────────────────┐    ┌────────────────────────┐    auto-spawn:
│ insula-bundle    │    │  insula-host-macos     │   ┌─────────────────┐
│ + insula-manifest│    │  - SBPL gen            │   │ insula-logd     │
└──────────────────┘    │  - install + launch    │   │ vestibulum-     │
                        │  - canonical paths     │   │   macos         │
                        │  - 4 daemon sockets    │   │ atrium-netd-    │
                        │    threaded into env   │   │   macos         │
                        └────────────────────────┘   │ praeco-macos    │
                                    │                └─────────────────┘
                            ┌───────┴───────┐                ▲
                            ▼   sandbox-exec │                │
                        ┌───────────────────┐│  Aqueduct      │
                        │   Insula app      ││  CLASS_LOG=10  │
                        │   (insula-hello)  ││  CLASS_VESTIBULUM=11
                        │                   ││  CLASS_NET=12  │
                        │ ┌───────────────┐ ││  CLASS_NOTIFY=3│
                        │ │  libatrium    │─┘└────────────────┘
                        │ │ init / log /  │
                        │ │ exit          │
                        │ │ storage       │ (in-process; container fd)
                        │ │ keychain      │  → vestibulum-macos
                        │ │ net           │  → atrium-netd-macos
                        │ │ notify        │  → praeco-macos
                        │ └───────────────┘
                        └───────────────────┘
                          sandboxed by App Sandbox
                          via generated SBPL profile
```

## Running the demo

Prerequisites: macOS 14+ on Apple Silicon (this branch's
test target); standard `cargo` (Rust nightly toolchain
per the repo).

```sh
# Build all crates we need
for c in insula-manifest insula-bundle libatrium \
         insula-host-macos insula-hello insula-cli \
         insula-logd vestibulum-macos; do
  cargo build --manifest-path "$c/Cargo.toml"
done

# Synthesize a bundle for insula-hello
BUNDLE=$(mktemp -d)
mkdir -p "$BUNDLE/bin"
cp insula-hello/manifest.toml "$BUNDLE/"
cp insula-hello/target/debug/insula-hello "$BUNDLE/bin/"

# Use a fresh install root for the demo
ROOT=$(mktemp -d)

# Point insula at the locally-built daemon binaries
export INSULA_LOGD_BIN=$(pwd)/insula-logd/target/debug/insula-logd
export INSULA_VESTIBULUMD_BIN=$(pwd)/vestibulum-macos/target/debug/vestibulum-macos
export INSULA_INSTALL_ROOT="$ROOT"
INSULA=$(pwd)/insula-cli/target/debug/insula

$INSULA install $BUNDLE
$INSULA daemons status     # both stopped
$INSULA launch com.atrium-os.insula-hello
$INSULA daemons status     # both running now
cat $ROOT/run/insula-logd.log
$INSULA daemons down
```

## Testing

94 tests pass across all 10 crates on this macOS host.

```sh
for c in insula-manifest insula-bundle libatrium \
         insula-host-macos insula-hello insula-cli \
         insula-logd vestibulum-macos \
         atrium-netd-macos praeco-macos; do
  cargo test --manifest-path "$c/Cargo.toml"
done
```

Test distribution:

| Crate | Tests | Notable |
|---|---|---|
| insula-manifest | 17 | Full coverage of manifest sections + roundtrip |
| insula-bundle | 5 | Bundle layout validation incl. malformed cases |
| libatrium | 11 | C ABI surface tests + Aqueduct routing + storage |
| insula-host-macos | 16 | SBPL gen + actual sandboxed launch + install layout |
| insula-hello | 5 | Bundle parses; install+run via host adapter; per-app netd enforcement E2E (allow + deny) |
| insula-cli | 11 | All subcommands + auto-spawn + logd integration |
| insula-logd | 3 | Daemon decodes Aqueduct messages + writes log file |
| vestibulum-macos | 10 | ed25519 keychain roundtrip incl. signature verify, persistence across restart |
| atrium-netd-macos | 13 | Per-app manifest enforcement (8 unit) + broker behavior (5 integration) |
| praeco-macos | 3 | Notification posts log + monotonic ids + degraded |

The *load-bearing* integration tests (the ones that prove
the design isn't just on paper):

- `insula-host-macos/tests/launch_sandboxed.rs::sandbox_actually_constrains_writes_outside_container`
  — generates SBPL, launches `/bin/sh`, attempts a write
  outside the container, verifies the write was blocked.
  This is the "does the sandbox actually sandbox?" check.

- `insula-hello/tests/install_and_run.rs::install_and_run_insula_hello`
  — full install→launch cycle; verifies (1) stderr
  contains the lifecycle markers libatrium emitted, and
  (2) the marker file written via `atrium_storage_open`
  actually exists in the canonical container path.

- `insula-cli/tests/auto_daemons.rs::launch_auto_spawns_logd_and_routes_to_it`
  — `insula launch` with no env vars set, no daemons
  running first; expects auto-spawn + log message to
  reach the auto-spawned daemon's log file.

- `vestibulum-macos/tests/keychain_roundtrip.rs::pubkey_then_sign_roundtrip_via_libatrium`
  — calls `atrium_keychain_pubkey` + `atrium_keychain_sign`
  via libatrium; verifies the returned signature under the
  returned pubkey using ed25519-dalek's `Verifier` trait.

- `vestibulum-macos/tests/keychain_roundtrip.rs::keys_survive_daemon_restart`
  — spawn daemon, mint key, KILL the daemon process,
  spawn fresh daemon on same keystore, sign with the
  same service; signature must verify under the
  ORIGINAL pubkey. The persistence load-bearer.

- `atrium-netd-macos/tests/network_roundtrip.rs::connect_through_broker_reaches_tcp_echo_server`
  — spawn broker, spawn local TCP echo server, call
  `atrium_net_connect` via libatrium, use the returned fd
  as a TcpStream, bytes round-trip end-to-end. Proves the
  byte-proxy actually proxies.

- `praeco-macos/tests/notification_roundtrip.rs::post_returns_id_and_logs_record`
  — call `atrium_notify_post` via libatrium, verify the
  daemon assigns an id, returns it, and appends the
  structured record.

- `insula-hello/tests/per_app_netd.rs::manifest_allowing_host_lets_app_connect`
  + `::manifest_denying_host_blocks_app_connect` — full E2E
  proof of per-app `[network]` enforcement: vary insula-hello's
  bundle manifest, launch through the CLI, observe the broker's
  manifest-driven verdict in the auto-spawned daemon's log file.
  Closes the loop on SO_PEERPID + proc_pidpath + manifest lookup
  + verdict — the whole chain proven on real bytes.

## v0 limitations (documented)

- **No bundle signing.** Bundles are unsigned in v0;
  `manifest.signature` is unused. Per spec §3.1 and
  `insula-host-macos.md` §10.3 — signing + notarization
  are Phase 1C work.
- **Vestibulum keystore is plaintext on disk.** Keys
  survive daemon restart (real persistence) but the
  `.key` files are unencrypted ed25519 secrets. macOS
  Keychain Services wrapping is `vestibulum.md` §3.1
  future work. Marked with `TODO(vestibulum-secure-
  storage)` at every write site.
- **`sandbox-exec` not `sandbox_init_with_parameters`.**
  Using Apple's supported CLI tool avoids private-SPI
  coupling. Per-unix-socket SBPL grants don't work via
  this path (we tested `(literal …)`, `(path …)`,
  `(remote unix-socket (path-literal …))` — all denied)
  — when a daemon socket is needed, a broad
  `(allow network-outbound)` is the workable grant.
  Tighter posture lives behind direct `sandbox_init`
  (private SPI), post-v0.
- **~~Network broker is broker-wide, not per-app.~~** Done.
  The broker uses `getsockopt(LOCAL_PEERPID)` for kernel-
  attested peer identification, `libc::proc_pidpath` to
  resolve the executable, walks `<install_root>/apps/*/
  bundle/` to match an installed app, loads its manifest,
  and enforces `[network].hosts` per-connection. Broker-
  wide `$INSULA_NETD_ALLOWED_HOSTS` is the fallback for
  unidentified peers. E2E-tested with insula-hello.
- **UDP unsupported in the broker.** TCP only; the
  ABI accepts `ATRIUM_NET_UDP` but the daemon returns
  PROTO_UNSUPPORTED.
- **Praeco routes to a file, not to UserNotifications.**
  Wire shape is correct; backend swap to the macOS
  Notification Center is future polish.
- **No Pergola wire emission.** The Pergola crate exists
  with view/node/layout, but wire emission to a Fresco
  server is phase-4 work in Pergola itself. Insula apps
  cannot open windows yet through libatrium.
- **No bundle update / capability-diff consent flow.**
  Re-install replaces bundle/ but preserves container/;
  no diff prompt UX yet.

Each of these is a slice that can land independently;
none of them invalidates the current shape.

## Where the design lives

- [`spec/insula.md`](spec/insula.md) — parent spec, 26 sections.
- [`spec/insula-host-macos.md`](spec/insula-host-macos.md) — macOS host adapter design.
- [`spec/vestibulum.md`](spec/vestibulum.md) — keychain service.
- [`spec/limen.md`](spec/limen.md) — cross-jail embed broker (not implemented yet).
- [`spec/nomenclator.md`](spec/nomenclator.md) — name resolution (not implemented yet).
- [`spec/tabellarius.md`](spec/tabellarius.md) — push delivery (not implemented yet).
- [`spec/loculus.md`](spec/loculus.md) — wallet / autofill (not implemented yet).
- [`spec/concursus.md`](spec/concursus.md) — peer-to-peer (not implemented yet).
- [`spec/atrium-ax.md`](spec/atrium-ax.md) — accessibility (not implemented yet).
- [`spec/artifex.md`](spec/artifex.md) — reference IDE (not implemented yet).
- [`ROADMAP-INSULA.md`](ROADMAP-INSULA.md) — phased plan.

## Commit history of the implementation effort

25 implementation commits, oldest at the bottom:

```
5e0505f insula-hello: optional net-connect + E2E per-app netd enforcement test
4725305 atrium-netd-macos: per-app manifest enforcement (SO_PEERPID + proc_pidpath)
f2b10da docs/INSULA-IMPLEMENTATION: reflect 5-ABI / 4-daemon state
2dd0a93 praeco-macos: notifications daemon + atrium_notify_post
06866a7 insula-cli + host-macos: auto-spawn atrium-netd-macos too
b79da55 atrium-netd-macos: network broker daemon + atrium_net_connect ABI
0ffbe19 vestibulum-macos: disk-backed keystore -- keys survive daemon restart
108e18e docs/INSULA-IMPLEMENTATION: status doc for the implementation work
ffc6fac insula-cli: auto-spawn daemons + daemons up/down/status
5c5f83f vestibulum-macos: ed25519 keychain daemon + atrium_keychain_* ABI
2ba7205 libatrium + host-macos: atrium_container_path + atrium_storage_open + canonical-path fix
3b2d179 aqueduct: register CLASS_LOG=10; libatrium + insula-logd migrate
daccfb9 insula-cli + host-macos: thread logd socket through launch
0e08120 insula-logd: daemon that catches libatrium log forwarding
bcb1df2 libatrium: route atrium_log over real Aqueduct when socket available
338c10c insula-cli: user-facing command-line frontend
c94c125 insula-bundle + install pathway: install once, launch by app-id
029530b libatrium + insula-hello: end-to-end bring-up loop closed
c9a91f4 insula-host-macos: launch path via sandbox-exec + integration tests
af39721 insula-host-macos: SBPL + entitlements generation from manifest
2d4fa17 insula-manifest: add [background] [role] [peer] [sync] [entry-points] [capabilities]
1f076b7 insula-manifest: add [network] with host-allowlist shape
42448cd insula-manifest: add [render] [input] [ipc] [storage] [compute]
b651f13 insula-manifest: initial skeleton -- parse [app] + [bundle]
```

## Strategic checkpoints from ROADMAP-INSULA

- **§7.1 "does the abstraction work?"** — yes. The host-adapter
  abstraction holds; an Insula app launches sandboxed with
  capability shape derived from a TOML manifest.
- **§7.2 "is Artifex compelling?"** — not yet attempted.
  Pergola wire emission is the blocker.
- **§7.3 "is the platform real?"** — substantially yes.
  Four daemons running real Aqueduct traffic. Real ed25519
  signatures, real TCP byte-proxy, real notification ids,
  real sandboxed file I/O, real key persistence across
  daemon restart. The platform shape is proven at the
  smallest scope; "real" in the developer-beta sense
  still needs the Pergola path for windowed UI.

## What to build next, in order

1. **Pergola wire emission** (the M1C blocker, per ROADMAP-INSULA §1.3).
   Until this lands, no Insula app can open a window.
2. **macOS-Keychain-Services-backed vestibulum keystore**
   — wrap the `.key` files via SecItemAdd. Closes the
   "plaintext on disk" caveat. Depends on the
   `security-framework` crate or raw libc bindings.
3. **Bundle signing** — minisign / ed25519-style as a first
   pass per `atrium-pkg.md`. Adds an `insula sign` CLI
   subcommand and verification at install time.
4. **Tabellarius push delivery** — `insula.md` §11.5 isn't
   started; pattern is well-established now (sixth daemon
   following the established shape).
5. **More sample apps** — the platform supports the
   primitives but only one app (insula-hello) exercises
   them. A second app (e.g. an atrium-fetch tool that
   uses atrium_net_connect with structured output) would
   show breadth.

Each of these is one or two commits given the current
shape; the foundation underneath is in place.
