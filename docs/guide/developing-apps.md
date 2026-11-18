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
`.cargo/config.toml` naming the Atrium crate registry.

Two templates show an app split into a core and a worker
(app-composition.md §4.3):

- `viewer`: a BMP viewer. Files offers it for BMP pictures (`[[opens]]`);
  the core hands each opened file's descriptor to the `decode` worker,
  which runs in a jail of its own with no capabilities and answers with a
  checked `Picture`. The decoder lives in the lib (`workers::decode`), the
  worker binary is one line (`atrium_worker::main!`), and a seed picture
  is in the fuzz corpus.
- `indexer`: word counts of the text files you open. One `index` worker is
  kept for the app's run (`lifetime = "session"`, `input = "message"`): the
  core sends each file's text as a message, the worker keeps the counts.

- `client`: a JSON Feed reader. The core has no network: it asks Nuntius
  (`net-fetch`, `net-fetch-sites`) for the feed, gets the response as a
  descriptor and hands it unread to the `parse` worker, which parses it
  with serde_json (any crate can parse there; it's caged).
- `media`: plays the audio and video you open with it, in the system's
  media player embedded through Limen (`embeds = ["media-player"]`). The
  core sends the opened file's descriptor to the player and draws the
  controls; it never demuxes or decodes.

- `service`: provides a role to other apps (`[roles] implements =
  ["share-target"]`). It shows up in the share sheet
  ([share-sheet.md](../spec/share-sheet.md)) for text, shows what was
  shared in an embedded window, and keeps it in `~/kept.txt` when you
  press Keep. It asks for no network and no graphics of its own; its
  window comes from the app that shares. Try it with `share-demo` from
  `share-sheet/`.
  A provider is started as a one-shot jail, so its manifest needs
  `[resources] memory` (for example `memory = "256M"`); portcullisd
  refuses to start one without.

`cargo atrium new hello --template c-app` is the same in C: a window on
libatrium and a Makefile on `atrium.app.mk` (`make`, `make run`).

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
- restricted capabilities and the review they need (not for `org.atrium.*`
  ids: only the platform key signs those, and the system policy grants
  what they ask);
- **the consent text**: exactly what the person will be asked to allow;
- **the crate-graph lint**: parsers of outside data (image, font, archive,
  document, media formats) and network clients linked into the **core**,
  the process that holds the capabilities. The fix it suggests is to move
  that work into a worker, which runs with no capabilities, or to use a
  system service. SDK crates are trusted and not walked. What only the
  font stack reaches (fresco-text, navigator-fonts and the font parsers
  under them, WOFF's flate2 and brotli included) reads the system's fonts
  and the app's own, so it isn't flagged; the same crate by any other
  path is. A font parser the core links itself is a note.

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

### Workers: `atrium-worker`

```rust
// The core
let workers = atrium_worker::Workers::get()?;
let pic: Picture = workers.call("decode", &Ask { max_side: 2048 }, Some(file))?;
let mut index = workers.session("index")?;           // lifetime = "session"
let top: Top = index.call(&Add { text }, None)?;

// The worker's binary
atrium_worker::main!(myapp::workers::decode);       // fn(Req, Option<File>) -> Result<Resp, String>
```

Each `[[workers]]` entry in `atrium.toml` names the binary (`entry`), what
it takes (`input = "fd"` or `"message"`), `lifetime`, `memory` and `cpu`.
A crash, a limit or a timeout comes back as an `Err`, never as a panic in
the core. Off Atrium, `ATRIUM_WORKERS=local ATRIUM_APP_ROOT=.` runs the
workers as plain children (`cargo run` in a workspace).

### Moving code into a worker: `cargo atrium worker add`

`cargo atrium worker add <name>` makes a new worker in an existing
project (Artifex's "Move into a worker" runs it):

```sh
cargo atrium worker add thumb                                  # input = "fd"
cargo atrium worker add raw --input bytes                      # bytes the core holds
cargo atrium worker add count --from parse::lines              # run a lib function there
cargo atrium worker add shout --input message --from parse::shout --json
```

It adds the `[[workers]]` entry and a `[[bin]]`, writes
`src/<name>_worker.rs` (the messages, the handler, and `call`, the core's
side) and `src/bin/<name>.rs`, and re-exports the handler from the lib's
`workers` module so `cargo atrium fuzz worker:<name>` finds it. Comments
and layout in your files stay as they were: every edit is an insertion.

With `--from`, the handler runs that library function, and `call` takes
the function's own arguments and returns `Result<what it returned,
String>`, so a call in the core changes from `parse::lines(&bytes)` to
`count_worker::call(file)`. The command lists those calls (`calls` in the
JSON) but doesn't rewrite them. A function in `src/main.rs` has to move
into the library first, since the worker is another binary. Parameter
and answer types the wire doesn't carry on its own need
`#[derive(Message)]`; the command says which. `--dry-run` shows what it
would change.

## 5. Crates and the registry

SDK crates (`pergola`, `libatrium`, `atrium-fonts`, `fresco-client`,
`fresco-protocol`, `atrium-message`, `atrium-worker`, `atrium-open`,
`limen`, `nuntius` and what they use) come from the Atrium
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
libraries (`pkg-config --cflags --libs atrium`) and `atrium.app.mk`:
`PROG=`, `ATRIUM_LIBS=` and `.include <atrium.app.mk>` give `make`,
`make atrium.toml` and `make run`. `cargo atrium new <dir> --template c-app`
starts one. `make bundle` writes an installable bundle through
`cargo-atrium bundle` (add the Rust toolchain to the workspace for it);
`make bundle BUNDLE_FLAGS="--sign key.pem"` signs it.

## 7. Running, fuzzing and bundling

In a workspace's Terminal, `cargo atrium run [--release]` builds, puts the
core and each worker where `atrium.toml`'s entries say (the templates use
`bin/<name>` and `bin/<name>-<worker>`), and asks you to Run the project as an app: the Terminal shows
what it asks and Run needs your press. The app runs with the capabilities
its manifest asks for and the libraries it links, in its own jail (amber
window frame). For C, `make run` does the same.

### Fuzzing

Every `[[workers]]` entry is a target, `worker:<name>`, with nothing to
declare: the harness calls the worker's handler, `<lib>::workers::<name>`
(dashes as underscores), the function its `main!` serves. For
`input = "fd"` each input is the file and the request is `Req::default()`;
for `input = "message"` each input is a frame, decoded with its bounds as
the worker decodes it. An answer that breaks its own message's bounds,
which the core would refuse, counts as a crash.

Other surfaces, above all the core's, are declared in Cargo.toml; each is a
`fn(&[u8])` in the project's lib crate (a declared `worker:` name replaces
the automatic one):

```toml
[package.metadata.atrium.fuzz."worker:decode"]
function = "photos::decode"
about = "the picture decoder"

[package.metadata.atrium.fuzz."core:album"]
function = "photos::read_album"     # an input surface of the core
```

```sh
cargo atrium fuzz --list [--json]
cargo atrium fuzz core:album [--max-time 600] [--json]
```

It writes a libFuzzer harness under `target/atrium-fuzz/`, builds it on
stable Rust with coverage instrumentation, debug assertions and overflow
checks, and runs it until the first crash (or `--max-time`). The corpus is
kept in `fuzz/corpus/<target>/`, crashing inputs in `fuzz/crashes/<target>/`;
replay one with the harness binary and the input. A crash in a `core:`
target is the insecure pattern itself: hostile input crashed the process
that holds your capabilities, and the fix is to move that parsing into a
worker. `--json` prints one event a line (`progress`, `crash` with input,
frame, `in_core`, `fix` and `binary`, `done`), which Artifex's fuzz panel
reads. Without AddressSanitizer (stable Rust), comparisons that go through
`memcmp` (slice equality) aren't traced, so magic numbers are found slowly;
seed the corpus with a valid file.

### Workers built in another crate

A worker's binary is found by its entry's file name: among the project's
own binaries, then in the crates it depends on (built with `-p`). A
worker the app doesn't link, like Mail's parse jail, is named in
`Cargo.toml`, and `run`, `bundle` and `check` build or find it there:

```toml
[package.metadata.atrium.workers]
parse = "../mail-parse"      # its folder, or its Cargo.toml
```

### Bundling: `cargo atrium bundle`

`cargo atrium bundle --sign <key.pem>` makes the installable bundle
(atrium-bundle-format.md): it builds release, puts `atrium.toml`, the core
and each worker at their `entry` paths and the icon (an SVG path) in a
tree, and adds the runtime closure: every library the programs need,
read from their `DT_NEEDED` and `DT_RUNPATH` (never by running them), and
the loader. Base libraries keep their paths (`lib/`, `usr/lib/`); others
go to `usr/lib/`. Then `opifex pack` writes the content index and the
signature, and `opifex archive` the `.insula` beside the tree (the key may be PKCS#8 or
SEC1 PEM)
(`target/atrium-bundle/<id>-<version>-<arch>`), which installs through
the install sheet.

In a workspace the libraries come from what the workspace sees, so a
library from `pkg` or a linked workspace goes in too. From Linux or macOS
they come from the SDK sysroot (`--arch`, `--sysroot` as for `cross`), so
the app can only need base libraries there, and packing needs `OPIFEX`
pointing at an opifex. A `dev.` id isn't bundled: give `[app] id` one of
your own. In a folder without `Cargo.toml` (a C project) it bundles what
`make` put at the entries, into `bundle/`. A library the app only `dlopen`s
isn't in its ELF, so list it: `[bundle] extra-libs = ["libjansson.so.4"]`
(file names, `*` and `?` allowed); Run carries it too. **Planned:** the key
from Arca instead of a file.

## 8. Reference

| Command | Does |
|---|---|
| `cargo atrium new <dir> [--template t] [--id id] [--sdk-path p] [--registry url]` | a project from a template |
| `cargo atrium new --list [--json]` | the templates |
| `cargo atrium check [--manifest p] [--json]` | manifest and crate-graph lint, consent text |
| `cargo atrium cross <amd64\|aarch64> [--sysroot d] [cargo args]` | build for Atrium from another OS or arch |
| `cargo atrium run [--release] [cargo args]` | build the core and workers, and Run the project as a dev app (asks for your press) |
| `cargo atrium bundle [--arch a] [--sign key.pem] [--out dir] [--json]` | a release bundle with its closure, packed, signed, and an `.insula` |
| `cargo atrium worker add <name> [--input fd\|bytes\|message] [--from path::fn] [--dry-run] [--json]` | move part of the app into a new worker |
| `cargo atrium fuzz --list \| <target> [--max-time s] [--json]` | fuzz workers and the core's input surfaces |
| `cargo atrium registry verify <dir\|url> [--keys d]` | check the registry's signed index |
| `cargo atrium registry build --out d --url u [--sign key] <seed>...` | for SDK maintainers: pack and sign the registry |
| `atrium-schema <schema.json> [--prefix p] [--out d] [--name n]` | C types, encoder and decoder |

Scripts for maintainers: `scripts/layers/build-c-dev-layer.sh`,
`build-rust-layer.sh`, `pack-rust-layer.sh` (toolchain layers),
`scripts/layers/sdk-sysroot.sh` (sysroot archives),
`scripts/sdk/build-registry.sh` (the registry).
