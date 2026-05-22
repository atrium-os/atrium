# Insula — implementation status

**Branch:** `claude/romantic-rubin-42085b`
**Last updated:** 2026-05-22
**Phase:** M1A (Foundation) + M1B (service-catalogue MVP) + M1C (Pergola path) complete. **Tabellarius Phase B (push relay + wake-on-push) complete** — the full path publisher → relay → device daemon → {queue, wake the owning app} works end-to-end. Beyond the ABI: full publish-side toolchain (`keygen` / `sign` / `bundle` / `release` / `publishers`), dev-iteration tooling (`init` / `run` / `install --link` / `doctor` / `clean`), four convenience scripts, GitHub Actions CI on macos-14, opt-in macOS NotificationCenter backend, encryption-at-rest for both keystores. Five sample apps cover all four scene-node primitives. 256 tests across 16 crates.

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

Sixteen crates at the repo root. Each is its own `Cargo.toml`;
the repo has no top-level workspace by convention.

| Crate | Purpose | LoC (src + tests) |
|---|---|---|
| [`insula-manifest`](../insula-manifest/) | TOML parser + capability-diff for the full Insula manifest spec | ~1000 |
| [`insula-bundle`](../insula-bundle/) | On-disk bundle reader + signing + `.insula` archive container | ~800 |
| [`libatrium`](../libatrium/) | Platform C ABI (cdylib + rlib + staticlib) — full window ABI with all four scene-node primitives | ~2900 |
| [`insula-host-macos`](../insula-host-macos/) | macOS host adapter: SBPL gen, install, launch (six platform sockets threaded) | ~1100 |
| [`insula-hello`](../insula-hello/) | Demo Insula app + manifest | ~250 |
| [`atrium-fetch`](../atrium-fetch/) | Sample app — HTTP GET via the platform ABI | ~150 |
| [`atrium-mon`](../atrium-mon/) | Composite sample — net probe + notification (proves ABI composition) | ~120 |
| [`atrium-paint`](../atrium-paint/) | Windowed sample — first to use the Pergola path (open / paint / poll / destroy) | ~130 |
| [`insula-clock`](../insula-clock/) | Windowed sample — multi-primitive composition (RECT + 14 PATHs + TEXTURE per frame) | ~170 |
| [`insula-cli`](../insula-cli/) | `insula install / launch / run / list / info / uninstall / daemons {up,down,restart,status,logs} / keygen / sign / publishers / bundle / push / keychain / notify / doctor / init` | ~2800 |
| [`insula-logd`](../insula-logd/) | Aqueduct log-forwarding daemon | ~300 |
| [`vestibulum-macos`](../vestibulum-macos/) | ed25519 keychain daemon (disk-backed) | ~450 |
| [`atrium-netd-macos`](../atrium-netd-macos/) | Network broker (allowlist + byte proxy + SO_PEERPID per-app enforcement) | ~400 |
| [`praeco-macos`](../praeco-macos/) | Notifications daemon | ~300 |
| [`tabellarius-macos`](../tabellarius-macos/) | Push-delivery daemon — subscribe/list + relay client + GET_PUSH + wake-on-push | ~750 |
| [`tabellarius-relay`](../tabellarius-relay/) | Standalone push relay — routing core + TCP daemon (publisher → device fan-out) | ~600 |

## Spec → implementation mapping

| Spec section | Implemented in |
|---|---|
| `insula.md` §2.3 (libatrium C ABI) | `libatrium/src/lib.rs` + `libatrium/include/atrium.h` |
| `insula.md` §3.1 (bundle format) | `insula-bundle/src/lib.rs` |
| `insula.md` §4 (sandbox + network) | `insula-host-macos/src/sbpl.rs` + `src/launch.rs` |
| `insula.md` §5.1 (manifest schema) | `insula-manifest/src/lib.rs` + `src/sections.rs` |
| `insula.md` §4.2 (network broker) | `atrium-netd-macos/src/main.rs` + libatrium `atrium_net_connect` |
| `insula.md` §11.5 (push delivery) | `tabellarius-macos` + `tabellarius-relay` — subscribe/list, relay fan-out, GET_PUSH, wake-on-push (Phase A + B complete) |
| `insula.md` §13.3 (per-service keypairs) | `vestibulum-macos/src/main.rs` + libatrium `atrium_keychain_*` |
| `insula.md` §15.2 (per-app storage) | libatrium `atrium_container_path` + `atrium_storage_open`; container provisioning in `insula-host-macos/src/install.rs` |
| `insula.md` §20 (notifications) | `praeco-macos/src/main.rs` + libatrium `atrium_notify_post` |
| `insula.md` §3.1 (bundle archive format) | `insula-bundle/src/archive.rs` (`INSB` v1) + `insula bundle` CLI |
| `insula.md` §5.4 (capability-diff consent) | `insula-manifest::diff::CapabilityDiff` + `insula install --accept-changes` |
| `tabellarius.md` §9.1 (subscribe/list/get-push ABI) | `tabellarius-macos/src/main.rs` + libatrium `atrium_tabellarius_*` |
| `tabellarius.md` §3 (relay protocol) | `tabellarius-relay` — CBOR frames over plaintext TCP; mutual-auth TLS is the remaining §3.2 hardening item |
| `tabellarius.md` §4.2 (wake-on-push) | `tabellarius-macos/src/relay_client.rs` — `$INSULA_TABELLARIUSD_WAKE_CMD` hook |
| `insula.md` §6 (windowed UI / Pergola) | libatrium `atrium_window_*` (open/destroy/fill_rect/frame_begin/frame_rect/frame_end/poll_event) → CLASS_DISPLAY → external Fresco scene server (frescod) |
| `insula-host-macos.md` §2 (SBPL generation) | `insula-host-macos/src/sbpl.rs` |
| `insula-host-macos.md` §10 (bundle format on macOS) | `insula-host-macos/src/install.rs` (no `.app` wrapping yet) |
| `aqueduct.md` opcode-class registry | `aqueduct/src/classes.rs` — `CLASS_LOG=10`, `CLASS_VESTIBULUM=11`, `CLASS_NET=12`, `CLASS_TABELLARIUS=13` added; `CLASS_NOTIFY=3` + `CLASS_DISPLAY=1` reused |

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

256 tests pass across all 16 crates on this macOS host.

```sh
# One-line runner: scripts/test-insula.sh
# Or in CI: .github/workflows/insula.yml

scripts/test-insula.sh           # full regression
scripts/test-insula.sh --quick   # skips heavy windowed E2Es
```

Test distribution:

| Crate | Tests | Notable |
|---|---|---|
| insula-manifest | 24 | Full coverage of manifest sections + roundtrip + 7 capability-diff cases |
| insula-bundle | 18 | Bundle layout + ed25519 sign/verify + `.insula` archive (roundtrip, deterministic, unsafe-path refused) |
| libatrium | 32 | C ABI tests + Aqueduct routing + storage + the full window ABI: open / fill_rect / poll_event / multi-node frame builder / PATH / TEXTURE (incl. CAS upload state machine) / GLYPH_RUN (decoded via canonical fresco_protocol) |
| insula-host-macos | 16 | SBPL gen + actual sandboxed launch + install layout (six platform sockets threaded; copy + link install modes) |
| insula-hello | 6 | Bundle parses; install+run via host adapter; per-app netd E2E (allow + deny); per-app tabellarius E2E |
| atrium-fetch | 1 | Bundle + manifest parse for the HTTP-GET sample |
| atrium-mon | 2 | Net probe + notification compose end-to-end (reachable + unreachable cases) |
| atrium-paint | 3 | Pergola path E2E (stub fresco server) — open / paint / poll / destroy, standalone and via `insula launch` |
| insula-clock | 1 | Multi-primitive frame E2E — startup CAS upload + per-frame composition (1 RECT + 14 PATHs + 1 TEXTURE) via stub fresco server |
| insula-cli | 88 | All subcommands; signing/archive/diff E2E; push + keychain + notify + daemons {restart,logs} + doctor + init + run + clean + release + version + uninstall --all + install --link + info-with-verify E2E; list table with per-app capability tags |
| praeco-macos | 6 | Notification posts log + monotonic ids + degraded + opt-in osascript backend (NotificationCenter delivery) |
| insula-logd | 3 | Daemon decodes Aqueduct messages + writes log file |
| vestibulum-macos | 13 | ed25519 keychain roundtrip + signature verify + persistence; XChaCha20-Poly1305 encryption-at-rest (on-disk-not-raw, master-key-governs-decrypt, legacy plaintext compat) |
| atrium-netd-macos | 13 | Per-app manifest enforcement (8 unit) + broker behavior (5 integration) |
| tabellarius-macos | 16 | substore unit + encryption-at-rest + subscribe/unsubscribe/count via libatrium + relay-delivery E2E (publisher→relay→daemon→GET_PUSH, libatrium ABI drain, wake-on-push fires / doesn't-fire) |
| tabellarius-relay | 14 | wire-protocol round-trips + routing core (subscribe/fan-out/unsubscribe/dedup) + TCP daemon E2E |

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

## CLI surface

After the M1B daemon work + the recent ergonomics
pass, the `insula` CLI exposes a complete operator
loop:

**App lifecycle**
| Command | Purpose |
|---|---|
| `insula init <dir> [--name <id>]` | Scaffold a new app skeleton |
| `insula install <bundle\|.insula>` | Install from directory or archive |
| `insula install ... --allow-unsigned` | Skip signature check (dev only) |
| `insula install ... --accept-changes` | Accept widened capability grants on re-install |
| `insula install ... --link` | Symlink the source dir instead of copying (dev iteration) |
| `insula list` | Show installed apps as a table with capability tags + sig status |
| `insula info <app\|dir\|insula>` | Show capability surface + signature (with verify) + paths; works on installed apps, bundle dirs, or `.insula` archives |
| `insula launch <app-id> [args]` | Run a sandboxed app (auto-spawns daemons; threads `$INSULA_FRESCO_SOCKET` through for windowed apps) |
| `insula run <bundle> [args]` | Install + launch in one shot (dev iteration; auto-applies `--allow-unsigned` + `--accept-changes`) |
| `insula uninstall <app\|--all>` | Remove one app or every installed app |
| `insula doctor` | Health check across the install (install-root / daemons / publishers / app layouts) |
| `insula clean [--all]` | Drop ephemeral state (run/); `--all` also wipes apps + trust store |
| `insula version [--verbose]` | Print version (and ABI-family enumeration with `--verbose`) |

**Publish-side tooling**
| Command | Purpose |
|---|---|
| `insula keygen <id> <out-dir>` | Generate a publisher ed25519 keypair |
| `insula sign <bundle> --key <f>` | Sign a bundle in place |
| `insula bundle <src> <out.insula>` | Pack a bundle directory into a single-file archive |
| `insula release <src> <out.insula> --key <sk>` | Sign + pack in one shot (publish-ready artifact) |
| `insula publishers add\|list\|remove` | Manage the install-time trust store |

**Daemon ops**
| Command | Purpose |
|---|---|
| `insula daemons up\|down\|status` | Lifecycle for all five platform daemons |
| `insula daemons restart [name\|all]` | Stop + start one daemon or all of them |
| `insula daemons logs <name>` | Print a daemon's log file to stdout |

**Direct daemon surfaces** — handy for development /
debugging without writing an app:
| Command | Daemon |
|---|---|
| `insula push subscribe\|list\|unsubscribe` | tabellarius |
| `insula keychain pubkey\|sign` | vestibulum |
| `insula notify <title> <body> [--urgency]` | praeco |

All daemon-talking subcommands auto-spawn the daemon
if it isn't running yet (via the shared
`daemons::ensure_started` helper) and honor the
`INSULA_<NAME>_SOCKET` env vars for test / power-user
overrides.

## Scripts + CI

Four shell scripts under `scripts/` wrap the common
loops. All POSIX `/bin/sh` so they run on macOS and
FreeBSD alike (no bash extensions).

| Script | Purpose |
|---|---|
| [`scripts/test-insula.sh`](../scripts/test-insula.sh) | Run `cargo test` across every Insula crate; aligned summary table. `--quick` skips the heavy windowed E2Es. |
| [`scripts/build-insula.sh`](../scripts/build-insula.sh) | Build every crate (release by default) into `./dist/{bin,lib,include}` for shipping. `--debug` for fast iteration. |
| [`scripts/install-insula.sh`](../scripts/install-insula.sh) | Install `./dist/` to a prefix (default `~/.local`). `--prefix /opt` overrides. `--uninstall` reverses. |
| [`scripts/insula-demo.sh`](../scripts/insula-demo.sh) | End-to-end walkthrough — builds, then runs init→keygen→publishers add→release→info→install→list→doctor→uninstall in one go. Doubles as a smoke test. |

[`.github/workflows/insula.yml`](../.github/workflows/insula.yml)
wires the first three together on macos-14 — `build` →
`test` (full, no `--quick`) → `demo` for every Insula-
touching push or PR. Path filters scope the workflow
so unrelated atrium-vk-icd / fresco-rs / tessera
commits don't trigger it.

## v0 limitations (documented)

- **~~No bundle signing.~~** Done. ed25519 signatures with
  an "INSL" v1 wire format (key_id + pubkey + signature
  over `SHA256(manifest.toml) || SHA256(entry binary)`).
  CLI subcommands `insula keygen`, `insula sign`,
  `insula publishers {add,list,remove}`. Verification at
  install time against `<install_root>/trusted-publishers/<key-id>.pub`;
  unsigned installs require explicit `--allow-unsigned`.
  E2E-tested: trusted-publisher install, tampered-bundle
  rejection, wrong-publisher rejection.
- **~~Vestibulum + tabellarius stores plaintext on disk.~~**
  Done for both daemons. Each store gets a per-
  installation 32-byte `master.key` (0o600) that wraps
  every per-(service|subscription) file via
  XChaCha20-Poly1305. File layout: `[24B nonce |
  ciphertext | 16B tag]`. master.key itself is plaintext
  on disk (an attacker with full directory read gets
  everything anyway), but the wrapping defends against
  partial-backup scenarios: someone gets a few `.key` /
  `.sub` files via accidental sync or backup tooling
  but not master.key — those bytes are useless. Both
  daemons accept the legacy plaintext format on read
  (zero-friction migration). Hardware-backed wrapping
  (macOS Keychain Services / Secure Enclave) remains
  the next layer of future work.
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
- **~~Praeco routes to a file, not to UserNotifications.~~**
  Done opt-in. Setting `INSULA_PRAECOD_BACKEND=osascript`
  delivers via `/usr/bin/osascript` (`display notification`)
  in addition to the structured log record. Default is
  still file-only so tests + headless workflows are
  unchanged. The osascript path was chosen over
  UNUserNotificationCenter because the modern framework
  requires an app bundle + entitlement, which the daemon
  doesn't have; osascript works without entitlements.
- **~~No Pergola wire emission.~~** Done at the ABI
  level. libatrium exposes a complete window surface
  (`atrium_window_open` / `atrium_window_fill_rect` /
  `atrium_window_frame_begin` / `_frame_rect` / `_frame_end`
  / `atrium_window_poll_event` / `atrium_window_destroy`)
  on top of CLASS_DISPLAY + `fresco_protocol`. The host
  adapter threads `$ATRIUM_FRESCO_SOCKET` into sandboxed
  children + emits an SBPL grant for the socket path.
  Sample app `atrium-paint` proves the full open / paint /
  poll / destroy lifecycle end-to-end against a stub
  Fresco server, both standalone and via `insula launch`.
  A real visual demo against frescod is the remaining
  piece, and is bring-up work rather than design.
- **~~No bundle update / capability-diff consent flow.~~**
  Done. The CLI computes a structured diff between the
  previously-installed manifest and the incoming one
  (`insula_manifest::CapabilityDiff::between`) and
  refuses re-installs that widen network hosts, raw-
  network, storage quotas, IPC services, capabilities,
  background tasks, peer roles, or entry-point schemes
  unless `--accept-changes` is passed. Narrowing is
  silent. E2E-tested with 5 install-path scenarios.
- **~~No single-file bundle distribution.~~** Done.
  `insula bundle <src> <out.insula>` packs a bundle
  directory into a single-file `INSB` v1 archive
  (deterministic, mode-preserving, unsafe-path refused
  at unpack). `insula install` auto-detects archives by
  leading magic and extracts to a self-cleaning tempdir
  before continuing the install flow. Signature
  pipeline preserved end-to-end through the archive.
- **~~Tabellarius is Phase A only.~~** Phase B done.
  `tabellarius-relay` is a standalone relay (routing
  core + TCP daemon); the device daemon connects, an-
  nounces its subscription pubkeys, queues inbound
  pushes for `GET_PUSH` / `atrium_tabellarius_get_push`,
  and fires a wake hook for the owning app. Wake-on-push
  identity is kernel-attested (SO_PEERPID + proc_pidpath
  → install-root match), so an app can't be woken on
  another app's pushes by lying. The relay wire is CBOR
  frames (spec §3.2) over plaintext TCP; mutual-auth TLS
  is the remaining transport-hardening item. Per-app
  rate limiting + at-least-once retry are also follow-
  ups (`tabellarius.md` §11.2).

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

40+ implementation commits on this branch (latest first; see
`git log` for the full history):

```
atrium-mon: composite sample app — TCP probe + notification
insula-cli: `insula daemons logs <name>` + log path in status
insula-cli: `insula notify` subcommand — direct praeco wrapper
insula-cli: `insula keychain pubkey / sign` subcommand
insula-cli: expand `insula info` to show full capability surface
insula-cli: `insula push subscribe / list / unsubscribe` subcommand
insula-hello: optional tabellarius-subscribe + E2E test through insula launch
docs/INSULA-IMPLEMENTATION: refresh for archive + diff + tabellarius
66a31cc tabellarius-macos: push-delivery daemon + atrium_tabellarius_* ABI
84d2cf6 insula-manifest + insula-cli: capability-diff consent on re-install
2665e4f insula-bundle + insula-cli: single-file .insula archive packaging
16ed3cc docs/INSULA-IMPLEMENTATION: refresh for signing + atrium-fetch (108 tests)
cd82a5b insula-bundle + insula-cli: ed25519 bundle signing + install verification
a5810d4 atrium-fetch: second sample app + half-close fix in netd broker
988b972 docs/INSULA-IMPLEMENTATION: bump to 94 tests + per-app E2E story
6204649 insula-hello: optional net-connect + E2E per-app netd enforcement test
bc0826c atrium-netd-macos: per-app manifest enforcement (SO_PEERPID + proc_pidpath)
3a695e3 docs/INSULA-IMPLEMENTATION: reflect 5-ABI / 4-daemon state
c69111c praeco-macos: notifications daemon + atrium_notify_post
0d3c6f9 insula-cli + host-macos: auto-spawn atrium-netd-macos too
2614fec atrium-netd-macos: network broker daemon + atrium_net_connect ABI
6dfbdc2 vestibulum-macos: disk-backed keystore -- keys survive daemon restart
564d7f8 docs/INSULA-IMPLEMENTATION: status doc for the implementation work
5ba3e4a insula-cli: auto-spawn daemons + daemons up/down/status subcommand
da073d3 vestibulum-macos: ed25519 keychain daemon + atrium_keychain_* ABI
ae5af97 libatrium + host-macos: atrium_container_path + atrium_storage_open + canonical-path fix
55ce71d aqueduct: register CLASS_LOG=10; libatrium + insula-logd migrate
063eee0 insula-cli + host-macos: thread logd socket through launch
88fe51c insula-logd: daemon that catches libatrium log forwarding
2f1b212 libatrium: route atrium_log over real Aqueduct when socket available
1039fd9 insula-cli: user-facing command-line frontend
8959808 insula-bundle + install pathway: install once, launch by app-id
fa24f87 libatrium + insula-hello: end-to-end bring-up loop closed
559bda1 insula-host-macos: launch path via sandbox-exec + integration tests
5ef075b insula-host-macos: SBPL + entitlements generation from manifest
8197907 insula-manifest: add [background] [role] [peer] [sync] [entry-points] [capabilities]
6240143 insula-manifest: add [network] with host-allowlist shape
fa75556 insula-manifest: add [render] [input] [ipc] [storage] [compute]
5d68af9 insula-manifest: initial skeleton -- parse [app] + [bundle]
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

1. **Visual demo against a real frescod**. The ABI + host
   adapter + all four scene-node primitives are done;
   what's missing is running the externally-managed
   Fresco scene server (which already exists in this tree
   at `fresco-scene-server`) and launching `insula-clock`
   or `atrium-paint` against it. This is bring-up work,
   not new code.
2. **An `atrium-text` shaper helper crate** to unlock
   GLYPH_RUN in samples. The libatrium wire surface is
   ready; what's missing is a small layer that takes a
   UTF-8 string + font and produces the pre-shaped
   `AtriumGlyph[]` + R8 atlas the wire format needs.
   rustybuzz + swash are the obvious deps; a bitmap-font
   alternative is possible if pulling those in is too
   heavy.
3. **Hardening pass**, in rough priority order:
   - Hardware-backed keystore wrapping (macOS Keychain
     Services / Secure Enclave) on top of the
     XChaCha20-Poly1305 file encryption already in
     place for vestibulum + tabellarius.
   - Mutual-auth TLS for the device↔relay wire. The
     wire is already CBOR (`tabellarius.md` §3.2); the
     transport is still plaintext TCP — adding TLS
     needs a device-identity cert (Vestibulum-attested)
     and is the remaining §3.2 item.
   - Tighter SBPL via `sandbox_init_with_parameters`
     (private SPI) for per-socket grants.

   (Kernel-attested wake-on-push identity and the CBOR
   relay wire — both formerly on this list — have
   landed.)

Each of these is a self-contained slice; none
invalidates the current shape.
