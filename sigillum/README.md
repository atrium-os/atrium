# Sigillum

Atrium's identity verification service: FIDO2/U2F security keys, smartcards and
USB tokens, fingerprint sensors. Design: [docs/spec/sigillum.md](../docs/spec/sigillum.md).

Its own workspace, with no path dependency outside `sigillum/`, like Vinculum.

| crate | what | where it runs |
|---|---|---|
| `sigillum-cbor` | bounded CTAP2 CBOR: canonical encoder, strict decoder (depth, item and size caps; no tags, floats or indefinite lengths) | driver jails |
| `sigillum-ctap` | CTAPHID framing, CTAP 2.1 client (`GetInfo`, `MakeCredential`, `GetAssertion`, `ClientPIN` with PIN/UV protocol 2), authenticator data and COSE parsing; sans-I/O plus a blocking client over a `HidDevice` | driver jails |
| `sigillum-softkey` | a CTAP 2.1 authenticator in memory, with misbehaviour knobs; tests only | tests |
| `sigillum-proto` | the two wires of spec §14: clients ↔ `sigillumd`, drivers ↔ `sigillumd`, with caps | everywhere |
| `sigillum-fido` | the FIDO driver's logic: requests in, checked and capped records out; attestation stripped to `none` | `sigillum-fido` jail (binary in S2) |
| `sigillumd` | the broker as a pure state machine: labels, the press gate, ceremonies, byte checks of driver records, fingerprint policy | `sigillumd` (daemon loop in S2) |

```sh
cd sigillum && cargo test
```

`sigillumd/tests/ceremonies.rs` is S1's gate: the broker, the driver and the soft
key wired as the daemon will wire them, with a relying party verifying the
registration and the ES256 assertion at the end, plus the refusals (apps and other
RPs, no press, a lying key, a lying driver, busy, cancel, unplug).
