# Atrium SDK

Crates third-party apps build against (dev-tools.md §5.4, app-composition.md),
versioned together and published to the Atrium crate registry
(dev-tools-plan.md M7, M12).

| Crate | What |
|---|---|
| `atrium-message` | `#[derive(Message)]`: postcard-compatible encoding, a decoder that checks bounds, ranges, UTF-8 and depth while it decodes, versioned frames, a JSON schema |
| `atrium-message-derive` | the derive |
| `atrium-worker` | an app's `[[workers]]`: `Workers::get()?.call(name, &req, file)` and `.session(name)` in the core, `atrium_worker::main!(handler)` in the worker; jailed through portcullisd's `WorkerCall`, a Capsicum child for `isolation = "capsicum"`, plain children with `ATRIUM_WORKERS=local`, threads with `Workers::in_process()`; a crash, a limit or the entry's `cpu` time comes back as an `Error` |
| `cargo-atrium` | `cargo atrium run` (build and Run as a dev app), `cargo atrium fuzz` (libFuzzer harnesses for workers and core input surfaces, stable Rust), `cargo atrium cross amd64|aarch64` (clang + ld.lld with the SDK sysroot from `scripts/layers/sdk-sysroot.sh`), `cargo atrium new` (templates `app`, `c-app`, `viewer`, `indexer`, `client`, `media` and `service` in `templates/`, `--list`; `--sdk-path` builds against this tree instead of the registry), `cargo atrium bundle` (a release bundle with its library closure, packed, signed and archived to an `.insula` with opifex), `cargo atrium worker add` (a new `[[workers]]` entry, its messages, handler and the core's `call`; `--from path::fn` moves a lib function there) and `cargo atrium check`: manifest lint, the consent text, and the crate-graph lint (parsers and network clients linked into the core that holds the grants, with where to move them) |
| `atrium-schema` | C types, encoder and decoder from a schema file; `tests/c_parity.rs` holds the C decoder to the Rust one byte for byte (ASan, UBSan) |

The registry: `scripts/sdk/build-registry.sh OUT URL` packs the SDK crates
and every path crate they reach into a static sparse registry (opifex.md
§8.4) with `index.sig` over `index.files`; `cargo atrium registry verify`
checks it against the platform keys. `scripts/layers/sdk-sysroot.sh` makes the
sysroot archives `cargo atrium cross` uses.

`cargo test` here runs everything; the C parity test needs a C compiler with
ASan (`CC`, default `cc`).
