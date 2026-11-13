# Sigillum

Atrium's identity verification service: FIDO2/U2F security keys, smartcards and
USB tokens, fingerprint sensors. Design: [docs/spec/sigillum.md](../docs/spec/sigillum.md).

Its own workspace. The one path dependency outside `sigillum/` is `portcullis-peer` (peer
uids and the launch registry), in `sigillumd` only, as in arcad.

| crate | what | where it runs |
|---|---|---|
| `sigillum-cbor` | bounded CTAP2 CBOR: canonical encoder, strict decoder (depth, item and size caps; no tags, floats or indefinite lengths) | driver jails |
| `sigillum-ctap` | CTAPHID framing, CTAP 2.1 client (`GetInfo`, `MakeCredential`, `GetAssertion`, `ClientPIN` with PIN/UV protocol 2), authenticator data and COSE parsing; sans-I/O plus a blocking client over a `HidDevice` | driver jails |
| `sigillum-softkey` | a CTAP 2.1 authenticator in memory, with misbehaviour knobs; tests only | tests |
| `sigillum-proto` | the wires of spec §14: clients ↔ `sigillumd`, drivers ↔ `sigillumd`, `sigillumd` ↔ `sigillum-ui`, with caps | everywhere |
| `sigillum-fido` | the FIDO driver: logic, socket loop and the hidraw binary (CTAP2 and U2F-only keys); attestation stripped to `none` | `sigillum-fido` jail |
| `sigillumd` | the broker as a pure state machine (labels, the press gate, ceremonies, byte checks of driver records, fingerprint policy) and the daemon around it (three sockets, peer uids, the launch registry) | `sigillumd` jail |

```sh
cd sigillum && cargo test
```

`sigillumd/tests/ceremonies.rs` is S1's gate: the broker, the driver and the soft
key wired as the daemon will wire them, with a relying party verifying the
registration and the ES256 assertion at the end, plus the refusals (apps and other
RPs, no press, a lying key, a lying driver, busy, cancel, unplug).

`sigillumd/tests/daemon.rs` is S2c's: the real daemon on real sockets with the
driver's serve loop over the soft key, a scripted trusted UI and a client.

Running by hand on a development host (a real key, Linux or FreeBSD):

```sh
SIGILLUM_SOCKET=/tmp/s.sock SIGILLUM_DRIVER_SOCKET=/tmp/d.sock SIGILLUM_UI_SOCKET=/tmp/u.sock \
  SIGILLUM_DEV_UID=$(id -u) SIGILLUM_UI_UID=$(id -u) cargo run -p sigillumd
cargo run -p sigillum-fido -- --dev /dev/hidraw0 --connect /tmp/d.sock
```
