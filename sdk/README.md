# Atrium SDK

Crates third-party apps build against (dev-tools.md §5.4, app-composition.md),
versioned together and published to the Atrium crate registry
(dev-tools-plan.md M7, M12).

| Crate | What |
|---|---|
| `atrium-message` | `#[derive(Message)]`: postcard-compatible encoding, a decoder that checks bounds, ranges, UTF-8 and depth while it decodes, versioned frames, a JSON schema |
| `atrium-message-derive` | the derive |
| `cargo-atrium` | `cargo atrium check`: manifest lint, the consent text, and the crate-graph lint (parsers and network clients linked into the core that holds the grants, with where to move them) |
| `atrium-schema` | C types, encoder and decoder from a schema file; `tests/c_parity.rs` holds the C decoder to the Rust one byte for byte (ASan, UBSan) |

`cargo test` here runs everything; the C parity test needs a C compiler with
ASan (`CC`, default `cc`).
