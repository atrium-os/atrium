# Developing Atrium apps

How to build an app for Atrium with the SDK: in an Atrium workspace, or
from Linux or macOS. The design behind it is in
[dev-tools.md](../spec/dev-tools.md) and
[app-composition.md](../spec/app-composition.md); this guide is the how-to.

What exists today is marked **built**; what is planned is marked
**planned** with the step of [dev-tools-plan.md](../spec/dev-tools-plan.md)
that brings it.

## 1. Where you build

| Where | Toolchain | Status |
|---|---|---|
| An Atrium workspace | the `rust` and `c-dev` layers, mounted read-only at `/atrium/toolchains/<name>` and on `PATH` | layers built; the workspace composes them (built); VM gate pending |
| Linux or macOS | your own rustup, clang and lld, plus the SDK sysroot | **built** (`cargo atrium cross`) |

### In a workspace

Make a workspace with the Rust toolchain (Workspaces app, or `portcullis
workspace create <name> --toolchain rust`). It gets the newest installed
`rust` layer and the `c-dev` layer: rustc, cargo, clippy, rustfmt,
rust-analyzer and std for both arches; pkgconf, CMake, Meson, Ninja, gmake,
git, clangd, clang-format, bear and Helix. Build scripts and proc macros run
inside the workspace's jail and can't reach your home, Arca or other
workspaces.

### From Linux or macOS

1. Rust: `rustup target add x86_64-unknown-freebsd`. aarch64 has no rustup
   build (tier 3): use a nightly with `-Zbuild-std`, or the SDK's
   `rust-std` for aarch64 (published with the Rust layer, for the same rustc
   version).
2. clang and ld.lld: on Linux your distribution's `clang` and `lld`; on
   macOS `brew install llvm lld` (found in `/opt/homebrew/opt/llvm/bin`
   without changing `PATH`). `ATRIUM_CLANG` picks a specific clang.
3. The sysroot: unpack `atrium-sysroot-<sdk>-<arch>.tar.xz` so that it is
   `~/.atrium/sysroot/<arch>` (or `$ATRIUM_SYSROOT/<arch>`). Each archive is
   FreeBSD 16's `lib`, `usr/lib`, `usr/include`, pkg-config files and CRT
   objects, from pkgbase with each package checked against FreeBSD's signed
   catalogue (`SOURCES` lists them).
4. `cargo install --path sdk/cargo-atrium` (until it is published).

Then, in your project:

```sh
cargo atrium cross amd64            # or aarch64 (add -Zbuild-std on nightly)
cargo atrium cross aarch64 --release
```

`cross` writes clang wrappers for the FreeBSD 16 target and the sysroot
into `target/atrium-cross/<arch>/`, sets the linker, the `cc` crate's
compilers and pkg-config for that target only (host build scripts are
untouched), and runs `cargo build --target <triple>` with the rest of the
arguments.

## 2. A new app

```sh
cargo atrium new hello                       # ./hello from the app template
cargo atrium new hello --id org.example.hello
cargo atrium new --list                      # templates (--json for tools)
```

The `app` template is a Pergola window with a button and a counter, an
`atrium.toml` that asks for the display and nothing else, and a
`.cargo/config.toml` naming the Atrium crate registry. **Planned (M7):**
`viewer`, `client`, `media` and `service` templates with workers, once
`atrium-worker` lands, and a C template on `atrium.app.mk`.

`--sdk-path <atrium checkout>` makes path dependencies on a local tree
instead of registry ones (what the SDK's own tests use).

## 3. The manifest and `cargo atrium check`

`atrium.toml` says what the app is and what it may use. Ask for as little as
possible: everything there is put to the person at install.

```sh
cargo atrium check          # human output
cargo atrium check --json   # {manifest, consent[], findings[{level, code, message, fix}]}
```

It reports:

- what the validator rejects (errors, exit status 1);
- restricted capabilities and the review they need;
- **the consent text**: exactly what the person will be asked to allow;
- **the crate-graph lint**: parsers of outside data (image, font, archive,
  document, media formats) and network clients linked into the **core**,
  the process that holds the capabilities. The fix it suggests is to move
  that work into a worker, which runs with no capabilities, or to use a
  system service. SDK crates are trusted and not walked.

## 4. Messages between processes: `atrium-message`

Workers, services and the core talk in messages. Derive them:

```rust
use atrium_message::Message;

#[derive(Message, Debug, PartialEq)]
#[message(version = 1)]
struct Thumbnail {
    #[message(min = 1, max = 4096)]
    width: u32,
    #[message(min = 1, max = 4096)]
    height: u32,
    #[message(max_len = 67_108_864)]
    pixels: Vec<u8>,
    #[message(max_len = 256, no_control)]
    title: String,
}

let frame = t.to_frame();                          // version + body
let t2 = Thumbnail::from_frame(&frame, 70 << 20)?; // checked while decoding
```

The encoding is postcard's. The decoder checks every bound while it reads:
lengths (`max_len`, `item_max_len`; 64 KiB when not given), ranges (`min`,
`max`), UTF-8 and control characters (`no_control`), nesting depth, the
frame's version and size, and that nothing is left over. A message that
breaks a bound is an error, never a partly filled value.

C code gets the same checks from the schema:

```sh
# from Rust: Thumbnail::schema_json() written to thumbnail.json
atrium-schema thumbnail.json --prefix thumb --out gen/
# gen/thumbnail.h, gen/thumbnail.c: thumb_Thumbnail_decode_frame() etc.
```

The C decoder is held to the Rust one byte for byte (the SDK's parity test
runs thousands of valid and broken inputs under ASan and UBSan).

## 5. Crates and the registry

SDK crates (`pergola`, `libatrium`, `atrium-fonts`, `fresco-client`,
`fresco-protocol`, `atrium-message` and what they use) come from the Atrium
crate registry, a static cargo sparse registry; everything else comes from
crates.io as usual. All SDK crates are `MIT OR Apache-2.0` and versioned
together.

Cargo checks each crate against the index but not the index itself, so the
index is signed with Atrium's platform key:

```sh
cargo atrium registry verify https://atrium-os.github.io/catalog/sdk --keys <platform key .pem or folder>
```

checks `index.sig` over `index.files` and every listed file. In a workspace
the Rust layer carries an offline copy, covered by the layer's signature.
The registry is served with the package catalog
(`sparse+https://atrium-os.github.io/catalog/sdk/index/`). **Planned (M12):**
the first signed publication, and `cargo atrium` verifying before each build.

## 6. C apps

In a workspace, the `c-dev` layer has CMake, Meson, Ninja, gmake and
pkgconf; the base system has clang, lld and the Atrium headers and
libraries (`pkg-config --cflags --libs atrium`). **Planned (M4):**
`atrium.app.mk` with `make run` and `make bundle`, and a C template.

## 7. Running, bundling, fuzzing

**Planned:**

- `cargo atrium run` (M7): build, then ask portcullisd to run the app from
  the workspace as a dev app (amber window frame).
- `cargo atrium bundle [--sign]` (M7): a release bundle with its closure,
  signed with your publisher key from Arca.
- `cargo atrium fuzz` (M7, Artifex panel in M12): fuzz each worker's input
  and each input surface of the core; a crash in the core comes with "Move
  into a worker".

## 8. Reference

| Command | Does |
|---|---|
| `cargo atrium new <dir> [--template t] [--id id] [--sdk-path p] [--registry url]` | a project from a template |
| `cargo atrium new --list [--json]` | the templates |
| `cargo atrium check [--manifest p] [--json]` | manifest and crate-graph lint, consent text |
| `cargo atrium cross <amd64\|aarch64> [--sysroot d] [cargo args]` | build for Atrium from another OS or arch |
| `cargo atrium registry verify <dir\|url> [--keys d]` | check the registry's signed index |
| `cargo atrium registry build --out d --url u [--sign key] <seed>...` | for SDK maintainers: pack and sign the registry |
| `atrium-schema <schema.json> [--prefix p] [--out d] [--name n]` | C types, encoder and decoder |

Scripts for maintainers: `scripts/layers/build-c-dev-layer.sh`,
`build-rust-layer.sh`, `pack-rust-layer.sh` (toolchain layers),
`scripts/layers/sdk-sysroot.sh` (sysroot archives),
`scripts/sdk/build-registry.sh` (the registry).
